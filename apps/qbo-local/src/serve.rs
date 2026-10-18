//! A loopback HTTP front door for [`crate::api`], for developing and testing the
//! UI in an ordinary browser where the Tauri shell is not available.
//!
//! `POST /api/<command>` with a JSON body returns the command's JSON. Anything
//! else is a static file from the built UI. Bound to 127.0.0.1 only and
//! read-only by construction, since [`crate::api`] has no write commands.
//!
//! Deliberately minimal — one connection at a time, no keep-alive. The
//! production front door is Tauri's IPC; this exists so the same UI can be
//! driven by Playwright here, against a synthetic book.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::api::dispatch;
use crate::store::Store;

const MAX_BODY: usize = 1 << 20;

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length.min(MAX_BODY)];
    reader.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok(Some(Request { method: method.to_string(), path, body }))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Resolve a URL path inside `root`, refusing anything that climbs out of it.
fn static_path(root: &Path, url_path: &str) -> Option<PathBuf> {
    let relative = Path::new(url_path.trim_start_matches('/'));
    if relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let candidate = root.join(relative);
    if candidate.is_file() {
        Some(candidate)
    } else {
        // A single-page app: unknown paths get the shell.
        Some(root.join("index.html"))
    }
}

/// Handle one connection.
pub fn handle(stream: &mut TcpStream, store: &Store, ui_root: &Path) -> std::io::Result<()> {
    let Some(request) = read_request(stream)? else {
        return Ok(());
    };

    if let Some(command) = request.path.strip_prefix("/api/") {
        if request.method != "POST" {
            return respond(stream, 405, "text/plain", b"POST only");
        }
        let args: Value = if request.body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(e) => return respond(stream, 400, "text/plain", e.to_string().as_bytes()),
            }
        };
        return match dispatch(store, command, args) {
            Ok(value) => respond(stream, 200, "application/json", value.to_string().as_bytes()),
            Err(error) => {
                let body = serde_json::json!({ "error": error.to_string() }).to_string();
                respond(stream, error.status(), "application/json", body.as_bytes())
            }
        };
    }

    if request.method != "GET" {
        return respond(stream, 405, "text/plain", b"GET only");
    }
    let path = if request.path == "/" { "/index.html" } else { &request.path };
    match static_path(ui_root, path).and_then(|p| std::fs::read(&p).ok().map(|b| (p, b))) {
        Some((file, bytes)) => respond(stream, 200, content_type(&file), &bytes),
        None => respond(
            stream,
            404,
            "text/plain",
            b"UI not built: run `npm install && npm run build` in apps/qbo-local/ui",
        ),
    }
}

/// Serve until the process is stopped.
pub fn run(listener: TcpListener, store: &Store, ui_root: &Path) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(error) = handle(&mut stream, store, ui_root) {
            eprintln!("serve: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::demo::{self, DemoSize, DEMO_REALM};

    fn exchange(port: u16, raw: String) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn api_and_static_files_are_served_and_traversal_is_refused() {
        let ui = tempfile::tempdir().unwrap();
        std::fs::write(ui.path().join("index.html"), "<!doctype html><title>shell</title>").unwrap();
        std::fs::write(ui.path().join("app.js"), "console.log(1)").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = ui.path().to_path_buf();
        let server = thread::spawn(move || {
            let store = Store::open_in_memory().unwrap();
            let size = DemoSize { customers: 5, vendors: 2, items: 5, invoices: 10, purchase_orders: 3 };
            demo::seed(&store, size, 1, Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()).unwrap();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                handle(&mut stream, &store, &root).unwrap();
            }
        });

        let body = format!(r#"{{"realm":"{DEMO_REALM}"}}"#);
        let api = exchange(port, format!(
            "POST /api/status HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        assert!(api.starts_with("HTTP/1.1 200"), "{api}");
        assert!(api.contains("\"writeEnabled\":false"));

        let js = exchange(port, "GET /app.js HTTP/1.1\r\n\r\n".into());
        assert!(js.contains("text/javascript") && js.ends_with("console.log(1)"));

        let deep = exchange(port, "GET /invoices/42 HTTP/1.1\r\n\r\n".into());
        assert!(deep.contains("<title>shell</title>"), "client routes get the shell");

        let climb = exchange(port, "GET /../../etc/passwd HTTP/1.1\r\n\r\n".into());
        assert!(climb.starts_with("HTTP/1.1 404"));

        let unknown = exchange(port, "POST /api/nope HTTP/1.1\r\nContent-Length: 0\r\n\r\n".into());
        assert!(unknown.starts_with("HTTP/1.1 404"));
        server.join().unwrap();
    }
}
