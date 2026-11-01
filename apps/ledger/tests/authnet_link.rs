//! The hosted payment link, end to end and offline (D30, `docs/AUTHNET.md`):
//! an invoice entered through `create_document`, a `payment_link` call
//! against [`MockAuthnet`] carrying that invoice's number and open balance,
//! then a synthetic settlement CSV row for the same number imported through
//! [`authnet::import_settlement`] — and the invoice leaves the AR aging. Plus
//! [`HttpAuthnetClient`] against a loopback fake gateway, so the wire shape is
//! proven without ever reaching Authorize.net.
//!
//! Names and figures are invented.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{Duration, Utc};
use serde_json::{json, Value};

use ledger::authnet::link::{
    self, Credentials, HttpAuthnetClient, LinkConfig, LinkError, LinkRequest, MockAuthnet,
    StaticCredentials,
};
use ledger::authnet::{self, AuthnetConfig};
use ledger::books::{self, Book};
use ledger::mcp::{self, Server};
use ledger::store::{CommandMeta, Ledger};
use ledger::types::{AccountId, ClassId, LedgerDocument};
use ledger_core::Money;

const COMPANY: &str = "aquamentor";

fn seed_ledger() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(COMPANY, "Test Co", None, Utc::now())
        .unwrap();
    ledger
}

fn call_tool(server: &mut Server, id: i64, name: &str, arguments: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
    .to_string();
    serde_json::from_str(&server.handle_line(&line).unwrap()).unwrap()
}

fn ok(response: &Value) -> &Value {
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    &response["result"]["structuredContent"]
}

fn error_text(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A net-30 customer whose id the gateway accepts as `customer.id`, and one
/// taxable item.
fn seed_master_data(server: &mut Server) {
    ok(&call_tool(
        server,
        1,
        "upsert_contact",
        json!({ "contact": {
            "contact_id": "cpool", "kind": "Customer", "display_name": "Pool Co",
            "email": "ap@pool.example", "terms_days": 30, "default_class": "foam"
        }}),
    ));
    ok(&call_tool(
        server,
        2,
        "upsert_item",
        json!({ "item": {
            "item_id": "mat", "sku": "MAT-1", "name": "Swim mat",
            "item_type": "non_inventory", "income_account": "4100",
            "expense_account": "6100", "default_class": "foam", "sales_price": "25.00"
        }}),
    ));
}

fn create_invoice(server: &mut Server, id: i64, amount: &str) -> (String, String, String) {
    let created = ok(&call_tool(
        server,
        id,
        "create_document",
        json!({ "actor": "dan", "document": {
            "kind": "Invoice", "txn_date": "2026-09-01",
            "contact": { "kind": "Customer", "id": "cpool" },
            "lines": [{ "kind": "Item", "item_id": "mat", "amount": amount, "is_taxable": true }]
        }}),
    ))
    .clone();
    (
        created["document_id"].as_str().unwrap().to_string(),
        created["number"].as_str().unwrap().to_string(),
        created["total"].as_str().unwrap().to_string(),
    )
}

fn authnet_config() -> AuthnetConfig {
    AuthnetConfig {
        clearing: AccountId("1160".into()),
        fees: AccountId("6700".into()),
        undeposited: AccountId("1150".into()),
        refund_class: ClassId("drop".into()),
        fee_class: ClassId("drop".into()),
    }
}

#[test]
fn invoice_to_link_to_settlement_clears_the_ar_aging() {
    let ledger = seed_ledger();
    let mock = MockAuthnet::new();
    let mut server = mcp::new_server_with_payments(&ledger, COMPANY.into(), Box::new(mock.clone()));
    seed_master_data(&mut server);

    let (invoice_id, number, total) = create_invoice(&mut server, 3, "100.00");
    assert_eq!(total, "106.63");

    // A $40 payment already applied: the link charges what is still open.
    ok(&call_tool(
        &mut server,
        4,
        "create_document",
        json!({ "actor": "dan", "document": {
            "kind": "Payment", "txn_date": "2026-09-10",
            "contact": { "kind": "Customer", "id": "cpool" },
            "applications": [{ "target_document_id": invoice_id,
                               "target_kind": "Invoice", "amount": "40.00" }]
        }}),
    ));

    let before = Utc::now();
    let link = ok(&call_tool(
        &mut server,
        5,
        "payment_link",
        json!({ "document_id": invoice_id }),
    ))
    .clone();
    assert_eq!(link["invoice_number"], json!(number));
    assert_eq!(link["amount"], json!("66.63"));
    assert_eq!(link["environment"], json!("sandbox"));
    assert_eq!(
        link["url"],
        json!("https://test.authorize.net/payment/payment")
    );
    assert_eq!(link["token"], json!("mock-token-1"));
    let expires: chrono::DateTime<Utc> = link["expires_at"].as_str().unwrap().parse().unwrap();
    assert!(expires >= before + Duration::minutes(15));
    assert!(expires <= Utc::now() + Duration::minutes(15));
    assert!(link["html"]
        .as_str()
        .unwrap()
        .contains("value=\"mock-token-1\""));

    // What went to the gateway: this invoice's number and open balance.
    let sent = mock.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].invoice_number, number);
    assert_eq!(sent[0].amount, Money::from_minor(6_663));
    assert_eq!(sent[0].customer_id.as_deref(), Some("cpool"));
    assert_eq!(sent[0].customer_email.as_deref(), Some("ap@pool.example"));
    assert_eq!(sent[0].description, format!("Invoice {number} - Pool Co"));

    let open = books::open_documents(
        &ledger,
        COMPANY,
        Book::Receivable,
        "2026-10-15".parse().unwrap(),
    )
    .unwrap();
    assert_eq!(open.len(), 1);

    // The customer paid on the hosted page; the batch settled, and the
    // Merchant Interface export carries the same invoice number back.
    let csv = format!(
        "Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount,Invoice Number,Customer ID\n\
         60001,Settled Successfully,9/20/2026,b77,{},{number},cpool\n",
        link["amount"].as_str().unwrap()
    );
    let report = authnet::import_settlement(
        &ledger,
        COMPANY,
        &csv,
        &authnet_config(),
        "test",
        Utc::now(),
    )
    .unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(report.applied, 1, "{report:?}");
    assert_eq!(report.unapplied, 0, "{report:?}");
    assert!(report.rejected.is_empty(), "{report:?}");

    let aging = ok(&call_tool(
        &mut server,
        6,
        "aging",
        json!({ "book": "receivable", "as_of": "2026-10-15" }),
    ))
    .clone();
    assert_eq!(aging["totals"]["total"], json!("0.00"), "{aging}");
    assert_eq!(aging["rows"], json!([]));
    let open = books::open_documents(
        &ledger,
        COMPANY,
        Book::Receivable,
        "2026-10-15".parse().unwrap(),
    )
    .unwrap();
    assert!(open.is_empty(), "{open:?}");

    // Paid in full now: a second link is refused, and the gateway is not asked.
    let text = error_text(&call_tool(
        &mut server,
        7,
        "payment_link",
        json!({ "document_id": invoice_id }),
    ));
    assert!(text.contains("fully paid"), "{text}");
    assert_eq!(mock.requests().len(), 1);

    // Re-importing the same file posts nothing new.
    let again = authnet::import_settlement(
        &ledger,
        COMPANY,
        &csv,
        &authnet_config(),
        "test",
        Utc::now(),
    )
    .unwrap();
    assert_eq!(again.skipped, 2);
    assert_eq!(again.applied, 0);
}

/// One invoice of $100.00 + tax = $106.63, open on the receivable book.
fn invoice_and_server(ledger: &Ledger) -> (Server<'_>, String, String) {
    let mut server =
        mcp::new_server_with_payments(ledger, COMPANY.into(), Box::new(MockAuthnet::new()));
    seed_master_data(&mut server);
    let (invoice_id, number, total) = create_invoice(&mut server, 3, "100.00");
    assert_eq!(total, "106.63");
    (server, invoice_id, number)
}

fn settlement_csv(rows: &[(&str, &str, &str)]) -> String {
    let mut csv = String::from(
        "Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount,Invoice Number,Customer ID\n",
    );
    for (txn, amount, number) in rows {
        csv.push_str(&format!(
            "{txn},Settled Successfully,9/20/2026,b88,{amount},{number},cpool\n"
        ));
    }
    csv
}

fn import(ledger: &Ledger, csv: &str) -> authnet::ImportReport {
    let report =
        authnet::import_settlement(ledger, COMPANY, csv, &authnet_config(), "test", Utc::now())
            .unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");
    report
}

fn payment(ledger: &Ledger, txn: &str) -> LedgerDocument {
    let payload = ledger
        .latest_document_payload(COMPANY, &format!("authnet:txn:{txn}"))
        .unwrap()
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

fn applied(doc: &LedgerDocument) -> Money {
    Money::checked_sum(doc.applications.iter().map(|a| a.amount)).unwrap()
}

fn open_on(ledger: &Ledger, invoice_id: &str) -> Money {
    books::open_documents(
        ledger,
        COMPANY,
        Book::Receivable,
        "2026-12-31".parse().unwrap(),
    )
    .unwrap()
    .into_iter()
    .find(|d| d.document_id == invoice_id)
    .map(|d| d.open)
    .unwrap_or(Money::ZERO)
}

#[test]
fn an_exact_payment_applies_in_full() {
    let ledger = seed_ledger();
    let (_server, invoice_id, number) = invoice_and_server(&ledger);
    let report = import(&ledger, &settlement_csv(&[("70001", "106.63", &number)]));
    assert_eq!((report.applied, report.unapplied), (1, 0), "{report:?}");
    assert!(report.warnings.is_empty());
    let doc = payment(&ledger, "70001");
    assert_eq!(applied(&doc), Money::from_minor(10_663));
    assert_eq!(doc.unapplied, Money::ZERO);
    assert_eq!(open_on(&ledger, &invoice_id), Money::ZERO);
}

#[test]
fn an_overpayment_applies_the_open_balance_and_parks_the_rest() {
    let ledger = seed_ledger();
    let (_server, invoice_id, number) = invoice_and_server(&ledger);
    let report = import(&ledger, &settlement_csv(&[("70002", "120.00", &number)]));
    assert_eq!((report.applied, report.unapplied), (0, 1), "{report:?}");
    let doc = payment(&ledger, "70002");
    assert_eq!(applied(&doc), Money::from_minor(10_663));
    assert_eq!(doc.unapplied, Money::from_minor(1_337));
    assert_eq!(open_on(&ledger, &invoice_id), Money::ZERO);
    let warning = &report.warnings[0];
    assert!(warning.contains("106.63 applied"), "{warning}");
    assert!(warning.contains("13.37 over its open balance"), "{warning}");
}

#[test]
fn a_payment_against_an_already_paid_invoice_is_all_unapplied() {
    let ledger = seed_ledger();
    let (mut server, invoice_id, number) = invoice_and_server(&ledger);
    // Paid by check first.
    ok(&call_tool(
        &mut server,
        4,
        "create_document",
        json!({ "actor": "dan", "document": {
            "kind": "Payment", "txn_date": "2026-09-25",
            "contact": { "kind": "Customer", "id": "cpool" },
            "applications": [{ "target_document_id": invoice_id,
                               "target_kind": "Invoice", "amount": "106.63" }]
        }}),
    ));
    let report = import(&ledger, &settlement_csv(&[("70003", "106.63", &number)]));
    assert_eq!((report.applied, report.unapplied), (0, 1), "{report:?}");
    let doc = payment(&ledger, "70003");
    assert!(doc.applications.is_empty());
    assert_eq!(doc.unapplied, Money::from_minor(10_663));
    assert!(
        report.warnings[0].contains("nothing left open"),
        "{report:?}"
    );
    assert_eq!(open_on(&ledger, &invoice_id), Money::ZERO);
}

#[test]
fn two_sales_in_one_file_for_one_invoice_share_its_open_balance() {
    let ledger = seed_ledger();
    let (_server, invoice_id, number) = invoice_and_server(&ledger);
    let csv = settlement_csv(&[("70004", "80.00", &number), ("70005", "80.00", &number)]);
    let report = import(&ledger, &csv);
    assert_eq!((report.applied, report.unapplied), (1, 1), "{report:?}");
    assert_eq!(
        applied(&payment(&ledger, "70004")),
        Money::from_minor(8_000)
    );
    let second = payment(&ledger, "70005");
    assert_eq!(applied(&second), Money::from_minor(2_663));
    assert_eq!(second.unapplied, Money::from_minor(5_337));
    assert_eq!(open_on(&ledger, &invoice_id), Money::ZERO);

    // A re-import neither posts them again nor re-counts them against the
    // (now zero) open balance.
    let again = import(&ledger, &csv);
    assert_eq!(again.skipped, 3);
    assert_eq!((again.applied, again.unapplied), (0, 0));
}

#[test]
fn a_voided_invoice_a_non_invoice_and_an_unknown_id_are_refused() {
    let ledger = seed_ledger();
    let mock = MockAuthnet::new();
    let mut server = mcp::new_server_with_payments(&ledger, COMPANY.into(), Box::new(mock.clone()));
    seed_master_data(&mut server);
    let (invoice_id, _, _) = create_invoice(&mut server, 3, "50.00");

    // Void it: a new version with is_voided set.
    let payload = ledger
        .latest_document_payload(COMPANY, &invoice_id)
        .unwrap()
        .unwrap();
    let mut doc: LedgerDocument = serde_json::from_str(&payload).unwrap();
    doc.is_voided = true;
    let now = Utc::now();
    ledger
        .save_document(
            COMPANY,
            &doc,
            CommandMeta {
                actor_id: "dan".into(),
                kind: "test_void".into(),
                hlc: now.to_rfc3339(),
            },
            now,
        )
        .unwrap();
    let text = error_text(&call_tool(
        &mut server,
        4,
        "payment_link",
        json!({ "document_id": invoice_id }),
    ));
    assert!(text.contains("voided"), "{text}");

    let estimate = ok(&call_tool(
        &mut server,
        5,
        "create_document",
        json!({ "actor": "dan", "document": {
            "kind": "Estimate", "txn_date": "2026-09-01",
            "contact": { "kind": "Customer", "id": "cpool" },
            "lines": [{ "kind": "Item", "item_id": "mat", "amount": "25.00" }]
        }}),
    ))
    .clone();
    let text = error_text(&call_tool(
        &mut server,
        6,
        "payment_link",
        json!({ "document_id": estimate["document_id"] }),
    ));
    assert!(text.contains("not an invoice"), "{text}");

    let text = error_text(&call_tool(
        &mut server,
        7,
        "payment_link",
        json!({ "document_id": "nope" }),
    ));
    assert!(text.contains("no document nope"), "{text}");
    assert!(mock.requests().is_empty());
}

#[test]
fn without_a_client_the_tool_says_it_is_not_configured() {
    let ledger = seed_ledger();
    let mut server = mcp::new_server(&ledger, COMPANY.into());
    seed_master_data(&mut server);
    let (invoice_id, _, _) = create_invoice(&mut server, 3, "50.00");
    let text = error_text(&call_tool(
        &mut server,
        4,
        "payment_link",
        json!({ "document_id": invoice_id }),
    ));
    assert!(text.contains("not configured"), "{text}");
}

#[test]
fn a_gateway_refusal_comes_back_as_is_error_and_leaves_the_invoice_open() {
    let ledger = seed_ledger();
    let mock = MockAuthnet::new();
    let mut server = mcp::new_server_with_payments(&ledger, COMPANY.into(), Box::new(mock.clone()));
    seed_master_data(&mut server);
    let (invoice_id, _, _) = create_invoice(&mut server, 3, "50.00");
    mock.fail_next(LinkError::Gateway {
        code: "E00007".into(),
        text: "User authentication failed due to invalid authentication values.".into(),
    });
    let text = error_text(&call_tool(
        &mut server,
        4,
        "payment_link",
        json!({ "document_id": invoice_id }),
    ));
    assert!(text.contains("E00007"), "{text}");
    let open = books::open_documents(
        &ledger,
        COMPANY,
        Book::Receivable,
        "2026-10-15".parse().unwrap(),
    )
    .unwrap();
    assert_eq!(open.len(), 1);
}

// ---------------------------------------------------------------------------
// HttpAuthnetClient against a loopback fake gateway
// ---------------------------------------------------------------------------

struct Captured {
    path: String,
    content_type: String,
    body: String,
}

/// Serves exactly one request on a free loopback port, answering `status`
/// with `response`, and hands back what it received.
fn one_shot_server(
    status: u16,
    response: &'static str,
) -> (String, Arc<Mutex<Option<Captured>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/xml/v1/request.api",
        listener.local_addr().unwrap().port()
    );
    let captured = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let mut content_length = 0;
        let mut content_type = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                match key.trim().to_ascii_lowercase().as_str() {
                    "content-length" => content_length = value.trim().parse().unwrap(),
                    "content-type" => content_type = value.trim().to_string(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        *slot.lock().unwrap() = Some(Captured {
            path,
            content_type,
            body: String::from_utf8(body).unwrap(),
        });
        let mut stream = reader.into_inner();
        write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
        stream.flush().unwrap();
    });
    (url, captured, handle)
}

fn credentials() -> StaticCredentials {
    StaticCredentials(Credentials {
        api_login_id: "fake-login".into(),
        transaction_key: "fake-key".into(),
    })
}

fn link_request() -> LinkRequest {
    LinkRequest {
        document_id: "inv-9".into(),
        invoice_number: "1042".into(),
        amount: Money::from_minor(123_456),
        description: "Invoice 1042 - Pool Co".into(),
        customer_id: Some("cpool".into()),
        customer_email: None,
    }
}

#[test]
fn the_http_client_posts_json_and_reads_a_bom_prefixed_token() {
    use link::HostedPaymentClient;
    let (url, captured, handle) = one_shot_server(
        200,
        "\u{feff}{\"token\":\"FCfc6VbKGFztf8g\",\"messages\":{\"resultCode\":\"Ok\",\"message\":[{\"code\":\"I00001\",\"text\":\"Successful.\"}]}}",
    );
    let mut client = HttpAuthnetClient::new(LinkConfig::default(), Box::new(credentials()))
        .unwrap()
        .with_api_url(url);
    let page = client.hosted_payment_page(&link_request()).unwrap();
    handle.join().unwrap();
    assert_eq!(page.token, "FCfc6VbKGFztf8g");

    let captured = captured.lock().unwrap();
    let captured = captured.as_ref().unwrap();
    assert_eq!(captured.path, "/xml/v1/request.api");
    assert!(captured.content_type.starts_with("application/json"));
    let body: Value = serde_json::from_str(&captured.body).unwrap();
    let request = &body["getHostedPaymentPageRequest"];
    assert_eq!(request["merchantAuthentication"]["name"], "fake-login");
    assert_eq!(
        request["merchantAuthentication"]["transactionKey"],
        "fake-key"
    );
    assert_eq!(request["transactionRequest"]["amount"], "1234.56");
    assert_eq!(
        request["transactionRequest"]["order"]["invoiceNumber"],
        "1042"
    );
    assert_eq!(request["transactionRequest"]["customer"]["id"], "cpool");
    assert!(request["transactionRequest"]["customer"]
        .get("email")
        .is_none());
}

#[test]
fn the_http_client_maps_a_gateway_error_and_never_echoes_the_key() {
    use link::HostedPaymentClient;
    let (url, _captured, handle) = one_shot_server(
        200,
        "{\"messages\":{\"resultCode\":\"Error\",\"message\":[{\"code\":\"E00007\",\"text\":\"User authentication failed due to invalid authentication values.\"}]}}",
    );
    let mut client = HttpAuthnetClient::new(LinkConfig::default(), Box::new(credentials()))
        .unwrap()
        .with_api_url(url);
    let err = client.hosted_payment_page(&link_request()).unwrap_err();
    handle.join().unwrap();
    let text = err.to_string();
    assert!(text.contains("E00007"), "{text}");
    assert!(
        !text.contains("fake-key") && !text.contains("fake-login"),
        "{text}"
    );
}

#[test]
fn the_http_client_maps_a_5xx() {
    use link::HostedPaymentClient;
    let (url, _captured, handle) = one_shot_server(503, "{}");
    let mut client = HttpAuthnetClient::new(LinkConfig::default(), Box::new(credentials()))
        .unwrap()
        .with_api_url(url);
    let err = client.hosted_payment_page(&link_request()).unwrap_err();
    handle.join().unwrap();
    assert!(matches!(err, LinkError::Http(503)), "{err:?}");
}
