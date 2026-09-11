//! The reconciliation sweep's read side: the local id + timestamp index that
//! gets diffed against QBO's, orphaned `document_lines` rows, and quarantining
//! an entity that the sweep found no longer exists in QBO. `DESIGN.md` §7,
//! steps 1-4 (step 5, the trial-balance diff, is out of scope — see
//! `crate::reconcile`).
//!
//! Quarantine here never deletes. §7 step 4 is explicit that an entity absent
//! from QBO's index is copied into `quarantine_entities` and reported, and the
//! `entities` row it came from is left exactly as it was.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};

use super::{Store, StoreError};
use crate::domain::{EntityType, RealmId};

/// One row of the local id + `MetaData.LastUpdatedTime` index — the same shape
/// [`crate::client::IndexEntry`] returns from QBO, so the two sides diff
/// directly without either one carrying a raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIndexEntry {
    pub qbo_id: String,
    pub last_updated_utc: DateTime<Utc>,
    pub is_deleted: bool,
}

/// A `document_lines` row with no matching `documents` row.
///
/// Reported, never modified (`DESIGN.md` §7 step 2 / step 4) — an orphan is
/// evidence of a bug in how a document was written, not data to throw away.
/// In this schema `document_lines` carries a `FOREIGN KEY` to `documents` and
/// the connection runs with `foreign_keys=ON`, which should make this
/// permanently empty; the read exists anyway because a foreign key only holds
/// while it stays enabled, and the sweep's job is to not take that on faith.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanedLine {
    pub doc_qbo_id: String,
    pub line_no: i64,
}

impl Store {
    /// The mirror's own id + timestamp index for one entity type, for diffing
    /// against QBO's (`DESIGN.md` §7 steps 1-2).
    pub fn entity_index(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
    ) -> Result<Vec<LocalIndexEntry>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, last_updated_utc, is_deleted
             FROM entities
             WHERE realm_id = ?1 AND entity_type = ?2",
        )?;
        let rows = statement.query_map(params![realm.as_str(), entity_type.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut entries = Vec::new();
        for row in rows {
            let (qbo_id, last_updated, is_deleted) = row?;
            entries.push(LocalIndexEntry {
                qbo_id,
                last_updated_utc: DateTime::parse_from_rfc3339(&last_updated)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                is_deleted: is_deleted != 0,
            });
        }
        Ok(entries)
    }

    /// Rows in `document_lines` with no matching `documents` row, realm-scoped
    /// (`DESIGN.md` §7 step 2, the "orphaned" case).
    pub fn orphaned_lines(&self, realm: &RealmId) -> Result<Vec<OrphanedLine>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT dl.doc_qbo_id, dl.line_no
             FROM document_lines dl
             LEFT JOIN documents d
                    ON d.realm_id = dl.realm_id AND d.qbo_id = dl.doc_qbo_id
             WHERE dl.realm_id = ?1 AND d.qbo_id IS NULL",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok(OrphanedLine {
                doc_qbo_id: row.get(0)?,
                line_no: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Copy an already-mirrored entity's raw payload into quarantine with a
    /// reason, leaving its `entities` row untouched.
    ///
    /// This is the sweep's "extra" case (`DESIGN.md` §7 step 4): a record the
    /// replica holds that QBO's index no longer names. **Never auto-delete** —
    /// quarantine and report, so the record is still on disk if the QBO index
    /// was itself the thing that was wrong. Returns `false` if there is no such
    /// entity to quarantine.
    pub fn quarantine_existing(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let raw_json: Option<String> = self
            .connection
            .query_row(
                "SELECT raw_json FROM entities
                 WHERE realm_id = ?1 AND entity_type = ?2 AND qbo_id = ?3",
                params![realm.as_str(), entity_type.as_str(), qbo_id],
                |row| row.get(0),
            )
            .optional()?;

        let Some(raw_json) = raw_json else {
            return Ok(false);
        };

        self.connection.execute(
            "INSERT INTO quarantine_entities
                 (realm_id, entity_type, qbo_id, reason, raw_json, quarantined_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(realm_id, entity_type, qbo_id) DO UPDATE SET
                 reason = excluded.reason,
                 raw_json = excluded.raw_json,
                 quarantined_at = excluded.quarantined_at",
            params![
                realm.as_str(),
                entity_type.as_str(),
                qbo_id,
                reason,
                raw_json,
                now.to_rfc3339(),
            ],
        )?;
        Ok(true)
    }
}
