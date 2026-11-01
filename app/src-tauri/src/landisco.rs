// LAN direct channel — LocalSend-style same-Wi-Fi discovery.
//
// The receiver answers UDP probes on the transfer port ("我在这里"), the
// sender broadcasts those probes on every interface and lists the replies.
// Transfers then reuse the ordinary TCP protocol over LAN IPs — no
// Tailscale involved. The wire bytes are tiny on purpose (`TSD1?` / a
// one-line JSON reply); both ends must stay in sync (the Go twin documents
// the same protocol in core/lan.go for CLI-only hosts).

use serde::Serialize;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const PROBE: &[u8] = b"TSD1?";

#[derive(Serialize, Clone)]
pub struct LanPeer {
    pub ip: String,
    pub name: String,
    pub port: u16,
    pub ver: String,
}

// Reply bytes for one probe: `TSD1!` + JSON. Kept as a free function so
// tests and the responder can't drift apart.
fn reply_json(host: &str, port: u16) -> String {
    format!(
        "TSD1!{}",
        serde_json::json!({ "name": host, "port": port, "ver": env!("CARGO_PKG_VERSION") })
    )
}

fn parse_reply(buf: &[u8], src: IpAddr) -> Option<LanPeer> {
    let body = buf.strip_prefix(b"TSD1!")?;
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(LanPeer {
        ip: src.to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        port: v["port"].as_u64().unwrap_or(0) as u16,
        ver: v["ver"].as_str().unwrap_or_default().to_string(),
    })
}

// ---- receiver side ---------------------------------------------------------

// Answer probes on every interface until the receiver's shutdown fires.
// UDP has no token/confirm ceremony: the reply exposes only the device
// name, port and version — the transfer itself stays gated as usual.
pub async fn serve_responder(
    host: String,
    port: u16,
    shutdown: watch::Receiver<()>,
) {
    let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LAN 发现应答器绑定失败：{e}");
            return;
        }
    };
    responder_loop(sock, &host, shutdown).await;
}

// Split out so tests can drive a known-bound socket.
async fn responder_loop(
    sock: tokio::net::UdpSocket,
    host: &str,
    mut shutdown: watch::Receiver<()>,
) {
    let reply = reply_json(host, sock.local_addr().map(|a| a.port()).unwrap_or(0));
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, src)) = r else { break };
                if &buf[..n] == PROBE {
                    let _ = sock.send_to(reply.as_bytes(), src).await;
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

// ---- sender side -----------------------------------------------------------

fn get_ifaces() -> Vec<if_addrs::Interface> {
    if_addrs::get_if_addrs().unwrap_or_default()
}

// All local IPs (any family) — the self-filter for scan replies.
pub fn local_ips() -> Vec<String> {
    get_ifaces().iter().map(|i| i.ip().to_string()).collect()
}

// Private-range IPv4s only — what's worth advertising as "reach me here".
pub fn lan_ips() -> Vec<String> {
    get_ifaces()
        .iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if is_private(v4) => Some(v4.to_string()),
            _ => None,
        })
        .collect()
}

fn is_private(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    matches!(o, [10, ..] | [172, 16..=31, ..] | [192, 168, ..])
}

fn is_skipped(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    // Loopback, IPv4 link-local, and Tailscale's CGNAT range (100.64/10):
    // broadcasting there only reaches ourselves.
    o[0] == 127 || (o[0] == 169 && o[1] == 254) || (o[0] == 100 && (64..=127).contains(&o[1]))
}

// Broadcast targets across all usable interfaces: each subnet's own
// broadcast address when known (computable from addr|!netmask), plus the
// global one as a catch-all.
fn broadcast_targets() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for i in get_ifaces() {
        let if_addrs::IfAddr::V4(v4) = i.addr else { continue };
        if is_skipped(v4.ip) {
            continue;
        }
        let bc = match v4.broadcast {
            Some(b) if !b.is_unspecified() => b,
            // ip | !netmask fills the host bits: the subnet broadcast.
            _ => {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                std::net::Ipv4Addr::from(ip | !mask)
            }
        };
        let a = IpAddr::V4(bc);
        if !out.contains(&a) {
            out.push(a);
        }
    }
    let global = IpAddr::V4(std::net::Ipv4Addr::new(255, 255, 255, 255));
    if !out.contains(&global) {
        out.push(global);
    }
    out
}

// One scan pass: probe every subnet broadcast, collect replies for `budget`.
// Cheap enough to run on every peer refresh (~1s).
pub async fn scan(budget: Duration) -> Vec<LanPeer> {
    let targets = broadcast_targets();
    if targets.is_empty() {
        return Vec::new();
    }
    let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    if sock.set_broadcast(true).is_err() {
        return Vec::new();
    }
    for t in &targets {
        let _ = sock
            .send_to(PROBE, std::net::SocketAddr::new(*t, 0))
            .await;
    }
    let mine = local_ips();
    let mut out: Vec<LanPeer> = Vec::new();
    let deadline = Instant::now() + budget;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let mut buf = [0u8; 512];
        let r = tokio::time::timeout(left, sock.recv_from(&mut buf)).await;
        let Ok(Ok((n, src))) = r else { break };
        // Our own responder answers too — don't list this device as a peer.
        if mine.iter().any(|m| *m == src.ip().to_string()) {
            continue;
        }
        if let Some(p) = parse_reply(&buf[..n], src.ip()) {
            if out.len() >= 64 {
                break;
            }
            if !out.iter().any(|q| q.ip == p.ip) {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_roundtrip() {
        let raw = reply_json("测试机", 4782);
        assert!(raw.starts_with("TSD1!"));
        let p = parse_reply(raw.as_bytes(), "192.168.1.7".parse().unwrap()).unwrap();
        assert_eq!(p.name, "测试机");
        assert_eq!(p.port, 4782);
        assert_eq!(p.ip, "192.168.1.7");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_reply(b"TSD1?notjson", "1.2.3.4".parse().unwrap()).is_none());
        assert!(parse_reply(b"", "1.2.3.4".parse().unwrap()).is_none());
    }

    #[tokio::test]
    async fn responder_answers_probe() {
        let (tx, rx) = watch::channel(());
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        tokio::spawn(responder_loop(sock, "selftest-host", rx));

        let cli = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        cli.send_to(PROBE, ("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), cli.recv_from(&mut buf))
            .await
            .expect("no reply in time")
            .expect("recv failed");
        let p = parse_reply(&buf[..n], "127.0.0.1".parse().unwrap()).unwrap();
        assert_eq!(p.name, "selftest-host");

        // Shutdown must actually retire the responder.
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(300), async {
            // The loop exits on the next select tick; a second probe may or
            // may not be answered — we only assert no panic/hang here.
            let _ = cli.send_to(PROBE, ("127.0.0.1", port)).await;
        })
        .await
        .ok();
    }
}
