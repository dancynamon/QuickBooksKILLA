//! A [`QboClient`] double that survives a process death. `DESIGN.md` §6.1, §10.
//!
//! [`MockQbo`] lives entirely in memory, which is exactly right for the rest
//! of the suite and exactly wrong for the chaos test: if "QBO" forgets
//! everything when the process calling it dies, a second process can never
//! tell whether the first one's write actually landed, and the exercise
//! proves nothing. [`JournalledMock`] fixes that by writing the mock's entire
//! state to disk after every call that mutates it, and reloading that file on
//! open — a second process sees what the first one really did to "QBO", not a
//! re-run from empty.
//!
//! It also appends every create/update attempt to `calls.jsonl`, echoing the
//! append-only write audit `DESIGN.md` §8 requires of the real client: enough
//! for `tests/chaos.rs` to prove a `RequestId` was never sent twice with two
//! different payloads, which is the failure mode that would turn a retry into
//! a corrupted write rather than a safe replay.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::client::{
    EntityPayload, IndexEntry, MockQbo, MockQboSnapshot, QboClient, QboError, UpdatedRange,
};
use crate::domain::{EntityType, RealmId};

/// One line of `calls.jsonl` — a durable record of what was actually sent to
/// "QBO", independent of whether the outbox record that caused it, or the
/// process that sent it, survives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallRecord {
    pub at: DateTime<Utc>,
    pub realm_id: String,
    pub entity_type: String,
    pub operation: String,
    pub request_id: Uuid,
    /// The id this call addressed. Always `None` for a create — obtaining one
    /// is the point of the call, so there is nothing to log yet. `Some` for an
    /// update, carrying exactly what the caller passed: a real QBO id once
    /// resolved, or still a `local:`-prefixed one if the drain worker had not
    /// resolved it at the time.
    pub target_id: Option<String>,
    pub payload: serde_json::Value,
    pub outcome: CallOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CallOutcome {
    Applied { qbo_id: String },
    Error { message: String },
}

/// A [`QboClient`] backed by [`MockQbo`], durable across process restarts.
///
/// [`Self::open`] reloads `<dir>/state.json` if present, so a second process
/// resumes with exactly the entities and `RequestId` log the first one left —
/// the property the chaos test depends on to tell a safe replay from a
/// duplicate. Every `create`/`update` call rewrites that file and appends to
/// `<dir>/calls.jsonl` before returning, so both are durable by the time the
/// caller — and, in the chaos test, a crash hook that may `abort()` the whole
/// process — sees the result.
pub struct JournalledMock {
    inner: MockQbo,
    dir: PathBuf,
}

impl JournalledMock {
    const STATE_FILE: &'static str = "state.json";
    const CALLS_FILE: &'static str = "calls.jsonl";

    /// Open the journal directory: reload the persisted mock state if this
    /// directory has seen a run before, or start a fresh double if this is
    /// the first run.
    pub fn open(dir: impl AsRef<Path>, now: DateTime<Utc>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let state_path = dir.join(Self::STATE_FILE);

        let inner = match fs::read_to_string(&state_path) {
            Ok(raw) => {
                let snapshot: MockQboSnapshot =
                    serde_json::from_str(&raw).unwrap_or_else(|error| {
                        panic!("corrupt journal state at {state_path:?}: {error}")
                    });
                MockQbo::restore(snapshot)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MockQbo::new(now),
            Err(error) => return Err(error),
        };

        let journal = JournalledMock { inner, dir };
        if !state_path.exists() {
            journal.persist_state()?;
        }
        Ok(journal)
    }

    /// Read-only accessors for the chaos test's final assertions.
    pub fn count(&self, realm: &RealmId, entity_type: EntityType) -> usize {
        self.inner.count(realm, entity_type)
    }

    pub fn get(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Option<&EntityPayload> {
        self.inner.get(realm, entity_type, qbo_id)
    }

    /// Every call ever recorded to `calls.jsonl`, oldest first — across every
    /// process that has opened this journal directory.
    pub fn read_calls(dir: impl AsRef<Path>) -> std::io::Result<Vec<CallRecord>> {
        let path = dir.as_ref().join(Self::CALLS_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("corrupt calls.jsonl line {line:?}: {error}"))
            })
            .collect())
    }

    fn persist_state(&self) -> std::io::Result<()> {
        let json = serde_json::to_string(&self.inner.snapshot())
            .expect("MockQboSnapshot always serialises");
        write_atomically(&self.dir.join(Self::STATE_FILE), json.as_bytes())
    }

    fn append_call(&self, record: &CallRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).expect("CallRecord always serialises");
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(Self::CALLS_FILE))?;
        file.write_all(line.as_bytes())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_call(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
        operation: &str,
        request_id: Uuid,
        target_id: Option<String>,
        payload: &serde_json::Value,
        outcome: &Result<EntityPayload, QboError>,
    ) {
        let call = CallRecord {
            at: Utc::now(),
            realm_id: realm.as_str().to_string(),
            entity_type: entity_type.as_str().to_string(),
            operation: operation.to_string(),
            request_id,
            target_id,
            payload: payload.clone(),
            outcome: match outcome {
                Ok(applied) => CallOutcome::Applied {
                    qbo_id: applied.qbo_id.clone(),
                },
                Err(error) => CallOutcome::Error {
                    message: error.to_string(),
                },
            },
        };
        // A journal write that fails is a bug in the test harness itself, not
        // a condition a chaos run should silently tolerate.
        self.append_call(&call).expect("append to calls.jsonl");
    }
}

/// Write-temp-then-rename, so a reader never observes a half-written file.
/// `DESIGN.md` §5.1 additionally `fsync`s both the file and its directory for
/// the OAuth token store, where surviving a *power loss* matters; this journal
/// only has to survive `tests/chaos.rs`'s `std::process::abort()`, which does
/// not touch the page cache, so the plain rename is enough.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

impl QboClient for JournalledMock {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        self.inner
            .query(realm, entity_type, updated, start_position, max_results)
    }

    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        self.inner.cdc(realm, entity_types, changed_since)
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        let outcome = self.inner.create(realm, entity_type, payload, request_id);
        self.persist_state()
            .expect("persist journal state after create");
        self.record_call(
            realm,
            entity_type,
            "create",
            request_id,
            None,
            payload,
            &outcome,
        );
        outcome
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
        let outcome = self.inner.update(
            realm,
            entity_type,
            qbo_id,
            payload,
            base_sync_token,
            request_id,
        );
        self.persist_state()
            .expect("persist journal state after update");
        self.record_call(
            realm,
            entity_type,
            "update",
            request_id,
            Some(qbo_id.to_string()),
            payload,
            &outcome,
        );
        outcome
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.inner.find_by_name(realm, entity_type, name)
    }

    fn index(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<IndexEntry>, QboError> {
        self.inner
            .index(realm, entity_type, start_position, max_results)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + secs, 0).unwrap()
    }

    #[test]
    fn a_second_open_sees_what_the_first_process_created() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut journal = JournalledMock::open(dir.path(), at(0)).unwrap();
            journal
                .create(
                    &realm(),
                    EntityType::Invoice,
                    &serde_json::json!({ "DocNumber": "1" }),
                    Uuid::now_v7(),
                )
                .unwrap();
        }

        let reopened = JournalledMock::open(dir.path(), at(1)).unwrap();
        assert_eq!(reopened.count(&realm(), EntityType::Invoice), 1);
    }

    #[test]
    fn every_create_and_update_lands_in_calls_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = JournalledMock::open(dir.path(), at(0)).unwrap();

        let request_id = Uuid::now_v7();
        let created = journal
            .create(
                &realm(),
                EntityType::Invoice,
                &serde_json::json!({ "DocNumber": "1" }),
                request_id,
            )
            .unwrap();
        journal
            .update(
                &realm(),
                EntityType::Invoice,
                &created.qbo_id,
                &serde_json::json!({ "DocNumber": "1", "TotalAmt": 5.0 }),
                &created.sync_token,
                Uuid::now_v7(),
            )
            .unwrap();

        let calls = JournalledMock::read_calls(dir.path()).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].operation, "create");
        assert_eq!(calls[0].request_id, request_id);
        assert!(matches!(calls[0].outcome, CallOutcome::Applied { .. }));
        assert_eq!(calls[1].operation, "update");
        assert_eq!(calls[1].target_id.as_deref(), Some(created.qbo_id.as_str()));
    }

    #[test]
    fn a_retry_after_reopening_still_replays_rather_than_duplicating() {
        // The whole point: RequestId reuse must survive a process boundary,
        // not just an in-memory retry.
        let dir = tempfile::tempdir().unwrap();
        let request_id = Uuid::now_v7();

        {
            let mut journal = JournalledMock::open(dir.path(), at(0)).unwrap();
            journal
                .create(
                    &realm(),
                    EntityType::Invoice,
                    &serde_json::json!({ "DocNumber": "1" }),
                    request_id,
                )
                .unwrap();
        }

        let mut reopened = JournalledMock::open(dir.path(), at(1)).unwrap();
        reopened
            .create(
                &realm(),
                EntityType::Invoice,
                &serde_json::json!({ "DocNumber": "1" }),
                request_id,
            )
            .unwrap();

        assert_eq!(
            reopened.count(&realm(), EntityType::Invoice),
            1,
            "duplicate invoice across a restart"
        );
    }
}
