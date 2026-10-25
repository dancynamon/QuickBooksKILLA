//! The CDC poll loop: when to run the sync driver, and what to do when it
//! fails. `DESIGN.md` §4.2, `HANDOFF.md` §2.5.
//!
//! Every decision about *how* to sync already lives elsewhere — `plan_sync`
//! chooses between a poll and a sweep, the driver recovers truncation through
//! the bounded query endpoint (`DECISIONS.md` D12), and `Store::apply_batch`
//! keeps the cursor inside the transaction that wrote its entities. This module
//! decides only *when*, and it adds no path of its own that can move a cursor.
//!
//! Each realm gets its own [`SyncDriver`], and so its own rate budget: a heavy
//! sweep on one company must not throttle the other (§4.4). Failures are scoped
//! the same way. A transient failure backs one realm off; a permanent one halts
//! one realm until a person resumes it; only a local store failure — which no
//! realm can work around — stops the loop.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::client::QboClient;
use crate::domain::{EntityType, RealmId};
use crate::driver::{SyncDriver, SyncError, SyncReport};
use crate::ratelimit::backoff_delay;
use crate::store::{Store, StoreError};

/// How often to poll. §4.2: 15 seconds focused, 5 minutes idle, both from
/// config rather than code.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PollCadence {
    pub focused: StdDuration,
    pub idle: StdDuration,
}

impl Default for PollCadence {
    fn default() -> Self {
        PollCadence {
            focused: StdDuration::from_secs(15),
            idle: StdDuration::from_secs(5 * 60),
        }
    }
}

impl PollCadence {
    fn interval(&self, focus: Focus) -> Duration {
        let interval = match focus {
            Focus::Focused => self.focused,
            Focus::Idle => self.idle,
        };
        Duration::from_std(interval).unwrap_or(Duration::MAX)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Focused,
    Idle,
}

/// Shared switch the UI flips when the window gains or loses focus. Cheap to
/// clone and safe to set from another thread; the loop reads it every wake.
#[derive(Clone, Debug)]
pub struct FocusHandle(Arc<AtomicBool>);

impl FocusHandle {
    fn new(focus: Focus) -> Self {
        FocusHandle(Arc::new(AtomicBool::new(focus == Focus::Focused)))
    }

    pub fn set(&self, focus: Focus) {
        self.0.store(focus == Focus::Focused, Ordering::Relaxed);
    }

    pub fn get(&self) -> Focus {
        if self.0.load(Ordering::Relaxed) {
            Focus::Focused
        } else {
            Focus::Idle
        }
    }
}

/// Where a realm's polling stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealmHealth {
    Healthy,
    /// Transient failures in a row — rate budget, 429, network. Retried on a
    /// jittered exponential backoff, never faster than the cadence.
    BackingOff {
        consecutive_failures: u32,
        last_error: String,
    },
    /// A failure retrying cannot fix. The realm stops polling until
    /// [`PollLoop::resume`]; spinning against it would spend budget to learn
    /// nothing, and hide the problem behind a loop that looks alive.
    Halted {
        error: String,
    },
}

/// What one realm's turn in a tick did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Synced(SyncReport),
    Deferred { error: String, retry_in: Duration },
    Halted { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealmTick {
    pub realm: RealmId,
    pub outcome: PollOutcome,
}

#[derive(Debug, Error)]
pub enum PollError {
    /// The replica itself failed. Not scoped to a realm, so not something a
    /// backoff can wait out.
    #[error("store: {0}")]
    Store(#[from] StoreError),
}

/// Why [`PollLoop::run`] returned without an error.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PollExit {
    Stopped,
    /// Every realm is halted, or none was registered. Returning says so; a loop
    /// that kept sleeping would look healthy while syncing nothing.
    NothingToPoll,
}

/// Time as the loop sees it. Only ever used for cadence — cursors come from the
/// sync path, not from here.
pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
    fn sleep(&mut self, duration: StdDuration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn sleep(&mut self, duration: StdDuration) {
        std::thread::sleep(duration);
    }
}

/// Longest the loop sleeps before re-checking focus and the stop flag. Bounds
/// how long an idle-to-focused switch waits before its first fast poll.
pub const WAKE_SLICE: StdDuration = StdDuration::from_secs(1);

struct RealmPoller<C: QboClient> {
    realm: RealmId,
    entity_types: Vec<EntityType>,
    driver: SyncDriver<C>,
    last_attempt: Option<DateTime<Utc>>,
    /// Set while backing off: the delay chosen at the last failure.
    backoff: Option<Duration>,
    health: RealmHealth,
}

impl<C: QboClient> RealmPoller<C> {
    /// Due time is derived, not stored, so a focus change takes effect on the
    /// next check without rescheduling anything.
    fn next_due(&self, interval: Duration) -> Option<DateTime<Utc>> {
        if matches!(self.health, RealmHealth::Halted { .. }) {
            return None;
        }
        let Some(last) = self.last_attempt else {
            return Some(DateTime::<Utc>::MIN_UTC);
        };
        let wait = self
            .backoff
            .map_or(interval, |backoff| backoff.max(interval));
        Some(
            last.checked_add_signed(wait)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        )
    }
}

pub struct PollLoop<C: QboClient> {
    cadence: PollCadence,
    focus: FocusHandle,
    realms: Vec<RealmPoller<C>>,
    jitter: Box<dyn FnMut() -> f64 + Send>,
}

impl<C: QboClient> PollLoop<C> {
    /// Starts focused: the app polls fast when it opens.
    pub fn new(cadence: PollCadence) -> Self {
        PollLoop {
            cadence,
            focus: FocusHandle::new(Focus::Focused),
            realms: Vec::new(),
            jitter: Box::new(random_unit),
        }
    }

    /// Replace the jitter source, which must return values in `[0, 1]`. For
    /// tests that need a backoff they can predict.
    pub fn with_jitter(mut self, jitter: impl FnMut() -> f64 + Send + 'static) -> Self {
        self.jitter = Box::new(jitter);
        self
    }

    /// Register a realm with the driver — and so the rate budget — that is its
    /// alone. Due immediately.
    pub fn add_realm(
        &mut self,
        realm: RealmId,
        entity_types: Vec<EntityType>,
        driver: SyncDriver<C>,
    ) {
        self.realms.push(RealmPoller {
            realm,
            entity_types,
            driver,
            last_attempt: None,
            backoff: None,
            health: RealmHealth::Healthy,
        });
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn health(&self, realm: &RealmId) -> Option<&RealmHealth> {
        self.poller(realm).map(|poller| &poller.health)
    }

    pub fn driver_mut(&mut self, realm: &RealmId) -> Option<&mut SyncDriver<C>> {
        self.realms
            .iter_mut()
            .find(|poller| &poller.realm == realm)
            .map(|poller| &mut poller.driver)
    }

    /// Clear a halt, once a person has dealt with its cause. Due immediately.
    pub fn resume(&mut self, realm: &RealmId) {
        if let Some(poller) = self.realms.iter_mut().find(|poller| &poller.realm == realm) {
            poller.health = RealmHealth::Healthy;
            poller.backoff = None;
            poller.last_attempt = None;
        }
    }

    /// When this realm next polls. `None` while halted.
    pub fn next_due(&self, realm: &RealmId) -> Option<DateTime<Utc>> {
        let interval = self.cadence.interval(self.focus.get());
        self.poller(realm)
            .and_then(|poller| poller.next_due(interval))
    }

    /// Time until the earliest realm is due. `None` when nothing can poll.
    pub fn until_next_due(&self, now: DateTime<Utc>) -> Option<Duration> {
        let interval = self.cadence.interval(self.focus.get());
        self.realms
            .iter()
            .filter_map(|poller| poller.next_due(interval))
            .min()
            .map(|due| (due - now).max(Duration::zero()))
    }

    /// Poll every realm that is due at `now`, and report what each did.
    pub fn tick(&mut self, store: &Store, now: DateTime<Utc>) -> Result<Vec<RealmTick>, PollError> {
        let interval = self.cadence.interval(self.focus.get());
        let mut ticks = Vec::new();

        for poller in &mut self.realms {
            match poller.next_due(interval) {
                Some(due) if due <= now => {}
                _ => continue,
            }

            poller.last_attempt = Some(now);
            let result = poller
                .driver
                .sync_realm(store, &poller.realm, &poller.entity_types, now);

            // A failure part-way through a realm leaves the entity types before
            // it synced and their cursors committed (D13), so retrying the whole
            // realm repeats nothing that was not already idempotent.
            let outcome = match result {
                Ok(report) => {
                    poller.health = RealmHealth::Healthy;
                    poller.backoff = None;
                    PollOutcome::Synced(report)
                }
                Err(SyncError::Store(error)) => return Err(PollError::Store(error)),
                Err(error) if is_transient(&error) => {
                    let failures = match &poller.health {
                        RealmHealth::BackingOff {
                            consecutive_failures,
                            ..
                        } => consecutive_failures + 1,
                        _ => 1,
                    };
                    let retry_in = backoff_delay(failures, (self.jitter)()).max(interval);
                    poller.backoff = Some(retry_in);
                    poller.health = RealmHealth::BackingOff {
                        consecutive_failures: failures,
                        last_error: error.to_string(),
                    };
                    PollOutcome::Deferred {
                        error: error.to_string(),
                        retry_in,
                    }
                }
                Err(error) => {
                    poller.health = RealmHealth::Halted {
                        error: error.to_string(),
                    };
                    PollOutcome::Halted {
                        error: error.to_string(),
                    }
                }
            };

            ticks.push(RealmTick {
                realm: poller.realm.clone(),
                outcome,
            });
        }

        Ok(ticks)
    }

    /// Poll until `stop` is set or nothing is left that can poll.
    ///
    /// Sleeps in slices of at most [`WAKE_SLICE`], so a focus change or a stop
    /// is noticed within a second rather than at the end of a five-minute idle
    /// wait. `on_tick` sees every realm's outcome as it happens.
    pub fn run(
        &mut self,
        store: &Store,
        clock: &mut impl Clock,
        stop: &AtomicBool,
        mut on_tick: impl FnMut(&[RealmTick]),
    ) -> Result<PollExit, PollError> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(PollExit::Stopped);
            }

            let ticks = self.tick(store, clock.now())?;
            if !ticks.is_empty() {
                on_tick(&ticks);
            }

            let Some(wait) = self.until_next_due(clock.now()) else {
                return Ok(PollExit::NothingToPoll);
            };
            let wait = wait.to_std().unwrap_or(StdDuration::ZERO).min(WAKE_SLICE);
            if !wait.is_zero() && !stop.load(Ordering::Relaxed) {
                clock.sleep(wait);
            }
        }
    }

    fn poller(&self, realm: &RealmId) -> Option<&RealmPoller<C>> {
        self.realms.iter().find(|poller| &poller.realm == realm)
    }
}

/// Worth retrying unchanged: the rate budget refused, or Intuit said so
/// (`QboError::is_transient`). Everything else needs a person.
fn is_transient(error: &SyncError) -> bool {
    match error {
        SyncError::RateBudgetExhausted { .. } => true,
        SyncError::Qbo(error) => error.is_transient(),
        SyncError::Store(_) => false,
    }
}

/// A uniform value in `[0, 1)` for backoff jitter, without a dependency on a
/// random-number crate. `RandomState` is seeded randomly per instance, which is
/// all jitter needs — it spreads retries, it is not a security boundary.
fn random_unit() -> f64 {
    let bits = RandomState::new().hash_one(0u8) >> 11;
    bits as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_cadence_is_the_documented_one() {
        let cadence = PollCadence::default();
        assert_eq!(cadence.focused, StdDuration::from_secs(15));
        assert_eq!(cadence.idle, StdDuration::from_secs(300));
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..1000 {
            let value = random_unit();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn a_focus_handle_is_shared_between_clones() {
        let handle = FocusHandle::new(Focus::Focused);
        let other = handle.clone();
        other.set(Focus::Idle);
        assert_eq!(handle.get(), Focus::Idle);
    }
}
