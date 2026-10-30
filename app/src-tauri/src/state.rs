// Application state: receiver lifecycle + outgoing transfer bookkeeping.
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::Manager;
use tokio::sync::{mpsc, oneshot, watch};

pub struct AppState {
    pub recv: Mutex<RecvState>,
    pub batch_seq: AtomicU64,
    pub data_seq: AtomicU64,
    pub slots: Mutex<HashMap<u64, Slot>>,
}

pub struct RecvState {
    pub listening: bool,
    pub addr: String,
    pub advertise: Vec<String>,
    pub error: Option<String>,
    pub save_dir: PathBuf,
    pub port: u16,
    pub token: String,
    pub max: u64,
    shutdown: Option<watch::Sender<()>>,
}

// One in-flight webview-provided (JS) file upload.
pub struct Slot {
    pub tx: mpsc::Sender<bytes::Bytes>,
    pub done: oneshot::Receiver<Result<(), String>>,
}

#[derive(Serialize)]
pub struct RecvSnapshot {
    pub listening: bool,
    pub addr: String,
    pub advertise: Vec<String>,
    pub error: Option<String>,
    pub save_dir: String,
    pub port: u16,
    pub token_set: bool,
}

impl RecvState {
    fn snapshot(&self) -> RecvSnapshot {
        RecvSnapshot {
            listening: self.listening,
            addr: self.addr.clone(),
            advertise: self.advertise.clone(),
            error: self.error.clone(),
            save_dir: self.save_dir.display().to_string(),
            port: self.port,
            token_set: !self.token.is_empty(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        let save_dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Tailsend");
        let _ = std::fs::create_dir_all(&save_dir);
        Self {
            recv: Mutex::new(RecvState {
                listening: false,
                addr: String::new(),
                advertise: vec![],
                error: None,
                save_dir,
                port: crate::netcore::DEFAULT_PORT,
                token: String::new(),
                max: 0,
                shutdown: None,
            }),
            batch_seq: AtomicU64::new(1),
            data_seq: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn recv_snapshot(&self) -> RecvSnapshot {
        self.recv.lock().unwrap().snapshot()
    }

    pub fn next_batch(&self) -> u64 {
        self.batch_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn next_data_id(&self) -> u64 {
        self.data_seq.fetch_add(1, Ordering::Relaxed)
    }
}

// Bind the tailnet receiver; on failure record why and keep serving the UI.
pub async fn start_receiver(state: &AppState) -> Result<(), String> {
    {
        let rs = state.recv.lock().unwrap();
        if rs.listening {
            return Ok(());
        }
    }

    let (bind_ip, advertise) = match resolve_bind().await {
        Ok(v) => v,
        Err(e) => {
            let mut rs = state.recv.lock().unwrap();
            rs.listening = false;
            rs.error = Some(e.clone());
            return Err(e);
        }
    };
    let port = state.recv.lock().unwrap().port;
    let addr = format!("{bind_ip}:{port}");

    let ln = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            let mut rs = state.recv.lock().unwrap();
            rs.listening = false;
            rs.error = Some(e.to_string());
            return Err(format!("监听 {addr} 失败：{e}"));
        }
    };

    let (tx, mut rx) = watch::channel(());
    let cfg = {
        let rs = state.recv.lock().unwrap();
        crate::netcore::RecvCfg {
            dir: rs.save_dir.clone(),
            token: rs.token.clone(),
            max: rs.max,
            host: crate::netcore::host_name(),
        }
    };
    let serve = axum::serve(
        ln,
        crate::netcore::router(cfg).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = rx.changed().await;
    });
    tokio::spawn(async move {
        if let Err(e) = serve.await {
            eprintln!("接收端退出：{e}");
        }
    });

    let mut rs = state.recv.lock().unwrap();
    rs.listening = true;
    rs.addr = addr.clone();
    rs.advertise = advertise;
    rs.error = None;
    rs.shutdown = Some(tx);
    println!("接收端已启动：http://{addr}");
    Ok(())
}

pub fn stop_receiver(state: &AppState) {
    let mut rs = state.recv.lock().unwrap();
    if let Some(tx) = rs.shutdown.take() {
        let _ = tx.send(());
    }
    rs.listening = false;
    rs.addr.clear();
    rs.advertise.clear();
}

async fn resolve_bind() -> Result<(String, Vec<String>), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        // The VPN interface's address isn't bindable from userspace on mobile;
        // 0.0.0.0 stays unreachable off the tailnet in practice.
        Ok(("0.0.0.0".to_string(), vec![]))
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let ip = crate::discovery::primary_ip().await?;
        Ok((ip.clone(), vec![ip]))
    }
}

// Receiver bootstrap with auto-retry: if Tailscale connects later, the
// receiver comes up on its own within ~20s.
pub async fn bootstrap(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    let _ = start_receiver(&state).await;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let listening = state.recv.lock().unwrap().listening;
        if !listening {
            let _ = start_receiver(&state).await;
        }
    }
}
