//! A hand-rolled fake Intuit server for `tests/http_client.rs` and
//! `tests/oauth.rs`: a std `TcpListener` on `127.0.0.1:0` (an OS-assigned free
//! port, so the whole suite stays offline and parallel-safe), served on a
//! background thread. Each test supplies its own handler closure — this
//! module is only the HTTP/1.1 plumbing: read the request line and headers,
//! decode the query string, hand a [`FakeRequest`] to the handler, and write
//! back whatever [`FakeResponse`] it returns as JSON.
//!
//! Deliberately not a real HTTP library: the whole point is that
//! `HttpQboClient` is exercised against something that behaves like Intuit
//! without ever leaving the loopback interface.
//!
//! Shared by `tests/http_client.rs` and `tests/oauth.rs`, each its own crate
//! — every field and helper here is used by at least one of them, but never
//! both, so `dead_code` is silenced per compilation rather than treated as a
//! real signal.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

pub struct FakeRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

pub struct FakeResponse {
    pub status: u16,
    pub body: Value,
}

impl FakeResponse {
    pub fn json(status: u16, body: Value) -> Self {
        FakeResponse { status, body }
    }
}

pub struct FakeServer {
    pub base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for FakeServer {
    /// Stops the accept loop and joins the thread, so a test's server never
    /// outlives the test and no two tests can ever cross-talk on a port.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start a fake server on a free loopback port. `handler` decides the
/// response for every request; tests close over a `Mutex`-guarded state to
/// script sequences (a 500 then a 200, a 401 then a 200, and so on).
pub fn start<H>(handler: H) -> FakeServer
where
    H: Fn(&FakeRequest) -> FakeResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Intuit server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("local addr").port();
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(handler);

    let loop_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !loop_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => serve_one(stream, handler.as_ref()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    FakeServer {
        base_url: format!("http://127.0.0.1:{port}"),
        stop,
        handle: Some(handle),
    }
}

fn serve_one(stream: TcpStream, handler: &(dyn Fn(&FakeRequest) -> FakeResponse + Send + Sync)) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap_or(0);
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let body = if content_length > 0 {
        let mut buffer = vec![0u8; content_length];
        reader.read_exact(&mut buffer).ok();
        serde_json::from_slice(&buffer).unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    let (path, query_string) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let request = FakeRequest {
        method,
        path: path.to_string(),
        query: parse_query(query_string),
        headers,
        body,
    };

    let response = handler(&request);
    write_response(reader.into_inner(), &response);
}

fn write_response(mut stream: TcpStream, response: &FakeResponse) {
    let payload = serde_json::to_vec(&response.body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        status_text(response.status),
        payload.len(),
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => out.push(' '),
            b'%' => {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                match std::str::from_utf8(&hex)
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(value) => out.push(value as char),
                    None => out.push('%'),
                }
            }
            other => out.push(other as char),
        }
    }
    out
}

/// A minimal but real-shaped Reports API `TrialBalance` response: a flat
/// `Rows.Row[]` of `Data` rows built from `(id, name, debit, credit)`
/// tuples — no sections, since `apps/qbo-local/src/reports.rs`'s own tests
/// already cover walking nested ones. Enough for the report route tests here
/// to exercise a real client end to end without hand-writing the whole tree
/// per test.
pub fn trial_balance_response(rows: &[(&str, &str, &str, &str)]) -> Value {
    let row_values: Vec<Value> = rows
        .iter()
        .map(|(id, name, debit, credit)| {
            serde_json::json!({
                "type": "Data",
                "ColData": [
                    {"value": name, "id": id},
                    {"value": debit},
                    {"value": credit},
                ]
            })
        })
        .collect();
    serde_json::json!({
        "Header": { "ReportName": "TrialBalance" },
        "Rows": { "Row": row_values }
    })
}

/// A `MetaData.LastUpdatedTime`-bearing entity JSON object, the shape every
/// canned response in these tests builds from.
pub fn entity(id: &str, sync_token: &str, fields: Value, last_updated: &str) -> Value {
    let mut object = fields;
    let map = object
        .as_object_mut()
        .expect("entity fields must be an object");
    map.insert("Id".to_string(), Value::String(id.to_string()));
    map.insert(
        "SyncToken".to_string(),
        Value::String(sync_token.to_string()),
    );
    map.insert(
        "MetaData".to_string(),
        serde_json::json!({ "LastUpdatedTime": last_updated, "CreateTime": last_updated }),
    );
    object
}
