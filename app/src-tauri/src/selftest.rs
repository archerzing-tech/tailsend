// Headless diagnostics sharing the exact GUI code paths:
//   tailsend --selftest serve [--bind A] [--port N] [--dir D] [--token T]
//   tailsend --selftest send [--port N] [--token T] <target> <path>...
//   tailsend --selftest peers
use crate::netcore::{self, BatchDoneEvt, Sink};
use std::path::PathBuf;

pub fn run(args: &[String]) -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(dispatch(args)) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn dispatch(args: &[String]) -> Result<(), i32> {
    match args.first().map(|s| s.as_str()) {
        Some("serve") => serve(&args[1..]).await,
        Some("send") => send(&args[1..]).await,
        Some("peers") => peers().await,
        _ => {
            eprintln!("用法: tailsend --selftest <serve|send|peers> [参数]");
            Err(2)
        }
    }
}

struct Flags {
    bind: Option<String>,
    port: Option<u16>,
    dir: Option<String>,
    token: Option<String>,
    ask: bool,
    rest: Vec<String>,
}

fn parse_flags(args: &[String]) -> Flags {
    let mut f = Flags {
        bind: None,
        port: None,
        dir: None,
        token: None,
        ask: false,
        rest: vec![],
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                i += 1;
                f.bind = args.get(i).cloned();
            }
            "--port" => {
                i += 1;
                f.port = args.get(i).and_then(|s| s.parse().ok());
            }
            "--dir" => {
                i += 1;
                f.dir = args.get(i).cloned();
            }
            "--token" => {
                i += 1;
                f.token = args.get(i).cloned();
            }
            "--ask" => {
                f.ask = true;
            }
            other => f.rest.push(other.to_string()),
        }
        i += 1;
    }
    f
}

async fn serve(args: &[String]) -> Result<(), i32> {
    let f = parse_flags(args);
    let bind = match f.bind.clone() {
        Some(b) => b,
        None => crate::discovery::primary_ip()
            .await
            .unwrap_or_else(|_| "127.0.0.1".into()),
    };
    let port = f.port.unwrap_or(netcore::DEFAULT_PORT);
    let dir = f.dir.map(PathBuf::from).unwrap_or_else(|| {
        dirs::download_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Tailsend")
    });
    let _ = std::fs::create_dir_all(&dir);
    let cfg = netcore::RecvCfg {
        dir,
        token: f.token.unwrap_or_default(),
        max: 0,
        host: netcore::host_name(),
        uploads: std::sync::Arc::default(),
        confirm: f.ask,
        consents: std::sync::Arc::default(),
        pending: std::sync::Arc::default(),
        app: None, // headless: with --ask there is nobody to answer → 403
    };
    let addr = format!("{bind}:{port}");
    let ln = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("监听 {addr} 失败：{e}");
            return Err(1);
        }
    };
    println!(
        "Tailsend selftest receiver on http://{addr}  ->  {}",
        cfg.dir.display()
    );
    if let Err(e) = axum::serve(
        ln,
        netcore::router(cfg).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        eprintln!("serve 退出：{e}");
        return Err(1);
    }
    Ok(())
}

async fn send(args: &[String]) -> Result<(), i32> {
    let f = parse_flags(args);
    let mut it = f.rest.iter();
    let target = match it.next() {
        Some(t) => t.clone(),
        None => {
            eprintln!("用法: tailsend --selftest send [--port N] [--token T] <target> <path>...");
            return Err(2);
        }
    };
    let paths: Vec<String> = it.cloned().collect();
    let entries = match netcore::collect_entries(&paths) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => {
            eprintln!("没有可发送的文件");
            return Err(1);
        }
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };
    let (ip, port) =
        match crate::discovery::find_target(&target, f.port.unwrap_or(netcore::DEFAULT_PORT)).await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return Err(1);
            }
        };
    let addr = format!("{ip}:{port}");
    let token = f.token.unwrap_or_default();
    if let Err(e) = netcore::ping(&addr, &token).await {
        eprintln!("{e}");
        return Err(1);
    }
    let sink = Sink::Std;
    let sender = netcore::host_name();
    // Consent (protocol v3): ask before the first byte moves.
    let req_id = match netcore::request_consent(&addr, &token, &sender, &entries).await {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };
    let n = entries.len();
    println!("发送 {} 个文件到 {addr}", n);
    let mut okc = 0usize;
    let mut failed = 0usize;
    for (i, e) in entries.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, n, e.rel);
        match netcore::push_file(&sink, &addr, &token, &sender, &req_id, 0, i, n, e).await {
            Ok(()) => okc += 1,
            Err(err) => {
                failed += 1;
                eprintln!("  失败：{err}");
            }
        }
    }
    sink.batch_done(&BatchDoneEvt {
        batch: 0,
        ok: okc,
        failed,
        total_bytes: 0,
        elapsed_ms: 0,
    });
    if failed > 0 {
        Err(1)
    } else {
        Ok(())
    }
}

async fn peers() -> Result<(), i32> {
    match crate::discovery::status().await {
        Ok((self_info, peers)) => {
            println!("本机: {} ({})", self_info.name, self_info.ip);
            for p in &peers {
                let mark = if p.online { "●" } else { "○" };
                println!(
                    "  {mark} {:<16} {:<16} {:<8} {}",
                    p.ip, p.name, p.os, p.fqdn
                );
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            Err(1)
        }
    }
}
