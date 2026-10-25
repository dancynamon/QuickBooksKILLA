//! A loopback HTTP front door for [`crate::api`], for developing and testing the
//! UI in an ordinary browser where the Tauri shell is not available.
//!
//! `POST /api/<command>` with a JSON body returns the command's JSON. Anything
//! else is a static file from the built UI. Bound to 127.0.0.1 only. Write
//! commands only queue into the outbox, and only for a company whose writes
//! are on; nothing here sends anything to Intuit.
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
    content_type: Option<String>,
    origin: Option<String>,
    host: Option<String>,
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
    let (mut content_type, mut origin, mut host) = (None, None, None);
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let name = name.trim();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value);
            } else if name.eq_ignore_ascii_case("origin") {
                origin = Some(value);
            } else if name.eq_ignore_ascii_case("host") {
                host = Some(value);
            }
        }
    }
    let mut body = vec![0; length.min(MAX_BODY)];
    reader.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok(Some(Request { method: method.to_string(), path, body, content_type, origin, host }))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
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
        // Any web page the person visits can send a POST to 127.0.0.1. Two
        // checks keep those out now that the API can queue writes: a JSON
        // content type, which a cross-site form or `no-cors` fetch cannot set
        // without a preflight this server never answers; and an Origin, when
        // the browser sends one, that is this server's own.
        let json = request
            .content_type
            .as_deref()
            .is_some_and(|c| c.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return respond(stream, 415, "text/plain", b"application/json only");
        }
        if let Some(origin) = &request.origin {
            let own = request.host.as_deref().is_some_and(|host| {
                let host_name = host.split(':').next().unwrap_or("");
                (host_name == "127.0.0.1" || host_name == "localhost") && origin == &format!("http://{host}")
            });
            if !own {
                return respond(stream, 403, "text/plain", b"cross-origin request refused");
            }
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
pub fn run(listener: TcpListener, store: &Store, ui_root: &Path, mut demo: Option<&mut DemoQbo>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Some(demo) = demo.as_deref_mut() {
            if let Err(error) = demo.tick(store, chrono::Utc::now()) {
                eprintln!("demo drain: {error}");
            }
        }
        if let Err(error) = handle(&mut stream, store, ui_root) {
            eprintln!("serve: {error}");
        }
    }
    Ok(())
}

/// A stand-in QuickBooks for the synthetic book, so the whole write path —
/// queue, drain, land, mirror back — can be driven from the browser with no
/// Intuit account. Seeded from the replica; never used with a real realm.
pub struct DemoQbo {
    mock: crate::client::MockQbo,
    realm: crate::domain::RealmId,
    /// How long a write sits as "Not yet in QuickBooks" before it lands, so
    /// the state is visible rather than gone before the page redraws.
    delay: chrono::Duration,
}

impl DemoQbo {
    pub fn new(store: &Store, realm: &crate::domain::RealmId, delay: chrono::Duration) -> Result<Self, crate::store::StoreError> {
        let mut mock = crate::client::MockQbo::new(chrono::Utc::now());
        let mut highest = 0u64;
        for entity in store.all_entities(realm)? {
            highest = highest.max(entity.qbo_id.parse().unwrap_or(0));
            mock.seed(realm, crate::client::EntityPayload {
                entity_type: entity.entity_type,
                qbo_id: entity.qbo_id,
                sync_token: entity.sync_token,
                last_updated_utc: entity.last_updated_utc,
                is_deleted: entity.is_deleted,
                raw_json: entity.raw_json,
            });
        }
        mock.set_next_id(highest + 1);
        Ok(DemoQbo { mock, realm: realm.clone(), delay })
    }

    /// Drain once queued writes are `delay` old, then mirror what landed
    /// back into the replica the way a sync would.
    pub fn tick(&mut self, store: &Store, now: chrono::DateTime<chrono::Utc>) -> Result<(), String> {
        use crate::outbox::OutboxState;
        if !store.is_write_enabled(&self.realm).map_err(|e| e.to_string())? {
            return Ok(());
        }
        let due = store
            .outbox_records(&self.realm, &[OutboxState::Pending])
            .map_err(|e| e.to_string())?
            .iter()
            .any(|r| now - r.updated_at >= self.delay);
        if !due {
            return Ok(());
        }
        self.mock.advance(now);
        let report = crate::writes::drain_once(store, &self.realm, &mut self.mock, now).map_err(|e| e.to_string())?;
        let mut landed = Vec::new();
        for (entity_type, qbo_id) in report.landed {
            if let Some(payload) = self.mock.get(&self.realm, entity_type, &qbo_id) {
                landed.push(crate::store::MirroredEntity {
                    entity_type,
                    qbo_id: payload.qbo_id.clone(),
                    sync_token: payload.sync_token.clone(),
                    last_updated_utc: payload.last_updated_utc,
                    is_deleted: false,
                    raw_json: as_quickbooks_returns(&payload.raw_json, &payload.qbo_id, &payload.sync_token, entity_type),
                });
            }
        }
        store.apply_batch(&self.realm, &landed, None, now).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Fill in what QuickBooks adds to a body it accepts: the id, the token, and
/// for a document the total and open balance. The synthetic book has no
/// sales tax, so the total is the sum of the lines.
fn as_quickbooks_returns(raw: &serde_json::Value, id: &str, token: &str, entity_type: crate::domain::EntityType) -> serde_json::Value {
    use std::str::FromStr;
    let mut out = raw.clone();
    let Some(object) = out.as_object_mut() else { return out };
    object.insert("Id".into(), id.into());
    object.insert("SyncToken".into(), token.into());
    object.remove("sparse");
    if entity_type.as_document().is_some() {
        // QuickBooks numbers a document left blank; the stand-in uses its id.
        object.entry("DocNumber").or_insert_with(|| id.into());
        let total: rust_decimal::Decimal = object
            .get("Line")
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("Amount"))
            .filter_map(|a| rust_decimal::Decimal::from_str(&a.to_string()).ok())
            .sum();
        let number = serde_json::Number::from_str(&total.normalize().to_string()).expect("decimal");
        object.insert("TotalAmt".into(), serde_json::Value::Number(number.clone()));
        let has_balance = matches!(entity_type, crate::domain::EntityType::Invoice | crate::domain::EntityType::Bill);
        if has_balance && !object.contains_key("Balance") {
            object.insert("Balance".into(), serde_json::Value::Number(number));
        }
    }
    if entity_type == crate::domain::EntityType::Customer {
        object.entry("Active").or_insert(true.into());
    }
    out
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
            for _ in 0..8 {
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

        let unknown = exchange(port, "POST /api/nope HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n".into());
        assert!(unknown.starts_with("HTTP/1.1 404"));

        // A page elsewhere cannot drive the API: no JSON type, or a foreign Origin.
        let form = exchange(port, format!(
            "POST /api/status HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}", body.len()
        ));
        assert!(form.starts_with("HTTP/1.1 415"), "{form}");
        let foreign = exchange(port, format!(
            "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()
        ));
        assert!(foreign.starts_with("HTTP/1.1 403"), "{foreign}");
        let own = exchange(port, format!(
            "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()
        ));
        assert!(own.starts_with("HTTP/1.1 200"), "{own}");
        server.join().unwrap();
    }
}

#[cfg(test)]
mod demo_tests {
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::api::dispatch_at;
    use crate::demo::{self, DemoSize, DEMO_REALM};

    #[test]
    fn a_saved_invoice_lands_and_replaces_its_queued_row() {
        let now = Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap();
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 5, vendors: 2, items: 5, invoices: 10, purchase_orders: 2 };
        demo::seed(&store, size, 2, now).unwrap();
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        store.set_write_enabled(&realm, true).unwrap();
        let mut qbo = DemoQbo::new(&store, &realm, Duration::seconds(2)).unwrap();

        let customer = store.contacts(&realm, crate::domain::ContactType::Customer).unwrap().remove(0).0;
        let draft = json!({ "docType": "Invoice", "contactId": customer, "txnDate": "2026-11-02",
                            "lines": [ { "description": "Kickboard", "qty": "3", "rate": "11.50" } ] });
        let saved = dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": draft }), now).unwrap();
        let local = saved["entityId"].as_str().unwrap().to_string();

        qbo.tick(&store, now + Duration::seconds(1)).unwrap();
        assert_eq!(store.resolve_local_id(&realm, &local).unwrap(), None, "not before the delay");
        qbo.tick(&store, now + Duration::seconds(3)).unwrap();
        let qbo_id = store.resolve_local_id(&realm, &local).unwrap().expect("landed");

        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Invoice" }), now).unwrap();
        let rows = register.as_array().unwrap();
        assert!(rows.iter().all(|r| r["qboId"] != local.as_str()), "the queued row is gone");
        let landed = rows.iter().find(|r| r["qboId"] == qbo_id.as_str()).expect("the real row is there");
        assert_eq!(landed["totalMinor"], 3_450);
        assert!(landed.get("pendingWrite").is_none());
    }
}
