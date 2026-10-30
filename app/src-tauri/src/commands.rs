// Tauri commands — the bridge between the webview UI and the Rust core.
use crate::discovery;
use crate::netcore::{self, BatchDoneEvt, Entry, FileDoneEvt, Sink, DATA_CAP};
use crate::state::{AppState, Slot};
use base64::Engine as _;
use serde::Serialize;
use serde_json::json;

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

/// Mobile node discovery: ask reachable receivers (favorites, usually a NAS)
/// for their `tailscale status` view and merge everything into one list.
/// Targets are literal addresses — this must work without a local CLI.
#[tauri::command]
pub async fn discover_peers(
    state: tauri::State<'_, AppState>,
    targets: Vec<String>,
) -> Result<serde_json::Value, String> {
    let token = state.recv.lock().unwrap().token.clone();
    let mut own_ip = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut peers: Vec<serde_json::Value> = Vec::new();
    for t in targets {
        let (ip, port) = match discovery::find_target(&t, netcore::DEFAULT_PORT).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let addr = format!("{ip}:{port}");
        let Ok(ping) = netcore::ping(&addr, &token).await else {
            continue; // unreachable source, try the next one
        };
        if own_ip.is_empty() {
            own_ip = ping.your_ip.clone().unwrap_or_default();
        }
        let Ok(list) = netcore::fetch_peers(&addr, &token).await else {
            continue; // older receiver without /api/peers
        };
        for p in list {
            if p.ip.is_empty() || !seen.insert(p.ip.clone()) || p.ip == own_ip {
                continue;
            }
            peers.push(json!({
                "name": if p.name.is_empty() { p.ip.clone() } else { p.name },
                "ip": p.ip,
                "os": p.os,
                "online": p.online,
                "via": addr,
            }));
        }
    }
    peers.sort_by(|a, b| {
        let on = |v: &serde_json::Value| v["online"].as_bool().unwrap_or(false);
        on(b).cmp(&on(a)).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(json!({ "self_ip": own_ip, "peers": peers }))
}

#[tauri::command]
pub async fn restart_receiver(state: tauri::State<'_, AppState>) -> Result<(), String> {
    crate::state::stop_receiver(&state);
    crate::state::start_receiver(&state).await
}

// Store the receiver's shared secret and rebind so it takes effect at once
// (an empty token means "accept anything the tailnet lets through").
#[tauri::command]
pub async fn set_receiver_token(
    state: tauri::State<'_, AppState>,
    token: String,
) -> Result<(), String> {
    let token = token.trim().to_string();
    {
        let mut rs = state.recv.lock().unwrap();
        if rs.token == token {
            return Ok(());
        }
        rs.token = token;
    }
    crate::state::stop_receiver(&state);
    crate::state::start_receiver(&state).await
}

#[tauri::command]
pub async fn pick_files(window: tauri::WebviewWindow) -> Result<Vec<String>, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端请使用「选择文件（界面）」按钮".to_string());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        // Parent the panel to our window: an orphan NSOpenPanel appears late,
        // behind other apps, and steals focus slowly on macOS.
        Ok(tauri::async_runtime::spawn_blocking(move || {
            rfd::FileDialog::new()
                .set_title("选择要发送的文件")
                .set_parent(&window)
                .pick_files()
        })
        .await
        .map_err(|e| e.to_string())?
        .map(|v| v.into_iter().map(|p| p.display().to_string()).collect())
        .unwrap_or_default())
    }
}

#[tauri::command]
pub async fn pick_folders(window: tauri::WebviewWindow) -> Result<Vec<String>, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端不支持文件夹选择".to_string());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        Ok(tauri::async_runtime::spawn_blocking(move || {
            rfd::FileDialog::new()
                .set_title("选择要发送的文件夹")
                .set_parent(&window)
                .pick_folders()
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
    #[serde(default)]
    incoming: bool,
}

/// A `.tspart-*`/`.part-*` temp file back to its original name and relative dir.
/// Returns None for names that are not receiver temp files.
fn strip_temp_suffix(rel: &str) -> Option<String> {
    let (dir, fname) = match rel.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, rel),
    };
    let cut = match fname.rfind(".tspart-").or_else(|| fname.rfind(".part-")) {
        Some(i) => i,
        None => {
            return fname
                .ends_with(".part")
                .then(|| fname.trim_end_matches(".part").to_string())
                .map(|base| match dir {
                    Some(d) => format!("{d}/{base}"),
                    None => base,
                });
        }
    };
    let base = &fname[..cut];
    if base.is_empty() {
        return None;
    }
    Some(match dir {
        Some(d) => format!("{d}/{base}"),
        None => base.to_string(),
    })
}

#[tauri::command]
pub async fn inbox_list(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let dir = state.recv.lock().unwrap().save_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out: Vec<InboxEntry> = vec![];
        let mut total = 0usize;
        // (rel path, size, mtime, incoming) — in-flight temps collapse onto the
        // final name so a receiving file shows up once, growing live.
        let mut incoming: std::collections::HashMap<String, InboxEntry> =
            std::collections::HashMap::new();
        for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|r| r.ok()) {
            if !e.file_type().is_file() {
                continue;
            }
            let fname = e.file_name().to_string_lossy();
            let is_incoming = fname.ends_with(".part") || netcore::is_temp_name(&fname);
            let rel = e.path().strip_prefix(&dir).unwrap_or(e.path());
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
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
            if is_incoming {
                let shown = match strip_temp_suffix(&rel) {
                    Some(s) => s,
                    None => rel.clone(),
                };
                let slot = incoming.entry(shown).or_insert(InboxEntry {
                    path: rel.clone(),
                    size: 0,
                    mtime: 0,
                    incoming: true,
                });
                slot.size += size; // parallel chunks land in separate temps
                slot.mtime = slot.mtime.max(mtime);
                continue;
            }
            total += 1;
            out.push(InboxEntry {
                path: rel,
                size,
                mtime,
                incoming: false,
            });
        }
        let mut inc: Vec<InboxEntry> = incoming.into_values().collect();
        total += inc.len();
        inc.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        let mut files = inc;
        files.extend(out);
        Ok(serde_json::json!({ "total": total, "files": files.into_iter().take(200).collect::<Vec<_>>() }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn inbox_delete(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let dir = state.recv.lock().unwrap().save_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let base = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Receivers only ever write temp names — a temp means some upload is
        // still streaming into it, so refuse rather than yank it mid-flight.
        if netcore::is_temp_name(&base) || path.ends_with(".part") {
            return Err("文件还在接收中，暂时不能删除".into());
        }
        let full = netcore::safe_join(&dir, &path)?;
        let meta = std::fs::symlink_metadata(&full).map_err(|e| format!("{path}: {e}"))?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&full).map_err(|e| e.to_string())?;
            return Ok(());
        }
        std::fs::remove_file(&full).map_err(|e| e.to_string())?;
        // Prune now-empty parent folders (a received directory leaves husks).
        let mut p = full.parent();
        while let Some(d) = p {
            if d == dir || std::fs::remove_dir(d).is_err() {
                break;
            }
            p = d.parent();
        }
        Ok(())
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
