//! The CDC poll loop against the in-memory double. `DESIGN.md` §4, HANDOFF §2.5.
//!
//! The driver's own tests cover what a sync does. These cover when one runs:
//! the focused and idle cadence, focus changes mid-wait, and backing off a
//! refusing Intuit rather than polling it on the fixed schedule.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo, QboError};
use qbo_local::domain::{DocumentType, EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncError, SyncOptions, SyncPath};
use qbo_local::poll::{Focus, PollCadence, PollClock, PollLoop, TickOutcome};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::store::Store;
use serde_json::json;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap() + Duration::seconds(seconds)
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", at(0))
        .unwrap();
    store
}

fn invoice(id: &str, number: &str, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({
            "Id": id, "DocNumber": number, "TxnDate": "2026-07-14",
            "CustomerRef": { "value": "31" }, "TotalAmt": 100.0,
            "Line": [ { "LineNum": 1, "Description": "Ring buoy, 24 inch",
                        "Amount": 100.0, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
        }),
    }
}

/// Generous budgets: these tests are about scheduling, not rate limiting.
fn options() -> SyncOptions {
    SyncOptions {
        page_size: 100,
        limits: RealmLimits {
            general: BucketBudget::per_minute(100_000.0),
            ..RealmLimits::default()
        },
        ..SyncOptions::default()
    }
}

fn poll_loop_with(qbo: MockQbo, options: SyncOptions, cadence: PollCadence) -> PollLoop<MockQbo> {
    PollLoop::new(
        SyncDriver::new(qbo, options, at(0)),
        realm(),
        vec![EntityType::Invoice],
        cadence,
    )
}

fn poll_loop(qbo: MockQbo) -> PollLoop<MockQbo> {
    poll_loop_with(qbo, options(), PollCadence::default())
}

fn invoices(store: &Store) -> usize {
    store
        .list_documents(&realm(), DocumentType::Invoice, 500)
        .unwrap()
        .len()
}

fn synced(outcome: Result<TickOutcome, SyncError>) -> qbo_local::driver::SyncReport {
    match outcome.unwrap() {
        TickOutcome::Synced(report) => report,
        other => panic!("expected a sync, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Cadence
// ---------------------------------------------------------------------------

#[test]
fn the_first_tick_syncs_immediately() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&realm(), invoice("418", "1088", at(-60)));
    let store = store();
    let mut poll = poll_loop(qbo);

    let report = synced(poll.tick(&store, at(0), Focus::Idle, 0.5));
    assert!(matches!(
        report.entities[0].path,
        SyncPath::FullSweep { .. }
    ));
    assert_eq!(invoices(&store), 1);
}

#[test]
fn focused_polls_every_fifteen_seconds_and_picks_up_changes() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));

    poll.driver_mut()
        .client_mut()
        .seed(&realm(), invoice("419", "1089", at(5)));

    // Not yet: nothing spent, nothing fetched.
    let calls_before = poll.driver_mut().client_mut().query_calls;
    match poll.tick(&store, at(14), Focus::Focused, 0.5).unwrap() {
        TickOutcome::NotDue { due_at } => assert_eq!(due_at, at(15)),
        other => panic!("expected not due, got {other:?}"),
    }
    assert_eq!(poll.driver_mut().client_mut().query_calls, calls_before);
    assert_eq!(invoices(&store), 0);

    let report = synced(poll.tick(&store, at(15), Focus::Focused, 0.5));
    assert!(matches!(report.entities[0].path, SyncPath::Cdc { .. }));
    assert_eq!(invoices(&store), 1);
}

#[test]
fn idle_polls_every_five_minutes() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Idle, 0.5));

    assert!(matches!(
        poll.tick(&store, at(15), Focus::Idle, 0.5).unwrap(),
        TickOutcome::NotDue { .. }
    ));
    assert!(matches!(
        poll.tick(&store, at(299), Focus::Idle, 0.5).unwrap(),
        TickOutcome::NotDue { .. }
    ));
    synced(poll.tick(&store, at(300), Focus::Idle, 0.5));
}

#[test]
fn gaining_focus_mid_wait_brings_the_next_poll_forward() {
    // The user comes back to the app two minutes into a five-minute idle wait.
    // The next poll is due 15 seconds after the last one, which is already
    // past, so it runs now rather than three minutes from now.
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Idle, 0.5));

    assert_eq!(poll.due_at(Focus::Idle), Some(at(300)));
    assert_eq!(poll.due_at(Focus::Focused), Some(at(15)));
    synced(poll.tick(&store, at(120), Focus::Focused, 0.5));
}

#[test]
fn the_cadence_is_configurable() {
    let store = store();
    let mut poll = poll_loop_with(
        MockQbo::new(at(0)),
        options(),
        PollCadence {
            focused: Duration::seconds(5),
            idle: Duration::seconds(60),
        },
    );
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));
    assert_eq!(poll.due_at(Focus::Focused), Some(at(5)));
    assert_eq!(poll.due_at(Focus::Idle), Some(at(60)));
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_rate_limited_poll_backs_off_and_focus_does_not_override_it() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));

    // Four 429s in a row. Jitter of 1.0 makes the backoffs exactly 2, 4, 8
    // and 16 seconds.
    for _ in 0..4 {
        poll.driver_mut()
            .client_mut()
            .fail_next(QboError::RateLimited);
    }
    let mut now = at(15);
    let mut delays = Vec::new();
    for _ in 0..4 {
        match poll.tick(&store, now, Focus::Focused, 1.0).unwrap() {
            TickOutcome::Deferred { retry_at, .. } => {
                delays.push((retry_at - now).num_seconds());
                now = retry_at;
            }
            other => panic!("expected a deferral, got {other:?}"),
        }
    }
    assert_eq!(poll.consecutive_failures(), 4);
    // The first two are under the 15-second focused cadence, so the cadence
    // wins; after that the backoff is the floor.
    assert_eq!(delays, [15, 15, 15, 16]);

    // Recovery clears the backoff and the failure count.
    synced(poll.tick(&store, now, Focus::Focused, 1.0));
    assert_eq!(poll.consecutive_failures(), 0);
    assert_eq!(
        poll.due_at(Focus::Focused),
        Some(now + Duration::seconds(15))
    );
}

#[test]
fn a_deferred_poll_moves_no_cursor_and_loses_nothing() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));
    let cursor_before = store.load_cursor(&realm(), EntityType::Invoice).unwrap();

    poll.driver_mut()
        .client_mut()
        .seed(&realm(), invoice("419", "1089", at(5)));
    poll.driver_mut()
        .client_mut()
        .fail_next(QboError::Network("connection reset".into()));

    assert!(matches!(
        poll.tick(&store, at(15), Focus::Focused, 0.5).unwrap(),
        TickOutcome::Deferred { .. }
    ));
    assert_eq!(
        store.load_cursor(&realm(), EntityType::Invoice).unwrap(),
        cursor_before
    );

    // The retry covers the same window, so the invoice changed during the
    // outage still arrives.
    synced(poll.tick(&store, at(30), Focus::Focused, 0.5));
    assert_eq!(invoices(&store), 1);
}

#[test]
fn a_non_transient_error_is_surfaced_and_retried_on_the_cadence() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));

    poll.driver_mut()
        .client_mut()
        .fail_next(QboError::Validation("malformed query".into()));
    assert!(matches!(
        poll.tick(&store, at(15), Focus::Focused, 0.5),
        Err(SyncError::Qbo(QboError::Validation(_)))
    ));
    assert_eq!(poll.due_at(Focus::Focused), Some(at(30)));
    synced(poll.tick(&store, at(30), Focus::Focused, 0.5));
}

#[test]
fn a_truncated_poll_inside_the_loop_still_lands_every_record() {
    // The driver's backfill path, reached through the loop: a burst of edits
    // between two polls overflows the CDC cap.
    let mut qbo = MockQbo::new(at(0));
    qbo.set_cdc_cap(3);
    let store = store();
    let mut poll = poll_loop_with(
        qbo,
        SyncOptions {
            cdc_response_cap: 3,
            ..options()
        },
        PollCadence::default(),
    );
    synced(poll.tick(&store, at(0), Focus::Focused, 0.5));

    for index in 0..7 {
        poll.driver_mut().client_mut().seed(
            &realm(),
            invoice(
                &format!("5{index:02}"),
                &format!("2{index:03}"),
                at(1 + index),
            ),
        );
    }
    let report = synced(poll.tick(&store, at(15), Focus::Focused, 0.5));
    assert!(matches!(
        report.entities[0].path,
        SyncPath::CdcThenBackfill { .. }
    ));
    assert_eq!(invoices(&store), 7);
}

// ---------------------------------------------------------------------------
// The outer loop
// ---------------------------------------------------------------------------

/// Advances instead of sleeping, flips focus at a scripted time, and stops the
/// loop after a set number of syncs.
struct FakeClock {
    now: DateTime<Utc>,
    focus_from: DateTime<Utc>,
    slept: Duration,
}

impl PollClock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    fn focus(&self) -> Focus {
        if self.now >= self.focus_from {
            Focus::Focused
        } else {
            Focus::Idle
        }
    }

    fn sleep(&mut self, duration: Duration) {
        assert!(
            duration <= Duration::seconds(qbo_local::poll::SLEEP_SLICE_SECONDS),
            "slept {duration} in one go; focus changes would go unnoticed"
        );
        self.now += duration;
        self.slept += duration;
    }

    fn jitter(&mut self) -> f64 {
        0.5
    }
}

#[test]
fn the_run_loop_follows_the_cadence_and_reacts_to_focus() {
    let store = store();
    let mut poll = poll_loop(MockQbo::new(at(0)));
    // Idle for the first ten minutes, focused after.
    let mut clock = FakeClock {
        now: at(0),
        focus_from: at(600),
        slept: Duration::zero(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let mut syncs = 0;

    let stop_flag = Arc::clone(&stop);
    poll.run(&store, &mut clock, &stop, |outcome| {
        if let Ok(TickOutcome::Synced(_)) = outcome {
            syncs += 1;
            if syncs == 5 {
                stop_flag.store(true, Ordering::Relaxed);
            }
        }
    });

    // Syncs at 0, 300 and 600 while idle; at 600 focus arrives, so the next
    // two are 15 seconds apart: 615 and 630.
    assert_eq!(syncs, 5);
    assert_eq!(clock.now, at(630));
    assert_eq!(clock.slept, Duration::seconds(630));
}
