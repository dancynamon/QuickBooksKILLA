//! The sync loop end to end against the in-memory double. `DESIGN.md` §4.
//!
//! These are the tests that decide whether the replica can be trusted, because
//! every failure mode here is silent by nature: a cursor advanced too far, a
//! truncated response taken as complete, a crash between writing entities and
//! writing the cursor. None of them raise an error at the time. All of them
//! leave the replica quietly missing records.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use chrono::{DateTime, Duration, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo};
use qbo_local::domain::{DocumentType, EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncError, SyncOptions, SyncPath};
use qbo_local::ratelimit::RealmLimits;
use qbo_local::store::{ProjectedTable, Store};
use qbo_local::sync::SweepReason;
use serde_json::json;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn at(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap() + Duration::hours(hours)
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Test Company", at(0)).unwrap();
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

fn driver(qbo: MockQbo, options: SyncOptions) -> SyncDriver<MockQbo> {
    SyncDriver::new(qbo, options, at(0))
}

/// Generous budgets: these tests are about sync, not about rate limiting.
fn options(page_size: usize) -> SyncOptions {
    SyncOptions {
        page_size,
        limits: RealmLimits {
            general: qbo_local::ratelimit::BucketBudget::per_minute(100_000.0),
            ..RealmLimits::default()
        },
        ..SyncOptions::default()
    }
}

// ---------------------------------------------------------------------------
// The initial pull
// ---------------------------------------------------------------------------

#[test]
fn a_first_sync_sweeps_and_lands_a_usable_replica() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), customer("31", "Blue Harbor Swim Club", at(1)));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(2)));

    let store = store();
    let mut driver = driver(qbo, options(100));

    let report = driver
        .sync_realm(
            &store,
            &realm(),
            &[EntityType::Invoice, EntityType::Customer],
            at(3),
        )
        .unwrap();

    assert_eq!(report.mirrored(), 2);
    assert_eq!(report.quarantined(), 0);
    assert!(matches!(
        report.entities[0].path,
        SyncPath::FullSweep { reason: SweepReason::NoCursor, .. }
    ));

    // Mirrored, projected, and findable — the whole point of the exercise.
    let invoice = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(invoice.doc_number.as_deref(), Some("1088"));
    assert_eq!(invoice.contact_name.as_deref(), Some("Blue Harbor Swim Club"));
    assert!(!store.search(&realm(), "1088", 5).unwrap().is_empty());
}

#[test]
fn masters_are_mirrored_before_the_documents_that_display_them() {
    // Asked for in the awkward order on purpose.
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), customer("31", "Blue Harbor Swim Club", at(1)));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(2)));

    let store = store();
    let mut driver = driver(qbo, options(100));
    let report = driver
        .sync_realm(
            &store,
            &realm(),
            &[EntityType::Invoice, EntityType::Customer],
            at(3),
        )
        .unwrap();

    let order: Vec<_> = report.entities.iter().map(|e| e.entity_type).collect();
    assert_eq!(order, vec![EntityType::Customer, EntityType::Invoice]);
}

#[test]
fn a_sweep_pages_until_it_runs_out() {
    let mut qbo = MockQbo::new(at(0));
    for index in 0..25 {
        qbo.seed(
            &realm(),
            invoice(&format!("{index}"), &format!("{}", 2000 + index), 100.0, at(1)),
        );
    }

    let store = store();
    let mut driver = driver(qbo, options(10));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    assert_eq!(report.mirrored, 25);
    match report.path {
        // 10 + 10 + 5: the short page is what ends it.
        SyncPath::FullSweep { pages, .. } => assert_eq!(pages, 3),
        other => panic!("expected a sweep, got {other:?}"),
    }
    assert_eq!(
        store.count_projected(&realm(), ProjectedTable::Documents).unwrap(),
        25
    );
}

#[test]
fn a_sweep_whose_record_count_is_an_exact_multiple_of_the_page_still_terminates() {
    // The off-by-one that would loop forever: a full final page looks like
    // there is more to come.
    let mut qbo = MockQbo::new(at(0));
    for index in 0..20 {
        qbo.seed(
            &realm(),
            invoice(&format!("{index}"), &format!("{}", 3000 + index), 100.0, at(1)),
        );
    }

    let store = store();
    let mut driver = driver(qbo, options(10));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    assert_eq!(report.mirrored, 20);
    match report.path {
        SyncPath::FullSweep { pages, .. } => assert_eq!(pages, 3, "two full pages then an empty one"),
        other => panic!("expected a sweep, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Incremental polling
// ---------------------------------------------------------------------------

#[test]
fn a_second_sync_polls_instead_of_sweeping() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let mut driver = driver(qbo, options(100));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    driver
        .client_mut()
        .seed(&realm(), invoice("419", "1089", 500.00, at(3)));

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(4))
        .unwrap();

    assert!(matches!(report.path, SyncPath::Cdc { .. }));
    assert_eq!(report.mirrored, 1, "only the new invoice, not the whole book");
    assert_eq!(
        store.list_documents(&realm(), DocumentType::Invoice, 50).unwrap().len(),
        2
    );
}

#[test]
fn an_edit_in_qbo_replaces_the_local_copy_rather_than_duplicating_it() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let mut driver = driver(qbo, options(100));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    driver
        .client_mut()
        .seed(&realm(), invoice("418", "1088", 999.00, at(3)));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(4))
        .unwrap();

    let invoice = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(invoice.total, ledger_core::Money::from_minor(99_900));
    assert_eq!(
        store.count_projected(&realm(), ProjectedTable::Documents).unwrap(),
        1
    );
}

#[test]
fn a_cursor_too_old_to_trust_sweeps_rather_than_polling() {
    // The laptop-closed-for-a-month case. CDC past its window does not fail; it
    // under-reports, which is worse.
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", 1234.56, at(1)));

    let store = store();
    let mut driver = driver(qbo, options(100));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    let forty_days_later = at(2) + Duration::days(40);
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, forty_days_later)
        .unwrap();

    match report.path {
        SyncPath::FullSweep {
            reason: SweepReason::CursorTooOld { age_days },
            ..
        } => assert_eq!(age_days, 40),
        other => panic!("expected a sweep on an old cursor, got {other:?}"),
    }
}

#[test]
fn an_entity_type_with_unconfirmed_cdc_coverage_always_sweeps() {
    // §0 leaves the CDC exclusion list unverified, so the peripheral tier takes
    // the safe path every time rather than the cheap one.
    let store = store();
    let mut driver = driver(MockQbo::new(at(0)), options(100));

    for now in [at(2), at(3)] {
        let report = driver
            .sync_entity_type(&store, &realm(), EntityType::Attachable, now)
            .unwrap();
        assert!(
            matches!(report.path, SyncPath::FullSweep { .. }),
            "peripheral types never poll, got {:?}",
            report.path
        );
    }
}

// ---------------------------------------------------------------------------
// Truncation — the failure that is silent by default
// ---------------------------------------------------------------------------

#[test]
fn a_truncated_poll_is_backfilled_through_bounded_windows() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("1", "4001", 100.0, at(1)));

    let store = store();
    let mut driver = driver(
        qbo,
        SyncOptions {
            cdc_response_cap: 2,
            ..options(100)
        },
    );
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    // Four changes since the cursor, against a CDC cap of two.
    for index in 2..=5 {
        driver.client_mut().seed(
            &realm(),
            invoice(&index.to_string(), &format!("400{index}"), 100.0, at(2 + index)),
        );
    }
    driver.client_mut().set_cdc_cap(2);

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(20))
        .unwrap();

    match report.path {
        SyncPath::CdcThenBackfill { pages, .. } => assert!(pages >= 1),
        other => panic!("expected a backfill, got {other:?}"),
    }

    // The point of the exercise: nothing was lost to truncation.
    assert_eq!(
        store.list_documents(&realm(), DocumentType::Invoice, 50).unwrap().len(),
        5,
        "every changed invoice should have landed"
    );
}

#[test]
fn a_truncated_poll_does_not_advance_the_cursor_past_what_it_proved() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("1", "4001", 100.0, at(1)));

    let store = store();
    let mut driver = driver(
        qbo,
        SyncOptions {
            cdc_response_cap: 2,
            ..options(100)
        },
    );
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();
    driver.client_mut().set_cdc_cap(2);

    for index in 2..=5 {
        driver.client_mut().seed(
            &realm(),
            invoice(&index.to_string(), &format!("400{index}"), 100.0, at(2 + index)),
        );
    }

    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(20))
        .unwrap();

    // The cursor advances to the moment the sync started, not to the newest
    // record CDC happened to return — which is the whole difference between a
    // complete backfill and a silent gap.
    let cursor = store.load_cursor(&realm(), EntityType::Invoice).unwrap();
    assert_eq!(cursor.last_cdc_cursor, Some(at(20)));

    // Everything in the truncated window landed, and a poll from the new cursor
    // finds nothing left over.
    assert_eq!(
        store.list_documents(&realm(), DocumentType::Invoice, 50).unwrap().len(),
        5
    );
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(21))
        .unwrap();
    assert_eq!(report.mirrored, 0, "nothing was skipped, so nothing is left");
}

// ---------------------------------------------------------------------------
// Durability and refusal
// ---------------------------------------------------------------------------

#[test]
fn the_cursor_moves_only_when_the_entities_it_covers_are_written() {
    let mut qbo = MockQbo::new(at(0));
    for index in 0..15 {
        qbo.seed(
            &realm(),
            invoice(&format!("{index}"), &format!("{}", 5000 + index), 100.0, at(1)),
        );
    }

    let store = store();
    let mut driver = driver(qbo, options(10));

    // Fail on the second page: the first page is committed, the cursor is not.
    driver
        .client_mut()
        .fail_next(qbo_local::client::QboError::Network("dropped".into()));
    let first = driver.sync_entity_type(&store, &realm(), EntityType::Invoice, at(2));
    assert!(first.is_err());

    let cursor = store.load_cursor(&realm(), EntityType::Invoice).unwrap();
    assert_eq!(
        cursor.last_cdc_cursor, None,
        "a partial sweep must not leave a cursor behind, or the rest is never fetched"
    );

    // Retrying picks up the whole type, not the remainder of a lost sweep.
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(3))
        .unwrap();
    assert!(matches!(report.path, SyncPath::FullSweep { .. }));
    assert_eq!(
        store.count_projected(&realm(), ProjectedTable::Documents).unwrap(),
        15
    );
}

#[test]
fn a_payload_that_will_not_project_is_still_mirrored() {
    // Losing the projection is recoverable. Losing the payload is not.
    let mut qbo = MockQbo::new(at(0));
    let mut broken = invoice("418", "1088", 1234.56, at(1));
    broken.raw_json.as_object_mut().unwrap().remove("TotalAmt");
    qbo.seed(&realm(), broken);

    let store = store();
    let mut driver = driver(qbo, options(100));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();

    assert_eq!(report.mirrored, 1);
    assert_eq!(report.quarantined, 1);
    assert!(store.get_document(&realm(), "418").unwrap().is_none());
    assert!(store.get_entity(&realm(), EntityType::Invoice, "418").unwrap().is_some());
    assert_eq!(store.quarantined(&realm()).unwrap().len(), 1);
}

#[test]
fn sync_refuses_rather_than_burning_through_the_rate_budget() {
    let mut qbo = MockQbo::new(at(0));
    for index in 0..50 {
        qbo.seed(
            &realm(),
            invoice(&format!("{index}"), &format!("{}", 6000 + index), 100.0, at(1)),
        );
    }

    let store = store();
    let mut driver = SyncDriver::new(
        qbo,
        SyncOptions {
            page_size: 1,
            limits: RealmLimits {
                general: qbo_local::ratelimit::BucketBudget::per_minute(3.0),
                ..RealmLimits::default()
            },
            ..SyncOptions::default()
        },
        at(0),
    );

    let outcome = driver.sync_entity_type(&store, &realm(), EntityType::Invoice, at(0));
    assert!(
        matches!(outcome, Err(SyncError::RateBudgetExhausted { .. })),
        "expected a refusal, got {outcome:?}"
    );
}

#[test]
fn a_sync_that_returned_nothing_still_records_that_it_ran() {
    let store = store();
    let mut driver = driver(MockQbo::new(at(0)), options(100));

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();
    assert_eq!(report.mirrored, 0);

    let cursor = store.load_cursor(&realm(), EntityType::Invoice).unwrap();
    assert_eq!(cursor.last_full_sweep, Some(at(2)));
    assert_eq!(
        cursor.last_cdc_cursor,
        Some(at(2)),
        "an empty book is synced, not unsynced — the next run should poll"
    );
}

#[test]
fn a_backfill_pages_through_a_window_bigger_than_one_page() {
    // The first test's window fitted in a single page, so it never proved the
    // loop terminates. This one makes the window larger than the page.
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("1", "7001", 100.0, at(1)));

    let store = store();
    let mut driver = driver(
        qbo,
        SyncOptions {
            cdc_response_cap: 2,
            ..options(3)
        },
    );
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(2))
        .unwrap();
    driver.client_mut().set_cdc_cap(2);

    for index in 2..=9 {
        driver.client_mut().seed(
            &realm(),
            invoice(&index.to_string(), &format!("700{index}"), 100.0, at(2 + index)),
        );
    }

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, at(30))
        .unwrap();

    match report.path {
        SyncPath::CdcThenBackfill { pages, .. } => {
            assert_eq!(pages, 3, "8 changed records at 3 per page");
        }
        other => panic!("expected a paged backfill, got {other:?}"),
    }
    assert_eq!(
        store.list_documents(&realm(), DocumentType::Invoice, 50).unwrap().len(),
        9
    );
}
