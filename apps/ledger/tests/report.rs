//! Report integration tests, against the public API only.
//! `LEDGER-DESIGN.md` §7, §9.
//!
//! One small invented quarter carries every test: a taxable Aquamentor
//! invoice, an out-of-state exempt one, an unapplied customer payment, and
//! one manual opening-balance journal entry — enough to exercise the trial
//! balance, the P&L, the balance sheet, the sales tax lines and the QBO
//! diff's three tiers against numbers worked out by hand below.

use chrono::{DateTime, NaiveDate, Utc};
use ledger::chart;
use ledger::post::{post as post_doc, Posting};
use ledger::report::{self, QboTbRow, Tier, TierRules};
use ledger::store::{CommandMeta, DocumentVersionId, Ledger};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, JournalEntry,
    JournalLine, LedgerDocument, LineKind, PostingConfig, PostingContext, TaxDetail,
};
use ledger_core::Money;
use rust_decimal_macros::dec;

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-12T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn save_doc(
    ledger: &Ledger,
    company: &str,
    id: &str,
    kind: DocKind,
    txn_date: NaiveDate,
) -> DocumentVersionId {
    let doc = LedgerDocument {
        document_id: id.to_string(),
        kind,
        number: Some(id.to_string()),
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
            amount: Money::from_minor(100000),
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
        memo: None,
    };
    ledger
        .save_document(
            company,
            &doc,
            CommandMeta {
                actor_id: "dan".to_string(),
                kind: "test".to_string(),
                hlc: "hlc".to_string(),
            },
            now(),
        )
        .unwrap()
}

/// Builds the fixture quarter described at the top of this file and returns
/// the ledger ready to report on, as of 30 June 2026.
fn fixture() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", None, now())
        .unwrap();

    // 1200 and 2200 get QBO source refs so the diff test has something to
    // join against; the rest stay unmapped, which is realistic.
    ledger
        .add_account(
            "aquamentor",
            chart::ACCOUNTS_RECEIVABLE,
            "Accounts receivable",
            chart::Classification::Asset,
            false,
            Some("qbo-ar"),
            false,
        )
        .unwrap();
    ledger
        .add_account(
            "aquamentor",
            chart::SALES_TAX_PAYABLE,
            "Sales tax payable",
            chart::Classification::Liability,
            false,
            Some("qbo-tax"),
            false,
        )
        .unwrap();

    // Doc 1: a taxable $1,000.00 foam sale, 6.625% NJ tax = $66.25.
    let doc1 = save_doc(
        &ledger,
        "aquamentor",
        "inv-1",
        DocKind::Invoice,
        ymd(2026, 4, 10),
    );
    ledger
        .post_entry(
            "aquamentor",
            &JournalEntry {
                entry_date: ymd(2026, 4, 10),
                memo: Some("taxable foam sale".to_string()),
                source_type: DocKind::Invoice,
                source_id: doc1.document_id.clone(),
                source_version: doc1.version,
                reversal_of: None,
                is_flagged: false,
                lines: vec![
                    JournalLine::debit(
                        1,
                        AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                        Money::from_minor(106625),
                    ),
                    JournalLine::credit(
                        2,
                        AccountId(chart::SALES_INCOME.to_string()),
                        Money::from_minor(100000),
                    )
                    .with_class(Some(ClassId("foam".to_string()))),
                    JournalLine::credit(
                        3,
                        AccountId(chart::SALES_TAX_PAYABLE.to_string()),
                        Money::from_minor(6625),
                    ),
                ],
            },
            now(),
        )
        .unwrap();

    // Doc 2: an out-of-state exempt $500.00 chair sale, no tax.
    let doc2 = save_doc(
        &ledger,
        "aquamentor",
        "inv-2",
        DocKind::Invoice,
        ymd(2026, 5, 5),
    );
    ledger
        .post_entry(
            "aquamentor",
            &JournalEntry {
                entry_date: ymd(2026, 5, 5),
                memo: Some("exempt chair sale".to_string()),
                source_type: DocKind::Invoice,
                source_id: doc2.document_id.clone(),
                source_version: doc2.version,
                reversal_of: None,
                is_flagged: false,
                lines: vec![
                    JournalLine::debit(
                        1,
                        AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
                        Money::from_minor(50000),
                    ),
                    JournalLine::credit(
                        2,
                        AccountId(chart::SALES_INCOME.to_string()),
                        Money::from_minor(50000),
                    )
                    .with_class(Some(ClassId("chair".to_string()))),
                ],
            },
            now(),
        )
        .unwrap();

    // Doc 3: $200.00 received and not yet applied to any invoice (W8).
    let doc3 = save_doc(
        &ledger,
        "aquamentor",
        "pmt-1",
        DocKind::Payment,
        ymd(2026, 5, 10),
    );
    ledger
        .post_entry(
            "aquamentor",
            &JournalEntry {
                entry_date: ymd(2026, 5, 10),
                memo: Some("unapplied payment".to_string()),
                source_type: DocKind::Payment,
                source_id: doc3.document_id.clone(),
                source_version: doc3.version,
                reversal_of: None,
                is_flagged: false,
                lines: vec![
                    JournalLine::debit(
                        1,
                        AccountId(chart::CHECKING.to_string()),
                        Money::from_minor(20000),
                    ),
                    JournalLine::credit(
                        2,
                        AccountId(chart::CUSTOMER_DEPOSITS.to_string()),
                        Money::from_minor(20000),
                    ),
                ],
            },
            now(),
        )
        .unwrap();

    // Doc 4: a manual opening-balance entry into 3950 (§6), the one account
    // the §7 diff never looks at.
    let doc4 = save_doc(
        &ledger,
        "aquamentor",
        "je-1",
        DocKind::JournalEntry,
        ymd(2026, 1, 1),
    );
    ledger
        .post_entry(
            "aquamentor",
            &JournalEntry {
                entry_date: ymd(2026, 1, 1),
                memo: Some("opening balance".to_string()),
                source_type: DocKind::JournalEntry,
                source_id: doc4.document_id.clone(),
                source_version: doc4.version,
                reversal_of: None,
                is_flagged: true,
                lines: vec![
                    JournalLine::debit(
                        1,
                        AccountId(chart::PREPAID.to_string()),
                        Money::from_minor(50000),
                    ),
                    JournalLine::credit(
                        2,
                        AccountId(chart::OPENING_BALANCE_EQUITY.to_string()),
                        Money::from_minor(50000),
                    ),
                ],
            },
            now(),
        )
        .unwrap();

    ledger
}

const AS_OF: fn() -> NaiveDate = || NaiveDate::from_ymd_opt(2026, 6, 30).unwrap();

#[test]
fn trial_balance_balances_and_flags_no_wrong_side_accounts() {
    let ledger = fixture();
    let tb = report::trial_balance(&ledger, "aquamentor", AS_OF()).unwrap();
    assert_eq!(tb.total_debits, tb.total_credits);
    assert!(
        tb.wrong_side.is_empty(),
        "unexpected wrong-side accounts: {:?}",
        tb.wrong_side
    );

    let ar = tb
        .rows
        .iter()
        .find(|r| r.number == chart::ACCOUNTS_RECEIVABLE)
        .unwrap();
    assert_eq!(ar.balance, Money::from_minor(156625));
}

#[test]
fn balance_sheet_balances_with_current_year_net_income_folded_into_equity() {
    let ledger = fixture();
    let bs = report::balance_sheet(&ledger, "aquamentor", AS_OF()).unwrap();
    assert_eq!(bs.assets.total, bs.total_liabilities_and_equity);
    assert_eq!(bs.assets.total, Money::from_minor(226625));

    let net_income_row = bs
        .equity
        .rows
        .iter()
        .find(|r| r.account_id.is_none())
        .unwrap();
    assert_eq!(net_income_row.balance, Money::from_minor(150000));
}

#[test]
fn profit_and_loss_by_class_sums_to_the_section_total() {
    let ledger = fixture();
    let pnl = report::profit_and_loss(&ledger, "aquamentor", ymd(2026, 1, 1), AS_OF()).unwrap();

    let by_class_sum = pnl
        .income
        .by_class
        .iter()
        .try_fold(Money::ZERO, |acc, (_, amount)| acc.checked_add(*amount))
        .unwrap();
    assert_eq!(by_class_sum, pnl.income.total);
    assert_eq!(pnl.income.total, Money::from_minor(150000));

    let foam = pnl
        .income
        .by_class
        .iter()
        .find(|(class, _)| class.as_ref().map(|c| c.0.as_str()) == Some("foam"))
        .unwrap();
    assert_eq!(foam.1, Money::from_minor(100000));

    // No COGS or expense entries in the fixture: net income is total income.
    assert_eq!(pnl.net_income, Money::from_minor(150000));
}

#[test]
fn sales_tax_lines_reproduces_a_b_c_d_and_the_st50_mapping() {
    let ledger = fixture();
    let lines = report::sales_tax_lines(&ledger, "aquamentor", (2026, 2), dec!(0.06625)).unwrap();

    assert_eq!(lines.a_total_income, Money::from_minor(150000));
    assert_eq!(lines.b_tax_collected, Money::from_minor(6625));
    assert_eq!(lines.c_taxable_sales, Money::from_minor(100000));
    assert_eq!(lines.d_nontaxable_sales, Money::from_minor(50000));
    // Neither fixture invoice's lines were posted with `is_taxable` set (they
    // are built by hand, straight to `post_entry`, not through `post::post`),
    // so E is zero and the variance against C is the whole of C.
    assert_eq!(lines.e_line_level_taxable, Some(Money::ZERO));
    assert_eq!(lines.variance, Some(Money::from_minor(-100000)));
    assert_eq!(
        lines.st50,
        [
            (1, Money::from_minor(150000)),
            (2, Money::from_minor(50000)),
            (3, Money::from_minor(100000)),
        ]
    );
}

#[test]
fn balances_by_source_ref_reports_the_mapped_accounts() {
    let ledger = fixture();
    let rows = report::balances_by_source_ref(&ledger, "aquamentor", AS_OF()).unwrap();

    assert!(rows.contains(&(
        Some("qbo-ar".to_string()),
        AccountId(chart::ACCOUNTS_RECEIVABLE.to_string()),
        Money::from_minor(156625)
    )));
    assert!(rows.contains(&(
        Some("qbo-tax".to_string()),
        AccountId(chart::SALES_TAX_PAYABLE.to_string()),
        Money::from_minor(-6625)
    )));
}

#[test]
fn tb_diff_flags_a_must_tier_delta_accepts_ar_as_1200_plus_2300_and_skips_3950() {
    let ledger = fixture();
    let tb = report::trial_balance(&ledger, "aquamentor", AS_OF()).unwrap();

    let qbo = vec![
        // AR combined (1200 + 2300) is $1,566.25 - $200.00 = $1,366.25 — the
        // QBO figure that matches, even though 1200 alone ($1,566.25) does not.
        QboTbRow {
            qbo_account_id: "qbo-ar".to_string(),
            name: "Accounts Receivable".to_string(),
            balance: Money::from_minor(136625),
        },
        // Deliberately wrong, to prove a must-tier delta is flagged.
        QboTbRow {
            qbo_account_id: "qbo-tax".to_string(),
            name: "Sales Tax Payable".to_string(),
            balance: Money::from_minor(-6000),
        },
    ];

    let diff = report::tb_diff(&tb, &qbo, &TierRules::default()).unwrap();

    assert!(
        diff.must_failures
            .contains(&"Sales tax payable (2200)".to_string()),
        "expected a must-tier failure on 2200, got {:?}",
        diff.must_failures
    );
    assert!(
        !diff
            .must_failures
            .contains(&"Accounts receivable (1200+2300)".to_string()),
        "AR should be accepted as 1200+2300, must_failures = {:?}",
        diff.must_failures
    );

    let equity_row = diff
        .rows
        .iter()
        .find(|r| r.label == "Total equity (3xxx)")
        .unwrap();
    assert_eq!(
        equity_row.ledger_amount,
        Money::ZERO,
        "3950 must be excluded from the total-equity comparison"
    );
    assert_eq!(equity_row.tier, Tier::Must);
}

// ---------------------------------------------------------------------------
// §9 line E: per-line taxability, posted through `post::post` itself rather
// than hand-built entries, then read back from the store and reported on.
// ---------------------------------------------------------------------------

#[test]
fn per_line_taxability_persists_and_drives_sales_tax_line_e() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", None, now())
        .unwrap();

    let doc = LedgerDocument {
        document_id: "inv-tax-1".to_string(),
        kind: DocKind::Invoice,
        number: Some("INV-TAX-1".to_string()),
        txn_date: ymd(2026, 4, 15),
        due_date: None,
        contact: Some(ContactRef {
            kind: ContactKind::Customer,
            id: "cust-1".to_string(),
        }),
        header_class: None,
        lines: vec![
            // A taxable $1,000.00 foam sale.
            DocLine {
                line_no: 1,
                kind: LineKind::Item,
                amount: Money::from_minor(100000),
                class: Some(ClassId("foam".to_string())),
                item_id: None,
                account: None,
                is_taxable: true,
                qty: None,
                unit_cost: None,
                description: Some("taxable foam sale".to_string()),
                posting: None,
                entity: None,
            },
            // A $500.00 out-of-state exempt chair sale on the same invoice.
            DocLine {
                line_no: 2,
                kind: LineKind::Item,
                amount: Money::from_minor(50000),
                class: Some(ClassId("chair".to_string())),
                item_id: None,
                account: None,
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: Some("exempt chair sale".to_string()),
                posting: None,
                entity: None,
            },
        ],
        tax: Some(TaxDetail {
            total_tax: Money::from_minor(6625),
            taxable_base: Money::from_minor(100000),
            rate: dec!(0.06625),
        }),
        deposit_to: None,
        pay_from: None,
        applications: Vec::<Application>::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: None,
    };

    let version = ledger
        .save_document(
            "aquamentor",
            &doc,
            CommandMeta {
                actor_id: "dan".to_string(),
                kind: "save_invoice".to_string(),
                hlc: "hlc-1".to_string(),
            },
            now(),
        )
        .unwrap();

    let ctx = PostingContext {
        items: Default::default(),
        customers: Default::default(),
        config: PostingConfig::default(),
    };
    let Posting::Entry(entry) = post_doc(&doc, version.version, &ctx).unwrap() else {
        panic!("an invoice with lines always posts an entry");
    };
    ledger.post_entry("aquamentor", &entry, now()).unwrap();

    // The persisted 4100 lines each carry their own flags: the taxable line
    // its share of the tax and the rate, the exempt one neither.
    let for_doc = ledger
        .entries_for_document("aquamentor", "inv-tax-1")
        .unwrap();
    assert_eq!(for_doc.len(), 1);
    let stored = &for_doc[0].1;

    let taxable_line = stored
        .lines
        .iter()
        .find(|line| line.class.as_ref().map(|c| c.0.as_str()) == Some("foam"))
        .expect("the taxable line's own 4100 line");
    assert!(taxable_line.is_taxable);
    assert_eq!(taxable_line.tax_amount, Some(Money::from_minor(6625)));
    assert_eq!(taxable_line.tax_rate, Some(dec!(0.06625)));

    let exempt_line = stored
        .lines
        .iter()
        .find(|line| line.class.as_ref().map(|c| c.0.as_str()) == Some("chair"))
        .expect("the exempt line's own 4100 line");
    assert!(!exempt_line.is_taxable);
    assert_eq!(exempt_line.tax_amount, None);
    assert_eq!(exempt_line.tax_rate, None);

    // §9 line E: only the taxable line's amount counts, and it matches C
    // exactly here because B was computed from the same $1,000.00 base.
    let lines = report::sales_tax_lines(&ledger, "aquamentor", (2026, 2), dec!(0.06625)).unwrap();
    assert_eq!(lines.e_line_level_taxable, Some(Money::from_minor(100000)));
    assert_eq!(lines.c_taxable_sales, Money::from_minor(100000));
    assert_eq!(lines.variance, Some(Money::ZERO));
}
