//! `Ledger` store integration tests, against the public API only.
//! `LEDGER-DESIGN.md` §4, §5.
//!
//! Names and figures are invented.

use chrono::{DateTime, NaiveDate, Utc};
use ledger::chart;
use ledger::store::{CommandMeta, DocumentVersionId, Ledger, LedgerError};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, JournalEntry,
    JournalLine, LedgerDocument, LineKind,
};
use ledger_core::Money;

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn command(kind: &str) -> CommandMeta {
    CommandMeta {
        actor_id: "dan".to_string(),
        kind: kind.to_string(),
        hlc: "hlc-1".to_string(),
    }
}

fn seed_document(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    txn_date: NaiveDate,
) -> DocumentVersionId {
    let doc = LedgerDocument {
        document_id: document_id.to_string(),
        kind: DocKind::Invoice,
        number: Some("INV-9001".to_string()),
        txn_date,
        due_date: None,
        contact: Some(ContactRef {
            kind: ContactKind::Customer,
            id: "cust-1".to_string(),
        }),
        header_class: None,
        lines: vec![DocLine {
            line_no: 1,
            kind: LineKind::Item,
            amount: Money::from_minor(10000),
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
        .save_document(company, &doc, command("save_invoice"), now())
        .unwrap()
}

fn aquamentor() -> (Ledger, DocumentVersionId) {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", None, now())
        .unwrap();
    let doc = seed_document(&ledger, "aquamentor", "doc-1", ymd(2026, 6, 15));
    (ledger, doc)
}

fn invoice_entry(doc: &DocumentVersionId, on: NaiveDate) -> JournalEntry {
    JournalEntry {
        entry_date: on,
        memo: Some("INV-9001".to_string()),
        source_type: DocKind::Invoice,
        source_id: doc.document_id.clone(),
        source_version: doc.version,
        reversal_of: None,
        is_flagged: false,
        lines: vec![
            JournalLine::debit(
                1,
                AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                Money::from_minor(10000),
            ),
            JournalLine::credit(
                2,
                AccountId(chart::SALES_INCOME.to_string()),
                Money::from_minor(10000),
            )
            .with_class(Some(ClassId("foam".to_string()))),
        ],
    }
}

#[test]
fn open_in_memory_migrates_to_the_latest_schema() {
    let ledger = Ledger::open_in_memory().unwrap();
    assert!(ledger.schema_version().unwrap() >= 1);
}

#[test]
fn create_company_seeds_the_full_chart() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", Some("realm-1"), now())
        .unwrap();
    let accounts = ledger.list_accounts("aquamentor").unwrap();
    assert_eq!(accounts.len(), chart::seed_chart().len());
    assert!(accounts
        .iter()
        .any(|a| a.number == chart::ACCOUNTS_RECEIVABLE));
}

#[test]
fn the_two_companies_share_nothing() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", None, now())
        .unwrap();
    ledger
        .create_company("waterline", "WaterLine CNC", None, now())
        .unwrap();

    let aqua_classes = ledger.list_classes("aquamentor").unwrap();
    let water_classes = ledger.list_classes("waterline").unwrap();
    assert_eq!(aqua_classes.len(), 6);
    assert_eq!(water_classes.len(), 2);
    assert!(water_classes
        .iter()
        .all(|c| c.class_id.0 == "cnc" || c.class_id.0 == "uv"));
}

#[test]
fn add_account_marks_needs_mapping_and_is_found_by_source_ref() {
    let (ledger, _doc) = aquamentor();
    ledger
        .add_account(
            "aquamentor",
            "4995",
            "QBO Uncategorised Income",
            chart::Classification::Income,
            false,
            Some("qbo-acct-77"),
            true,
        )
        .unwrap();

    let found = ledger
        .account_by_source_ref("aquamentor", "qbo-acct-77")
        .unwrap()
        .unwrap();
    assert_eq!(found.number, "4995");
    assert!(found.needs_mapping);
    assert_eq!(found.normal_balance, ledger::types::Side::Credit);
}

#[test]
fn a_balanced_entry_posts_and_is_retrievable() {
    let (ledger, doc) = aquamentor();
    let entry_id = ledger
        .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
        .unwrap();

    let (entry, is_posted) = ledger.entry("aquamentor", &entry_id).unwrap();
    assert!(is_posted);
    assert_eq!(entry.lines.len(), 2);

    let for_doc = ledger
        .entries_for_document("aquamentor", &doc.document_id)
        .unwrap();
    assert_eq!(for_doc.len(), 1);
    assert_eq!(for_doc[0].0, entry_id);
}

#[test]
fn posting_an_entry_with_an_unknown_account_is_rejected() {
    let (ledger, doc) = aquamentor();
    let mut entry = invoice_entry(&doc, ymd(2026, 6, 15));
    entry.lines[0].account = AccountId("9999".to_string());
    let err = ledger.post_entry("aquamentor", &entry, now()).unwrap_err();
    assert!(matches!(err, LedgerError::UnknownAccount(_)));
}

#[test]
fn posting_an_entry_with_an_unknown_class_is_rejected() {
    let (ledger, doc) = aquamentor();
    let mut entry = invoice_entry(&doc, ymd(2026, 6, 15));
    entry.lines[1].class = Some(ClassId("skydiving".to_string()));
    let err = ledger.post_entry("aquamentor", &entry, now()).unwrap_err();
    assert!(matches!(err, LedgerError::UnknownClass(_)));
}

#[test]
fn a_document_dated_on_the_lock_boundary_is_rejected_and_the_day_after_posts() {
    let (ledger, doc) = aquamentor();
    ledger
        .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
        .unwrap();

    // On the boundary: rejected.
    let boundary_err = ledger
        .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 30)), now())
        .unwrap_err();
    assert!(
        matches!(boundary_err, LedgerError::PeriodClosed { period_end } if period_end == ymd(2026, 6, 30))
    );

    // The day after: posts.
    let doc2 = seed_document(&ledger, "aquamentor", "doc-2", ymd(2026, 7, 1));
    ledger
        .post_entry("aquamentor", &invoice_entry(&doc2, ymd(2026, 7, 1)), now())
        .unwrap();
}

#[test]
fn reopen_is_never_quiet() {
    let (ledger, _doc) = aquamentor();
    ledger
        .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
        .unwrap();
    ledger
        .reopen_period(
            "aquamentor",
            ymd(2026, 6, 30),
            "dan",
            "late bill from Polymer Source",
            now(),
        )
        .unwrap();
    assert_eq!(ledger.locked_through("aquamentor").unwrap(), None);
}

#[test]
fn reopening_a_period_that_was_never_closed_is_not_found() {
    let (ledger, _doc) = aquamentor();
    let err = ledger
        .reopen_period(
            "aquamentor",
            ymd(2026, 6, 30),
            "dan",
            "nothing to reopen",
            now(),
        )
        .unwrap_err();
    assert!(matches!(err, LedgerError::NotFound));
}

#[test]
fn reversal_mirrors_the_original_and_both_remain_posted() {
    let (ledger, doc) = aquamentor();
    let entry_id = ledger
        .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
        .unwrap();
    let reversal_id = ledger
        .reverse_entry("aquamentor", &entry_id, ymd(2026, 6, 16), now())
        .unwrap();

    let (_original, original_posted) = ledger.entry("aquamentor", &entry_id).unwrap();
    let (reversal, reversal_posted) = ledger.entry("aquamentor", &reversal_id).unwrap();
    assert!(original_posted);
    assert!(reversal_posted);
    assert_eq!(reversal.entry_date, ymd(2026, 6, 16));
    assert_eq!(reversal.reversal_of.as_deref(), Some(entry_id.as_str()));

    let for_doc = ledger
        .entries_for_document("aquamentor", &doc.document_id)
        .unwrap();
    assert_eq!(for_doc.len(), 2);
}

#[test]
fn reversal_respects_the_period_gate_on_the_reversal_date() {
    let (ledger, doc) = aquamentor();
    let entry_id = ledger
        .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
        .unwrap();
    ledger
        .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
        .unwrap();

    let err = ledger
        .reverse_entry("aquamentor", &entry_id, ymd(2026, 6, 20), now())
        .unwrap_err();
    assert!(matches!(err, LedgerError::PeriodClosed { .. }));
}

#[test]
fn oplog_len_counts_saved_documents_for_the_company() {
    let (ledger, _doc) = aquamentor();
    assert_eq!(ledger.oplog_len("aquamentor").unwrap(), 1);
    seed_document(&ledger, "aquamentor", "doc-2", ymd(2026, 6, 20));
    assert_eq!(ledger.oplog_len("aquamentor").unwrap(), 2);
}

#[test]
fn snapshot_trial_balance_records_a_row_per_nonzero_account() {
    let (ledger, doc) = aquamentor();
    ledger
        .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
        .unwrap();
    let snapshot_id = ledger
        .snapshot_trial_balance("aquamentor", ymd(2026, 6, 30), now())
        .unwrap();
    assert!(!snapshot_id.is_empty());
}

/// Per-account `(debit, credit)` sums over posted entries, straight from
/// SQL, so the before and after sides do not depend on this build's
/// `report` module reading an old schema.
fn posted_sums(conn: &rusqlite::Connection) -> Vec<(String, i64, i64)> {
    let mut stmt = conn
        .prepare(
            "SELECT l.account_id, SUM(l.debit_minor), SUM(l.credit_minor)
             FROM journal_lines l
             JOIN journal_entries e
               ON e.company_id = l.company_id AND e.entry_id = l.entry_id AND e.is_posted = 1
             GROUP BY l.account_id ORDER BY l.account_id",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A file written by the first build (schema version 1) with posted
/// entries in it, opened by this one: every pending migration runs, a
/// backup of the version-1 file is taken first, and the trial balance is
/// the same before and after.
#[test]
fn a_version_one_book_migrates_to_latest_with_its_trial_balance_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");

    let before = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (
                 version    INTEGER PRIMARY KEY,
                 name       TEXT NOT NULL,
                 applied_at TEXT NOT NULL
             ) STRICT;",
        )
        .unwrap();
        let v1 = &ledger::store::MIGRATIONS[0];
        assert_eq!(v1.version, 1);
        conn.execute_batch(v1.sql).unwrap();
        conn.execute(
            "INSERT INTO schema_version VALUES (1, ?1, '2026-06-01T00:00:00Z')",
            [v1.name],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO companies (company_id, display_name, created_at)
                 VALUES ('aquamentor', 'Aquamentor', '2026-06-01T00:00:00Z');
             INSERT INTO accounts (company_id, account_id, number, name, classification, normal_balance)
                 VALUES ('aquamentor', '1000', '1000', 'Checking', 'Asset', 'Dr'),
                        ('aquamentor', '1200', '1200', 'Accounts receivable', 'Asset', 'Dr'),
                        ('aquamentor', '2200', '2200', 'Sales tax payable', 'Liability', 'Cr'),
                        ('aquamentor', '4000', '4000', 'Sales', 'Income', 'Cr');",
        )
        .unwrap();
        // Three posted entries and one left unposted, which no trial
        // balance may count either side of the migration.
        type Line<'a> = (&'a str, i64, i64);
        let entries: [(&str, &str, bool, &[Line]); 4] = [
            (
                "e1",
                "2026-06-10",
                true,
                &[("1200", 10663, 0), ("4000", 0, 10000), ("2200", 0, 663)],
            ),
            (
                "e2",
                "2026-06-20",
                true,
                &[("1000", 5000, 0), ("1200", 0, 5000)],
            ),
            (
                "e3",
                "2026-06-25",
                true,
                &[("1200", 99, 0), ("4000", 0, 99)],
            ),
            (
                "e4",
                "2026-06-30",
                false,
                &[("1000", 70000, 0), ("4000", 0, 70000)],
            ),
        ];
        for (entry_id, date, posted, lines) in entries {
            conn.execute(
                "INSERT INTO document_versions
                     (company_id, document_id, version, doc_type, txn_date, payload_json,
                      created_at, command_id)
                 VALUES ('aquamentor', ?1, 1, 'Invoice', ?2, '{}', ?2, ?1)",
                [entry_id, date],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO journal_entries
                     (company_id, entry_id, entry_date, source_type, source_id, source_version)
                 VALUES ('aquamentor', ?1, ?2, 'invoice', ?1, 1)",
                [entry_id, date],
            )
            .unwrap();
            for (line_no, (account, debit, credit)) in lines.iter().enumerate() {
                conn.execute(
                    "INSERT INTO journal_lines
                         (company_id, entry_id, line_no, account_id, debit_minor, credit_minor)
                     VALUES ('aquamentor', ?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![entry_id, line_no as i64 + 1, account, debit, credit],
                )
                .unwrap();
            }
            if posted {
                conn.execute(
                    "UPDATE journal_entries SET is_posted = 1 WHERE entry_id = ?1",
                    [entry_id],
                )
                .unwrap();
            }
        }
        posted_sums(&conn)
    };
    assert_eq!(before.len(), 4, "every account carries a posted balance");

    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.schema_version().unwrap(),
        ledger::store::latest_version()
    );
    let reader = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(posted_sums(&reader), before);
    drop(reader);

    let tb = ledger::report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();
    let after: Vec<(String, i64, i64)> = tb
        .rows
        .iter()
        .map(|row| {
            (
                row.account_id.0.clone(),
                row.debit.minor(),
                row.credit.minor(),
            )
        })
        .collect();
    assert_eq!(after, before);
    assert_eq!(tb.total_debits, tb.total_credits);
    assert_eq!(tb.total_debits, Money::from_minor(10663 + 5000 + 99));

    // The backup is the version-1 file as it stood, readable on its own.
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".bak"))
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    let latest = ledger::store::latest_version();
    assert!(
        backups[0].starts_with(&format!("ledger.db.pre-v{latest}-from-v1-")),
        "{backups:?}"
    );
    let backup = rusqlite::Connection::open(dir.path().join(&backups[0])).unwrap();
    let version: i64 = backup
        .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(posted_sums(&backup), before);

    // Opening the now-current file again migrates nothing and copies nothing.
    drop(ledger);
    drop(backup);
    Ledger::open(&path).unwrap();
    let count = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".bak")
        })
        .count();
    assert_eq!(count, 1);
}
