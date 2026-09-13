//! The read-only query API, end to end. `DESIGN.md` §11, `ROADMAP.md` §B1.
//!
//! Like `tests/replica.rs`, this goes through the crate's public surface only
//! — if a read is only reachable from inside `store`, the UI cannot use it
//! either. Every name, number and figure below is invented (HANDOFF.md §2.6).

use chrono::{NaiveDate, TimeZone, Utc};
use ledger_core::Money;
use qbo_local::domain::{ContactType, DocumentType, EntityType, RealmId};
use qbo_local::store::query::{Page, MAX_PAGE_LIMIT};
use qbo_local::store::{MirroredEntity, Store};
use qbo_local::sync::SyncCursor;
use rust_decimal::Decimal;
use serde_json::{json, Value};

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

/// Enforces a `DESIGN.md` §11 measurement's budget only when `QBO_BENCH_ASSERT=1`
/// is set, so an ordinary `cargo test -p qbo-local -- --ignored` reports a
/// number without failing a slower laptop, while the dedicated CI `bench` job
/// (`.github/workflows/ci.yml`) — a fixed, comparable machine — enforces it.
fn bench_assert(label: &str, elapsed: std::time::Duration, budget_ms: u128) {
    if std::env::var("QBO_BENCH_ASSERT").as_deref() == Ok("1") {
        assert!(
            elapsed.as_millis() < budget_ms,
            "{label} took {elapsed:?}, over the {budget_ms}ms QBO_BENCH_ASSERT budget"
        );
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap()
}

fn mirrored(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
    MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "1".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    }
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", now())
        .unwrap();
    store
}

fn seed(store: &Store, entity: MirroredEntity) {
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();
}

// ---------------------------------------------------------------------------
// document_detail
// ---------------------------------------------------------------------------

#[test]
fn document_detail_round_trips_lines_and_a_link() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Customer,
            "31",
            json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Estimate,
            "377",
            json!({
                "Id": "377", "DocNumber": "3590", "TxnDate": "2026-06-30",
                "CustomerRef": { "value": "31" }, "TotalAmt": 1234.56
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "418",
            json!({
                "Id": "418", "DocNumber": "1088", "TxnDate": "2026-07-14",
                "CustomerRef": { "value": "31" }, "TotalAmt": 985.00, "Balance": 985.00,
                "LinkedTxn": [ { "TxnId": "377", "TxnType": "Estimate" } ],
                "Line": [ {
                    "LineNum": 1, "Description": "Rescue tube, 50 inch, red",
                    "Amount": 985.00, "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": "12" }, "Qty": 20, "UnitPrice": 49.25 }
                } ]
            }),
        ),
    );

    let detail = store.document_detail(&realm(), "418").unwrap().unwrap();
    assert_eq!(detail.document.qbo_id, "418");
    assert_eq!(detail.document.doc_number.as_deref(), Some("1088"));
    assert_eq!(detail.lines.len(), 1);
    assert_eq!(detail.lines[0].item_id.as_deref(), Some("12"));

    let ids: Vec<_> = detail
        .lineage
        .documents
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["377", "418"],
        "the estimate should be reachable from its invoice"
    );
    assert!(!detail.lineage.edges.is_empty());

    assert!(store
        .document_detail(&realm(), "does-not-exist")
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// contact_detail
// ---------------------------------------------------------------------------

#[test]
fn contact_detail_splits_open_from_recent() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Customer,
            "31",
            json!({
                "Id": "31", "DisplayName": "Blue Harbor Swim Club",
                "CompanyName": "Blue Harbor Swim Club LLC", "Balance": 300.00, "Active": true
            }),
        ),
    );
    // Two open invoices...
    for (id, amount, date) in [("1", 100.00, "2026-07-01"), ("2", 200.00, "2026-08-01")] {
        seed(
            &store,
            mirrored(
                EntityType::Invoice,
                id,
                json!({
                    "Id": id, "TxnDate": date, "CustomerRef": { "value": "31" },
                    "TotalAmt": amount, "Balance": amount
                }),
            ),
        );
    }
    // ...and one paid one, which should show up in "recent" but not "open".
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "3",
            json!({
                "Id": "3", "TxnDate": "2026-08-15", "CustomerRef": { "value": "31" },
                "TotalAmt": 50.00, "Balance": 0
            }),
        ),
    );

    let detail = store
        .contact_detail(&realm(), ContactType::Customer, "31")
        .unwrap()
        .unwrap();
    assert_eq!(detail.contact.display_name, "Blue Harbor Swim Club");
    assert_eq!(detail.contact.balance, Some(Money::from_minor(30_000)));

    let open_ids: Vec<_> = detail
        .open_documents
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert_eq!(
        open_ids,
        vec!["2", "1"],
        "newest first, balance-carrying only"
    );

    let recent_ids: Vec<_> = detail
        .recent_documents
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert_eq!(
        recent_ids,
        vec!["3", "2", "1"],
        "recent includes the paid invoice too"
    );

    assert!(store
        .contact_detail(&realm(), ContactType::Vendor, "31")
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// item_detail
// ---------------------------------------------------------------------------

#[test]
fn item_detail_sums_units_sold_across_invoices_and_sales_receipts() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Item,
            "12",
            json!({
                "Id": "12", "Name": "Rescue tube, 50 inch", "Sku": "RT-50-RED",
                "Type": "Inventory", "UnitPrice": 49.25, "Active": true
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "418",
            json!({
                "Id": "418", "TxnDate": "2026-07-14", "TotalAmt": 985.00,
                "Line": [ {
                    "LineNum": 1, "Amount": 985.00, "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": "12" }, "Qty": 20, "UnitPrice": 49.25 }
                } ]
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::SalesReceipt,
            "419",
            json!({
                "Id": "419", "TxnDate": "2026-07-20", "TotalAmt": 246.25,
                "Line": [ {
                    "LineNum": 1, "Amount": 246.25, "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": "12" }, "Qty": 5, "UnitPrice": 49.25 }
                } ]
            }),
        ),
    );
    // A bill line for the same item must not count toward units *sold*.
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "801",
            json!({
                "Id": "801", "TxnDate": "2026-05-20", "VendorRef": { "value": "77" },
                "TotalAmt": 100.00,
                "Line": [ { "LineNum": 1, "Amount": 100.00, "DetailType": "ItemBasedExpenseLineDetail",
                            "ItemBasedExpenseLineDetail": { "ItemRef": { "value": "12" }, "Qty": 999 } } ]
            }),
        ),
    );

    let detail = store.item_detail(&realm(), "12").unwrap().unwrap();
    assert_eq!(detail.item.sku.as_deref(), Some("RT-50-RED"));
    assert_eq!(
        detail.units_sold,
        Decimal::from(25),
        "20 from the invoice plus 5 from the receipt"
    );

    let used_ids: Vec<_> = detail
        .where_used
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert!(used_ids.contains(&"418"));
    assert!(used_ids.contains(&"419"));
    assert!(
        used_ids.contains(&"801"),
        "where-used covers every line, not only sales"
    );

    assert!(store
        .item_detail(&realm(), "does-not-exist")
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// AR / AP aging
// ---------------------------------------------------------------------------

fn invoice_for_aging(
    id: &str,
    customer: &str,
    due_date: Option<&str>,
    txn_date: &str,
    amount: f64,
) -> MirroredEntity {
    let mut payload = json!({
        "Id": id, "TxnDate": txn_date, "CustomerRef": { "value": customer },
        "TotalAmt": amount, "Balance": amount
    });
    if let Some(due) = due_date {
        payload["DueDate"] = json!(due);
    }
    mirrored(EntityType::Invoice, id, payload)
}

#[test]
fn ar_aging_buckets_by_days_past_due_with_a_null_due_date_fallback() {
    let store = store();
    let as_of = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();

    for (id, name) in [
        ("60", "Current Co"),
        ("61", "Thirty Co"),
        ("62", "Sixty Co"),
        ("63", "Ninety Co"),
        ("64", "Old Co"),
    ] {
        seed(
            &store,
            mirrored(
                EntityType::Customer,
                id,
                json!({ "Id": id, "DisplayName": name, "Active": true }),
            ),
        );
    }

    // Current: due five days from now.
    seed(
        &store,
        invoice_for_aging("601", "60", Some("2026-09-20"), "2026-08-01", 100.00),
    );
    // 1-30: due 14 days ago.
    seed(
        &store,
        invoice_for_aging("602", "61", Some("2026-09-01"), "2026-08-01", 200.00),
    );
    // 31-60: no due date at all — falls back to txn_date, 45 days before as_of.
    seed(
        &store,
        invoice_for_aging("603", "62", None, "2026-08-01", 300.00),
    );
    // 61-90: due 76 days ago.
    seed(
        &store,
        invoice_for_aging("604", "63", Some("2026-07-01"), "2026-06-01", 400.00),
    );
    // Over 90: due 137 days ago.
    seed(
        &store,
        invoice_for_aging("605", "64", Some("2026-05-01"), "2026-04-01", 500.00),
    );
    // Paid in full — must be excluded entirely, despite a stale due date.
    seed(
        &store,
        invoice_for_aging("606", "60", Some("2026-01-01"), "2026-01-01", 0.0),
    );

    let report = store.ar_aging(&realm(), as_of).unwrap();
    assert_eq!(report.as_of, as_of);
    assert_eq!(report.rows.len(), 5, "the paid invoice contributes no row");

    let bucket_of = |contact: &str| {
        report
            .rows
            .iter()
            .find(|row| row.contact_id == contact)
            .unwrap_or_else(|| panic!("no aging row for {contact}"))
    };

    assert_eq!(bucket_of("60").buckets.current, Money::from_minor(10_000));
    assert_eq!(bucket_of("61").buckets.d1_30, Money::from_minor(20_000));
    assert_eq!(
        bucket_of("62").buckets.d31_60,
        Money::from_minor(30_000),
        "null due_date falls back to txn_date"
    );
    assert_eq!(bucket_of("63").buckets.d61_90, Money::from_minor(40_000));
    assert_eq!(bucket_of("64").buckets.over_90, Money::from_minor(50_000));

    // Totals equal the sum of the rows, computed independently of them.
    let summed = Money::checked_sum(report.rows.iter().map(|row| row.total)).unwrap();
    assert_eq!(report.totals.current, Money::from_minor(10_000));
    assert_eq!(report.totals.d1_30, Money::from_minor(20_000));
    assert_eq!(report.totals.d31_60, Money::from_minor(30_000));
    assert_eq!(report.totals.d61_90, Money::from_minor(40_000));
    assert_eq!(report.totals.over_90, Money::from_minor(50_000));
    let bucket_sum = Money::checked_sum([
        report.totals.current,
        report.totals.d1_30,
        report.totals.d31_60,
        report.totals.d61_90,
        report.totals.over_90,
    ])
    .unwrap();
    assert_eq!(summed, bucket_sum);

    // Rows sorted by total descending.
    let totals: Vec<_> = report.rows.iter().map(|row| row.total.minor()).collect();
    let mut sorted = totals.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(totals, sorted);
}

#[test]
fn ap_aging_mirrors_ar_aging_over_bills() {
    let store = store();
    let as_of = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();

    seed(
        &store,
        mirrored(
            EntityType::Vendor,
            "77",
            json!({ "Id": "77", "DisplayName": "Continental Foam", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "801",
            json!({
                "Id": "801", "TxnDate": "2026-09-01", "DueDate": "2026-09-20",
                "VendorRef": { "value": "77" }, "TotalAmt": 1600.00, "Balance": 1600.00
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "802",
            json!({
                "Id": "802", "TxnDate": "2026-04-01", "DueDate": "2026-05-01",
                "VendorRef": { "value": "77" }, "TotalAmt": 500.00, "Balance": 500.00
            }),
        ),
    );

    let report = store.ap_aging(&realm(), as_of).unwrap();
    assert_eq!(report.rows.len(), 1, "same vendor on both bills");
    let row = &report.rows[0];
    assert_eq!(row.contact_id, "77");
    assert_eq!(row.buckets.current, Money::from_minor(160_000));
    assert_eq!(row.buckets.over_90, Money::from_minor(50_000));
    assert_eq!(row.total, Money::from_minor(210_000));
    let totals_sum = Money::checked_sum([
        report.totals.current,
        report.totals.d1_30,
        report.totals.d31_60,
        report.totals.d61_90,
        report.totals.over_90,
    ])
    .unwrap();
    assert_eq!(totals_sum, row.total);
}

// ---------------------------------------------------------------------------
// open_documents
// ---------------------------------------------------------------------------

#[test]
fn open_documents_for_purchase_orders_honours_doc_status() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Vendor,
            "77",
            json!({ "Id": "77", "DisplayName": "Continental Foam", "Active": true }),
        ),
    );
    // Open status, zero balance: still open.
    seed(
        &store,
        mirrored(
            EntityType::PurchaseOrder,
            "700",
            json!({
                "Id": "700", "DocNumber": "4470", "TxnDate": "2026-05-11",
                "VendorRef": { "value": "77" }, "TotalAmt": 1600.00, "Balance": 0,
                "POStatus": "Open"
            }),
        ),
    );
    // Closed status, zero balance: not open.
    seed(
        &store,
        mirrored(
            EntityType::PurchaseOrder,
            "701",
            json!({
                "Id": "701", "DocNumber": "4471", "TxnDate": "2026-05-12",
                "VendorRef": { "value": "77" }, "TotalAmt": 800.00, "Balance": 0,
                "POStatus": "Closed"
            }),
        ),
    );
    // Closed status but a balance still owed: open via the balance fallback.
    seed(
        &store,
        mirrored(
            EntityType::PurchaseOrder,
            "702",
            json!({
                "Id": "702", "DocNumber": "4472", "TxnDate": "2026-05-13",
                "VendorRef": { "value": "77" }, "TotalAmt": 900.00, "Balance": 900.00,
                "POStatus": "Closed"
            }),
        ),
    );

    let open = store
        .open_documents(&realm(), DocumentType::PurchaseOrder, Page::default())
        .unwrap();
    let ids: Vec<_> = open.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["702", "700"],
        "closed-and-unpaid and open-and-unbilled; not closed-and-paid"
    );
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[test]
fn a_paged_read_is_capped_at_the_hard_limit_regardless_of_what_is_asked_for() {
    let store = store();

    let documents: Vec<_> = (0..(MAX_PAGE_LIMIT + 5))
        .map(|i| {
            mirrored(
                EntityType::Invoice,
                &format!("cap{i}"),
                json!({
                    "Id": format!("cap{i}"), "TxnDate": "2026-01-01",
                    "ClassRef": { "value": "9" }, "TotalAmt": 1.00
                }),
            )
        })
        .collect();
    store
        .apply_batch(&realm(), &documents, None, now())
        .unwrap();

    let page = Page {
        offset: 0,
        limit: 50_000,
    };
    let rows = store.documents_by_class(&realm(), "9", page).unwrap();
    assert_eq!(
        rows.len(),
        MAX_PAGE_LIMIT,
        "the hard cap, not the caller's oversized limit"
    );
}

// ---------------------------------------------------------------------------
// sync_status
// ---------------------------------------------------------------------------

#[test]
fn sync_status_reflects_a_saved_cursor_and_a_quarantined_row() {
    let store = store();

    let cursor = SyncCursor {
        realm_id: realm(),
        entity_type: EntityType::Invoice,
        last_cdc_cursor: Some(now()),
        last_full_sweep: None,
    };
    store.save_cursor(&cursor).unwrap();

    // A quarantined invoice: missing TotalAmt.
    let broken = mirrored(
        EntityType::Invoice,
        "bad-1",
        json!({ "Id": "bad-1", "TxnDate": "2026-08-01" }),
    );
    store.upsert_entity(&realm(), &broken, now()).unwrap();
    store.project_entity(&realm(), &broken, now()).unwrap();

    let status = store.sync_status(&realm()).unwrap();
    assert!(!status.write_enabled);
    assert_eq!(status.quarantined_total, 1);

    let invoice_status = status
        .entities
        .iter()
        .find(|e| e.entity_type == EntityType::Invoice)
        .unwrap();
    assert_eq!(invoice_status.mirrored, 1);
    assert_eq!(invoice_status.last_cdc_cursor, Some(now()));
    assert_eq!(invoice_status.last_full_sweep, None);
    assert_eq!(invoice_status.quarantined, 1);

    let customer_status = status
        .entities
        .iter()
        .find(|e| e.entity_type == EntityType::Customer)
        .unwrap();
    assert_eq!(customer_status.mirrored, 0);
    assert_eq!(customer_status.quarantined, 0);

    store.set_write_enabled(&realm(), true).unwrap();
    assert!(store.sync_status(&realm()).unwrap().write_enabled);
}

// ---------------------------------------------------------------------------
// class_tree / chart_of_accounts
// ---------------------------------------------------------------------------

#[test]
fn class_tree_and_chart_of_accounts_return_projected_rows() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Class,
            "4",
            json!({ "Id": "4", "Name": "Foam", "FullyQualifiedName": "Foam", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Class,
            "5",
            json!({
                "Id": "5", "Name": "Rescue tubes", "FullyQualifiedName": "Foam:Rescue tubes",
                "ParentRef": { "value": "4" }, "Active": true
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "79",
            json!({
                "Id": "79", "Name": "Sales", "AcctNum": "4000", "AccountType": "Income",
                "CurrentBalance": 50_000.00, "Active": true
            }),
        ),
    );

    let classes = store.class_tree(&realm()).unwrap();
    let names: Vec<_> = classes.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"Foam"));
    assert!(names.contains(&"Rescue tubes"));
    let child = classes.iter().find(|c| c.qbo_id == "5").unwrap();
    assert_eq!(child.parent_id.as_deref(), Some("4"));

    let accounts = store.chart_of_accounts(&realm()).unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].acct_num.as_deref(), Some("4000"));
    assert_eq!(accounts[0].balance, Some(Money::from_minor(5_000_000)));
}

// ---------------------------------------------------------------------------
// documents_in_range
// ---------------------------------------------------------------------------

#[test]
fn documents_in_range_filters_by_date_and_optionally_by_type() {
    let store = store();
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "1",
            json!({ "Id": "1", "TxnDate": "2026-06-01", "TotalAmt": 1.0 }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "2",
            json!({ "Id": "2", "TxnDate": "2026-07-01", "TotalAmt": 1.0 }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "3",
            json!({ "Id": "3", "TxnDate": "2026-07-01", "TotalAmt": 1.0 }),
        ),
    );

    let from = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
    let to = NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();

    let all = store
        .documents_in_range(&realm(), None, from, to, Page::default())
        .unwrap();
    let ids: Vec<_> = all.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(
        ids.len(),
        2,
        "excludes the June-01 invoice, includes both July documents"
    );
    assert!(ids.contains(&"2"));
    assert!(ids.contains(&"3"));

    let invoices_only = store
        .documents_in_range(
            &realm(),
            Some(DocumentType::Invoice),
            from,
            to,
            Page::default(),
        )
        .unwrap();
    let ids: Vec<_> = invoices_only.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(ids, vec!["2"]);
}

// ---------------------------------------------------------------------------
// Scale (§11) — measured, not asserted
// ---------------------------------------------------------------------------

/// Reuses `tests/replica.rs`'s synthetic-book shape, extended with due dates
/// and balances so `ar_aging` has something real to bucket.
///
/// Run with `cargo test -p qbo-local --test query -- --ignored --nocapture`.
#[test]
#[ignore = "scale check; run explicitly with --ignored"]
fn ar_aging_and_document_detail_stay_fast_at_a_realistic_book_size() {
    use std::time::Instant;

    const CUSTOMERS: usize = 200;
    const INVOICES: usize = 10_000;
    const PAGE: usize = 500;

    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Scale Test", now()).unwrap();

    let contacts: Vec<_> = (0..CUSTOMERS)
        .map(|customer| {
            mirrored(
                EntityType::Customer,
                &format!("c{customer}"),
                json!({
                    "Id": format!("c{customer}"),
                    "DisplayName": format!("Test Customer {customer:04}"),
                    "Active": true
                }),
            )
        })
        .collect();

    let invoices: Vec<_> = (0..INVOICES)
        .map(|invoice| {
            // A third paid, the rest open across a spread of due dates so
            // every aging bucket has real rows in it.
            let balance = if invoice % 3 == 0 { 0.0 } else { 150.00 };
            let due_day = 1 + (invoice % 28);
            mirrored(
                EntityType::Invoice,
                &format!("i{invoice}"),
                json!({
                    "Id": format!("i{invoice}"),
                    "DocNumber": format!("{}", 10_000 + invoice),
                    "TxnDate": "2026-01-15",
                    "DueDate": format!("2026-{:02}-{:02}", 1 + (invoice % 9), due_day),
                    "CustomerRef": { "value": format!("c{}", invoice % CUSTOMERS) },
                    "TotalAmt": 150.00,
                    "Balance": balance,
                    "Line": [ {
                        "LineNum": 1,
                        "Description": format!("Foam blank batch {invoice}, blue"),
                        "Amount": 150.00, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } }
                    } ]
                }),
            )
        })
        .collect();

    for page in contacts.chunks(PAGE).chain(invoices.chunks(PAGE)) {
        store.apply_batch(&realm(), page, None, now()).unwrap();
    }

    let as_of = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();
    let started = Instant::now();
    let report = store.ar_aging(&realm(), as_of).unwrap();
    let ar_aging_elapsed = started.elapsed();
    assert!(!report.rows.is_empty());

    let started = Instant::now();
    let detail = store.document_detail(&realm(), "i5000").unwrap();
    let document_detail_elapsed = started.elapsed();
    assert!(detail.is_some());

    println!(
        "  {INVOICES} invoices, {CUSTOMERS} customers\n    \
         ar_aging        : {ar_aging_elapsed:?}\n    \
         document_detail : {document_detail_elapsed:?}"
    );

    // DESIGN.md §11 measured ~59ms and ~0.3ms on this same shape (debug
    // build); these budgets are 3x that, enforced only under
    // QBO_BENCH_ASSERT=1 (`.github/workflows/ci.yml`'s `bench` job) so a
    // regression is caught in CI rather than merely reported on whoever
    // happens to run the ignored suite by hand.
    bench_assert("ar_aging", ar_aging_elapsed, 200);
    bench_assert("document_detail", document_detail_elapsed, 5);
}
