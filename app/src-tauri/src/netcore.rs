// Transfer core: the HTTP receiver (axum) and the streaming sender (reqwest).
// Wire-compatible with the Go CLI in the repo root — same endpoints, headers
// and percent-encoded relative paths, so both ends can talk to each other.
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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

pub const DEFAULT_PORT: u16 = 4782;
// Backpressure-bounded channel for webview (JS) file uploads.
pub const DATA_CAP: u64 = 1024 * 1024 * 1024;

// Go's url.PathEscape semantics for a path segment: escape everything but
// unreserved characters; "/" becomes %2F so relative paths survive headers.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
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
}

pub fn router(cfg: RecvCfg) -> Router {
    Router::new()
        .route("/", get(info))
        .route("/api/ping", get(ping_info))
        .route("/api/upload", post(upload))
        .layer(DefaultBodyLimit::disable())
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
    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(".part");
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
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.replace('\\', "/");
    let mut out = base.to_path_buf();
    let mut any = false;
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(format!("拒绝可疑路径 “{rel}”")),
            s => {
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

// ---------------------------------------------------------------- sender

#[derive(Deserialize, Default)]
struct UploadResp {
    #[serde(default)]
    ok: bool,
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

// Stream one on-disk file to the peer as a single-part multipart POST.
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
