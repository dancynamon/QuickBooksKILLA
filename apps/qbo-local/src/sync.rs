//! CDC cursor strategy and its failure modes. `DESIGN.md` §4.
//!
//! Change Data Capture is how the replica stays current cheaply: ask Intuit what
//! changed since a timestamp and get back the changed entities, deletes
//! included. It is also where incremental sync quietly breaks, so each failure
//! mode here has an explicit, tested response rather than a runtime judgement
//! call.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{EntityType, RealmId};

/// CDC returns at most this many objects in one response.
pub const CDC_RESPONSE_CAP: usize = 1000;

/// Documented CDC lookback window. Verified as 30 days (`DESIGN.md` §0).
pub const CDC_LOOKBACK_DAYS: i64 = 30;

/// How stale a cursor may get before CDC is abandoned for a full sweep.
///
/// Five days of margin under the documented 30. The margin matters because
/// exceeding the window is not an error — CDC returns what it can, which is the
/// dangerous shape of failure: silent incompleteness.
pub const DEFAULT_CDC_MAX_AGE_DAYS: i64 = 25;

// The safety margin is the whole point of the constant, so enforce it at compile
// time rather than trusting a future edit to keep it.
const _: () = assert!(DEFAULT_CDC_MAX_AGE_DAYS < CDC_LOOKBACK_DAYS);

/// Per realm, per entity type. Persisted in the same transaction that writes the
/// entities it covers — a cursor advanced outside that transaction can skip
/// changes after a crash.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SyncCursor {
    pub realm_id: RealmId,
    pub entity_type: EntityType,
    /// The `changedSince` value for the next poll. `None` before the first sync.
    pub last_cdc_cursor: Option<DateTime<Utc>>,
    pub last_full_sweep: Option<DateTime<Utc>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SweepReason {
    /// Never synced — the initial pull.
    NoCursor,
    /// Cursor older than the safe age; CDC would silently miss changes.
    CursorTooOld { age_days: i64 },
    /// CDC coverage for this entity type is unconfirmed (`DESIGN.md` §0), so the
    /// sweep path is the correctness baseline rather than a fallback.
    CoverageUnconfirmed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SyncStrategy {
    /// Incremental poll from this timestamp.
    Cdc { changed_since: DateTime<Utc> },
    /// Full comparison sweep.
    FullSweep { reason: SweepReason },
}

/// Choose between an incremental poll and a full sweep.
///
/// This is the "laptop closed for a month" decision, and it must not be a
/// judgement call at runtime — past the lookback window a CDC call does not
/// fail, it silently under-reports.
pub fn plan_sync(
    cursor: &SyncCursor,
    now: DateTime<Utc>,
    max_age: Duration,
    coverage_confirmed: bool,
) -> SyncStrategy {
    if !coverage_confirmed {
        return SyncStrategy::FullSweep {
            reason: SweepReason::CoverageUnconfirmed,
        };
    }

    let Some(changed_since) = cursor.last_cdc_cursor else {
        return SyncStrategy::FullSweep {
            reason: SweepReason::NoCursor,
        };
    };

    let age = now - changed_since;
    if age >= max_age {
        return SyncStrategy::FullSweep {
            reason: SweepReason::CursorTooOld {
                age_days: age.num_days(),
            },
        };
    }

    SyncStrategy::Cdc { changed_since }
}

/// What to do with a CDC response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CdcOutcome {
    /// Under the cap, so the window is fully covered. Advance the cursor.
    Complete,
    /// At the cap, so the response may be truncated. Narrow the window and
    /// re-poll; do **not** advance the cursor.
    PossiblyTruncated,
}

/// A response at exactly the cap must be assumed truncated.
///
/// Advancing the cursor to the newest returned record would silently drop
/// everything the response did not include, which is unrecoverable without a
/// full sweep nobody knows to run.
pub fn classify_response(returned: usize) -> CdcOutcome {
    if returned >= CDC_RESPONSE_CAP {
        CdcOutcome::PossiblyTruncated
    } else {
        CdcOutcome::Complete
    }
}

/// Halve a polling window towards its start, for re-polling after truncation.
///
/// Returns `None` once the window cannot be meaningfully narrowed further — at
/// that point more than [`CDC_RESPONSE_CAP`] entities changed within a second
/// and the caller must fall back to a full sweep rather than loop.
pub fn halve_window(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let span = to - from;
    if span <= Duration::seconds(1) {
        return None;
    }
    Some((from, from + span / 2))
}

impl SyncCursor {
    pub fn new(realm_id: RealmId, entity_type: EntityType) -> Self {
        SyncCursor {
            realm_id,
            entity_type,
            last_cdc_cursor: None,
            last_full_sweep: None,
        }
    }

    /// Advance only on a complete response. Takes the outcome rather than a
    /// bare timestamp so that advancing on a truncated response is not
    /// expressible at the call site.
    pub fn advance(&mut self, outcome: &CdcOutcome, up_to: DateTime<Utc>) {
        if *outcome == CdcOutcome::Complete {
            self.last_cdc_cursor = Some(up_to);
        }
    }

    pub fn record_full_sweep(&mut self, at: DateTime<Utc>) {
        self.last_full_sweep = Some(at);
        // A sweep is authoritative for everything up to its start, so it also
        // re-bases the incremental cursor.
        self.last_cdc_cursor = Some(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(days: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + days * 24 * 3600, 0).unwrap()
    }

    fn cursor(last: Option<DateTime<Utc>>) -> SyncCursor {
        SyncCursor {
            realm_id: realm(),
            entity_type: EntityType::Invoice,
            last_cdc_cursor: last,
            last_full_sweep: None,
        }
    }

    fn max_age() -> Duration {
        Duration::days(DEFAULT_CDC_MAX_AGE_DAYS)
    }

    #[test]
    fn a_first_sync_is_a_full_sweep() {
        let strategy = plan_sync(&cursor(None), at(0), max_age(), true);
        assert_eq!(
            strategy,
            SyncStrategy::FullSweep {
                reason: SweepReason::NoCursor
            }
        );
    }

    #[test]
    fn a_fresh_cursor_polls_incrementally() {
        let strategy = plan_sync(&cursor(Some(at(0))), at(1), max_age(), true);
        assert_eq!(strategy, SyncStrategy::Cdc { changed_since: at(0) });
    }

    #[test]
    fn a_stale_cursor_sweeps_instead_of_polling() {
        // The laptop-closed-for-a-month case. 26 days is inside CDC's documented
        // 30-day window but outside our 25-day safety margin, and that margin is
        // the point: CDC does not error past the window, it under-reports.
        let strategy = plan_sync(&cursor(Some(at(0))), at(26), max_age(), true);
        assert_eq!(
            strategy,
            SyncStrategy::FullSweep {
                reason: SweepReason::CursorTooOld { age_days: 26 }
            }
        );
    }

    #[test]
    fn the_safety_margin_sits_below_the_documented_window() {
        // The margin itself is asserted at compile time above; this covers the
        // behaviour that depends on it — a cursor between the two must sweep,
        // never poll.
        for day in DEFAULT_CDC_MAX_AGE_DAYS..=CDC_LOOKBACK_DAYS {
            let strategy = plan_sync(&cursor(Some(at(0))), at(day), max_age(), true);
            assert!(
                matches!(strategy, SyncStrategy::FullSweep { .. }),
                "day {day} should sweep"
            );
        }
    }

    #[test]
    fn exactly_at_the_limit_sweeps() {
        // Boundary chosen deliberately: at the limit, prefer the slower correct
        // path over the faster possibly-incomplete one.
        let strategy = plan_sync(&cursor(Some(at(0))), at(25), max_age(), true);
        assert!(matches!(strategy, SyncStrategy::FullSweep { .. }));
    }

    #[test]
    fn unconfirmed_coverage_always_sweeps() {
        // CDC's entity exclusion list is unverified, so correctness rests on the
        // sweep path and CDC is only ever a latency optimisation.
        let strategy = plan_sync(&cursor(Some(at(0))), at(1), max_age(), false);
        assert_eq!(
            strategy,
            SyncStrategy::FullSweep {
                reason: SweepReason::CoverageUnconfirmed
            }
        );
    }

    #[test]
    fn a_response_under_the_cap_is_complete() {
        assert_eq!(classify_response(0), CdcOutcome::Complete);
        assert_eq!(classify_response(999), CdcOutcome::Complete);
    }

    #[test]
    fn a_response_at_the_cap_is_treated_as_truncated() {
        assert_eq!(classify_response(CDC_RESPONSE_CAP), CdcOutcome::PossiblyTruncated);
        assert_eq!(classify_response(CDC_RESPONSE_CAP + 1), CdcOutcome::PossiblyTruncated);
    }

    #[test]
    fn the_cursor_does_not_advance_on_a_truncated_response() {
        // The invariant that stops silent data loss.
        let mut cursor = cursor(Some(at(0)));
        cursor.advance(&CdcOutcome::PossiblyTruncated, at(5));
        assert_eq!(cursor.last_cdc_cursor, Some(at(0)), "cursor moved on a truncated response");

        cursor.advance(&CdcOutcome::Complete, at(5));
        assert_eq!(cursor.last_cdc_cursor, Some(at(5)));
    }

    #[test]
    fn halving_narrows_towards_the_window_start() {
        let (from, to) = halve_window(at(0), at(10)).unwrap();
        assert_eq!(from, at(0));
        assert_eq!(to, at(5));
    }

    #[test]
    fn halving_bottoms_out_rather_than_looping_forever() {
        // More than 1000 entities changed inside one second: narrowing cannot
        // help, so the caller must sweep instead of spinning.
        let from = at(0);
        assert_eq!(halve_window(from, from + Duration::seconds(1)), None);
        assert_eq!(halve_window(from, from), None);
    }

    #[test]
    fn repeated_halving_terminates() {
        let (mut from, mut to) = (at(0), at(30));
        let mut iterations = 0;
        while let Some((next_from, next_to)) = halve_window(from, to) {
            from = next_from;
            to = next_to;
            iterations += 1;
            assert!(iterations < 64, "halving failed to converge");
        }
        assert!(iterations > 0);
    }

    #[test]
    fn a_full_sweep_rebases_the_incremental_cursor() {
        let mut cursor = cursor(None);
        cursor.record_full_sweep(at(3));
        assert_eq!(cursor.last_full_sweep, Some(at(3)));
        assert_eq!(cursor.last_cdc_cursor, Some(at(3)));
        // And the next plan polls rather than sweeping again.
        assert_eq!(
            plan_sync(&cursor, at(4), max_age(), true),
            SyncStrategy::Cdc { changed_since: at(3) }
        );
    }

    proptest! {
        /// Whenever a poll is chosen, the cursor is genuinely inside the safe
        /// window. This is the property that guarantees no silently-incomplete
        /// sync, independent of the specific boundaries tested above.
        #[test]
        fn polling_implies_a_cursor_inside_the_safe_window(age_hours in 0i64..(40 * 24)) {
            let now = at(40);
            let changed_since = now - Duration::hours(age_hours);
            let strategy = plan_sync(&cursor(Some(changed_since)), now, max_age(), true);
            if let SyncStrategy::Cdc { changed_since: used } = strategy {
                prop_assert!(now - used < max_age());
                prop_assert_eq!(used, changed_since);
            }
        }

        /// Halving always produces a strictly smaller window that still starts
        /// where the original did, so no part of the range is skipped.
        #[test]
        fn halving_never_skips_the_start(span_secs in 2i64..1_000_000) {
            let from = at(0);
            let to = from + Duration::seconds(span_secs);
            let (next_from, next_to) = halve_window(from, to).unwrap();
            prop_assert_eq!(next_from, from);
            prop_assert!(next_to > from);
            prop_assert!(next_to < to);
        }
    }
}
