//! Nightly replica snapshots, with rotation. `DESIGN.md` §8.
//!
//! A snapshot is taken through SQLite's online backup API rather than a plain
//! file copy, because the replica runs in WAL mode: copying `replica.db`
//! itself while a write is in flight can produce a file that is missing pages
//! the WAL has not yet checked back into it. The backup API reads through
//! SQLite's own page cache instead, so the copy it produces is exactly what a
//! reader would have seen at the moment the backup started, even against a
//! live database.
//!
//! The outbox lives in the same file as everything else this backs up — it is
//! not a separate table space, just rows in `outbox` — so it is included in
//! every snapshot by construction, with nothing here that could omit it.
//! `DESIGN.md` §8 is explicit about why that matters: losing the replica is
//! not a business event, because it is rebuildable from QBO. Losing the outbox
//! is, because it may hold writes QBO has never seen.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::backup::Backup;
use rusqlite::Connection;

use super::{Store, StoreError};

const SNAPSHOT_PREFIX: &str = "replica-";
const SNAPSHOT_SUFFIX: &str = ".db";
/// `YYYYMMDDTHHMMSSZ`, chosen so lexical order and chronological order agree —
/// rotation can sort filenames as strings and get the right answer.
const SNAPSHOT_TIMESTAMP_FORMAT: &str = "%Y%m%dT%H%M%SZ";

/// Where nightly snapshots live and how many to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotPolicy {
    pub directory: PathBuf,
    pub keep: usize,
}

/// What one snapshot did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotReport {
    pub path: PathBuf,
    /// Older snapshots removed to stay within `policy.keep`, oldest first.
    pub pruned: Vec<PathBuf>,
}

impl Store {
    /// Copy the live replica to `path` through SQLite's online backup API.
    ///
    /// Consistent against a database that is being written to concurrently —
    /// see the module doc for why that rules out a plain file copy. The
    /// destination is created (or overwritten) at `path`; its parent directory
    /// is created if it does not exist yet.
    pub fn snapshot_to(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut destination = Connection::open(path)?;
        let backup = Backup::new(&self.connection, &mut destination)?;
        // No pause between steps and no progress callback: a nightly snapshot
        // is not latency-sensitive, and there is no competing writer here that
        // the pause exists to make room for.
        backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        Ok(())
    }

    /// Take a dated snapshot under `policy.directory`, then prune anything
    /// older than the newest `policy.keep` — the N-deep rotation `DESIGN.md`
    /// §8 asks for.
    ///
    /// `keep = 0` is treated as `keep = 1`: rotation exists to bound disk use,
    /// not to make "no snapshots ever survive" expressible, and a policy that
    /// deletes the backup it just took defeats the point of taking one.
    pub fn take_snapshot(
        &self,
        policy: &SnapshotPolicy,
        now: DateTime<Utc>,
    ) -> Result<SnapshotReport, StoreError> {
        let keep = policy.keep.max(1);
        let file_name = format!(
            "{SNAPSHOT_PREFIX}{}{SNAPSHOT_SUFFIX}",
            now.format(SNAPSHOT_TIMESTAMP_FORMAT)
        );
        let path = policy.directory.join(file_name);
        self.snapshot_to(&path)?;

        let mut existing = list_snapshots(&policy.directory)?;
        // The timestamp format sorts lexically the same as chronologically, so
        // a plain string sort of the filenames is a correct age ordering.
        existing.sort();

        let mut pruned = Vec::new();
        if existing.len() > keep {
            for old in &existing[..existing.len() - keep] {
                fs::remove_file(old)?;
                pruned.push(old.clone());
            }
        }

        Ok(SnapshotReport { path, pruned })
    }
}

/// Every file in `directory` that looks like a snapshot this module wrote,
/// full path, unsorted.
fn list_snapshots(directory: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let is_snapshot = entry.file_name().to_str().is_some_and(|name| {
            name.starts_with(SNAPSHOT_PREFIX) && name.ends_with(SNAPSHOT_SUFFIX)
        });
        if is_snapshot {
            found.push(entry.path());
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EntityType, RealmId};
    use crate::store::MirroredEntity;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + seconds, 0).unwrap()
    }

    fn entity(id: &str) -> MirroredEntity {
        MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: id.to_string(),
            sync_token: "0".into(),
            last_updated_utc: now(0),
            is_deleted: false,
            raw_json: serde_json::json!({ "DocNumber": id }),
        }
    }

    #[test]
    fn a_snapshot_of_a_file_store_opens_and_matches_its_source() {
        let source_dir = tempfile::tempdir().unwrap();
        let store = Store::open(source_dir.path().join("replica.db")).unwrap();
        store
            .register_realm(&realm(), "Test Company", now(0))
            .unwrap();
        store.upsert_entity(&realm(), &entity("1"), now(0)).unwrap();
        store.upsert_entity(&realm(), &entity("2"), now(0)).unwrap();

        let snapshot_dir = tempfile::tempdir().unwrap();
        let snapshot_path = snapshot_dir.path().join("copy.db");
        store.snapshot_to(&snapshot_path).unwrap();

        let copy = Store::open(&snapshot_path).unwrap();
        assert_eq!(
            copy.count_entities(&realm(), EntityType::Invoice).unwrap(),
            store.count_entities(&realm(), EntityType::Invoice).unwrap(),
        );
        assert_eq!(
            copy.count_entities(&realm(), EntityType::Invoice).unwrap(),
            2
        );
    }

    #[test]
    fn the_outbox_is_inside_the_snapshot_by_construction() {
        // The property DESIGN.md §8 cares about: the outbox is not special-
        // cased into (or out of) a snapshot, because it is just a table in the
        // same file everything else here backs up.
        let source_dir = tempfile::tempdir().unwrap();
        let path = source_dir.path().join("replica.db");
        let store = Store::open(&path).unwrap();
        store
            .register_realm(&realm(), "Test Company", now(0))
            .unwrap();

        // Store has no public API for writing an outbox row yet (nothing in
        // this crate does), so reach the table the same way a migration
        // would: a second connection onto the same file.
        {
            let raw = Connection::open(&path).unwrap();
            raw.execute(
                "INSERT INTO outbox
                     (id, realm_id, entity_type, operation, payload_json,
                      local_entity_id, request_id, state, created_at, updated_at)
                 VALUES ('11111111-1111-7111-8111-111111111111', ?1, 'Invoice', 'Create',
                         '{}', 'local:1', '22222222-2222-7222-8222-222222222222',
                         'pending', ?2, ?2)",
                rusqlite::params![realm().as_str(), now(0).to_rfc3339()],
            )
            .unwrap();
        }

        let snapshot_dir = tempfile::tempdir().unwrap();
        let snapshot_path = snapshot_dir.path().join("copy.db");
        store.snapshot_to(&snapshot_path).unwrap();

        let copy = Connection::open(&snapshot_path).unwrap();
        let outbox_rows: i64 = copy
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE realm_id = ?1",
                [realm().as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            outbox_rows, 1,
            "the outbox row should have travelled with the snapshot"
        );
    }

    #[test]
    fn rotation_keeps_exactly_the_newest_keep_snapshots() {
        let source_dir = tempfile::tempdir().unwrap();
        let store = Store::open(source_dir.path().join("replica.db")).unwrap();
        store
            .register_realm(&realm(), "Test Company", now(0))
            .unwrap();

        let snapshot_dir = tempfile::tempdir().unwrap();
        let policy = SnapshotPolicy {
            directory: snapshot_dir.path().to_path_buf(),
            keep: 2,
        };

        let mut reports = Vec::new();
        for day in 0..4 {
            reports.push(store.take_snapshot(&policy, now(day * 86_400)).unwrap());
        }

        // The first two snapshots are the ones that eventually get pruned —
        // one on the day it is bumped out, and the day after.
        assert!(reports[0].pruned.is_empty());
        assert!(reports[1].pruned.is_empty());
        assert_eq!(reports[2].pruned, vec![reports[0].path.clone()]);
        assert_eq!(reports[3].pruned, vec![reports[1].path.clone()]);

        let remaining = list_snapshots(&policy.directory).unwrap();
        assert_eq!(
            remaining.len(),
            2,
            "rotation should leave exactly `keep` files behind"
        );
        assert!(remaining.contains(&reports[2].path));
        assert!(remaining.contains(&reports[3].path));
    }

    #[test]
    fn keep_zero_is_treated_as_keep_one_rather_than_rejected() {
        // Documented choice: a policy that would delete the backup it just
        // took defeats the point of taking one, so `keep = 0` is floored to 1
        // instead of being refused outright.
        let source_dir = tempfile::tempdir().unwrap();
        let store = Store::open(source_dir.path().join("replica.db")).unwrap();
        store
            .register_realm(&realm(), "Test Company", now(0))
            .unwrap();

        let snapshot_dir = tempfile::tempdir().unwrap();
        let policy = SnapshotPolicy {
            directory: snapshot_dir.path().to_path_buf(),
            keep: 0,
        };

        let first = store.take_snapshot(&policy, now(0)).unwrap();
        assert!(first.pruned.is_empty());
        assert!(
            first.path.exists(),
            "keep = 0 must not delete the snapshot just taken"
        );

        let second = store.take_snapshot(&policy, now(86_400)).unwrap();
        assert_eq!(second.pruned, vec![first.path.clone()]);
        assert_eq!(list_snapshots(&policy.directory).unwrap().len(), 1);
    }
}
