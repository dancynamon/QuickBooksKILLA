//! The two safety nets `DESIGN.md` §8 requires before any write reaches QBO:
//! an append-only audit file of every write, and dated replica snapshots.
//!
//! Losing the replica is not a business event, since QBO rebuilds it. Losing
//! the outbox is, because it can hold writes QBO has never seen; snapshots
//! copy the whole database, so the outbox is in every one.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::client::{EntityPayload, IndexEntry, QboClient, QboError, ReportName, ReportParams, UpdatedRange};
use crate::domain::{EntityType, RealmId};
use crate::store::{Store, StoreError};

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// One line of the audit file. The request body is kept whole: this file is
/// the answer to "what exactly did we send QuickBooks", and a summary of the
/// request cannot answer it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: DateTime<Utc>,
    pub realm_id: RealmId,
    pub entity_type: EntityType,
    pub operation: String,
    pub request_id: Uuid,
    pub qbo_id: Option<String>,
    pub base_sync_token: Option<String>,
    pub request: serde_json::Value,
    pub outcome: String,
    pub response: Option<serde_json::Value>,
}

/// An append-only JSONL file. Each entry is written and fsynced before the
/// call that produced it returns, so a crash cannot lose the record of a
/// write QBO accepted.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        AuditLog { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(&line)?;
        file.sync_all()
    }

    pub fn read_all(&self) -> std::io::Result<Vec<AuditEntry>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(std::io::Error::other))
            .collect()
    }
}

/// A [`QboClient`] that records every write before returning its result.
///
/// Reads pass straight through: the audit file is about what changed the book
/// of record, and logging every sync page would bury the writes in noise.
/// If the audit line cannot be written, the call reports an error even though
/// QBO accepted it: the worker then retries with the same `RequestId`, which
/// replays rather than duplicates, and the retry is audited.
pub struct AuditedClient<C: QboClient> {
    inner: C,
    log: AuditLog,
    clock: Box<dyn FnMut() -> DateTime<Utc>>,
}

impl<C: QboClient> AuditedClient<C> {
    pub fn new(inner: C, log: AuditLog) -> Self {
        Self::with_clock(inner, log, Box::new(Utc::now))
    }

    pub fn with_clock(inner: C, log: AuditLog, clock: Box<dyn FnMut() -> DateTime<Utc>>) -> Self {
        AuditedClient { inner, log, clock }
    }

    pub fn inner(&self) -> &C {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut C {
        &mut self.inner
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        operation: &str,
        request_id: Uuid,
        qbo_id: Option<&str>,
        base_sync_token: Option<&str>,
        request: &serde_json::Value,
        result: &Result<EntityPayload, QboError>,
    ) -> Result<(), QboError> {
        let (outcome, response, resulting_id) = match result {
            Ok(payload) => ("applied".to_string(), Some(payload.raw_json.clone()), Some(payload.qbo_id.clone())),
            Err(error) => (format!("failed: {error}"), None, qbo_id.map(str::to_string)),
        };
        let entry = AuditEntry {
            at: (self.clock)(),
            realm_id: realm.clone(),
            entity_type,
            operation: operation.to_string(),
            request_id,
            qbo_id: resulting_id,
            base_sync_token: base_sync_token.map(str::to_string),
            request: request.clone(),
            outcome,
            response,
        };
        self.log
            .append(&entry)
            .map_err(|e| QboError::Network(format!("audit log unwritable, write not confirmed: {e}")))
    }
}

impl<C: QboClient> QboClient for AuditedClient<C> {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        self.inner.query(realm, entity_type, updated, start_position, max_results)
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
        let result = self.inner.create(realm, entity_type, payload, request_id);
        self.record(realm, entity_type, "create", request_id, None, None, payload, &result)?;
        result
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
        let result = self.inner.update(realm, entity_type, qbo_id, payload, base_sync_token, request_id);
        self.record(realm, entity_type, "update", request_id, Some(qbo_id), Some(base_sync_token), payload, &result)?;
        result
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
        self.inner.index(realm, entity_type, start_position, max_results)
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.inner.fetch(realm, entity_type, qbo_id)
    }

    fn report(
        &mut self,
        realm: &RealmId,
        name: ReportName,
        params: &ReportParams,
    ) -> Result<serde_json::Value, QboError> {
        self.inner.report(realm, name, params)
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

const SNAPSHOT_PREFIX: &str = "snapshot-";
const SNAPSHOT_SUFFIX: &str = ".db";

fn snapshot_name(date: NaiveDate) -> String {
    format!("{SNAPSHOT_PREFIX}{}{SNAPSHOT_SUFFIX}", date.format("%Y-%m-%d"))
}

/// The date of a file this module wrote, or `None` for anything else. Only a
/// file whose whole name parses is ever considered for rotation, so a file
/// someone else put in the directory is never touched.
fn snapshot_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let date = name.strip_prefix(SNAPSHOT_PREFIX)?.strip_suffix(SNAPSHOT_SUFFIX)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotReport {
    /// `None` when today's snapshot already existed.
    pub written: Option<PathBuf>,
    pub kept: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

/// Take today's snapshot if it is not already there, then keep the newest
/// `keep` snapshots and remove older ones this module wrote.
pub fn snapshot_and_rotate(
    store: &Store,
    directory: &Path,
    keep: usize,
    now: DateTime<Utc>,
) -> Result<SnapshotReport, StoreError> {
    let keep = keep.max(1);
    fs::create_dir_all(directory).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let today = directory.join(snapshot_name(now.date_naive()));
    let written = if today.exists() {
        None
    } else {
        store.vacuum_into(&today)?;
        Some(today)
    };

    let mut snapshots: Vec<(NaiveDate, PathBuf)> = fs::read_dir(directory)
        .map_err(|e| StoreError::Corrupt(e.to_string()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter_map(|path| snapshot_date(&path).map(|date| (date, path)))
        .collect();
    snapshots.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for (index, (_, path)) in snapshots.into_iter().enumerate() {
        if index < keep {
            kept.push(path);
        } else {
            fs::remove_file(&path).map_err(|e| StoreError::Corrupt(e.to_string()))?;
            removed.push(path);
        }
    }
    Ok(SnapshotReport { written, kept, removed })
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::client::MockQbo;
    use crate::demo::{self, DemoSize, DEMO_REALM};

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(day: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 3, 0, 0).unwrap() + Duration::days(day)
    }

    #[test]
    fn every_write_is_audited_with_its_request_id_and_result() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit-test.jsonl"));
        let mut client = AuditedClient::with_clock(MockQbo::new(at(0)), log, Box::new(|| at(0)));
        let request_id = Uuid::now_v7();
        let created = client
            .create(&realm(), EntityType::Invoice, &serde_json::json!({ "DocNumber": "T-1" }), request_id)
            .unwrap();
        client
            .update(&realm(), EntityType::Invoice, &created.qbo_id, &serde_json::json!({ "TotalAmt": 5 }), "0", Uuid::now_v7())
            .unwrap();

        let entries = AuditLog::new(dir.path().join("audit-test.jsonl")).read_all().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].operation, "create");
        assert_eq!(entries[0].request_id, request_id);
        assert_eq!(entries[0].qbo_id.as_deref(), Some(created.qbo_id.as_str()));
        assert_eq!(entries[0].request["DocNumber"], "T-1");
        assert_eq!(entries[1].base_sync_token.as_deref(), Some("0"));
    }

    #[test]
    fn a_refused_write_is_audited_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut qbo = MockQbo::new(at(0));
        qbo.fail_next(QboError::Validation("customer required".into()));
        let mut client = AuditedClient::new(qbo, AuditLog::new(dir.path().join("audit-test.jsonl")));
        assert!(client.create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7()).is_err());
        let entries = AuditLog::new(dir.path().join("audit-test.jsonl")).read_all().unwrap();
        assert!(entries[0].outcome.starts_with("failed: validation"));
        assert!(entries[0].response.is_none());
    }

    #[test]
    fn reads_are_not_audited() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = AuditedClient::new(MockQbo::new(at(0)), AuditLog::new(dir.path().join("audit-test.jsonl")));
        client.query(&realm(), EntityType::Invoice, None, 0, 10).unwrap();
        assert!(AuditLog::new(dir.path().join("audit-test.jsonl")).read_all().unwrap().is_empty());
    }

    #[test]
    fn an_unwritable_audit_log_fails_the_call_as_transient() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: every append fails.
        let path = dir.path().join("audit-test.jsonl");
        fs::create_dir(&path).unwrap();
        let mut client = AuditedClient::new(MockQbo::new(at(0)), AuditLog::new(path));
        let error = client
            .create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7())
            .unwrap_err();
        assert!(error.is_transient(), "the worker must retry, replaying the RequestId");
    }

    fn book() -> Store {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 5, vendors: 2, items: 5, invoices: 20, purchase_orders: 3 };
        demo::seed(&store, size, 1, at(0)).unwrap();
        store
    }

    #[test]
    fn a_snapshot_is_a_complete_openable_replica() {
        let dir = tempfile::tempdir().unwrap();
        let store = book();
        let report = snapshot_and_rotate(&store, dir.path(), 7, at(0)).unwrap();
        let path = report.written.unwrap();
        let copy = Store::open(&path).unwrap();
        let realm = RealmId::parse(DEMO_REALM).unwrap();
        assert_eq!(
            copy.count_entities(&realm, EntityType::Invoice).unwrap(),
            store.count_entities(&realm, EntityType::Invoice).unwrap()
        );
    }

    #[test]
    fn a_second_run_the_same_day_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = book();
        snapshot_and_rotate(&store, dir.path(), 7, at(0)).unwrap();
        assert!(snapshot_and_rotate(&store, dir.path(), 7, at(0)).unwrap().written.is_none());
    }

    #[test]
    fn rotation_keeps_the_newest_and_never_touches_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = book();
        fs::write(dir.path().join("notes.txt"), "mine").unwrap();
        fs::write(dir.path().join("snapshot-final.db"), "not a dated snapshot").unwrap();
        for day in 0..5 {
            snapshot_and_rotate(&store, dir.path(), 3, at(day)).unwrap();
        }
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["notes.txt", "snapshot-2026-10-03.db", "snapshot-2026-10-04.db", "snapshot-2026-10-05.db", "snapshot-final.db"]
        );
    }
}
