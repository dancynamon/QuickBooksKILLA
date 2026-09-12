//! Bank statement import, matching, proposals and the per-statement close.
//! `LEDGER-DESIGN.md` §10.
//!
//! Names and figures are invented.

use std::process::Command;

use chrono::{DateTime, NaiveDate, Utc};

use ledger::bank::{MatchRules, NewStatement, ParsedLine};
use ledger::chart;
use ledger::post::{self, Posting};
use ledger::store::{CommandMeta, Ledger, LedgerError};
use ledger::types::{
    AccountId, ClassId, DocKind, DocLine, LedgerDocument, LineKind, PostingContext,
};
use ledger_core::Money;

const COMPANY: &str = "aquamentor";

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn account_line(account: &str, amount: i64, class: Option<&str>) -> DocLine {
    DocLine {
        line_no: 1,
        kind: LineKind::Account,
        amount: Money::from_minor(amount),
        class: class.map(|c| ClassId(c.to_string())),
        item_id: None,
        account: Some(AccountId(account.to_string())),
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

fn base_doc(id: &str, kind: DocKind, txn_date: NaiveDate) -> LedgerDocument {
    LedgerDocument {
        document_id: id.to_string(),
        kind,
        number: None,
        txn_date,
        due_date: None,
        contact: None,
        header_class: None,
        lines: Vec::new(),
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: None,
    }
}

/// Save and post `doc` through the same [`post::post`] every document goes
/// through, exactly as `crate::pipeline` and `Ledger::confirm_proposal` do.
fn post_doc(ledger: &Ledger, doc: &LedgerDocument) {
    let version = ledger
        .save_document(
            COMPANY,
            doc,
            CommandMeta {
                actor_id: "dan".to_string(),
                kind: "test_seed".to_string(),
                hlc: "hlc-1".to_string(),
            },
            now(),
        )
        .expect("saves");
    let ctx = PostingContext::default();
    match post::post(doc, version.version, &ctx).expect("posts") {
        Posting::Entry(entry) => {
            ledger.post_entry(COMPANY, &entry, now()).expect("posts");
        }
        Posting::NonPosting => panic!("expected {:?} to post", doc.kind),
    }
}

fn checking() -> AccountId {
    AccountId(chart::CHECKING.to_string())
}

/// A posted payment, deposit, Authorize.net settlement and expense, all
/// hitting 1100 Checking on four different days in the same week.
fn seed_ledger() -> Ledger {
    let ledger = Ledger::open_in_memory().expect("open");
    ledger
        .create_company(COMPANY, "Aquamentor LLC", None, now())
        .expect("create company");

    // A payment landing straight in the bank (rather than 1150), unapplied:
    // Dr 1100 500.00, Cr 2300 500.00.
    let mut payment = base_doc("pay-1", DocKind::Payment, ymd(2026, 9, 1));
    payment.deposit_to = Some(checking());
    payment.unapplied = Money::from_minor(50_000);
    post_doc(&ledger, &payment);

    // A deposit grouping one payment already in 2300: Dr 1100 300.00,
    // Cr 2300 300.00.
    let mut deposit = base_doc("dep-1", DocKind::Deposit, ymd(2026, 9, 2));
    deposit.deposit_to = Some(checking());
    deposit.lines = vec![account_line(chart::CUSTOMER_DEPOSITS, 30_000, None)];
    post_doc(&ledger, &deposit);

    // An Authorize.net settlement, no fee: Dr 1100 750.00, Cr 1160 750.00.
    let mut settlement = base_doc("settle-1", DocKind::Settlement, ymd(2026, 9, 3));
    settlement.deposit_to = Some(checking());
    settlement.lines = vec![account_line(chart::AUTHNET_CLEARING, 75_000, None)];
    post_doc(&ledger, &settlement);

    // A non-PO expense paid from the bank: Dr 6100 50.00 (foam), Cr 1100 50.00.
    let mut purchase = base_doc("purch-1", DocKind::Purchase, ymd(2026, 9, 4));
    purchase.pay_from = Some(checking());
    purchase.lines = vec![account_line(chart::SHOP_SUPPLIES, 5_000, Some("foam"))];
    post_doc(&ledger, &purchase);

    ledger
}

/// The statement lines covering all four seeded entries plus one the ledger
/// has never heard of.
fn statement_lines() -> Vec<ParsedLine> {
    vec![
        ParsedLine {
            posted_on: ymd(2026, 9, 1),
            amount: Money::from_minor(50_000),
            description: "CUSTOMER CHECK DEPOSIT".to_string(),
            external_id: Some("line-1".to_string()),
        },
        ParsedLine {
            posted_on: ymd(2026, 9, 2),
            amount: Money::from_minor(30_000),
            description: "DEPOSIT".to_string(),
            external_id: Some("line-2".to_string()),
        },
        ParsedLine {
            posted_on: ymd(2026, 9, 3),
            amount: Money::from_minor(75_000),
            description: "AUTHORIZE.NET SETTLEMENT".to_string(),
            external_id: Some("line-3".to_string()),
        },
        ParsedLine {
            posted_on: ymd(2026, 9, 4),
            amount: Money::from_minor(-5_000),
            description: "SHOP SUPPLIES CO".to_string(),
            external_id: Some("line-4".to_string()),
        },
        // The unknown line: no posted entry anywhere near it, and no
        // keyword in its description.
        ParsedLine {
            posted_on: ymd(2026, 9, 5),
            amount: Money::from_minor(-4_500),
            description: "MYSTERY VENDOR LLC".to_string(),
            external_id: Some("line-5".to_string()),
        },
    ]
}

fn new_statement(lines: Vec<ParsedLine>) -> NewStatement {
    NewStatement {
        period_start: ymd(2026, 9, 1),
        period_end: ymd(2026, 9, 5),
        // opening + the four matched lines (500 + 300 + 750 - 50) = 2500.00,
        // but the statement's own stated closing balance is 2455.00 — the
        // true ending balance once the unknown -45.00 line is included too.
        // Until that line is resolved, the statement is exactly 45.00 short
        // of closing.
        opening_balance: Money::from_minor(100_000),
        closing_balance: Money::from_minor(245_500),
        lines,
    }
}

#[test]
fn import_match_close_fails_confirm_close_succeeds_reimport_and_rematch_are_refused() {
    let ledger = seed_ledger();
    let account = checking();

    // -- import -------------------------------------------------------------
    let report = ledger
        .import_statement(COMPANY, &account, new_statement(statement_lines()), now())
        .expect("imports");
    assert_eq!(report.inserted, 5);
    assert_eq!(report.skipped_duplicate, 0);
    let statement_id = report.statement_id.clone();

    // -- match: 3 exact, 1 settlement, 1 proposal ----------------------------
    let match_report = ledger
        .match_statement(COMPANY, &statement_id, &MatchRules::default(), now())
        .expect("matches");
    assert_eq!(match_report.exact, 3, "{match_report:?}");
    assert_eq!(match_report.settlement, 1, "{match_report:?}");
    assert_eq!(match_report.proposals.len(), 1, "{match_report:?}");
    let proposal = &match_report.proposals[0];
    assert_eq!(
        proposal.suggested_account,
        AccountId(chart::OTHER_OPERATING.to_string())
    );
    assert_eq!(proposal.reason, "unknown vendor");

    // Re-matching is idempotent: nothing already resolved is touched again.
    let second_match = ledger
        .match_statement(COMPANY, &statement_id, &MatchRules::default(), now())
        .expect("matches again");
    assert_eq!(second_match, ledger::bank::MatchReport::default());

    // -- close fails with the exact difference ------------------------------
    let err = ledger
        .close_statement(COMPANY, &statement_id, now())
        .expect_err("one line still unmatched");
    match err {
        LedgerError::StatementDoesNotClose {
            difference,
            unmatched,
        } => {
            assert_eq!(difference, Money::from_minor(-4_500));
            assert_eq!(unmatched, 1);
        }
        other => panic!("expected StatementDoesNotClose, got {other:?}"),
    }

    // -- confirm the proposal to 6900 (a class is required, §3: 6xxx) -------
    let entry_id = ledger
        .confirm_proposal(
            COMPANY,
            &proposal.line_id,
            &AccountId(chart::OTHER_OPERATING.to_string()),
            Some(ClassId("foam".to_string())),
            now(),
        )
        .expect("confirms");
    let (entry, is_posted) = ledger.entry(COMPANY, &entry_id).expect("entry exists");
    assert!(is_posted);
    assert_eq!(entry.source_type, DocKind::BankLine);

    // Confirming again is refused: it is no longer a pending proposal.
    let err = ledger
        .confirm_proposal(
            COMPANY,
            &proposal.line_id,
            &AccountId(chart::OTHER_OPERATING.to_string()),
            None,
            now(),
        )
        .expect_err("already confirmed");
    assert!(matches!(err, LedgerError::NotAProposal { .. }));

    // -- close now succeeds ---------------------------------------------------
    let close = ledger
        .close_statement(COMPANY, &statement_id, now())
        .expect("closes now that every line resolves");
    assert_eq!(close.statement_id, statement_id);

    // Closing an already-closed statement is a harmless no-op.
    let close_again = ledger
        .close_statement(COMPANY, &statement_id, now())
        .expect("idempotent");
    assert_eq!(close_again.closed_at, close.closed_at);

    // -- re-match on a closed statement is refused ---------------------------
    let err = ledger
        .match_statement(COMPANY, &statement_id, &MatchRules::default(), now())
        .expect_err("closed statements cannot be re-matched");
    assert!(matches!(err, LedgerError::StatementClosed { .. }));

    // -- re-importing the same file inserts nothing --------------------------
    let reimport = ledger
        .import_statement(COMPANY, &account, new_statement(statement_lines()), now())
        .expect("reimports");
    assert_eq!(reimport.inserted, 0);
    assert_eq!(reimport.skipped_duplicate, 5);
}

#[test]
fn a_line_with_no_amount_match_and_no_keyword_gets_the_honest_default() {
    let ledger = Ledger::open_in_memory().expect("open");
    ledger
        .create_company(COMPANY, "Aquamentor LLC", None, now())
        .expect("create company");
    let statement = NewStatement {
        period_start: ymd(2026, 9, 1),
        period_end: ymd(2026, 9, 1),
        opening_balance: Money::ZERO,
        closing_balance: Money::from_minor(-1234),
        lines: vec![ParsedLine {
            posted_on: ymd(2026, 9, 1),
            amount: Money::from_minor(-1234),
            description: "SOME LOCAL SHOP".to_string(),
            external_id: None,
        }],
    };
    let report = ledger
        .import_statement(COMPANY, &checking(), statement, now())
        .expect("imports");
    let match_report = ledger
        .match_statement(COMPANY, &report.statement_id, &MatchRules::default(), now())
        .expect("matches");
    assert_eq!(match_report.proposals.len(), 1);
    assert_eq!(
        match_report.proposals[0].suggested_account,
        AccountId(chart::OTHER_OPERATING.to_string())
    );
    assert_eq!(match_report.proposals[0].reason, "unknown vendor");

    // The statement does not close while that one line stays unmatched.
    let close_err = ledger
        .close_statement(COMPANY, &report.statement_id, now())
        .expect_err("still unmatched");
    assert!(matches!(
        close_err,
        LedgerError::StatementDoesNotClose { .. }
    ));
}

// ---------------------------------------------------------------------------
// CLI: import -> match -> status through the real binary.
// ---------------------------------------------------------------------------

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(args)
        .output()
        .expect("spawn the ledger binary")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_bank_import_match_status_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");
    let csv_path = dir.path().join("statement.csv");
    std::fs::write(
        &csv_path,
        "date,description,amount,external_id\n\
         2026-09-02,VENDOR A,-25.00,ext-a\n\
         2026-09-03,VENDOR B,-30.00,ext-b\n",
    )
    .unwrap();

    let init = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--name",
        "Aquamentor LLC",
    ]);
    assert!(init.status.success(), "init failed: {}", stdout(&init));

    let import = run(&[
        "bank",
        "import",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--account",
        "1100",
        "--file",
        csv_path.to_str().unwrap(),
        "--format",
        "csv",
        "--profile",
        "generic",
        "--opening",
        "1000.00",
        "--closing",
        "945.00",
        "--from",
        "2026-09-01",
        "--to",
        "2026-09-10",
    ]);
    assert!(
        import.status.success(),
        "import failed: {}",
        stdout(&import)
    );
    let import_out = stdout(&import);
    assert!(import_out.contains("inserted 2"), "{import_out}");
    let statement_id = import_out
        .split("imported statement ")
        .nth(1)
        .and_then(|rest| rest.split(" - inserted").next())
        .expect("statement id in output")
        .trim()
        .to_string();

    let bank_match = run(&[
        "bank",
        "match",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--statement",
        &statement_id,
    ]);
    assert!(
        bank_match.status.success(),
        "match failed: {}",
        stdout(&bank_match)
    );
    let match_out = stdout(&bank_match);
    assert!(
        match_out
            .lines()
            .any(|line| line.trim_start().starts_with("proposals")
                && line.trim_end().ends_with(": 2")),
        "{match_out}"
    );

    let status = run(&[
        "bank",
        "status",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--statement",
        &statement_id,
    ]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stdout(&status)
    );
    let status_out = stdout(&status);
    assert!(status_out.contains("PROPOSALS:"), "{status_out}");
    assert!(status_out.contains("matched: 0"), "{status_out}");
    assert!(status_out.contains("proposals: 2"), "{status_out}");
}
