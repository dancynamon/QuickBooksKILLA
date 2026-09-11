//! The CDC daemon loop. `HANDOFF.md` §2.5, `DESIGN.md` §4.2, §8.
//!
//! [`crate::driver::SyncDriver`] already makes the incremental-versus-sweep
//! decision and handles truncation (`DESIGN.md` §4.3) — this module is just
//! the loop that calls it on a cadence, backs off when QBO or the rate budget
//! says to slow down, and never stops running because of an error. A daemon
//! that exits on the first bad response stops mirroring changes silently,
//! which is worse than the error it was trying not to crash on.
//!
//! Nightly snapshots (`DESIGN.md` §8) are folded into the same loop rather
//! than run on a second timer, because both need the same answer to "has it
//! been long enough" against the same clock, and a daemon that already wakes
//! up every few seconds is a snapshot scheduler for free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Timelike, Utc};

use crate::client::QboClient;
use crate::clock::Clock;
use crate::domain::{EntityType, RealmId};
use crate::driver::{SyncDriver, SyncError, SyncOptions, SyncReport};
use crate::ratelimit::backoff_delay;
use crate::store::backup::{SnapshotPolicy, SnapshotReport};
use crate::store::{Store, StoreError};

/// How often the daemon polls, focused versus idle.
///
/// Both come from config, never a constant — `DESIGN.md` §4.2 gives 15
/// seconds and 5 minutes as the defaults, not as fixed values, and a machine
/// running two realms may want them different per realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    pub focused: Duration,
    pub idle: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence {
            focused: Duration::from_secs(15),
            idle: Duration::from_secs(5 * 60),
        }
    }
}

/// Everything one [`Daemon`] needs beyond the client and the clock.
#[derive(Clone)]
pub struct DaemonOptions {
    pub cadence: Cadence,
    pub sync: SyncOptions,
    /// `None` disables nightly snapshots entirely — useful for a test that
    /// only wants to exercise the sync side of the loop.
    pub snapshot: Option<SnapshotPolicy>,
    /// The UTC hour at or after which a snapshot may fire, once per day.
    /// Default 7 UTC — roughly 3am US Eastern, when nobody is using the app.
    pub snapshot_hour_utc: u32,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        DaemonOptions {
            cadence: Cadence::default(),
            sync: SyncOptions::default(),
            snapshot: None,
            snapshot_hour_utc: 7,
        }
    }
}

/// What one [`Daemon::tick`] did.
#[derive(Debug)]
pub struct Tick {
    pub report: Result<SyncReport, SyncError>,
    /// How long to wait before the next tick.
    pub next_delay: Duration,
    /// `None` when a snapshot was not due this tick. `Some(Err(_))` when one
    /// was due and failed — a snapshot failure is reported here, not
    /// propagated, because losing tonight's snapshot is not a reason to stop
    /// syncing (`DESIGN.md` §8: the replica itself is always rebuildable).
    pub snapshot: Option<Result<SnapshotReport, StoreError>>,
}

/// Runs [`SyncDriver`] on a cadence against a real clock, or a
/// [`crate::clock::FakeClock`] under test.
///
/// Generic over [`Clock`] for the same reason [`SyncDriver`] is generic over
/// [`QboClient`]: the whole loop, including its timing, is then exercised by
/// the test suite rather than only the parts that don't sleep.
pub struct Daemon<C: QboClient, K: Clock> {
    driver: SyncDriver<C>,
    clock: K,
    options: DaemonOptions,
    /// Flipped by whatever UI sits on top of this — focused cadence while the
    /// app is in front of someone, idle cadence while it isn't. A plain
    /// `Arc<AtomicBool>` rather than a channel because the UI thread only
    /// ever needs to set one bit, not queue a message.
    focus: Arc<AtomicBool>,
    last_snapshot_day: Option<NaiveDate>,
    /// Consecutive ticks that ended in a transient failure. Reset to zero by
    /// any tick that completes the sync, even one that mirrored nothing.
    consecutive_failures: u32,
}

impl<C: QboClient, K: Clock> Daemon<C, K> {
    /// Build the [`SyncDriver`] from `options.sync` and start the loop at
    /// `clock`'s current time — one source of truth for what the driver's
    /// rate budget and CDC settings are, rather than a driver assembled
    /// elsewhere that could disagree with `options`.
    pub fn new(client: C, clock: K, options: DaemonOptions) -> Self {
        let now = clock.now();
        let driver = SyncDriver::new(client, options.sync.clone(), now);
        Daemon {
            driver,
            clock,
            options,
            focus: Arc::new(AtomicBool::new(false)),
            last_snapshot_day: None,
            consecutive_failures: 0,
        }
    }

    /// A handle a UI thread can flip on activity, to force the focused
    /// cadence regardless of whether the last poll found anything.
    pub fn focus_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.focus)
    }

    /// The underlying client, for a test that wants to seed or script the
    /// double between ticks the way [`crate::driver::SyncDriver::client_mut`]
    /// already allows for the driver directly.
    pub fn client_mut(&mut self) -> &mut C {
        self.driver.client_mut()
    }

    /// Run one sync, decide how long to wait before the next one, and take a
    /// snapshot if one is due. Never returns an error: every failure this
    /// loop can hit is reported inside the [`Tick`] instead, because a daemon
    /// is exactly the place where "stop on error" is the wrong behaviour.
    pub fn tick(&mut self, store: &Store, realm: &RealmId) -> Tick {
        let now = self.clock.now();
        let scope: Vec<EntityType> = EntityType::m0_scope().collect();
        let report = self.driver.sync_realm(store, realm, &scope, now);
        let next_delay = self.next_delay(&report, now);
        let snapshot = self.maybe_snapshot(store, now);

        Tick {
            report,
            next_delay,
            snapshot,
        }
    }

    /// Loop [`Daemon::tick`] until `stop` is set, sleeping on `self.clock`
    /// between ticks and handing each [`Tick`] to `on_tick` before sleeping.
    ///
    /// `stop` is checked both before a tick starts and after the sleep that
    /// follows it — never in between — so a tick already in flight when `stop`
    /// is set is always allowed to finish and report itself through
    /// `on_tick` before the loop exits.
    pub fn run(
        &mut self,
        store: &Store,
        realm: &RealmId,
        stop: &AtomicBool,
        mut on_tick: impl FnMut(&Tick),
    ) {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let tick = self.tick(store, realm);
            let delay = tick.next_delay;
            on_tick(&tick);
            self.clock.sleep(delay);

            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
    }

    // -----------------------------------------------------------------------

    /// `DESIGN.md` §4.2's cadence rule: focused while the app is in front of
    /// someone or something actually changed, idle otherwise. A transient
    /// failure — the rate budget, or a `QboError` worth retrying — backs off
    /// instead of picking either cadence; anything else falls back to the
    /// idle cadence rather than hammering an endpoint that just rejected the
    /// request outright.
    fn next_delay(
        &mut self,
        report: &Result<SyncReport, SyncError>,
        now: DateTime<Utc>,
    ) -> Duration {
        match report {
            Ok(report) => {
                self.consecutive_failures = 0;
                let focused = self.focus.load(Ordering::Relaxed);
                if focused || report.mirrored() > 0 {
                    self.options.cadence.focused
                } else {
                    self.options.cadence.idle
                }
            }
            Err(SyncError::RateBudgetExhausted { .. }) => self.backoff(now),
            Err(SyncError::Qbo(qbo_error)) if qbo_error.is_transient() => self.backoff(now),
            Err(_) => {
                // A non-transient error — a validation failure, a stale token,
                // a store error. Retrying immediately would not help, but the
                // loop keeps running: the next scheduled poll may find the
                // underlying condition gone (someone fixed the record in QBO,
                // for instance).
                self.consecutive_failures += 1;
                self.options.cadence.idle
            }
        }
    }

    fn backoff(&mut self, now: DateTime<Utc>) -> Duration {
        self.consecutive_failures += 1;
        let jitter = jitter(now, self.consecutive_failures);
        backoff_delay(self.consecutive_failures, jitter)
            .to_std()
            .unwrap_or(Duration::ZERO)
    }

    /// Take a snapshot if none has run yet today and it is late enough in the
    /// UTC day to. A failed attempt does not mark the day as done, so the next
    /// tick — 15 seconds or 5 minutes later, not tomorrow — tries again.
    fn maybe_snapshot(
        &mut self,
        store: &Store,
        now: DateTime<Utc>,
    ) -> Option<Result<SnapshotReport, StoreError>> {
        let policy = self.options.snapshot.as_ref()?;
        if now.hour() < self.options.snapshot_hour_utc {
            return None;
        }

        let today = now.date_naive();
        if self.last_snapshot_day == Some(today) {
            return None;
        }

        let outcome = store.take_snapshot(policy, now);
        if outcome.is_ok() {
            self.last_snapshot_day = Some(today);
        }
        Some(outcome)
    }
}

/// A jitter value in `0.0..1.0` derived from the clock and the attempt count,
/// rather than pulling in a `rand` dependency for this one call site.
///
/// Not cryptographic, and it does not need to be: [`backoff_delay`]'s jitter
/// exists only to keep queued retries from firing at the same instant
/// (`ratelimit.rs`), and mixing the timestamp with the attempt number is
/// enough spread for that — including under a [`crate::clock::FakeClock`]
/// whose `now` may not move between consecutive failures.
fn jitter(now: DateTime<Utc>, attempt: u32) -> f64 {
    let seed = (now.timestamp_nanos_opt().unwrap_or(0) as u64)
        ^ u64::from(attempt).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mixed = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
    (mixed >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_in_range() {
        for attempt in 0..50 {
            let now = DateTime::from_timestamp(1_755_300_000 + i64::from(attempt) * 7, 0).unwrap();
            let value = jitter(now, attempt);
            assert!((0.0..1.0).contains(&value), "jitter out of range: {value}");
        }
    }

    #[test]
    fn default_cadence_matches_design() {
        let cadence = Cadence::default();
        assert_eq!(cadence.focused, Duration::from_secs(15));
        assert_eq!(cadence.idle, Duration::from_secs(300));
    }

    #[test]
    fn default_snapshot_hour_is_seven_utc() {
        assert_eq!(DaemonOptions::default().snapshot_hour_utc, 7);
    }
}
