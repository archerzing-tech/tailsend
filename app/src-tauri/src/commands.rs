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
        Ok((self_info, peers)) => {
            serde_json::json!({ "self": self_info, "peers": peers, "error": serde_json::Value::Null })
        }
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
    Ok(PeerPing {
        online: true,
        name: p.name,
        version: p.version,
        your_ip: p.your_ip,
    })
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
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(json!({ "self_ip": own_ip, "peers": peers }))
}

#[tauri::command]
pub async fn restart_receiver(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    crate::state::stop_receiver(&state);
    crate::state::start_receiver(&app, &state).await
}

// Toggle the confirmation gate and rebind so it takes effect at once.
#[tauri::command]
pub async fn set_confirm(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    confirm: bool,
) -> Result<(), String> {
    {
        let mut rs = state.recv.lock().unwrap();
        if rs.confirm == confirm {
            return Ok(());
        }
        rs.confirm = confirm;
    }
    crate::state::stop_receiver(&state);
    crate::state::start_receiver(&app, &state).await
}

// The webview's answer to an /api/request surfaced as `incoming-request`.
// An unknown id (already answered, or the request timed out) is an error.
#[tauri::command]
pub async fn request_respond(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    accept: bool,
) -> Result<(), String> {
    let tx = state.pending.lock().unwrap().remove(&id);
    match tx {
        Some(tx) => tx.send(accept).map_err(|_| "该请求已结束".to_string()),
        // Not one of the Rust receiver's asks — embedded-node asks use "n…"
        // ids and are answered through the Kotlin bridge instead.
        None => {
            let node = app.state::<tauri_plugin_tsnet::TsnetNode<tauri::Wry>>();
            node.call("respond", json!({ "id": id, "accept": accept }))
                .await
                .map(|_| ())
                .or(Err("该请求已超时或不存在".to_string()))
        }
    }
}

// Store the receiver's shared secret and rebind so it takes effect at once
// (an empty token means "accept anything the tailnet lets through").
#[tauri::command]
pub async fn set_receiver_token(
    app: tauri::AppHandle,
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
    crate::state::start_receiver(&app, &state).await
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
    {
        let _ = window;
        return Err("移动端不支持文件夹选择".to_string());
    }
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

// 队列卡片用 Finder 同款图标：NSWorkspace 按真实路径取系统图标，画进
// 128px 位图（64pt @2x，PNG 只有几 KB，缓存友好）再转 base64。
// AppKit 的图标解析必须跑在主线程——后台线程拿到的一律是通用「?」文档——
// 所以经 run_on_main_thread 执行，用 channel 把结果带回来。
#[tauri::command]
pub fn file_icons(window: tauri::WebviewWindow, paths: Vec<String>) -> Vec<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{
            NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace,
            NSGraphicsContext, NSWorkspace,
        };
        use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSString, NSSize};

        let n = paths.len();
        let (tx, rx) = std::sync::mpsc::channel();
        if window
            .run_on_main_thread(move || {
                use objc2::AnyThread;
                let draw = |p: &str| -> Option<String> {
                    let icon = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(p));
                    let rep = unsafe {
                        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                            NSBitmapImageRep::alloc(),
                            std::ptr::null_mut(),
                            128, 128, 8, 4, true, false,
                            NSDeviceRGBColorSpace,
                            0, 0,
                        )
                    }?;
                    let prev = NSGraphicsContext::currentContext();
                    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
                    NSGraphicsContext::setCurrentContext(Some(&ctx));
                    icon.drawInRect_fromRect_operation_fraction(
                        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(64.0, 64.0)),
                        NSRect::ZERO,
                        NSCompositingOperation::SourceOver,
                        1.0,
                    );
                    NSGraphicsContext::setCurrentContext(prev.as_deref());
                    let empty = NSDictionary::new();
                    let png = unsafe {
                        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty)
                    }?;
                    Some(base64::engine::general_purpose::STANDARD.encode(png.to_vec()))
                };
                let _ = tx.send(paths.iter().map(|p| draw(p.as_str())).collect::<Vec<_>>());
            })
            .is_err()
        {
            return vec![None; n];
        }
        rx.recv().unwrap_or_else(|_| vec![None; n])
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        vec![None; paths.len()]
    }
}

// One consent round trip per target: the whole batch is confirmed together
// so a multi-file send pops a single dialog, not one per file. Returns the
// grant id ("" when the receiver predates consent) for the send calls below.
#[tauri::command]
pub async fn request_send(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    target: String,
    entries: Vec<Entry>,
) -> Result<String, String> {
    if entries.is_empty() {
        return Err("没有可发送的文件".into());
    }
    let _ = app;
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    netcore::ping(&addr, &token).await?;
    let sender = netcore::host_name();
    Ok(netcore::request_consent(&addr, &token, &sender, &entries)
        .await?
        .unwrap_or_default())
}

#[tauri::command]
pub async fn send_files(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    target: String,
    entries: Vec<Entry>,
    req_id: Option<String>,
) -> Result<u64, String> {
    if entries.is_empty() {
        return Err("没有可发送的文件".into());
    }
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    netcore::ping(&addr, &token).await?;
    // Consent: the id comes from request_send; callers that skip it still
    // get one here (covers direct/selftest use).
    let req_id = match req_id {
        Some(v) => v,
        None => {
            let sender0 = netcore::host_name();
            netcore::request_consent(&addr, &token, &sender0, &entries)
                .await?
                .unwrap_or_default()
        }
    };

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
            match netcore::push_file(&sink, &addr, &token, &sender, &req_id, batch, i, n, e).await {
                Ok(()) => {
                    okc += 1;
                    bytes += e.size;
                    sink.file_done(&FileDoneEvt {
                        batch,
                        idx: i,
                        rel: e.rel.clone(),
                        ok: true,
                        error: None,
                    });
                }
                Err(err) => {
                    failed += 1;
                    sink.file_done(&FileDoneEvt {
                        batch,
                        idx: i,
                        rel: e.rel.clone(),
                        ok: false,
                        error: Some(err),
                    });
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
    req_id: Option<String>,
) -> Result<u64, String> {
    if size > DATA_CAP {
        return Err(format!(
            "通过界面选择的大文件上限为 {}，更大的文件请在桌面端用「打开文件选择器」发送",
            netcore::human(DATA_CAP as i64)
        ));
    }
    // Embedded-node route: no OS VPN, so the Rust sender can't reach the
    // tailnet — stage the bytes to a temp file and let Go do consent +
    // upload through tsnet (send_data_end triggers it).
    {
        let node = app.state::<NodeState>();
        if let Some(path) = node_stage_begin(&app, &state, &node, &name).await? {
            let id = state.next_data_id();
            state
                .slots
                .lock()
                .unwrap()
                .insert(id, Slot::NodeTemp { file: path.0, path: path.1, target });
            return Ok(id);
        }
    }
    let (ip, port) = discovery::find_target(&target, netcore::DEFAULT_PORT).await?;
    let addr = format!("{ip}:{port}");
    let token = state.recv.lock().unwrap().token.clone();
    netcore::ping(&addr, &token).await?;
    let req_id = match req_id {
        Some(v) => v,
        None => {
            let entry = vec![netcore::Entry {
                path: String::new(),
                rel: name.clone(),
                size,
            }];
            let sender0 = netcore::host_name();
            netcore::request_consent(&addr, &token, &sender0, &entry)
                .await?
                .unwrap_or_default()
        }
    };

    let id = state.next_data_id();
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let sink = Sink::Gui(app);
    let sender = netcore::host_name();
    tokio::spawn(async move {
        let res = netcore::push_data(
            &sink, &addr, &token, &sender, &req_id, id, 0, name, size, rx,
        )
        .await;
        let _ = done_tx.send(res);
    });
    state
        .slots
        .lock()
        .unwrap()
        .insert(id, Slot::Stream { tx, done: done_rx });
    Ok(id)
}

#[tauri::command]
pub async fn send_data_chunk(
    state: tauri::State<'_, AppState>,
    id: u64,
    b64: String,
) -> Result<(), String> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| format!("数据解码失败：{e}"))?;
    let tx = {
        let mut slots = state.slots.lock().unwrap();
        match slots.get_mut(&id) {
            Some(Slot::NodeTemp { file, .. }) => {
                use std::io::Write;
                file.write_all(&data)
                    .map_err(|e| format!("暂存写入失败：{e}"))?;
                return Ok(());
            }
            Some(Slot::Stream { tx, .. }) => tx.clone(),
            None => return Err(format!("传输 #{id} 不存在或已结束")),
        }
    };
    tx.send(bytes::Bytes::from(data))
        .await
        .map_err(|_| format!("传输 #{id} 已中断"))
}

#[tauri::command]
pub async fn send_data_end(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: u64,
) -> Result<(), String> {
    let slot = state.slots.lock().unwrap().remove(&id);
    match slot {
        None => Err(format!("传输 #{id} 不存在或已结束")),
        Some(Slot::Stream { tx, done }) => {
            drop(tx); // EOF for the stream
            done.await.map_err(|_| format!("传输 #{id} 已中断"))?
        }
        Some(Slot::NodeTemp { file, path, target }) => {
            drop(file);
            let token = app.state::<AppState>().recv.lock().unwrap().token.clone();
            let node = app.state::<NodeState>();
            let r = node
                .call(
                    "send_files",
                    json!({ "target": target, "paths": [path.to_string_lossy()], "token": token }),
                )
                .await?;
            let _ = std::fs::remove_file(&path);
            if let Some(e) = r.get("error").and_then(|v| v.as_str()) {
                return Err(e.to_string());
            }
            if r.get("failed").and_then(|v| v.as_u64()).unwrap_or(0) > 0 {
                let msg = r
                    .get("results")
                    .and_then(|v| v.as_array())
                    .and_then(|a| {
                        a.iter().find_map(|f| {
                            f.get("error").and_then(|e| e.as_str()).map(String::from)
                        })
                    })
                    .unwrap_or_else(|| "发送失败".into());
                return Err(msg);
            }
            Ok(())
        }
    }
}

// When the embedded node owns the route, stage a data-mode upload to a temp
// file; None keeps the normal Rust-sender path. The Go side asks for consent
// and uploads through tsnet once send_data_end fires.
async fn node_stage_begin(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    node: &NodeState,
    name: &str,
) -> Result<Option<(std::fs::File, std::path::PathBuf)>, String> {
    use tauri::Manager;
    let st = node.call("status", json!({})).await.unwrap_or(json!({}));
    if st.get("state").and_then(|s| s.as_str()) != Some("running") {
        return Ok(None);
    }
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("send-tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("临时目录创建失败：{e}"))?;
    let base = name.rsplit('/').next().unwrap_or("file");
    let base = base.rsplit('\\').next().unwrap_or(base);
    let safe: String = base.trim().trim_matches('.').replace(['/', '\\', ':', '\u{0}'], "_");
    let safe = if safe.is_empty() { "file".to_string() } else { safe };
    let path = dir.join(format!("stage-{}-{safe}", state.next_data_id()));
    let file = std::fs::File::create(&path).map_err(|e| format!("临时文件创建失败：{e}"))?;
    Ok(Some((file, path)))
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
pub async fn inbox_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let dir = state.recv.lock().unwrap().save_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out: Vec<InboxEntry> = vec![];
        // (rel path, size, mtime, incoming) — in-flight temps collapse onto the
        // final name so a receiving file shows up once, growing live.
        let mut incoming: std::collections::HashMap<String, InboxEntry> =
            std::collections::HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for dir in inbox_roots(&app, &dir) {
            scan_inbox_dir(&dir, &mut out, &mut incoming, &mut seen);
        }
        let total = seen.len();
        let mut inc: Vec<InboxEntry> = incoming.into_values().collect();
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
pub async fn inbox_delete(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
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
        let mut hit: Option<(std::path::PathBuf, std::path::PathBuf)> = None;
        for root in inbox_roots(&app, &dir) {
            if let Ok(full) = netcore::safe_join(&root, &path) {
                if full.symlink_metadata().is_ok() {
                    hit = Some((full, root));
                    break;
                }
            }
        }
        let Some((full, dir)) = hit else {
            return Err(format!("{path}: 文件不存在"));
        };
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

// Every directory the inbox reads: the Rust receiver's save dir plus, on
// Android, the embedded node's inbox (filesDir/inbox inside app data).
fn inbox_roots(app: &tauri::AppHandle, save_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    #[allow(unused_mut)] // the push is cfg'd to android
    let mut roots = vec![save_dir.to_path_buf()];
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        if let Ok(d) = app.path().app_data_dir() {
            roots.push(d.join("inbox"));
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = app;
    roots
}

fn scan_inbox_dir(
    dir: &std::path::Path,
    out: &mut Vec<InboxEntry>,
    incoming: &mut std::collections::HashMap<String, InboxEntry>,
    seen: &mut std::collections::HashSet<String>,
) {
    for e in walkdir::WalkDir::new(dir).into_iter().filter_map(|r| r.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let fname = e.file_name().to_string_lossy();
        let is_incoming = fname.ends_with(".part") || netcore::is_temp_name(&fname);
        let rel = e.path().strip_prefix(dir).unwrap_or(e.path());
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        if !seen.insert(rel.clone()) {
            continue; // same rel name in both roots — first root wins
        }
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
        out.push(InboxEntry {
            path: rel,
            size,
            mtime,
            incoming: false,
        });
    }
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
    let r: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "移动端不支持",
    ));
    r.map(|_| ()).map_err(|e| e.to_string())
}

// ---- Embedded tsnet node (Android) ----
// Thin wrappers over the plugin's managed bridge; payloads and replies are
// plain JSON matching the Kotlin TsnetNodePlugin command surface. The node
// serves its own receiver on the tailnet, so while it is running the GUI
// routes receiving, pinging and sending through these instead of netcore.

type NodeState = tauri_plugin_tsnet::TsnetNode<tauri::Wry>;

use tauri::Manager;

#[tauri::command]
pub async fn node_status(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("status", json!({})).await
}

#[tauri::command]
pub async fn node_ensure(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("ensure", json!({})).await
}

#[tauri::command]
pub async fn node_start_receiver(
    state: tauri::State<'_, NodeState>,
    token: Option<String>,
    confirm: Option<bool>,
) -> Result<serde_json::Value, String> {
    state
        .call(
            "start_receiver",
            json!({ "token": token.unwrap_or_default(), "confirm": confirm.unwrap_or(true) }),
        )
        .await
}

#[tauri::command]
pub async fn node_stop_receiver(
    state: tauri::State<'_, NodeState>,
) -> Result<serde_json::Value, String> {
    state.call("stop_receiver", json!({})).await
}

#[tauri::command]
pub async fn node_pending(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("pending", json!({})).await
}

#[tauri::command]
pub async fn node_respond(
    state: tauri::State<'_, NodeState>,
    id: String,
    accept: bool,
) -> Result<serde_json::Value, String> {
    state.call("respond", json!({ "id": id, "accept": accept })).await
}

#[tauri::command]
pub async fn node_ping_target(
    state: tauri::State<'_, NodeState>,
    app: tauri::AppHandle,
    target: String,
) -> Result<serde_json::Value, String> {
    let token = app.state::<AppState>().recv.lock().unwrap().token.clone();
    state
        .call("ping_target", json!({ "target": target, "token": token }))
        .await
}

#[tauri::command]
pub async fn node_send_files(
    state: tauri::State<'_, NodeState>,
    app: tauri::AppHandle,
    target: String,
    paths: Vec<String>,
) -> Result<serde_json::Value, String> {
    if paths.is_empty() {
        return Err("没有可发送的文件".into());
    }
    let app_state = app.state::<AppState>();
    let token = app_state.recv.lock().unwrap().token.clone();
    drop(app_state);
    state
        .call(
            "send_files",
            json!({ "target": target, "paths": paths, "token": token }),
        )
        .await
}

#[tauri::command]
pub async fn node_open_login(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("open_login", json!({})).await
}
