// ---------------------------------------------------------------------------
// Loop dashboard server
//
// A minimal HTTP server on 127.0.0.1 (std::net only, no framework):
//   GET  /            embedded dashboard page
//   GET  /api/loops   {generated_at, awake, loops: [...]}
//   POST /api/awake   {"on": bool} → toggle the keep-awake blocker
//
// Transcript scans are incremental: a shared ScanCache keeps per-file byte
// offsets so each poll only reads what sessions appended since the last one.
// ---------------------------------------------------------------------------

use crate::{awake, loops};
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

pub fn serve(port: u16, roots: Vec<PathBuf>, open: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");
    println!("🔁 Loop dashboard: {url}");
    println!("   Scanning: .ralph loops + ~/.claude/sessions (refresh every few seconds)");

    if open {
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }

    let cache: Arc<Mutex<loops::ScanCache>> = Arc::new(Mutex::new(Default::default()));
    let roots = Arc::new(roots);

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let timeout = Some(std::time::Duration::from_secs(5));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let cache = Arc::clone(&cache);
        let roots = Arc::clone(&roots);
        std::thread::spawn(move || {
            let _ = handle(stream, &roots, &cache);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, roots: &[PathBuf], cache: &Mutex<loops::ScanCache>) -> Result<()> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    // Read until end of headers
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    // The only POST body is a tiny `{"on":bool}`; cap it so a bogus
    // Content-Length can't drive an unbounded read.
    if content_length > 64 * 1024 {
        return respond(&mut stream, 400, "text/plain", "request body too large");
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    match (method, path) {
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", DASHBOARD_HTML),
        ("GET", "/api/loops") => {
            let loops = {
                // Recover the guard through poison — a handler panic must not
                // wedge the endpoint for the server's lifetime.
                let mut c = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loops::collect_loops(roots, &mut c)
            };
            let payload = serde_json::json!({
                "generated_at": chrono::Utc::now().to_rfc3339(),
                "awake": awake_json(),
                "loops": loops,
            });
            respond(&mut stream, 200, "application/json", &payload.to_string())
        }
        ("POST", "/api/awake") => {
            let on = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("on").and_then(serde_json::Value::as_bool));
            let Some(on) = on else {
                return respond(&mut stream, 400, "text/plain", "expected {\"on\": bool}");
            };
            let warning = if on {
                // never lid mode from the web UI — that path needs sudo
                awake::turn_on(None, false).err().map(|e| e.to_string())
            } else {
                match awake::turn_off() {
                    Ok(w) => w,
                    Err(e) => Some(e.to_string()),
                }
            };
            let payload = serde_json::json!({ "awake": awake_json(), "warning": warning });
            respond(&mut stream, 200, "application/json", &payload.to_string())
        }
        _ => respond(&mut stream, 404, "text/plain", "not found"),
    }
}

fn awake_json() -> serde_json::Value {
    match awake::status() {
        Some(s) => serde_json::json!({
            "active": true,
            "since": s.started,
            "until": s.until,
            "lid": s.lid,
            "method": s.method,
        }),
        None => serde_json::json!({ "active": false }),
    }
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &str) -> Result<()> {
    let status = match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        _ => "500 Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status,
        ctype,
        body.len(),
        body
    )?;
    stream.flush()?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
