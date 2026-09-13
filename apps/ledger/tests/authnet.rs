//! Authorize.net settlement import, end to end: parse -> group -> post ->
//! re-import (idempotent) -> a fee-bearing bank statement line confirms the
//! batch. `LEDGER-DESIGN.md` §1, §10; `DECISIONS.md` D15.
//!
//! Names, ids and figures are invented.

use std::process::Command;

use chrono::{DateTime, NaiveDate, Utc};

use ledger::authnet::{self, AuthnetConfig};
use ledger::bank::{MatchRules, NewStatement, ParsedLine};
use ledger::chart;
use ledger::post::{self, Posting};
use ledger::report;
use ledger::store::{CommandMeta, Ledger};
use ledger::types::{
    AccountId, ClassId, ContactKind, ContactRef, DocKind, DocLine, LedgerDocument, LineKind,
    PostingContext,
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

fn checking() -> AccountId {
    AccountId(chart::CHECKING.to_string())
}

fn config() -> AuthnetConfig {
    AuthnetConfig {
        clearing: AccountId(chart::AUTHNET_CLEARING.to_string()),
        fees: AccountId(chart::MERCHANT_FEES.to_string()),
        undeposited: AccountId(chart::UNDEPOSITED_FUNDS.to_string()),
        refund_class: ClassId("foam".to_string()),
        fee_class: ClassId("foam".to_string()),
    }
}

/// Save and post `doc` through the same [`post::post`] every document goes
/// through. Returns the posted entry id.
fn post_doc(ledger: &Ledger, doc: &LedgerDocument) -> String {
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
        Posting::Entry(entry) => ledger.post_entry(COMPANY, &entry, now()).expect("posts"),
        Posting::NonPosting => panic!("expected {:?} to post", doc.kind),
    }
}

fn minimal_invoice(id: &str, number: &str, customer: &str, amount_minor: i64) -> LedgerDocument {
    LedgerDocument {
        document_id: id.to_string(),
        kind: DocKind::Invoice,
        number: Some(number.to_string()),
        txn_date: ymd(2026, 9, 1),
        due_date: None,
        contact: Some(ContactRef {
            kind: ContactKind::Customer,
            id: customer.to_string(),
        }),
        header_class: None,
        lines: vec![DocLine {
            line_no: 1,
            kind: LineKind::Item,
            amount: Money::from_minor(amount_minor),
            class: Some(ClassId("foam".to_string())),
            item_id: None,
            account: None,
            is_taxable: false,
            qty: None,
            unit_cost: None,
            description: Some("Rescue tube".to_string()),
            posting: None,
            entity: None,
        }],
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

/// Two open invoices, seeded exactly as `qbo-local` import would leave them:
/// posted, unpaid, AR carrying the full amount.
fn seed_invoices(ledger: &Ledger) -> (String, String) {
    let inv1 = minimal_invoice("inv-1001", "1001", "cust-priya-shah", 10_000);
    let inv2 = minimal_invoice("inv-1002", "1002", "cust-marcus-liu", 15_000);
    post_doc(ledger, &inv1);
    post_doc(ledger, &inv2);
    (inv1.document_id, inv2.document_id)
}

/// One batch: two sales matching the seeded invoices, one sale against an
/// invoice number nobody has, and one refund against the first invoice's
/// customer. Batch gross 325.00, refunds 40.00, net before fees 285.00.
const SAMPLE_CSV: &str = "\
Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount,Invoice Number,Customer ID,Card Type,Transaction Type
70001,Settled Successfully,9/10/2026,BATCH-500,100.00,1001,cust-priya-shah,Visa xxxx4242,auth_capture
70002,Settled Successfully,9/10/2026,BATCH-500,150.00,1002,cust-marcus-liu,Mastercard xxxx5588,auth_capture
70003,Settled Successfully,9/10/2026,BATCH-500,75.00,9999,cust-dana-osei,Visa xxxx7711,auth_capture
70004,Refund Settled Successfully,9/10/2026,BATCH-500,-40.00,1001,cust-priya-shah,Visa xxxx4242,credit
70005,Voided,9/10/2026,BATCH-500,25.00,,cust-dana-osei,Visa xxxx7711,void
";

/// Imports [`SAMPLE_CSV`] against `ledger`, saving and posting every
/// document `resolve_invoice` (built from `known_invoices`) has not already
/// been saved for. Mirrors `main.rs`'s `run_authnet_import`, minus the
/// printing.
fn import_sample(ledger: &Ledger, known_invoices: &[(&str, &str, DocKind)]) -> ImportCounts {
    let rows = authnet::parse_transactions(SAMPLE_CSV).expect("parses");
    let batches = authnet::group_batches(&rows).expect("groups");
    assert_eq!(batches.len(), 1, "one batch in the sample");

    let cfg = config();
    let resolve = |number: &str| -> Option<(String, DocKind)> {
        known_invoices
            .iter()
            .find(|(inv_number, _, _)| *inv_number == number)
            .map(|(_, doc_id, kind)| (doc_id.to_string(), *kind))
    };
    let documents = authnet::settlement_documents(&batches, &cfg, &resolve);

    let ctx = PostingContext::default();
    let mut counts = ImportCounts::default();
    for doc in &documents {
        let already = ledger
            .document_version_count(COMPANY, &doc.document_id)
            .expect("reads version count")
            > 0;
        if already {
            counts.skipped += 1;
            continue;
        }
        let version = ledger
            .save_document(
                COMPANY,
                doc,
                CommandMeta {
                    actor_id: "dan".to_string(),
                    kind: "authnet_import".to_string(),
                    hlc: "hlc-authnet".to_string(),
                },
                now(),
            )
            .expect("saves");
        match post::post(doc, version.version, &ctx).expect("posts") {
            Posting::Entry(entry) => {
                let entry_id = ledger.post_entry(COMPANY, &entry, now()).expect("posts");
                match doc.kind {
                    DocKind::Settlement => counts.settlement_entry = Some(entry_id),
                    DocKind::Payment if doc.unapplied.is_zero() => counts.applied += 1,
                    DocKind::Payment => counts.unapplied += 1,
                    DocKind::RefundReceipt => counts.refunds += 1,
                    _ => {}
                }
            }
            Posting::NonPosting => panic!("authnet documents always post"),
        }
    }
    counts
}

#[derive(Default)]
struct ImportCounts {
    settlement_entry: Option<String>,
    applied: usize,
    unapplied: usize,
    refunds: usize,
    skipped: usize,
}

#[test]
fn settlement_posts_ar_relief_unapplied_and_refund_then_reimport_is_a_noop() {
    let ledger = Ledger::open_in_memory().expect("open");
    ledger
        .create_company(COMPANY, "Aquamentor LLC", None, now())
        .expect("create company");
    let (inv1, inv2) = seed_invoices(&ledger);
    let known = [
        ("1001", inv1.as_str(), DocKind::Invoice),
        ("1002", inv2.as_str(), DocKind::Invoice),
    ];

    let counts = import_sample(&ledger, &known);
    assert_eq!(counts.applied, 2, "two sales match a known invoice");
    assert_eq!(counts.unapplied, 1, "one sale matches no invoice");
    assert_eq!(counts.refunds, 1);
    assert_eq!(counts.skipped, 0);

    // -- the Settlement entry's legs: Dr 1100 net-before-fees, Cr 1160
    // net-before-fees (325.00 gross - 40.00 refunds = 285.00); no fee leg,
    // since Authorize.net's own export never states one.
    let settlement_entry_id = counts.settlement_entry.expect("settlement posted");
    let (entry, is_posted) = ledger
        .entry(COMPANY, &settlement_entry_id)
        .expect("entry exists");
    assert!(is_posted);
    assert_eq!(entry.source_type, DocKind::Settlement);
    let bank_line = entry
        .lines
        .iter()
        .find(|line| line.account == checking())
        .expect("bank leg present");
    assert_eq!(bank_line.debit, Money::from_minor(28_500));
    let clearing_line = entry
        .lines
        .iter()
        .find(|line| line.account == AccountId(chart::AUTHNET_CLEARING.to_string()))
        .expect("clearing leg present");
    assert_eq!(clearing_line.credit, Money::from_minor(28_500));
    assert!(
        entry
            .lines
            .iter()
            .all(|line| line.account != AccountId(chart::MERCHANT_FEES.to_string())),
        "no fee leg until the bank side supplies one"
    );

    // -- AR relieved on the two known invoices, 1200 credited by each sale.
    let as_of = ymd(2026, 9, 30);
    let tb = report::trial_balance(&ledger, COMPANY, as_of).expect("trial balance");
    let ar = tb
        .rows
        .iter()
        .find(|row| row.number == chart::ACCOUNTS_RECEIVABLE)
        .expect("AR row present");
    // Two invoices totalling 250.00, relieved by 100.00 + 150.00 = 250.00.
    assert_eq!(ar.balance, Money::ZERO, "AR fully relieved");

    // -- 2300 carries the unknown sale's 75.00. `balance` is signed
    // debit-positive (`report::TbRow` doc comment), so a liability's credit
    // balance shows negative.
    let deposits = tb
        .rows
        .iter()
        .find(|row| row.number == chart::CUSTOMER_DEPOSITS)
        .expect("2300 row present");
    assert_eq!(deposits.balance, Money::from_minor(-7_500));

    // -- re-import posts nothing new: every document_id was already saved.
    let reimport = import_sample(&ledger, &known);
    assert_eq!(reimport.skipped, 5, "settlement + 3 payments + 1 refund");
    assert_eq!(reimport.applied, 0);
    assert_eq!(reimport.unapplied, 0);
    assert_eq!(reimport.refunds, 0);

    // -- a bank statement line for the net minus a plausible fee matches the
    // settlement and confirms with 1160 at zero, 6700 carrying the fee.
    let fee = Money::from_minor(800); // 8.00, 2.8% of the 285.00 net.
    let net_paid = bank_line.debit.checked_sub(fee).expect("checked sub");
    let statement = NewStatement {
        period_start: ymd(2026, 9, 1),
        period_end: ymd(2026, 9, 15),
        opening_balance: Money::ZERO,
        closing_balance: net_paid,
        lines: vec![ParsedLine {
            posted_on: ymd(2026, 9, 11),
            amount: net_paid,
            description: "AUTHORIZE.NET SETTLEMENT".to_string(),
            external_id: Some("stmt-line-1".to_string()),
        }],
    };
    let import_report = ledger
        .import_statement(COMPANY, &checking(), statement, now())
        .expect("imports");
    let match_report = ledger
        .match_statement(
            COMPANY,
            &import_report.statement_id,
            &MatchRules::default(),
            now(),
        )
        .expect("matches");
    assert_eq!(match_report.exact, 0, "{match_report:?}");
    assert_eq!(match_report.settlement, 0, "{match_report:?}");
    assert_eq!(match_report.settlement_fee, 1, "{match_report:?}");
    assert!(match_report.proposals.is_empty(), "{match_report:?}");

    let tb_after = report::trial_balance(&ledger, COMPANY, as_of).expect("trial balance");
    let clearing_after = tb_after
        .rows
        .iter()
        .find(|row| row.number == chart::AUTHNET_CLEARING)
        .expect("1160 row present");
    assert_eq!(
        clearing_after.balance,
        Money::ZERO,
        "1160 nets to zero regardless of the fee"
    );
    let fees_after = tb_after
        .rows
        .iter()
        .find(|row| row.number == chart::MERCHANT_FEES)
        .expect("6700 row present");
    assert_eq!(fees_after.balance, fee, "6700 carries the discovered fee");
    let checking_after = tb_after
        .rows
        .iter()
        .find(|row| row.number == chart::CHECKING)
        .expect("1100 row present");
    assert_eq!(
        checking_after.balance, net_paid,
        "the bank account settles to what actually landed, not the settlement's guess"
    );
}

#[test]
fn a_settled_sale_with_no_invoice_number_is_unapplied_with_no_lookup() {
    let ledger = Ledger::open_in_memory().expect("open");
    ledger
        .create_company(COMPANY, "Aquamentor LLC", None, now())
        .expect("create company");

    let cfg = config();
    let text = "\
Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount,Customer ID\n\
80001,Settled Successfully,9/10/2026,BATCH-600,20.00,cust-nobody\n";
    let rows = authnet::parse_transactions(text).expect("parses");
    let batches = authnet::group_batches(&rows).expect("groups");
    let never_resolves = |_: &str| None;
    let documents = authnet::settlement_documents(&batches, &cfg, &never_resolves);
    let payment = documents
        .iter()
        .find(|doc| doc.kind == DocKind::Payment)
        .expect("a payment document");
    assert_eq!(payment.unapplied, Money::from_minor(2_000));
    assert!(payment.applications.is_empty());
}

// ---------------------------------------------------------------------------
// CLI: authnet import through the real binary.
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
fn cli_authnet_import_end_to_end_and_reimport_skips() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");
    let csv_path = dir.path().join("settlement.csv");
    std::fs::write(&csv_path, SAMPLE_CSV).unwrap();

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
        "authnet",
        "import",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--file",
        csv_path.to_str().unwrap(),
    ]);
    assert!(
        import.status.success(),
        "import failed: {}",
        stdout(&import)
    );
    let out = stdout(&import);
    assert!(out.contains("batches settled    : 1"), "{out}");
    // No invoices exist in this fresh company, so all three sales park
    // unapplied and the CLI prints a warning for each.
    assert!(out.contains("payments unapplied : 3"), "{out}");
    assert!(out.contains("refunds            : 1"), "{out}");
    assert!(out.contains("warning:"), "{out}");

    let reimport = run(&[
        "authnet",
        "import",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--file",
        csv_path.to_str().unwrap(),
    ]);
    assert!(
        reimport.status.success(),
        "reimport failed: {}",
        stdout(&reimport)
    );
    let reimport_out = stdout(&reimport);
    assert!(
        reimport_out.contains("skipped (already imported): 5"),
        "{reimport_out}"
    );
    assert!(
        reimport_out.contains("batches settled    : 0"),
        "{reimport_out}"
    );
}
