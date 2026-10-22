//! D42: QBO sales orders reach the replica from a `qbo-sales-orders/1` feed,
//! over HTTP (the fake server here) or from a file (the CLI test), and are
//! projected with their open lines. Synthetic data only.

mod support;

use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use ledger_core::Money;
use rust_decimal::Decimal;
use serde_json::{json, Value};

use qbo_local::domain::RealmId;
use qbo_local::sales_orders::{fetch_feed, FEED_FORMAT};
use qbo_local::store::Store;

use support::FakeResponse;

const REALM: &str = "9341452983741";

fn realm() -> RealmId {
    RealmId::parse(REALM).unwrap()
}

fn feed(orders: Value, complete: bool) -> Value {
    json!({
        "feed": FEED_FORMAT,
        "realmId": REALM,
        "capturedAt": "2026-10-17T12:00:00Z",
        "complete": complete,
        "salesOrders": orders,
    })
}

fn open_order() -> Value {
    json!({
        "id": "1490000011", "orderNumber": "1011", "issuedAt": "2026-10-13T07:00:00Z",
        "customerId": "58", "customerPONumber": "PO-77",
        "orderStatus": "OPEN", "paymentStatus": "NOT_INVOICED",
        "fulfillmentStatus": "PENDING_FULFILLMENT", "totalAmount": "250.00", "version": 3,
        "lines": [
            { "id": "1", "itemId": "10", "description": "Swim mat", "quantity": 2,
              "rate": "100.00", "amount": "200.00", "taxable": false,
              "quantityConsumedInInvoices": null },
            { "id": "2", "itemId": "40", "description": "Shipping", "quantity": 1,
              "rate": "50.00", "amount": "50.00", "taxable": false }
        ]
    })
}

fn invoiced_order() -> Value {
    json!({
        "id": "1490000012", "orderNumber": "1012", "issuedAt": "2026-09-30",
        "customerId": "59", "orderStatus": "OPEN", "paymentStatus": "INVOICED",
        "totalAmount": "75.00", "version": 9, "invoiceIds": ["900"],
        "lines": [
            { "id": "1", "itemId": "11", "quantity": 3, "rate": "25", "amount": "75",
              "quantityConsumedInInvoices": 3, "invoiceIds": ["900"] }
        ]
    })
}

#[test]
fn a_feed_served_over_http_is_mirrored_with_its_open_lines() {
    let served = Arc::new(Mutex::new(feed(
        json!([open_order(), invoiced_order()]),
        true,
    )));
    let state = Arc::clone(&served);
    let server = support::start(move |request| {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/sales-orders.json");
        FakeResponse::json(200, state.lock().unwrap().clone())
    });
    let url = format!("{}/sales-orders.json", server.base_url);

    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Synthetic", Utc::now())
        .unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 17, 13, 0, 0).unwrap();

    let parsed = fetch_feed(&url, &realm()).unwrap();
    let summary = store
        .apply_sales_order_feed(&realm(), &parsed, &url, now)
        .unwrap();
    assert_eq!((summary.seen, summary.inserted, summary.open), (2, 2, 1));

    let open = store.list_sales_orders(&realm(), true).unwrap();
    assert_eq!(open.len(), 1);
    let so = &open[0];
    assert_eq!(so.doc_number.as_deref(), Some("1011"));
    assert_eq!(so.po_number.as_deref(), Some("PO-77"));
    assert_eq!(so.open_amount, Money::from_minor(25_000));
    assert_eq!(so.lines.len(), 2);
    assert_eq!(so.lines[0].qty_open, Some(Decimal::from(2)));
    assert_eq!(so.lines[0].item_id.as_deref(), Some("10"));

    let all = store.list_sales_orders(&realm(), false).unwrap();
    assert_eq!(all.len(), 2);
    let invoiced = store
        .get_sales_order(&realm(), "1490000012")
        .unwrap()
        .unwrap();
    assert!(!invoiced.is_open);
    assert_eq!(invoiced.invoice_ids, vec!["900".to_string()]);
    assert_eq!(invoiced.lines[0].invoice_ids, vec!["900".to_string()]);

    let snapshot = store.sales_order_snapshot(&realm()).unwrap().unwrap();
    assert_eq!(snapshot.order_count, 2);
    assert!(snapshot.complete);
    assert_eq!(snapshot.source, url);

    // The same feed again changes nothing.
    let again = fetch_feed(&url, &realm()).unwrap();
    let summary = store
        .apply_sales_order_feed(&realm(), &again, &url, now)
        .unwrap();
    assert_eq!(
        (summary.unchanged, summary.updated, summary.inserted),
        (2, 0, 0)
    );

    // Invoiced since, and the other one gone from a complete feed.
    let mut now_invoiced = open_order();
    now_invoiced["paymentStatus"] = json!("INVOICED");
    now_invoiced["lines"][0]["quantityConsumedInInvoices"] = json!(2);
    now_invoiced["lines"][1]["quantityConsumedInInvoices"] = json!(1);
    *served.lock().unwrap() = feed(json!([now_invoiced]), true);
    let third = fetch_feed(&url, &realm()).unwrap();
    let summary = store
        .apply_sales_order_feed(&realm(), &third, &url, now)
        .unwrap();
    assert_eq!((summary.updated, summary.deleted, summary.open), (1, 1, 0));
    assert!(store.list_sales_orders(&realm(), true).unwrap().is_empty());
    assert_eq!(store.list_sales_orders(&realm(), false).unwrap().len(), 1);
}

#[test]
fn a_partial_feed_never_deletes_and_a_bad_order_writes_nothing() {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Synthetic", Utc::now())
        .unwrap();
    let now = Utc::now();
    let first =
        serde_json::from_value(feed(json!([open_order(), invoiced_order()]), true)).unwrap();
    store
        .apply_sales_order_feed(&realm(), &first, "test", now)
        .unwrap();

    let partial = serde_json::from_value(feed(json!([invoiced_order()]), false)).unwrap();
    let summary = store
        .apply_sales_order_feed(&realm(), &partial, "test", now)
        .unwrap();
    assert_eq!(summary.deleted, 0);
    assert_eq!(store.list_sales_orders(&realm(), true).unwrap().len(), 1);

    let mut bad = open_order();
    bad["issuedAt"] = json!("not a date");
    let broken = serde_json::from_value(feed(json!([invoiced_order(), bad]), true)).unwrap();
    assert!(store
        .apply_sales_order_feed(&realm(), &broken, "test", now)
        .is_err());
    assert_eq!(store.list_sales_orders(&realm(), false).unwrap().len(), 2);
}

#[test]
fn a_server_error_is_reported_not_taken_as_no_orders() {
    let server = support::start(|_| FakeResponse::json(503, json!({ "error": "down" })));
    let err = fetch_feed(&format!("{}/feed", server.base_url), &realm()).unwrap_err();
    assert!(err.to_string().contains("HTTP 503"), "{err}");
}

#[test]
fn the_cli_syncs_a_feed_file_and_lists_open_orders() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("replica.sqlite");
    let feed_path = dir.path().join("feed.json");
    std::fs::write(
        &feed_path,
        feed(json!([open_order(), invoiced_order()]), true).to_string(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_qbo-local"))
            .args(args)
            .output()
            .unwrap()
    };
    let db_text = db.to_str().unwrap();
    let init = run(&[
        "init",
        "--db",
        db_text,
        "--realm",
        REALM,
        "--name",
        "Synthetic",
    ]);
    assert!(init.status.success(), "{init:?}");

    let sync = run(&[
        "sync-sales-orders",
        "--db",
        db_text,
        "--realm",
        REALM,
        "--feed",
        feed_path.to_str().unwrap(),
    ]);
    assert!(sync.status.success(), "{sync:?}");
    let out = String::from_utf8_lossy(&sync.stdout);
    assert!(out.contains("inserted  : 2"), "{out}");
    assert!(out.contains("open now  : 1"), "{out}");

    let list = run(&["sales-orders", "--db", db_text, "--realm", REALM]);
    assert!(list.status.success(), "{list:?}");
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(out.contains("1011"), "{out}");
    assert!(!out.contains("1012"), "{out}");
    assert!(out.contains("1 open sales orders"), "{out}");

    let missing = run(&["sync-sales-orders", "--db", db_text, "--realm", REALM]);
    assert_eq!(missing.status.code(), Some(2));
}
