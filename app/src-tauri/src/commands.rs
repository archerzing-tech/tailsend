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
    match discovery::status_detailed().await {
        Ok((self_info, peers)) => {
            serde_json::json!({ "self": self_info, "peers": peers, "error": serde_json::Value::Null })
        }
        // `state` tells the UI which fix button to offer: install / start /
        // connect. Absent on success and on uncategorized errors.
        Err(e) => serde_json::json!({
            "self": serde_json::Value::Null,
            "peers": [],
            "error": e.message,
            "state": e.state,
        }),
    }
}

// One LAN direct-channel sweep: broadcast a discovery probe on every
// interface, answer-collect for under a second. The UI merges the replies
// into the peer list next to tailnet devices.
#[tauri::command]
pub async fn lan_scan() -> Result<Vec<crate::landisco::LanPeer>, String> {
    Ok(crate::landisco::scan(std::time::Duration::from_millis(900)).await)
}

// Desktop helpers behind the peers page's Tailscale guidance buttons. Mobile
// runs the embedded tsnet node instead, so none of these apply there.

#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = &url;
        return Err("移动端请在系统浏览器里打开链接".into());
    }
    // Only pass through web links — an unrestricted opener command would be
    // a hole (file://, shell: …).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("只支持打开网页链接".into());
        }
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(target_os = "windows")]
        // `start` needs a placeholder window title so & in URLs survives.
        let r = std::process::Command::new("cmd").args(["/c", "start", "", &url]).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = std::process::Command::new("xdg-open").arg(&url).spawn();
        r.map(|_| ()).map_err(|e| format!("打开浏览器失败：{e}"))
    }
}

#[tauri::command]
pub async fn tailscale_start_service() -> Result<(), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端走内置节点，无需启动 Tailscale".into());
    #[cfg(target_os = "macos")]
    {
        // Launching the app connects its backend if a login is on file.
        let r = tokio::process::Command::new("open").args(["-a", "Tailscale"]).status().await;
        return match r {
            Ok(s) if s.success() => Ok(()),
            _ => Err("没有找到 Tailscale——请先安装 Tailscale".into()),
        };
    }
    #[cfg(target_os = "windows")]
    {
        // The backend runs as a Windows service; starting it needs one
        // elevated prompt. `-Verb runAs` raises it — the user sees UAC once.
        // `Get-Service Tailscale*` spares us guessing the exact service name.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = "Start-Process powershell -Verb runAs -Wait -ArgumentList '-NoProfile','-Command','Get-Service Tailscale* | Start-Service'";
        // A stuck Start-Service would hold this command open forever and the
        // fix button with it; bound the whole thing (kill_on_drop reaps the
        // child when the timeout cancels the future).
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .creation_flags(CREATE_NO_WINDOW)
                .kill_on_drop(true)
                .output()
                .await
        })
        .await;
        let out = match out {
            Ok(r) => r.map_err(|e| format!("无法调用 PowerShell：{e}"))?,
            Err(_) => return Err("启动服务超时：请手动打开 Tailscale 客户端试试".into()),
        };
        if out.status.success() {
            Ok(())
        } else {
            Err("没有拿到管理员授权（取消了弹窗？）——也可以手动打开 Tailscale".into())
        }
    }
    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(any(target_os = "android", target_os = "ios"))
    ))]
    {
        let r = tokio::process::Command::new("pkexec")
            .args(["systemctl", "start", "tailscaled"])
            .status()
            .await;
        match r {
            Ok(s) if s.success() => Ok(()),
            _ => Err("启动 tailscaled 失败——请手动执行：sudo systemctl start tailscaled".into()),
        }
    }
}

#[tauri::command]
pub async fn tailscale_login() -> Result<String, String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    return Err("移动端走内置节点登录，无需这一步".into());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        use std::process::Stdio;
        use tokio::io::AsyncBufReadExt;
        let mut cmd = tokio::process::Command::new(discovery::cli_bin());
        cmd.arg("login")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "windows")]
        {
            // Console program spawned from a GUI app — never flash a window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| format!("无法启动 Tailscale CLI：{e}"))?;
        // `login` prints the auth URL then blocks until the browser flow
        // finishes. We only need the URL: the local backend completes the
        // login on its own, so grabbing it and dropping the CLI is enough.
        let url = {
            let out = child.stdout.take().ok_or("无法读取登录输出")?;
            let mut lines = tokio::io::BufReader::new(out).lines();
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if let Some(u) = line
                            .split_whitespace()
                            .find(|s| s.starts_with("https://login.tailscale.com/"))
                        {
                            break u.to_string();
                        }
                    }
                    // EOF (already logged in) or timeout — nothing to open.
                    _ => break String::new(),
                }
            }
        };
        drop(child); // kill_on_drop — don't leave the CLI hanging around
        Ok(url)
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
) -> Result<serde_json::Value, String> {
    if size > DATA_CAP {
        return Err(format!(
            "通过界面选择的大文件上限为 {}，更大的文件请在桌面端用「打开文件选择器」发送",
            netcore::human(DATA_CAP as i64)
        ));
    }
    // Embedded-node route: no OS VPN, so the Rust sender can't reach the
    // tailnet — stage the bytes to a temp file and let Go do consent +
    // upload through tsnet (send_data_end triggers it). Only tailnet
    // destinations go this way: tsnet has no route for LAN/internet
    // addresses, so feeding it 192.168.x blackholes the send (each file
    // burns the full consent timeout before failing, and the UI sits
    // locked on 「发送中」 the whole time).
    {
        let node = app.state::<NodeState>();
        if needs_node_route(&target) {
            if let Some(path) = node_stage_begin(&app, &state, &node, &name).await? {
                let id = state.next_data_id();
                state
                    .slots
                    .lock()
                    .unwrap()
                    .insert(id, Slot::NodeTemp { file: path.0, path: path.1, target });
                // Tauri ships bare primitives to JS as raw literals
                // (`runCallback(id, 5)`) — `const { id } = 5` yields undefined
                // and every later chunk dies with `invalid args "id"`. Always
                // return an object.
                return Ok(json!({ "id": id }));
            }
            return Err("跨网地址需要内置节点在线：请在「本机」页启动并登录 Tailscale 后再发送".into());
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
    Ok(json!({ "id": id }))
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
            // Kotlin reads paths back with getString() — a JSON *string*
            // holding the array — so pre-encode; a structured array arrives
            // as a JSArray there and reads back empty.
            let r = node
                .call(
                    "send_files",
                    json!({
                        "target": target,
                        "paths": serde_json::to_string(&[path.to_string_lossy()])
                            .unwrap_or_else(|_| "[]".into()),
                        "token": token,
                    }),
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

// True when only the embedded tsnet node can reach the target: tailnet IPs
// (Tailscale CGNAT v4, the fd7a:115c:a1e0::/48 ULA) and hostnames (MagicDNS
// resolves only inside the node). LAN and internet IPs the Rust sender
// reaches directly — parse order mirrors discovery::find_target.
fn needs_node_route(target: &str) -> bool {
    let t = target.trim();
    if let Ok(sa) = t.parse::<std::net::SocketAddr>() {
        return tailnet_ip(sa.ip());
    }
    if let Ok(ip) = t.parse::<std::net::IpAddr>() {
        return tailnet_ip(ip);
    }
    let host = match t.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.parse::<u16>().map(|pp| pp > 0).unwrap_or(false) => h,
        _ => t,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<std::net::IpAddr>().map_or(true, tailnet_ip)
}

fn tailnet_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        std::net::IpAddr::V6(v6) => v6.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
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
    // Sweep stage files from crashed sends before making room.
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
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
    // Declared total for an in-flight file; 0 = unknown (Go-side or v1 temp).
    total: u64,
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
    node: tauri::State<'_, NodeState>,
) -> Result<serde_json::Value, String> {
    // The embedded node's receiver has its own in-flight uploads (that's the
    // receiver phone transfers actually hit); merge its progress into the
    // same temp-name→(got,total) feed so the inbox gets determinate bars.
    #[cfg(target_os = "android")]
    let go_prog: std::collections::HashMap<String, (u64, u64)> = match node
        .call("inbox_progress", json!({}))
        .await
    {
        Ok(v) => v
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, arr)| {
                        let a = arr.as_array()?;
                        Some((k.clone(), (a.first()?.as_u64()?, a.get(1)?.as_u64()?)))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Default::default(),
    };
    #[cfg(not(target_os = "android"))]
    let go_prog: std::collections::HashMap<String, (u64, u64)> = Default::default();
    #[cfg(not(target_os = "android"))]
    let _ = &node; // desktop: the embedded node has no receiver here
    let dir = state.recv.lock().unwrap().save_dir.clone();
    let uploads = state.uploads.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // upload_progress blocking_locks tokio mutexes — it must run here, off
        // the async runtime. Called on a worker thread it panics while holding
        // the registry lock, poisoning it: every later receiver access
        // (begin/chunk/complete/janitor) panics too and ALL transfers die.
        let mut prog = netcore::upload_progress(&uploads);
        prog.extend(go_prog);
        let mut out: Vec<InboxEntry> = vec![];
        // (rel path, size, mtime, incoming) — in-flight temps collapse onto the
        // final name so a receiving file shows up once, growing live.
        let mut incoming: std::collections::HashMap<String, InboxEntry> =
            std::collections::HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for dir in inbox_roots(&app, &dir) {
            scan_inbox_dir(&dir, &prog, &mut out, &mut incoming, &mut seen);
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

// Tauri's app_data_dir() is Context.getDataDir() on Android — the sandbox
// root, NOT getFilesDir(). Everything the tap-to-open FileProvider serves
// (<files-path> = filesDir exactly) must live under filesDir, which is also
// where the Go node keeps its state (TsnetNodePlugin uses filesDir too).
#[cfg(target_os = "android")]
pub fn android_files_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    app.path().app_data_dir().ok().map(|d| d.join("files"))
}

// Every directory the inbox reads: the Rust receiver's save dir plus, on
// Android, the embedded node's inbox (filesDir/inbox — its state dir is filesDir).
fn inbox_roots(app: &tauri::AppHandle, save_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    #[allow(unused_mut)] // the push is cfg'd to android
    let mut roots = vec![save_dir.to_path_buf()];
    #[cfg(target_os = "android")]
    {
        if let Some(d) = android_files_dir(app) {
            roots.push(d.join("inbox"));
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = app;
    roots
}

fn scan_inbox_dir(
    dir: &std::path::Path,
    prog: &std::collections::HashMap<String, (u64, u64)>,
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
            let total = prog.get(fname.as_ref()).map(|(_, t)| *t).unwrap_or(0);
            let slot = incoming.entry(shown).or_insert(InboxEntry {
                path: rel.clone(),
                size: 0,
                total: 0,
                mtime: 0,
                incoming: true,
            });
            slot.size += size; // parallel chunks land in separate temps
            slot.total += total;
            slot.mtime = slot.mtime.max(mtime);
            continue;
        }
        out.push(InboxEntry {
            path: rel,
            size,
            total: 0,
            mtime,
            incoming: false,
        });
    }
}

#[tauri::command]
pub async fn open_inbox(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    node: tauri::State<'_, NodeState>,
    path: Option<String>,
) -> Result<(), String> {
    // The inbox lists paths relative to whichever root it scanned from; hand
    // the UI's rel path back and open the root where the file actually is.
    let hinted = matches!(path.as_deref(), None | Some(""));
    let target = path.as_deref().filter(|p| !p.is_empty()).and_then(|p| {
        inbox_roots(&app, &state.recv.lock().unwrap().save_dir)
            .into_iter()
            .map(|root| root.join(p))
            .find(|c| c.exists())
    });
    // Android has no "open folder" concept — hand a single file to the
    // system viewer via the Kotlin plugin (FileProvider + ACTION_VIEW).
    #[cfg(target_os = "android")]
    {
        if hinted {
            return Err("在收件列表里点文件名即可打开".into());
        }
        let p = match target {
            Some(p) => p,
            None => return Err("文件不存在或已被删除".into()),
        };
        let _: serde_json::Value =
            node.call("open_path", json!({ "path": p.to_string_lossy() })).await?;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let dir = state.recv.lock().unwrap().save_dir.clone();
        let _ = std::fs::create_dir_all(&dir);
        let _ = node; // desktop: no embedded node bridge involved
        let _ = hinted;
        let target = target.unwrap_or(dir);
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").arg(&target).spawn();
        #[cfg(target_os = "windows")]
        let r = std::process::Command::new("explorer").arg(&target).spawn();
        #[cfg(target_os = "linux")]
        let r = std::process::Command::new("xdg-open").arg(&target).spawn();
        #[cfg(target_os = "ios")]
        let r: std::io::Result<()> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "移动端不支持",
        ));
        r.map(|_| ()).map_err(|e| e.to_string())
    }
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
    let token = app.state::<AppState>().recv.lock().unwrap().token.clone();
    // Kotlin reads paths back with getString() — a JSON *string* holding the
    // array — so hand it over pre-encoded instead of as a structured array.
    state
        .call(
            "send_files",
            json!({
                "target": target,
                "paths": serde_json::to_string(&paths).unwrap_or_else(|_| "[]".into()),
                "token": token,
                "sender": netcore::host_name(),
            }),
        )
        .await
}

#[tauri::command]
pub async fn node_open_login(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("open_login", json!({})).await
}

// Tailnet device table straight from the embedded node — mobile has no
// tailscale CLI, so list_peers below can never answer there.
#[tauri::command]
pub async fn node_peers(state: tauri::State<'_, NodeState>) -> Result<serde_json::Value, String> {
    state.call("node_peers", json!({})).await
}

#[cfg(test)]
mod route_tests {
    use super::{needs_node_route, tailnet_ip};

    #[test]
    fn lan_and_internet_targets_dial_directly() {
        // Same-Wi-Fi peers (the LAN discovery channel) must never enter the
        // embedded node — tsnet has no route there and the send stalls.
        assert!(!needs_node_route("192.168.1.5:4782"));
        assert!(!needs_node_route("192.168.1.5"));
        assert!(!needs_node_route("10.0.0.2"));
        assert!(!needs_node_route("172.16.4.9:4782"));
        assert!(!needs_node_route("8.8.8.8:4782"));
        assert!(!needs_node_route("127.0.0.1:4782"));
    }

    #[test]
    fn tailnet_targets_stage_through_the_node() {
        assert!(needs_node_route("100.101.1.2:4782"));
        assert!(needs_node_route("100.64.0.1"));
        assert!(needs_node_route("100.127.255.9:4782"));
        assert!(needs_node_route("fd7a:115c:a1e0:1234::56"));
        assert!(needs_node_route("[fd7a:115c:a1e0::1]:4782"));
        // Names resolve (and route) only through the node's MagicDNS.
        assert!(needs_node_route("magicpc.tail1234.ts.net"));
        assert!(needs_node_route("magicpc.tail1234.ts.net:4782"));
    }

    #[test]
    fn cgnat_boundary_is_exact() {
        assert!(!tailnet_ip("100.63.255.255".parse().unwrap()));
        assert!(tailnet_ip("100.64.0.0".parse().unwrap()));
        assert!(tailnet_ip("100.127.255.255".parse().unwrap()));
        assert!(!tailnet_ip("100.128.0.0".parse().unwrap()));
    }
}
