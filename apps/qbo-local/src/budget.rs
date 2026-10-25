//! A hard ceiling on Intuit API calls per day.
//!
//! [`crate::ratelimit`] keeps qbo-local under Intuit's per-minute throttle.
//! It does nothing about the monthly quota an Intuit app gets (500,000
//! CorePlus calls on the builder tier), and a daemon polling every entity
//! type every fifteen seconds spent that in four days in October 2026. This
//! is the second line: every HTTP request to the API counts against a daily
//! figure, persisted so a restart does not reset it, and once it is spent
//! the client refuses locally until the next UTC day.
//!
//! The ledger is re-read before each call, so two processes sharing one file
//! (a daemon and a hand-run sweep) see each other's spending. The read and
//! the write are not locked, so two calls landing in the same instant can
//! both count from the same figure; that loses a call or two from the count,
//! not the ceiling.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::atomic_write;

/// The ceiling a live config gets unless it says otherwise. At thirty days a
/// month it keeps one app under the builder tier's 500,000 CorePlus calls.
pub const DEFAULT_DAILY_CALLS: u32 = 15_000;

/// Warn once a day when spending crosses this share of the ceiling.
const WARN_AT_PERCENT: u32 = 80;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ledger {
    /// The UTC day `calls` belongs to.
    day: NaiveDate,
    calls: u32,
}

/// The day's ceiling has been reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetExhausted {
    pub day: NaiveDate,
    pub used: u32,
    pub limit: u32,
}

impl BudgetExhausted {
    /// Midnight UTC after [`Self::day`], when calls may resume.
    pub fn resets_at(&self) -> DateTime<Utc> {
        (self.day + Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc()
    }
}

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "daily Intuit API call budget spent: {}/{} calls on {} (UTC); no calls until {}",
            self.used,
            self.limit,
            self.day,
            self.resets_at().to_rfc3339()
        )
    }
}

pub struct CallBudget {
    /// `None` counts in memory only — tests and the fake server.
    path: Option<PathBuf>,
    limit: u32,
    in_memory: Option<Ledger>,
    warned: Option<NaiveDate>,
    announced: Option<NaiveDate>,
}

impl CallBudget {
    /// Count against `limit` per UTC day in the ledger file at `path`.
    pub fn persistent(path: impl Into<PathBuf>, limit: u32) -> Self {
        CallBudget {
            path: Some(path.into()),
            limit,
            in_memory: None,
            warned: None,
            announced: None,
        }
    }

    /// Count against `limit` in memory, forgotten when the process exits.
    pub fn in_memory(limit: u32) -> Self {
        CallBudget {
            path: None,
            limit,
            in_memory: None,
            warned: None,
            announced: None,
        }
    }

    /// No ceiling. What [`crate::http::HttpQboClient::new`] starts with, so
    /// tests against the fake server need not set one; every live path sets
    /// a real one from the config.
    pub fn unlimited() -> Self {
        Self::in_memory(u32::MAX)
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Calls already spent on `now`'s UTC day.
    pub fn used(&self, now: DateTime<Utc>) -> Result<u32, std::io::Error> {
        Ok(self.load(now.date_naive())?.calls)
    }

    /// Spend one call, or refuse if the day's ceiling is reached. Refusal is
    /// logged once per day on stderr, which is the daemon's log.
    ///
    /// A ledger file that exists but cannot be read refuses too: spending
    /// blind is exactly what this exists to prevent, and the error names the
    /// file to fix.
    pub fn spend(&mut self, now: DateTime<Utc>) -> Result<u32, SpendError> {
        let today = now.date_naive();
        let mut ledger = self.load(today).map_err(SpendError::Ledger)?;
        if ledger.calls >= self.limit {
            let exhausted = BudgetExhausted {
                day: today,
                used: ledger.calls,
                limit: self.limit,
            };
            if self.announced != Some(today) {
                eprintln!("{} qbo-local: STOP — {exhausted}", now.to_rfc3339());
                self.announced = Some(today);
            }
            return Err(SpendError::Exhausted(exhausted));
        }
        ledger.calls += 1;
        self.store(&ledger).map_err(SpendError::Ledger)?;

        let warn_at = u64::from(self.limit) * u64::from(WARN_AT_PERCENT) / 100;
        if u64::from(ledger.calls) >= warn_at && self.warned != Some(today) {
            eprintln!(
                "{} qbo-local: {}/{} Intuit API calls used today (UTC); stops at {}",
                now.to_rfc3339(),
                ledger.calls,
                self.limit,
                self.limit
            );
            self.warned = Some(today);
        }
        Ok(ledger.calls)
    }

    fn load(&self, today: NaiveDate) -> Result<Ledger, std::io::Error> {
        let stored = match &self.path {
            None => self.in_memory.clone(),
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => Some(serde_json::from_slice::<Ledger>(&bytes).map_err(|error| {
                    std::io::Error::other(format!("{}: {error}", path.display()))
                })?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            },
        };
        Ok(stored
            .filter(|ledger| ledger.day == today)
            .unwrap_or(Ledger {
                day: today,
                calls: 0,
            }))
    }

    fn store(&mut self, ledger: &Ledger) -> Result<(), std::io::Error> {
        match &self.path {
            None => {
                self.in_memory = Some(ledger.clone());
                Ok(())
            }
            Some(path) => {
                let bytes = serde_json::to_vec(ledger).map_err(std::io::Error::other)?;
                atomic_write(path, &bytes)
            }
        }
    }
}

#[derive(Debug)]
pub enum SpendError {
    Exhausted(BudgetExhausted),
    Ledger(std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn refuses_once_the_days_ceiling_is_reached() {
        let mut budget = CallBudget::in_memory(3);
        for expected in 1..=3 {
            assert_eq!(budget.spend(at(23, 9)).unwrap(), expected);
        }
        match budget.spend(at(23, 10)) {
            Err(SpendError::Exhausted(exhausted)) => {
                assert_eq!(exhausted.used, 3);
                assert_eq!(exhausted.resets_at(), at(24, 0));
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn a_new_utc_day_starts_from_zero() {
        let mut budget = CallBudget::in_memory(1);
        budget.spend(at(23, 23)).unwrap();
        assert!(budget.spend(at(23, 23)).is_err());
        assert_eq!(budget.spend(at(24, 0)).unwrap(), 1);
    }

    #[test]
    fn the_count_survives_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("call-budget.json");
        {
            let mut budget = CallBudget::persistent(&path, 2);
            budget.spend(at(23, 9)).unwrap();
            budget.spend(at(23, 9)).unwrap();
        }
        let mut restarted = CallBudget::persistent(&path, 2);
        assert_eq!(restarted.used(at(23, 12)).unwrap(), 2);
        assert!(matches!(
            restarted.spend(at(23, 12)),
            Err(SpendError::Exhausted(_))
        ));
    }

    #[test]
    fn two_processes_on_one_ledger_share_the_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("call-budget.json");
        let mut daemon = CallBudget::persistent(&path, 2);
        let mut sweep = CallBudget::persistent(&path, 2);
        daemon.spend(at(23, 9)).unwrap();
        sweep.spend(at(23, 9)).unwrap();
        assert!(daemon.spend(at(23, 9)).is_err());
    }

    #[test]
    fn an_unreadable_ledger_refuses_rather_than_spending_blind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("call-budget.json");
        std::fs::write(&path, b"not json").unwrap();
        let mut budget = CallBudget::persistent(&path, 10);
        assert!(matches!(
            budget.spend(at(23, 9)),
            Err(SpendError::Ledger(_))
        ));
    }
}
