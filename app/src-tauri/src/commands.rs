// Tauri commands — the bridge between the webview UI and the Rust core.
use crate::discovery;
use crate::netcore::{self, BatchDoneEvt, Entry, FileDoneEvt, Sink, DATA_CAP};
use crate::state::{AppState, Slot};
use base64::Engine as _;
use serde::Serialize;

#[tauri::command]
pub async fn get_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let recv = state.recv_snapshot();
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "receiver": recv,
        "transfer_port": netcore::DEFAULT_PORT,
    }))
}

#[tauri::command]
pub async fn list_peers() -> serde_json::Value {
    match discovery::status().await {
        Ok((self_info, peers)) => serde_json::json!({ "self": self_info, "peers": peers, "error": serde_json::Value::Null }),
        Err(e) => serde_json::json!({ "self": serde_json::Value::Null, "peers": [], "error": e }),
    }
}

#[derive(Serialize)]
pub struct PeerPing {
    pub online: bool,
    pub name: String,
    pub version: Option<String>,
    pub your_ip: Option<String>,
}

// Probe one saved target: is a Tailsend receiver up there? Mobile relies on
// this for peer status (no tailscale CLI there); the reply's your_ip also
// tells the prober its own tailnet address.
#[tauri::command]
pub async fn ping_peer(
    state: tauri::State<'_, AppState>,
    target: String,
) -> Result<PeerPing, String> {
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    let p = netcore::ping(&addr, &token).await?;
    Ok(PeerPing { online: true, name: p.name, version: p.version, your_ip: p.your_ip })
}

#[tauri::command]
pub async fn restart_receiver(state: tauri::State<'_, AppState>) -> Result<(), String> {
    crate::state::stop_receiver(&state);
    crate::state::start_receiver(&state).await
}

#[tauri::command]
pub async fn pick_files() -> Result<Vec<String>, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端请使用「选择文件（界面）」按钮".to_string());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        Ok(tauri::async_runtime::spawn_blocking(|| {
            rfd::FileDialog::new().set_title("选择要发送的文件").pick_files()
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| v.into_iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default())
    }
}

#[tauri::command]
pub async fn pick_folders() -> Result<Vec<String>, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端不支持文件夹选择".to_string());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        Ok(tauri::async_runtime::spawn_blocking(|| {
            rfd::FileDialog::new().set_title("选择要发送的文件夹").pick_folders()
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| v.into_iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default())
    }
}

#[tauri::command]
pub async fn stat_paths(paths: Vec<String>) -> Result<Vec<Entry>, String> {
    tauri::async_runtime::spawn_blocking(move || netcore::collect_entries(&paths))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn send_files(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    target: String,
    entries: Vec<Entry>,
) -> Result<u64, String> {
    if entries.is_empty() {
        return Err("没有可发送的文件".into());
    }
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    netcore::ping(&addr, &token).await?;

    let batch = state.next_batch();
    let sink = Sink::Gui(app);
    let sender = netcore::host_name();
    let n = entries.len();
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let mut okc = 0usize;
        let mut failed = 0usize;
        let mut bytes = 0u64;
        for (i, e) in entries.iter().enumerate() {
            match netcore::push_file(&sink, &addr, &token, &sender, batch, i, n, e).await {
                Ok(()) => {
                    okc += 1;
                    bytes += e.size;
                    sink.file_done(&FileDoneEvt { batch, idx: i, rel: e.rel.clone(), ok: true, error: None });
                }
                Err(err) => {
                    failed += 1;
                    sink.file_done(&FileDoneEvt { batch, idx: i, rel: e.rel.clone(), ok: false, error: Some(err) });
                }
            }
        }
        sink.batch_done(&BatchDoneEvt {
            batch,
            ok: okc,
            failed,
            total_bytes: bytes,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    });
    Ok(batch)
}

#[tauri::command]
pub async fn send_data_begin(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    target: String,
    name: String,
    size: u64,
) -> Result<u64, String> {
    if size > DATA_CAP {
        return Err(format!(
            "通过界面选择的大文件上限为 {}，更大的文件请在桌面端用「打开文件选择器」发送",
            netcore::human(DATA_CAP as i64)
        ));
    }
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    netcore::ping(&addr, &token).await?;

    let id = state.next_data_id();
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let sink = Sink::Gui(app);
    let sender = netcore::host_name();
    tokio::spawn(async move {
        let res = netcore::push_data(&sink, &addr, &token, &sender, id, 0, name, size, rx).await;
        let _ = done_tx.send(res);
    });
    state.slots.lock().unwrap().insert(id, Slot { tx, done: done_rx });
    Ok(id)
}

#[tauri::command]
pub async fn send_data_chunk(
    state: tauri::State<'_, AppState>,
    id: u64,
    b64: String,
) -> Result<(), String> {
    let tx = {
        let slots = state.slots.lock().unwrap();
        match slots.get(&id) {
            Some(s) => s.tx.clone(),
            None => return Err(format!("传输 #{id} 不存在或已结束")),
        }
    };
    let data = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| format!("数据解码失败：{e}"))?;
    tx.send(bytes::Bytes::from(data))
        .await
        .map_err(|_| format!("传输 #{id} 已中断"))
}

#[tauri::command]
pub async fn send_data_end(state: tauri::State<'_, AppState>, id: u64) -> Result<(), String> {
    let slot = state.slots.lock().unwrap().remove(&id);
    match slot {
        None => Err(format!("传输 #{id} 不存在或已结束")),
        Some(slot) => {
            drop(slot.tx); // EOF for the stream
            slot.done.await.map_err(|_| format!("传输 #{id} 已中断"))?
        }
    }
}

#[derive(Serialize)]
struct InboxEntry {
    path: String,
    size: u64,
    mtime: u64,
}

#[tauri::command]
pub async fn inbox_list(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let dir = state.recv.lock().unwrap().save_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out: Vec<InboxEntry> = vec![];
        let mut total = 0usize;
        for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|r| r.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            if e.file_name().to_string_lossy().ends_with(".part") {
                continue;
            }
            let rel = e.path().strip_prefix(&dir).unwrap_or(e.path());
            let (size, mtime) = match e.metadata() {
                Ok(m) => (
                    m.len(),
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                ),
                Err(_) => (0, 0),
            };
            out.push(InboxEntry {
                path: rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/"),
                size,
                mtime,
            });
            total += 1;
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        Ok(serde_json::json!({ "total": total, "files": out.into_iter().take(200).collect::<Vec<_>>() }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn open_inbox(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let dir = state.recv.lock().unwrap().save_dir.clone();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(&dir).spawn();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let r: std::io::Result<()> = Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "移动端不支持"));
    r.map(|_| ()).map_err(|e| e.to_string())
}
