//! The replica end to end: mirror a payload, project it, find it, walk it.
//!
//! These go through the crate's public surface rather than its internals, which
//! is the point — if the projection is only reachable from inside `store`, the
//! UI cannot use it either.
//!
//! Every name, number and figure below is invented (HANDOFF.md §2.6).

use chrono::{TimeZone, Utc};
use ledger_core::Money;
use qbo_local::domain::{ContactType, DocumentType, EntityType, RealmId};
use qbo_local::store::search::{Hit, MatchReason};
use qbo_local::store::{MirroredEntity, ProjectedTable, Store};
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

fn other_realm() -> RealmId {
    RealmId::parse("1234567890123457").unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap()
}

fn mirrored(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
    MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "3".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    }
}

/// A small, complete book: two contacts, two items, and both document chains a
/// job shop lives on — estimate to invoice to payment, and PO to bill to bill
/// payment.
fn seeded() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", now())
        .unwrap();
    store
        .register_realm(&other_realm(), "Second Company", now())
        .unwrap();

    for entity in book() {
        store.upsert_entity(&realm(), &entity, now()).unwrap();
        store.project_entity(&realm(), &entity, now()).unwrap();
    }
    store
}

fn book() -> Vec<MirroredEntity> {
    vec![
        mirrored(
            EntityType::Customer,
            "31",
            json!({
                "Id": "31",
                "DisplayName": "Blue Harbor Swim Club",
                "CompanyName": "Blue Harbor Swim Club LLC",
                "PrimaryEmailAddr": { "Address": "ap@blueharborswim.example" },
                "Balance": 0,
                "Active": true
            }),
        ),
        mirrored(
            EntityType::Vendor,
            "77",
            json!({
                "Id": "77",
                "DisplayName": "Continental Foam",
                "CompanyName": "Continental Foam Supply",
                "Balance": 1600.00,
                "Active": true
            }),
        ),
        mirrored(
            EntityType::Item,
            "12",
            json!({
                "Id": "12", "Name": "Rescue tube, 50 inch", "Sku": "RT-50-RED",
                "Type": "Inventory", "UnitPrice": 49.25, "Active": true
            }),
        ),
        mirrored(
            EntityType::Item,
            "18",
            json!({
                "Id": "18", "Name": "XLPE sheet, 2 inch blue", "Sku": "FOAM-2-BLU",
                "Type": "Inventory", "PurchaseCost": 18.4375, "Active": true
            }),
        ),
        mirrored(
            EntityType::Estimate,
            "377",
            json!({
                "Id": "377", "DocNumber": "3590", "TxnDate": "2026-06-30",
                "CustomerRef": { "value": "31" }, "TotalAmt": 1234.56,
                "TxnStatus": "Accepted",
                "Line": [ {
                    "LineNum": 1, "Description": "Rescue tube, 50 inch, red",
                    "Amount": 1234.56, "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": "12" },
                                             "Qty": 25, "UnitPrice": 49.25 }
                } ]
            }),
        ),
        mirrored(EntityType::Invoice, "418", invoice_payload()),
        mirrored(
            EntityType::Payment,
            "500",
            json!({
                "Id": "500", "TxnDate": "2026-08-01",
                "CustomerRef": { "value": "31" }, "TotalAmt": 1000.00,
                "Line": [ { "Amount": 1000.00,
                            "LinkedTxn": [ { "TxnId": "418", "TxnType": "Invoice" } ] } ]
            }),
        ),
        mirrored(
            EntityType::PurchaseOrder,
            "700",
            json!({
                "Id": "700", "DocNumber": "4470", "TxnDate": "2026-05-11",
                "VendorRef": { "value": "77" }, "TotalAmt": 1600.00,
                "POStatus": "Closed",
                "Line": [ {
                    "LineNum": 1, "Description": "XLPE sheet, 2 inch blue, 40 sheets",
                    "Amount": 1600.00, "DetailType": "ItemBasedExpenseLineDetail",
                    "ItemBasedExpenseLineDetail": { "ItemRef": { "value": "18" },
                                                    "Qty": 40, "UnitPrice": 40 }
                } ]
            }),
        ),
        mirrored(
            EntityType::Bill,
            "801",
            json!({
                "Id": "801", "DocNumber": "CF-88120", "TxnDate": "2026-05-20",
                "DueDate": "2026-06-19",
                "VendorRef": { "value": "77" }, "TotalAmt": 1600.00, "Balance": 0,
                "LinkedTxn": [ { "TxnId": "700", "TxnType": "PurchaseOrder" } ],
                "Line": [ { "LineNum": 1, "Amount": 1600.00,
                            "DetailType": "ItemBasedExpenseLineDetail",
                            "ItemBasedExpenseLineDetail": { "ItemRef": { "value": "18" } } } ]
            }),
        ),
        mirrored(
            EntityType::BillPayment,
            "902",
            json!({
                "Id": "902", "TxnDate": "2026-06-15",
                "VendorRef": { "value": "77" }, "TotalAmt": 1600.00,
                "Line": [ { "Amount": 1600.00,
                            "LinkedTxn": [ { "TxnId": "801", "TxnType": "Bill" } ] } ]
            }),
        ),
    ]
}

fn invoice_payload() -> Value {
    json!({
        "Id": "418", "DocNumber": "1088", "TxnDate": "2026-07-14",
        "DueDate": "2026-08-13",
        "CustomerRef": { "value": "31" }, "ClassRef": { "value": "4" },
        "TotalAmt": 1234.56, "Balance": 234.56,
        "PrivateNote": "Rush — dock bumpers first",
        "CustomField": [ { "Name": "P.O. Number", "StringValue": "BH-99214" } ],
        "LinkedTxn": [ { "TxnId": "377", "TxnType": "Estimate" } ],
        "Line": [
            {
                "LineNum": 1, "Description": "Rescue tube, 50 inch, red",
                "Amount": 985.00, "DetailType": "SalesItemLineDetail",
                "SalesItemLineDetail": { "ItemRef": { "value": "12" },
                                         "Qty": 20, "UnitPrice": 49.25,
                                         "TaxCodeRef": { "value": "TAX" } }
            },
            {
                "LineNum": 2, "Description": "Freight", "Amount": 249.56,
                "DetailType": "SalesItemLineDetail",
                "SalesItemLineDetail": { "ItemRef": { "value": "3" },
                                         "TaxCodeRef": { "value": "NON" } }
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

#[test]
fn projecting_a_document_writes_its_header_lines_and_links() {
    let store = seeded();

    let invoice = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(invoice.doc_type, DocumentType::Invoice);
    assert_eq!(invoice.doc_number.as_deref(), Some("1088"));
    assert_eq!(invoice.total, Money::from_minor(123_456));
    assert_eq!(invoice.po_number.as_deref(), Some("BH-99214"));
    // Joined, not fetched per row.
    assert_eq!(
        invoice.contact_name.as_deref(),
        Some("Blue Harbor Swim Club")
    );

    let lines = store.document_lines(&realm(), "418").unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].unit_price.as_deref(), Some("49.25"));
    assert!(lines[0].is_taxable);

    let links = store.links_from(&realm(), "418").unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].to_qbo_id, "377");
}

#[test]
fn a_line_deleted_in_qbo_disappears_from_the_projection() {
    // An upsert alone would leave the removed line behind, silently inflating
    // every total computed off document_lines.
    let store = seeded();
    assert_eq!(store.document_lines(&realm(), "418").unwrap().len(), 2);

    let mut payload = invoice_payload();
    payload["Line"].as_array_mut().unwrap().pop();
    payload["TotalAmt"] = json!(985.00);
    let edited = mirrored(EntityType::Invoice, "418", payload);

    store.upsert_entity(&realm(), &edited, now()).unwrap();
    store.project_entity(&realm(), &edited, now()).unwrap();

    let lines = store.document_lines(&realm(), "418").unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].description.as_deref(),
        Some("Rescue tube, 50 inch, red")
    );
}

#[test]
fn reprojection_rebuilds_the_whole_projection_from_local_disk() {
    let store = seeded();
    let before = store.get_document(&realm(), "418").unwrap().unwrap();
    let counts = |table| store.count_projected(&realm(), table).unwrap();
    let (documents, lines, links) = (
        counts(ProjectedTable::Documents),
        counts(ProjectedTable::DocumentLines),
        counts(ProjectedTable::DocumentLinks),
    );

    let report = store.reproject_all(&realm(), now()).unwrap();

    assert_eq!(report.parsed, 10);
    assert_eq!(report.quarantined, 0);
    assert_eq!(counts(ProjectedTable::Documents), documents);
    assert_eq!(counts(ProjectedTable::DocumentLines), lines);
    assert_eq!(counts(ProjectedTable::DocumentLinks), links);
    assert_eq!(
        store.get_document(&realm(), "418").unwrap().unwrap(),
        before
    );
}

#[test]
fn a_payload_that_will_not_parse_is_quarantined_and_recoverable() {
    let store = seeded();

    let mut broken = invoice_payload();
    broken.as_object_mut().unwrap().remove("TotalAmt");
    broken["DocNumber"] = json!("1099");
    let entity = mirrored(EntityType::Invoice, "419", broken);
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();

    let held = store.quarantined(&realm()).unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].qbo_id, "419");
    assert!(store.get_document(&realm(), "419").unwrap().is_none());

    // The raw JSON never left `entities`, so a corrected payload clears it.
    let mut fixed = invoice_payload();
    fixed["DocNumber"] = json!("1099");
    let entity = mirrored(EntityType::Invoice, "419", fixed);
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();

    assert!(store.quarantined(&realm()).unwrap().is_empty());
    assert!(store.get_document(&realm(), "419").unwrap().is_some());
}

#[test]
fn one_realm_cannot_see_another_realms_documents() {
    let store = seeded();
    let entity = mirrored(EntityType::Invoice, "418", invoice_payload());
    store.upsert_entity(&other_realm(), &entity, now()).unwrap();
    store
        .project_entity(&other_realm(), &entity, now())
        .unwrap();

    // Same QBO id, both realms, no bleed in either direction.
    assert_eq!(
        store
            .count_projected(&realm(), ProjectedTable::Documents)
            .unwrap(),
        6
    );
    assert_eq!(
        store
            .count_projected(&other_realm(), ProjectedTable::Documents)
            .unwrap(),
        1
    );
    assert!(store.search(&other_realm(), "3590", 10).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Search (§3.3)
// ---------------------------------------------------------------------------

fn only_document(store: &Store, query: &str) -> (MatchReason, String) {
    let hits = store.search(&realm(), query, 10).unwrap();
    let first = hits
        .first()
        .unwrap_or_else(|| panic!("no hit for {query:?}"));
    match &first.hit {
        Hit::Document(document) => (first.reason, document.qbo_id.clone()),
        other => panic!("expected a document for {query:?}, got {other:?}"),
    }
}

#[test]
fn typing_a_document_number_lands_on_the_document() {
    let store = seeded();
    assert_eq!(
        only_document(&store, "1088"),
        (MatchReason::DocumentNumber, "418".to_string())
    );
    assert_eq!(
        only_document(&store, "3590"),
        (MatchReason::DocumentNumber, "377".to_string())
    );
}

#[test]
fn typing_a_customers_po_number_finds_the_order_it_belongs_to() {
    let store = seeded();
    assert_eq!(
        only_document(&store, "BH-99214"),
        (MatchReason::PurchaseOrderNumber, "418".to_string())
    );
}

#[test]
fn a_partial_document_number_still_finds_it_but_ranks_below_an_exact_one() {
    let store = seeded();
    let (reason, id) = only_document(&store, "108");
    assert_eq!(reason, MatchReason::DocumentNumberPrefix);
    assert_eq!(id, "418");
}

#[test]
fn an_exact_number_outranks_everything_it_also_matches() {
    // "1088" is the invoice's number and a prefix of nothing else here; the
    // point is that the exact reason wins and is reported as such.
    let store = seeded();
    let hits = store.search(&realm(), "1088", 10).unwrap();
    assert_eq!(hits[0].reason, MatchReason::DocumentNumber);
    assert!(hits.windows(2).all(|pair| pair[0].reason <= pair[1].reason));
}

#[test]
fn an_amount_finds_documents_only_when_the_query_says_money() {
    let store = seeded();

    let hits = store.search(&realm(), "1,234.56", 10).unwrap();
    let ids: Vec<_> = hits
        .iter()
        .filter(|hit| hit.reason == MatchReason::Amount)
        .filter_map(|hit| match &hit.hit {
            Hit::Document(document) => Some(document.qbo_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        ids.contains(&"418".to_string()),
        "invoice total, got {ids:?}"
    );
    assert!(
        ids.contains(&"377".to_string()),
        "estimate total, got {ids:?}"
    );

    // A bare number is a reference, not a dollar figure.
    let hits = store.search(&realm(), "160000", 10).unwrap();
    assert!(hits.iter().all(|hit| hit.reason != MatchReason::Amount));
}

#[test]
fn typing_a_sku_finds_the_item() {
    let store = seeded();
    let hits = store.search(&realm(), "FOAM-2-BLU", 10).unwrap();
    assert_eq!(hits[0].reason, MatchReason::Sku);
    match &hits[0].hit {
        Hit::Item(item) => assert_eq!(item.qbo_id, "18"),
        other => panic!("expected an item, got {other:?}"),
    }
}

#[test]
fn a_few_letters_of_a_customer_name_is_enough() {
    let store = seeded();
    let hits = store.search(&realm(), "blue har", 10).unwrap();
    let named = hits.iter().any(|hit| match &hit.hit {
        Hit::Contact(contact) => contact.display_name == "Blue Harbor Swim Club",
        _ => false,
    });
    assert!(
        named,
        "prefix search should reach the customer, got {hits:?}"
    );
}

#[test]
fn text_on_a_line_finds_the_document_that_line_belongs_to() {
    let store = seeded();
    let hits = store.search(&realm(), "dock bumpers", 10).unwrap();
    assert!(
        hits.iter()
            .any(|hit| matches!(&hit.hit, Hit::Document(d) if d.qbo_id == "418")),
        "memo text should reach the invoice"
    );

    let hits = store.search(&realm(), "XLPE sheet", 10).unwrap();
    assert!(
        hits.iter()
            .any(|hit| matches!(&hit.hit, Hit::Document(d) if d.qbo_id == "700")),
        "a line description should reach its purchase order"
    );
}

#[test]
fn the_search_index_follows_an_edit_rather_than_drifting_from_it() {
    let store = seeded();
    assert!(!store
        .search(&realm(), "Blue Harbor", 10)
        .unwrap()
        .is_empty());

    let renamed = mirrored(
        EntityType::Customer,
        "31",
        json!({ "Id": "31", "DisplayName": "Northgate Aquatic Center", "Active": true }),
    );
    store.upsert_entity(&realm(), &renamed, now()).unwrap();
    store.project_entity(&realm(), &renamed, now()).unwrap();

    let stale = store.search(&realm(), "Blue Harbor", 10).unwrap();
    assert!(
        !stale.iter().any(|hit| matches!(&hit.hit, Hit::Contact(_))),
        "the old name should be gone from the index, got {stale:?}"
    );
    assert!(
        store
            .search(&realm(), "Northgate", 10)
            .unwrap()
            .iter()
            .any(|hit| matches!(&hit.hit, Hit::Contact(_))),
        "the new name should be in it"
    );
}

#[test]
fn punctuation_and_fts_operators_in_a_query_are_harmless() {
    let store = seeded();
    for query in ["\"", "* OR *", "NEAR(", "---", "a AND", "()"] {
        // The bar is that none of these panic or return an SQL error.
        store
            .search(&realm(), query, 10)
            .unwrap_or_else(|error| panic!("{query:?} should be inert, got {error}"));
    }
}

#[test]
fn search_respects_its_limit() {
    let store = seeded();
    assert!(store.search(&realm(), "1", 3).unwrap().len() <= 3);
    assert!(store.search(&realm(), "", 10).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Lineage
// ---------------------------------------------------------------------------

#[test]
fn an_estimate_knows_what_it_became() {
    let store = seeded();
    let onward = store.links_to(&realm(), "377").unwrap();
    assert_eq!(onward.len(), 1);
    assert_eq!(onward[0].from_qbo_id, "418");
    assert_eq!(onward[0].from_type, "Invoice");
}

#[test]
fn a_lineage_walk_reaches_the_whole_chain() {
    let store = seeded();

    let sales = store.lineage(&realm(), "377", 5).unwrap();
    let ids: Vec<_> = sales.documents.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["377", "418", "500"],
        "estimate to invoice to payment"
    );
    assert!(sales.unresolved.is_empty());

    let purchases = store.lineage(&realm(), "902", 5).unwrap();
    let ids: Vec<_> = purchases
        .documents
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert_eq!(ids, vec!["700", "801", "902"], "PO to bill to bill payment");
}

#[test]
fn a_lineage_walk_stops_at_the_depth_it_was_given() {
    let store = seeded();
    let one_hop = store.lineage(&realm(), "377", 1).unwrap();
    let ids: Vec<_> = one_hop
        .documents
        .iter()
        .map(|d| d.qbo_id.as_str())
        .collect();
    assert_eq!(ids, vec!["377", "418"], "the payment is two hops out");
}

#[test]
fn a_link_to_a_document_outside_the_mirror_is_reported_not_hidden() {
    let store = seeded();

    let mut payload = invoice_payload();
    payload["LinkedTxn"] = json!([ { "TxnId": "99999", "TxnType": "Estimate" } ]);
    let entity = mirrored(EntityType::Invoice, "418", payload);
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();

    let lineage = store.lineage(&realm(), "418", 5).unwrap();
    assert_eq!(lineage.unresolved, vec!["99999".to_string()]);
}

#[test]
fn a_contact_and_an_item_can_both_answer_where_used() {
    let store = seeded();

    let customer_history = store
        .documents_for_contact(&realm(), ContactType::Customer, "31", 50)
        .unwrap();
    let ids: Vec<_> = customer_history.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(ids, vec!["500", "418", "377"], "newest first");

    let where_used = store.documents_for_item(&realm(), "18", 50).unwrap();
    let ids: Vec<_> = where_used.iter().map(|d| d.qbo_id.as_str()).collect();
    assert_eq!(ids, vec!["801", "700"]);
}

#[test]
fn a_register_lists_one_document_type_newest_first() {
    let store = seeded();
    let invoices = store
        .list_documents(&realm(), DocumentType::Invoice, 50)
        .unwrap();
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0].doc_number.as_deref(), Some("1088"));
}

// ---------------------------------------------------------------------------
// Scale (§11)
// ---------------------------------------------------------------------------

/// §11 asks for measured numbers rather than asserted ones, so this prints what
/// it measured and only fails on a ceiling loose enough that hitting it means a
/// missing index rather than a slow machine.
///
/// Run it with `cargo test -p qbo-local --test replica -- --ignored --nocapture`.
#[test]
#[ignore = "scale check; run explicitly with --ignored"]
fn search_stays_fast_at_a_realistic_book_size() {
    use std::time::Instant;

    const CUSTOMERS: usize = 200;
    const INVOICES: usize = 10_000;

    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Scale Test", now()).unwrap();

    // Built through `apply_batch`, which is the path a real sync takes — one
    // commit per page rather than one per entity.
    const PAGE: usize = 500;

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
            mirrored(
                EntityType::Invoice,
                &format!("i{invoice}"),
                json!({
                    "Id": format!("i{invoice}"),
                    "DocNumber": format!("{}", 10_000 + invoice),
                    "TxnDate": "2026-01-15",
                    "CustomerRef": { "value": format!("c{}", invoice % CUSTOMERS) },
                    "TotalAmt": 100.00,
                    "Line": [ {
                        "LineNum": 1,
                        "Description": format!("Foam blank batch {invoice}, blue"),
                        "Amount": 100.00, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } }
                    } ]
                }),
            )
        })
        .collect();

    let built = Instant::now();
    for page in contacts.chunks(PAGE).chain(invoices.chunks(PAGE)) {
        store.apply_batch(&realm(), page, None, now()).unwrap();
    }
    let build = built.elapsed();

    let started = Instant::now();
    let hits = store.search(&realm(), "19999", 20).unwrap();
    let by_number = started.elapsed();
    assert_eq!(hits[0].reason, MatchReason::DocumentNumber);

    let started = Instant::now();
    let hits = store.search(&realm(), "Test Customer 0142", 20).unwrap();
    let by_text = started.elapsed();
    assert!(!hits.is_empty());

    let started = Instant::now();
    store.search(&realm(), "foam blank", 20).unwrap();
    let by_line_text = started.elapsed();

    println!(
        "  {INVOICES} invoices, {CUSTOMERS} customers, projected in {build:?} \
         ({PAGE} per commit)\n    \
         document number : {by_number:?}\n    \
         customer name   : {by_text:?}\n    \
         line text       : {by_line_text:?}"
    );

    // Loose enough that a slow machine passes; tight enough that a dropped
    // index on documents(realm_id, doc_number) does not.
    assert!(
        by_number.as_millis() < 50,
        "document-number search took {by_number:?}, which reads like a table scan"
    );
    assert!(by_text.as_millis() < 100, "name search took {by_text:?}");

    // DESIGN.md §11 measured ~7ms and ~41ms on this same shape (debug build);
    // these budgets are 3x that, enforced only under QBO_BENCH_ASSERT=1
    // (`.github/workflows/ci.yml`'s `bench` job).
    bench_assert("search by document number", by_number, 25);
    bench_assert("search by line description", by_line_text, 125);
}
