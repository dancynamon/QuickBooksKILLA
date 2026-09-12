//! The SQLite ledger: schema, migrations, invariants and the period gate.
//! `LEDGER-DESIGN.md` §4, §5.
//!
//! Every invariant in §4's table is enforced either by a `CHECK` constraint, a
//! trigger, or a composite foreign key — never by "the caller remembered to
//! check first". [`Ledger`] owns the [`rusqlite::Connection`]; nothing outside
//! this module can issue raw SQL against it.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::Money;
use rusqlite::{params, Connection, OptionalExtension, Row};
use rust_decimal::Decimal;
use thiserror::Error;
use uuid::Uuid;

use crate::chart::{self, Classification};
use crate::types::{
    AccountId, ClassId, ContactKind, ContactRef, DocKind, JournalEntry, JournalLine,
    LedgerDocument, Side,
};

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored value is not valid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("money: {0}")]
    Money(#[from] ledger_core::MoneyError),
    #[error("period locked through {period_end}")]
    PeriodClosed { period_end: NaiveDate },
    #[error("entry does not balance")]
    Unbalanced,
    #[error("line {line_no} touches income, COGS or expense and requires a class")]
    MissingClass { line_no: i64 },
    #[error("line {line_no} is a balance-sheet line and may not carry a class")]
    UnexpectedClass { line_no: i64 },
    #[error("unknown account: {0}")]
    UnknownAccount(String),
    #[error("unknown class: {0}")]
    UnknownClass(String),
    #[error("not found")]
    NotFound,
    #[error("entry already posted")]
    AlreadyPosted,
    #[error("ledger schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("stored date {0:?} is not a valid date")]
    BadDate(String),
    #[error("stored timestamp {0:?} is not a valid RFC 3339 timestamp")]
    BadTimestamp(String),
    #[error(transparent)]
    Post(#[from] crate::post::PostError),
    /// §10 close checklist: `opening_balance + sum(matched lines) !=
    /// closing_balance`, or an unmatched line remains. Nothing is changed.
    #[error("statement does not close: off by {difference:?} with {unmatched} unmatched line(s)")]
    StatementDoesNotClose { difference: Money, unmatched: usize },
    /// §10: "A closed statement's lines cannot be re-matched."
    #[error("bank statement {statement_id} is closed")]
    StatementClosed { statement_id: String },
    /// [`Ledger::confirm_proposal`] on a line that is not currently a
    /// pending proposal (already resolved, or never matched at all).
    #[error("bank line {line_id} is not a pending proposal")]
    NotAProposal { line_id: String },
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
        name: "initial ledger schema",
        sql: r#"
CREATE TABLE companies (
    company_id        TEXT PRIMARY KEY,
    display_name      TEXT NOT NULL,
    fiscal_year_start TEXT NOT NULL DEFAULT '01-01',
    realm_id          TEXT,
    created_at        TEXT NOT NULL
) STRICT;

CREATE TABLE accounts (
    company_id      TEXT NOT NULL REFERENCES companies(company_id),
    account_id      TEXT NOT NULL,
    number          TEXT NOT NULL,
    name            TEXT NOT NULL,
    classification  TEXT NOT NULL,
    subtype         TEXT,
    parent_id       TEXT,
    normal_balance  TEXT NOT NULL CHECK (normal_balance IN ('Dr','Cr')),
    is_contra       INTEGER NOT NULL DEFAULT 0,
    is_active       INTEGER NOT NULL DEFAULT 1,
    needs_mapping   INTEGER NOT NULL DEFAULT 0,
    source_ref      TEXT,
    PRIMARY KEY (company_id, account_id)
) STRICT;

CREATE TABLE classes (
    company_id  TEXT NOT NULL REFERENCES companies(company_id),
    class_id    TEXT NOT NULL,
    name        TEXT NOT NULL,
    parent_id   TEXT,
    is_active   INTEGER NOT NULL DEFAULT 1,
    source_ref  TEXT,
    PRIMARY KEY (company_id, class_id)
) STRICT;

CREATE TABLE periods (
    company_id     TEXT NOT NULL REFERENCES companies(company_id),
    period_end     TEXT NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open','closed')),
    closed_at      TEXT,
    closed_by      TEXT,
    tb_snapshot_id TEXT,
    PRIMARY KEY (company_id, period_end)
) STRICT;

CREATE TABLE close_history (
    company_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    at          TEXT NOT NULL,
    actor       TEXT NOT NULL,
    moved_from  TEXT NOT NULL,
    moved_to    TEXT NOT NULL,
    is_reopen   INTEGER NOT NULL DEFAULT 0,
    note        TEXT NOT NULL,
    PRIMARY KEY (company_id, seq)
) STRICT;

CREATE TABLE document_versions (
    company_id    TEXT NOT NULL,
    document_id   TEXT NOT NULL,
    version       INTEGER NOT NULL,
    doc_type      TEXT NOT NULL,
    doc_number    TEXT,
    txn_date      TEXT NOT NULL,
    contact_id    TEXT,
    payload_json  TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    command_id    TEXT NOT NULL,
    source_ref    TEXT,
    PRIMARY KEY (company_id, document_id, version)
) STRICT;

CREATE TABLE journal_entries (
    company_id      TEXT NOT NULL,
    entry_id        TEXT NOT NULL,
    entry_date      TEXT NOT NULL,
    posted_at       TEXT,
    memo            TEXT,
    source_type     TEXT NOT NULL,
    source_id       TEXT NOT NULL,
    source_version  INTEGER NOT NULL,
    reversal_of_id  TEXT,
    is_posted       INTEGER NOT NULL DEFAULT 0,
    is_flagged      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (company_id, entry_id),
    FOREIGN KEY (company_id, source_id, source_version)
        REFERENCES document_versions(company_id, document_id, version)
) STRICT;

CREATE TABLE journal_lines (
    company_id    TEXT NOT NULL,
    entry_id      TEXT NOT NULL,
    line_no       INTEGER NOT NULL,
    account_id    TEXT NOT NULL,
    class_id      TEXT,
    debit_minor   INTEGER NOT NULL DEFAULT 0 CHECK (debit_minor  >= 0),
    credit_minor  INTEGER NOT NULL DEFAULT 0 CHECK (credit_minor >= 0),
    memo          TEXT,
    entity_type   TEXT,
    entity_id     TEXT,
    cleared_at    TEXT,
    CHECK (debit_minor = 0 OR credit_minor = 0),
    CHECK (debit_minor + credit_minor > 0),
    PRIMARY KEY (company_id, entry_id, line_no),
    FOREIGN KEY (company_id, entry_id)
        REFERENCES journal_entries(company_id, entry_id),
    FOREIGN KEY (company_id, account_id)
        REFERENCES accounts(company_id, account_id)
) STRICT;

CREATE INDEX idx_lines_account ON journal_lines(company_id, account_id);
CREATE INDEX idx_entries_date  ON journal_entries(company_id, entry_date);

CREATE TABLE tb_snapshots (
    company_id  TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    as_of       TEXT NOT NULL,
    taken_at    TEXT NOT NULL,
    rows_json   TEXT NOT NULL,
    PRIMARY KEY (company_id, snapshot_id)
) STRICT;

CREATE TABLE oplog (
    company_id   TEXT NOT NULL,
    command_id   TEXT NOT NULL,
    actor_id     TEXT NOT NULL,
    hlc          TEXT NOT NULL,
    kind         TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    applied_at   TEXT NOT NULL,
    PRIMARY KEY (company_id, command_id)
) STRICT;

-- Every entry balances (§4 invariants table): lines are inserted while
-- is_posted = 0, and only the transition to 1 is checked, because that
-- transition is the only moment a report can see the entry.
CREATE TRIGGER trg_journal_entries_balance_on_post
BEFORE UPDATE OF is_posted ON journal_entries
FOR EACH ROW
WHEN NEW.is_posted = 1 AND OLD.is_posted = 0
BEGIN
    SELECT RAISE(ABORT, 'journal entry does not balance')
    WHERE COALESCE((
        SELECT SUM(debit_minor) - SUM(credit_minor)
        FROM journal_lines
        WHERE company_id = NEW.company_id AND entry_id = NEW.entry_id
    ), 1) <> 0;
END;

-- A posted entry is never updated or deleted.
CREATE TRIGGER trg_journal_entries_no_update_when_posted
BEFORE UPDATE ON journal_entries
FOR EACH ROW
WHEN OLD.is_posted = 1
BEGIN
    SELECT RAISE(ABORT, 'posted journal entry is immutable');
END;

CREATE TRIGGER trg_journal_entries_no_delete_when_posted
BEFORE DELETE ON journal_entries
FOR EACH ROW
WHEN OLD.is_posted = 1
BEGIN
    SELECT RAISE(ABORT, 'posted journal entry cannot be deleted');
END;

-- Same for lines, with the single exception of cleared_at (reconciliation
-- state, not accounting content).
CREATE TRIGGER trg_journal_lines_no_edit_when_posted
BEFORE UPDATE ON journal_lines
FOR EACH ROW
WHEN (
    NEW.line_no      IS NOT OLD.line_no OR
    NEW.account_id   IS NOT OLD.account_id OR
    NEW.class_id     IS NOT OLD.class_id OR
    NEW.debit_minor  IS NOT OLD.debit_minor OR
    NEW.credit_minor IS NOT OLD.credit_minor OR
    NEW.memo         IS NOT OLD.memo OR
    NEW.entity_type  IS NOT OLD.entity_type OR
    NEW.entity_id    IS NOT OLD.entity_id
) AND EXISTS (
    SELECT 1 FROM journal_entries e
    WHERE e.company_id = OLD.company_id
      AND e.entry_id   = OLD.entry_id
      AND e.is_posted  = 1
)
BEGIN
    SELECT RAISE(ABORT, 'posted journal line is immutable except cleared_at');
END;

CREATE TRIGGER trg_journal_lines_no_delete_when_posted
BEFORE DELETE ON journal_lines
FOR EACH ROW
WHEN EXISTS (
    SELECT 1 FROM journal_entries e
    WHERE e.company_id = OLD.company_id
      AND e.entry_id   = OLD.entry_id
      AND e.is_posted  = 1
)
BEGIN
    SELECT RAISE(ABORT, 'posted journal line cannot be deleted');
END;
"#,
    },
    Migration {
        version: 2,
        name: "accountant mode: adjusting-entry request queue (§8)",
        sql: r#"
CREATE TABLE adjustment_requests (
    company_id      TEXT NOT NULL,
    request_id      TEXT NOT NULL,
    requested_by    TEXT NOT NULL,
    requested_at    TEXT NOT NULL,
    description     TEXT NOT NULL,
    lines_json      TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('proposed','approved','rejected','posted')),
    decided_by      TEXT,
    decided_at      TEXT,
    decision_note   TEXT,
    posted_entry_id TEXT,
    PRIMARY KEY (company_id, request_id)
) STRICT;
"#,
    },
    Migration {
        version: 3,
        name: "bank statements and lines (§10)",
        sql: r#"
CREATE TABLE bank_statements (
    company_id            TEXT NOT NULL REFERENCES companies(company_id),
    statement_id          TEXT NOT NULL,
    account_id            TEXT NOT NULL,
    period_start          TEXT NOT NULL,
    period_end            TEXT NOT NULL,
    opening_balance_minor INTEGER NOT NULL,
    closing_balance_minor INTEGER NOT NULL,
    imported_at           TEXT NOT NULL,
    closed_at             TEXT,
    PRIMARY KEY (company_id, statement_id),
    FOREIGN KEY (company_id, account_id) REFERENCES accounts(company_id, account_id)
) STRICT;

CREATE TABLE bank_lines (
    company_id       TEXT NOT NULL,
    line_id          TEXT NOT NULL,
    statement_id     TEXT NOT NULL,
    account_id       TEXT NOT NULL,
    posted_on        TEXT NOT NULL,
    amount_minor     INTEGER NOT NULL,
    description      TEXT NOT NULL,
    external_id      TEXT,
    matched_entry_id TEXT,
    matched_line_no  INTEGER,
    match_kind       TEXT CHECK (match_kind IS NULL OR match_kind IN ('exact','settlement','proposed','manual')),
    matched_at       TEXT,
    PRIMARY KEY (company_id, line_id),
    FOREIGN KEY (company_id, statement_id) REFERENCES bank_statements(company_id, statement_id),
    FOREIGN KEY (company_id, account_id) REFERENCES accounts(company_id, account_id),
    FOREIGN KEY (company_id, matched_entry_id) REFERENCES journal_entries(company_id, entry_id)
) STRICT;

-- D15/§10: the same bank/card feed is re-imported often (statements overlap
-- at the edges); this is what makes a re-import harmless. Scoped to the
-- account rather than the statement, since a duplicate line is a duplicate
-- regardless of which statement's window it fell into.
CREATE UNIQUE INDEX idx_bank_lines_external
    ON bank_lines(company_id, account_id, external_id)
    WHERE external_id IS NOT NULL;

CREATE INDEX idx_bank_lines_statement ON bank_lines(company_id, statement_id);
"#,
    },
    Migration {
        version: 4,
        name: "line-level taxability on journal_lines (§9 line E)",
        sql: r#"
ALTER TABLE journal_lines ADD COLUMN is_taxable INTEGER NOT NULL DEFAULT 0;
ALTER TABLE journal_lines ADD COLUMN tax_amount_minor INTEGER;
ALTER TABLE journal_lines ADD COLUMN tax_rate TEXT;

-- Migration 1's immutability trigger predates these columns; replaced here
-- (never edited in place, D "never edit one that has shipped") so a posted
-- line's tax fields are covered by the same immutability guarantee as
-- everything but cleared_at.
DROP TRIGGER trg_journal_lines_no_edit_when_posted;

CREATE TRIGGER trg_journal_lines_no_edit_when_posted
BEFORE UPDATE ON journal_lines
FOR EACH ROW
WHEN (
    NEW.line_no          IS NOT OLD.line_no OR
    NEW.account_id       IS NOT OLD.account_id OR
    NEW.class_id         IS NOT OLD.class_id OR
    NEW.debit_minor      IS NOT OLD.debit_minor OR
    NEW.credit_minor     IS NOT OLD.credit_minor OR
    NEW.memo             IS NOT OLD.memo OR
    NEW.entity_type      IS NOT OLD.entity_type OR
    NEW.entity_id        IS NOT OLD.entity_id OR
    NEW.is_taxable       IS NOT OLD.is_taxable OR
    NEW.tax_amount_minor IS NOT OLD.tax_amount_minor OR
    NEW.tax_rate         IS NOT OLD.tax_rate
) AND EXISTS (
    SELECT 1 FROM journal_entries e
    WHERE e.company_id = OLD.company_id
      AND e.entry_id   = OLD.entry_id
      AND e.is_posted  = 1
)
BEGIN
    SELECT RAISE(ABORT, 'posted journal line is immutable except cleared_at');
END;
"#,
    },
];

pub fn latest_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

/// One row of `accounts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub account_id: AccountId,
    pub number: String,
    pub name: String,
    pub classification: Classification,
    pub subtype: Option<String>,
    pub parent_id: Option<String>,
    pub normal_balance: Side,
    pub is_contra: bool,
    pub is_active: bool,
    pub needs_mapping: bool,
    pub source_ref: Option<String>,
}

/// One row of `classes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Class {
    pub class_id: ClassId,
    pub name: String,
    pub parent_id: Option<String>,
    pub is_active: bool,
    pub source_ref: Option<String>,
}

/// What [`Ledger::save_document`] records about the command that produced a
/// document version. `command_id` itself is generated by the store, so a
/// replay never has to invent one (§4 "Commands and the oplog").
pub struct CommandMeta {
    pub actor_id: String,
    pub kind: String,
    pub hlc: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentVersionId {
    pub document_id: String,
    pub version: i64,
}

pub type EntryId = String;

// ---------------------------------------------------------------------------
// Accountant mode (§8): the adjustment-request queue, and read helpers for
// the general ledger detail and audit trail. `crate::accountant` owns the
// business rules (balance and class-rule validation, the propose/decide state
// machine); this module owns only the table and the raw reads/writes, same
// discipline as every other table above.
// ---------------------------------------------------------------------------

/// `adjustment_requests.state`. `Approved` is in the `CHECK` constraint for a
/// future two-step approve-then-post split; today [`crate::accountant`]'s
/// approve path goes straight through to `Posted`, since approving an
/// accountant's request *is* posting it (§8).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AdjustmentState {
    Proposed,
    Approved,
    Rejected,
    Posted,
}

impl AdjustmentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            AdjustmentState::Proposed => "proposed",
            AdjustmentState::Approved => "approved",
            AdjustmentState::Rejected => "rejected",
            AdjustmentState::Posted => "posted",
        }
    }

    pub fn parse(raw: &str) -> Option<AdjustmentState> {
        match raw {
            "proposed" => Some(AdjustmentState::Proposed),
            "approved" => Some(AdjustmentState::Approved),
            "rejected" => Some(AdjustmentState::Rejected),
            "posted" => Some(AdjustmentState::Posted),
            _ => None,
        }
    }
}

/// One row of `adjustment_requests`, exactly as stored. `lines_json` is the
/// caller's problem to deserialise (it is a `Vec<crate::types::JournalLine>`
/// in practice, but this module does not depend on that shape).
#[derive(Clone, Debug, PartialEq)]
pub struct AdjustmentRequestRow {
    pub request_id: String,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub description: String,
    pub lines_json: String,
    pub state: AdjustmentState,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    pub posted_entry_id: Option<String>,
}

/// One row of `close_history`, for the accountant's read-only close-history
/// view (§8 "Close history log").
#[derive(Clone, Debug, PartialEq)]
pub struct CloseHistoryRow {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub moved_from: String,
    pub moved_to: String,
    pub is_reopen: bool,
    pub note: String,
}

/// A posted entry together with who posted it and when, read off the oplog
/// row that produced its source document (§8 "Audit trail").
#[derive(Clone, Debug, PartialEq)]
pub struct PostedEntryWithActor {
    pub entry_id: EntryId,
    pub entry: JournalEntry,
    pub actor_id: String,
    pub command_kind: String,
    pub command_at: DateTime<Utc>,
}

/// One posted `journal_lines` row joined to its account and entry, for the
/// GL detail report (§8 "GL detail"). Ordered by [`Ledger::gl_lines`] so that
/// a caller can compute a running balance per account in one pass.
#[derive(Clone, Debug, PartialEq)]
pub struct GlLineRow {
    pub account_id: AccountId,
    pub number: String,
    pub name: String,
    pub entry_id: EntryId,
    pub entry_date: NaiveDate,
    pub source_type: DocKind,
    pub source_id: String,
    pub line_memo: Option<String>,
    pub entry_memo: Option<String>,
    pub class_id: Option<ClassId>,
    pub entity: Option<ContactRef>,
    pub is_flagged: bool,
    pub reversal_of: Option<String>,
    pub debit: Money,
    pub credit: Money,
}

/// One row of `bank_statements` (§10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankStatementRow {
    pub statement_id: String,
    pub account_id: AccountId,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    pub opening_balance: Money,
    pub closing_balance: Money,
    pub imported_at: String,
    pub closed_at: Option<String>,
}

/// One row of `bank_lines` (§10). `match_kind` is `None` until
/// [`Ledger::match_statement`] has run at least once over the statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BankLineRow {
    pub line_id: String,
    pub statement_id: String,
    pub account_id: AccountId,
    pub posted_on: NaiveDate,
    pub amount: Money,
    pub description: String,
    pub external_id: Option<String>,
    pub matched_entry_id: Option<String>,
    pub matched_line_no: Option<i64>,
    pub match_kind: Option<String>,
    pub matched_at: Option<String>,
}

/// The ledger. Owns the connection; nothing outside this module can reach it.
pub struct Ledger {
    connection: Connection,
    /// Consulted by the importer/outbox: a replay in progress suppresses
    /// outbox enqueueing (§4 "How that interacts with the outbox"). Plain
    /// in-memory state, not persisted — a fresh process starts not replaying.
    replay_marker: AtomicBool,
}

impl Ledger {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, LedgerError> {
        let connection = Connection::open(path)?;
        Self::configure(&connection)?;
        let mut ledger = Ledger {
            connection,
            replay_marker: AtomicBool::new(false),
        };
        ledger.migrate()?;
        Ok(ledger)
    }

    /// In-memory, for tests. WAL does not apply to a memory database.
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        let mut ledger = Ledger {
            connection,
            replay_marker: AtomicBool::new(false),
        };
        ledger.migrate()?;
        Ok(ledger)
    }

    fn configure(connection: &Connection) -> Result<(), LedgerError> {
        // journal_mode returns a row, so it cannot go through execute_batch.
        let _: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = FULL;",
        )?;
        Ok(())
    }

    fn migrate(&mut self) -> Result<(), LedgerError> {
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
            return Err(LedgerError::SchemaTooNew {
                found: current,
                supported: latest_version(),
            });
        }

        for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
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

    pub fn schema_version(&self) -> Result<i64, LedgerError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?)
    }

    /// Read access for [`crate::report`], which is read-only by construction
    /// and queries this same connection directly rather than duplicating
    /// every accessor here.
    pub(crate) fn conn(&self) -> &Connection {
        &self.connection
    }

    pub fn is_replaying(&self) -> bool {
        self.replay_marker.load(Ordering::Relaxed)
    }

    pub fn set_replaying(&self, replaying: bool) {
        self.replay_marker.store(replaying, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Chart of accounts and classes (§2, §3)
    // -----------------------------------------------------------------------

    /// Seeds a company with [`chart::seed_chart`] and its class list.
    /// `"aquamentor"` gets [`chart::AQUAMENTOR_CLASSES`], `"waterline"` gets
    /// [`chart::WATERLINE_CLASSES`]; any other id gets the Aquamentor list.
    ///
    /// Idempotent: a second call for a `company` id that already exists
    /// updates `display_name` and `realm_id` (never `created_at`) rather than
    /// erroring, and every seeded account and class is inserted only if it is
    /// not already there — an existing row's `source_ref`, `is_contra`, or
    /// anything else a prior `ledger import` run may have set is never
    /// touched by a re-run of `init`.
    pub fn create_company(
        &self,
        company: &str,
        display_name: &str,
        realm_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let tx = self.connection.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO companies (company_id, display_name, realm_id, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(company_id) DO UPDATE SET
                 display_name = excluded.display_name,
                 realm_id     = excluded.realm_id",
            params![company, display_name, realm_id, now.to_rfc3339()],
        )?;

        for seed in chart::seed_chart() {
            let side = if seed.is_contra {
                flip(seed.classification.normal_balance())
            } else {
                seed.classification.normal_balance()
            };
            tx.execute(
                "INSERT INTO accounts
                     (company_id, account_id, number, name, classification, subtype,
                      parent_id, normal_balance, is_contra, is_active, needs_mapping, source_ref)
                 VALUES (?1, ?2, ?2, ?3, ?4, NULL, NULL, ?5, ?6, 1, 0, NULL)
                 ON CONFLICT(company_id, account_id) DO NOTHING",
                params![
                    company,
                    seed.number,
                    seed.name,
                    seed.classification.as_str(),
                    side_str(side),
                    i64::from(seed.is_contra),
                ],
            )?;
        }

        let classes = match company {
            "aquamentor" => chart::AQUAMENTOR_CLASSES,
            "waterline" => chart::WATERLINE_CLASSES,
            _ => chart::AQUAMENTOR_CLASSES,
        };
        for (id, name) in classes {
            tx.execute(
                "INSERT INTO classes (company_id, class_id, name, parent_id, is_active, source_ref)
                 VALUES (?1, ?2, ?3, NULL, 1, NULL)
                 ON CONFLICT(company_id, class_id) DO NOTHING",
                params![company, id, name],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_account(
        &self,
        company: &str,
        number: &str,
        name: &str,
        classification: Classification,
        is_contra: bool,
        source_ref: Option<&str>,
        needs_mapping: bool,
    ) -> Result<(), LedgerError> {
        let side = if is_contra {
            flip(classification.normal_balance())
        } else {
            classification.normal_balance()
        };
        self.connection.execute(
            "INSERT INTO accounts
                 (company_id, account_id, number, name, classification, subtype,
                  parent_id, normal_balance, is_contra, is_active, needs_mapping, source_ref)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, NULL, ?5, ?6, 1, ?7, ?8)
             ON CONFLICT(company_id, account_id) DO UPDATE SET
                 name           = excluded.name,
                 classification = excluded.classification,
                 normal_balance = excluded.normal_balance,
                 is_contra      = excluded.is_contra,
                 needs_mapping  = excluded.needs_mapping,
                 source_ref     = excluded.source_ref",
            params![
                company,
                number,
                name,
                classification.as_str(),
                side_str(side),
                i64::from(is_contra),
                i64::from(needs_mapping),
                source_ref,
            ],
        )?;
        Ok(())
    }

    pub fn add_class(
        &self,
        company: &str,
        id: &str,
        name: &str,
        source_ref: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.connection.execute(
            "INSERT INTO classes (company_id, class_id, name, parent_id, is_active, source_ref)
             VALUES (?1, ?2, ?3, NULL, 1, ?4)
             ON CONFLICT(company_id, class_id) DO UPDATE SET
                 name = excluded.name, source_ref = excluded.source_ref",
            params![company, id, name, source_ref],
        )?;
        Ok(())
    }

    pub fn account_by_source_ref(
        &self,
        company: &str,
        source_ref: &str,
    ) -> Result<Option<Account>, LedgerError> {
        self.connection
            .query_row(
                &format!(
                    "{ACCOUNT_COLUMNS} FROM accounts WHERE company_id = ?1 AND source_ref = ?2"
                ),
                params![company, source_ref],
                row_to_account,
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Record the QBO account id on a seed-chart account the importer mapped
    /// onto it, so the §7 trial balance diff can join the standard accounts
    /// (1200, 2200, ...) to QBO's rows. Sets only where nothing is recorded:
    /// a source ref, once known, is never overwritten (§2, kept forever).
    /// Returns whether a value was written.
    pub fn set_account_source_ref(
        &self,
        company: &str,
        number: &str,
        source_ref: &str,
    ) -> Result<bool, LedgerError> {
        let changed = self.connection.execute(
            "UPDATE accounts SET source_ref = ?3
             WHERE company_id = ?1 AND number = ?2 AND source_ref IS NULL",
            params![company, number, source_ref],
        )?;
        Ok(changed == 1)
    }

    pub fn list_accounts(&self, company: &str) -> Result<Vec<Account>, LedgerError> {
        let mut stmt = self.connection.prepare(&format!(
            "{ACCOUNT_COLUMNS} FROM accounts WHERE company_id = ?1 ORDER BY number"
        ))?;
        let rows = stmt
            .query_map(params![company], row_to_account)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        Ok(rows)
    }

    pub fn list_classes(&self, company: &str) -> Result<Vec<Class>, LedgerError> {
        let mut stmt = self.connection.prepare(&format!(
            "{CLASS_COLUMNS} FROM classes WHERE company_id = ?1 ORDER BY class_id"
        ))?;
        let rows = stmt
            .query_map(params![company], row_to_class)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Period gate (§5, D7)
    // -----------------------------------------------------------------------

    pub fn locked_through(&self, company: &str) -> Result<Option<NaiveDate>, LedgerError> {
        locked_through_in(&self.connection, company)
    }

    pub fn close_period(
        &self,
        company: &str,
        period_end: NaiveDate,
        actor: &str,
        note: &str,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let snapshot_id = self.snapshot_trial_balance(company, period_end, now)?;
        let tx = self.connection.unchecked_transaction()?;

        let moved_from = locked_through_in(&tx, company)?;

        tx.execute(
            "INSERT INTO periods (company_id, period_end, state, closed_at, closed_by, tb_snapshot_id)
             VALUES (?1, ?2, 'closed', ?3, ?4, ?5)
             ON CONFLICT(company_id, period_end) DO UPDATE SET
                 state = 'closed', closed_at = excluded.closed_at,
                 closed_by = excluded.closed_by, tb_snapshot_id = excluded.tb_snapshot_id",
            params![company, period_end.to_string(), now.to_rfc3339(), actor, snapshot_id],
        )?;

        let seq = next_close_history_seq(&tx, company)?;
        tx.execute(
            "INSERT INTO close_history (company_id, seq, at, actor, moved_from, moved_to, is_reopen, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![
                company,
                seq,
                now.to_rfc3339(),
                actor,
                moved_from.map(|d| d.to_string()).unwrap_or_else(|| "none".to_string()),
                period_end.to_string(),
                note,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Never quiet (D7): always writes an `is_reopen = 1` `close_history` row.
    pub fn reopen_period(
        &self,
        company: &str,
        period_end: NaiveDate,
        actor: &str,
        note: &str,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let tx = self.connection.unchecked_transaction()?;

        let was_closed: i64 = tx.query_row(
            "SELECT COUNT(*) FROM periods
             WHERE company_id = ?1 AND period_end = ?2 AND state = 'closed'",
            params![company, period_end.to_string()],
            |row| row.get(0),
        )?;
        if was_closed == 0 {
            return Err(LedgerError::NotFound);
        }

        tx.execute(
            "UPDATE periods SET state = 'open' WHERE company_id = ?1 AND period_end = ?2",
            params![company, period_end.to_string()],
        )?;

        let moved_to = locked_through_in(&tx, company)?
            .map(|d| d.to_string())
            .unwrap_or_else(|| "none".to_string());

        let seq = next_close_history_seq(&tx, company)?;
        tx.execute(
            "INSERT INTO close_history (company_id, seq, at, actor, moved_from, moved_to, is_reopen, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
            params![company, seq, now.to_rfc3339(), actor, period_end.to_string(), moved_to, note],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Stores the current trial balance as of `as_of` in `tb_snapshots` and
    /// returns the snapshot id (§5 close checklist item 6).
    pub fn snapshot_trial_balance(
        &self,
        company: &str,
        as_of: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<String, LedgerError> {
        let tb = crate::report::trial_balance(self, company, as_of)?;
        let snapshot_id = Uuid::now_v7().to_string();
        let rows: Vec<serde_json::Value> = tb
            .rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "account_id": row.account_id.0,
                    "number": row.number,
                    "name": row.name,
                    "debit": row.debit.minor(),
                    "credit": row.credit.minor(),
                    "balance": row.balance.minor(),
                })
            })
            .collect();
        let rows_json = serde_json::to_string(&rows)?;

        self.connection.execute(
            "INSERT INTO tb_snapshots (company_id, snapshot_id, as_of, taken_at, rows_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company,
                snapshot_id,
                as_of.to_string(),
                now.to_rfc3339(),
                rows_json
            ],
        )?;
        Ok(snapshot_id)
    }

    // -----------------------------------------------------------------------
    // Documents and the oplog (§4 "Commands and the oplog")
    // -----------------------------------------------------------------------

    pub fn save_document(
        &self,
        company: &str,
        doc: &LedgerDocument,
        command: CommandMeta,
        now: DateTime<Utc>,
    ) -> Result<DocumentVersionId, LedgerError> {
        let tx = self.connection.unchecked_transaction()?;

        let previous_version: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM document_versions
             WHERE company_id = ?1 AND document_id = ?2",
            params![company, doc.document_id],
            |row| row.get(0),
        )?;
        let version = previous_version + 1;

        let command_id = Uuid::now_v7().to_string();
        let payload = serde_json::to_string(doc)?;

        tx.execute(
            "INSERT INTO oplog (company_id, command_id, actor_id, hlc, kind, payload_json, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                company,
                command_id,
                command.actor_id,
                command.hlc,
                command.kind,
                payload,
                now.to_rfc3339(),
            ],
        )?;

        tx.execute(
            "INSERT INTO document_versions
                 (company_id, document_id, version, doc_type, doc_number, txn_date,
                  contact_id, payload_json, created_at, command_id, source_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                company,
                doc.document_id,
                version,
                doc.kind.as_str(),
                doc.number,
                doc.txn_date.to_string(),
                doc.contact.as_ref().map(|c| c.id.clone()),
                payload,
                now.to_rfc3339(),
                command_id,
                doc.source_ref,
            ],
        )?;

        tx.commit()?;
        Ok(DocumentVersionId {
            document_id: doc.document_id.clone(),
            version,
        })
    }

    /// The `payload_json` of the newest saved version of `document_id`, or
    /// `None` when the document has never been saved. `crate::pipeline`'s
    /// re-run check (`LEDGER-DESIGN.md` §6: "re-run replaces derived entries")
    /// compares this against the freshly translated document to decide
    /// whether anything actually changed before reversing and reposting.
    pub fn latest_document_payload(
        &self,
        company: &str,
        document_id: &str,
    ) -> Result<Option<String>, LedgerError> {
        self.connection
            .query_row(
                "SELECT payload_json FROM document_versions
                 WHERE company_id = ?1 AND document_id = ?2
                 ORDER BY version DESC LIMIT 1",
                params![company, document_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// The number of versions saved for `document_id` — most directly useful
    /// for proving a pipeline re-run's skip-when-unchanged path did not add
    /// one (`LEDGER-DESIGN.md` §6).
    pub fn document_version_count(
        &self,
        company: &str,
        document_id: &str,
    ) -> Result<i64, LedgerError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM document_versions WHERE company_id = ?1 AND document_id = ?2",
            params![company, document_id],
            |row| row.get(0),
        )?)
    }

    pub fn oplog_len(&self, company: &str) -> Result<i64, LedgerError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM oplog WHERE company_id = ?1",
            params![company],
            |row| row.get(0),
        )?)
    }

    // -----------------------------------------------------------------------
    // Posting (§1, §3, §4, §5)
    // -----------------------------------------------------------------------

    /// The only writer to `journal_entries`/`journal_lines`. Checks the
    /// period gate on `entry.entry_date`, that every account and class
    /// exist, and the §3 class rule, then inserts the entry unposted and
    /// flips it posted so the balance trigger fires. `entry.source_id` /
    /// `source_version` must reference an existing `document_versions` row
    /// — enforced by the composite foreign key, not re-checked here.
    pub fn post_entry(
        &self,
        company: &str,
        entry: &JournalEntry,
        now: DateTime<Utc>,
    ) -> Result<EntryId, LedgerError> {
        if let Some(locked) = self.locked_through(company)? {
            if entry.entry_date <= locked {
                return Err(LedgerError::PeriodClosed { period_end: locked });
            }
        }

        if !entry.is_balanced() {
            return Err(LedgerError::Unbalanced);
        }

        let tx = self.connection.unchecked_transaction()?;

        for line in &entry.lines {
            check_class_rule(&tx, company, line)?;
        }

        let entry_id = Uuid::now_v7().to_string();

        tx.execute(
            "INSERT INTO journal_entries
                 (company_id, entry_id, entry_date, posted_at, memo, source_type,
                  source_id, source_version, reversal_of_id, is_posted, is_flagged)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
            params![
                company,
                entry_id,
                entry.entry_date.to_string(),
                entry.memo,
                entry.source_type.as_str(),
                entry.source_id,
                entry.source_version,
                entry.reversal_of,
                i64::from(entry.is_flagged),
            ],
        )?;

        for line in &entry.lines {
            tx.execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor,
                      credit_minor, memo, entity_type, entity_id, cleared_at,
                      is_taxable, tax_amount_minor, tax_rate)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, ?13)",
                params![
                    company,
                    entry_id,
                    line.line_no,
                    line.account.0,
                    line.class.as_ref().map(|c| c.0.clone()),
                    line.debit.minor(),
                    line.credit.minor(),
                    line.memo,
                    line.entity.as_ref().map(|e| contact_kind_str(e.kind)),
                    line.entity.as_ref().map(|e| e.id.clone()),
                    i64::from(line.is_taxable),
                    line.tax_amount.map(|amount| amount.minor()),
                    line.tax_rate.map(|rate| rate.to_string()),
                ],
            )?;
        }

        tx.execute(
            "UPDATE journal_entries SET is_posted = 1, posted_at = ?3
             WHERE company_id = ?1 AND entry_id = ?2",
            params![company, entry_id, now.to_rfc3339()],
        )?;

        tx.commit()?;
        Ok(entry_id)
    }

    /// The only correction path (§4): a new entry with `reversal_of` set and
    /// the sides swapped, posted through [`Ledger::post_entry`] so the
    /// period gate applies to the reversal date, not the original.
    pub fn reverse_entry(
        &self,
        company: &str,
        entry_id: &str,
        on: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<EntryId, LedgerError> {
        let (original, is_posted) = self.entry(company, entry_id)?;
        if !is_posted {
            return Err(LedgerError::NotFound);
        }
        let reversal = original.reversed(on, entry_id);
        self.post_entry(company, &reversal, now)
    }

    pub fn entry(
        &self,
        company: &str,
        entry_id: &str,
    ) -> Result<(JournalEntry, bool), LedgerError> {
        let row: Option<EntryHeaderRow> = self
            .connection
            .query_row(
                "SELECT entry_date, memo, source_type, source_id, source_version,
                        reversal_of_id, is_posted, is_flagged
                 FROM journal_entries WHERE company_id = ?1 AND entry_id = ?2",
                params![company, entry_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;

        let (
            entry_date,
            memo,
            source_type,
            source_id,
            source_version,
            reversal_of,
            is_posted,
            is_flagged,
        ) = row.ok_or(LedgerError::NotFound)?;

        let entry_date = parse_date(&entry_date)?;
        let source_type = DocKind::parse(&source_type).ok_or(LedgerError::NotFound)?;

        let mut stmt = self.connection.prepare(
            "SELECT line_no, account_id, class_id, debit_minor, credit_minor, memo,
                    entity_type, entity_id, is_taxable, tax_amount_minor, tax_rate
             FROM journal_lines WHERE company_id = ?1 AND entry_id = ?2 ORDER BY line_no",
        )?;
        let lines = stmt
            .query_map(params![company, entry_id], row_to_journal_line)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        Ok((
            JournalEntry {
                entry_date,
                memo,
                source_type,
                source_id,
                source_version,
                reversal_of,
                is_flagged: is_flagged != 0,
                lines,
            },
            is_posted != 0,
        ))
    }

    pub fn entries_for_document(
        &self,
        company: &str,
        document_id: &str,
    ) -> Result<Vec<(EntryId, JournalEntry, bool)>, LedgerError> {
        let mut stmt = self.connection.prepare(
            "SELECT entry_id FROM journal_entries
             WHERE company_id = ?1 AND source_id = ?2
             ORDER BY entry_date, entry_id",
        )?;
        let ids = stmt
            .query_map(params![company, document_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        ids.into_iter()
            .map(|id| {
                let (entry, posted) = self.entry(company, &id)?;
                Ok((id, entry, posted))
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Accountant mode (§8): adjustment requests, close history, GL and audit
    // reads. Business rules live in `crate::accountant`; these are plain CRUD
    // and joins, same as the rest of this module.
    // -----------------------------------------------------------------------

    /// Inserts a new request in state `'proposed'`. `request_id` is generated
    /// by the caller (a UUIDv7, matching every other id in this file).
    pub fn insert_adjustment_request(
        &self,
        company: &str,
        request_id: &str,
        requested_by: &str,
        requested_at: DateTime<Utc>,
        description: &str,
        lines_json: &str,
    ) -> Result<(), LedgerError> {
        self.connection.execute(
            "INSERT INTO adjustment_requests
                 (company_id, request_id, requested_by, requested_at, description,
                  lines_json, state, decided_by, decided_at, decision_note, posted_entry_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'proposed', NULL, NULL, NULL, NULL)",
            params![
                company,
                request_id,
                requested_by,
                requested_at.to_rfc3339(),
                description,
                lines_json,
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Bank statements and lines (§10) — schema and plain reads/writes only.
    // The matching rules, parsers and the per-statement close live in
    // `crate::bank`, built on top of these.
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn create_bank_statement(
        &self,
        company: &str,
        statement_id: &str,
        account: &AccountId,
        period_start: NaiveDate,
        period_end: NaiveDate,
        opening_balance: Money,
        closing_balance: Money,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        self.connection.execute(
            "INSERT INTO bank_statements
                 (company_id, statement_id, account_id, period_start, period_end,
                  opening_balance_minor, closing_balance_minor, imported_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
            params![
                company,
                statement_id,
                account.0,
                period_start.to_string(),
                period_end.to_string(),
                opening_balance.minor(),
                closing_balance.minor(),
                now.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn adjustment_request(
        &self,
        company: &str,
        request_id: &str,
    ) -> Result<Option<AdjustmentRequestRow>, LedgerError> {
        self.connection
            .query_row(
                &format!(
                    "{ADJUSTMENT_COLUMNS} FROM adjustment_requests \
                     WHERE company_id = ?1 AND request_id = ?2"
                ),
                params![company, request_id],
                row_to_adjustment_request,
            )
            .optional()
            .map_err(LedgerError::from)
    }

    pub fn list_adjustment_requests(
        &self,
        company: &str,
        state: Option<AdjustmentState>,
    ) -> Result<Vec<AdjustmentRequestRow>, LedgerError> {
        match state {
            Some(state) => {
                let mut stmt = self.connection.prepare(&format!(
                    "{ADJUSTMENT_COLUMNS} FROM adjustment_requests \
                     WHERE company_id = ?1 AND state = ?2 ORDER BY requested_at, request_id"
                ))?;
                let rows = stmt
                    .query_map(params![company, state.as_str()], row_to_adjustment_request)?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok(rows)
            }
            None => {
                let mut stmt = self.connection.prepare(&format!(
                    "{ADJUSTMENT_COLUMNS} FROM adjustment_requests \
                     WHERE company_id = ?1 ORDER BY requested_at, request_id"
                ))?;
                let rows = stmt
                    .query_map(params![company], row_to_adjustment_request)?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok(rows)
            }
        }
    }

    /// Records a decision. Plain CRUD: it does not check that the request was
    /// still `'proposed'` — the caller (`crate::accountant::decide_adjustment`)
    /// reads the row first and refuses a second decision itself, so that
    /// "already decided" is one clear error rather than a silent overwrite.
    #[allow(clippy::too_many_arguments)]
    pub fn decide_adjustment_request(
        &self,
        company: &str,
        request_id: &str,
        decided_by: &str,
        decided_at: DateTime<Utc>,
        note: &str,
        new_state: AdjustmentState,
        posted_entry_id: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.connection.execute(
            "UPDATE adjustment_requests
             SET state = ?3, decided_by = ?4, decided_at = ?5, decision_note = ?6, posted_entry_id = ?7
             WHERE company_id = ?1 AND request_id = ?2",
            params![
                company,
                request_id,
                new_state.as_str(),
                decided_by,
                decided_at.to_rfc3339(),
                note,
                posted_entry_id,
            ],
        )?;
        Ok(())
    }

    /// The full close history, oldest first, reopens flagged (§5, §8 "Close
    /// history log": readable but not writable through this method).
    pub fn close_history(&self, company: &str) -> Result<Vec<CloseHistoryRow>, LedgerError> {
        let mut stmt = self.connection.prepare(
            "SELECT seq, at, actor, moved_from, moved_to, is_reopen, note
             FROM close_history WHERE company_id = ?1 ORDER BY seq",
        )?;
        let raw = stmt
            .query_map(params![company], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        raw.into_iter()
            .map(|(seq, at, actor, moved_from, moved_to, is_reopen, note)| {
                Ok(CloseHistoryRow {
                    seq,
                    at: parse_datetime(&at)?,
                    actor,
                    moved_from,
                    moved_to,
                    is_reopen: is_reopen != 0,
                    note,
                })
            })
            .collect()
    }

    /// Every posted entry dated after `since`, each with the actor and
    /// command that produced its source document, flagged entries first
    /// (§8 "Audit trail"). `since` is normally the last close's period end;
    /// [`crate::accountant::audit_trail`] works out the default.
    pub fn posted_entries_since(
        &self,
        company: &str,
        since: NaiveDate,
    ) -> Result<Vec<PostedEntryWithActor>, LedgerError> {
        let mut stmt = self.connection.prepare(
            "SELECT e.entry_id, o.actor_id, o.kind, o.applied_at
             FROM journal_entries e
             JOIN document_versions dv
                 ON dv.company_id = e.company_id
                AND dv.document_id = e.source_id
                AND dv.version = e.source_version
             JOIN oplog o ON o.company_id = dv.company_id AND o.command_id = dv.command_id
             WHERE e.company_id = ?1 AND e.is_posted = 1 AND e.entry_date > ?2
             ORDER BY e.is_flagged DESC, e.entry_date, e.entry_id",
        )?;
        let headers = stmt
            .query_map(params![company, since.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        headers
            .into_iter()
            .map(|(entry_id, actor_id, command_kind, applied_at)| {
                let (entry, _is_posted) = self.entry(company, &entry_id)?;
                Ok(PostedEntryWithActor {
                    entry_id,
                    entry,
                    actor_id,
                    command_kind,
                    command_at: parse_datetime(&applied_at)?,
                })
            })
            .collect()
    }

    /// Every posted `journal_lines` row with `entry_date <= to`, ordered by
    /// account number then date then entry then line — everything
    /// [`crate::accountant::general_ledger`] needs to compute an opening
    /// balance (lines before its `from`) and a running balance per account
    /// in one pass, with no second query.
    pub fn gl_lines(
        &self,
        company: &str,
        to: NaiveDate,
        account: Option<&AccountId>,
    ) -> Result<Vec<GlLineRow>, LedgerError> {
        const COLUMNS: &str = "a.account_id, a.number, a.name, e.entry_id, e.entry_date, \
            e.source_type, e.source_id, l.memo, e.memo, l.class_id, l.entity_type, \
            l.entity_id, e.is_flagged, e.reversal_of_id, l.debit_minor, l.credit_minor";
        const JOIN: &str = "FROM journal_lines l \
            JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id \
            JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id";
        const ORDER: &str = " ORDER BY a.number, e.entry_date, e.entry_id, l.line_no";

        let rows = if let Some(account_id) = account {
            let sql = format!(
                "SELECT {COLUMNS} {JOIN} \
                 WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date <= ?2 \
                   AND a.account_id = ?3{ORDER}"
            );
            let mut stmt = self.connection.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![company, to.to_string(), account_id.0],
                    row_to_gl_line,
                )?
                .collect::<Result<Vec<_>, rusqlite::Error>>()?;
            rows
        } else {
            let sql = format!(
                "SELECT {COLUMNS} {JOIN} \
                 WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date <= ?2{ORDER}"
            );
            let mut stmt = self.connection.prepare(&sql)?;
            let rows = stmt
                .query_map(params![company, to.to_string()], row_to_gl_line)?
                .collect::<Result<Vec<_>, rusqlite::Error>>()?;
            rows
        };
        Ok(rows)
    }

    pub fn bank_statement(
        &self,
        company: &str,
        statement_id: &str,
    ) -> Result<BankStatementRow, LedgerError> {
        self.connection
            .query_row(
                "SELECT statement_id, account_id, period_start, period_end,
                        opening_balance_minor, closing_balance_minor, imported_at, closed_at
                 FROM bank_statements WHERE company_id = ?1 AND statement_id = ?2",
                params![company, statement_id],
                row_to_bank_statement,
            )
            .optional()?
            .ok_or(LedgerError::NotFound)
    }

    pub fn close_bank_statement(
        &self,
        company: &str,
        statement_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let changed = self.connection.execute(
            "UPDATE bank_statements SET closed_at = ?3
             WHERE company_id = ?1 AND statement_id = ?2",
            params![company, statement_id, now.to_rfc3339()],
        )?;
        if changed == 0 {
            return Err(LedgerError::NotFound);
        }
        Ok(())
    }

    /// Inserts one bank line unless `(account, external_id)` already exists
    /// for this company (§10: "what makes re-importing the same file
    /// harmless"). `external_id` of `None` never dedupes — every line
    /// without one is inserted. Returns whether it was actually inserted.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_bank_line_if_new(
        &self,
        company: &str,
        line_id: &str,
        statement_id: &str,
        account: &AccountId,
        posted_on: NaiveDate,
        amount: Money,
        description: &str,
        external_id: Option<&str>,
    ) -> Result<bool, LedgerError> {
        if let Some(external_id) = external_id {
            let exists: i64 = self.connection.query_row(
                "SELECT COUNT(*) FROM bank_lines
                 WHERE company_id = ?1 AND account_id = ?2 AND external_id = ?3",
                params![company, account.0, external_id],
                |row| row.get(0),
            )?;
            if exists > 0 {
                return Ok(false);
            }
        }
        self.connection.execute(
            "INSERT INTO bank_lines
                 (company_id, line_id, statement_id, account_id, posted_on, amount_minor,
                  description, external_id, matched_entry_id, matched_line_no, match_kind,
                  matched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, NULL)",
            params![
                company,
                line_id,
                statement_id,
                account.0,
                posted_on.to_string(),
                amount.minor(),
                description,
                external_id,
            ],
        )?;
        Ok(true)
    }

    pub fn list_bank_lines(
        &self,
        company: &str,
        statement_id: &str,
    ) -> Result<Vec<BankLineRow>, LedgerError> {
        let mut stmt = self.connection.prepare(
            "SELECT line_id, statement_id, account_id, posted_on, amount_minor, description,
                    external_id, matched_entry_id, matched_line_no, match_kind, matched_at
             FROM bank_lines WHERE company_id = ?1 AND statement_id = ?2
             ORDER BY posted_on, line_id",
        )?;
        let rows = stmt
            .query_map(params![company, statement_id], row_to_bank_line)?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        Ok(rows)
    }

    pub fn bank_line(&self, company: &str, line_id: &str) -> Result<BankLineRow, LedgerError> {
        self.connection
            .query_row(
                "SELECT line_id, statement_id, account_id, posted_on, amount_minor, description,
                        external_id, matched_entry_id, matched_line_no, match_kind, matched_at
                 FROM bank_lines WHERE company_id = ?1 AND line_id = ?2",
                params![company, line_id],
                row_to_bank_line,
            )
            .optional()?
            .ok_or(LedgerError::NotFound)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_bank_line_match(
        &self,
        company: &str,
        line_id: &str,
        match_kind: &str,
        entry_id: Option<&str>,
        line_no: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let changed = self.connection.execute(
            "UPDATE bank_lines SET match_kind = ?3, matched_entry_id = ?4,
                    matched_line_no = ?5, matched_at = ?6
             WHERE company_id = ?1 AND line_id = ?2",
            params![
                company,
                line_id,
                match_kind,
                entry_id,
                line_no,
                now.to_rfc3339()
            ],
        )?;
        if changed == 0 {
            return Err(LedgerError::NotFound);
        }
        Ok(())
    }

    /// The one permitted update on a posted line (§4): stamps `cleared_at`.
    pub fn clear_journal_line(
        &self,
        company: &str,
        entry_id: &str,
        line_no: i64,
        now: DateTime<Utc>,
    ) -> Result<(), LedgerError> {
        let changed = self.connection.execute(
            "UPDATE journal_lines SET cleared_at = ?4
             WHERE company_id = ?1 AND entry_id = ?2 AND line_no = ?3",
            params![company, entry_id, line_no, now.to_rfc3339()],
        )?;
        if changed == 0 {
            return Err(LedgerError::NotFound);
        }
        Ok(())
    }

    pub fn is_line_cleared(
        &self,
        company: &str,
        entry_id: &str,
        line_no: i64,
    ) -> Result<bool, LedgerError> {
        let cleared: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT cleared_at FROM journal_lines
                 WHERE company_id = ?1 AND entry_id = ?2 AND line_no = ?3",
                params![company, entry_id, line_no],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(cleared.flatten().is_some())
    }

    /// §10 match rule 2 ("exact"): posted, uncleared lines on `account` at
    /// exactly `side`/`amount`, dated within `[from, to]`, on an entry whose
    /// `source_type` is one of `source_types` — the design's "an unmatched
    /// payment, deposit, bill payment, or purchase", which deliberately
    /// excludes a Settlement entry's own bank leg so rule 3 has something
    /// distinct to match. `source_types` is always a small internal
    /// constant, never external input.
    #[allow(clippy::too_many_arguments)]
    pub fn uncleared_lines_matching(
        &self,
        company: &str,
        account: &AccountId,
        side: Side,
        amount: Money,
        source_types: &[&str],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<(EntryId, i64, NaiveDate)>, LedgerError> {
        if source_types.is_empty() {
            return Ok(Vec::new());
        }
        let column = match side {
            Side::Debit => "debit_minor",
            Side::Credit => "credit_minor",
        };
        let list = source_types
            .iter()
            .map(|kind| format!("'{kind}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT l.entry_id, l.line_no, e.entry_date
             FROM journal_lines l
             JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
             WHERE l.company_id = ?1 AND l.account_id = ?2 AND l.{column} = ?3
               AND l.cleared_at IS NULL AND e.is_posted = 1
               AND e.entry_date BETWEEN ?4 AND ?5 AND e.source_type IN ({list})
             ORDER BY e.entry_date, l.entry_id, l.line_no"
        );
        let mut stmt = self.connection.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    company,
                    account.0,
                    amount.minor(),
                    from.to_string(),
                    to.to_string()
                ],
                |row| {
                    let entry_id: String = row.get(0)?;
                    let line_no: i64 = row.get(1)?;
                    let entry_date: String = row.get(2)?;
                    Ok((entry_id, line_no, entry_date))
                },
            )?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        rows.into_iter()
            .map(|(entry_id, line_no, date)| Ok((entry_id, line_no, parse_date(&date)?)))
            .collect()
    }

    /// §10 match rule 3 ("settlement"): entries of one of `source_types`
    /// whose credit lines on `account` (1160, in practice) sum to `amount`
    /// for the entry, dated within `[from, to]`. `source_types` is always a
    /// small internal constant, never external input.
    pub fn entries_with_credit_sum(
        &self,
        company: &str,
        account: &str,
        amount: Money,
        source_types: &[&str],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<EntryId>, LedgerError> {
        if source_types.is_empty() {
            return Ok(Vec::new());
        }
        let list = source_types
            .iter()
            .map(|kind| format!("'{kind}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT l.entry_id
             FROM journal_lines l
             JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
             WHERE l.company_id = ?1 AND l.account_id = ?2 AND e.is_posted = 1
               AND e.entry_date BETWEEN ?3 AND ?4 AND e.source_type IN ({list})
             GROUP BY l.entry_id
             HAVING SUM(l.credit_minor) = ?5
             ORDER BY l.entry_id"
        );
        let mut stmt = self.connection.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    company,
                    account,
                    from.to_string(),
                    to.to_string(),
                    amount.minor()
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        Ok(rows)
    }
}

/// `entry_date, memo, source_type, source_id, source_version, reversal_of_id,
/// is_posted, is_flagged`, in that order — named here purely to keep
/// [`Ledger::entry`]'s local binding under clippy's type-complexity limit.
type EntryHeaderRow = (
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
    i64,
    i64,
);

const ACCOUNT_COLUMNS: &str = "SELECT account_id, number, name, classification, subtype, \
     parent_id, normal_balance, is_contra, is_active, needs_mapping, source_ref";
const CLASS_COLUMNS: &str = "SELECT class_id, name, parent_id, is_active, source_ref";
const ADJUSTMENT_COLUMNS: &str = "SELECT request_id, requested_by, requested_at, description, \
     lines_json, state, decided_by, decided_at, decision_note, posted_entry_id";

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    let classification_raw: String = row.get(3)?;
    let classification = Classification::parse(&classification_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(3, "classification".into(), rusqlite::types::Type::Text)
    })?;
    let normal_balance_raw: String = row.get(6)?;
    let normal_balance = parse_side(&normal_balance_raw, 6)?;

    Ok(Account {
        account_id: AccountId(row.get(0)?),
        number: row.get(1)?,
        name: row.get(2)?,
        classification,
        subtype: row.get(4)?,
        parent_id: row.get(5)?,
        normal_balance,
        is_contra: row.get::<_, i64>(7)? != 0,
        is_active: row.get::<_, i64>(8)? != 0,
        needs_mapping: row.get::<_, i64>(9)? != 0,
        source_ref: row.get(10)?,
    })
}

fn row_to_class(row: &Row<'_>) -> rusqlite::Result<Class> {
    Ok(Class {
        class_id: ClassId(row.get(0)?),
        name: row.get(1)?,
        parent_id: row.get(2)?,
        is_active: row.get::<_, i64>(3)? != 0,
        source_ref: row.get(4)?,
    })
}

fn row_to_journal_line(row: &Row<'_>) -> rusqlite::Result<JournalLine> {
    let entity_type: Option<String> = row.get(6)?;
    let entity_id: Option<String> = row.get(7)?;
    let entity = match (entity_type, entity_id) {
        (Some(kind), Some(id)) => Some(ContactRef {
            kind: parse_contact_kind(&kind, 6)?,
            id,
        }),
        _ => None,
    };
    let is_taxable: i64 = row.get(8)?;
    let tax_amount: Option<i64> = row.get(9)?;
    let tax_rate_raw: Option<String> = row.get(10)?;
    let tax_rate = tax_rate_raw
        .map(|raw| {
            Decimal::from_str(&raw).map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    10,
                    "tax_rate".into(),
                    rusqlite::types::Type::Text,
                )
            })
        })
        .transpose()?;

    Ok(JournalLine {
        line_no: row.get(0)?,
        account: AccountId(row.get(1)?),
        class: row.get::<_, Option<String>>(2)?.map(ClassId),
        debit: ledger_core::Money::from_minor(row.get(3)?),
        credit: ledger_core::Money::from_minor(row.get(4)?),
        memo: row.get(5)?,
        entity,
        is_taxable: is_taxable != 0,
        tax_amount: tax_amount.map(ledger_core::Money::from_minor),
        tax_rate,
    })
}

fn row_to_bank_statement(row: &Row<'_>) -> rusqlite::Result<BankStatementRow> {
    let period_start: String = row.get(2)?;
    let period_end: String = row.get(3)?;
    Ok(BankStatementRow {
        statement_id: row.get(0)?,
        account_id: AccountId(row.get(1)?),
        period_start: period_start.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                2,
                "period_start".into(),
                rusqlite::types::Type::Text,
            )
        })?,
        period_end: period_end.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(3, "period_end".into(), rusqlite::types::Type::Text)
        })?,
        opening_balance: Money::from_minor(row.get(4)?),
        closing_balance: Money::from_minor(row.get(5)?),
        imported_at: row.get(6)?,
        closed_at: row.get(7)?,
    })
}

fn row_to_bank_line(row: &Row<'_>) -> rusqlite::Result<BankLineRow> {
    let posted_on: String = row.get(3)?;
    Ok(BankLineRow {
        line_id: row.get(0)?,
        statement_id: row.get(1)?,
        account_id: AccountId(row.get(2)?),
        posted_on: posted_on.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(3, "posted_on".into(), rusqlite::types::Type::Text)
        })?,
        amount: Money::from_minor(row.get(4)?),
        description: row.get(5)?,
        external_id: row.get(6)?,
        matched_entry_id: row.get(7)?,
        matched_line_no: row.get(8)?,
        match_kind: row.get(9)?,
        matched_at: row.get(10)?,
    })
}

fn parse_side(raw: &str, col: usize) -> rusqlite::Result<Side> {
    match raw {
        "Dr" => Ok(Side::Debit),
        "Cr" => Ok(Side::Credit),
        _ => Err(rusqlite::Error::InvalidColumnType(
            col,
            "normal_balance".into(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn parse_contact_kind(raw: &str, col: usize) -> rusqlite::Result<ContactKind> {
    match raw {
        "Customer" => Ok(ContactKind::Customer),
        "Vendor" => Ok(ContactKind::Vendor),
        _ => Err(rusqlite::Error::InvalidColumnType(
            col,
            "entity_type".into(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn parse_date(raw: &str) -> Result<NaiveDate, LedgerError> {
    raw.parse::<NaiveDate>()
        .map_err(|_| LedgerError::BadDate(raw.to_string()))
}

fn parse_datetime(raw: &str) -> Result<DateTime<Utc>, LedgerError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| LedgerError::BadTimestamp(raw.to_string()))
}

/// Same as [`parse_date`], but for use inside a `rusqlite` row mapper, whose
/// closure must return `rusqlite::Result`.
fn parse_date_col(raw: &str, col: usize) -> rusqlite::Result<NaiveDate> {
    parse_date(raw).map_err(|_| {
        rusqlite::Error::InvalidColumnType(col, "date".into(), rusqlite::types::Type::Text)
    })
}

/// Same as [`parse_datetime`], but for use inside a `rusqlite` row mapper.
fn parse_datetime_col(raw: &str, col: usize) -> rusqlite::Result<DateTime<Utc>> {
    parse_datetime(raw).map_err(|_| {
        rusqlite::Error::InvalidColumnType(col, "timestamp".into(), rusqlite::types::Type::Text)
    })
}

fn row_to_adjustment_request(row: &Row<'_>) -> rusqlite::Result<AdjustmentRequestRow> {
    let state_raw: String = row.get(5)?;
    let state = AdjustmentState::parse(&state_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(5, "state".into(), rusqlite::types::Type::Text)
    })?;
    let requested_at: String = row.get(2)?;
    let decided_at: Option<String> = row.get(7)?;

    Ok(AdjustmentRequestRow {
        request_id: row.get(0)?,
        requested_by: row.get(1)?,
        requested_at: parse_datetime_col(&requested_at, 2)?,
        description: row.get(3)?,
        lines_json: row.get(4)?,
        state,
        decided_by: row.get(6)?,
        decided_at: decided_at.map(|s| parse_datetime_col(&s, 7)).transpose()?,
        decision_note: row.get(8)?,
        posted_entry_id: row.get(9)?,
    })
}

fn row_to_gl_line(row: &Row<'_>) -> rusqlite::Result<GlLineRow> {
    let source_type_raw: String = row.get(5)?;
    let source_type = DocKind::parse(&source_type_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(5, "source_type".into(), rusqlite::types::Type::Text)
    })?;
    let entry_date: String = row.get(4)?;
    let entity_type: Option<String> = row.get(10)?;
    let entity_id: Option<String> = row.get(11)?;
    let entity = match (entity_type, entity_id) {
        (Some(kind), Some(id)) => Some(ContactRef {
            kind: parse_contact_kind(&kind, 10)?,
            id,
        }),
        _ => None,
    };

    Ok(GlLineRow {
        account_id: AccountId(row.get(0)?),
        number: row.get(1)?,
        name: row.get(2)?,
        entry_id: row.get(3)?,
        entry_date: parse_date_col(&entry_date, 4)?,
        source_type,
        source_id: row.get(6)?,
        line_memo: row.get(7)?,
        entry_memo: row.get(8)?,
        class_id: row.get::<_, Option<String>>(9)?.map(ClassId),
        entity,
        is_flagged: row.get::<_, i64>(12)? != 0,
        reversal_of: row.get(13)?,
        debit: Money::from_minor(row.get(14)?),
        credit: Money::from_minor(row.get(15)?),
    })
}

fn flip(side: Side) -> Side {
    match side {
        Side::Debit => Side::Credit,
        Side::Credit => Side::Debit,
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Debit => "Dr",
        Side::Credit => "Cr",
    }
}

fn contact_kind_str(kind: ContactKind) -> &'static str {
    match kind {
        ContactKind::Customer => "Customer",
        ContactKind::Vendor => "Vendor",
    }
}

fn locked_through_in(conn: &Connection, company: &str) -> Result<Option<NaiveDate>, LedgerError> {
    let raw: Option<String> = conn.query_row(
        "SELECT MAX(period_end) FROM periods WHERE company_id = ?1 AND state = 'closed'",
        params![company],
        |row| row.get(0),
    )?;
    raw.map(|s| parse_date(&s)).transpose()
}

fn next_close_history_seq(conn: &Connection, company: &str) -> Result<i64, LedgerError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM close_history WHERE company_id = ?1",
        params![company],
        |row| row.get(0),
    )?)
}

/// The §3 class rule: every line on a 4xxx, 5xxx or 6xxx account carries
/// exactly one class; a balance-sheet line carries none. Also checks that
/// the line's account and (if present) class exist in this company's chart.
fn check_class_rule(
    conn: &Connection,
    company: &str,
    line: &JournalLine,
) -> Result<(), LedgerError> {
    let account = account_row_in(conn, company, &line.account.0)?
        .ok_or_else(|| LedgerError::UnknownAccount(line.account.0.clone()))?;

    if let Some(class) = &line.class {
        if !class_exists_in(conn, company, &class.0)? {
            return Err(LedgerError::UnknownClass(class.0.clone()));
        }
    }

    let needs_class = account
        .number
        .chars()
        .next()
        .map(|c| matches!(c, '4' | '5' | '6'))
        .unwrap_or(false);

    if needs_class && line.class.is_none() {
        return Err(LedgerError::MissingClass {
            line_no: line.line_no,
        });
    }
    if !needs_class && line.class.is_some() {
        return Err(LedgerError::UnexpectedClass {
            line_no: line.line_no,
        });
    }
    Ok(())
}

fn account_row_in(
    conn: &Connection,
    company: &str,
    account_id: &str,
) -> Result<Option<Account>, LedgerError> {
    conn.query_row(
        &format!("{ACCOUNT_COLUMNS} FROM accounts WHERE company_id = ?1 AND account_id = ?2"),
        params![company, account_id],
        row_to_account,
    )
    .optional()
    .map_err(LedgerError::from)
}

fn class_exists_in(conn: &Connection, company: &str, class_id: &str) -> Result<bool, LedgerError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM classes WHERE company_id = ?1 AND class_id = ?2",
        params![company, class_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Application, DocLine, LineKind};
    use ledger_core::Money;
    use proptest::prelude::*;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    /// A minimal saved document, so `post_entry`'s foreign key to
    /// `document_versions` has something to point at.
    fn seed_document(ledger: &Ledger, company: &str, document_id: &str) -> DocumentVersionId {
        let doc = LedgerDocument {
            document_id: document_id.to_string(),
            kind: DocKind::Invoice,
            number: Some("INV-1".to_string()),
            txn_date: ymd(2026, 6, 15),
            due_date: None,
            contact: None,
            header_class: None,
            lines: vec![DocLine {
                line_no: 1,
                kind: LineKind::Item,
                amount: Money::from_minor(1000),
                class: Some(ClassId("foam".to_string())),
                item_id: None,
                account: None,
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: None,
                posting: None,
                entity: None,
            }],
            tax: None,
            deposit_to: None,
            pay_from: None,
            applications: Vec::<Application>::new(),
            unapplied: Money::ZERO,
            is_voided: false,
            source_ref: None,
            memo: None,
        };
        ledger
            .save_document(
                company,
                &doc,
                CommandMeta {
                    actor_id: "dan".to_string(),
                    kind: "save_invoice".to_string(),
                    hlc: "hlc-1".to_string(),
                },
                now(),
            )
            .expect("seed document saves")
    }

    fn setup() -> (Ledger, DocumentVersionId) {
        let ledger = Ledger::open_in_memory().expect("open in memory");
        ledger
            .create_company("aquamentor", "Aquamentor LLC", None, now())
            .expect("create company");
        let doc = seed_document(&ledger, "aquamentor", "doc-1");
        (ledger, doc)
    }

    fn balanced_entry(doc: &DocumentVersionId, on: NaiveDate) -> JournalEntry {
        JournalEntry {
            entry_date: on,
            memo: Some("test entry".to_string()),
            source_type: DocKind::Invoice,
            source_id: doc.document_id.clone(),
            source_version: doc.version,
            reversal_of: None,
            is_flagged: false,
            lines: vec![
                JournalLine::debit(
                    1,
                    AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                    Money::from_minor(1000),
                ),
                JournalLine::credit(
                    2,
                    AccountId(chart::SALES_INCOME.to_string()),
                    Money::from_minor(1000),
                )
                .with_class(Some(ClassId("foam".to_string()))),
            ],
        }
    }

    #[test]
    fn a_balanced_entry_posts() {
        let (ledger, doc) = setup();
        let entry_id = ledger
            .post_entry("aquamentor", &balanced_entry(&doc, ymd(2026, 6, 15)), now())
            .expect("balanced entry posts");
        let (_entry, is_posted) = ledger.entry("aquamentor", &entry_id).expect("entry exists");
        assert!(is_posted);
    }

    #[test]
    fn an_unbalanced_entry_is_rejected_by_the_rust_check() {
        let (ledger, doc) = setup();
        let mut entry = balanced_entry(&doc, ymd(2026, 6, 15));
        entry.lines[1].credit = Money::from_minor(900); // now unbalanced
        let err = ledger
            .post_entry("aquamentor", &entry, now())
            .expect_err("unbalanced entry is rejected");
        assert!(matches!(err, LedgerError::Unbalanced));
    }

    /// The trigger, not the Rust guard: bypass `post_entry` and try to flip
    /// `is_posted` on an unbalanced entry with raw SQL.
    #[test]
    fn the_balance_trigger_rejects_an_unbalanced_entry_bypassing_rust() {
        let (ledger, doc) = setup();
        let conn = &ledger.connection;
        conn.execute(
            "INSERT INTO journal_entries
                 (company_id, entry_id, entry_date, memo, source_type, source_id,
                  source_version, is_posted, is_flagged)
             VALUES ('aquamentor', 'e-raw', '2026-06-15', NULL, 'invoice', ?1, ?2, 0, 0)",
            params![doc.document_id, doc.version],
        )
        .expect("insert unposted entry");
        conn.execute(
            "INSERT INTO journal_lines
                 (company_id, entry_id, line_no, account_id, debit_minor, credit_minor)
             VALUES ('aquamentor', 'e-raw', 1, ?1, 1000, 0)",
            params![chart::ACCOUNTS_RECEIVABLE],
        )
        .expect("insert debit line");
        conn.execute(
            "INSERT INTO journal_lines
                 (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
             VALUES ('aquamentor', 'e-raw', 2, ?1, 'foam', 0, 900)",
            params![chart::SALES_INCOME],
        )
        .expect("insert credit line");

        let result = conn.execute(
            "UPDATE journal_entries SET is_posted = 1 WHERE company_id = 'aquamentor' AND entry_id = 'e-raw'",
            [],
        );
        assert!(
            result.is_err(),
            "the balance trigger must reject this update"
        );
    }

    #[test]
    fn a_posted_entry_cannot_be_updated_or_deleted_except_cleared_at() {
        let (ledger, doc) = setup();
        let entry_id = ledger
            .post_entry("aquamentor", &balanced_entry(&doc, ymd(2026, 6, 15)), now())
            .expect("posts");
        let conn = &ledger.connection;

        let update_memo = conn.execute(
            "UPDATE journal_entries SET memo = 'edited' WHERE company_id = 'aquamentor' AND entry_id = ?1",
            params![entry_id],
        );
        assert!(
            update_memo.is_err(),
            "a posted entry's memo must be immutable"
        );

        let delete_entry = conn.execute(
            "DELETE FROM journal_entries WHERE company_id = 'aquamentor' AND entry_id = ?1",
            params![entry_id],
        );
        assert!(delete_entry.is_err(), "a posted entry cannot be deleted");

        let edit_line = conn.execute(
            "UPDATE journal_lines SET debit_minor = 1 WHERE company_id = 'aquamentor' AND entry_id = ?1 AND line_no = 1",
            params![entry_id],
        );
        assert!(
            edit_line.is_err(),
            "a posted line's content must be immutable"
        );

        let delete_line = conn.execute(
            "DELETE FROM journal_lines WHERE company_id = 'aquamentor' AND entry_id = ?1 AND line_no = 1",
            params![entry_id],
        );
        assert!(delete_line.is_err(), "a posted line cannot be deleted");

        let clear = conn.execute(
            "UPDATE journal_lines SET cleared_at = ?2 WHERE company_id = 'aquamentor' AND entry_id = ?1 AND line_no = 1",
            params![entry_id, now().to_rfc3339()],
        );
        assert!(
            clear.is_ok(),
            "cleared_at is the one mutable field on a posted line"
        );
    }

    #[test]
    fn posting_into_a_closed_period_fails() {
        let (ledger, doc) = setup();
        ledger
            .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
            .expect("close June");

        let err = ledger
            .post_entry("aquamentor", &balanced_entry(&doc, ymd(2026, 6, 15)), now())
            .expect_err("June is closed");
        assert!(
            matches!(err, LedgerError::PeriodClosed { period_end } if period_end == ymd(2026, 6, 30))
        );
    }

    #[test]
    fn reopen_writes_an_is_reopen_row_and_then_posting_succeeds() {
        let (ledger, doc) = setup();
        ledger
            .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
            .expect("close June");
        ledger
            .reopen_period(
                "aquamentor",
                ymd(2026, 6, 30),
                "dan",
                "Polymer Source bill arrived late",
                now(),
            )
            .expect("reopen June");

        let is_reopen: i64 = ledger
            .connection
            .query_row(
                "SELECT is_reopen FROM close_history WHERE company_id = 'aquamentor' ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("close history row exists");
        assert_eq!(is_reopen, 1);

        assert_eq!(
            ledger.locked_through("aquamentor").expect("locked_through"),
            None
        );

        ledger
            .post_entry("aquamentor", &balanced_entry(&doc, ymd(2026, 6, 15)), now())
            .expect("June is open again");
    }

    #[test]
    fn class_rule_rejects_an_income_line_without_class() {
        let (ledger, doc) = setup();
        let mut entry = balanced_entry(&doc, ymd(2026, 6, 15));
        entry.lines[1].class = None; // 4100 with no class
        let err = ledger
            .post_entry("aquamentor", &entry, now())
            .expect_err("missing class");
        assert!(matches!(err, LedgerError::MissingClass { line_no: 2 }));
    }

    #[test]
    fn class_rule_rejects_a_class_on_ar() {
        let (ledger, doc) = setup();
        let mut entry = balanced_entry(&doc, ymd(2026, 6, 15));
        entry.lines[0].class = Some(ClassId("foam".to_string())); // AR carrying a class
        let err = ledger
            .post_entry("aquamentor", &entry, now())
            .expect_err("class on AR");
        assert!(matches!(err, LedgerError::UnexpectedClass { line_no: 1 }));
    }

    #[test]
    fn reversal_produces_mirrored_lines_and_both_stay_posted() {
        let (ledger, doc) = setup();
        let entry_id = ledger
            .post_entry("aquamentor", &balanced_entry(&doc, ymd(2026, 6, 15)), now())
            .expect("original posts");

        let reversal_id = ledger
            .reverse_entry("aquamentor", &entry_id, ymd(2026, 6, 20), now())
            .expect("reversal posts");

        let (original, original_posted) = ledger.entry("aquamentor", &entry_id).expect("original");
        let (reversal, reversal_posted) =
            ledger.entry("aquamentor", &reversal_id).expect("reversal");

        assert!(original_posted);
        assert!(reversal_posted);
        assert_eq!(reversal.reversal_of.as_deref(), Some(entry_id.as_str()));
        assert_eq!(reversal.lines[0].debit, original.lines[0].credit);
        assert_eq!(reversal.lines[0].credit, original.lines[0].debit);
        assert_eq!(reversal.lines[1].debit, original.lines[1].credit);
        assert_eq!(reversal.lines[1].credit, original.lines[1].debit);
    }

    #[test]
    fn create_company_seeds_the_right_class_list_per_company() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .create_company("aquamentor", "Aquamentor LLC", None, now())
            .expect("create");
        ledger
            .create_company("waterline", "WaterLine CNC", None, now())
            .expect("create");

        let aqua_classes = ledger.list_classes("aquamentor").expect("list");
        let water_classes = ledger.list_classes("waterline").expect("list");
        assert_eq!(aqua_classes.len(), chart::AQUAMENTOR_CLASSES.len());
        assert_eq!(water_classes.len(), chart::WATERLINE_CLASSES.len());
    }

    #[test]
    fn oplog_grows_with_each_saved_document() {
        let (ledger, _doc) = setup();
        assert_eq!(ledger.oplog_len("aquamentor").expect("len"), 1);
        seed_document(&ledger, "aquamentor", "doc-2");
        assert_eq!(ledger.oplog_len("aquamentor").expect("len"), 2);
    }

    #[test]
    fn replay_marker_round_trips() {
        let ledger = Ledger::open_in_memory().expect("open");
        assert!(!ledger.is_replaying());
        ledger.set_replaying(true);
        assert!(ledger.is_replaying());
    }

    proptest! {
        /// Any balanced entry, with any (checked) split of a total between
        /// however many debit/credit line pairs, posts, and the trial
        /// balance stays balanced afterwards.
        #[test]
        fn any_random_balanced_entry_posts_and_tb_stays_balanced(
            amounts in prop::collection::vec(1i64..100_000, 1..6),
        ) {
            let (ledger, doc) = setup();
            let total: i64 = amounts.iter().sum();

            let mut lines = vec![JournalLine::debit(
                1,
                AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                Money::from_minor(total),
            )];
            for (i, amount) in amounts.iter().enumerate() {
                lines.push(
                    JournalLine::credit(
                        (i + 2) as i64,
                        AccountId(chart::SALES_INCOME.to_string()),
                        Money::from_minor(*amount),
                    )
                    .with_class(Some(ClassId("foam".to_string()))),
                );
            }

            let entry = JournalEntry {
                entry_date: ymd(2026, 6, 15),
                memo: None,
                source_type: DocKind::Invoice,
                source_id: doc.document_id.clone(),
                source_version: doc.version,
                reversal_of: None,
                is_flagged: false,
                lines,
            };

            let result = ledger.post_entry("aquamentor", &entry, now());
            prop_assert!(result.is_ok());

            let tb = crate::report::trial_balance(&ledger, "aquamentor", ymd(2026, 6, 15))
                .expect("trial balance computes");
            prop_assert_eq!(tb.total_debits, tb.total_credits);
        }
    }
}
