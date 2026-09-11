//! Nightly replica snapshots end to end, against real files on disk.
//! `DESIGN.md` §8.
//!
//! `store/backup.rs` already has white-box unit tests for the same rules;
//! these are the black-box versions, reached only through `qbo_local`'s
//! public surface — the same bar `tests/replica.rs` holds the rest of the
//! store to.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use chrono::{DateTime, TimeZone, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::backup::SnapshotPolicy;
use qbo_local::store::{MirroredEntity, Store};
use rusqlite::Connection;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, 7, 0, 0).unwrap()
}

fn entity(id: &str) -> MirroredEntity {
    MirroredEntity {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: "0".into(),
        last_updated_utc: at(1),
        is_deleted: false,
        raw_json: serde_json::json!({ "DocNumber": id, "TotalAmt": 100.0 }),
    }
}

// ---------------------------------------------------------------------------
// (a) a snapshot opens and reports the same counts as its source
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_opens_with_store_open_and_matches_the_source_counts() {
    let source_dir = tempfile::tempdir().unwrap();
    let store = Store::open(source_dir.path().join("replica.db")).unwrap();
    store
        .register_realm(&realm(), "Test Company", at(1))
        .unwrap();
    for id in ["1", "2", "3"] {
        store.upsert_entity(&realm(), &entity(id), at(1)).unwrap();
    }

    let snapshot_dir = tempfile::tempdir().unwrap();
    let snapshot_path = snapshot_dir.path().join("replica-copy.db");
    store.snapshot_to(&snapshot_path).unwrap();

    // The copy is a real, independently openable replica — not a file that
    // merely exists, but one `Store::open` accepts and migrates correctly.
    let copy = Store::open(&snapshot_path).unwrap();
    assert_eq!(
        copy.schema_version().unwrap(),
        store.schema_version().unwrap()
    );
    assert_eq!(
        copy.count_entities(&realm(), EntityType::Invoice).unwrap(),
        store.count_entities(&realm(), EntityType::Invoice).unwrap(),
    );
    assert_eq!(
        copy.count_entities(&realm(), EntityType::Invoice).unwrap(),
        3
    );
}

// ---------------------------------------------------------------------------
// (b) the outbox rides along inside every snapshot
// ---------------------------------------------------------------------------

#[test]
fn an_outbox_row_written_before_the_snapshot_is_present_in_the_copy() {
    // `Store` has no public method that inserts an outbox row yet — nothing in
    // this crate does, `worker.rs` drains rows it is handed rather than
    // creating them — so this reaches the table the only way anything outside
    // `store` currently could: a second connection onto the same file, the
    // same path a future `enqueue` would use.
    let source_dir = tempfile::tempdir().unwrap();
    let path = source_dir.path().join("replica.db");
    let store = Store::open(&path).unwrap();
    store
        .register_realm(&realm(), "Test Company", at(1))
        .unwrap();

    {
        let raw = Connection::open(&path).unwrap();
        raw.execute(
            "INSERT INTO outbox
                 (id, realm_id, entity_type, operation, payload_json,
                  local_entity_id, request_id, state, created_at, updated_at)
             VALUES ('11111111-1111-7111-8111-111111111111', ?1, 'Invoice', 'Create',
                     '{\"TotalAmt\": 100}', 'local:1',
                     '22222222-2222-7222-8222-222222222222', 'pending', ?2, ?2)",
            rusqlite::params![realm().as_str(), at(1).to_rfc3339()],
        )
        .unwrap();
    }

    let snapshot_dir = tempfile::tempdir().unwrap();
    let snapshot_path = snapshot_dir.path().join("replica-copy.db");
    store.snapshot_to(&snapshot_path).unwrap();

    let copy = Connection::open(&snapshot_path).unwrap();
    let (state, payload): (String, String) = copy
        .query_row(
            "SELECT state, payload_json FROM outbox WHERE realm_id = ?1 AND local_entity_id = 'local:1'",
            [realm().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(payload, "{\"TotalAmt\": 100}");
}

// ---------------------------------------------------------------------------
// (c) rotation
// ---------------------------------------------------------------------------

#[test]
fn rotation_keeps_exactly_the_newest_keep_snapshots_and_names_the_rest_as_pruned() {
    let source_dir = tempfile::tempdir().unwrap();
    let store = Store::open(source_dir.path().join("replica.db")).unwrap();
    store
        .register_realm(&realm(), "Test Company", at(1))
        .unwrap();

    let snapshot_dir = tempfile::tempdir().unwrap();
    let policy = SnapshotPolicy {
        directory: snapshot_dir.path().to_path_buf(),
        keep: 3,
    };

    let mut taken = Vec::new();
    for day in 1..=6 {
        taken.push(store.take_snapshot(&policy, at(day)).unwrap());
    }

    // Every report after the third names exactly the one snapshot that fell
    // out of the window, oldest first.
    for report in &taken[..3] {
        assert!(report.pruned.is_empty());
    }
    for (index, report) in taken[3..].iter().enumerate() {
        assert_eq!(report.pruned, vec![taken[index].path.clone()]);
    }

    let files: Vec<_> = std::fs::read_dir(&policy.directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        files.len(),
        3,
        "only the newest `keep` snapshots should remain on disk"
    );
    for report in &taken[3..] {
        assert!(files.contains(&report.path));
    }
    for report in &taken[..3] {
        assert!(
            !files.contains(&report.path),
            "pruned snapshot should no longer exist"
        );
    }
}

// ---------------------------------------------------------------------------
// (d) keep = 0
// ---------------------------------------------------------------------------

#[test]
fn keep_zero_is_floored_to_one_rather_than_deleting_every_snapshot() {
    // Documented choice (see `store/backup.rs`): `keep = 0` behaves as
    // `keep = 1`. The alternative — rejecting the policy outright — would
    // just move this same decision to every caller that builds one from a
    // user-editable config value.
    let source_dir = tempfile::tempdir().unwrap();
    let store = Store::open(source_dir.path().join("replica.db")).unwrap();
    store
        .register_realm(&realm(), "Test Company", at(1))
        .unwrap();

    let snapshot_dir = tempfile::tempdir().unwrap();
    let policy = SnapshotPolicy {
        directory: snapshot_dir.path().to_path_buf(),
        keep: 0,
    };

    let first = store.take_snapshot(&policy, at(1)).unwrap();
    assert!(
        first.path.exists(),
        "the snapshot just taken must survive its own rotation"
    );

    let second = store.take_snapshot(&policy, at(2)).unwrap();
    assert_eq!(second.pruned, vec![first.path.clone()]);
    assert!(!first.path.exists());
    assert!(second.path.exists());
}
