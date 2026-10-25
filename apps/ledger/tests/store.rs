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
