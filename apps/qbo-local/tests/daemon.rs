//! The CDC daemon loop end to end, against the in-memory double and a
//! [`FakeClock`]. `HANDOFF.md` §2.5, `DESIGN.md` §4.2, §8.
//!
//! Every test here drives a clock the test owns, never a real one — the
//! whole point of [`FakeClock`] is that "the daemon waited five minutes" is
//! an assertion, not a five-minute test.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo, QboError};
use qbo_local::clock::FakeClock;
use qbo_local::daemon::{Cadence, Daemon, DaemonOptions};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::driver::{SyncError, SyncOptions};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::store::backup::SnapshotPolicy;
use qbo_local::store::Store;
use serde_json::json;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn day_start(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", day_start(1, 0))
        .unwrap();
    store
}

fn invoice(id: &str, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({ "Id": id, "TotalAmt": 100.0 }),
    }
}

/// A rate budget generous enough that these tests are about the daemon's
/// cadence, not about the limiter refusing a request.
fn generous_sync_options() -> SyncOptions {
    SyncOptions {
        limits: RealmLimits {
            general: BucketBudget::per_minute(100_000.0),
            ..RealmLimits::default()
        },
        ..SyncOptions::default()
    }
}

fn daemon_at(qbo: MockQbo, clock: FakeClock, sync: SyncOptions) -> Daemon<MockQbo, FakeClock> {
    Daemon::new(
        qbo,
        clock,
        DaemonOptions {
            sync,
            ..DaemonOptions::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Cadence
// ---------------------------------------------------------------------------

#[test]
fn a_tick_that_mirrors_something_polls_again_on_the_focused_cadence() {
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.seed(&realm(), invoice("1", day_start(1, 0)));

    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(qbo, clock, generous_sync_options());

    let tick = daemon.tick(&store, &realm());
    assert!(tick.report.as_ref().unwrap().mirrored() > 0);
    assert_eq!(tick.next_delay, Cadence::default().focused);
}

#[test]
fn a_tick_that_mirrors_nothing_polls_again_on_the_idle_cadence() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    let tick = daemon.tick(&store, &realm());
    assert_eq!(tick.report.as_ref().unwrap().mirrored(), 0);
    assert_eq!(tick.next_delay, Cadence::default().idle);
}

#[test]
fn the_focus_flag_forces_the_focused_cadence_even_with_nothing_to_mirror() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    daemon.focus_handle().store(true, Ordering::Relaxed);

    let tick = daemon.tick(&store, &realm());
    assert_eq!(
        tick.report.as_ref().unwrap().mirrored(),
        0,
        "nothing changed"
    );
    assert_eq!(
        tick.next_delay,
        Cadence::default().focused,
        "but focus wins anyway"
    );
}

// ---------------------------------------------------------------------------
// Backoff and error handling — the daemon never exits on an error
// ---------------------------------------------------------------------------

#[test]
fn a_transient_qbo_failure_backs_off_and_then_a_clean_tick_returns_to_cadence() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    // Repeat the fail/succeed cycle a few times. If the failure counter were
    // not reset on success, the backoff would keep growing cycle over cycle
    // (2s, then 4s, then 8s); resetting it keeps every failure at the same
    // first-attempt ceiling.
    for _ in 0..3 {
        daemon
            .client_mut()
            .fail_next(QboError::Network("connection reset".into()));
        let failed = daemon.tick(&store, &realm());
        assert!(
            matches!(failed.report, Err(SyncError::Qbo(QboError::Network(_)))),
            "expected a network failure, got {:?}",
            failed.report
        );
        assert!(
            failed.next_delay < Duration::from_secs(2),
            "a reset counter should keep every failure at the first-attempt backoff ceiling, got {:?}",
            failed.next_delay
        );

        let recovered = daemon.tick(&store, &realm());
        assert!(
            recovered.report.is_ok(),
            "the next tick should succeed with nothing scripted to fail"
        );
        assert_eq!(
            recovered.next_delay,
            Cadence::default().idle,
            "back to the ordinary cadence"
        );
    }
}

#[test]
fn a_validation_failure_is_not_transient_and_falls_back_to_the_idle_cadence() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    daemon
        .client_mut()
        .fail_next(QboError::Validation("bad request".into()));
    let tick = daemon.tick(&store, &realm());

    assert!(matches!(
        tick.report,
        Err(SyncError::Qbo(QboError::Validation(_)))
    ));
    // Not a backoff: a validation failure will not be fixed by retrying
    // sooner, so the daemon just falls back to its ordinary idle cadence.
    assert_eq!(tick.next_delay, Cadence::default().idle);

    // And the loop keeps running — the very next tick is a plain success.
    let next = daemon.tick(&store, &realm());
    assert!(next.report.is_ok());
}

#[test]
fn an_exhausted_rate_budget_backs_off_instead_of_panicking() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    // A budget of one request is exhausted partway through the very first
    // entity type's sweep.
    let tiny_budget = SyncOptions {
        limits: RealmLimits {
            general: BucketBudget::per_minute(0.0),
            ..RealmLimits::default()
        },
        ..SyncOptions::default()
    };
    let mut daemon = daemon_at(MockQbo::new(day_start(1, 0)), clock, tiny_budget);

    let tick = daemon.tick(&store, &realm());

    assert!(
        matches!(tick.report, Err(SyncError::RateBudgetExhausted { .. })),
        "expected a refusal, got {:?}",
        tick.report
    );
    assert!(
        tick.next_delay <= Duration::from_secs(qbo_local::ratelimit::MAX_BACKOFF_SECONDS as u64)
    );
}

// ---------------------------------------------------------------------------
// The run loop
// ---------------------------------------------------------------------------

#[test]
fn run_stops_after_the_requested_number_of_ticks_with_one_sleep_per_tick() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let clock_for_assertions = clock.clone();
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    let stop = AtomicBool::new(false);
    let mut ticks = 0usize;

    daemon.run(&store, &realm(), &stop, |_tick| {
        ticks += 1;
        if ticks >= 3 {
            stop.store(true, Ordering::Relaxed);
        }
    });

    assert_eq!(ticks, 3);
    assert_eq!(
        clock_for_assertions.sleeps().len(),
        3,
        "a tick that requests stop should still be allowed to sleep before the loop exits"
    );
}

#[test]
fn run_never_ticks_if_stop_is_already_set() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let clock_for_assertions = clock.clone();
    let mut daemon = daemon_at(
        MockQbo::new(day_start(1, 0)),
        clock,
        generous_sync_options(),
    );

    let stop = AtomicBool::new(true);
    let mut ticks = 0usize;
    daemon.run(&store, &realm(), &stop, |_tick| ticks += 1);

    assert_eq!(ticks, 0);
    assert!(clock_for_assertions.sleeps().is_empty());
}

// ---------------------------------------------------------------------------
// Nightly snapshots
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_fires_once_per_utc_day_at_or_after_the_configured_hour() {
    let store = store();
    let snapshot_dir = tempfile::tempdir().unwrap();
    let policy = SnapshotPolicy {
        directory: snapshot_dir.path().to_path_buf(),
        keep: 5,
    };

    let clock = FakeClock::new(day_start(1, 6)); // before the snapshot hour
    let clock_handle = clock.clone();
    let mut daemon = Daemon::new(
        MockQbo::new(day_start(1, 6)),
        clock,
        DaemonOptions {
            sync: generous_sync_options(),
            snapshot: Some(policy),
            snapshot_hour_utc: 7,
            ..DaemonOptions::default()
        },
    );

    let before_hour = daemon.tick(&store, &realm());
    assert!(
        before_hour.snapshot.is_none(),
        "too early in the day for a snapshot"
    );

    clock_handle.advance(Duration::from_secs(3600)); // 06:00 -> 07:00
    let first_after_hour = daemon.tick(&store, &realm());
    assert!(
        matches!(&first_after_hour.snapshot, Some(Ok(_))),
        "expected a snapshot once past the hour, got {:?}",
        first_after_hour.snapshot
    );

    let again_same_day = daemon.tick(&store, &realm());
    assert!(again_same_day.snapshot.is_none(), "only once per day");

    clock_handle.advance(Duration::from_secs(25 * 3600)); // 07:00 day 1 -> 08:00 day 2
    let next_day = daemon.tick(&store, &realm());
    assert!(
        matches!(&next_day.snapshot, Some(Ok(_))),
        "a new UTC day should snapshot again, got {:?}",
        next_day.snapshot
    );
}

#[test]
fn a_daemon_with_no_snapshot_policy_never_snapshots() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 7));
    let mut daemon = Daemon::new(
        MockQbo::new(day_start(1, 7)),
        clock,
        DaemonOptions {
            sync: generous_sync_options(),
            ..DaemonOptions::default()
        },
    );

    let tick = daemon.tick(&store, &realm());
    assert!(tick.snapshot.is_none());
}
