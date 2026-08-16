//! The SQLite replica: schema, migrations, and realm-scoped access.
//! `DESIGN.md` §3.
//!
//! Realm scoping is structural, not documented (§2.1). The [`rusqlite`]
//! connection is private to this module, so no code elsewhere can issue SQL;
//! every function below takes `&RealmId` as its first parameter, so there is no
//! accessor that omits it.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::domain::{EntityType, RealmId};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored value is not valid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("replica schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
}

/// A forward-only, numbered migration. Applied in a transaction, version
/// recorded in the DB. **Never edit one that has shipped** — add another.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial replica schema",
        sql: r#"
CREATE TABLE realms (
    realm_id          TEXT PRIMARY KEY,
    display_name      TEXT NOT NULL,
    -- Read-only until explicitly flipped, per realm (DESIGN.md §8).
    is_write_enabled  INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL
) STRICT;

-- The durable truth of the mirror. Parsed tables below are a projection of
-- this and can be dropped and rebuilt when a parser improves, with no Intuit
-- round-trip.
CREATE TABLE entities (
    realm_id          TEXT NOT NULL REFERENCES realms(realm_id),
    entity_type       TEXT NOT NULL,
    qbo_id            TEXT NOT NULL,
    sync_token        TEXT NOT NULL,
    last_updated_utc  TEXT NOT NULL,
    is_deleted        INTEGER NOT NULL DEFAULT 0,
    raw_json          TEXT NOT NULL,
    mirrored_at       TEXT NOT NULL,
    PRIMARY KEY (realm_id, entity_type, qbo_id)
) STRICT;

CREATE INDEX idx_entities_updated
    ON entities(realm_id, entity_type, last_updated_utc);

CREATE TABLE documents (
    realm_id       TEXT NOT NULL REFERENCES realms(realm_id),
    qbo_id         TEXT NOT NULL,
    doc_type       TEXT NOT NULL,
    doc_number     TEXT,
    txn_date       TEXT NOT NULL,
    contact_id     TEXT,
    class_id       TEXT,
    total_minor    INTEGER NOT NULL,
    balance_minor  INTEGER,
    is_deleted     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (realm_id, qbo_id)
) STRICT;

CREATE INDEX idx_documents_date ON documents(realm_id, doc_type, txn_date);
CREATE INDEX idx_documents_contact ON documents(realm_id, contact_id);
CREATE INDEX idx_documents_class ON documents(realm_id, class_id);

CREATE TABLE document_lines (
    realm_id      TEXT NOT NULL,
    doc_qbo_id    TEXT NOT NULL,
    line_no       INTEGER NOT NULL,
    item_id       TEXT,
    description   TEXT,
    -- Decimal as text, never REAL: a float column would reintroduce exactly
    -- the drift the money type exists to prevent.
    qty           TEXT,
    unit_price    TEXT,
    amount_minor  INTEGER NOT NULL,
    class_id      TEXT,
    is_taxable    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (realm_id, doc_qbo_id, line_no),
    FOREIGN KEY (realm_id, doc_qbo_id) REFERENCES documents(realm_id, qbo_id)
) STRICT;

CREATE TABLE sync_cursors (
    realm_id         TEXT NOT NULL REFERENCES realms(realm_id),
    entity_type      TEXT NOT NULL,
    last_cdc_cursor  TEXT,
    last_full_sweep  TEXT,
    PRIMARY KEY (realm_id, entity_type)
) STRICT;

CREATE TABLE outbox (
    id                TEXT PRIMARY KEY,
    realm_id          TEXT NOT NULL REFERENCES realms(realm_id),
    entity_type       TEXT NOT NULL,
    operation         TEXT NOT NULL,
    payload_json      TEXT NOT NULL,
    local_entity_id   TEXT NOT NULL,
    base_sync_token   TEXT,
    request_id        TEXT NOT NULL,
    state             TEXT NOT NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    depends_on        TEXT REFERENCES outbox(id),
    last_error        TEXT,
    qbo_response_json TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
) STRICT;

-- Drain order: per entity, oldest first.
CREATE INDEX idx_outbox_drain
    ON outbox(realm_id, entity_type, local_entity_id, id);
-- The failure inbox and the pending counter in the chrome (DESIGN.md §6.6).
CREATE INDEX idx_outbox_state ON outbox(realm_id, state);

-- Maps a local:-prefixed id to the id QBO assigned once the create applied,
-- so dependants can rewrite their references (DESIGN.md §6.4).
CREATE TABLE local_id_map (
    realm_id        TEXT NOT NULL REFERENCES realms(realm_id),
    local_entity_id TEXT NOT NULL,
    entity_type     TEXT NOT NULL,
    qbo_id          TEXT NOT NULL,
    mapped_at       TEXT NOT NULL,
    PRIMARY KEY (realm_id, local_entity_id)
) STRICT;

-- Reconciliation quarantines rather than deletes (DESIGN.md §7.4).
CREATE TABLE quarantine_entities (
    realm_id       TEXT NOT NULL REFERENCES realms(realm_id),
    entity_type    TEXT NOT NULL,
    qbo_id         TEXT NOT NULL,
    reason         TEXT NOT NULL,
    raw_json       TEXT NOT NULL,
    quarantined_at TEXT NOT NULL,
    PRIMARY KEY (realm_id, entity_type, qbo_id)
) STRICT;
"#,
    },
];

pub fn latest_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

/// The replica. Owns the connection; nothing outside this module can reach it.
pub struct Store {
    connection: Connection,
}

/// One mirrored entity as stored, raw payload included.
#[derive(Clone, Debug, PartialEq)]
pub struct MirroredEntity {
    pub entity_type: EntityType,
    pub qbo_id: String,
    pub sync_token: String,
    pub last_updated_utc: DateTime<Utc>,
    pub is_deleted: bool,
    pub raw_json: serde_json::Value,
}

impl Store {
    /// Open (creating if absent) and migrate to the latest schema version.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::configure(&connection)?;
        let mut store = Store { connection };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory, for tests. WAL does not apply to a memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        let mut store = Store { connection };
        store.migrate()?;
        Ok(store)
    }

    fn configure(connection: &Connection) -> Result<(), StoreError> {
        // journal_mode returns a row, so it cannot go through execute_batch.
        let _: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = FULL;",
        )?;
        Ok(())
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                 version    INTEGER PRIMARY KEY,
                 name       TEXT NOT NULL,
                 applied_at TEXT NOT NULL
             ) STRICT;",
        )?;

        let current: i64 = self
            .connection
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |row| {
                row.get(0)
            })?;

        if current > latest_version() {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported: latest_version(),
            });
        }

        for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
            // Each migration and its version stamp land in one transaction, so a
            // crash mid-migration cannot leave a half-applied schema recorded as
            // complete.
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(migration.sql)?;
            transaction.execute(
                "INSERT INTO schema_version (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![migration.version, migration.name, Utc::now().to_rfc3339()],
            )?;
            transaction.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn register_realm(
        &self,
        realm: &RealmId,
        display_name: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO realms (realm_id, display_name, is_write_enabled, created_at)
             VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(realm_id) DO UPDATE SET display_name = excluded.display_name",
            params![realm.as_str(), display_name, now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Writes stay off until explicitly enabled, per realm, one realm at a time.
    pub fn is_write_enabled(&self, realm: &RealmId) -> Result<bool, StoreError> {
        let enabled: Option<i64> = self
            .connection
            .query_row(
                "SELECT is_write_enabled FROM realms WHERE realm_id = ?1",
                params![realm.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(enabled.unwrap_or(0) != 0)
    }

    pub fn set_write_enabled(&self, realm: &RealmId, enabled: bool) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE realms SET is_write_enabled = ?2 WHERE realm_id = ?1",
            params![realm.as_str(), i64::from(enabled)],
        )?;
        Ok(())
    }

    pub fn upsert_entity(
        &self,
        realm: &RealmId,
        entity: &MirroredEntity,
        mirrored_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO entities
                 (realm_id, entity_type, qbo_id, sync_token, last_updated_utc,
                  is_deleted, raw_json, mirrored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(realm_id, entity_type, qbo_id) DO UPDATE SET
                 sync_token       = excluded.sync_token,
                 last_updated_utc = excluded.last_updated_utc,
                 is_deleted       = excluded.is_deleted,
                 raw_json         = excluded.raw_json,
                 mirrored_at      = excluded.mirrored_at",
            params![
                realm.as_str(),
                entity.entity_type.as_str(),
                entity.qbo_id,
                entity.sync_token,
                entity.last_updated_utc.to_rfc3339(),
                i64::from(entity.is_deleted),
                entity.raw_json.to_string(),
                mirrored_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_entity(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<MirroredEntity>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT sync_token, last_updated_utc, is_deleted, raw_json
                 FROM entities
                 WHERE realm_id = ?1 AND entity_type = ?2 AND qbo_id = ?3",
                params![realm.as_str(), entity_type.as_str(), qbo_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((sync_token, last_updated, is_deleted, raw)) = row else {
            return Ok(None);
        };

        Ok(Some(MirroredEntity {
            entity_type,
            qbo_id: qbo_id.to_string(),
            sync_token,
            last_updated_utc: DateTime::parse_from_rfc3339(&last_updated)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            is_deleted: is_deleted != 0,
            raw_json: serde_json::from_str(&raw)?,
        }))
    }

    pub fn count_entities(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
    ) -> Result<i64, StoreError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM entities WHERE realm_id = ?1 AND entity_type = ?2",
            params![realm.as_str(), entity_type.as_str()],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aquamentor() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn waterline() -> RealmId {
        RealmId::parse("1234567890123457").unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000, 0).unwrap()
    }

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&aquamentor(), "Aquamentor, Inc.", now()).unwrap();
        store.register_realm(&waterline(), "WaterLine CNC", now()).unwrap();
        store
    }

    fn entity(qbo_id: &str) -> MirroredEntity {
        MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: qbo_id.to_string(),
            sync_token: "0".to_string(),
            last_updated_utc: now(),
            is_deleted: false,
            raw_json: serde_json::json!({ "DocNumber": "21234", "TotalAmt": 74.24 }),
        }
    }

    #[test]
    fn a_fresh_database_is_migrated_to_the_latest_version() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), latest_version());
        assert!(latest_version() > 0);
    }

    #[test]
    fn migration_versions_are_unique_and_ordered() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(versions, sorted, "migrations must be uniquely and increasingly numbered");
    }

    #[test]
    fn reopening_an_existing_database_does_not_reapply_migrations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.db");

        let first = Store::open(&path).unwrap();
        first.register_realm(&aquamentor(), "Aquamentor, Inc.", now()).unwrap();
        first.upsert_entity(&aquamentor(), &entity("71204"), now()).unwrap();
        drop(first);

        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), latest_version());
        // Data survived, and re-running migrations did not clear it.
        assert_eq!(second.count_entities(&aquamentor(), EntityType::Invoice).unwrap(), 1);
    }

    #[test]
    fn wal_and_foreign_keys_are_enabled_on_a_file_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.db");
        let store = Store::open(&path).unwrap();

        let journal: String = store
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");

        let foreign_keys: i64 = store
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn a_newer_schema_than_this_build_supports_is_refused() {
        // Prevents an older build silently operating on a database written by a
        // newer one.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO schema_version (version, name, applied_at) VALUES (999, 'future', ?1)",
                    params![now().to_rfc3339()],
                )
                .unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::SchemaTooNew { found: 999, .. })
        ));
    }

    #[test]
    fn an_entity_round_trips_with_its_raw_payload_intact() {
        let store = store();
        let original = entity("71204");
        store.upsert_entity(&aquamentor(), &original, now()).unwrap();

        let loaded = store
            .get_entity(&aquamentor(), EntityType::Invoice, "71204")
            .unwrap()
            .unwrap();
        assert_eq!(loaded, original);
        // The raw payload is what makes re-parsing possible without re-syncing.
        assert_eq!(loaded.raw_json["DocNumber"], "21234");
    }

    #[test]
    fn upsert_updates_rather_than_duplicating() {
        let store = store();
        store.upsert_entity(&aquamentor(), &entity("71204"), now()).unwrap();

        let mut updated = entity("71204");
        updated.sync_token = "1".to_string();
        updated.raw_json = serde_json::json!({ "DocNumber": "21234", "TotalAmt": 99.99 });
        store.upsert_entity(&aquamentor(), &updated, now()).unwrap();

        assert_eq!(store.count_entities(&aquamentor(), EntityType::Invoice).unwrap(), 1);
        let loaded = store
            .get_entity(&aquamentor(), EntityType::Invoice, "71204")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.sync_token, "1");
    }

    #[test]
    fn realms_share_nothing() {
        // The isolation both kickoff prompts call non-negotiable. The same QBO
        // id in two realms is two distinct records, and neither realm can see
        // the other's.
        let store = store();
        let mut aqua_invoice = entity("71204");
        aqua_invoice.raw_json = serde_json::json!({ "DocNumber": "AQUA" });
        let mut water_invoice = entity("71204");
        water_invoice.raw_json = serde_json::json!({ "DocNumber": "WATER" });

        store.upsert_entity(&aquamentor(), &aqua_invoice, now()).unwrap();
        store.upsert_entity(&waterline(), &water_invoice, now()).unwrap();

        assert_eq!(store.count_entities(&aquamentor(), EntityType::Invoice).unwrap(), 1);
        assert_eq!(store.count_entities(&waterline(), EntityType::Invoice).unwrap(), 1);

        let from_aqua = store
            .get_entity(&aquamentor(), EntityType::Invoice, "71204")
            .unwrap()
            .unwrap();
        assert_eq!(from_aqua.raw_json["DocNumber"], "AQUA");
        let from_water = store
            .get_entity(&waterline(), EntityType::Invoice, "71204")
            .unwrap()
            .unwrap();
        assert_eq!(from_water.raw_json["DocNumber"], "WATER");
    }

    #[test]
    fn entity_types_do_not_collide_within_a_realm() {
        let store = store();
        let invoice = entity("100");
        let mut customer = entity("100");
        customer.entity_type = EntityType::Customer;

        store.upsert_entity(&aquamentor(), &invoice, now()).unwrap();
        store.upsert_entity(&aquamentor(), &customer, now()).unwrap();

        assert_eq!(store.count_entities(&aquamentor(), EntityType::Invoice).unwrap(), 1);
        assert_eq!(store.count_entities(&aquamentor(), EntityType::Customer).unwrap(), 1);
    }

    #[test]
    fn a_missing_entity_is_none_not_an_error() {
        let store = store();
        assert!(store
            .get_entity(&aquamentor(), EntityType::Invoice, "does-not-exist")
            .unwrap()
            .is_none());
    }

    #[test]
    fn writes_are_disabled_until_explicitly_enabled_per_realm() {
        let store = store();
        assert!(!store.is_write_enabled(&aquamentor()).unwrap());
        assert!(!store.is_write_enabled(&waterline()).unwrap());

        // One realm at a time (DESIGN.md §8).
        store.set_write_enabled(&aquamentor(), true).unwrap();
        assert!(store.is_write_enabled(&aquamentor()).unwrap());
        assert!(!store.is_write_enabled(&waterline()).unwrap());
    }

    #[test]
    fn an_unregistered_realm_is_not_write_enabled() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.is_write_enabled(&aquamentor()).unwrap());
    }

    #[test]
    fn strict_tables_reject_a_wrong_typed_value() {
        // Without STRICT, SQLite's type affinity would accept a string into an
        // integer column, which is not acceptable in a financial store.
        let store = store();
        let result = store.connection.execute(
            "INSERT INTO documents
                 (realm_id, qbo_id, doc_type, txn_date, total_minor)
             VALUES (?1, '1', 'Invoice', '2026-08-15', 'not-a-number')",
            params![aquamentor().as_str()],
        );
        assert!(result.is_err(), "STRICT should have rejected a text total");
    }

    #[test]
    fn foreign_keys_stop_an_entity_landing_under_an_unknown_realm() {
        let store = Store::open_in_memory().unwrap();
        let result = store.upsert_entity(&aquamentor(), &entity("71204"), now());
        assert!(result.is_err(), "unregistered realm should have been refused");
    }

    #[test]
    fn document_lines_cannot_orphan_their_document() {
        let store = store();
        let result = store.connection.execute(
            "INSERT INTO document_lines
                 (realm_id, doc_qbo_id, line_no, amount_minor)
             VALUES (?1, 'no-such-document', 1, 4925)",
            params![aquamentor().as_str()],
        );
        assert!(result.is_err(), "orphaned line should have been refused");
    }
}
