//! A clock abstraction so the daemon's cadence and backoff are testable.
//! `HANDOFF.md` §2.5.
//!
//! Nothing about the CDC poll loop is interesting to test against a real
//! clock: asserting "the daemon slept for five minutes" would make the suite
//! either flaky or five minutes long. Everything that waits goes through this
//! trait instead, so a test can drive time itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// A source of "now" and a way to wait, abstracted so [`crate::daemon::Daemon`]
/// can be driven deterministically under test.
pub trait Clock {
    fn now(&self) -> DateTime<Utc>;

    /// Wait for `d`. A real clock blocks the thread; a fake one advances its
    /// own notion of "now" and records the call so a test can assert not just
    /// where time ended up but the cadence that got it there.
    fn sleep(&self, d: Duration);
}

/// The clock production runs on: `now` is the wall clock, `sleep` actually
/// sleeps.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[derive(Debug)]
struct FakeClockState {
    now: DateTime<Utc>,
    sleeps: Vec<Duration>,
}

/// A clock a test owns outright: `now` only moves when `sleep` is called (or
/// the test calls [`FakeClock::advance`] directly to set a scene), and every
/// `sleep` is recorded.
///
/// Public on purpose, not a `#[cfg(test)]` fixture — `HANDOFF.md` §2.5 wants
/// the daemon's own integration tests built against this directly, and a
/// second private copy living only under test-cfg would drift from it.
#[derive(Clone, Debug)]
pub struct FakeClock {
    state: Arc<Mutex<FakeClockState>>,
}

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        FakeClock {
            state: Arc::new(Mutex::new(FakeClockState {
                now: start,
                sleeps: Vec::new(),
            })),
        }
    }

    /// Move `now` forward without going through `sleep` — for setting up a
    /// scenario (e.g. crossing into the next UTC day) rather than exercising
    /// the daemon's own waiting.
    pub fn advance(&self, d: Duration) {
        let mut state = self.lock();
        state.now += to_chrono(d);
    }

    /// Every duration passed to `sleep`, oldest first. This is what a test
    /// asserts cadence against — the sequence, not just the final clock value.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.lock().sleeps.clone()
    }

    /// Never panics on a poisoned lock: a fake clock used only to observe a
    /// daemon under test should not itself become the reason a test fails.
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeClockState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.lock().now
    }

    fn sleep(&self, d: Duration) {
        let mut state = self.lock();
        state.now += to_chrono(d);
        state.sleeps.push(d);
    }
}

/// `std::time::Duration` is unsigned and `chrono::Duration` is not, so the
/// conversion can only fail by overflowing `chrono::Duration`'s range — never
/// for the second-and-minute-scale durations a daemon actually sleeps.
fn to_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + secs, 0).unwrap()
    }

    #[test]
    fn sleeping_advances_now_and_is_recorded() {
        let clock = FakeClock::new(at(0));
        clock.sleep(Duration::from_secs(15));
        assert_eq!(clock.now(), at(15));
        assert_eq!(clock.sleeps(), vec![Duration::from_secs(15)]);

        clock.sleep(Duration::from_secs(300));
        assert_eq!(clock.now(), at(315));
        assert_eq!(
            clock.sleeps(),
            vec![Duration::from_secs(15), Duration::from_secs(300)]
        );
    }

    #[test]
    fn advance_moves_time_without_recording_a_sleep() {
        let clock = FakeClock::new(at(0));
        clock.advance(Duration::from_secs(3600));
        assert_eq!(clock.now(), at(3600));
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn clones_share_the_same_underlying_state() {
        // A daemon test hands out clones (e.g. one to the daemon, one kept for
        // assertions), and both must see the same clock.
        let clock = FakeClock::new(at(0));
        let handle = clock.clone();
        handle.sleep(Duration::from_secs(5));
        assert_eq!(clock.now(), at(5));
    }
}
