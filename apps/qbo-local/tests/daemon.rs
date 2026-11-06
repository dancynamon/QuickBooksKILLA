//! The CDC daemon loop end to end, against the in-memory double and a
//! [`FakeClock`]. `HANDOFF.md` §2.5, `DESIGN.md` §4.2, §8.
//!
//! Every test here drives a clock the test owns, never a real one — the
//! whole point of [`FakeClock`] is that "the daemon waited five minutes" is
//! an assertion, not a five-minute test.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo, QboClient, QboError, ReadPage, UpdatedRange};
use qbo_local::clock::{Clock, FakeClock};
use qbo_local::daemon::{Cadence, Daemon, DaemonOptions};
use qbo_local::domain::{DocumentType, EntityType, RealmId};
use qbo_local::driver::{SyncError, SyncOptions, SyncPath};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::store::backup::SnapshotPolicy;
use qbo_local::store::Store;
use serde_json::json;
use uuid::Uuid;

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

fn tax(entity_type: EntityType, id: &str, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({ "Id": id, "Name": format!("Tax {id}"), "Active": true }),
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
fn a_tick_that_finds_a_change_polls_again_on_the_focused_cadence() {
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.seed(&realm(), invoice("1", day_start(1, 0)));

    let store = store();
    let clock = FakeClock::new(day_start(1, 1));
    let mut daemon = daemon_at(qbo, clock.clone(), generous_sync_options());
    daemon.tick(&store, &realm());

    // A new invoice after the first sweep reaches the replica through CDC.
    clock.advance(Duration::from_secs(300));
    daemon.client_mut().advance(day_start(1, 2));
    daemon.client_mut().seed(
        &realm(),
        invoice("2", day_start(1, 1) + chrono::Duration::minutes(2)),
    );

    let tick = daemon.tick(&store, &realm());
    assert!(tick.report.as_ref().unwrap().changed() > 0);
    assert_eq!(tick.next_delay, Cadence::default().focused);
}

#[test]
fn a_tick_of_sweeps_alone_is_not_activity() {
    // October 2026: re-mirrored sweep rows read as activity held the daemon
    // at fifteen seconds for days, about 125,000 calls a day.
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.seed(&realm(), invoice("1", day_start(1, 0)));

    let store = store();
    let clock = FakeClock::new(day_start(1, 1));
    let mut daemon = daemon_at(qbo, clock, generous_sync_options());

    let tick = daemon.tick(&store, &realm());
    assert!(tick.report.as_ref().unwrap().mirrored() > 0);
    assert_eq!(tick.report.as_ref().unwrap().changed(), 0);
    assert_eq!(tick.next_delay, Cadence::default().idle);
}

#[test]
fn three_idle_ticks_with_no_changes_hold_the_idle_cadence_and_rest_the_tax_sweeps() {
    // The October replay: a book with tax codes and no edits. Every tick must
    // land on the idle cadence, and only the first may sweep TaxCode/TaxRate.
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.seed(&realm(), invoice("1", day_start(1, 0)));
    qbo.seed(&realm(), tax(EntityType::TaxCode, "2", day_start(1, 0)));
    qbo.seed(&realm(), tax(EntityType::TaxRate, "3", day_start(1, 0)));

    let store = store();
    let clock = FakeClock::new(day_start(1, 1));
    let mut daemon = daemon_at(qbo, clock.clone(), generous_sync_options());

    let mut requests = Vec::new();
    for tick_no in 0..3 {
        let tick = daemon.tick(&store, &realm());
        let report = tick.report.as_ref().unwrap();
        assert_eq!(report.changed(), 0, "tick {tick_no}");
        assert_eq!(tick.next_delay, Cadence::default().idle, "tick {tick_no}");
        assert_ne!(tick.next_delay, Cadence::default().focused);
        for entity_type in [EntityType::TaxCode, EntityType::TaxRate] {
            let path = &report
                .entities
                .iter()
                .find(|entity| entity.entity_type == entity_type)
                .unwrap()
                .path;
            if tick_no == 0 {
                assert!(matches!(path, SyncPath::FullSweep { .. }));
            } else {
                assert!(
                    matches!(path, SyncPath::SweepNotDue { .. }),
                    "{entity_type:?} on tick {tick_no}: {path:?}"
                );
            }
        }
        requests.push(report.requests());
        clock.advance(tick.next_delay);
        daemon
            .client_mut()
            .advance(day_start(1, 1) + chrono::Duration::minutes(5 * (tick_no + 1)));
    }
    assert!(requests[1] < requests[0], "{requests:?}");
    assert_eq!(requests[1], requests[2]);
}

#[test]
fn a_spent_daily_budget_sleeps_until_the_utc_reset() {
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.fail_next(QboError::DailyBudgetExhausted(
        qbo_local::budget::BudgetExhausted {
            day: day_start(1, 0).date_naive(),
            used: 15_000,
            limit: 15_000,
            period: qbo_local::budget::BudgetPeriod::Day,
        },
    ));
    let store = store();
    let clock = FakeClock::new(day_start(1, 18));
    let mut daemon = daemon_at(qbo, clock, generous_sync_options());

    let tick = daemon.tick(&store, &realm());
    assert!(tick.report.is_err());
    assert_eq!(tick.next_delay, Duration::from_secs(6 * 3600 + 60));
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
fn run_stops_after_the_requested_number_of_ticks_without_waiting_out_the_last() {
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
    // Two full idle waits between the three ticks, slept in focused-cadence
    // slices; the stop the third tick asked for ends the loop before its wait.
    let sleeps = clock_for_assertions.sleeps();
    assert_eq!(sleeps.iter().sum::<Duration>(), Cadence::default().idle * 2);
    assert!(sleeps.iter().all(|d| *d <= Cadence::default().focused));
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

// ---------------------------------------------------------------------------
// Scenarios carried over from the stale poll-loop branch
// (`claude/happy-lehmann-b4dffd`, `tests/poll_loop.rs`), rewritten against
// this daemon. That branch's multi-realm `PollLoop` with halt/resume did not
// survive the rewrite; the behaviours below did, and nothing tested them here.
// ---------------------------------------------------------------------------

/// A [`FakeClock`] that raises flags once simulated time reaches them — how a
/// test flips focus, or asks for a stop, while the daemon is mid-wait.
#[derive(Clone)]
struct HookedClock {
    inner: FakeClock,
    hooks: Arc<Mutex<Vec<Hook>>>,
}

/// A flag to raise, and the simulated time to raise it at.
type Hook = (DateTime<Utc>, Arc<AtomicBool>);

impl HookedClock {
    fn new(start: DateTime<Utc>) -> Self {
        HookedClock {
            inner: FakeClock::new(start),
            hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn raise_at(&self, at: DateTime<Utc>, flag: Arc<AtomicBool>) {
        self.hooks.lock().unwrap().push((at, flag));
    }
}

impl Clock for HookedClock {
    fn now(&self) -> DateTime<Utc> {
        self.inner.now()
    }

    fn sleep(&self, d: Duration) {
        self.inner.sleep(d);
        let now = self.inner.now();
        self.hooks.lock().unwrap().retain(|(at, flag)| {
            if now >= *at {
                flag.store(true, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
    }
}

/// [`MockQbo`] that fails once, with `error`, on the first call touching
/// `entity_type` — a failure partway through a tick, after earlier tiers have
/// already synced, the way a token that expires mid-poll lands (a second 401
/// after the reactive refresh surfaces as [`QboError::Network`]).
struct FailOn {
    inner: MockQbo,
    entity_type: EntityType,
    error: Option<QboError>,
}

impl FailOn {
    fn check(&mut self, types: &[EntityType]) -> Result<(), QboError> {
        if types.contains(&self.entity_type) {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl QboClient for FailOn {
    fn query_page(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<ReadPage, QboError> {
        self.check(&[entity_type])?;
        self.inner
            .query_page(realm, entity_type, updated, start_position, max_results)
    }

    fn cdc_page(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<ReadPage, QboError> {
        self.check(entity_types)?;
        self.inner.cdc_page(realm, entity_types, changed_since)
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        self.inner.create(realm, entity_type, payload, request_id)
    }

    fn update(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
        payload: &serde_json::Value,
        base_sync_token: &str,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        self.inner.update(
            realm,
            entity_type,
            qbo_id,
            payload,
            base_sync_token,
            request_id,
        )
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.inner.find_by_name(realm, entity_type, name)
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.inner.fetch(realm, entity_type, qbo_id)
    }
}

fn other_realm() -> RealmId {
    RealmId::parse("1234567890123457").unwrap()
}

/// An invoice complete enough to project, so the replica's document list can
/// be counted rather than only the raw mirror.
fn projectable_invoice(id: &str, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({
            "Id": id, "DocNumber": format!("80{id}"), "TxnDate": "2026-07-14",
            "CustomerRef": { "value": "31" }, "TotalAmt": 100.0,
            "Line": [ { "LineNum": 1, "Description": "Kickboard, blue",
                        "Amount": 100.0, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
        }),
    }
}

fn invoices_in(store: &Store, realm: &RealmId) -> usize {
    store
        .list_documents(realm, DocumentType::Invoice, 500)
        .unwrap()
        .len()
}

fn invoice_path(report: &qbo_local::driver::SyncReport) -> &SyncPath {
    &report
        .entities
        .iter()
        .find(|entity| entity.entity_type == EntityType::Invoice)
        .unwrap()
        .path
}

#[test]
fn a_configured_cadence_replaces_both_defaults() {
    let store = store();
    let cadence = Cadence {
        focused: Duration::from_secs(2),
        idle: Duration::from_secs(30),
    };
    let mut daemon = Daemon::new(
        MockQbo::new(day_start(1, 0)),
        FakeClock::new(day_start(1, 0)),
        DaemonOptions {
            cadence,
            sync: generous_sync_options(),
            ..DaemonOptions::default()
        },
    );

    assert_eq!(daemon.tick(&store, &realm()).next_delay, cadence.idle);
    daemon.focus_handle().store(true, Ordering::Relaxed);
    assert_eq!(daemon.tick(&store, &realm()).next_delay, cadence.focused);
}

#[test]
fn regaining_focus_cuts_an_idle_wait_short() {
    // Idle for a minute, then the user comes back: the next poll should come
    // within one focused interval, not after whatever is left of five minutes.
    let store = store();
    let clock = HookedClock::new(day_start(1, 0));
    let mut daemon = Daemon::new(
        MockQbo::new(day_start(1, 0)),
        clock.clone(),
        DaemonOptions {
            sync: generous_sync_options(),
            ..DaemonOptions::default()
        },
    );
    clock.raise_at(
        day_start(1, 0) + chrono::Duration::seconds(60),
        daemon.focus_handle(),
    );

    let stop = AtomicBool::new(false);
    let mut tick_times = Vec::new();
    daemon.run(&store, &realm(), &stop, |_| {
        tick_times.push(clock.now());
        if tick_times.len() == 2 {
            stop.store(true, Ordering::Relaxed);
        }
    });

    assert_eq!(
        tick_times,
        vec![
            day_start(1, 0),
            day_start(1, 0) + chrono::Duration::seconds(60)
        ]
    );
}

#[test]
fn focus_does_not_cut_a_backoff_short() {
    // Focus means "someone is watching", not "QBO stopped throttling us".
    let store = store();
    let clock = HookedClock::new(day_start(1, 18));
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.fail_next(QboError::DailyBudgetExhausted(
        qbo_local::budget::BudgetExhausted {
            day: day_start(1, 0).date_naive(),
            used: 15_000,
            limit: 15_000,
            period: qbo_local::budget::BudgetPeriod::Day,
        },
    ));
    let mut daemon = daemon_at_clock(qbo, clock.clone());
    clock.raise_at(
        day_start(1, 18) + chrono::Duration::seconds(30),
        daemon.focus_handle(),
    );

    let stop = AtomicBool::new(false);
    let mut tick_times = Vec::new();
    daemon.run(&store, &realm(), &stop, |_| {
        tick_times.push(clock.now());
        if tick_times.len() == 2 {
            stop.store(true, Ordering::Relaxed);
        }
    });

    assert_eq!(
        tick_times[1],
        day_start(2, 0) + chrono::Duration::seconds(60)
    );
}

#[test]
fn stop_during_a_long_wait_is_honoured_within_one_focused_interval() {
    // A spent daily budget sleeps to the UTC reset — six hours here. Typing
    // "stop" must not have to wait that out.
    let store = store();
    let clock = HookedClock::new(day_start(1, 18));
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.fail_next(QboError::DailyBudgetExhausted(
        qbo_local::budget::BudgetExhausted {
            day: day_start(1, 0).date_naive(),
            used: 15_000,
            limit: 15_000,
            period: qbo_local::budget::BudgetPeriod::Day,
        },
    ));
    let mut daemon = daemon_at_clock(qbo, clock.clone());
    let stop = Arc::new(AtomicBool::new(false));
    clock.raise_at(
        day_start(1, 18) + chrono::Duration::seconds(40),
        Arc::clone(&stop),
    );

    let mut ticks = 0;
    daemon.run(&store, &realm(), &stop, |_| ticks += 1);

    assert_eq!(ticks, 1);
    let waited = clock.now() - day_start(1, 18);
    assert!(
        waited <= chrono::Duration::seconds(40 + 15),
        "stop took {waited} to take effect"
    );
}

fn daemon_at_clock<C: QboClient, K: Clock>(qbo: C, clock: K) -> Daemon<C, K> {
    Daemon::new(
        qbo,
        clock,
        DaemonOptions {
            sync: generous_sync_options(),
            ..DaemonOptions::default()
        },
    )
}

#[test]
fn a_busy_window_is_backfilled_and_nothing_is_lost_across_polls() {
    // CDC capped at two, with bursts of five changes between polls. Every
    // burst truncates; every burst must still land in full.
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.set_cdc_cap(2);
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut daemon = daemon_at(
        qbo,
        clock.clone(),
        SyncOptions {
            cdc_response_cap: 2,
            ..generous_sync_options()
        },
    );
    daemon.tick(&store, &realm());

    let mut next_id = 1;
    for round in 1..=4 {
        clock.advance(Duration::from_secs(15));
        let updated = clock.now() - chrono::Duration::seconds(10);
        daemon.client_mut().advance(clock.now());
        for _ in 0..5 {
            daemon
                .client_mut()
                .seed(&realm(), projectable_invoice(&next_id.to_string(), updated));
            next_id += 1;
        }

        let tick = daemon.tick(&store, &realm());
        let report = tick.report.as_ref().unwrap();
        assert!(
            matches!(invoice_path(report), SyncPath::CdcThenBackfill { .. }),
            "round {round}: a capped response must be backfilled, got {:?}",
            invoice_path(report)
        );
        assert_eq!(invoices_in(&store, &realm()), round * 5, "round {round}");
    }
}

#[test]
fn a_failure_partway_through_a_tick_moves_no_cursor_and_the_retry_picks_up_what_it_missed() {
    let mut mock = MockQbo::new(day_start(1, 0));
    mock.seed(&realm(), projectable_invoice("1", day_start(1, 0)));
    let store = store();
    let clock = FakeClock::new(day_start(1, 1));
    let mut daemon = daemon_at_clock(
        FailOn {
            inner: mock,
            entity_type: EntityType::Invoice,
            error: None,
        },
        clock.clone(),
    );
    daemon.tick(&store, &realm());
    let cursor_before = store.load_cursor(&realm(), EntityType::Invoice).unwrap();

    clock.advance(Duration::from_secs(300));
    let edited_at = clock.now() - chrono::Duration::seconds(30);
    daemon.client_mut().inner.advance(clock.now());
    daemon
        .client_mut()
        .inner
        .seed(&realm(), projectable_invoice("2", edited_at));
    daemon.client_mut().error = Some(QboError::Network(
        "401 Unauthorized after token refresh".into(),
    ));

    let failed = daemon.tick(&store, &realm());
    assert!(
        matches!(failed.report, Err(SyncError::Qbo(QboError::Network(_)))),
        "{:?}",
        failed.report
    );
    assert_eq!(
        store.load_cursor(&realm(), EntityType::Invoice).unwrap(),
        cursor_before,
        "a failed poll moved the cursor"
    );
    assert_eq!(invoices_in(&store, &realm()), 1);

    clock.sleep(failed.next_delay);
    let retried = daemon.tick(&store, &realm());
    assert!(retried.report.is_ok(), "{:?}", retried.report);
    assert_eq!(invoices_in(&store, &realm()), 2, "the retry lost the edit");
}

#[test]
fn repeated_transient_failures_back_off_exponentially_up_to_the_cap() {
    let store = store();
    let clock = FakeClock::new(day_start(1, 0));
    let mut qbo = MockQbo::new(day_start(1, 0));
    for _ in 0..12 {
        qbo.fail_next(QboError::RateLimited);
    }
    let mut daemon = daemon_at(qbo, clock.clone(), generous_sync_options());

    let mut delays = Vec::new();
    for _ in 0..12 {
        let tick = daemon.tick(&store, &realm());
        assert!(matches!(
            tick.report,
            Err(SyncError::Qbo(QboError::RateLimited))
        ));
        delays.push(tick.next_delay);
        clock.sleep(tick.next_delay);
    }

    let cap = qbo_local::ratelimit::MAX_BACKOFF_SECONDS as u64;
    for (index, delay) in delays.iter().enumerate() {
        let ceiling = Duration::from_secs((1u64 << (index + 1)).min(cap));
        assert!(
            *delay <= ceiling,
            "attempt {}: {delay:?} over {ceiling:?}",
            index + 1
        );
    }
    // Full jitter can land any single attempt near zero, but a counter that
    // never grew would keep every one under the first ceiling of two seconds.
    assert!(
        delays.iter().any(|delay| *delay > Duration::from_secs(16)),
        "{delays:?}"
    );

    let recovered = daemon.tick(&store, &realm());
    assert!(recovered.report.is_ok());
    assert_eq!(recovered.next_delay, Cadence::default().idle);
}

#[test]
fn one_realms_daemon_failing_does_not_stop_anothers_on_the_same_store() {
    // One daemon per realm; a realm that keeps failing must not hold the other
    // one's rows back, even though both write into one replica.
    let store = store();
    store
        .register_realm(&other_realm(), "Other Company", day_start(1, 0))
        .unwrap();

    let mut broken = MockQbo::new(day_start(1, 0));
    for _ in 0..3 {
        broken.fail_next(QboError::Validation("bad".into()));
    }
    let mut broken = daemon_at(
        broken,
        FakeClock::new(day_start(1, 1)),
        generous_sync_options(),
    );

    let mut healthy = MockQbo::new(day_start(1, 0));
    healthy.seed(&other_realm(), projectable_invoice("1", day_start(1, 0)));
    let healthy_clock = FakeClock::new(day_start(1, 1));
    let mut healthy = daemon_at(healthy, healthy_clock.clone(), generous_sync_options());

    for _ in 0..3 {
        assert!(broken.tick(&store, &realm()).report.is_err());
        assert!(healthy.tick(&store, &other_realm()).report.is_ok());
    }
    healthy_clock.advance(Duration::from_secs(300));
    healthy.client_mut().advance(healthy_clock.now());
    healthy.client_mut().seed(
        &other_realm(),
        projectable_invoice("2", healthy_clock.now() - chrono::Duration::seconds(30)),
    );
    assert!(healthy.tick(&store, &other_realm()).report.is_ok());

    assert_eq!(invoices_in(&store, &other_realm()), 2);
    assert_eq!(invoices_in(&store, &realm()), 0);
}

#[test]
fn polling_never_enables_writes() {
    // HANDOFF §3.1: read-only until explicitly enabled, per realm.
    let mut qbo = MockQbo::new(day_start(1, 0));
    qbo.seed(&realm(), projectable_invoice("1", day_start(1, 0)));
    let store = store();
    let clock = FakeClock::new(day_start(1, 1));
    let mut daemon = daemon_at(qbo, clock.clone(), generous_sync_options());

    for _ in 0..3 {
        let tick = daemon.tick(&store, &realm());
        assert!(tick.report.is_ok());
        clock.sleep(tick.next_delay);
    }
    assert!(!store.is_write_enabled(&realm()).unwrap());
}
