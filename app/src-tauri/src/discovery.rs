// Node discovery through `tailscale status --json`.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Clone)]
pub struct Peer {
    pub name: String,
    pub fqdn: String,
    pub ip: String,
    pub online: bool,
    pub os: String,
}

#[derive(Serialize, Clone)]
pub struct SelfInfo {
    pub name: String,
    pub ip: String,
}

#[derive(Deserialize)]
#[allow(dead_code)] // declared fields document the wire format
struct TsStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Self", default)]
    self_: Option<TsPeer>,
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, TsPeer>,
}

#[derive(Deserialize, Clone, Default)]
#[allow(dead_code)] // declared fields document the wire format
struct TsPeer {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "LastSeen", default)]
    last_seen: String,
}

// The App Store build of Tailscale on macOS doesn't put the CLI on PATH.
// Also used by the login/service-fix commands.
pub fn cli_bin() -> String {
    let exe = if cfg!(windows) {
        "tailscale.exe"
    } else {
        "tailscale"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return cand.display().to_string();
            }
        }
    }
    // Windows GUI installs usually put the CLI on PATH, but Store/zip
    // deployments don't — check the standard installer location.
    #[cfg(target_os = "windows")]
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let cand = std::path::Path::new(&pf).join("Tailscale").join(&exe);
        if cand.is_file() {
            return cand.display().to_string();
        }
    }
    let fallbacks = [
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/usr/bin/tailscale",
    ];
    for c in fallbacks {
        if std::path::Path::new(c).is_file() {
            return c.to_string();
        }
    }
    exe.to_string()
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub async fn status() -> Result<(SelfInfo, Vec<Peer>), String> {
    Err("移动端无法读取本机 Tailscale 节点列表，请手动输入对方的 IP 或主机名".into())
}

// Discovery failed — but with a reason the UI can act on: `state` picks the
// peers page's guidance button (install / start / connect), `message` is the
// one-liner for the banner.
#[derive(Serialize, Clone)]
pub struct DiscoveredErr {
    pub state: String, // "cli_missing" | "daemon_down" | "logged_out" | "error"
    pub message: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub async fn status_detailed() -> Result<(SelfInfo, Vec<Peer>), DiscoveredErr> {
    status()
        .await
        .map_err(|e| DiscoveredErr { state: "error".into(), message: e })
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn status() -> Result<(SelfInfo, Vec<Peer>), String> {
    status_detailed().await.map_err(|e| e.message)
}

// Same as status(), but the failure says which link in the chain broke — CLI
// absent, backend service down, or tailnet login missing.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub async fn status_detailed() -> Result<(SelfInfo, Vec<Peer>), DiscoveredErr> {
    let mut cmd = tokio::process::Command::new(cli_bin());
    cmd.args(["status", "--json"]);
    // The UI polls this every 15s — a wedged backend (sleep/wake, hung
    // tailscaled pipe) must not leave the poll awaiting forever, or the
    // device list freezes with no error at all. Bound it and kill the child.
    cmd.kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        // The CLI is a console program and the UI polls this constantly —
        // without CREATE_NO_WINDOW every poll flashes a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output())
        .await
        .map_err(|_| DiscoveredErr {
            state: "daemon_down".into(),
            message: "Tailscale 服务没有响应（8 秒超时）：重启一下 Tailscale 客户端试试".to_string(),
        })?
        .map_err(|_| DiscoveredErr {
            state: "cli_missing".into(),
            message: "没有找到 Tailscale 命令行（CLI）：请确认已安装并至少打开过一次 Tailscale 客户端"
                .to_string(),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let low = stderr.to_lowercase();
        return Err(if low.contains("logged out") {
            DiscoveredErr { state: "logged_out".into(), message: "Tailscale 还没连接（未登录）".into() }
        } else if low.contains("unable to connect")
            || low.contains("failed to connect")
            || low.contains("no such file")
            || low.contains("pipe")
        {
            DiscoveredErr { state: "daemon_down".into(), message: "Tailscale 服务没在运行".into() }
        } else {
            DiscoveredErr {
                state: "error".into(),
                message: format!("tailscale status 失败：{}", stderr.trim()),
            }
        });
    }
    let st: TsStatus = serde_json::from_slice(&out.stdout)
        .map_err(|e| DiscoveredErr { state: "error".into(), message: format!("解析 tailscale status 输出失败：{e}") })?;

    // Backend reachable but the tailnet login isn't: "Stopped" is a user
    // disconnect, "NoState" a fresh install that never connected, "NeedsLogin"
    // an expired/missing session. All three mean: no node list exists yet.
    if matches!(st.backend_state.as_str(), "NeedsLogin" | "Stopped" | "NoState") {
        return Err(DiscoveredErr {
            state: "logged_out".into(),
            message: "Tailscale 还没连接（未登录）".into(),
        });
    }

    // Grab before `self_` is consumed for SelfInfo below.
    let self_ips = st.self_.as_ref().map(|s| s.tailscale_ips.clone()).unwrap_or_default();

    let self_info = st
        .self_
        .and_then(|p| node_from(&p, true))
        .map(|n| SelfInfo {
            name: n.name,
            ip: n.ip,
        });

    let self_info = match self_info {
        Some(s) if !s.name.is_empty() => s,
        _ => SelfInfo {
            name: crate::netcore::host_name(),
            ip: String::new(),
        },
    };

    // Some tailscale builds keep this machine inside `Peer` as well — the
    // CLI's own status on macOS does. Sending to yourself isn't a thing, so
    // keep the self entry out of the list (the UI shows 本机 separately).
    let mut peers: Vec<Peer> = st
        .peers
        .values()
        .filter_map(|p| node_from(p, false))
        .filter(|p| !p.ip.is_empty() && !self_ips.iter().any(|s| *s == p.ip))
        .collect();
    peers.sort_by(|a, b| match (a.online, b.online) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok((self_info, peers))
}

fn node_from(p: &TsPeer, self_: bool) -> Option<Peer> {
    if p.host_name.is_empty() && p.dns_name.is_empty() {
        return None;
    }
    let fqdn = p.dns_name.trim_end_matches('.').to_string();
    let name = if p.host_name.is_empty() {
        fqdn.split('.').next().unwrap_or("").to_string()
    } else {
        p.host_name.clone()
    };
    let ip = primary_of(&p.tailscale_ips).unwrap_or_default();
    let _ = self_;
    Some(Peer {
        name,
        fqdn,
        ip,
        online: p.online,
        os: p.os.clone(),
    })
}

// Prefer the IPv4 tailnet address.
pub fn primary_of(ips: &[String]) -> Option<String> {
    ips.iter()
        .find(|ip| !ip.contains(':'))
        .cloned()
        .or_else(|| ips.first().cloned())
}

pub async fn primary_ip() -> Result<String, String> {
    let (s, _) = status().await?;
    if s.ip.is_empty() {
        return Err("本机没有 Tailscale IP——请确认 Tailscale 已连接（稍后会自动重试）".into());
    }
    Ok(s.ip)
}

// Resolve a user-supplied target to (ip-or-host, port).
// IP literals — with or without ":port" — are accepted without discovery.
pub async fn find_target(target: &str, default_port: u16) -> Result<(String, u16), String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("请先选择一个节点，或输入对方的 IP".into());
    }
    if let Ok(sa) = t.parse::<std::net::SocketAddr>() {
        return Ok((sa.ip().to_string(), sa.port()));
    }
    // Bare IP literal (incl. IPv6, which must come before host:port splitting).
    if let Ok(ip) = t.parse::<std::net::IpAddr>() {
        return Ok((ip.to_string(), default_port));
    }
    if let Some((h, p)) = t.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            if port > 0 && !h.is_empty() {
                if let Ok(ip) = h.parse::<std::net::IpAddr>() {
                    return Ok((ip.to_string(), port));
                }
                // hostname:port — resolve the host part through discovery
                let (_, peers) = status().await?;
                let hl = h.trim_end_matches('.').to_lowercase();
                if let Some(p2) = peers.iter().find(|pp| {
                    pp.name.eq_ignore_ascii_case(&hl)
                        || pp.fqdn.eq_ignore_ascii_case(&hl)
                        || pp.name.to_lowercase().contains(&hl)
                }) {
                    return Ok((p2.ip.clone(), port));
                }
                return Err(format!("没有匹配 “{h}” 的节点"));
            }
        }
    }
    let (_, peers) = status().await?;
    let q = t.trim_end_matches('.').to_lowercase();
    for p in &peers {
        if p.name.eq_ignore_ascii_case(&q) || p.fqdn.eq_ignore_ascii_case(&q) {
            return Ok((p.ip.clone(), default_port));
        }
    }
    let hits: Vec<&Peer> = peers
        .iter()
        .filter(|p| p.name.to_lowercase().contains(&q) || p.fqdn.to_lowercase().contains(&q))
        .collect();
    match hits.len() {
        1 => Ok((hits[0].ip.clone(), default_port)),
        0 => Err(format!(
            "没有匹配 “{t}” 的节点——查看节点列表，或直接输入对方 IP"
        )),
        _ => Err(format!(
            "“{t}” 有多个匹配：{}",
            hits.iter()
                .map(|h| h.name.clone())
                .collect::<Vec<_>>()
                .join("、")
        )),
    }
}
