//! The CDC poll loop: when to run the sync driver. `DESIGN.md` §4, HANDOFF §2.5.
//!
//! The driver decides *what* a sync does — incremental poll, sweep, or bounded
//! backfill after a truncated response. This module decides only *when*: every
//! 15 seconds while the app has focus, every 5 minutes while it does not, and
//! later than either after a refusal, so a rate-limited or unreachable Intuit
//! is not hammered on the fixed cadence.
//!
//! The schedule is a pure function of the clock, the focus state and the last
//! outcome, so it is tested without sleeping. [`PollLoop::run`] is the thin
//! outer loop that sleeps between ticks; it takes a [`PollClock`] so the same
//! loop runs under test against a clock that advances instead of waiting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::client::QboClient;
use crate::domain::{EntityType, RealmId};
use crate::driver::{SyncDriver, SyncError, SyncReport};
use crate::ratelimit::backoff_delay;
use crate::store::Store;

/// Longest single sleep inside [`PollLoop::run`]. Bounds how long a focus change
/// or a stop request goes unnoticed during a 5-minute idle wait.
pub const SLEEP_SLICE_SECONDS: i64 = 1;

/// Whether the user is looking at the app. Decides the cadence.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Focused,
    Idle,
}

/// Poll intervals. Both configurable; the defaults are HANDOFF §2.5's.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PollCadence {
    pub focused: Duration,
    pub idle: Duration,
}

impl Default for PollCadence {
    fn default() -> Self {
        PollCadence {
            focused: Duration::seconds(15),
            idle: Duration::minutes(5),
        }
    }
}

impl PollCadence {
    pub fn interval(&self, focus: Focus) -> Duration {
        match focus {
            Focus::Focused => self.focused,
            Focus::Idle => self.idle,
        }
    }
}

/// What one tick did.
#[derive(Debug)]
pub enum TickOutcome {
    /// Not due yet. Nothing was spent.
    NotDue { due_at: DateTime<Utc> },
    /// A sync ran to completion.
    Synced(SyncReport),
    /// The sync was refused for a reason that clears on its own — rate budget,
    /// 429, network. Retried after a backoff; the replica is unchanged and no
    /// cursor moved, because the driver only advances one alongside the
    /// entities it covers.
    Deferred {
        error: SyncError,
        retry_at: DateTime<Utc>,
    },
}

/// Whether waiting and retrying the same sync can plausibly succeed.
pub fn is_transient(error: &SyncError) -> bool {
    match error {
        SyncError::RateBudgetExhausted { .. } => true,
        SyncError::Qbo(error) => error.is_transient(),
        SyncError::Store(_) => false,
    }
}

/// Time, focus, sleep and jitter — everything the loop takes from outside.
pub trait PollClock {
    fn now(&self) -> DateTime<Utc>;
    fn focus(&self) -> Focus;
    fn sleep(&mut self, duration: Duration);
    /// A value in `0.0..=1.0` for backoff jitter.
    fn jitter(&mut self) -> f64;
}

/// Polls one realm's entity types on the cadence.
pub struct PollLoop<C: QboClient> {
    driver: SyncDriver<C>,
    realm: RealmId,
    entity_types: Vec<EntityType>,
    cadence: PollCadence,
    /// When the last sync was attempted. `None` means the first tick runs
    /// immediately — opening the app should not wait an interval to refresh.
    last_attempt: Option<DateTime<Utc>>,
    /// Earliest the next attempt may run after a transient refusal.
    backoff_until: Option<DateTime<Utc>>,
    consecutive_failures: u32,
}

impl<C: QboClient> PollLoop<C> {
    pub fn new(
        driver: SyncDriver<C>,
        realm: RealmId,
        entity_types: Vec<EntityType>,
        cadence: PollCadence,
    ) -> Self {
        PollLoop {
            driver,
            realm,
            entity_types,
            cadence,
            last_attempt: None,
            backoff_until: None,
            consecutive_failures: 0,
        }
    }

    pub fn driver_mut(&mut self) -> &mut SyncDriver<C> {
        &mut self.driver
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// When the next sync should run under `focus`.
    ///
    /// Computed from the last attempt rather than stored, so gaining focus
    /// during a 5-minute idle wait brings the next poll forward to 15 seconds
    /// after the last one instead of waiting out the idle interval. A backoff
    /// is a floor that focus does not override: a rate-limited realm stays
    /// rate-limited however hard the user is looking.
    pub fn due_at(&self, focus: Focus) -> Option<DateTime<Utc>> {
        let cadence_due = self
            .last_attempt
            .map(|last| last + self.cadence.interval(focus));
        match (cadence_due, self.backoff_until) {
            (Some(due), Some(floor)) => Some(due.max(floor)),
            (due, floor) => due.or(floor),
        }
    }

    /// Run a sync if one is due. Non-transient errors are returned; the loop
    /// still reschedules, so the next tick retries on the normal cadence.
    pub fn tick(
        &mut self,
        store: &Store,
        now: DateTime<Utc>,
        focus: Focus,
        jitter: f64,
    ) -> Result<TickOutcome, SyncError> {
        if let Some(due_at) = self.due_at(focus) {
            if now < due_at {
                return Ok(TickOutcome::NotDue { due_at });
            }
        }

        self.last_attempt = Some(now);
        match self
            .driver
            .sync_realm(store, &self.realm, &self.entity_types, now)
        {
            Ok(report) => {
                self.consecutive_failures = 0;
                self.backoff_until = None;
                Ok(TickOutcome::Synced(report))
            }
            Err(error) if is_transient(&error) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                let retry_at = now + backoff_delay(self.consecutive_failures, jitter);
                self.backoff_until = Some(retry_at);
                Ok(TickOutcome::Deferred {
                    error,
                    retry_at: self.due_at(focus).unwrap_or(retry_at),
                })
            }
            Err(error) => {
                // Not a backoff case, and not the loop's to fix. Surface it and
                // try again on the ordinary cadence.
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.backoff_until = None;
                Err(error)
            }
        }
    }

    /// Tick until `stop` is set, sleeping between ticks in slices of at most
    /// [`SLEEP_SLICE_SECONDS`]. Every outcome, including errors, goes to
    /// `on_tick`; the loop never exits on a sync error.
    pub fn run<K, F>(
        &mut self,
        store: &Store,
        clock: &mut K,
        stop: &Arc<AtomicBool>,
        mut on_tick: F,
    ) where
        K: PollClock,
        F: FnMut(Result<TickOutcome, SyncError>),
    {
        let slice = Duration::seconds(SLEEP_SLICE_SECONDS);
        while !stop.load(Ordering::Relaxed) {
            let focus = clock.focus();
            let now = clock.now();
            match self.due_at(focus) {
                Some(due_at) if now < due_at => {
                    clock.sleep((due_at - now).min(slice));
                }
                _ => {
                    let jitter = clock.jitter();
                    on_tick(self.tick(store, now, focus, jitter));
                }
            }
        }
    }
}

/// The wall clock, with focus read from a flag the UI sets.
pub struct SystemClock {
    focused: Arc<AtomicBool>,
}

impl SystemClock {
    pub fn new(focused: Arc<AtomicBool>) -> Self {
        SystemClock { focused }
    }
}

impl PollClock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn focus(&self) -> Focus {
        if self.focused.load(Ordering::Relaxed) {
            Focus::Focused
        } else {
            Focus::Idle
        }
    }

    fn sleep(&mut self, duration: Duration) {
        if let Ok(duration) = duration.to_std() {
            std::thread::sleep(duration);
        }
    }

    fn jitter(&mut self) -> f64 {
        // Only needs to decorrelate retries, not be unpredictable. Sub-second
        // wall-clock nanos do that without pulling in a random-number crate.
        f64::from(Utc::now().timestamp_subsec_nanos()) / 1e9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cadence_is_fifteen_seconds_focused_five_minutes_idle() {
        let cadence = PollCadence::default();
        assert_eq!(cadence.interval(Focus::Focused), Duration::seconds(15));
        assert_eq!(cadence.interval(Focus::Idle), Duration::minutes(5));
    }

    #[test]
    fn transient_errors_are_the_ones_worth_waiting_out() {
        use crate::client::QboError;
        assert!(is_transient(&SyncError::RateBudgetExhausted {
            realm: "r".into()
        }));
        assert!(is_transient(&SyncError::Qbo(QboError::RateLimited)));
        assert!(is_transient(&SyncError::Qbo(QboError::Network(
            "reset".into()
        ))));
        assert!(!is_transient(&SyncError::Qbo(QboError::Validation(
            "bad query".into()
        ))));
    }
}
