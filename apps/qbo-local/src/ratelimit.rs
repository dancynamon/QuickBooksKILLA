//! Per-realm rate limiting. `DESIGN.md` §4.4.
//!
//! Every budget here is a config value rather than a constant buried in code,
//! because Intuit changes them: the batch limit moved from ~40/min to 120/min in
//! October 2025, which the kickoff prompt predated.
//!
//! Budgets are per realm. A heavy sweep on Aquamentor must not throttle
//! WaterLine.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Which of Intuit's limits an outbound request draws against.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BucketClass {
    /// General API surface: 500/min/realm.
    General,
    /// The batch endpoint: 120/min/realm as of 31 Oct 2025.
    Batch,
    /// Reports and other resource-intensive endpoints: 200/min/realm.
    Reports,
}

#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct BucketBudget {
    pub capacity: f64,
    pub refill_per_minute: f64,
}

impl BucketBudget {
    pub const fn per_minute(rate: f64) -> Self {
        BucketBudget {
            capacity: rate,
            refill_per_minute: rate,
        }
    }
}

/// Config for one realm's limits. Serialisable so it lives in config, not code.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RealmLimits {
    pub general: BucketBudget,
    pub batch: BucketBudget,
    pub reports: BucketBudget,
    pub max_concurrent: usize,
}

impl Default for RealmLimits {
    fn default() -> Self {
        RealmLimits {
            general: BucketBudget::per_minute(500.0),
            // Corrects the kickoff prompt's ~40/min (DESIGN.md §0).
            batch: BucketBudget::per_minute(120.0),
            reports: BucketBudget::per_minute(200.0),
            max_concurrent: 10,
        }
    }
}

#[derive(Clone, Debug)]
struct TokenBucket {
    budget: BucketBudget,
    tokens: f64,
    last_refill: DateTime<Utc>,
}

impl TokenBucket {
    fn new(budget: BucketBudget, now: DateTime<Utc>) -> Self {
        TokenBucket {
            budget,
            tokens: budget.capacity,
            last_refill: now,
        }
    }

    fn refill(&mut self, now: DateTime<Utc>) {
        if now <= self.last_refill {
            return;
        }
        let minutes = (now - self.last_refill).num_milliseconds() as f64 / 60_000.0;
        self.tokens =
            (self.tokens + minutes * self.budget.refill_per_minute).min(self.budget.capacity);
        self.last_refill = now;
    }

    fn try_acquire(&mut self, now: DateTime<Utc>) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn available(&self) -> f64 {
        self.tokens
    }
}

/// One realm's limiter. Not shared across realms — that separation is the point.
#[derive(Clone, Debug)]
pub struct RealmLimiter {
    general: TokenBucket,
    batch: TokenBucket,
    reports: TokenBucket,
    in_flight: usize,
    max_concurrent: usize,
}

impl RealmLimiter {
    pub fn new(limits: RealmLimits, now: DateTime<Utc>) -> Self {
        RealmLimiter {
            general: TokenBucket::new(limits.general, now),
            batch: TokenBucket::new(limits.batch, now),
            reports: TokenBucket::new(limits.reports, now),
            in_flight: 0,
            max_concurrent: limits.max_concurrent,
        }
    }

    fn bucket(&mut self, class: BucketClass) -> &mut TokenBucket {
        match class {
            BucketClass::General => &mut self.general,
            BucketClass::Batch => &mut self.batch,
            BucketClass::Reports => &mut self.reports,
        }
    }

    /// Take a slot for one outbound request, or refuse.
    ///
    /// Refusal means wait, never proceed anyway — a 429 costs more than the
    /// delay avoided.
    pub fn try_acquire(&mut self, class: BucketClass, now: DateTime<Utc>) -> bool {
        if self.in_flight >= self.max_concurrent {
            return false;
        }
        if self.bucket(class).try_acquire(now) {
            self.in_flight += 1;
            true
        } else {
            false
        }
    }

    /// Release a concurrency slot once a request completes, however it ended.
    pub fn release(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub fn available(&self, class: BucketClass) -> f64 {
        match class {
            BucketClass::General => self.general.available(),
            BucketClass::Batch => self.batch.available(),
            BucketClass::Reports => self.reports.available(),
        }
    }
}

/// Cap on exponential backoff, so a long outage does not produce hour-long waits.
pub const MAX_BACKOFF_SECONDS: i64 = 300;

/// Exponential backoff with jitter, for 429 and 5xx responses.
///
/// `jitter` is supplied by the caller in `0.0..=1.0` rather than drawn inside,
/// so the schedule is deterministic under test. Jitter is full-range: without it
/// every queued request retries at the same instant and the thundering herd
/// reproduces the overload that caused the 429.
pub fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let exponential = 2f64
        .powi(attempt.min(16) as i32)
        .min(MAX_BACKOFF_SECONDS as f64);
    let jittered = exponential * jitter.clamp(0.0, 1.0);
    Duration::milliseconds((jittered * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + seconds, 0).unwrap()
    }

    fn limiter() -> RealmLimiter {
        RealmLimiter::new(RealmLimits::default(), at(0))
    }

    #[test]
    fn the_batch_budget_reflects_the_october_2025_change() {
        // The kickoff prompt said ~40/min; it is 120/min.
        let limits = RealmLimits::default();
        assert_eq!(limits.batch.refill_per_minute, 120.0);
        assert_eq!(limits.general.refill_per_minute, 500.0);
        assert_eq!(limits.reports.refill_per_minute, 200.0);
    }

    #[test]
    fn requests_are_granted_until_the_budget_is_spent() {
        let mut limiter = limiter();
        // Concurrency would bind first, so release after each acquire to isolate
        // the bucket behaviour.
        for _ in 0..500 {
            assert!(limiter.try_acquire(BucketClass::General, at(0)));
            limiter.release();
        }
        assert!(!limiter.try_acquire(BucketClass::General, at(0)));
    }

    #[test]
    fn buckets_refill_over_time() {
        let mut limiter = limiter();
        for _ in 0..500 {
            assert!(limiter.try_acquire(BucketClass::General, at(0)));
            limiter.release();
        }
        assert!(!limiter.try_acquire(BucketClass::General, at(0)));

        // Six seconds is a tenth of a minute: 50 tokens back.
        assert!(limiter.try_acquire(BucketClass::General, at(6)));
        limiter.release();
        assert!((limiter.available(BucketClass::General) - 49.0).abs() < 0.001);
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let mut limiter = limiter();
        assert!(limiter.try_acquire(BucketClass::General, at(0)));
        limiter.release();
        // An hour later the bucket is full, not overflowing.
        limiter.try_acquire(BucketClass::General, at(3600));
        limiter.release();
        assert!(limiter.available(BucketClass::General) <= 500.0);
    }

    #[test]
    fn the_classes_draw_on_separate_budgets() {
        let mut limiter = limiter();
        for _ in 0..120 {
            assert!(limiter.try_acquire(BucketClass::Batch, at(0)));
            limiter.release();
        }
        assert!(!limiter.try_acquire(BucketClass::Batch, at(0)));
        // Exhausting batch must not touch the general budget.
        assert!(limiter.try_acquire(BucketClass::General, at(0)));
    }

    #[test]
    fn concurrency_is_capped_independently_of_the_budget() {
        let mut limiter = limiter();
        for _ in 0..10 {
            assert!(limiter.try_acquire(BucketClass::General, at(0)));
        }
        // Budget remains, but ten are already in flight.
        assert!(!limiter.try_acquire(BucketClass::General, at(0)));
        assert!(limiter.available(BucketClass::General) > 0.0);

        limiter.release();
        assert!(limiter.try_acquire(BucketClass::General, at(0)));
    }

    #[test]
    fn a_refused_request_does_not_consume_a_concurrency_slot() {
        let mut limiter = limiter();
        for _ in 0..10 {
            limiter.try_acquire(BucketClass::General, at(0));
        }
        assert_eq!(limiter.in_flight(), 10);
        assert!(!limiter.try_acquire(BucketClass::General, at(0)));
        assert_eq!(limiter.in_flight(), 10, "refusal leaked a slot");
    }

    #[test]
    fn release_does_not_underflow() {
        let mut limiter = limiter();
        limiter.release();
        limiter.release();
        assert_eq!(limiter.in_flight(), 0);
    }

    #[test]
    fn realms_do_not_share_budgets() {
        // Aquamentor and WaterLine hold separate limiters, so exhausting one
        // leaves the other untouched.
        let mut aquamentor = limiter();
        let mut waterline = limiter();
        for _ in 0..500 {
            aquamentor.try_acquire(BucketClass::General, at(0));
            aquamentor.release();
        }
        assert!(!aquamentor.try_acquire(BucketClass::General, at(0)));
        assert!(waterline.try_acquire(BucketClass::General, at(0)));
    }

    #[test]
    fn a_fifteen_second_poll_is_a_rounding_error_against_the_budget() {
        // DESIGN.md §4.2: 4 requests/min/realm against 500 is under 1%.
        let mut limiter = limiter();
        for minute in 0..10 {
            for tick in 0..4 {
                assert!(limiter.try_acquire(BucketClass::General, at(minute * 60 + tick * 15)));
                limiter.release();
            }
        }
        assert!(limiter.available(BucketClass::General) > 495.0);
    }

    #[test]
    fn backoff_grows_and_then_caps() {
        // Full jitter for a deterministic reading of the ceiling.
        assert_eq!(backoff_delay(0, 1.0), Duration::seconds(1));
        assert_eq!(backoff_delay(1, 1.0), Duration::seconds(2));
        assert_eq!(backoff_delay(4, 1.0), Duration::seconds(16));
        assert_eq!(
            backoff_delay(20, 1.0),
            Duration::seconds(MAX_BACKOFF_SECONDS)
        );
    }

    #[test]
    fn jitter_spreads_retries() {
        // Without this, every queued request retries at the same instant and
        // recreates the overload that caused the 429.
        assert_eq!(backoff_delay(4, 0.0), Duration::zero());
        assert_eq!(backoff_delay(4, 0.5), Duration::seconds(8));
        assert_eq!(backoff_delay(4, 1.0), Duration::seconds(16));
    }

    proptest! {
        /// Never exceeds the cap and never returns a negative delay, for any
        /// attempt count or jitter value including out-of-range ones.
        #[test]
        fn backoff_is_always_bounded(attempt: u32, jitter: f64) {
            let delay = backoff_delay(attempt, jitter);
            prop_assert!(delay >= Duration::zero());
            prop_assert!(delay <= Duration::seconds(MAX_BACKOFF_SECONDS));
        }

        /// The limiter never grants more than the concurrency cap, whatever
        /// sequence of acquires and releases it sees.
        #[test]
        fn concurrency_cap_always_holds(operations: Vec<bool>) {
            let mut limiter = limiter();
            for (index, acquire) in operations.iter().enumerate() {
                if *acquire {
                    limiter.try_acquire(BucketClass::General, at(index as i64));
                } else {
                    limiter.release();
                }
                prop_assert!(limiter.in_flight() <= 10);
            }
        }
    }
}
