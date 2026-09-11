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

use ledger_core::Money;

pub mod lineage;
pub mod query;
pub mod search;

use crate::domain::{ContactType, DocumentType, EntityType, RealmId};
use crate::project::{ParsedDocument, ParsedEntity, Projection};
use crate::sync::SyncCursor;

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
    Migration {
        version: 2,
        name: "projection tables, document links, and search",
        sql: r#"
-- Document columns the UI needs but §3.2's first cut did not carry. These are
-- projection columns: added here rather than by dropping and rebuilding the
-- table, so no replica loses its documents during the upgrade.
ALTER TABLE documents ADD COLUMN contact_type TEXT;   -- 'Customer' | 'Vendor' | NULL
ALTER TABLE documents ADD COLUMN doc_status   TEXT;   -- TxnStatus / POStatus
ALTER TABLE documents ADD COLUMN po_number    TEXT;   -- the customer's PO, not ours
ALTER TABLE documents ADD COLUMN due_date     TEXT;
ALTER TABLE documents ADD COLUMN private_note TEXT;
ALTER TABLE documents ADD COLUMN customer_memo TEXT;
ALTER TABLE documents ADD COLUMN currency     TEXT;
ALTER TABLE documents ADD COLUMN projected_at TEXT;

-- Typing a document number is the fastest path to a document (§3.3), so it
-- gets an index rather than a scan.
CREATE INDEX idx_documents_number ON documents(realm_id, doc_number);
CREATE INDEX idx_documents_po     ON documents(realm_id, po_number);

-- Customers and vendors share a table because every document points at one or
-- the other and the UI treats them the same way. Their QBO id spaces are
-- separate, so contact_type is part of the key.
CREATE TABLE contacts (
    realm_id      TEXT NOT NULL REFERENCES realms(realm_id),
    contact_type  TEXT NOT NULL,
    qbo_id        TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    company_name  TEXT,
    email         TEXT,
    phone         TEXT,
    balance_minor INTEGER,
    is_active     INTEGER NOT NULL DEFAULT 1,
    is_deleted    INTEGER NOT NULL DEFAULT 0,
    projected_at  TEXT NOT NULL,
    PRIMARY KEY (realm_id, contact_type, qbo_id)
) STRICT;

CREATE INDEX idx_contacts_name ON contacts(realm_id, display_name);

CREATE TABLE items (
    realm_id            TEXT NOT NULL REFERENCES realms(realm_id),
    qbo_id              TEXT NOT NULL,
    name                TEXT NOT NULL,
    sku                 TEXT,
    description         TEXT,
    item_type           TEXT,
    -- Prices carry more than two decimals in QBO; amounts do not. Prices stay
    -- Decimal-as-text, amounts become minor units (§3.2).
    unit_price          TEXT,
    purchase_cost       TEXT,
    qty_on_hand         TEXT,
    income_account_id   TEXT,
    expense_account_id  TEXT,
    asset_account_id    TEXT,
    is_active           INTEGER NOT NULL DEFAULT 1,
    is_deleted          INTEGER NOT NULL DEFAULT 0,
    projected_at        TEXT NOT NULL,
    PRIMARY KEY (realm_id, qbo_id)
) STRICT;

CREATE INDEX idx_items_sku ON items(realm_id, sku);

CREATE TABLE accounts (
    realm_id       TEXT NOT NULL REFERENCES realms(realm_id),
    qbo_id         TEXT NOT NULL,
    name           TEXT NOT NULL,
    acct_num       TEXT,
    account_type   TEXT,
    account_subtype TEXT,
    classification TEXT,
    balance_minor  INTEGER,
    is_active      INTEGER NOT NULL DEFAULT 1,
    is_deleted     INTEGER NOT NULL DEFAULT 0,
    projected_at   TEXT NOT NULL,
    PRIMARY KEY (realm_id, qbo_id)
) STRICT;

CREATE TABLE classes (
    realm_id             TEXT NOT NULL REFERENCES realms(realm_id),
    qbo_id               TEXT NOT NULL,
    name                 TEXT NOT NULL,
    fully_qualified_name TEXT,
    parent_id            TEXT,
    is_active            INTEGER NOT NULL DEFAULT 1,
    is_deleted           INTEGER NOT NULL DEFAULT 0,
    projected_at         TEXT NOT NULL,
    PRIMARY KEY (realm_id, qbo_id)
) STRICT;

-- QBO's LinkedTxn, flattened. One row per link, in payload order, so an
-- estimate can find its invoices and a bill payment can find its bills.
-- `seq` keys the row because a link may hang off the header (line_no NULL) or
-- off any line, and the same pair can legitimately appear twice.
CREATE TABLE document_links (
    realm_id    TEXT NOT NULL REFERENCES realms(realm_id),
    from_qbo_id TEXT NOT NULL,
    from_type   TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    to_qbo_id   TEXT NOT NULL,
    to_type     TEXT NOT NULL,
    line_no     INTEGER,
    PRIMARY KEY (realm_id, from_qbo_id, seq)
) STRICT;

-- The reverse direction is the interesting one: given this estimate, what came
-- out of it? Without this index that question is a table scan.
CREATE INDEX idx_links_to ON document_links(realm_id, to_qbo_id);

-- §3.3. External-content FTS5 over the projected tables, maintained by trigger
-- so the index cannot drift from its source. `prefix` is what makes typing the
-- first few characters fast rather than merely correct.
CREATE VIRTUAL TABLE contacts_fts USING fts5(
    display_name, company_name, email,
    content='contacts', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);

CREATE TRIGGER contacts_fts_ai AFTER INSERT ON contacts BEGIN
    INSERT INTO contacts_fts(rowid, display_name, company_name, email)
    VALUES (new.rowid, new.display_name, new.company_name, new.email);
END;
CREATE TRIGGER contacts_fts_ad AFTER DELETE ON contacts BEGIN
    INSERT INTO contacts_fts(contacts_fts, rowid, display_name, company_name, email)
    VALUES ('delete', old.rowid, old.display_name, old.company_name, old.email);
END;
CREATE TRIGGER contacts_fts_au AFTER UPDATE ON contacts BEGIN
    INSERT INTO contacts_fts(contacts_fts, rowid, display_name, company_name, email)
    VALUES ('delete', old.rowid, old.display_name, old.company_name, old.email);
    INSERT INTO contacts_fts(rowid, display_name, company_name, email)
    VALUES (new.rowid, new.display_name, new.company_name, new.email);
END;

CREATE VIRTUAL TABLE items_fts USING fts5(
    name, sku, description,
    content='items', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);

CREATE TRIGGER items_fts_ai AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, name, sku, description)
    VALUES (new.rowid, new.name, new.sku, new.description);
END;
CREATE TRIGGER items_fts_ad AFTER DELETE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, sku, description)
    VALUES ('delete', old.rowid, old.name, old.sku, old.description);
END;
CREATE TRIGGER items_fts_au AFTER UPDATE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, sku, description)
    VALUES ('delete', old.rowid, old.name, old.sku, old.description);
    INSERT INTO items_fts(rowid, name, sku, description)
    VALUES (new.rowid, new.name, new.sku, new.description);
END;

CREATE VIRTUAL TABLE documents_fts USING fts5(
    doc_number, po_number, private_note, customer_memo,
    content='documents', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);

CREATE TRIGGER documents_fts_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, doc_number, po_number, private_note, customer_memo)
    VALUES (new.rowid, new.doc_number, new.po_number, new.private_note, new.customer_memo);
END;
CREATE TRIGGER documents_fts_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, doc_number, po_number, private_note, customer_memo)
    VALUES ('delete', old.rowid, old.doc_number, old.po_number, old.private_note, old.customer_memo);
END;
CREATE TRIGGER documents_fts_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, doc_number, po_number, private_note, customer_memo)
    VALUES ('delete', old.rowid, old.doc_number, old.po_number, old.private_note, old.customer_memo);
    INSERT INTO documents_fts(rowid, doc_number, po_number, private_note, customer_memo)
    VALUES (new.rowid, new.doc_number, new.po_number, new.private_note, new.customer_memo);
END;

CREATE VIRTUAL TABLE lines_fts USING fts5(
    description,
    content='document_lines', content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);

CREATE TRIGGER lines_fts_ai AFTER INSERT ON document_lines BEGIN
    INSERT INTO lines_fts(rowid, description) VALUES (new.rowid, new.description);
END;
CREATE TRIGGER lines_fts_ad AFTER DELETE ON document_lines BEGIN
    INSERT INTO lines_fts(lines_fts, rowid, description)
    VALUES ('delete', old.rowid, old.description);
END;
CREATE TRIGGER lines_fts_au AFTER UPDATE ON document_lines BEGIN
    INSERT INTO lines_fts(lines_fts, rowid, description)
    VALUES ('delete', old.rowid, old.description);
    INSERT INTO lines_fts(rowid, description) VALUES (new.rowid, new.description);
END;
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
        upsert_entity_in(&self.connection, realm, entity, mirrored_at)
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

    // -----------------------------------------------------------------------
    // Sync application (§4.1)
    // -----------------------------------------------------------------------

    /// Mirror a batch of entities, project them, and advance the cursor — all
    /// in one transaction.
    ///
    /// The atomicity is the requirement, not an optimisation. §4.1: a cursor
    /// advanced outside the transaction that wrote the entities it covers can
    /// skip changes after a crash, and nothing afterwards would know to look for
    /// them. One commit per batch is also what makes an initial sync of a whole
    /// book tractable — a commit per entity is fine for a CDC delta of six rows
    /// and wrong for thirty thousand.
    pub fn apply_batch(
        &self,
        realm: &RealmId,
        entities: &[MirroredEntity],
        cursor: Option<&SyncCursor>,
        now: DateTime<Utc>,
    ) -> Result<BatchReport, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut report = BatchReport::default();

        for entity in entities {
            upsert_entity_in(&transaction, realm, entity, now)?;

            match crate::project::parse(entity) {
                Projection::Parsed(parsed) => {
                    write_parsed(&transaction, realm, &parsed, now)?;
                    transaction.execute(
                        "DELETE FROM quarantine_entities
                         WHERE realm_id = ?1 AND entity_type = ?2 AND qbo_id = ?3",
                        params![realm.as_str(), entity.entity_type.as_str(), entity.qbo_id],
                    )?;
                    report.projected += 1;
                }
                Projection::NotProjected => report.not_projected += 1,
                Projection::Quarantine(reason) => {
                    quarantine_in(&transaction, realm, entity, &reason, now)?;
                    report.quarantined += 1;
                }
            }
            report.mirrored += 1;
        }

        if let Some(cursor) = cursor {
            save_cursor_in(&transaction, cursor)?;
        }

        transaction.commit()?;
        Ok(report)
    }

    pub fn load_cursor(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
    ) -> Result<SyncCursor, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT last_cdc_cursor, last_full_sweep FROM sync_cursors
                 WHERE realm_id = ?1 AND entity_type = ?2",
                params![realm.as_str(), entity_type.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;

        // No row means never synced, which is exactly what a fresh cursor says.
        let Some((cdc, sweep)) = row else {
            return Ok(SyncCursor::new(realm.clone(), entity_type));
        };

        Ok(SyncCursor {
            realm_id: realm.clone(),
            entity_type,
            last_cdc_cursor: cdc.as_deref().and_then(parse_optional_timestamp),
            last_full_sweep: sweep.as_deref().and_then(parse_optional_timestamp),
        })
    }

    /// Write a cursor on its own.
    ///
    /// Prefer [`Store::apply_batch`], which writes it alongside the entities it
    /// covers. This exists for the case where a sync legitimately advances a
    /// cursor without mirroring anything — a poll that returned nothing.
    pub fn save_cursor(&self, cursor: &SyncCursor) -> Result<(), StoreError> {
        save_cursor_in(&self.connection, cursor)
    }

    // -----------------------------------------------------------------------
    // Projection (§3.2)
    // -----------------------------------------------------------------------

    /// Parse one mirrored entity and write its projected form.
    ///
    /// The whole projection of a document — header, lines and links — lands in
    /// one transaction, so a crash cannot leave an invoice holding another
    /// invoice's lines.
    pub fn project_entity(
        &self,
        realm: &RealmId,
        entity: &MirroredEntity,
        now: DateTime<Utc>,
    ) -> Result<Projection, StoreError> {
        let projection = crate::project::parse(entity);
        let transaction = self.connection.unchecked_transaction()?;

        match &projection {
            Projection::Parsed(parsed) => {
                write_parsed(&transaction, realm, parsed, now)?;
                // A payload that used to fail and now parses should not keep
                // its old quarantine row sitting there accusing it.
                transaction.execute(
                    "DELETE FROM quarantine_entities
                     WHERE realm_id = ?1 AND entity_type = ?2 AND qbo_id = ?3",
                    params![realm.as_str(), entity.entity_type.as_str(), entity.qbo_id],
                )?;
            }
            Projection::Quarantine(reason) => quarantine_in(&transaction, realm, entity, reason, now)?,
            Projection::NotProjected => {}
        }

        transaction.commit()?;
        Ok(projection)
    }

    /// Rebuild the entire projection for a realm from the raw payloads already
    /// on disk.
    ///
    /// This is the point of keeping `raw_json`: a parser fix ships, this runs,
    /// and the corrected projection appears without a single call to Intuit.
    pub fn reproject_all(
        &self,
        realm: &RealmId,
        now: DateTime<Utc>,
    ) -> Result<ReprojectReport, StoreError> {
        let entities = self.all_entities(realm)?;

        let transaction = self.connection.unchecked_transaction()?;
        // Order matters only for document_links and document_lines, which point
        // at documents. Everything else is independent.
        for table in [
            "document_links",
            "document_lines",
            "documents",
            "contacts",
            "items",
            "accounts",
            "classes",
            "quarantine_entities",
        ] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE realm_id = ?1"),
                params![realm.as_str()],
            )?;
        }

        let mut report = ReprojectReport::default();
        for entity in &entities {
            match crate::project::parse(entity) {
                Projection::Parsed(parsed) => {
                    write_parsed(&transaction, realm, &parsed, now)?;
                    report.parsed += 1;
                }
                Projection::NotProjected => report.not_projected += 1,
                Projection::Quarantine(reason) => {
                    quarantine_in(&transaction, realm, entity, &reason, now)?;
                    report.quarantined += 1;
                }
            }
        }
        transaction.commit()?;
        Ok(report)
    }

    fn all_entities(&self, realm: &RealmId) -> Result<Vec<MirroredEntity>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT entity_type, qbo_id, sync_token, last_updated_utc, is_deleted, raw_json
             FROM entities WHERE realm_id = ?1
             ORDER BY entity_type, qbo_id",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut entities = Vec::new();
        for row in rows {
            let (entity_type, qbo_id, sync_token, last_updated, is_deleted, raw) = row?;
            // An entity type this build does not know is not a parse failure —
            // it is a newer schema. Leave it in `entities` and move on.
            let Ok(entity_type) = EntityType::parse(&entity_type) else {
                continue;
            };
            entities.push(MirroredEntity {
                entity_type,
                qbo_id,
                sync_token,
                last_updated_utc: parse_timestamp(&last_updated),
                is_deleted: is_deleted != 0,
                raw_json: serde_json::from_str(&raw)?,
            });
        }
        Ok(entities)
    }

    pub fn quarantined(&self, realm: &RealmId) -> Result<Vec<QuarantinedEntity>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT entity_type, qbo_id, reason FROM quarantine_entities
             WHERE realm_id = ?1 ORDER BY entity_type, qbo_id",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok(QuarantinedEntity {
                entity_type: row.get(0)?,
                qbo_id: row.get(1)?,
                reason: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // -----------------------------------------------------------------------
    // Projected reads
    // -----------------------------------------------------------------------

    pub fn get_document(
        &self,
        realm: &RealmId,
        qbo_id: &str,
    ) -> Result<Option<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT} WHERE d.realm_id = ?1 AND d.qbo_id = ?2"
        ))?;
        let row = statement
            .query_row(params![realm.as_str(), qbo_id], document_row)
            .optional()?;
        Ok(row)
    }

    /// Documents of one type, newest first — the list behind every register.
    pub fn list_documents(
        &self,
        realm: &RealmId,
        doc_type: DocumentType,
        limit: i64,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.doc_type = ?2 AND d.is_deleted = 0
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), doc_type.as_str(), limit],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn document_lines(
        &self,
        realm: &RealmId,
        doc_qbo_id: &str,
    ) -> Result<Vec<LineRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT line_no, item_id, description, qty, unit_price,
                    amount_minor, class_id, is_taxable
             FROM document_lines
             WHERE realm_id = ?1 AND doc_qbo_id = ?2
             ORDER BY line_no",
        )?;
        let rows = statement.query_map(params![realm.as_str(), doc_qbo_id], |row| {
            Ok(LineRow {
                line_no: row.get(0)?,
                item_id: row.get(1)?,
                description: row.get(2)?,
                qty: row.get::<_, Option<String>>(3)?,
                unit_price: row.get::<_, Option<String>>(4)?,
                amount: Money::from_minor(row.get(5)?),
                class_id: row.get(6)?,
                is_taxable: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn count_projected(&self, realm: &RealmId, table: ProjectedTable) -> Result<i64, StoreError> {
        Ok(self.connection.query_row(
            &format!("SELECT COUNT(*) FROM {} WHERE realm_id = ?1", table.as_str()),
            params![realm.as_str()],
            |row| row.get(0),
        )?)
    }
}

/// The projected tables, named as a type so `count_projected` cannot be handed
/// an arbitrary string that reaches SQL.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ProjectedTable {
    Contacts,
    Items,
    Accounts,
    Classes,
    Documents,
    DocumentLines,
    DocumentLinks,
}

impl ProjectedTable {
    const fn as_str(self) -> &'static str {
        match self {
            ProjectedTable::Contacts => "contacts",
            ProjectedTable::Items => "items",
            ProjectedTable::Accounts => "accounts",
            ProjectedTable::Classes => "classes",
            ProjectedTable::Documents => "documents",
            ProjectedTable::DocumentLines => "document_lines",
            ProjectedTable::DocumentLinks => "document_links",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReprojectReport {
    pub parsed: usize,
    pub not_projected: usize,
    pub quarantined: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedEntity {
    pub entity_type: String,
    pub qbo_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRow {
    pub qbo_id: String,
    pub doc_type: DocumentType,
    pub doc_number: Option<String>,
    pub txn_date: String,
    pub due_date: Option<String>,
    pub contact_id: Option<String>,
    pub contact_type: Option<ContactType>,
    /// Joined from `contacts` so a register does not issue one query per row.
    pub contact_name: Option<String>,
    pub class_id: Option<String>,
    pub total: Money,
    pub balance: Option<Money>,
    pub doc_status: Option<String>,
    pub po_number: Option<String>,
    pub private_note: Option<String>,
    pub customer_memo: Option<String>,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRow {
    pub line_no: i64,
    pub item_id: Option<String>,
    pub description: Option<String>,
    /// Decimal as text, exactly as stored — parsing it is the caller's choice.
    pub qty: Option<String>,
    pub unit_price: Option<String>,
    pub amount: Money,
    pub class_id: Option<String>,
    pub is_taxable: bool,
}

/// Every projected document read goes through this projection and join, so a
/// register never issues one query per row to name its customer.
pub(crate) const DOCUMENT_SELECT: &str = "\
SELECT d.qbo_id, d.doc_type, d.doc_number, d.txn_date, d.due_date,
       d.contact_id, d.contact_type, c.display_name, d.class_id,
       d.total_minor, d.balance_minor, d.doc_status, d.po_number,
       d.private_note, d.customer_memo, d.is_deleted
FROM documents d
LEFT JOIN contacts c
       ON c.realm_id = d.realm_id
      AND c.contact_type = d.contact_type
      AND c.qbo_id = d.contact_id
";

pub(crate) fn document_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRow> {
    let doc_type: String = row.get(1)?;
    Ok(DocumentRow {
        qbo_id: row.get(0)?,
        // A row is only written through `write_parsed`, which takes a
        // `DocumentType`, so an unreadable value here means the file was edited
        // outside the app. Falling back to `JournalEntry` would be a quiet lie;
        // surfacing it as a column decode error is not.
        doc_type: EntityType::parse(&doc_type)
            .ok()
            .and_then(EntityType::as_document)
            .ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(crate::domain::DomainError::UnknownEntityType(doc_type)),
                )
            })?,
        doc_number: row.get(2)?,
        txn_date: row.get(3)?,
        due_date: row.get(4)?,
        contact_id: row.get(5)?,
        contact_type: row
            .get::<_, Option<String>>(6)?
            .as_deref()
            .and_then(ContactType::parse),
        contact_name: row.get(7)?,
        class_id: row.get(8)?,
        total: Money::from_minor(row.get(9)?),
        balance: row.get::<_, Option<i64>>(10)?.map(Money::from_minor),
        doc_status: row.get(11)?,
        po_number: row.get(12)?,
        private_note: row.get(13)?,
        customer_memo: row.get(14)?,
        is_deleted: row.get::<_, i64>(15)? != 0,
    })
}

fn upsert_entity_in(
    connection: &rusqlite::Connection,
    realm: &RealmId,
    entity: &MirroredEntity,
    mirrored_at: DateTime<Utc>,
) -> Result<(), StoreError> {
    connection.execute(
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

fn quarantine_in(
    connection: &rusqlite::Connection,
    realm: &RealmId,
    entity: &MirroredEntity,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO quarantine_entities
             (realm_id, entity_type, qbo_id, reason, raw_json, quarantined_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(realm_id, entity_type, qbo_id) DO UPDATE SET
             reason = excluded.reason,
             raw_json = excluded.raw_json,
             quarantined_at = excluded.quarantined_at",
        params![
            realm.as_str(),
            entity.entity_type.as_str(),
            entity.qbo_id,
            reason,
            entity.raw_json.to_string(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn save_cursor_in(
    connection: &rusqlite::Connection,
    cursor: &SyncCursor,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO sync_cursors
             (realm_id, entity_type, last_cdc_cursor, last_full_sweep)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(realm_id, entity_type) DO UPDATE SET
             last_cdc_cursor = excluded.last_cdc_cursor,
             last_full_sweep = excluded.last_full_sweep",
        params![
            cursor.realm_id.as_str(),
            cursor.entity_type.as_str(),
            cursor.last_cdc_cursor.map(|at| at.to_rfc3339()),
            cursor.last_full_sweep.map(|at| at.to_rfc3339()),
        ],
    )?;
    Ok(())
}

/// A stored timestamp that will not parse is treated as absent rather than as
/// "now" — an unreadable cursor should cause a full sweep, which is safe, not a
/// poll from the current moment, which silently skips everything before it.
fn parse_optional_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub mirrored: usize,
    pub projected: usize,
    pub not_projected: usize,
    pub quarantined: usize,
}

fn parse_timestamp(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn write_parsed(
    transaction: &rusqlite::Transaction<'_>,
    realm: &RealmId,
    parsed: &ParsedEntity,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let stamp = now.to_rfc3339();
    match parsed {
        ParsedEntity::Contact(contact) => {
            transaction.execute(
                "INSERT INTO contacts
                     (realm_id, contact_type, qbo_id, display_name, company_name,
                      email, phone, balance_minor, is_active, is_deleted, projected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(realm_id, contact_type, qbo_id) DO UPDATE SET
                     display_name = excluded.display_name,
                     company_name = excluded.company_name,
                     email = excluded.email,
                     phone = excluded.phone,
                     balance_minor = excluded.balance_minor,
                     is_active = excluded.is_active,
                     is_deleted = excluded.is_deleted,
                     projected_at = excluded.projected_at",
                params![
                    realm.as_str(),
                    contact.contact_type.as_str(),
                    contact.qbo_id,
                    contact.display_name,
                    contact.company_name,
                    contact.email,
                    contact.phone,
                    contact.balance.map(Money::minor),
                    i64::from(contact.is_active),
                    i64::from(contact.is_deleted),
                    stamp,
                ],
            )?;
        }
        ParsedEntity::Item(item) => {
            transaction.execute(
                "INSERT INTO items
                     (realm_id, qbo_id, name, sku, description, item_type,
                      unit_price, purchase_cost, qty_on_hand, income_account_id,
                      expense_account_id, asset_account_id, is_active, is_deleted,
                      projected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(realm_id, qbo_id) DO UPDATE SET
                     name = excluded.name,
                     sku = excluded.sku,
                     description = excluded.description,
                     item_type = excluded.item_type,
                     unit_price = excluded.unit_price,
                     purchase_cost = excluded.purchase_cost,
                     qty_on_hand = excluded.qty_on_hand,
                     income_account_id = excluded.income_account_id,
                     expense_account_id = excluded.expense_account_id,
                     asset_account_id = excluded.asset_account_id,
                     is_active = excluded.is_active,
                     is_deleted = excluded.is_deleted,
                     projected_at = excluded.projected_at",
                params![
                    realm.as_str(),
                    item.qbo_id,
                    item.name,
                    item.sku,
                    item.description,
                    item.item_type,
                    item.unit_price.map(|d| d.to_string()),
                    item.purchase_cost.map(|d| d.to_string()),
                    item.qty_on_hand.map(|d| d.to_string()),
                    item.income_account_id,
                    item.expense_account_id,
                    item.asset_account_id,
                    i64::from(item.is_active),
                    i64::from(item.is_deleted),
                    stamp,
                ],
            )?;
        }
        ParsedEntity::Account(account) => {
            transaction.execute(
                "INSERT INTO accounts
                     (realm_id, qbo_id, name, acct_num, account_type, account_subtype,
                      classification, balance_minor, is_active, is_deleted, projected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(realm_id, qbo_id) DO UPDATE SET
                     name = excluded.name,
                     acct_num = excluded.acct_num,
                     account_type = excluded.account_type,
                     account_subtype = excluded.account_subtype,
                     classification = excluded.classification,
                     balance_minor = excluded.balance_minor,
                     is_active = excluded.is_active,
                     is_deleted = excluded.is_deleted,
                     projected_at = excluded.projected_at",
                params![
                    realm.as_str(),
                    account.qbo_id,
                    account.name,
                    account.acct_num,
                    account.account_type,
                    account.account_subtype,
                    account.classification,
                    account.balance.map(Money::minor),
                    i64::from(account.is_active),
                    i64::from(account.is_deleted),
                    stamp,
                ],
            )?;
        }
        ParsedEntity::Class(class) => {
            transaction.execute(
                "INSERT INTO classes
                     (realm_id, qbo_id, name, fully_qualified_name, parent_id,
                      is_active, is_deleted, projected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(realm_id, qbo_id) DO UPDATE SET
                     name = excluded.name,
                     fully_qualified_name = excluded.fully_qualified_name,
                     parent_id = excluded.parent_id,
                     is_active = excluded.is_active,
                     is_deleted = excluded.is_deleted,
                     projected_at = excluded.projected_at",
                params![
                    realm.as_str(),
                    class.qbo_id,
                    class.name,
                    class.fully_qualified_name,
                    class.parent_id,
                    i64::from(class.is_active),
                    i64::from(class.is_deleted),
                    stamp,
                ],
            )?;
        }
        ParsedEntity::Document(document) => write_document(transaction, realm, document, &stamp)?,
    }
    Ok(())
}

fn write_document(
    transaction: &rusqlite::Transaction<'_>,
    realm: &RealmId,
    document: &ParsedDocument,
    stamp: &str,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO documents
             (realm_id, qbo_id, doc_type, doc_number, txn_date, contact_id,
              class_id, total_minor, balance_minor, is_deleted, contact_type,
              doc_status, po_number, due_date, private_note, customer_memo,
              currency, projected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18)
         ON CONFLICT(realm_id, qbo_id) DO UPDATE SET
             doc_type = excluded.doc_type,
             doc_number = excluded.doc_number,
             txn_date = excluded.txn_date,
             contact_id = excluded.contact_id,
             class_id = excluded.class_id,
             total_minor = excluded.total_minor,
             balance_minor = excluded.balance_minor,
             is_deleted = excluded.is_deleted,
             contact_type = excluded.contact_type,
             doc_status = excluded.doc_status,
             po_number = excluded.po_number,
             due_date = excluded.due_date,
             private_note = excluded.private_note,
             customer_memo = excluded.customer_memo,
             currency = excluded.currency,
             projected_at = excluded.projected_at",
        params![
            realm.as_str(),
            document.qbo_id,
            document.doc_type.as_str(),
            document.doc_number,
            document.txn_date,
            document.contact_id,
            document.class_id,
            document.total.minor(),
            document.balance.map(Money::minor),
            i64::from(document.is_deleted),
            document.contact_type.map(ContactType::as_str),
            document.doc_status,
            document.po_number,
            document.due_date,
            document.private_note,
            document.customer_memo,
            document.currency,
            stamp,
        ],
    )?;

    // Lines and links are replaced wholesale rather than merged: an edit in QBO
    // can delete a line, and an upsert alone would leave the deleted one behind.
    transaction.execute(
        "DELETE FROM document_lines WHERE realm_id = ?1 AND doc_qbo_id = ?2",
        params![realm.as_str(), document.qbo_id],
    )?;
    transaction.execute(
        "DELETE FROM document_links WHERE realm_id = ?1 AND from_qbo_id = ?2",
        params![realm.as_str(), document.qbo_id],
    )?;

    for line in &document.lines {
        transaction.execute(
            "INSERT INTO document_lines
                 (realm_id, doc_qbo_id, line_no, item_id, description, qty,
                  unit_price, amount_minor, class_id, is_taxable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                realm.as_str(),
                document.qbo_id,
                line.line_no,
                line.item_id,
                line.description,
                line.qty.map(|d| d.to_string()),
                line.unit_price.map(|d| d.to_string()),
                line.amount.minor(),
                line.class_id,
                i64::from(line.is_taxable),
            ],
        )?;
    }

    for (seq, link) in document.links.iter().enumerate() {
        transaction.execute(
            "INSERT INTO document_links
                 (realm_id, from_qbo_id, from_type, seq, to_qbo_id, to_type, line_no)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                realm.as_str(),
                document.qbo_id,
                document.doc_type.as_str(),
                seq as i64,
                link.to_qbo_id,
                link.to_type,
                link.line_no,
            ],
        )?;
    }

    Ok(())
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
