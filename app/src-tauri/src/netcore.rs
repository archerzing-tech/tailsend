// Transfer core: the HTTP receiver (axum) and the streaming sender (reqwest).
// Wire-compatible with the Go CLI in the repo root — same endpoints, headers
// and percent-encoded relative paths, so both ends can talk to each other.
use std::time::Duration;
use axum::{
    extract::{connect_info::ConnectInfo, DefaultBodyLimit, Multipart, State},
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use percent_encoding::{percent_decode_str, percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

pub const DEFAULT_PORT: u16 = 4782;
// Backpressure-bounded channel for webview (JS) file uploads.
pub const DATA_CAP: u64 = 1024 * 1024 * 1024;

// Go's url.PathEscape semantics for a path segment: escape everything but
// unreserved characters; "/" becomes %2F so relative paths survive headers.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

// Data-plane client: no total deadline (a multi-GB stream may legitimately
// run for hours), but connect and per-read stalls are bounded so a hung
// receiver can't freeze a transfer indefinitely.
fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client")
    })
}

// Chunk uploads can legitimately crawl (DERP relay, phone on weak Wi-Fi), so
// this client only guards the connection; the idle timeout is generous — a
// slow-but-alive transfer must never be killed by our own stack.
fn chunk_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client")
    })
}

// Control-plane client (begin/complete/abort): tiny envelopes, so a total
// deadline is safe here.
fn ctrl_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client")
    })
}

pub fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into())
}

// ---------------------------------------------------------------- receiver

#[derive(Clone)]
pub struct RecvCfg {
    pub dir: PathBuf,
    pub token: String,
    pub max: u64,
    pub host: String,
    // In-flight chunked (v2) uploads, shared by the begin/chunk/complete/abort
    // handlers; each upload serializes its own writes behind a tokio mutex.
    pub uploads: UploadRegistry,
}

pub fn router(cfg: RecvCfg) -> Router {
    // Per-route body caps, enforced while the body streams in (a global
    // disable would buffer arbitrary request bodies in memory before any
    // handler check runs). Only the streaming v1 route is unlimited — it
    // enforces `max` itself while writing to disk.
    const CTRL_BODY_MAX: usize = 1 << 20;
    const CHUNK_BODY_MAX: usize = (UPLOAD_CHUNK as usize) + (1 << 20);
    Router::new()
        .route("/", get(info))
        .route("/api/ping", get(ping_info))
        .route("/api/peers", get(peers_info))
        .route("/api/upload", post(upload).layer(DefaultBodyLimit::disable()))
        .route(
            "/api/upload/begin",
            post(upload_begin).layer(DefaultBodyLimit::max(CTRL_BODY_MAX)),
        )
        .route(
            "/api/upload/chunk",
            post(upload_chunk).layer(DefaultBodyLimit::max(CHUNK_BODY_MAX)),
        )
        .route(
            "/api/upload/complete",
            post(upload_complete).layer(DefaultBodyLimit::max(CTRL_BODY_MAX)),
        )
        .route(
            "/api/upload/abort",
            post(upload_abort).layer(DefaultBodyLimit::max(CTRL_BODY_MAX)),
        )
        .with_state(cfg)
}

async fn info() -> Response {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Tailsend receiver is running here.\nUse the Tailsend app or `tailsend send` to send files.\n",
    )
        .into_response()
}

async fn ping_info(
    State(cfg): State<RecvCfg>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    Json(json!({
        "ok": true,
        "name": cfg.host,
        "version": env!("CARGO_PKG_VERSION"),
        "dir": cfg.dir.display().to_string(),
        // The caller's address as this machine sees it. Mobile has no
        // `tailscale status`; probing a peer is how it learns its own
        // tailnet IP to advertise.
        "your_ip": peer.ip().to_string(),
    }))
    .into_response()
}

// Peer-list exchange: CLI-less devices (phones) discover the tailnet by
// asking any reachable receiver for this machine's view of it. Mobile
// receivers answer ok:false — they cannot run `tailscale status`.
async fn peers_info(State(cfg): State<RecvCfg>, headers: HeaderMap) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    match crate::discovery::status().await {
        Ok((self_info, peers)) => Json(json!({
            "ok": true,
            "name": cfg.host,
            "version": env!("CARGO_PKG_VERSION"),
            "self_ip": self_info.ip,
            "peers": peers
                .iter()
                .map(|p| json!({ "name": p.name, "ip": p.ip, "os": p.os, "online": p.online }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => Json(json!({ "ok": false, "error": e, "peers": [] })).into_response(),
    }
}

async fn upload(State(cfg): State<RecvCfg>, headers: HeaderMap, mut mp: Multipart) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    let mut sender = String::new();
    let mut saved_rel = String::new();
    let mut files = 0usize;

    loop {
        let field = match mp.next_field().await {
            Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("bad multipart stream: {e}")),
            Ok(None) => break,
            Ok(Some(f)) => f,
        };
        match field.name().unwrap_or_default() {
            "sender" => {
                if let Ok(bytes) = field.bytes().await {
                    sender = String::from_utf8_lossy(&bytes).trim().to_string();
                }
            }
            "file" => {
                let raw = field.file_name().unwrap_or_default().to_string();
                let rel = percent_decode_str(&raw)
                    .decode_utf8()
                    .map(|s| s.into_owned())
                    .unwrap_or(raw);
                let dest = match safe_join(&cfg.dir, &rel) {
                    Ok(d) => d,
                    Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
                };
                let dest = unique_path(dest);
                if let Some(parent) = dest.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                match save_stream(field, &dest, cfg.max).await {
                    Ok(n) => {
                        files += 1;
                        saved_rel = dest
                            .strip_prefix(&cfg.dir)
                            .unwrap_or(&dest)
                            .display()
                            .to_string();
                        println!(
                            "{} ← {} ({}) from {}",
                            chrono_display(),
                            saved_rel,
                            human(n as i64),
                            if sender.is_empty() { "(unknown)".to_string() } else { sender.clone() }
                        );
                    }
                    Err(SaveErr::TooBig(limit)) => {
                        return err_json(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!("文件超过限制（{}）", human(limit as i64)),
                        );
                    }
                    Err(SaveErr::Io(e)) => {
                        return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
                    }
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    if files == 0 {
        return err_json(StatusCode::BAD_REQUEST, "no file part in request");
    }
    Json(json!({ "ok": true, "saved": saved_rel })).into_response()
}

enum SaveErr {
    TooBig(u64),
    Io(String),
}

// Write to "<dest>.part" and rename on success, so an aborted transfer never
// leaves a half-written file under its final name.
async fn save_stream(
    mut field: axum::extract::multipart::Field<'_>,
    dest: &Path,
    max: u64,
) -> Result<u64, SaveErr> {
    // Unique per upload: two simultaneous receives of the same filename must
    // not share one .part file. The distinctive "tailsend" marker lets the
    // startup sweep recognize crash leftovers without touching the user's
    // own "*.part-1" split archives.
    static PART_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(format!(".part-tailsend-{}", PART_SEQ.fetch_add(1, Ordering::Relaxed)));
    let tmp = PathBuf::from(tmp);
    let mut f = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| SaveErr::Io(e.to_string()))?;
    let mut n: u64 = 0;
    let mut too_big = false;
    loop {
        match field.chunk().await {
            Err(e) => {
                let _ = f.shutdown().await;
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(SaveErr::Io(format!("读取数据流失败：{e}")));
            }
            Ok(None) => break,
            Ok(Some(chunk)) => {
                n += chunk.len() as u64;
                if max > 0 && n > max {
                    too_big = true;
                    break;
                }
                if let Err(e) = f.write_all(&chunk).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(SaveErr::Io(e.to_string()));
                }
            }
        }
    }
    if let Err(e) = f.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(SaveErr::Io(e.to_string()));
    }
    drop(f);
    if too_big {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(SaveErr::TooBig(max));
    }
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| SaveErr::Io(e.to_string()))?;
    Ok(n)
}

// ------------------------------------------------- chunked upload (protocol v2)
//
// begin → parallel chunk posts → complete. Additive to the v1 multipart
// endpoint so older senders keep working; a big file rides several connections
// at once (one TCP stream rarely saturates a long tailnet path) while memory
// stays flat on both ends. Handlers run concurrently — the receiver takes
// simultaneous uploads from multiple senders by design.

pub const UPLOAD_CHUNK: u64 = 4 * 1024 * 1024;
// Sanity cap on declared sizes (Go-side constant twin): a huge declaration
// only ever produces sparse spool files that turn into garbage.
pub const MAX_DECLARED_SIZE: u64 = 1 << 50; // 1 PiB

pub struct Upload {
    pub final_path: PathBuf,
    pub tmp: PathBuf,
    pub size: u64,
    pub chunk: u64,
    pub sender: String,
    pub file: tokio::fs::File,
    pub received: BTreeSet<u64>,
    pub last: std::time::Instant, // last chunk arrival; the janitor reaps idles
}

pub type UploadRegistry = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Upload>>>>>;

static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_upload_id() -> String {
    format!("u{}-{}", std::process::id(), UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn header_get(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
}

async fn upload_begin(
    State(cfg): State<RecvCfg>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    let req: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, format!("bad json: {e}")),
    };
    let name = req.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let size = req.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    let sender = req.get("sender").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "文件名为空");
    }
    if cfg.max > 0 && size > cfg.max {
        return err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("文件超过限制（{}）", human(size as i64)),
        );
    }
    if size > MAX_DECLARED_SIZE {
        return err_json(StatusCode::BAD_REQUEST, "声明的文件大小异常");
    }
    let dest = match safe_join(&cfg.dir, &name) {
        Ok(d) => unique_path(d),
        Err(e) => return err_json(StatusCode::BAD_REQUEST, e),
    };
    if let Some(parent) = dest.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let id = new_upload_id();
    let tmp = dest.with_file_name(format!(
        "{}.tspart-{id}",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    let file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("创建临时文件失败：{e}")),
    };
    cfg.uploads.lock().unwrap().insert(
        id.clone(),
        Arc::new(tokio::sync::Mutex::new(Upload {
            final_path: dest,
            tmp,
            size,
            chunk: UPLOAD_CHUNK,
            sender,
            file,
            received: BTreeSet::new(),
            last: std::time::Instant::now(),
        })),
    );
    Json(json!({ "ok": true, "id": id, "chunk": UPLOAD_CHUNK })).into_response()
}

async fn upload_chunk(
    State(cfg): State<RecvCfg>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    let id = header_get(&headers, "x-upload-id");
    let idx: u64 = match header_get(&headers, "x-chunk-index").parse() {
        Ok(v) => v,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "bad chunk index"),
    };
    let up = cfg.uploads.lock().unwrap().get(&id).cloned();
    let Some(up) = up else {
        return err_json(StatusCode::NOT_FOUND, format!("未知上传 {id}"));
    };
    let mut u = up.lock().await;
    let chunks_total = u.size.div_ceil(u.chunk); // 0 for an empty file
    if idx >= chunks_total {
        return err_json(StatusCode::BAD_REQUEST, format!("chunk {idx} 超出范围（共 {chunks_total}）"));
    }
    let off = idx * u.chunk;
    let room = u.chunk.min(u.size - off);
    // Exactly the chunk's room: a short write would leave a silent zero hole
    // in the sparse spool file and still "complete" fine.
    if body.len() as u64 != room {
        return err_json(StatusCode::BAD_REQUEST, "chunk 数据长度与分块空间不符");
    }
    if let Err(e) = u.file.seek(SeekFrom::Start(off)).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("seek 失败：{e}"));
    }
    if let Err(e) = u.file.write_all(&body).await {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("写入失败：{e}"));
    }
    u.received.insert(idx);
    u.last = std::time::Instant::now();
    Json(json!({ "ok": true, "received": u.received.len() })).into_response()
}

async fn upload_complete(
    State(cfg): State<RecvCfg>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    let id = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    let up = cfg.uploads.lock().unwrap().remove(&id);
    let Some(up) = up else {
        return err_json(StatusCode::NOT_FOUND, format!("未知上传 {id}"));
    };
    let mut u = up.lock().await;
    let want = u.size.div_ceil(u.chunk);
    if u.received.len() as u64 != want {
        let missing = want - u.received.len() as u64;
        // Keep the upload alive so the sender can retry the stragglers.
        cfg.uploads.lock().unwrap().insert(id, up.clone());
        return err_json(StatusCode::CONFLICT, format!("缺少 {missing} 个分块"));
    }
    // The spool must be exactly the declared size before it ships; and on
    // failure the upload goes back in the registry so the sender can retry
    // complete instead of stranding the spool with no way back.
    if let Err(e) = u.file.set_len(u.size).await {
        cfg.uploads.lock().unwrap().insert(id, up.clone());
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("截断失败：{e}"));
    }
    if let Err(e) = u.file.flush().await {
        cfg.uploads.lock().unwrap().insert(id, up.clone());
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("flush 失败：{e}"));
    }
    if let Err(e) = u.file.sync_all().await {
        cfg.uploads.lock().unwrap().insert(id, up.clone());
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("sync 失败：{e}"));
    }
    let (tmp, mut final_path, size, sender) =
        (u.tmp.clone(), u.final_path.clone(), u.size, u.sender.clone());
    drop(u);
    if final_path.exists() {
        final_path = unique_path(final_path); // a sibling landed meanwhile
    }
    if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("组装文件失败：{e}"));
    }
    let saved_rel = final_path
        .strip_prefix(&cfg.dir)
        .unwrap_or(&final_path)
        .display()
        .to_string();
    println!(
        "{} ← {} ({}) from {}",
        chrono_display(),
        saved_rel,
        human(size as i64),
        if sender.is_empty() { "(unknown)".to_string() } else { sender }
    );
    Json(json!({ "ok": true, "path": saved_rel })).into_response()
}

async fn upload_abort(
    State(cfg): State<RecvCfg>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !token_ok(&headers, &cfg.token) {
        return err_json(StatusCode::FORBIDDEN, "invalid or missing token");
    }
    let id = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    let up = cfg.uploads.lock().unwrap().remove(&id);
    let Some(up) = up else {
        return err_json(StatusCode::NOT_FOUND, format!("未知上传 {id}"));
    };
    let tmp = up.lock().await.tmp.clone();
    let _ = tokio::fs::remove_file(&tmp).await;
    Json(json!({ "ok": true })).into_response()
}

fn err_json(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "ok": false, "error": msg.into() }))).into_response()
}

fn token_ok(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    let got = headers
        .get("x-tailsend-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ct_eq(got.as_bytes(), token.as_bytes())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}

// Join rel under base, refusing traversal outside it (Go-safeJoin twin).
pub fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.replace('\\', "/");
    let mut out = base.to_path_buf();
    let mut any = false;
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(format!("拒绝可疑路径 “{rel}”")),
            s => {
                // A Windows drive prefix ("C:/...") makes path resolution jump
                // out of the save directory; refuse it everywhere.
                if is_drive_prefix(s) {
                    return Err(format!("拒绝可疑路径 “{rel}”"));
                }
                out.push(s);
                any = true;
            }
        }
    }
    if any {
        Ok(out)
    } else {
        Err("文件名为空".into())
    }
}

// dest, dest (1), dest (2), ... whichever is free (Go-uniquePath twin).
fn unique_path(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    let dir = dest.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = dest.file_name().unwrap_or_default().to_string_lossy().to_string();
    let (stem, ext) = match dest.extension() {
        Some(e) => (
            dest.file_stem().unwrap_or_default().to_string_lossy().to_string(),
            format!(".{}", e.to_string_lossy()),
        ),
        None => (name.clone(), String::new()),
    };
    for i in 1..10000u32 {
        let cand = dir.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!("{stem} (dup){ext}"))
}

// True for the transient names both implementations write: v2 spools
// (.tspart-…), the Go v1 partial (.part-<12 hex>) and the Rust v1 partial
// (.part-tailsend-<seq>). A user's own "file.part-1" split archives never
// match (Go-side twin: isTempName).
pub fn is_temp_name(name: &str) -> bool {
    if let Some(i) = name.rfind(".tspart-") {
        let s = &name[i + ".tspart-".len()..];
        if is_hex_len(s, 16) || is_rust_upload_id(s) {
            return true;
        }
    }
    if let Some(i) = name.rfind(".part-") {
        let s = &name[i + ".part-".len()..];
        if is_hex_len(s, 12) || s.starts_with("tailsend-") {
            return true;
        }
    }
    false
}

fn is_hex_len(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// Matches the ids this file generates: "u<pid>-<seq>".
fn is_rust_upload_id(s: &str) -> bool {
    match s.split_once('-') {
        Some((a, b)) => {
            a.len() > 1
                && a.starts_with('u')
                && a[1..].bytes().all(|b| b.is_ascii_digit())
                && !b.is_empty()
                && b.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

fn is_drive_prefix(seg: &str) -> bool {
    let b = seg.as_bytes();
    b.len() >= 2
        && b[1] == b':'
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_uppercase())
}

// Delete .tspart-*/.part-* leftovers from a crashed run. Only safe while the
// receiver is not serving — the registry is empty, so nothing live can own
// one of these files (called once from state::start_receiver).
pub fn sweep_spools(dir: &Path) {
    for e in WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if e.file_type().is_file() && is_temp_name(&e.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// Reap uploads whose sender went quiet: without this a crashed sender would
// leave its spool file and registry entry behind forever.
pub async fn janitor(cfg: RecvCfg) {
    const TICK: Duration = Duration::from_secs(600);
    const IDLE: Duration = Duration::from_secs(3600);
    loop {
        tokio::time::sleep(TICK).await;
        let mut stale: Vec<(String, PathBuf)> = Vec::new();
        {
            let reg = cfg.uploads.lock().unwrap();
            for (id, up) in reg.iter() {
                // try_lock: a held lock means chunks are actively arriving.
                if let Ok(u) = up.try_lock() {
                    if u.last.elapsed() > IDLE {
                        stale.push((id.clone(), u.tmp.clone()));
                    }
                }
            }
        }
        for (id, tmp) in stale {
            cfg.uploads.lock().unwrap().remove(&id);
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

// ---------------------------------------------------------------- sender

#[derive(Deserialize, Default)]
struct UploadResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct BeginResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    id: String,
    #[serde(default)]
    chunk: u64,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)] // the peer's ping response; name/dir shown by the CLI twin
pub struct PingInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub version: Option<String>,
    // Our own address as the peer observed it (newer receivers only).
    #[serde(default)]
    pub your_ip: Option<String>,
}

pub async fn ping(addr: &str, token: &str) -> Result<PingInfo, String> {
    let mut req = client().get(format!("http://{addr}/api/ping")).timeout(std::time::Duration::from_secs(5));
    if !token.is_empty() {
        req = req.header("X-Tailsend-Token", token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "在 {addr} 上没有找到 Tailsend 接收端——先在对方机器上启动接收（{e}）"
            ))
        }
    };
    let status = resp.status();
    if status == StatusCode::FORBIDDEN {
        return Err(format!("对方接收端要求共享密钥（token），当前未提供"));
    }
    match resp.json::<PingInfo>().await {
        Ok(p) => Ok(p),
        Err(_) => Err(format!("{addr} 有响应但不是 Tailsend（HTTP {status}）")),
    }
}

/// One node as reported by a peer's `/api/peers`.
#[derive(serde::Deserialize, Clone, Debug)]
pub struct DiscPeer {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub online: bool,
}

// Ask a receiver for its view of the tailnet. Short total timeout: discovery
// sources are best-effort, a wedged one must not stall the whole refresh.
pub async fn fetch_peers(addr: &str, token: &str) -> Result<Vec<DiscPeer>, String> {
    let mut req = ctrl_client().get(format!("http://{addr}/api/peers"));
    req = req.timeout(std::time::Duration::from_secs(10));
    if !token.is_empty() {
        req = req.header("X-Tailsend-Token", token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    #[derive(serde::Deserialize)]
    struct P {
        #[serde(default)]
        peers: Vec<DiscPeer>,
    }
    let v: P = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v.peers)
}

#[derive(Clone)]
pub enum Sink {
    Gui(tauri::AppHandle),
    Std,
}

#[derive(Clone, Serialize)]
pub struct ProgressEvt {
    pub batch: u64,
    pub idx: usize,
    pub rel: String,
    pub loaded: u64,
    pub total: u64,
}

#[derive(Clone, Serialize)]
pub struct FileDoneEvt {
    pub batch: u64,
    pub idx: usize,
    pub rel: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct BatchDoneEvt {
    pub batch: u64,
    pub ok: usize,
    pub failed: usize,
    pub total_bytes: u64,
    pub elapsed_ms: u64,
}

impl Sink {
    pub fn progress(&self, e: &ProgressEvt) {
        match self {
            Sink::Gui(app) => {
                use tauri::Emitter;
                let _ = app.emit("send-progress", e);
            }
            Sink::Std => {
                let line = if e.total > 0 {
                    format!(
                        "\r  ↑ {} / {} ({:.0}%)",
                        human(e.loaded as i64),
                        human(e.total as i64),
                        100.0 * e.loaded as f64 / e.total as f64
                    )
                } else {
                    format!("\r  ↑ {}", human(e.loaded as i64))
                };
                print!("{line}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
    }
    pub fn file_done(&self, e: &FileDoneEvt) {
        match self {
            Sink::Gui(app) => {
                use tauri::Emitter;
                let _ = app.emit("send-file", e);
            }
            Sink::Std => {
                println!();
                match &e.error {
                    Some(err) => println!("  ✗ {}: {err}", e.rel),
                    None => println!("  ✓ {}", e.rel),
                }
            }
        }
    }
    pub fn batch_done(&self, e: &BatchDoneEvt) {
        match self {
            Sink::Gui(app) => {
                use tauri::Emitter;
                let _ = app.emit("send-batch", e);
            }
            Sink::Std => println!("完成：{}/{} 个文件成功", e.ok, e.ok + e.failed),
        }
    }
}

fn emit_step(total: u64) -> u64 {
    std::cmp::max(256 * 1024, total / 200)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub path: String,
    pub rel: String,
    pub size: u64,
}

fn rel_str(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

// Expand paths into entries; directories are walked recursively and each
// file keeps its path relative to the directory's base name.
pub fn collect_entries(paths: &[String]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for p in paths {
        let pb = Path::new(p);
        let md = std::fs::metadata(pb).map_err(|e| format!("{p}: {e}"))?;
        if md.is_file() {
            let name = pb
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            out.push(Entry { path: p.clone(), rel: name, size: md.len() });
        } else if md.is_dir() {
            let prefix = pb
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .ok_or_else(|| format!("无效目录：{p}"))?;
            for e in WalkDir::new(pb).into_iter().filter_map(|r| r.ok()) {
                if !e.file_type().is_file() {
                    continue;
                }
                let rel_path = e.path().strip_prefix(pb).unwrap_or(e.path());
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(Entry {
                    path: e.path().display().to_string(),
                    rel: format!("{prefix}/{}", rel_str(rel_path)),
                    size,
                });
                if out.len() > 5000 {
                    return Err("目录文件数超过 5000，请分批发送".into());
                }
            }
        }
    }
    Ok(out)
}

fn file_progress_state() -> Arc<Mutex<(u64, u64)>> {
    Arc::new(Mutex::new((0, 0))) // (loaded, last_emit)
}

// Stream one on-disk file to the peer: chunked parallel upload (v2) first,
// falling back to a single v1 multipart POST when the receiver predates it.
pub async fn push_file(
    sink: &Sink,
    addr: &str,
    token: &str,
    sender: &str,
    batch: u64,
    idx: usize,
    _total: usize,
    entry: &Entry,
) -> Result<(), String> {
    match push_file_chunked(sink, addr, token, sender, batch, idx, entry).await {
        Ok(true) => Ok(()),
        Ok(false) => push_file_v1(sink, addr, token, sender, batch, idx, 0, entry).await,
        Err(e) => Err(e),
    }
}

// v1 path: single-part multipart POST, streamed straight from disk.
#[allow(clippy::too_many_arguments)]
async fn push_file_v1(
    sink: &Sink,
    addr: &str,
    token: &str,
    sender: &str,
    batch: u64,
    idx: usize,
    _total: usize,
    entry: &Entry,
) -> Result<(), String> {
    let path = PathBuf::from(&entry.path);
    let file = tokio::fs::File::open(&path).await.map_err(|e| format!("打开 {} 失败：{e}", entry.rel))?;
    let size = entry.size;
    let seg = percent_encode(entry.rel.as_bytes(), &SEGMENT).to_string();

    let st = file_progress_state();
    let sink2 = sink.clone();
    let rel2 = entry.rel.clone();
    let stream = ReaderStream::new(file).map(move |chunk| {
        if let Ok(bytes) = &chunk {
            let mut s = st.lock().unwrap();
            s.0 += bytes.len() as u64;
            let loaded = s.0;
            let should = loaded >= size || loaded - s.1 >= emit_step(size);
            if should {
                s.1 = loaded;
                drop(s);
                sink2.progress(&ProgressEvt { batch, idx, rel: rel2.clone(), loaded, total: size });
            }
        }
        chunk
    });

    send_part(sink, addr, token, sender, batch, idx, size, &entry.rel, seg, reqwest::Body::wrap_stream(stream)).await
}

const CHUNK_CONCURRENCY: usize = 4;
const CHUNK_ATTEMPTS: usize = 3;

// v2 path: begin → parallel chunk posts → complete. Ok(false) = the peer's
// receiver predates chunked uploads (404 on begin) — caller falls back to v1.
async fn push_file_chunked(
    sink: &Sink,
    addr: &str,
    token: &str,
    sender: &str,
    batch: u64,
    idx: usize,
    entry: &Entry,
) -> Result<bool, String> {
    let begin = json!({ "name": entry.rel, "size": entry.size, "sender": sender });
    let (status, body) = post_json(addr, token, "/api/upload/begin", &begin).await?;
    if status == StatusCode::NOT_FOUND || status == StatusCode::NOT_IMPLEMENTED {
        return Ok(false);
    }
    let r: BeginResp = serde_json::from_slice(&body)
        .map_err(|_| format!("接收端返回异常（HTTP {status}）"))?;
    if !r.ok || status != StatusCode::OK {
        return Err(r.error.unwrap_or_else(|| format!("接收端拒绝（HTTP {status}）")));
    }

    let size = entry.size;
    // Cap the peer's chunk suggestion so our read buffers stay bounded.
    let chunk = if r.chunk == 0 { UPLOAD_CHUNK } else { r.chunk.min(32 * 1024 * 1024) };
    let nchunks = size.div_ceil(chunk);

    let loaded = Arc::new(AtomicU64::new(0));
    let last_emit = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(tokio::sync::Semaphore::new(CHUNK_CONCURRENCY));
    let mut tasks = Vec::with_capacity(nchunks as usize);
    for ci in 0..nchunks {
        let off = ci * chunk;
        let len = chunk.min(size - off);
        let permit = sem.clone().acquire_owned().await.unwrap();
        let fut = upload_chunk_task(
            addr.to_string(),
            token.to_string(),
            r.id.clone(),
            ci,
            PathBuf::from(&entry.path),
            off,
            len,
            loaded.clone(),
            last_emit.clone(),
            sink.clone(),
            batch,
            idx,
            entry.rel.clone(),
            size,
        );
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            fut.await
        }));
    }

    let mut first_err: Option<String> = None;
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
            Ok(Err(_)) => {}
            Err(e) if first_err.is_none() => first_err = Some(format!("内部错误：{e}")),
            Err(_) => {}
        }
    }
    if let Some(e) = first_err {
        let _ = post_json(addr, token, "/api/upload/abort", &json!({ "id": r.id })).await;
        return Err(e);
    }

    let (status, body) = post_json(addr, token, "/api/upload/complete", &json!({ "id": r.id })).await?;
    let cr: UploadResp = serde_json::from_slice(&body)
        .map_err(|_| format!("接收端返回异常（HTTP {status}）"))?;
    if !cr.ok || status != StatusCode::OK {
        return Err(cr.error.unwrap_or_else(|| format!("接收端组装文件失败（HTTP {status}）")));
    }
    sink.progress(&ProgressEvt { batch, idx, rel: entry.rel.clone(), loaded: size, total: size });
    Ok(true)
}

// POST a JSON envelope and return (status, raw body).
async fn post_json(
    addr: &str,
    token: &str,
    path: &str,
    value: &serde_json::Value,
) -> Result<(StatusCode, bytes::Bytes), String> {
    let mut req = ctrl_client().post(format!("http://{addr}{path}")).json(value);
    if !token.is_empty() {
        req = req.header("X-Tailsend-Token", token);
    }
    let resp = req.send().await.map_err(|e| format!("发送失败：{e}"))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| format!("读取响应失败：{e}"))?;
    Ok((status, body))
}

// Read one chunk from disk and push it (with retries); advances the shared
// byte counter so the progress bar fills as chunks land, in any order.
#[allow(clippy::too_many_arguments)]
async fn upload_chunk_task(
    addr: String,
    token: String,
    id: String,
    ci: u64,
    path: PathBuf,
    off: u64,
    len: u64,
    loaded: Arc<AtomicU64>,
    last_emit: Arc<AtomicU64>,
    sink: Sink,
    batch: u64,
    idx: usize,
    rel: String,
    size: u64,
) -> Result<(), String> {
    let mut data = vec![0u8; len as usize];
    let mut err = String::new();
    for attempt in 1..=CHUNK_ATTEMPTS {
        let read = async {
            let mut f = tokio::fs::File::open(&path).await?;
            f.seek(SeekFrom::Start(off)).await?;
            f.read_exact(&mut data).await
        };
        match read.await {
            Err(e) => err = format!("读取 {} 失败：{e}", rel),
            Ok(_) => {
                let mut req = chunk_client()
                    .post(format!("http://{addr}/api/upload/chunk"))
                    .header("X-Upload-Id", &id)
                    .header("X-Chunk-Index", ci.to_string())
                    .body(data.clone());
                if !token.is_empty() {
                    req = req.header("X-Tailsend-Token", &token);
                }
                match req.send().await {
                    Err(e) => err = format!("发送分块 {ci} 失败：{e}（对方连接中断或不稳定）"),
                    Ok(resp) => {
                        let status = resp.status();
                        let parsed = resp
                            .bytes()
                            .await
                            .ok()
                            .and_then(|b| serde_json::from_slice::<UploadResp>(&b).ok());
                        match parsed {
                            Some(r) if r.ok && status == StatusCode::OK => {
                                let done = loaded.fetch_add(len, Ordering::Relaxed) + len;
                                let step = emit_step(size);
                                let prev = last_emit.load(Ordering::Relaxed);
                                if done >= size || done - prev >= step {
                                    if last_emit
                                        .compare_exchange(prev, done, Ordering::Relaxed, Ordering::Relaxed)
                                        .is_ok()
                                    {
                                        sink.progress(&ProgressEvt {
                                            batch,
                                            idx,
                                            rel: rel.clone(),
                                            loaded: done,
                                            total: size,
                                        });
                                    }
                                }
                                return Ok(());
                            }
                            Some(r) => {
                                err = r.error.unwrap_or_else(|| format!("接收端拒绝分块 {ci}（HTTP {status}）"))
                            }
                            None => err = format!("接收端返回异常（HTTP {status}）"),
                        }
                    }
                }
            }
        }
        if attempt < CHUNK_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
        }
    }
    Err(err)
}

// Stream a webview-provided file (chuncked over IPC) to the peer.
pub async fn push_data(
    sink: &Sink,
    addr: &str,
    token: &str,
    sender: &str,
    batch: u64,
    idx: usize,
    rel: String,
    size: u64,
    rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
) -> Result<(), String> {
    use tokio_stream::wrappers::ReceiverStream;

    let st = file_progress_state();
    let sink2 = sink.clone();
    let rel2 = rel.clone();
    let stream = ReceiverStream::new(rx).map(move |chunk| {
        let mut s = st.lock().unwrap();
        s.0 += chunk.len() as u64;
        let loaded = s.0;
        let should = loaded >= size || loaded - s.1 >= emit_step(size);
        if should {
            s.1 = loaded;
            drop(s);
            sink2.progress(&ProgressEvt { batch, idx, rel: rel2.clone(), loaded, total: size });
        }
        Ok::<bytes::Bytes, std::io::Error>(chunk)
    });

    let seg = percent_encode(rel.as_bytes(), &SEGMENT).to_string();
    send_part(sink, addr, token, sender, batch, idx, size, &rel, seg, reqwest::Body::wrap_stream(stream)).await
}

#[allow(clippy::too_many_arguments)]
async fn send_part(
    sink: &Sink,
    addr: &str,
    token: &str,
    sender: &str,
    batch: u64,
    idx: usize,
    size: u64,
    rel: &str,
    seg: String,
    body: reqwest::Body,
) -> Result<(), String> {
    let mut req = client().post(format!("http://{addr}/api/upload"));
    if !token.is_empty() {
        req = req.header("X-Tailsend-Token", token);
    }
    let part = reqwest::multipart::Part::stream_with_length(body, size).file_name(seg);
    let form = reqwest::multipart::Form::new()
        .text("sender", sender.to_string())
        .part("file", part);
    let resp = req
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("发送失败：{e}"))?;
    let status = resp.status();
    sink.progress(&ProgressEvt { batch, idx, rel: rel.to_string(), loaded: size, total: size });
    let r: UploadResp = resp
        .json()
        .await
        .map_err(|_| format!("接收端返回异常（HTTP {status}）"))?;
    if !r.ok || status != StatusCode::OK {
        return Err(r.error.unwrap_or_else(|| format!("接收端拒绝（HTTP {status}）")));
    }
    Ok(())
}

// ---------------------------------------------------------------- helpers

pub fn human(n: i64) -> String {
    let (k, m, g) = (1i64 << 10, 1i64 << 20, 1i64 << 30);
    let neg = n < 0;
    let n = n.abs();
    let s = match n {
        x if x >= g => format!("{:.2} GB", x as f64 / g as f64),
        x if x >= m => format!("{:.1} MB", x as f64 / m as f64),
        x if x >= k => format!("{:.1} KB", x as f64 / k as f64),
        x => format!("{x} B"),
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn chrono_display() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
