//! The CDC poll loop against the in-memory double. `DESIGN.md` §4.2,
//! `HANDOFF.md` §2.5.
//!
//! The driver's own tests cover what one sync does. These cover what the loop
//! adds: when it polls, that each realm fails and backs off on its own, and that
//! nothing the loop does can move a cursor further than the driver proved.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use qbo_local::client::{EntityPayload, MockQbo, QboError};
use qbo_local::domain::{DocumentType, EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncOptions, SyncPath};
use qbo_local::poll::{
    Clock, Focus, PollCadence, PollExit, PollLoop, PollOutcome, RealmHealth, RealmTick,
};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::store::Store;
use serde_json::json;

fn aquamentor() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn waterline() -> RealmId {
    RealmId::parse("1234567890123457").unwrap()
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap() + Duration::seconds(seconds)
}

fn store(realms: &[RealmId]) -> Store {
    let store = Store::open_in_memory().unwrap();
    for realm in realms {
        store.register_realm(realm, "Test Company", at(0)).unwrap();
    }
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
            "Line": [ { "LineNum": 1, "Description": "Kickboard, blue",
                        "Amount": 100.0, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
        }),
    }
}

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

/// A loop over one realm, zero jitter so backoff is predictable.
fn single(qbo: MockQbo, options: SyncOptions) -> PollLoop<MockQbo> {
    let mut poll = PollLoop::new(PollCadence::default()).with_jitter(|| 0.0);
    poll.add_realm(
        aquamentor(),
        vec![EntityType::Invoice],
        SyncDriver::new(qbo, options, at(0)),
    );
    poll
}

fn outcome(ticks: &[RealmTick], realm: &RealmId) -> PollOutcome {
    ticks
        .iter()
        .find(|tick| &tick.realm == realm)
        .unwrap_or_else(|| panic!("realm {realm} did not poll"))
        .outcome
        .clone()
}

fn invoices(store: &Store, realm: &RealmId) -> usize {
    store
        .list_documents(realm, DocumentType::Invoice, 500)
        .unwrap()
        .len()
}

fn seed(poll: &mut PollLoop<MockQbo>, realm: &RealmId, payload: EntityPayload) {
    poll.driver_mut(realm)
        .unwrap()
        .client_mut()
        .seed(realm, payload);
}

// ---------------------------------------------------------------------------
// Cadence
// ---------------------------------------------------------------------------

#[test]
fn a_new_realm_syncs_on_the_first_tick() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&aquamentor(), invoice("1", "7001", at(0)));
    let store = store(&[aquamentor()]);
    let mut poll = single(qbo, options());

    let ticks = poll.tick(&store, at(1)).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Synced(_)
    ));
    assert_eq!(invoices(&store, &aquamentor()), 1);
}

#[test]
fn focused_polls_every_fifteen_seconds_and_not_before() {
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());

    poll.tick(&store, at(0)).unwrap();
    assert!(
        poll.tick(&store, at(14)).unwrap().is_empty(),
        "polled early"
    );
    assert_eq!(poll.tick(&store, at(15)).unwrap().len(), 1);
    assert_eq!(poll.next_due(&aquamentor()), Some(at(30)));
}

#[test]
fn idle_polls_every_five_minutes() {
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());
    poll.focus_handle().set(Focus::Idle);

    poll.tick(&store, at(0)).unwrap();
    assert!(poll.tick(&store, at(15)).unwrap().is_empty());
    assert!(poll.tick(&store, at(299)).unwrap().is_empty());
    assert_eq!(poll.tick(&store, at(300)).unwrap().len(), 1);
}

#[test]
fn regaining_focus_pulls_the_next_poll_in() {
    // Idle for a minute, then the user comes back: the next poll should be the
    // fast one, not whatever is left of the five-minute wait.
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());
    let focus = poll.focus_handle();
    focus.set(Focus::Idle);

    poll.tick(&store, at(0)).unwrap();
    assert!(poll.tick(&store, at(60)).unwrap().is_empty());
    focus.set(Focus::Focused);
    assert_eq!(poll.tick(&store, at(60)).unwrap().len(), 1);
}

#[test]
fn the_cadence_is_configurable() {
    let store = store(&[aquamentor()]);
    let mut poll = PollLoop::new(PollCadence {
        focused: StdDuration::from_secs(2),
        idle: StdDuration::from_secs(30),
    });
    poll.add_realm(
        aquamentor(),
        vec![EntityType::Invoice],
        SyncDriver::new(MockQbo::new(at(0)), options(), at(0)),
    );

    poll.tick(&store, at(0)).unwrap();
    assert_eq!(poll.tick(&store, at(2)).unwrap().len(), 1);
    poll.focus_handle().set(Focus::Idle);
    assert!(poll.tick(&store, at(31)).unwrap().is_empty());
    assert_eq!(poll.tick(&store, at(32)).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Keeping the replica current
// ---------------------------------------------------------------------------

#[test]
fn a_change_in_qbo_reaches_the_replica_on_the_next_poll_by_cdc() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&aquamentor(), invoice("1", "7001", at(0)));
    let store = store(&[aquamentor()]);
    let mut poll = single(qbo, options());
    poll.tick(&store, at(1)).unwrap();

    seed(&mut poll, &aquamentor(), invoice("2", "7002", at(5)));
    let ticks = poll.tick(&store, at(16)).unwrap();

    let PollOutcome::Synced(report) = outcome(&ticks, &aquamentor()) else {
        panic!("expected a sync");
    };
    assert!(matches!(report.entities[0].path, SyncPath::Cdc { .. }));
    assert_eq!(
        report.mirrored(),
        1,
        "incremental, not the whole book again"
    );
    assert_eq!(invoices(&store, &aquamentor()), 2);
}

#[test]
fn a_busy_window_is_backfilled_and_nothing_is_lost_across_polls() {
    // CDC capped at two, with bursts of five changes between polls. Every
    // burst truncates; every burst must still land in full.
    let mut qbo = MockQbo::new(at(0));
    qbo.set_cdc_cap(2);
    let store = store(&[aquamentor()]);
    let mut poll = single(
        qbo,
        SyncOptions {
            cdc_response_cap: 2,
            ..options()
        },
    );
    poll.tick(&store, at(0)).unwrap();

    let mut next_id = 1;
    for round in 1..=4 {
        let now = at(round * 15);
        for _ in 0..5 {
            let updated = now - Duration::seconds(10);
            seed(
                &mut poll,
                &aquamentor(),
                invoice(
                    &next_id.to_string(),
                    &format!("{}", 8000 + next_id),
                    updated,
                ),
            );
            next_id += 1;
        }
        let ticks = poll.tick(&store, now).unwrap();
        let PollOutcome::Synced(report) = outcome(&ticks, &aquamentor()) else {
            panic!("round {round} did not sync");
        };
        assert!(
            matches!(report.entities[0].path, SyncPath::CdcThenBackfill { .. }),
            "round {round}: a capped response must be backfilled, got {:?}",
            report.entities[0].path
        );
        assert_eq!(invoices(&store, &aquamentor()), (round * 5) as usize);
    }
}

#[test]
fn a_laptop_closed_for_a_month_sweeps_rather_than_polling() {
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());
    poll.tick(&store, at(0)).unwrap();

    let ticks = poll.tick(&store, at(0) + Duration::days(30)).unwrap();
    let PollOutcome::Synced(report) = outcome(&ticks, &aquamentor()) else {
        panic!("expected a sync");
    };
    assert!(matches!(
        report.entities[0].path,
        SyncPath::FullSweep { .. }
    ));
}

// ---------------------------------------------------------------------------
// Failure, scoped per realm
// ---------------------------------------------------------------------------

#[test]
fn a_network_failure_backs_off_and_moves_no_cursor() {
    let mut qbo = MockQbo::new(at(0));
    qbo.seed(&aquamentor(), invoice("1", "7001", at(0)));
    let store = store(&[aquamentor()]);
    let mut poll = single(qbo, options());
    poll.tick(&store, at(1)).unwrap();
    let cursor_before = store
        .load_cursor(&aquamentor(), EntityType::Invoice)
        .unwrap();

    seed(&mut poll, &aquamentor(), invoice("2", "7002", at(5)));
    poll.driver_mut(&aquamentor())
        .unwrap()
        .client_mut()
        .fail_next(QboError::Network("connection reset".into()));

    let ticks = poll.tick(&store, at(16)).unwrap();
    let PollOutcome::Deferred { retry_in, .. } = outcome(&ticks, &aquamentor()) else {
        panic!("a transient failure should defer, not halt");
    };
    assert!(
        retry_in >= Duration::seconds(15),
        "never faster than the cadence"
    );
    assert_eq!(
        store
            .load_cursor(&aquamentor(), EntityType::Invoice)
            .unwrap(),
        cursor_before,
        "a failed poll moved the cursor"
    );
    assert!(matches!(
        poll.health(&aquamentor()),
        Some(RealmHealth::BackingOff {
            consecutive_failures: 1,
            ..
        })
    ));

    // The retry picks up exactly what the failure missed.
    let due = poll.next_due(&aquamentor()).unwrap();
    let ticks = poll.tick(&store, due).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Synced(_)
    ));
    assert_eq!(invoices(&store, &aquamentor()), 2);
    assert_eq!(poll.health(&aquamentor()), Some(&RealmHealth::Healthy));
}

#[test]
fn repeated_failures_back_off_exponentially() {
    let store = store(&[aquamentor()]);
    let mut poll = PollLoop::new(PollCadence::default()).with_jitter(|| 1.0);
    let mut qbo = MockQbo::new(at(0));
    for _ in 0..8 {
        qbo.fail_next(QboError::RateLimited);
    }
    poll.add_realm(
        aquamentor(),
        vec![EntityType::Invoice],
        SyncDriver::new(qbo, options(), at(0)),
    );

    let mut now = at(0);
    let mut delays = Vec::new();
    for _ in 0..8 {
        let ticks = poll.tick(&store, now).unwrap();
        let PollOutcome::Deferred { retry_in, .. } = outcome(&ticks, &aquamentor()) else {
            panic!("expected a deferral");
        };
        delays.push(retry_in.num_seconds());
        now = poll.next_due(&aquamentor()).unwrap();
    }
    // Floored at the 15 s cadence, doubling from there, capped at 300 s.
    assert_eq!(delays, vec![15, 15, 15, 16, 32, 64, 128, 256]);
}

#[test]
fn an_exhausted_rate_budget_defers_rather_than_halting() {
    let store = store(&[aquamentor()]);
    let mut poll = single(
        MockQbo::new(at(0)),
        SyncOptions {
            limits: RealmLimits {
                general: BucketBudget {
                    capacity: 1.0,
                    refill_per_minute: 1.0,
                },
                ..RealmLimits::default()
            },
            ..options()
        },
    );
    poll.tick(&store, at(0)).unwrap();
    let ticks = poll.tick(&store, at(15)).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Deferred { .. }
    ));
}

#[test]
fn a_permanent_failure_halts_that_realm_until_resumed() {
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());
    poll.driver_mut(&aquamentor())
        .unwrap()
        .client_mut()
        .fail_next(QboError::Validation("unsupported query".into()));

    let ticks = poll.tick(&store, at(0)).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Halted { .. }
    ));
    assert_eq!(poll.next_due(&aquamentor()), None);
    assert!(
        poll.tick(&store, at(3600)).unwrap().is_empty(),
        "a halted realm polled"
    );

    poll.resume(&aquamentor());
    let ticks = poll.tick(&store, at(3601)).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Synced(_)
    ));
}

#[test]
fn one_realms_trouble_does_not_stop_the_other() {
    // §4.4: a heavy sweep on one company must not throttle the other. Each realm
    // has its own driver and budget, and its own failure state.
    let store = store(&[aquamentor(), waterline()]);
    let mut poll = PollLoop::new(PollCadence::default()).with_jitter(|| 0.0);

    let mut broken = MockQbo::new(at(0));
    broken.fail_next(QboError::Validation("bad".into()));
    poll.add_realm(
        aquamentor(),
        vec![EntityType::Invoice],
        SyncDriver::new(broken, options(), at(0)),
    );

    let mut healthy = MockQbo::new(at(0));
    healthy.seed(&waterline(), invoice("1", "9001", at(0)));
    poll.add_realm(
        waterline(),
        vec![EntityType::Invoice],
        SyncDriver::new(healthy, options(), at(0)),
    );

    let ticks = poll.tick(&store, at(1)).unwrap();
    assert!(matches!(
        outcome(&ticks, &aquamentor()),
        PollOutcome::Halted { .. }
    ));
    assert!(matches!(
        outcome(&ticks, &waterline()),
        PollOutcome::Synced(_)
    ));

    seed(&mut poll, &waterline(), invoice("2", "9002", at(5)));
    let ticks = poll.tick(&store, at(16)).unwrap();
    assert_eq!(ticks.len(), 1);
    assert_eq!(invoices(&store, &waterline()), 2);
}

#[test]
fn polling_never_enables_writes() {
    // HANDOFF §3.1: read-only until explicitly enabled, per realm.
    let store = store(&[aquamentor(), waterline()]);
    let mut poll = PollLoop::new(PollCadence::default());
    for realm in [aquamentor(), waterline()] {
        let mut qbo = MockQbo::new(at(0));
        qbo.seed(&realm, invoice("1", "7001", at(0)));
        poll.add_realm(
            realm,
            EntityType::m0_scope().collect(),
            SyncDriver::new(qbo, options(), at(0)),
        );
    }
    for seconds in [0, 15, 30] {
        poll.tick(&store, at(seconds)).unwrap();
    }
    assert!(!store.is_write_enabled(&aquamentor()).unwrap());
    assert!(!store.is_write_enabled(&waterline()).unwrap());
}

// ---------------------------------------------------------------------------
// The run loop
// ---------------------------------------------------------------------------

/// Simulated time: sleeping advances it instantly.
struct FakeClock {
    now: DateTime<Utc>,
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    fn sleep(&mut self, duration: StdDuration) {
        self.now += Duration::from_std(duration).unwrap();
    }
}

#[test]
fn run_polls_on_cadence_until_stopped() {
    let store = store(&[aquamentor()]);
    let mut poll = single(MockQbo::new(at(0)), options());
    let mut clock = FakeClock { now: at(0) };
    let stop = AtomicBool::new(false);
    let mut polls = 0;

    let exit = poll
        .run(&store, &mut clock, &stop, |_| {
            polls += 1;
            if polls == 5 {
                stop.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();

    assert_eq!(exit, PollExit::Stopped);
    // 0, 15, 30, 45, 60 — five polls in a simulated minute.
    assert_eq!(clock.now, at(60));
}

#[test]
fn run_returns_when_nothing_can_poll() {
    let store = store(&[aquamentor()]);
    let mut qbo = MockQbo::new(at(0));
    qbo.fail_next(QboError::Validation("bad".into()));
    let mut poll = single(qbo, options());

    let exit = poll
        .run(
            &store,
            &mut FakeClock { now: at(0) },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(exit, PollExit::NothingToPoll);
}
