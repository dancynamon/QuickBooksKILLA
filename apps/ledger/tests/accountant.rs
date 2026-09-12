//! Accountant mode integration tests, against the public API only.
//! `LEDGER-DESIGN.md` §8.
//!
//! One small fixture carries most of this file: two Q1 invoices before a
//! February close, one unflagged March invoice and one flagged manual
//! journal entry after it — enough to exercise GL running balances, the
//! audit trail's flagged-first ordering, and a queue of adjustment requests
//! against real postings.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use ledger::accountant::{self, AccountantError, AccountantView};
use ledger::chart;
use ledger::report;
use ledger::store::{AdjustmentState, CommandMeta, Ledger, LedgerError};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, JournalEntry,
    JournalLine, LedgerDocument, LineKind,
};
use ledger_core::Money;

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn dt(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

/// A minimal saved document, unrelated to `entry`'s own lines — the
/// document-versions foreign key only needs *something* to point at, same
/// pattern `tests/store.rs` uses.
#[allow(clippy::too_many_arguments)]
fn save_and_post(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    kind: DocKind,
    txn_date: NaiveDate,
    actor: &str,
    command_kind: &str,
    mut entry: JournalEntry,
) -> String {
    let doc = LedgerDocument {
        document_id: document_id.to_string(),
        kind,
        number: None,
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
            amount: Money::from_minor(1),
            class: None,
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
        memo: entry.memo.clone(),
    };
    let version = ledger
        .save_document(
            company,
            &doc,
            CommandMeta {
                actor_id: actor.to_string(),
                kind: command_kind.to_string(),
                hlc: format!("hlc-{document_id}"),
            },
            dt(txn_date.year(), txn_date.month(), txn_date.day()),
        )
        .unwrap();
    entry.source_id = doc.document_id.clone();
    entry.source_version = version.version;
    ledger
        .post_entry(
            company,
            &entry,
            dt(txn_date.year(), txn_date.month(), txn_date.day()),
        )
        .unwrap()
}

fn sale(entry_date: NaiveDate, amount: i64, class: &str) -> JournalEntry {
    JournalEntry {
        entry_date,
        memo: Some(format!("{class} sale")),
        source_type: DocKind::Invoice,
        source_id: String::new(),
        source_version: 0,
        reversal_of: None,
        is_flagged: false,
        lines: vec![
            JournalLine::debit(
                1,
                AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                Money::from_minor(amount),
            ),
            JournalLine::credit(
                2,
                AccountId(chart::SALES_INCOME.to_string()),
                Money::from_minor(amount),
            )
            .with_class(Some(ClassId(class.to_string()))),
        ],
    }
}

/// Builds the fixture described at the top of this file. Returns the ledger
/// and entry1's id (the Jan foam sale), which several tests reclassify.
fn fixture() -> (Ledger, String) {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", None, dt(2026, 1, 1))
        .unwrap();

    let entry1 = save_and_post(
        &ledger,
        "aquamentor",
        "inv-1",
        DocKind::Invoice,
        ymd(2026, 1, 10),
        "dan",
        "save_invoice",
        sale(ymd(2026, 1, 10), 100_000, "foam"),
    );

    save_and_post(
        &ledger,
        "aquamentor",
        "inv-2",
        DocKind::Invoice,
        ymd(2026, 2, 5),
        "dan",
        "save_invoice",
        sale(ymd(2026, 2, 5), 50_000, "chair"),
    );

    ledger
        .close_period(
            "aquamentor",
            ymd(2026, 2, 28),
            "dan",
            "February close",
            dt(2026, 3, 1),
        )
        .unwrap();

    // Unflagged, dated *before* the flagged entry below — proves the audit
    // trail's flagged-first ordering is not just date order in disguise.
    save_and_post(
        &ledger,
        "aquamentor",
        "inv-3",
        DocKind::Invoice,
        ymd(2026, 3, 5),
        "dan",
        "save_invoice",
        sale(ymd(2026, 3, 5), 20_000, "foam"),
    );

    save_and_post(
        &ledger,
        "aquamentor",
        "je-1",
        DocKind::JournalEntry,
        ymd(2026, 3, 20),
        "joel",
        "import_journal_entry",
        JournalEntry {
            entry_date: ymd(2026, 3, 20),
            memo: Some("March supplies".to_string()),
            source_type: DocKind::JournalEntry,
            source_id: String::new(),
            source_version: 0,
            reversal_of: None,
            is_flagged: true,
            lines: vec![
                JournalLine::debit(
                    1,
                    AccountId(chart::SHOP_SUPPLIES.to_string()),
                    Money::from_minor(5_000),
                )
                .with_class(Some(ClassId("foam".to_string()))),
                JournalLine::credit(
                    2,
                    AccountId(chart::CHECKING.to_string()),
                    Money::from_minor(5_000),
                ),
            ],
        },
    );

    (ledger, entry1)
}

// ---------------------------------------------------------------------------
// GL detail
// ---------------------------------------------------------------------------

#[test]
fn gl_running_balances_are_right_and_every_account_sums_to_the_trial_balance() {
    let (ledger, _entry1) = fixture();
    let as_of = ymd(2026, 12, 31);

    let gl =
        accountant::general_ledger(&ledger, "aquamentor", ymd(2000, 1, 1), as_of, None).unwrap();
    let tb = report::trial_balance(&ledger, "aquamentor", as_of).unwrap();

    assert!(!gl.sections.is_empty());
    for section in &gl.sections {
        assert_eq!(
            section.opening_balance,
            Money::ZERO,
            "from predates every posting"
        );
        let tb_row = tb
            .rows
            .iter()
            .find(|r| r.account_id == section.account_id)
            .expect("gl account is in the chart");
        assert_eq!(
            section.closing_balance, tb_row.balance,
            "{} closing balance should equal its trial-balance balance",
            section.number
        );
    }

    let ar = gl
        .sections
        .iter()
        .find(|s| s.number == chart::ACCOUNTS_RECEIVABLE)
        .unwrap();
    let running: Vec<Money> = ar.lines.iter().map(|l| l.running_balance).collect();
    assert_eq!(
        running,
        vec![
            Money::from_minor(100_000),
            Money::from_minor(150_000),
            Money::from_minor(170_000),
        ]
    );
}

#[test]
fn gl_can_be_restricted_to_one_account() {
    let (ledger, _entry1) = fixture();
    let account = AccountId(chart::ACCOUNTS_RECEIVABLE.to_string());
    let gl = accountant::general_ledger(
        &ledger,
        "aquamentor",
        ymd(2000, 1, 1),
        ymd(2026, 12, 31),
        Some(&account),
    )
    .unwrap();
    assert_eq!(gl.sections.len(), 1);
    assert_eq!(gl.sections[0].account_id, account);
}

// ---------------------------------------------------------------------------
// Audit trail
// ---------------------------------------------------------------------------

#[test]
fn audit_trail_since_the_last_close_lists_the_flagged_entry_first_with_its_actor() {
    let (ledger, _entry1) = fixture();
    let rows = accountant::audit_trail(&ledger, "aquamentor", None).unwrap();

    assert_eq!(rows.len(), 2, "only postings after the Feb close: {rows:?}");
    assert!(rows[0].is_flagged, "the flagged entry must sort first");
    assert_eq!(rows[0].actor, "joel");
    assert_eq!(rows[0].command_kind, "import_journal_entry");
    assert_eq!(rows[0].entry_date, ymd(2026, 3, 20));

    assert!(!rows[1].is_flagged);
    assert_eq!(rows[1].actor, "dan");
    assert_eq!(rows[1].entry_date, ymd(2026, 3, 5));
}

#[test]
fn audit_trail_since_an_explicit_date_overrides_the_last_close() {
    let (ledger, _entry1) = fixture();
    let rows = accountant::audit_trail(&ledger, "aquamentor", Some(ymd(2026, 1, 1))).unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].is_flagged);
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

#[test]
fn export_pack_writes_five_files_and_the_trial_balance_csv_balances() {
    let (ledger, _entry1) = fixture();
    let dir = tempfile::tempdir().unwrap();

    let paths =
        accountant::export_pack(&ledger, "aquamentor", ymd(2026, 12, 31), dir.path()).unwrap();
    assert_eq!(paths.len(), 5);

    let names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "tb-2026-12-31.csv",
            "pnl-2026.csv",
            "bs-2026-12-31.csv",
            "gl-2026.csv",
            "close-history.csv",
        ]
    );
    for path in &paths {
        assert!(path.exists(), "{path:?} should exist");
    }

    let tb_csv = std::fs::read_to_string(&paths[0]).unwrap();
    let mut lines = tb_csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "account_number,account_name,classification,debit,credit,balance"
    );
    let total_line = tb_csv.lines().find(|l| l.starts_with("TOTAL,")).unwrap();
    let fields: Vec<&str> = total_line.split(',').collect();
    assert_eq!(
        fields[3], fields[4],
        "TB CSV totals should be equal: {total_line}"
    );

    let gl_csv = std::fs::read_to_string(&paths[3]).unwrap();
    assert!(gl_csv.starts_with(
        "account_number,account_name,entry_id,entry_date,source_type,source_id,memo,class,\
         entity_kind,entity_id,is_flagged,reversal_of,debit,credit,running_balance\n"
    ));

    let history_csv = std::fs::read_to_string(&paths[4]).unwrap();
    assert_eq!(
        history_csv.lines().next().unwrap(),
        "seq,at,actor,moved_from,moved_to,is_reopen,note"
    );
    assert_eq!(history_csv.lines().count(), 2, "one close, one header row");
}

// ---------------------------------------------------------------------------
// The adjusting-entry request queue
// ---------------------------------------------------------------------------

fn balanced_lines() -> Vec<JournalLine> {
    vec![
        JournalLine::debit(
            1,
            AccountId(chart::SHOP_SUPPLIES.to_string()),
            Money::from_minor(1_234),
        )
        .with_class(Some(ClassId("foam".to_string()))),
        JournalLine::credit(
            2,
            AccountId(chart::CREDIT_CARD.to_string()),
            Money::from_minor(1_234),
        ),
    ]
}

#[test]
fn a_balanced_proposal_is_accepted() {
    let (ledger, _entry1) = fixture();
    let request = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "shop supplies on the card",
        balanced_lines(),
        dt(2026, 4, 1),
    )
    .unwrap();
    assert_eq!(request.state, AdjustmentState::Proposed);
    assert!(!request.period_closed);
}

#[test]
fn an_unbalanced_proposal_is_rejected_at_proposal_time() {
    let (ledger, _entry1) = fixture();
    let mut lines = balanced_lines();
    lines[1].credit = Money::from_minor(999);
    let err = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "oops",
        lines,
        dt(2026, 4, 1),
    )
    .unwrap_err();
    assert!(matches!(err, AccountantError::Unbalanced));
}

#[test]
fn an_income_line_without_a_class_is_rejected_at_proposal_time() {
    let (ledger, _entry1) = fixture();
    let lines = vec![
        JournalLine::credit(
            1,
            AccountId(chart::SALES_INCOME.to_string()),
            Money::from_minor(1_000),
        ),
        JournalLine::debit(
            2,
            AccountId(chart::SHOP_SUPPLIES.to_string()),
            Money::from_minor(1_000),
        )
        .with_class(Some(ClassId("foam".to_string()))),
    ];
    let err =
        accountant::propose_adjustment(&ledger, "aquamentor", "joel", "bad", lines, dt(2026, 4, 1))
            .unwrap_err();
    assert!(matches!(err, AccountantError::MissingClass { line_no: 1 }));
}

#[test]
fn approving_posts_a_flagged_entry_and_the_trial_balance_moves() {
    let (ledger, _entry1) = fixture();
    let request = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "shop supplies on the card",
        balanced_lines(),
        dt(2026, 4, 1),
    )
    .unwrap();

    let tb_before = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();

    let decided = accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        true,
        "approved",
        dt(2026, 4, 2),
    )
    .unwrap();
    assert_eq!(decided.state, AdjustmentState::Posted);
    let entry_id = decided
        .posted_entry_id
        .clone()
        .expect("posted_entry_id set");

    let (entry, is_posted) = ledger.entry("aquamentor", &entry_id).unwrap();
    assert!(is_posted);
    assert!(
        entry.is_flagged,
        "an approved adjustment posts flagged (§8)"
    );

    let tb_after = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();
    let before = |number: &str| {
        tb_before
            .rows
            .iter()
            .find(|r| r.number == number)
            .unwrap()
            .balance
    };
    let after = |number: &str| {
        tb_after
            .rows
            .iter()
            .find(|r| r.number == number)
            .unwrap()
            .balance
    };

    assert_eq!(
        after(chart::SHOP_SUPPLIES)
            .checked_sub(before(chart::SHOP_SUPPLIES))
            .unwrap(),
        Money::from_minor(1_234)
    );
    assert_eq!(
        after(chart::CREDIT_CARD)
            .checked_sub(before(chart::CREDIT_CARD))
            .unwrap(),
        Money::from_minor(-1_234)
    );
}

#[test]
fn rejecting_records_the_decision_and_posts_nothing() {
    let (ledger, _entry1) = fixture();
    let request = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "shop supplies on the card",
        balanced_lines(),
        dt(2026, 4, 1),
    )
    .unwrap();

    let tb_before = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();

    let decided = accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        false,
        "not this quarter",
        dt(2026, 4, 2),
    )
    .unwrap();
    assert_eq!(decided.state, AdjustmentState::Rejected);
    assert_eq!(decided.posted_entry_id, None);
    assert_eq!(decided.decision_note.as_deref(), Some("not this quarter"));

    let tb_after = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();
    assert_eq!(tb_before, tb_after, "a rejection posts nothing");
}

#[test]
fn deciding_twice_errors() {
    let (ledger, _entry1) = fixture();
    let request = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "shop supplies on the card",
        balanced_lines(),
        dt(2026, 4, 1),
    )
    .unwrap();
    accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        true,
        "ok",
        dt(2026, 4, 2),
    )
    .unwrap();

    let err = accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        false,
        "too late",
        dt(2026, 4, 3),
    )
    .unwrap_err();
    assert!(matches!(err, AccountantError::AlreadyDecided { .. }));
}

#[test]
fn reclassify_produces_a_balanced_two_line_proposal_and_approving_moves_the_balance() {
    let (ledger, entry1) = fixture();

    // entry1's line 2 is the Jan $1,000.00 foam sale credited to Sales
    // Income (4100). Reclassify it to Shipping Income (4300), class chair.
    let request = accountant::propose_reclassify(
        &ledger,
        "aquamentor",
        &entry1,
        2,
        AccountId(chart::SHIPPING_INCOME.to_string()),
        Some(ClassId("chair".to_string())),
        "joel",
        dt(2026, 4, 1),
    )
    .unwrap();

    assert_eq!(request.lines.len(), 2);
    let total_debit = Money::checked_sum(request.lines.iter().map(|l| l.debit)).unwrap();
    let total_credit = Money::checked_sum(request.lines.iter().map(|l| l.credit)).unwrap();
    assert_eq!(
        total_debit, total_credit,
        "a reclassify proposal always balances"
    );
    assert_eq!(total_debit, Money::from_minor(100_000));

    let tb_before = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();
    accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        true,
        "reclassify approved",
        dt(2026, 4, 2),
    )
    .unwrap();
    let tb_after = report::trial_balance(&ledger, "aquamentor", ymd(2026, 12, 31)).unwrap();

    let before = |number: &str| {
        tb_before
            .rows
            .iter()
            .find(|r| r.number == number)
            .unwrap()
            .balance
    };
    let after = |number: &str| {
        tb_after
            .rows
            .iter()
            .find(|r| r.number == number)
            .unwrap()
            .balance
    };

    assert_eq!(
        after(chart::SALES_INCOME)
            .checked_sub(before(chart::SALES_INCOME))
            .unwrap(),
        Money::from_minor(100_000)
    );
    assert_eq!(
        after(chart::SHIPPING_INCOME)
            .checked_sub(before(chart::SHIPPING_INCOME))
            .unwrap(),
        Money::from_minor(-100_000)
    );

    // The original entry is untouched — a reclassify never edits it.
    let (original, is_posted) = ledger.entry("aquamentor", &entry1).unwrap();
    assert!(is_posted);
    assert_eq!(
        original.lines[1].account,
        AccountId(chart::SALES_INCOME.to_string())
    );
}

#[test]
fn a_proposal_dated_into_the_closed_period_is_flagged_and_cannot_be_approved_without_a_reopen() {
    let (ledger, _entry1) = fixture();
    // February 15 is on or before the February 28 close (§8).
    let request = accountant::propose_adjustment(
        &ledger,
        "aquamentor",
        "joel",
        "late February adjustment",
        balanced_lines(),
        dt(2026, 2, 15),
    )
    .unwrap();
    assert!(request.period_closed);

    let err = accountant::decide_adjustment(
        &ledger,
        "aquamentor",
        &request.request_id,
        "dan",
        true,
        "try anyway",
        dt(2026, 4, 5),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        AccountantError::Ledger(LedgerError::PeriodClosed { .. })
    ));
}

// ---------------------------------------------------------------------------
// AccountantView: reads and propose_*, nothing else
// ---------------------------------------------------------------------------

#[test]
fn accountant_view_exposes_the_reads_and_propose_functions() {
    let (ledger, _entry1) = fixture();
    let view = AccountantView::new(&ledger, "aquamentor");

    let tb = view.trial_balance(ymd(2026, 12, 31)).unwrap();
    assert_eq!(tb.total_debits, tb.total_credits);

    let gl = view
        .general_ledger(ymd(2000, 1, 1), ymd(2026, 12, 31), None)
        .unwrap();
    assert!(!gl.sections.is_empty());

    let audit = view.audit_trail(None).unwrap();
    assert_eq!(audit.len(), 2);

    assert!(view.close_history().unwrap().len() == 1);

    let proposed = view
        .propose_adjustment("joel", "view test", balanced_lines(), dt(2026, 4, 1))
        .unwrap();
    assert_eq!(proposed.state, AdjustmentState::Proposed);

    let listed = view
        .list_adjustments(Some(AdjustmentState::Proposed))
        .unwrap();
    assert!(listed.iter().any(|r| r.request_id == proposed.request_id));

    // AccountantView has no post_entry, close_period, reopen_period or
    // save_document method — there is nothing to call here to prove that;
    // its absence is checked by every other module in this crate never
    // needing to route a write through this type, and by the fact that the
    // lines above are the entirety of what `AccountantView`'s inherent impl
    // offers (see `src/accountant.rs`).
}

// ---------------------------------------------------------------------------
// CLI: adjust propose -> list -> approve -> tb moved
// ---------------------------------------------------------------------------

fn run(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(args)
        .output()
        .expect("spawn the ledger binary")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_adjust_propose_list_approve_moves_the_trial_balance() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");

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

    let propose = run(&[
        "adjust",
        "propose",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--by",
        "joel",
        "--desc",
        "shop supplies on the card",
        "--line",
        "6100:dr:12.34:foam",
        "--line",
        "2100:cr:12.34",
    ]);
    assert!(
        propose.status.success(),
        "propose failed: {}",
        stdout(&propose)
    );
    let propose_out = stdout(&propose);
    let request_id = propose_out
        .split_whitespace()
        .next()
        .expect("propose prints the request id first")
        .to_string();

    let list = run(&[
        "adjust",
        "list",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--state",
        "proposed",
    ]);
    assert!(list.status.success(), "list failed: {}", stdout(&list));
    assert!(stdout(&list).contains(&request_id));

    let approve = run(&[
        "adjust",
        "approve",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--id",
        &request_id,
        "--by",
        "dan",
        "--note",
        "approved",
    ]);
    assert!(
        approve.status.success(),
        "approve failed: {}",
        stdout(&approve)
    );
    assert!(stdout(&approve).contains("Posted"));

    let tb = run(&[
        "tb",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2030-12-31",
    ]);
    assert!(tb.status.success(), "tb failed: {}", stdout(&tb));
    let tb_out = stdout(&tb);
    assert!(
        tb_out.contains("12.34"),
        "expected the approved adjustment's amount in the trial balance:\n{tb_out}"
    );
    assert!(
        tb_out.contains("balanced") && !tb_out.contains("DOES NOT BALANCE"),
        "{tb_out}"
    );
}
