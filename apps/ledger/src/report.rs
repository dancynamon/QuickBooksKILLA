//! Read-only reports over the ledger. `LEDGER-DESIGN.md` §7, §9.
//!
//! Nothing here writes. Every function takes an `as_of` or a date range and
//! reads posted entries directly off [`crate::store::Ledger`]'s connection —
//! there is no second copy of the balances to keep in sync.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use rusqlite::params;
use rust_decimal::Decimal;

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::chart::{self, Classification};
use crate::store::{Ledger, LedgerError};
use crate::types::{AccountId, ClassId, DocKind};

// ---------------------------------------------------------------------------
// Trial balance (§7)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct TbRow {
    pub account_id: AccountId,
    pub number: String,
    pub name: String,
    pub classification: Classification,
    pub is_contra: bool,
    pub needs_mapping: bool,
    pub source_ref: Option<String>,
    pub debit: Money,
    pub credit: Money,
    /// Signed, debit positive.
    pub balance: Money,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrialBalance {
    pub rows: Vec<TbRow>,
    pub total_debits: Money,
    pub total_credits: Money,
    /// Non-contra accounts whose balance sits on the side opposite their
    /// `normal_balance`. Contra accounts never appear here (§2: "a warning
    /// that is always on is noise").
    pub wrong_side: Vec<AccountId>,
}

pub fn trial_balance(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
) -> Result<TrialBalance, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT a.account_id, a.number, a.name, a.classification, a.is_contra,
                a.needs_mapping, a.source_ref, a.normal_balance,
                COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM accounts a
         LEFT JOIN journal_lines l
             ON l.company_id = a.company_id AND l.account_id = a.account_id
         LEFT JOIN journal_entries e
             ON e.company_id = l.company_id AND e.entry_id = l.entry_id
             AND e.is_posted = 1 AND e.entry_date <= ?2
         WHERE a.company_id = ?1
         GROUP BY a.account_id
         ORDER BY a.number",
    )?;

    let raw = stmt
        .query_map(params![company, as_of.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, i64>(5)? != 0,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

    let mut rows = Vec::with_capacity(raw.len());
    let mut total_debits = Money::ZERO;
    let mut total_credits = Money::ZERO;
    let mut wrong_side = Vec::new();

    for (
        account_id,
        number,
        name,
        classification_raw,
        is_contra,
        needs_mapping,
        source_ref,
        normal_balance_raw,
        debit_minor,
        credit_minor,
    ) in raw
    {
        let classification = Classification::parse(&classification_raw)
            .ok_or_else(|| LedgerError::UnknownAccount(account_id.clone()))?;
        let debit = Money::from_minor(debit_minor);
        let credit = Money::from_minor(credit_minor);
        let balance = debit.checked_sub(credit)?;

        total_debits = total_debits.checked_add(debit)?;
        total_credits = total_credits.checked_add(credit)?;

        if !is_contra && !balance.is_zero() {
            let normal_is_debit = normal_balance_raw == "Dr";
            let on_wrong_side = if normal_is_debit {
                balance.is_negative()
            } else {
                !balance.is_negative()
            };
            if on_wrong_side {
                wrong_side.push(AccountId(account_id.clone()));
            }
        }

        rows.push(TbRow {
            account_id: AccountId(account_id),
            number,
            name,
            classification,
            is_contra,
            needs_mapping,
            source_ref,
            debit,
            credit,
            balance,
        });
    }

    Ok(TrialBalance {
        rows,
        total_debits,
        total_credits,
        wrong_side,
    })
}

// ---------------------------------------------------------------------------
// Profit and loss (§1, §9 line A)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BySection {
    pub by_account: Vec<(AccountId, Money)>,
    pub by_class: Vec<(Option<ClassId>, Money)>,
    pub total: Money,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pnl {
    pub income: BySection,
    pub cogs: BySection,
    pub expense: BySection,
    pub gross_margin: Money,
    pub net_income: Money,
}

#[derive(Default)]
struct SectionBuilder {
    by_account: BTreeMap<String, Money>,
    by_class: BTreeMap<Option<String>, Money>,
    total: Money,
}

impl SectionBuilder {
    fn add(
        &mut self,
        account_id: String,
        class_id: Option<String>,
        amount: Money,
    ) -> Result<(), LedgerError> {
        let acc = self.by_account.entry(account_id).or_insert(Money::ZERO);
        *acc = acc.checked_add(amount)?;
        let cls = self.by_class.entry(class_id).or_insert(Money::ZERO);
        *cls = cls.checked_add(amount)?;
        self.total = self.total.checked_add(amount)?;
        Ok(())
    }

    fn finish(self) -> BySection {
        BySection {
            by_account: self
                .by_account
                .into_iter()
                .map(|(k, v)| (AccountId(k), v))
                .collect(),
            by_class: self
                .by_class
                .into_iter()
                .map(|(k, v)| (k.map(ClassId), v))
                .collect(),
            total: self.total,
        }
    }
}

/// Income, COGS and expense between `from` and `to` inclusive. Amounts are
/// reported on the section's normal side: income is `credit - debit` (so a
/// contra account like 4900 discounts, itself debit-heavy, correctly reduces
/// the total), COGS and expense are `debit - credit` (so 5300 parts and
/// labour applied, credit-heavy, correctly reduces COGS).
pub fn profit_and_loss(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Pnl, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT a.account_id, a.classification, l.class_id,
                COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id
         WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date BETWEEN ?2 AND ?3
           AND a.classification IN ('Income', 'COGS', 'Expense')
         GROUP BY a.account_id, l.class_id
         ORDER BY a.number",
    )?;

    let raw = stmt
        .query_map(params![company, from.to_string(), to.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

    let mut income = SectionBuilder::default();
    let mut cogs = SectionBuilder::default();
    let mut expense = SectionBuilder::default();

    for (account_id, classification, class_id, debit_minor, credit_minor) in raw {
        let debit = Money::from_minor(debit_minor);
        let credit = Money::from_minor(credit_minor);
        let (builder, amount) = match classification.as_str() {
            "Income" => (&mut income, credit.checked_sub(debit)?),
            "COGS" => (&mut cogs, debit.checked_sub(credit)?),
            _ => (&mut expense, debit.checked_sub(credit)?),
        };
        builder.add(account_id, class_id, amount)?;
    }

    let income = income.finish();
    let cogs = cogs.finish();
    let expense = expense.finish();

    let gross_margin = income.total.checked_sub(cogs.total)?;
    let net_income = gross_margin.checked_sub(expense.total)?;

    Ok(Pnl {
        income,
        cogs,
        expense,
        gross_margin,
        net_income,
    })
}

// ---------------------------------------------------------------------------
// Balance sheet (§7)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct BsRow {
    /// `None` for the synthetic "current year net income" line folded into
    /// equity, which names no account.
    pub account_id: Option<AccountId>,
    pub name: String,
    pub balance: Money,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BsSection {
    pub rows: Vec<BsRow>,
    pub total: Money,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BalanceSheet {
    pub assets: BsSection,
    pub liabilities: BsSection,
    pub equity: BsSection,
    pub total_liabilities_and_equity: Money,
}

/// Assets, liabilities and equity as of `as_of`, with the current fiscal
/// year's (calendar year, D7) net income folded into equity as its own row
/// so the sheet balances without a formal closing entry.
pub fn balance_sheet(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
) -> Result<BalanceSheet, LedgerError> {
    let mut assets = BsSection::default();
    let mut liabilities = BsSection::default();
    let mut equity = BsSection::default();

    let mut stmt = ledger.conn().prepare(
        "SELECT a.account_id, a.name, a.classification,
                COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM accounts a
         LEFT JOIN journal_lines l
             ON l.company_id = a.company_id AND l.account_id = a.account_id
         LEFT JOIN journal_entries e
             ON e.company_id = l.company_id AND e.entry_id = l.entry_id
             AND e.is_posted = 1 AND e.entry_date <= ?2
         WHERE a.company_id = ?1 AND a.classification IN ('Asset', 'Liability', 'Equity')
         GROUP BY a.account_id
         ORDER BY a.number",
    )?;

    let raw = stmt
        .query_map(params![company, as_of.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

    for (account_id, name, classification, debit_minor, credit_minor) in raw {
        let debit = Money::from_minor(debit_minor);
        let credit = Money::from_minor(credit_minor);
        let (section, amount) = match classification.as_str() {
            "Asset" => (&mut assets, debit.checked_sub(credit)?),
            "Liability" => (&mut liabilities, credit.checked_sub(debit)?),
            _ => (&mut equity, credit.checked_sub(debit)?),
        };
        if amount.is_zero() {
            continue;
        }
        section.total = section.total.checked_add(amount)?;
        section.rows.push(BsRow {
            account_id: Some(AccountId(account_id)),
            name,
            balance: amount,
        });
    }

    let year_start = NaiveDate::from_ymd_opt(as_of.year(), 1, 1)
        .ok_or_else(|| LedgerError::BadDate(as_of.to_string()))?;
    let net_income = profit_and_loss(ledger, company, year_start, as_of)?.net_income;
    equity.total = equity.total.checked_add(net_income)?;
    equity.rows.push(BsRow {
        account_id: None,
        name: "Net income (current year)".to_string(),
        balance: net_income,
    });

    let total_liabilities_and_equity = liabilities.total.checked_add(equity.total)?;

    Ok(BalanceSheet {
        assets,
        liabilities,
        equity,
        total_liabilities_and_equity,
    })
}

// ---------------------------------------------------------------------------
// Balances by source_ref (§7's reconciliation-sweep SQL)
// ---------------------------------------------------------------------------

pub fn balances_by_source_ref(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
) -> Result<Vec<(Option<String>, AccountId, Money)>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT a.source_ref, a.account_id,
                SUM(l.debit_minor) - SUM(l.credit_minor) AS balance_minor
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id
         WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date <= ?2
         GROUP BY a.account_id
         HAVING balance_minor <> 0",
    )?;

    let rows = stmt
        .query_map(params![company, as_of.to_string()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                AccountId(row.get::<_, String>(1)?),
                Money::from_minor(row.get::<_, i64>(2)?),
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Sales tax liability report (§9)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SalesTaxLines {
    pub a_total_income: Money,
    pub b_tax_collected: Money,
    pub c_taxable_sales: Money,
    pub d_nontaxable_sales: Money,
    /// §9 line E: credits less debits on 4xxx (income) lines marked
    /// `is_taxable`, for the quarter — `journal_lines.is_taxable` (D23) is
    /// what makes this computable at all; before it, `crate::post` had
    /// nowhere to record which posted line came from a taxable document
    /// line. Always `Some` now that the schema carries it; kept as an
    /// `Option` because a caller reading a pre-D23 ledger row is a real
    /// distinction worth typing for, not because this report can produce
    /// `None` going forward.
    pub e_line_level_taxable: Option<Money>,
    /// `E - C`, `Some` alongside `e_line_level_taxable`. §9: a non-zero
    /// variance here is *expected* whenever C is derived from a rounded B
    /// (D23) rather than a bug to chase to zero on its own — it is the
    /// number Dan checks before filing, not one that must always read zero.
    pub variance: Option<Money>,
    /// ST-50 lines 1 through 3: `(1, A)`, `(2, D)`, `(3, C)`.
    pub st50: [(u8, Money); 3],
}

/// `quarter` is `(year, 1..=4)`.
pub fn sales_tax_lines(
    ledger: &Ledger,
    company: &str,
    quarter: (i32, u32),
    rate: Decimal,
) -> Result<SalesTaxLines, LedgerError> {
    let (year, q) = quarter;
    let (from, to) = quarter_bounds(year, q);

    let a = signed_balance_for_classification(ledger, company, from, to, "Income")?;
    let b = sales_tax_collected(ledger, company, from, to)?;

    let b_dollars = Decimal::new(b.minor(), 2);
    let c = round_money(b_dollars / rate, RoundingPolicy::LineExtension)?;
    let d = a.checked_sub(c)?;
    let e = line_level_taxable(ledger, company, from, to)?;
    let variance = e.checked_sub(c)?;

    Ok(SalesTaxLines {
        a_total_income: a,
        b_tax_collected: b,
        c_taxable_sales: c,
        d_nontaxable_sales: d,
        e_line_level_taxable: Some(e),
        variance: Some(variance),
        st50: [(1, a), (2, d), (3, c)],
    })
}

/// §9 line E: credits less debits on 4xxx (income) lines marked
/// `is_taxable`, for the quarter — from the document lines themselves rather
/// than from the tax account C is derived from.
fn line_level_taxable(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Money, LedgerError> {
    let (debit_minor, credit_minor): (i64, i64) = ledger.conn().query_row(
        "SELECT COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id
         WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date BETWEEN ?2 AND ?3
           AND a.classification = 'Income' AND l.is_taxable = 1",
        params![company, from.to_string(), to.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Money::from_minor(credit_minor).checked_sub(Money::from_minor(debit_minor))?)
}

fn quarter_bounds(year: i32, q: u32) -> (NaiveDate, NaiveDate) {
    assert!((1..=4).contains(&q), "quarter must be 1..=4, got {q}");
    let start_month = (q - 1) * 3 + 1;
    let from = NaiveDate::from_ymd_opt(year, start_month, 1).expect("valid quarter start");
    let end_month = start_month + 2;
    let (next_year, next_month) = if end_month == 12 {
        (year + 1, 1)
    } else {
        (year, end_month + 1)
    };
    let to = NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .expect("valid month after quarter end")
        .pred_opt()
        .expect("day before a month's first day always exists");
    (from, to)
}

fn signed_balance_for_classification(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
    classification: &str,
) -> Result<Money, LedgerError> {
    let (debit_minor, credit_minor): (i64, i64) = ledger.conn().query_row(
        "SELECT COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id
         WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date BETWEEN ?2 AND ?3
           AND a.classification = ?4",
        params![company, from.to_string(), to.to_string(), classification],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Money::from_minor(credit_minor).checked_sub(Money::from_minor(debit_minor))?)
}

/// B (§9): credits less debits on 2200 for the quarter, excluding lines
/// whose entry is a tax payment. There is no `source_type` for a tax payment
/// yet, so the nearest stand-in, a non-posting bank line, is excluded
/// instead — it can never legitimately touch 2200, so this is a no-op today
/// and becomes load-bearing the day a tax-payment kind exists.
fn sales_tax_collected(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Money, LedgerError> {
    let (debit_minor, credit_minor): (i64, i64) = ledger.conn().query_row(
        "SELECT COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         WHERE l.company_id = ?1 AND e.is_posted = 1 AND e.entry_date BETWEEN ?2 AND ?3
           AND l.account_id = ?4 AND e.source_type <> ?5",
        params![
            company,
            from.to_string(),
            to.to_string(),
            chart::SALES_TAX_PAYABLE,
            DocKind::BankLine.as_str(),
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Money::from_minor(credit_minor).checked_sub(Money::from_minor(debit_minor))?)
}

// ---------------------------------------------------------------------------
// Trial balance diff against QBO (§7)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct QboTbRow {
    pub qbo_account_id: String,
    pub name: String,
    pub balance: Money,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    Must,
    May,
    Skip,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::Must => "must",
            Tier::May => "may",
            Tier::Skip => "skip",
        }
    }
}

#[derive(Clone, Debug)]
enum Members {
    Numbers(&'static [&'static str]),
    Prefix(&'static str),
}

#[derive(Clone, Debug)]
pub struct AccountGroup {
    label: &'static str,
    tier: Tier,
    members: Members,
}

const fn group(label: &'static str, tier: Tier, numbers: &'static [&'static str]) -> AccountGroup {
    AccountGroup {
        label,
        tier,
        members: Members::Numbers(numbers),
    }
}

const fn prefix_group(label: &'static str, tier: Tier, prefix: &'static str) -> AccountGroup {
    AccountGroup {
        label,
        tier,
        members: Members::Prefix(prefix),
    }
}

/// The §7 tier table, as data rather than a big match statement, so a
/// caller can hand `tb_diff` a variant set (a testing tier list, a future
/// second entity's chart) without touching the diff engine.
#[derive(Clone, Debug)]
pub struct TierRules {
    groups: Vec<AccountGroup>,
    /// Always skipped, in any group: opening balance equity is never diffed
    /// (§7 "expected to differ, not diffed").
    skip_numbers: Vec<&'static str>,
}

impl Default for TierRules {
    fn default() -> Self {
        TierRules {
            groups: vec![
                group("Bank — Checking (1100)", Tier::Must, &["1100"]),
                group("Credit card (2100)", Tier::Must, &["2100"]),
                group(
                    "Accounts receivable (1200+2300)",
                    Tier::Must,
                    &["1200", "2300"],
                ),
                group(
                    "Accounts payable (2000+2050)",
                    Tier::Must,
                    &["2000", "2050"],
                ),
                group("Sales tax payable (2200)", Tier::Must, &["2200"]),
                prefix_group("Total income (4xxx)", Tier::Must, "4"),
                prefix_group("Total equity (3xxx)", Tier::Must, "3"),
                group("Inventory, raw materials (1300)", Tier::May, &["1300"]),
                group("Inventory, finished goods (1310)", Tier::May, &["1310"]),
                group("Cost of goods sold (5000)", Tier::May, &["5000"]),
                group("Manufacturing variance (5100)", Tier::May, &["5100"]),
                group("Parts and labour applied (5300)", Tier::May, &["5300"]),
                group("Undeposited funds (1150)", Tier::May, &["1150"]),
                group("Sales income (4100)", Tier::May, &["4100"]),
                group("Shipping income (4300)", Tier::May, &["4300"]),
                group("Discounts given (4900)", Tier::May, &["4900"]),
                group("Returns and allowances (4950)", Tier::May, &["4950"]),
            ],
            skip_numbers: vec!["3950"],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TbDiffRow {
    pub label: String,
    pub tier: Tier,
    pub ledger_amount: Money,
    pub qbo_amount: Money,
    pub delta: Money,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TbDiff {
    pub rows: Vec<TbDiffRow>,
    pub must_failures: Vec<String>,
    pub may_unexplained: Vec<String>,
}

impl TbDiff {
    /// The §7 fixed-width shape, close enough to file next to a hand run.
    pub fn render_text(&self, company: &str, as_of: NaiveDate) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "TRIAL BALANCE DIFF   {company}   as of {as_of}\n\n"
        ));
        out.push_str(&format!(
            "{:<38}{:>14}{:>14}{:>12}  tier\n",
            "account", "ledger", "qbo", "delta"
        ));
        for row in &self.rows {
            out.push_str(&format!(
                "{:<38}{:>14}{:>14}{:>12}  {}\n",
                row.label,
                format_money(row.ledger_amount),
                format_money(row.qbo_amount),
                format_money(row.delta),
                row.tier.as_str(),
            ));
        }
        out.push_str(&format!(
            "\nMUST-MATCH FAILURES: {}        MAY-DIFFER UNEXPLAINED: {}\n",
            self.must_failures.len(),
            self.may_unexplained.len(),
        ));
        out
    }

    /// The same rows as [`Self::render_text`], as CSV — `LEDGER-DESIGN.md`
    /// §7: "written as CSV and as fixed width text, kept forever". No
    /// quoting, matching every other CSV this crate writes; a comma inside a
    /// label is replaced with a space rather than corrupting the column
    /// count.
    pub fn render_csv(&self) -> String {
        let mut out = String::from("account,tier,ledger,qbo,delta\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{}\n",
                row.label.replace(',', " "),
                row.tier.as_str(),
                format_money(row.ledger_amount),
                format_money(row.qbo_amount),
                format_money(row.delta),
            ));
        }
        out
    }
}

fn format_money(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// A pure function over two already-computed trial balances — no Intuit
/// call, so it is testable without QBO. `ledger_rows` normally comes from
/// [`trial_balance`]; `qbo` is whatever the nightly job fetched.
pub fn tb_diff(
    ledger_rows: &TrialBalance,
    qbo: &[QboTbRow],
    tiers: &TierRules,
) -> Result<TbDiff, LedgerError> {
    let ledger_by_number: BTreeMap<&str, &TbRow> = ledger_rows
        .rows
        .iter()
        .map(|row| (row.number.as_str(), row))
        .collect();

    let mut qbo_by_id: BTreeMap<&str, Money> = BTreeMap::new();
    for row in qbo {
        let entry = qbo_by_id
            .entry(row.qbo_account_id.as_str())
            .or_insert(Money::ZERO);
        *entry = entry.checked_add(row.balance)?;
    }

    let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut rows = Vec::new();
    let mut must_failures = Vec::new();
    let mut may_unexplained = Vec::new();

    for account_group in &tiers.groups {
        let numbers: Vec<&str> = match &account_group.members {
            Members::Numbers(nums) => nums.to_vec(),
            Members::Prefix(prefix) => ledger_by_number
                .keys()
                .filter(|number| number.starts_with(prefix))
                .copied()
                .collect(),
        };

        let mut ledger_amount = Money::ZERO;
        let mut qbo_amount = Money::ZERO;
        for number in numbers {
            covered.insert(number);
            if tiers.skip_numbers.contains(&number) {
                continue;
            }
            let Some(tb_row) = ledger_by_number.get(number) else {
                continue;
            };
            if tb_row.needs_mapping {
                continue;
            }
            ledger_amount = ledger_amount.checked_add(tb_row.balance)?;
            if let Some(source_ref) = &tb_row.source_ref {
                if let Some(amount) = qbo_by_id.get(source_ref.as_str()) {
                    qbo_amount = qbo_amount.checked_add(*amount)?;
                }
            }
        }

        let delta = ledger_amount.checked_sub(qbo_amount)?;
        let label = account_group.label.to_string();
        if account_group.tier == Tier::Must && !delta.is_zero() {
            must_failures.push(label.clone());
        }
        if account_group.tier == Tier::May && !delta.is_zero() {
            may_unexplained.push(label.clone());
        }
        rows.push(TbDiffRow {
            label,
            tier: account_group.tier,
            ledger_amount,
            qbo_amount,
            delta,
        });
    }

    // Explicit skip rows (§7 "expected to differ, not diffed"): 3950 and any
    // account flagged needs_mapping, when not already summed into a group above.
    for tb_row in &ledger_rows.rows {
        let is_skip_number = tiers.skip_numbers.contains(&tb_row.number.as_str());
        if !covered.contains(tb_row.number.as_str()) && (is_skip_number || tb_row.needs_mapping) {
            rows.push(TbDiffRow {
                label: format!("{} {}", tb_row.number, tb_row.name),
                tier: Tier::Skip,
                ledger_amount: tb_row.balance,
                qbo_amount: Money::ZERO,
                delta: Money::ZERO,
            });
        }
    }

    Ok(TbDiff {
        rows,
        must_failures,
        may_unexplained,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_bounds_cover_the_calendar_year() {
        assert_eq!(
            quarter_bounds(2026, 1),
            (
                NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()
            )
        );
        assert_eq!(
            quarter_bounds(2026, 4),
            (
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
            )
        );
    }

    #[test]
    fn tb_diff_render_csv_has_a_header_and_one_row_per_group() {
        let diff = TbDiff {
            rows: vec![TbDiffRow {
                label: "Bank — Checking (1100)".to_string(),
                tier: Tier::Must,
                ledger_amount: Money::from_minor(41_288_419),
                qbo_amount: Money::from_minor(41_288_419),
                delta: Money::ZERO,
            }],
            must_failures: Vec::new(),
            may_unexplained: Vec::new(),
        };
        let csv = diff.render_csv();
        assert_eq!(
            csv,
            "account,tier,ledger,qbo,delta\nBank — Checking (1100),must,412884.19,412884.19,0.00\n"
        );
    }

    #[test]
    fn format_money_handles_negative_cents() {
        assert_eq!(format_money(Money::from_minor(-105)), "-1.05");
        assert_eq!(format_money(Money::from_minor(500)), "5.00");
        assert_eq!(format_money(Money::ZERO), "0.00");
    }
}
