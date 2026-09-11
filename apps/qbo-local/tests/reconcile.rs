//! The reconciliation sweep end to end against the in-memory double.
//! `DESIGN.md` §7, steps 1-4.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use chrono::{DateTime, Duration, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::reconcile::{ReconcileError, ReconcileOptions, Reconciler};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::json;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn at(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap() + Duration::hours(hours)
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", at(0))
        .unwrap();
    store
}

fn customer(id: &str, name: &str, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Customer,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({ "Id": id, "DisplayName": name, "Active": true }),
    }
}

fn invoice(id: &str, number: &str, total: f64, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({
            "Id": id, "DocNumber": number, "TxnDate": "2026-07-14",
            "CustomerRef": { "value": "31" }, "TotalAmt": total,
            "Line": [ { "LineNum": 1, "Description": "Rescue tube, 50 inch",
                        "Amount": total, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
        }),
    }
}

/// The same shape `driver::mirrored` builds — this is what lets a test seed
/// the store with exactly what a payload seeded into QBO would mirror to.
fn mirrored(payload: &EntityPayload) -> MirroredEntity {
    MirroredEntity {
        entity_type: payload.entity_type,
        qbo_id: payload.qbo_id.clone(),
        sync_token: payload.sync_token.clone(),
        last_updated_utc: payload.last_updated_utc,
        is_deleted: payload.is_deleted,
        raw_json: payload.raw_json.clone(),
    }
}

fn reconciler(qbo: MockQbo, options: ReconcileOptions) -> Reconciler<MockQbo> {
    Reconciler::new(qbo, options, at(0))
}

/// Generous budget: these tests are about the sweep's logic, not rate limiting.
fn options(page_size: usize) -> ReconcileOptions {
    ReconcileOptions {
        page_size,
        limits: RealmLimits {
            general: BucketBudget::per_minute(100_000.0),
            ..RealmLimits::default()
        },
    }
}

// ---------------------------------------------------------------------------
// (a) A clean replica
// ---------------------------------------------------------------------------

#[test]
fn a_clean_replica_reports_clean_and_spends_only_index_requests() {
    let mut qbo = MockQbo::new(at(0));
    let the_customer = customer("31", "Blue Harbor Swim Club", at(1));
    let the_invoice = invoice("418", "1088", 1234.56, at(1));
    qbo.seed(&realm(), the_customer.clone());
    qbo.seed(&realm(), the_invoice.clone());

    let store = store();
    store
        .apply_batch(&realm(), &[mirrored(&the_customer)], None, at(2))
        .unwrap();
    store
        .apply_batch(&realm(), &[mirrored(&the_invoice)], None, at(2))
        .unwrap();

    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_realm(
            &store,
            &realm(),
            [EntityType::Customer, EntityType::Invoice],
            at(3),
        )
        .unwrap();

    assert!(report.is_clean(), "expected a clean sweep, got {report:?}");
    assert_eq!(report.entities.iter().map(|e| e.healed).sum::<usize>(), 0);
    assert_eq!(
        report.entities.iter().map(|e| e.quarantined).sum::<usize>(),
        0
    );
    // Two entity types, one short page each: nothing beyond the index reads,
    // because nothing needed healing or quarantining.
    assert_eq!(report.entities.iter().map(|e| e.requests).sum::<usize>(), 2);
}

// ---------------------------------------------------------------------------
// (b) Missing — in QBO, not local
// ---------------------------------------------------------------------------

#[test]
fn a_record_in_qbo_but_not_local_is_healed_and_projected() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let mut reconciler = reconciler(qbo, options(100));

    let report = reconciler
        .sweep_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    assert_eq!(report.missing, vec!["418".to_string()]);
    assert!(report.stale.is_empty());
    assert!(report.extra.is_empty());
    assert_eq!(report.healed, 1);
    assert_eq!(report.quarantined, 0);

    let document = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(document.doc_number.as_deref(), Some("1088"));
}

// ---------------------------------------------------------------------------
// (c) Stale — both present, QBO changed since
// ---------------------------------------------------------------------------

#[test]
fn a_record_updated_in_qbo_is_healed() {
    let mut qbo = MockQbo::new(at(0));
    let original = invoice("418", "1088", 1234.56, at(1));
    qbo.seed(&realm(), original.clone());

    let store = store();
    store
        .apply_batch(&realm(), &[mirrored(&original)], None, at(2))
        .unwrap();

    // Someone edits the invoice in QBO after the mirror was made — a later
    // `last_updated_utc` on the same id, the canonical "stale" shape.
    qbo.seed(&realm(), invoice("418", "1088", 999.00, at(5)));

    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_entity_type(&store, &realm(), EntityType::Invoice, at(6))
        .unwrap();

    assert_eq!(report.stale, vec!["418".to_string()]);
    assert!(report.missing.is_empty());
    assert!(report.extra.is_empty());
    assert_eq!(report.healed, 1);

    let document = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(document.total, ledger_core::Money::from_minor(99_900));
}

// ---------------------------------------------------------------------------
// (d) Extra — local, not in QBO's index — quarantined, never deleted
// ---------------------------------------------------------------------------

#[test]
fn a_local_record_absent_from_qbo_is_quarantined_not_deleted() {
    let qbo = MockQbo::new(at(0)); // Nothing seeded: QBO's index is empty.
    let store = store();
    let local_only = invoice("418", "1088", 1234.56, at(1));
    store
        .apply_batch(&realm(), &[mirrored(&local_only)], None, at(2))
        .unwrap();

    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_entity_type(&store, &realm(), EntityType::Invoice, at(3))
        .unwrap();

    assert_eq!(report.extra, vec!["418".to_string()]);
    assert!(report.missing.is_empty());
    assert!(report.stale.is_empty());
    assert_eq!(report.quarantined, 1);
    assert_eq!(report.healed, 0);

    // Never auto-deleted: the entities row — and its projection — are exactly
    // as they were before the sweep ran.
    assert!(store
        .get_entity(&realm(), EntityType::Invoice, "418")
        .unwrap()
        .is_some());
    assert!(store.get_document(&realm(), "418").unwrap().is_some());

    let quarantined = store.quarantined(&realm()).unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].qbo_id, "418");
    assert!(
        quarantined[0].reason.starts_with("not in QBO index at "),
        "unexpected reason: {}",
        quarantined[0].reason
    );
}

// ---------------------------------------------------------------------------
// (e) A locally-deleted row is not "extra"
// ---------------------------------------------------------------------------

#[test]
fn a_locally_deleted_row_absent_from_qbo_is_not_counted_extra() {
    let qbo = MockQbo::new(at(0));
    let store = store();
    let mut deleted = invoice("418", "1088", 1234.56, at(1));
    deleted.is_deleted = true;
    store
        .apply_batch(&realm(), &[mirrored(&deleted)], None, at(2))
        .unwrap();

    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_entity_type(&store, &realm(), EntityType::Invoice, at(3))
        .unwrap();

    assert!(
        report.extra.is_empty(),
        "a local delete already agrees with QBO's absence"
    );
    assert_eq!(report.quarantined, 0);
    assert_eq!(report.healed, 0);
}

// ---------------------------------------------------------------------------
// (f) Orphaned lines
// ---------------------------------------------------------------------------

#[test]
fn orphaned_lines_on_an_empty_realm_is_empty() {
    // GAP (see the JSON `gaps` field): `document_lines` carries a `FOREIGN KEY`
    // to `documents` and the connection runs with `foreign_keys=ON` (DESIGN.md
    // §3.2), so an orphaned line cannot actually be produced through any public
    // `Store` API — inserting one directly requires reaching into the private
    // `rusqlite::Connection`, which is exactly what §2.1's structural scoping
    // rule exists to prevent from outside the `store` module. This exercises
    // the read on the one state reachable from a test: no lines at all.
    let store = store();
    let orphans = store.orphaned_lines(&realm()).unwrap();
    assert!(orphans.is_empty());

    let qbo = MockQbo::new(at(0));
    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_realm(&store, &realm(), Vec::<EntityType>::new(), at(1))
        .unwrap();
    assert!(report.orphaned_lines.is_empty());
}

// ---------------------------------------------------------------------------
// (g) Healing converges
// ---------------------------------------------------------------------------

#[test]
fn a_second_sweep_after_healing_finds_nothing() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), customer("31", "Blue Harbor Swim Club", at(1)));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let mut reconciler = reconciler(qbo, options(100));

    let first = reconciler
        .sweep_realm(
            &store,
            &realm(),
            [EntityType::Customer, EntityType::Invoice],
            at(2),
        )
        .unwrap();
    assert!(!first.is_clean());
    assert_eq!(first.entities.iter().map(|e| e.healed).sum::<usize>(), 2);

    let second = reconciler
        .sweep_realm(
            &store,
            &realm(),
            [EntityType::Customer, EntityType::Invoice],
            at(3),
        )
        .unwrap();
    assert!(
        second.is_clean(),
        "expected nothing left after healing, got {second:?}"
    );
    assert_eq!(second.entities.iter().map(|e| e.healed).sum::<usize>(), 0);
    assert!(second.orphaned_lines.is_empty());
}

// ---------------------------------------------------------------------------
// (h) Budget exhaustion is a refusal, not a panic
// ---------------------------------------------------------------------------

#[test]
fn budget_exhaustion_is_refused_not_panicked() {
    let mut qbo = MockQbo::new(at(0));
    for index in 0..5 {
        qbo.seed(
            &realm(),
            invoice(
                &index.to_string(),
                &format!("{}", 9000 + index),
                100.0,
                at(1),
            ),
        );
    }

    let store = store();
    let mut reconciler = Reconciler::new(
        qbo,
        ReconcileOptions {
            page_size: 1,
            limits: RealmLimits {
                general: BucketBudget::per_minute(0.0),
                ..RealmLimits::default()
            },
        },
        at(0),
    );

    let outcome = reconciler.sweep_entity_type(&store, &realm(), EntityType::Invoice, at(0));
    assert!(
        matches!(outcome, Err(ReconcileError::RateBudgetExhausted { .. })),
        "expected a refusal, got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// (i) A sweep is not a sync: sync_cursors is untouched
// ---------------------------------------------------------------------------

#[test]
fn the_sweep_never_writes_sync_cursors() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let before = store.load_cursor(&realm(), EntityType::Invoice).unwrap();

    let mut reconciler = reconciler(qbo, options(100));
    let report = reconciler
        .sweep_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();
    assert_eq!(report.healed, 1, "sanity check: the sweep did do something");

    let after = store.load_cursor(&realm(), EntityType::Invoice).unwrap();
    assert_eq!(before, after, "a sweep must not move the CDC cursor");
}
