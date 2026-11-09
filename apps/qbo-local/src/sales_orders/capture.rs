//! The sales-order feed's producer. DECISIONS.md D42, docs/CUTOVER.md.
//!
//! QBO sales orders are only readable with the QBO web app's own session
//! (they are not a v3 entity), so the feed is captured from a signed-in
//! browser tab: [`PAGE_SCRIPT`], pasted into the DevTools console on the
//! Sales Orders list, lists every order on the grid and replays the page's
//! own `GetSalesOrder` GraphQL request once per order. That is web-app
//! traffic; it spends none of the developer app's API quota (D43).
//!
//! The script sends the **raw** commerce answers, unconverted, in a
//! `qbo-sales-order-capture/1` document ([`Capture`]). Converting them to a
//! `qbo-sales-orders/1` feed happens here ([`assemble`]), where it is tested:
//! which field is the v3 item id, which links are invoices, which lines are
//! deleted. A capture becomes a complete feed only when every order the grid
//! listed was read; anything less is reported and never written over a
//! good feed.
//!
//! [`receive`] is the loopback end. QBO's content security policy stops the
//! page fetching `127.0.0.1` directly, so the page opens a relay window
//! served from here, hands it the capture with `postMessage`, and the relay
//! (same origin as this server) POSTs it in. The server takes one capture
//! and stops.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::domain::RealmId;
use crate::sales_orders::{
    FeedLine, FeedSalesOrder, SalesOrder, SalesOrderError, SalesOrderFeed, FEED_FORMAT,
};

/// The `capture` tag the page script writes.
pub const CAPTURE_FORMAT: &str = "qbo-sales-order-capture/1";

/// The console script, with `__PORT__` where the receiver's port goes.
pub const PAGE_SCRIPT: &str = include_str!("../../assets/sales-order-capture.js");

/// The only origin the relay accepts a capture from.
pub const QBO_ORIGIN: &str = "https://qbo.intuit.com";

/// Default loopback port for [`receive`].
pub const DEFAULT_PORT: u16 = 8765;

/// A raw capture is a few hundred kilobytes per hundred orders; this is far
/// past any real one.
const MAX_CAPTURE: usize = 64 << 20;

/// What the page script sends.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    pub capture: String,
    /// The `intuit-company-id` the page's own request carried.
    #[serde(default)]
    pub realm_id: Option<String>,
    pub captured_at: DateTime<Utc>,
    /// Every order id the Sales Orders grid showed, in grid order.
    pub listed: Vec<String>,
    /// The `data.result` of each `GetSalesOrder` answer, untouched.
    pub orders: Vec<Value>,
    #[serde(default)]
    pub errors: Vec<CaptureError>,
}

#[derive(Debug, Deserialize)]
pub struct CaptureError {
    pub id: String,
    pub error: String,
}

/// A capture turned into a feed, with every reason it is not complete.
#[derive(Debug)]
pub struct Assembled {
    /// `complete` is true exactly when `problems` is empty.
    pub feed: SalesOrderFeed,
    pub problems: Vec<String>,
}

/// The console script for a receiver on `port`.
pub fn page_script(port: u16) -> String {
    PAGE_SCRIPT.replace("__PORT__", &port.to_string())
}

/// Turn a raw capture into a `qbo-sales-orders/1` feed for `realm`.
///
/// Refuses outright a document that is not a capture or belongs to another
/// company. Everything short of that becomes a feed, marked complete only
/// when every listed order was read and converted.
pub fn assemble(raw: &str, realm: &RealmId) -> Result<Assembled, SalesOrderError> {
    let capture: Capture = serde_json::from_str(raw)
        .map_err(|err| SalesOrderError::Feed(format!("not a sales-order capture: {err}")))?;
    if capture.capture != CAPTURE_FORMAT {
        return Err(SalesOrderError::Feed(format!(
            "capture format {:?} is not {CAPTURE_FORMAT:?}",
            capture.capture
        )));
    }
    if let Some(page_realm) = &capture.realm_id {
        if page_realm != realm.as_str() {
            return Err(SalesOrderError::Feed(format!(
                "the QBO tab was signed in to company {page_realm}, not {realm}"
            )));
        }
    }

    let mut problems = Vec::new();
    if capture.listed.is_empty() {
        problems.push("the Sales Orders grid listed no orders".to_string());
    }
    for error in &capture.errors {
        problems.push(format!("sales order {}: {}", error.id, error.error));
    }

    let mut orders: Vec<FeedSalesOrder> = Vec::with_capacity(capture.orders.len());
    let mut seen = std::collections::HashSet::new();
    for raw_order in &capture.orders {
        match order_from_commerce(raw_order) {
            Ok(order) => {
                if let Err(err) = SalesOrder::from_feed(&order) {
                    problems.push(err.to_string());
                } else if !seen.insert(order.id.clone()) {
                    problems.push(format!("sales order {} was read twice", order.id));
                } else {
                    orders.push(order);
                }
            }
            Err(message) => problems.push(message),
        }
    }
    let unread: Vec<&str> = capture
        .listed
        .iter()
        .map(|id| id.trim())
        .filter(|id| !seen.contains(*id))
        .collect();
    let failed: std::collections::HashSet<&str> =
        capture.errors.iter().map(|e| e.id.trim()).collect();
    for id in unread {
        if !failed.contains(id) {
            problems.push(format!("sales order {id} was listed but never read"));
        }
    }

    let feed = SalesOrderFeed {
        feed: FEED_FORMAT.to_string(),
        realm_id: Some(realm.as_str().to_string()),
        captured_at: capture.captured_at,
        complete: problems.is_empty(),
        sales_orders: orders,
    };
    Ok(Assembled { feed, problems })
}

/// One commerce `GetSalesOrder` result as a feed order.
pub fn order_from_commerce(raw: &Value) -> Result<FeedSalesOrder, String> {
    let id = id_text(raw.get("id")).ok_or_else(|| "a captured order has no id".to_string())?;
    let issued_at =
        text(raw.get("issuedAt")).ok_or_else(|| format!("sales order {id} has no issuedAt"))?;
    let lines: Vec<FeedLine> = raw
        .get("salesOrderLineItems")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|line| line.get("isDeleted").and_then(Value::as_bool) != Some(true))
                .map(line_from_commerce)
                .collect()
        })
        .unwrap_or_default();
    let mut invoice_ids = invoice_links(raw.get("links"));
    for line in &lines {
        for invoice in &line.invoice_ids {
            if !invoice_ids.contains(invoice) {
                invoice_ids.push(invoice.clone());
            }
        }
    }
    Ok(FeedSalesOrder {
        id,
        order_number: text(raw.get("orderNumber")),
        issued_at,
        customer_id: id_text(raw.get("customerId")),
        customer_name: text(raw.get("customerName")),
        customer_po_number: text(raw.get("customerPONumber")),
        due_date: text(raw.get("dueDate")),
        ship_date: text(raw.get("shipDate")),
        order_status: text(raw.get("orderStatus")),
        payment_status: text(raw.get("paymentStatus")),
        fulfillment_status: text(raw.get("fulfillmentStatus")),
        total_amount: present(raw.get("total")),
        total_discount: present(raw.get("totalDiscount")),
        total_shipping: present(raw.get("totalShipping")),
        memo: text(raw.get("internalNote")),
        version: present(raw.get("version")),
        lines,
        invoice_ids,
    })
}

fn line_from_commerce(raw: &Value) -> FeedLine {
    FeedLine {
        id: id_text(raw.get("id")),
        // The commerce line's `variantId` is the v3 Item id.
        item_id: id_text(raw.get("variantId")),
        description: text(raw.get("description")),
        quantity: present(raw.get("quantity")),
        rate: present(raw.get("unitPrice")),
        amount: present(raw.get("total")),
        taxable: raw.get("taxable").and_then(Value::as_bool),
        quantity_consumed_in_invoices: present(raw.get("quantityConsumedInInvoices")),
        invoice_ids: invoice_links(raw.get("links")),
    }
}

/// The v3 invoice ids among a commerce `links` object's targets, in order,
/// once each. Other target kinds (shipments, payments) are not invoices.
fn invoice_links(links: Option<&Value>) -> Vec<String> {
    let mut ids = Vec::new();
    let targets = links
        .and_then(|l| l.get("targets"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for target in targets {
        let Some(txn) = target.get("targetTxn") else {
            continue;
        };
        if txn.get("type").and_then(Value::as_str) != Some("SALE_INVOICE") {
            continue;
        }
        if let Some(id) = id_text(txn.get("id")) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn present(value: Option<&Value>) -> Option<Value> {
    value.filter(|v| !v.is_null()).cloned()
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Ids arrive as strings or numbers depending on the field.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// What [`receive`] did with the capture it took.
#[derive(Debug)]
pub struct Received {
    pub assembled: Assembled,
    /// Where the feed was written: `out` when complete, otherwise a
    /// `.partial.json` beside it so a good feed is never overwritten.
    pub written: PathBuf,
}

/// The `.partial.json` path beside `out`.
pub fn partial_path(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sales-orders".to_string());
    out.with_file_name(format!("{stem}.partial.json"))
}

/// Assemble `raw` and write it: a complete feed to `out`, anything less to
/// [`partial_path`]. Used by [`receive`] and by `--capture FILE`.
pub fn assemble_and_write(
    raw: &str,
    realm: &RealmId,
    out: &Path,
) -> Result<Received, SalesOrderError> {
    let assembled = assemble(raw, realm)?;
    let written = if assembled.feed.complete {
        out.to_path_buf()
    } else {
        partial_path(out)
    };
    let mut bytes = serde_json::to_vec_pretty(&assembled.feed)
        .map_err(|err| SalesOrderError::Feed(err.to_string()))?;
    bytes.push(b'\n');
    crate::auth::atomic_write(&written, &bytes).map_err(|err| SalesOrderError::Fetch {
        source_name: written.display().to_string(),
        message: err.to_string(),
    })?;
    Ok(Received { assembled, written })
}

fn relay_page(port: u16) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>qbo-local capture</title>
<body style="font:14px system-ui;margin:2em">Waiting for the QBO tab…<script>
const QBO = {QBO_ORIGIN:?};
let taken = false;
window.addEventListener('message', async (e) => {{
  if (e.origin !== QBO || typeof e.data !== 'string') return;
  e.source.postMessage('got', QBO);
  if (taken) return;
  taken = true;
  document.body.textContent = 'Sending ' + e.data.length + ' bytes to qbo-local…';
  let answer;
  try {{
    const r = await fetch('/capture', {{ method: 'POST', headers: {{ 'content-type': 'application/json' }}, body: e.data }});
    answer = await r.text();
  }} catch (err) {{ answer = 'relay failed: ' + err; }}
  document.body.textContent = answer + ' — this window can be closed.';
  e.source.postMessage('result:' + answer, QBO);
}});
</script>
<!-- port {port} -->"#
    )
}

/// Serve the relay on `listener` until one capture arrives, then write it
/// (see [`assemble_and_write`]) and return. A capture for another company
/// is answered with an error and the server keeps waiting.
pub fn receive(listener: TcpListener, realm: &RealmId, out: &Path) -> std::io::Result<Received> {
    let port = listener.local_addr()?.port();
    let own_origin = format!("http://127.0.0.1:{port}");
    for stream in listener.incoming() {
        let mut stream = stream?;
        match handle(&mut stream, port, &own_origin, realm, out) {
            Ok(Some(received)) => return Ok(received),
            Ok(None) => {}
            Err(err) => eprintln!("capture: {err}"),
        }
    }
    Err(std::io::Error::other("listener closed"))
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
    content_type: Option<String>,
    origin: Option<String>,
}

/// Read one request, refusing a body over [`MAX_CAPTURE`] rather than
/// truncating it.
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
    let (method, target) = (method.to_string(), target.to_string());
    let mut length = 0usize;
    let (mut content_type, mut origin) = (None, None);
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
            }
        }
    }
    if length > MAX_CAPTURE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("request body of {length} bytes is over the {MAX_CAPTURE}-byte limit"),
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok(Some(Request {
        method,
        path,
        body,
        content_type,
        origin,
    }))
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
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

fn handle(
    stream: &mut TcpStream,
    port: u16,
    own_origin: &str,
    realm: &RealmId,
    out: &Path,
) -> std::io::Result<Option<Received>> {
    let request = match read_request(stream) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            respond(stream, 413, "text/plain", err.to_string().as_bytes())?;
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => {
            respond(
                stream,
                200,
                "text/html; charset=utf-8",
                relay_page(port).as_bytes(),
            )?;
            Ok(None)
        }
        ("POST", "/capture") => {
            // Only the relay page, served from here, may post a capture.
            if request.origin.as_deref() != Some(own_origin) {
                respond(stream, 403, "text/plain", b"cross-origin request refused")?;
                return Ok(None);
            }
            let json = request.content_type.as_deref().is_some_and(|c| {
                c.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            });
            if !json {
                respond(stream, 415, "text/plain", b"application/json only")?;
                return Ok(None);
            }
            let raw = String::from_utf8_lossy(&request.body);
            match assemble_and_write(&raw, realm, out) {
                Ok(received) => {
                    let message = summary_line(&received);
                    respond(stream, 200, "text/plain; charset=utf-8", message.as_bytes())?;
                    stream.flush()?;
                    Ok(Some(received))
                }
                Err(err) => {
                    respond(
                        stream,
                        400,
                        "text/plain; charset=utf-8",
                        err.to_string().as_bytes(),
                    )?;
                    Ok(None)
                }
            }
        }
        _ => {
            respond(stream, 404, "text/plain", b"not found")?;
            Ok(None)
        }
    }
}

/// One line for the browser and the terminal.
pub fn summary_line(received: &Received) -> String {
    let feed = &received.assembled.feed;
    if feed.complete {
        format!(
            "complete: {} sales orders written to {}",
            feed.sales_orders.len(),
            received.written.display()
        )
    } else {
        format!(
            "NOT complete ({} problem(s)): {} sales orders written to {}",
            received.assembled.problems.len(),
            feed.sales_orders.len(),
            received.written.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn realm() -> RealmId {
        RealmId::parse("9130355180809026").unwrap()
    }

    /// Shaped like a real `GetSalesOrder` result (6 Nov 2026), trimmed.
    fn commerce_order(id: &str, number: &str) -> Value {
        json!({
            "id": id,
            "orderNumber": number,
            "issuedAt": "2026-11-06T05:00:00Z",
            "customerId": "21253",
            "customerName": "TEST SWIM CLUB",
            "customerPONumber": null,
            "dueDate": null,
            "shipDate": null,
            "orderStatus": "OPEN",
            "paymentStatus": "PAID",
            "fulfillmentStatus": "PENDING_FULFILLMENT",
            "total": 258.98,
            "totalDiscount": 0,
            "totalShipping": 0,
            "internalNote": "rush",
            "version": 3,
            "links": { "targets": [
                { "targetTxn": { "type": "SALE_INVOICE", "id": "96488" } },
                { "targetTxn": { "type": "SHIPMENT", "id": "77" } }
            ]},
            "salesOrderLineItems": [
                {
                    "id": "113818918", "variantId": 5338, "type": "PRODUCT",
                    "description": "Floating mat", "quantity": 1, "unitPrice": 179, "total": 179,
                    "taxable": false, "quantityConsumedInInvoices": 1, "isDeleted": false,
                    "links": { "targets": [
                        { "targetTxn": { "type": "SALE_INVOICE", "id": "96488" } },
                        { "targetTxn": { "type": "SALE_INVOICE", "id": "96488" } }
                    ]}
                },
                {
                    "id": "113818919", "variantId": "4900", "type": "PRODUCT",
                    "description": "Kickboard", "quantity": 2, "unitPrice": 39.99, "total": 79.98,
                    "taxable": false, "quantityConsumedInInvoices": 2,
                    "links": { "targets": [ { "targetTxn": { "type": "SALE_INVOICE", "id": "96501" } } ] }
                },
                {
                    "id": "113818920", "variantId": "4267", "description": "Removed line",
                    "quantity": 3, "unitPrice": 39.99, "total": 119.97, "isDeleted": true
                }
            ]
        })
    }

    fn capture(listed: &[&str], orders: Vec<Value>, errors: Value) -> String {
        json!({
            "capture": CAPTURE_FORMAT,
            "realmId": "9130355180809026",
            "capturedAt": "2026-11-06T22:24:17Z",
            "listed": listed,
            "orders": orders,
            "errors": errors,
        })
        .to_string()
    }

    #[test]
    fn a_commerce_order_maps_field_for_field() {
        let order = order_from_commerce(&commerce_order("1490000193", "1192")).unwrap();
        assert_eq!(order.id, "1490000193");
        assert_eq!(order.order_number.as_deref(), Some("1192"));
        assert_eq!(order.customer_id.as_deref(), Some("21253"));
        assert_eq!(order.memo.as_deref(), Some("rush"));
        assert_eq!(order.total_amount, Some(json!(258.98)));
        assert_eq!(order.customer_po_number, None);
        // The deleted line is gone; variantId (number or string) is the item id.
        assert_eq!(order.lines.len(), 2);
        assert_eq!(order.lines[0].item_id.as_deref(), Some("5338"));
        assert_eq!(order.lines[1].item_id.as_deref(), Some("4900"));
        assert_eq!(order.lines[1].rate, Some(json!(39.99)));
        assert_eq!(order.lines[1].amount, Some(json!(79.98)));
        assert_eq!(order.lines[1].quantity_consumed_in_invoices, Some(json!(2)));
        // Invoices only, once each; the order carries its lines' invoices too.
        assert_eq!(order.lines[0].invoice_ids, vec!["96488"]);
        assert_eq!(order.invoice_ids, vec!["96488", "96501"]);
    }

    #[test]
    fn a_full_capture_is_a_complete_feed_the_projection_accepts() {
        let raw = capture(
            &["1490000193", "1490000194"],
            vec![
                commerce_order("1490000193", "1192"),
                commerce_order("1490000194", "1193"),
            ],
            json!([]),
        );
        let assembled = assemble(&raw, &realm()).unwrap();
        assert!(assembled.problems.is_empty(), "{:?}", assembled.problems);
        assert!(assembled.feed.complete);
        assert_eq!(assembled.feed.feed, FEED_FORMAT);
        assert_eq!(assembled.feed.sales_orders.len(), 2);
        // Round-trips through the feed parser the sync uses.
        let text = serde_json::to_string(&assembled.feed).unwrap();
        let parsed = crate::sales_orders::parse_feed(&text, &realm()).unwrap();
        assert_eq!(parsed, assembled.feed);
    }

    #[test]
    fn a_failed_or_missing_order_makes_the_feed_incomplete() {
        let raw = capture(
            &["1490000193", "1490000194", "1490000195"],
            vec![commerce_order("1490000193", "1192")],
            json!([{ "id": "1490000194", "error": "HTTP 401" }]),
        );
        let assembled = assemble(&raw, &realm()).unwrap();
        assert!(!assembled.feed.complete);
        assert_eq!(assembled.problems.len(), 2, "{:?}", assembled.problems);
        assert!(assembled.problems[0].contains("HTTP 401"));
        assert!(assembled.problems[1].contains("1490000195"));
    }

    #[test]
    fn an_empty_grid_is_never_complete() {
        let assembled = assemble(&capture(&[], vec![], json!([])), &realm()).unwrap();
        assert!(!assembled.feed.complete);
    }

    #[test]
    fn a_capture_from_another_company_is_refused() {
        let raw = capture(&["1"], vec![], json!([])).replace("9130355180809026", "123");
        let err = assemble(&raw, &realm()).unwrap_err();
        assert!(err.to_string().contains("company 123"), "{err}");
    }

    #[test]
    fn a_duplicate_read_is_a_problem_not_a_second_order() {
        let raw = capture(
            &["1490000193"],
            vec![
                commerce_order("1490000193", "1192"),
                commerce_order("1490000193", "1192"),
            ],
            json!([]),
        );
        let assembled = assemble(&raw, &realm()).unwrap();
        assert_eq!(assembled.feed.sales_orders.len(), 1);
        assert!(!assembled.feed.complete);
    }

    #[test]
    fn an_incomplete_capture_never_overwrites_the_feed() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sales-orders.json");
        std::fs::write(&out, "good feed").unwrap();
        let raw = capture(
            &["1490000193", "1490000194"],
            vec![commerce_order("1490000193", "1192")],
            json!([]),
        );
        let received = assemble_and_write(&raw, &realm(), &out).unwrap();
        assert_eq!(
            received.written,
            dir.path().join("sales-orders.partial.json")
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "good feed");

        let raw = capture(
            &["1490000193"],
            vec![commerce_order("1490000193", "1192")],
            json!([]),
        );
        let received = assemble_and_write(&raw, &realm(), &out).unwrap();
        assert_eq!(received.written, out);
        let feed = crate::sales_orders::fetch_feed(out.to_str().unwrap(), &realm()).unwrap();
        assert!(feed.complete);
    }

    #[test]
    fn the_page_script_gets_the_port() {
        let script = page_script(9001);
        assert!(script.contains("const PORT = 9001;"));
        assert!(!script.contains("__PORT__"));
        assert!(script.contains(CAPTURE_FORMAT));
    }

    fn http(port: u16, request: &str) -> String {
        use std::io::Read;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn the_receiver_serves_the_relay_refuses_strangers_and_stops_after_one_capture() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("sales-orders.json");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let out_for_thread = out.clone();
        let server = std::thread::spawn(move || receive(listener, &realm(), &out_for_thread));

        let page = http(port, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(page.contains("\"https://qbo.intuit.com\""));

        let body = capture(
            &["1490000193"],
            vec![commerce_order("1490000193", "1192")],
            json!([]),
        );
        let post = |origin: &str| {
            format!(
                "POST /capture HTTP/1.1\r\nHost: x\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let refused = http(port, &post("https://evil.example"));
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(!out.exists());

        let accepted = http(port, &post(&format!("http://127.0.0.1:{port}")));
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        assert!(accepted.contains("complete: 1 sales orders"));

        let received = server.join().unwrap().unwrap();
        assert!(received.assembled.feed.complete);
        assert!(out.exists());
    }
}
