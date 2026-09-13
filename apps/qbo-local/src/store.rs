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

pub mod backup;
pub mod index;
pub mod lineage;
pub mod query;
pub mod search;

use crate::domain::{ContactType, DocumentType, EntityType, RealmId};
use crate::outbox::{Operation, OutboxRecord, OutboxState};
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
    /// A computed total overflowed `i64` minor units. Unreachable at any
    /// realistic book size, but money arithmetic returns `Result` everywhere
    /// (D5), and a read path is no exception.
    #[error("money: {0}")]
    Money(#[from] ledger_core::MoneyError),
    /// Snapshotting (`store::backup`, `DESIGN.md` §8) touches the filesystem
    /// directly — creating the snapshot directory, pruning old files — which
    /// none of the SQL-only paths above needed an error variant for.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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

    /// Open an existing replica strictly read-only — no create, no write.
    ///
    /// `SQLITE_OPEN_READ_ONLY` with `SQLITE_OPEN_CREATE` omitted is enforced by
    /// SQLite itself, not by caller discipline: an attempted write on this
    /// connection fails at the engine rather than depending on every future
    /// caller remembering not to issue one. This is what the MCP server
    /// (`mcp.rs`) opens with — a read surface that cannot become a write path
    /// by accident.
    ///
    /// The file must already exist and be migrated; this never runs
    /// migrations, which write.
    pub fn open_read_only(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        let store = Store { connection };

        let version = store.schema_version()?;
        if version > latest_version() {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: latest_version(),
            });
        }
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
        let _: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
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

        let current: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?;

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

    /// Every registered realm, for a status view that has no single realm to
    /// scope to yet — the one place in this module that reads across realms,
    /// because listing them is what lets a caller pick one.
    pub fn list_realms(&self) -> Result<Vec<RealmSummary>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT realm_id, display_name, is_write_enabled FROM realms ORDER BY realm_id",
        )?;
        let rows = statement.query_map([], |row| {
            let raw_realm_id: String = row.get(0)?;
            // As `document_row`'s `doc_type` decode: a row only ever lands here
            // through `register_realm`, which takes a `&RealmId`, so a value
            // that fails to parse means the file was edited outside the app.
            let realm_id = RealmId::parse(&raw_realm_id).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(RealmSummary {
                realm_id,
                display_name: row.get(1)?,
                is_write_enabled: row.get::<_, i64>(2)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
            Projection::Quarantine(reason) => {
                quarantine_in(&transaction, realm, entity, reason, now)?
            }
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

    pub fn count_projected(
        &self,
        realm: &RealmId,
        table: ProjectedTable,
    ) -> Result<i64, StoreError> {
        Ok(self.connection.query_row(
            &format!(
                "SELECT COUNT(*) FROM {} WHERE realm_id = ?1",
                table.as_str()
            ),
            params![realm.as_str()],
            |row| row.get(0),
        )?)
    }

    // -----------------------------------------------------------------------
    // The outbox (`DESIGN.md` §6). Durable storage for `outbox::OutboxRecord`
    // — `worker::Drainer` runs the state machine in memory, one drain pass at
    // a time; this is what lets a record's state survive past the process
    // that was draining it, which is the entire premise `tests/chaos.rs`
    // exercises.
    // -----------------------------------------------------------------------

    /// Persist a newly created outbox record.
    pub fn insert_outbox_record(&self, record: &OutboxRecord) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO outbox (id, realm_id, entity_type, operation, payload_json,
                 local_entity_id, base_sync_token, request_id, state, attempts,
                 depends_on, last_error, qbo_response_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13, ?14)",
            params![
                record.id.to_string(),
                record.realm_id.as_str(),
                record.entity_type.as_str(),
                record.operation.as_str(),
                record.payload_json.to_string(),
                record.local_entity_id,
                record.base_sync_token,
                record.request_id.to_string(),
                record.state.as_str(),
                record.attempts,
                record.depends_on.map(|id| id.to_string()),
                record.last_error,
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Persist a state transition.
    ///
    /// Call this **immediately** after any `OutboxRecord` transition, in
    /// particular `begin_attempt`'s move to `in_flight` — before the client
    /// call it guards is issued, per `DESIGN.md` §6.1. `worker::Drainer`
    /// exposes exactly this timing through `set_on_transition` without
    /// depending on `Store` itself; wiring the two together is the caller's
    /// job (`apps/qbo-local/src/bin/chaos-child.rs` is the only one that does
    /// today).
    pub fn save_outbox_record(&self, record: &OutboxRecord) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE outbox
             SET state = ?2, attempts = ?3, last_error = ?4, updated_at = ?5
             WHERE id = ?1",
            params![
                record.id.to_string(),
                record.state.as_str(),
                record.attempts,
                record.last_error,
                record.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Every outbox record for a realm, oldest first — UUIDv7 order, which is
    /// creation order and therefore drain order (`DESIGN.md` §6.3).
    pub fn list_outbox_records(&self, realm: &RealmId) -> Result<Vec<OutboxRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, realm_id, entity_type, operation, payload_json, local_entity_id,
                    base_sync_token, request_id, state, attempts, depends_on, last_error,
                    created_at, updated_at
             FROM outbox WHERE realm_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map(params![realm.as_str()], outbox_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record the id QBO assigned to a `local:`-prefixed id, once the create
    /// that owns it applies (`DESIGN.md` §6.4). Idempotent: replaying the same
    /// mapping — the case a recovered `in_flight` create adopting rather than
    /// duplicating produces — overwrites with the same value rather than
    /// erroring.
    pub fn record_local_id_mapping(
        &self,
        realm: &RealmId,
        local_entity_id: &str,
        entity_type: EntityType,
        qbo_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO local_id_map (realm_id, local_entity_id, entity_type, qbo_id, mapped_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(realm_id, local_entity_id) DO UPDATE SET
                 qbo_id = excluded.qbo_id, mapped_at = excluded.mapped_at",
            params![
                realm.as_str(),
                local_entity_id,
                entity_type.as_str(),
                qbo_id,
                now.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Every `local:` id this realm has resolved to a real QBO id — what a
    /// restarted process reloads to reseed a fresh `worker::Drainer` via
    /// `seed_mapping`, so a dependant queued before the crash can still have
    /// its reference rewritten without redraining its already-applied parent.
    pub fn load_local_id_map(&self, realm: &RealmId) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT local_entity_id, qbo_id FROM local_id_map WHERE realm_id = ?1")?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn outbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRecord> {
    let id: String = row.get(0)?;
    let realm_id: String = row.get(1)?;
    let entity_type: String = row.get(2)?;
    let operation: String = row.get(3)?;
    let payload_json: String = row.get(4)?;
    let local_entity_id: String = row.get(5)?;
    let base_sync_token: Option<String> = row.get(6)?;
    let request_id: String = row.get(7)?;
    let state: String = row.get(8)?;
    let attempts: u32 = row.get(9)?;
    let depends_on: Option<String> = row.get(10)?;
    let last_error: Option<String> = row.get(11)?;
    let created_at: String = row.get(12)?;
    let updated_at: String = row.get(13)?;

    // Every row here only ever came from `insert_outbox_record`, which takes
    // an already-validated `OutboxRecord` — a value that fails one of these
    // conversions means the file was edited outside the app, exactly as in
    // `list_realms`'s `raw_realm_id` decode above.
    fn fail(
        column: usize,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    }
    fn fail_message(column: usize, message: String) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            message.into(),
        )
    }

    Ok(OutboxRecord {
        id: uuid::Uuid::parse_str(&id).map_err(|e| fail(0, e))?,
        realm_id: RealmId::parse(&realm_id).map_err(|e| fail(1, e))?,
        entity_type: EntityType::parse(&entity_type).map_err(|e| fail(2, e))?,
        operation: Operation::parse(&operation)
            .ok_or_else(|| fail_message(3, format!("unknown outbox operation {operation:?}")))?,
        payload_json: serde_json::from_str(&payload_json).map_err(|e| fail(4, e))?,
        local_entity_id,
        base_sync_token,
        request_id: uuid::Uuid::parse_str(&request_id).map_err(|e| fail(7, e))?,
        state: OutboxState::parse(&state)
            .ok_or_else(|| fail_message(8, format!("unknown outbox state {state:?}")))?,
        attempts,
        depends_on: depends_on
            .map(|raw| uuid::Uuid::parse_str(&raw).map_err(|e| fail(10, e)))
            .transpose()?,
        last_error,
        created_at: parse_timestamp(&created_at),
        updated_at: parse_timestamp(&updated_at),
    })
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

/// A registered realm, as listed by [`Store::list_realms`] — enough for a
/// status view to name each realm and know whether writes are on before it
/// asks for anything more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealmSummary {
    pub realm_id: RealmId,
    pub display_name: String,
    pub is_write_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedEntity {
    pub entity_type: String,
    pub qbo_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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
    #[serde(serialize_with = "serialize_money")]
    pub total: Money,
    #[serde(serialize_with = "serialize_money_opt")]
    pub balance: Option<Money>,
    pub doc_status: Option<String>,
    pub po_number: Option<String>,
    pub private_note: Option<String>,
    pub customer_memo: Option<String>,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LineRow {
    pub line_no: i64,
    pub item_id: Option<String>,
    pub description: Option<String>,
    /// Decimal as text, exactly as stored — parsing it is the caller's choice.
    pub qty: Option<String>,
    pub unit_price: Option<String>,
    #[serde(serialize_with = "serialize_money")]
    pub amount: Money,
    pub class_id: Option<String>,
    pub is_taxable: bool,
}

/// Renders [`Money`] as a decimal string with two places — `"1234.56"`,
/// negative as `"-12.00"` — for MCP tool output (`mcp.rs`), whose consumers
/// are LLM skills reading text, not the bare minor-units integer `Money`'s own
/// `Serialize` impl produces. That impl is untouched; this is only ever named
/// from a field's `#[serde(serialize_with = ...)]`, never used to change what
/// `Money` itself serialises as.
pub(crate) fn serialize_money<S>(money: &Money, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&format_money(*money))
}

/// The `Option<Money>` counterpart of [`serialize_money`], for balance and
/// similar fields that may be absent.
pub(crate) fn serialize_money_opt<S>(
    money: &Option<Money>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match money {
        Some(money) => serializer.serialize_str(&format_money(*money)),
        None => serializer.serialize_none(),
    }
}

fn format_money(money: Money) -> String {
    // `unsigned_abs` rather than `.abs()`: `i64::MIN` has no positive
    // counterpart (money.rs's own overflow test makes the same point), and
    // this path must never panic on a value that reached storage safely.
    let minor = money.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let magnitude = minor.unsigned_abs();
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
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
        store
            .register_realm(&aquamentor(), "Aquamentor, Inc.", now())
            .unwrap();
        store
            .register_realm(&waterline(), "WaterLine CNC", now())
            .unwrap();
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
        assert_eq!(
            versions, sorted,
            "migrations must be uniquely and increasingly numbered"
        );
    }

    #[test]
    fn reopening_an_existing_database_does_not_reapply_migrations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.db");

        let first = Store::open(&path).unwrap();
        first
            .register_realm(&aquamentor(), "Aquamentor, Inc.", now())
            .unwrap();
        first
            .upsert_entity(&aquamentor(), &entity("71204"), now())
            .unwrap();
        drop(first);

        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), latest_version());
        // Data survived, and re-running migrations did not clear it.
        assert_eq!(
            second
                .count_entities(&aquamentor(), EntityType::Invoice)
                .unwrap(),
            1
        );
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
        store
            .upsert_entity(&aquamentor(), &original, now())
            .unwrap();

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
        store
            .upsert_entity(&aquamentor(), &entity("71204"), now())
            .unwrap();

        let mut updated = entity("71204");
        updated.sync_token = "1".to_string();
        updated.raw_json = serde_json::json!({ "DocNumber": "21234", "TotalAmt": 99.99 });
        store.upsert_entity(&aquamentor(), &updated, now()).unwrap();

        assert_eq!(
            store
                .count_entities(&aquamentor(), EntityType::Invoice)
                .unwrap(),
            1
        );
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

        store
            .upsert_entity(&aquamentor(), &aqua_invoice, now())
            .unwrap();
        store
            .upsert_entity(&waterline(), &water_invoice, now())
            .unwrap();

        assert_eq!(
            store
                .count_entities(&aquamentor(), EntityType::Invoice)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_entities(&waterline(), EntityType::Invoice)
                .unwrap(),
            1
        );

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
        store
            .upsert_entity(&aquamentor(), &customer, now())
            .unwrap();

        assert_eq!(
            store
                .count_entities(&aquamentor(), EntityType::Invoice)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_entities(&aquamentor(), EntityType::Customer)
                .unwrap(),
            1
        );
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
    fn list_realms_returns_every_registered_realm() {
        let store = store();
        store.set_write_enabled(&aquamentor(), true).unwrap();

        let realms = store.list_realms().unwrap();
        assert_eq!(realms.len(), 2);
        let aqua = realms.iter().find(|r| r.realm_id == aquamentor()).unwrap();
        assert_eq!(aqua.display_name, "Aquamentor, Inc.");
        assert!(aqua.is_write_enabled);
        let water = realms.iter().find(|r| r.realm_id == waterline()).unwrap();
        assert!(!water.is_write_enabled);
    }

    #[test]
    fn list_realms_is_empty_for_a_fresh_store() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.list_realms().unwrap().is_empty());
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
        assert!(
            result.is_err(),
            "unregistered realm should have been refused"
        );
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

    // -----------------------------------------------------------------------
    // Outbox persistence — the durability `tests/chaos.rs` depends on.
    // -----------------------------------------------------------------------

    fn outbox_record(entity_type: EntityType, local_entity_id: &str) -> OutboxRecord {
        OutboxRecord::new(
            crate::outbox::NewOutboxRecord {
                id: uuid::Uuid::now_v7(),
                request_id: uuid::Uuid::now_v7(),
                realm_id: aquamentor(),
                entity_type,
                operation: Operation::Create,
                payload_json: serde_json::json!({ "DisplayName": "BLUE HARBOR SWIM" }),
                local_entity_id: local_entity_id.to_string(),
                base_sync_token: None,
            },
            now(),
        )
    }

    #[test]
    fn an_inserted_outbox_record_round_trips_through_the_store() {
        let store = store();
        let record = outbox_record(EntityType::Customer, "local:cust-1");
        store.insert_outbox_record(&record).unwrap();

        let loaded = store.list_outbox_records(&aquamentor()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, record.id);
        assert_eq!(loaded[0].request_id, record.request_id);
        assert_eq!(loaded[0].entity_type, EntityType::Customer);
        assert_eq!(loaded[0].operation, Operation::Create);
        assert_eq!(loaded[0].state, OutboxState::Pending);
        assert_eq!(loaded[0].local_entity_id, "local:cust-1");
        assert_eq!(loaded[0].payload_json, record.payload_json);
    }

    #[test]
    fn saving_a_transition_persists_the_new_state() {
        let store = store();
        let mut record = outbox_record(EntityType::Invoice, "local:inv-1");
        store.insert_outbox_record(&record).unwrap();

        record.begin_attempt(true, now()).unwrap();
        store.save_outbox_record(&record).unwrap();

        let loaded = store.list_outbox_records(&aquamentor()).unwrap();
        assert_eq!(loaded[0].state, OutboxState::InFlight);
        assert_eq!(loaded[0].attempts, 1);

        record.mark_applied(now()).unwrap();
        store.save_outbox_record(&record).unwrap();
        let loaded = store.list_outbox_records(&aquamentor()).unwrap();
        assert_eq!(loaded[0].state, OutboxState::Applied);
    }

    #[test]
    fn outbox_records_are_listed_in_uuidv7_creation_order() {
        let store = store();
        let first = outbox_record(EntityType::Invoice, "local:inv-1");
        let second = outbox_record(EntityType::Invoice, "local:inv-2");
        // Insert out of order; listing order must come from the id, not from
        // insertion order.
        store.insert_outbox_record(&second).unwrap();
        store.insert_outbox_record(&first).unwrap();

        let loaded = store.list_outbox_records(&aquamentor()).unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![first.id, second.id]
        );
    }

    #[test]
    fn realms_do_not_see_each_others_outbox_records() {
        let store = store();
        let mut record = outbox_record(EntityType::Invoice, "local:inv-1");
        record.realm_id = waterline();
        store.insert_outbox_record(&record).unwrap();

        assert!(store.list_outbox_records(&aquamentor()).unwrap().is_empty());
        assert_eq!(store.list_outbox_records(&waterline()).unwrap().len(), 1);
    }

    #[test]
    fn a_local_id_mapping_round_trips_and_can_be_replaced() {
        let store = store();
        store
            .record_local_id_mapping(
                &aquamentor(),
                "local:cust-1",
                EntityType::Customer,
                "42",
                now(),
            )
            .unwrap();

        let map = store.load_local_id_map(&aquamentor()).unwrap();
        assert_eq!(map, vec![("local:cust-1".to_string(), "42".to_string())]);

        // Recovering the same in_flight create twice (once for real, once for
        // a test that re-adopts) must overwrite rather than error.
        store
            .record_local_id_mapping(
                &aquamentor(),
                "local:cust-1",
                EntityType::Customer,
                "42",
                now(),
            )
            .unwrap();
        assert_eq!(store.load_local_id_map(&aquamentor()).unwrap().len(), 1);
    }
}
