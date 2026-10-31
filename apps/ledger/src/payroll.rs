//! The pay-run journal. `LEDGER-DESIGN.md` §12, `DECISIONS.md` D38,
//! `docs/PAYROLL.md`.
//!
//! SurePayroll stays the payroll engine; this book only journals what it
//! already computed. One SurePayroll run (one check date) becomes one
//! `JournalEntry` document and one flagged entry (§4):
//!
//! - Dr wages by class — the run's cash earnings, per employee, onto the
//!   wages account and class the employee (or their department) maps to;
//! - Dr employer taxes and employer-paid deductions, same class, onto the
//!   mapping's employer-tax account (its wages account unless set);
//! - Cr 2400 payroll liabilities — employee withholdings, employee
//!   deductions, employer taxes and employer deductions, all owed onward;
//! - Cr 1100 (or the configured bank) — the net pay that left the bank.
//!
//! Nothing here computes a paycheck. The one arithmetic check is the one a
//! bookkeeper would do: where the summary states an employee's net pay, it
//! must equal earnings less withholdings and deductions to the cent, or the
//! run is refused. An employee with no mapping in `[payroll]` is refused
//! too — a run never posts with a guessed class.
//!
//! **Input.** SurePayroll's own exports are a rendered report (Payroll
//! Summary → PDF/Excel/CSV of the page layout), not a data file, so the
//! import reads a flat "pay-run lines" CSV (or the same shape as JSON),
//! one row per figure on the Payroll Summary: see [`parse_csv`] and
//! `docs/PAYROLL.md` for the columns and how each Payroll Summary item maps
//! to a [`Section`].
//!
//! **What stays out of the book.** Employee names and per-employee pay are
//! never written to the document or the entry: the journal carries totals
//! by account and class, and the line descriptions name SurePayroll items
//! (`FED WTH`, `CO FICA`), never people.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::chart;
use crate::post::{self, PostError, Posting};
use crate::store::{CommandMeta, Ledger, LedgerError};
use crate::types::{AccountId, ClassId, DocKind, DocLine, LedgerDocument, LineKind, Side};

// ---------------------------------------------------------------------------
// The pay-run shape
// ---------------------------------------------------------------------------

/// Which column of the Payroll Summary a figure sits in.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// EARNINGS that were paid in cash: REGULAR, SALARY, O/TIME, BONUS,
    /// VACATION. Debits wages.
    Earning,
    /// EARNINGS the summary marks non-cash: taxable, but not in net pay and
    /// not paid by this run. Read and reported, never posted.
    NoncashEarning,
    /// EMPLOYEE TAXES: FED WTH, FICA, MEDFICA, STATE-NJ, NJ SUI EE,
    /// DISAB-NJ, NJ FLI, NJWFEE, NJSWF. Credits 2400.
    EmployeeTax,
    /// EMPLOYEE DEDUCTIONS: 401(k), health premium share, garnishments.
    /// Credits 2400.
    EmployeeDeduction,
    /// EMPLOYER TAXES: CO FICA, CO MEDC, FUTA, CO UNEM-NJ, ER SDI-NJ,
    /// NJWFER. Debits the employer-tax account, credits 2400.
    EmployerTax,
    /// EMPLOYER DEDUCTIONS: an employer match or premium share. Same legs
    /// as an employer tax.
    EmployerDeduction,
    /// NET PAY as the summary states it. Not posted directly: it is the
    /// cross-check on the arithmetic, and the bank leg is computed.
    NetPay,
}

impl Section {
    pub const fn as_str(self) -> &'static str {
        match self {
            Section::Earning => "earning",
            Section::NoncashEarning => "noncash_earning",
            Section::EmployeeTax => "employee_tax",
            Section::EmployeeDeduction => "employee_deduction",
            Section::EmployerTax => "employer_tax",
            Section::EmployerDeduction => "employer_deduction",
            Section::NetPay => "net_pay",
        }
    }

    pub fn parse(raw: &str) -> Option<Section> {
        [
            Section::Earning,
            Section::NoncashEarning,
            Section::EmployeeTax,
            Section::EmployeeDeduction,
            Section::EmployerTax,
            Section::EmployerDeduction,
            Section::NetPay,
        ]
        .into_iter()
        .find(|section| section.as_str() == raw.trim().to_ascii_lowercase())
    }
}

/// One figure on the summary: one employee, one item, one period amount.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PayLine {
    /// The key `[payroll.employees]` maps: SurePayroll's employee id, or the
    /// name as the report prints it — whichever Dan keys the mapping on.
    pub employee: String,
    /// SurePayroll department, when the company uses them. A fallback key
    /// for the mapping (`[payroll.departments]`).
    #[serde(default)]
    pub department: Option<String>,
    pub section: Section,
    /// The SurePayroll item code, as printed (`REGULAR`, `FED WTH`).
    pub item: String,
    pub amount: Money,
}

/// One SurePayroll run: one check date's Payroll Summary.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PayRun {
    /// SurePayroll's id for the run (the Payroll Summary's check-date
    /// selector value). With `pay_date`, the dedupe key.
    pub run_id: String,
    /// The check date: the date the entry posts on.
    pub pay_date: NaiveDate,
    #[serde(default)]
    pub period_start: Option<NaiveDate>,
    #[serde(default)]
    pub period_end: Option<NaiveDate>,
    pub lines: Vec<PayLine>,
}

impl PayRun {
    /// `payroll:<pay date>:<run id>` — the document every import of this
    /// run saves to, so a second import finds the first.
    pub fn document_id(&self) -> String {
        format!("payroll:{}:{}", self.pay_date, self.run_id)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PayrollError {
    #[error("pay-run file row {row}: {reason}")]
    Malformed { row: usize, reason: String },
    #[error("pay-run file is missing column {0:?}")]
    MissingColumn(&'static str),
    #[error("pay-run file has no rows")]
    Empty,
    #[error("run {run_id}: {reason}")]
    Invalid { run_id: String, reason: String },
    /// Every employee and department in the run that `[payroll]` does not
    /// map. Nothing is posted until all of them are.
    #[error(
        "run {run_id}: no class mapping for {}; add them under [payroll.employees] or \
         [payroll.departments] in the payroll config",
        unmapped.join(", ")
    )]
    Unmapped {
        run_id: String,
        unmapped: Vec<String>,
    },
    #[error(
        "run {run_id}: stated net pay {stated} for one employee does not equal earnings less \
         withholdings and deductions {computed}"
    )]
    NetPayMismatch {
        run_id: String,
        stated: String,
        computed: String,
    },
    #[error(
        "{document_id} is already posted as entry {entry_id} with different figures; reverse \
         that entry first, then import again"
    )]
    Changed {
        document_id: String,
        entry_id: String,
    },
    #[error("payroll config {path}: {reason}")]
    Config { path: String, reason: String },
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error(transparent)]
    Post(#[from] PostError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

const COLUMNS: &[&str] = &[
    "run_id",
    "pay_date",
    "period_start",
    "period_end",
    "employee",
    "department",
    "section",
    "item",
    "amount",
];
const REQUIRED: &[&str] = &[
    "run_id", "pay_date", "employee", "section", "item", "amount",
];

/// Reads a pay-run lines CSV: a header row naming (in any order)
/// `run_id, pay_date, employee, section, item, amount`, and optionally
/// `period_start, period_end, department`. Dates are `YYYY-MM-DD` or
/// `MM/DD/YYYY`; amounts may carry `$`, thousands commas, and parentheses
/// for a negative. Rows group into runs by `(run_id, pay_date)`, in the
/// order each run first appears. A blank row is skipped; anything else
/// malformed fails the whole file.
pub fn parse_csv(text: &str) -> Result<Vec<PayRun>, PayrollError> {
    let mut lines = text
        .lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim_start_matches('\u{feff}')))
        .filter(|(_, line)| !line.trim().is_empty());
    let (_, header) = lines.next().ok_or(PayrollError::Empty)?;
    let header: Vec<String> = split_csv_line(header)
        .into_iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let col = |name: &str| header.iter().position(|h| h == name);
    for name in REQUIRED {
        if col(name).is_none() {
            let name = COLUMNS
                .iter()
                .find(|c| *c == name)
                .expect("required ⊆ columns");
            return Err(PayrollError::MissingColumn(name));
        }
    }

    let mut runs: Vec<PayRun> = Vec::new();
    for (row, line) in lines {
        let fields = split_csv_line(line);
        let get = |name: &str| -> &str {
            col(name)
                .and_then(|i| fields.get(i))
                .map(|s| s.trim())
                .unwrap_or("")
        };
        let malformed = |reason: String| PayrollError::Malformed { row, reason };
        let required = |name: &str| -> Result<&str, PayrollError> {
            match get(name) {
                "" => Err(malformed(format!("{name} is empty"))),
                value => Ok(value),
            }
        };

        let run_id = required("run_id")?.to_string();
        let pay_date = parse_date(required("pay_date")?).map_err(malformed)?;
        let period_start = optional_date(get("period_start")).map_err(malformed)?;
        let period_end = optional_date(get("period_end")).map_err(malformed)?;
        let section_raw = required("section")?;
        let section = Section::parse(section_raw)
            .ok_or_else(|| malformed(format!("unknown section {section_raw:?}")))?;
        let amount = parse_amount(required("amount")?).map_err(malformed)?;
        let department = match get("department") {
            "" => None,
            value => Some(value.to_string()),
        };
        let line = PayLine {
            employee: required("employee")?.to_string(),
            department,
            section,
            item: required("item")?.to_string(),
            amount,
        };

        match runs
            .iter_mut()
            .find(|r| r.run_id == run_id && r.pay_date == pay_date)
        {
            Some(run) => {
                if (period_start.is_some() && run.period_start != period_start)
                    || (period_end.is_some() && run.period_end != period_end)
                {
                    if run.period_start.is_none() && run.period_end.is_none() {
                        run.period_start = period_start;
                        run.period_end = period_end;
                    } else {
                        return Err(malformed(format!(
                            "run {run_id} gives two different pay periods"
                        )));
                    }
                }
                run.lines.push(line);
            }
            None => runs.push(PayRun {
                run_id,
                pay_date,
                period_start,
                period_end,
                lines: vec![line],
            }),
        }
    }
    if runs.is_empty() {
        return Err(PayrollError::Empty);
    }
    Ok(runs)
}

fn parse_date(raw: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(raw, "%m/%d/%Y"))
        .map_err(|_| format!("invalid date {raw:?} (YYYY-MM-DD or MM/DD/YYYY)"))
}

fn optional_date(raw: &str) -> Result<Option<NaiveDate>, String> {
    if raw.is_empty() {
        Ok(None)
    } else {
        parse_date(raw).map(Some)
    }
}

fn parse_amount(raw: &str) -> Result<Money, String> {
    let mut cleaned = raw.replace(['$', ','], "");
    let mut negative = false;
    if cleaned.starts_with('(') && cleaned.ends_with(')') {
        negative = true;
        cleaned = cleaned[1..cleaned.len() - 1].to_string();
    }
    let mut value =
        Decimal::from_str(cleaned.trim()).map_err(|_| format!("invalid amount {raw:?}"))?;
    if value.scale() > 2 {
        return Err(format!("amount {raw:?} has more than two decimal places"));
    }
    if negative {
        value = -value;
    }
    round_money(value, RoundingPolicy::MirroredAmount).map_err(|e| e.to_string())
}

/// The quoted-field splitter `crate::bank::csv` uses: a comma inside
/// `"..."` (a `"Last, First"` name) and a doubled `""` as an escaped quote.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

// ---------------------------------------------------------------------------
// Config: the employee/department → class mapping
// ---------------------------------------------------------------------------

/// Where one employee's (or department's) pay lands.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// A §3 class id (`foam`, `sign`, `chair`, `drop`, `cnc`, `uv`).
    pub class: String,
    /// Overrides [`PayrollConfig::wages_account`] for this person — e.g. an
    /// office wage onto a 6xxx account instead of 5400 direct labour.
    #[serde(default)]
    pub wages_account: Option<String>,
    /// Overrides where employer taxes and employer deductions land; default
    /// is this mapping's wages account.
    #[serde(default)]
    pub employer_tax_account: Option<String>,
}

/// The `[payroll]` table of `.local/config.toml` (gitignored; the template
/// is `.local/config.example.toml`). Every account has a default; the two
/// maps start empty, so an import with no config refuses every employee
/// rather than guessing.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayrollConfig {
    /// Where net pay leaves from. Default 1100.
    #[serde(default = "default_bank")]
    pub bank_account: String,
    /// Where withholdings and employer taxes wait until SurePayroll remits
    /// them. Default 2400.
    #[serde(default = "default_liabilities")]
    pub liabilities_account: String,
    /// Default wages account. 5400 direct labour.
    #[serde(default = "default_wages")]
    pub wages_account: String,
    /// Default employer-tax account; unset means each mapping's wages
    /// account, so taxes follow the wage they were levied on.
    #[serde(default)]
    pub employer_tax_account: Option<String>,
    /// SurePayroll employee (id or printed name) → mapping. Checked first.
    #[serde(default)]
    pub employees: BTreeMap<String, Mapping>,
    /// SurePayroll department → mapping, for an employee not listed above.
    #[serde(default)]
    pub departments: BTreeMap<String, Mapping>,
}

fn default_bank() -> String {
    chart::CHECKING.to_string()
}
fn default_liabilities() -> String {
    chart::PAYROLL_LIABILITIES.to_string()
}
fn default_wages() -> String {
    chart::DIRECT_LABOUR.to_string()
}

impl Default for PayrollConfig {
    fn default() -> Self {
        PayrollConfig {
            bank_account: default_bank(),
            liabilities_account: default_liabilities(),
            wages_account: default_wages(),
            employer_tax_account: None,
            employees: BTreeMap::new(),
            departments: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    payroll: Option<toml::Value>,
}

impl PayrollConfig {
    /// Reads `[payroll]` from `path`. A missing file or table is the
    /// default (which maps nobody); a present but unreadable one is an
    /// error. Other tables in the file are ignored.
    pub fn load(path: &Path) -> Result<PayrollConfig, PayrollError> {
        let config_err = |reason: String| PayrollError::Config {
            path: path.display().to_string(),
            reason,
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PayrollConfig::default())
            }
            Err(err) => return Err(config_err(err.to_string())),
        };
        PayrollConfig::parse(&text).map_err(config_err)
    }

    pub fn parse(text: &str) -> Result<PayrollConfig, String> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| e.to_string())?;
        match file.payroll {
            None => Ok(PayrollConfig::default()),
            Some(table) => table.try_into().map_err(|e: toml::de::Error| e.to_string()),
        }
    }

    fn resolve(&self, line: &PayLine) -> Option<&Mapping> {
        self.employees.get(line.employee.trim()).or_else(|| {
            line.department
                .as_deref()
                .and_then(|d| self.departments.get(d.trim()))
        })
    }
}

/// `LEDGER_PAYROLL_CONFIG`, else `.local/config.toml`.
pub fn default_config_path() -> PathBuf {
    std::env::var_os("LEDGER_PAYROLL_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".local/config.toml"))
}

// ---------------------------------------------------------------------------
// PayRun → JournalEntry document
// ---------------------------------------------------------------------------

/// The run's totals, as the journal books them. No per-employee figures.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RunTotals {
    pub gross: Money,
    pub employer_costs: Money,
    pub liabilities: Money,
    pub net_pay: Money,
    /// Non-cash earnings read and left out of the entry.
    pub noncash: Money,
}

/// Builds the run's `JournalEntry` document. Refuses an empty run, an
/// unmapped employee or department (listing every one), a negative net, and
/// a stated net pay that does not equal earnings less withholdings and
/// deductions. The result posts balanced through [`post::post`] by
/// construction: the bank leg is what is left.
pub fn journal_document(
    run: &PayRun,
    config: &PayrollConfig,
) -> Result<(LedgerDocument, RunTotals), PayrollError> {
    let invalid = |reason: &str| PayrollError::Invalid {
        run_id: run.run_id.clone(),
        reason: reason.to_string(),
    };
    if run.run_id.trim().is_empty() {
        return Err(invalid("run id is empty"));
    }
    if run.lines.is_empty() {
        return Err(invalid("no lines"));
    }

    // Every unmapped key first, so one error names them all.
    let mut unmapped = BTreeSet::new();
    for line in &run.lines {
        if line.section != Section::NetPay && config.resolve(line).is_none() {
            unmapped.insert(match &line.department {
                Some(d) => format!("employee {:?} (department {d:?})", line.employee),
                None => format!("employee {:?}", line.employee),
            });
        }
    }
    if !unmapped.is_empty() {
        return Err(PayrollError::Unmapped {
            run_id: run.run_id.clone(),
            unmapped: unmapped.into_iter().collect(),
        });
    }

    // (account, class) → (amount, items), debits only; credits to 2400 by item.
    let mut debits: BTreeMap<(String, String), (Money, BTreeSet<String>)> = BTreeMap::new();
    let mut liabilities: BTreeMap<String, Money> = BTreeMap::new();
    // Per employee: computed net and stated net, for the cross-check.
    let mut computed_net: BTreeMap<&str, Money> = BTreeMap::new();
    let mut stated_net: BTreeMap<&str, Money> = BTreeMap::new();
    let mut totals = RunTotals::default();

    for line in &run.lines {
        let employee = line.employee.trim();
        let net = computed_net.entry(employee).or_insert(Money::ZERO);
        if line.section == Section::NetPay {
            let stated = stated_net.entry(employee).or_insert(Money::ZERO);
            *stated = stated.checked_add(line.amount)?;
            continue;
        }
        let mapping = config.resolve(line).expect("checked above");
        let wages_account = mapping
            .wages_account
            .clone()
            .unwrap_or_else(|| config.wages_account.clone());
        let employer_account = mapping
            .employer_tax_account
            .clone()
            .or_else(|| config.employer_tax_account.clone())
            .unwrap_or_else(|| wages_account.clone());
        let mut debit = |account: String| -> Result<(), MoneyError> {
            let slot = debits
                .entry((account, mapping.class.clone()))
                .or_insert((Money::ZERO, BTreeSet::new()));
            slot.0 = slot.0.checked_add(line.amount)?;
            slot.1.insert(line.item.trim().to_string());
            Ok(())
        };
        let mut owe = |amount: Money| -> Result<(), MoneyError> {
            let slot = liabilities
                .entry(line.item.trim().to_string())
                .or_insert(Money::ZERO);
            *slot = slot.checked_add(amount)?;
            Ok(())
        };
        match line.section {
            Section::Earning => {
                debit(wages_account)?;
                *net = net.checked_add(line.amount)?;
                totals.gross = totals.gross.checked_add(line.amount)?;
            }
            Section::NoncashEarning => {
                totals.noncash = totals.noncash.checked_add(line.amount)?;
            }
            Section::EmployeeTax | Section::EmployeeDeduction => {
                owe(line.amount)?;
                *net = net.checked_sub(line.amount)?;
            }
            Section::EmployerTax | Section::EmployerDeduction => {
                debit(employer_account)?;
                owe(line.amount)?;
                totals.employer_costs = totals.employer_costs.checked_add(line.amount)?;
            }
            Section::NetPay => unreachable!("handled above"),
        }
    }

    for (employee, stated) in &stated_net {
        let computed = computed_net.get(employee).copied().unwrap_or(Money::ZERO);
        if *stated != computed {
            return Err(PayrollError::NetPayMismatch {
                run_id: run.run_id.clone(),
                stated: Money::display(*stated),
                computed: Money::display(computed),
            });
        }
    }
    totals.net_pay = Money::checked_sum(computed_net.values().copied())?;
    totals.liabilities = Money::checked_sum(liabilities.values().copied())?;
    if totals.net_pay.is_negative() {
        return Err(invalid("withholdings exceed earnings: net pay is negative"));
    }
    if totals.gross.is_zero() && totals.employer_costs.is_zero() {
        return Err(invalid(
            "nothing to post: no cash earnings and no employer costs",
        ));
    }

    let mut lines = Vec::new();
    let mut push =
        |account: String, class: Option<String>, side: Side, amount: Money, what: String| {
            let line_no = lines.len() as i64 + 1;
            lines.push(DocLine {
                line_no,
                kind: LineKind::Journal,
                amount,
                class: class.map(ClassId),
                item_id: None,
                account: Some(AccountId(account)),
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: Some(what),
                posting: Some(side),
                entity: None,
            });
        };
    for ((account, class), (amount, items)) in &debits {
        let items: Vec<&str> = items.iter().map(String::as_str).collect();
        push(
            account.clone(),
            Some(class.clone()),
            Side::Debit,
            *amount,
            format!("Payroll {class}: {}", items.join(", ")),
        );
    }
    for (item, amount) in &liabilities {
        push(
            config.liabilities_account.clone(),
            None,
            Side::Credit,
            *amount,
            format!("Payroll liability: {item}"),
        );
    }
    push(
        config.bank_account.clone(),
        None,
        Side::Credit,
        totals.net_pay,
        "Payroll net pay".to_string(),
    );

    let period = match (run.period_start, run.period_end) {
        (Some(a), Some(b)) => format!(", period {a} to {b}"),
        _ => String::new(),
    };
    let doc = LedgerDocument {
        document_id: run.document_id(),
        kind: DocKind::JournalEntry,
        number: Some(run.run_id.clone()),
        txn_date: run.pay_date,
        due_date: None,
        contact: None,
        header_class: None,
        lines,
        tax: None,
        deposit_to: None,
        pay_from: Some(AccountId(config.bank_account.clone())),
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: Some(format!("surepayroll:{}", run.run_id)),
        memo: Some(format!(
            "SurePayroll pay run {}, check date {}{period}",
            run.run_id, run.pay_date
        )),
    };
    Ok((doc, totals))
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RunStatus {
    Posted,
    /// The same figures are already posted; nothing was written.
    AlreadyImported,
}

impl RunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Posted => "posted",
            RunStatus::AlreadyImported => "already_imported",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunOutcome {
    pub run_id: String,
    pub pay_date: NaiveDate,
    pub document_id: String,
    pub status: RunStatus,
    pub entry_id: String,
    pub totals: RunTotals,
}

/// Posts every run, all or nothing. Each run is built and checked first
/// (mapping, net pay, balance); then, inside one [`Ledger::atomically`],
/// each is saved as a `JournalEntry` document and posted as a flagged entry
/// (§4) — through the period gate, so a check date on or before
/// `locked_through` refuses the whole import.
///
/// Idempotent by `payroll:<pay date>:<run id>`: a run whose document already
/// carries a live entry with the same figures is reported
/// [`RunStatus::AlreadyImported`] and not touched; with different figures
/// it is refused ([`PayrollError::Changed`]) until that entry is reversed,
/// after which an import saves the new version and posts it.
pub fn import_runs(
    ledger: &Ledger,
    company: &str,
    runs: &[PayRun],
    config: &PayrollConfig,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<Vec<RunOutcome>, PayrollError> {
    if runs.is_empty() {
        return Err(PayrollError::Empty);
    }
    let mut built = Vec::with_capacity(runs.len());
    for run in runs {
        built.push((run, journal_document(run, config)?));
    }
    if let Some(locked) = ledger.locked_through(company)? {
        if let Some((run, _)) = built.iter().find(|(run, _)| run.pay_date <= locked) {
            return Err(PayrollError::Invalid {
                run_id: run.run_id.clone(),
                reason: format!(
                    "check date {} is in a closed period ({})",
                    run.pay_date,
                    LedgerError::PeriodClosed { period_end: locked }
                ),
            });
        }
    }

    ledger.atomically(|| {
        let mut outcomes = Vec::with_capacity(built.len());
        for (run, (doc, totals)) in &built {
            let live = ledger.live_entries_for_document(company, &doc.document_id)?;
            if let Some((entry_id, _)) = live.first() {
                let same = ledger
                    .latest_document_payload(company, &doc.document_id)?
                    .map(|payload| payload == serde_json::to_string(doc).unwrap_or_default())
                    .unwrap_or(false);
                if !same {
                    return Err(PayrollError::Changed {
                        document_id: doc.document_id.clone(),
                        entry_id: entry_id.clone(),
                    });
                }
                outcomes.push(RunOutcome {
                    run_id: run.run_id.clone(),
                    pay_date: run.pay_date,
                    document_id: doc.document_id.clone(),
                    status: RunStatus::AlreadyImported,
                    entry_id: entry_id.clone(),
                    totals: *totals,
                });
                continue;
            }

            let version = ledger.save_document(
                company,
                doc,
                CommandMeta {
                    actor_id: actor.to_string(),
                    kind: "payroll_import".to_string(),
                    hlc: now.to_rfc3339(),
                },
                now,
            )?;
            let entry = match post::post(doc, version.version, &Default::default())? {
                Posting::Entry(entry) => entry,
                Posting::NonPosting => {
                    return Err(PayrollError::Invalid {
                        run_id: run.run_id.clone(),
                        reason: "the run moves no money".to_string(),
                    })
                }
            };
            debug_assert!(entry.is_flagged, "a journal entry posts flagged (§4)");
            let entry_id = ledger.post_entry(company, &entry, now)?;
            outcomes.push(RunOutcome {
                run_id: run.run_id.clone(),
                pay_date: run.pay_date,
                document_id: doc.document_id.clone(),
                status: RunStatus::Posted,
                entry_id,
                totals: *totals,
            });
        }
        Ok(outcomes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "run_id,pay_date,period_start,period_end,employee,department,section,item,amount";

    fn config() -> PayrollConfig {
        PayrollConfig::parse(
            r#"
            [payroll.employees]
            "E-ALPHA" = { class = "foam" }
            "Beta, Bo" = { class = "sign", wages_account = "6900" }
            [payroll.departments]
            "Shop" = { class = "chair" }
            "#,
        )
        .expect("config parses")
    }

    #[test]
    fn parses_quoted_names_money_and_both_date_forms() {
        let text = format!(
            "{HEADER}\nR1,01/15/2027,2027-01-01,2027-01-14,\"Beta, Bo\",,earning,REGULAR,\"$1,234.50\"\n\
             R1,2027-01-15,,,\"Beta, Bo\",,employee_tax,FED WTH,(10.00)\n"
        );
        let runs = parse_csv(&text).expect("parses");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lines.len(), 2);
        assert_eq!(runs[0].lines[0].employee, "Beta, Bo");
        assert_eq!(runs[0].lines[0].amount, Money::from_minor(123_450));
        assert_eq!(runs[0].lines[1].amount, Money::from_minor(-1_000));
        assert_eq!(runs[0].period_end, NaiveDate::from_ymd_opt(2027, 1, 14));
    }

    #[test]
    fn refuses_unknown_section_and_missing_columns() {
        let bad = format!("{HEADER}\nR1,2027-01-15,,,E-ALPHA,,fees,FEES,5.00\n");
        assert!(matches!(
            parse_csv(&bad),
            Err(PayrollError::Malformed { row: 2, .. })
        ));
        assert!(matches!(
            parse_csv("run_id,pay_date,employee,section,item\n"),
            Err(PayrollError::MissingColumn("amount"))
        ));
        assert!(matches!(parse_csv(""), Err(PayrollError::Empty)));
    }

    #[test]
    fn department_is_the_fallback_and_unmapped_lists_everyone() {
        let config = config();
        let run = PayRun {
            run_id: "R2".into(),
            pay_date: NaiveDate::from_ymd_opt(2027, 1, 29).expect("date"),
            period_start: None,
            period_end: None,
            lines: vec![
                PayLine {
                    employee: "E-GAMMA".into(),
                    department: Some("Shop".into()),
                    section: Section::Earning,
                    item: "REGULAR".into(),
                    amount: Money::from_minor(10_000),
                },
                PayLine {
                    employee: "E-DELTA".into(),
                    department: None,
                    section: Section::Earning,
                    item: "REGULAR".into(),
                    amount: Money::from_minor(10_000),
                },
                PayLine {
                    employee: "E-EPS".into(),
                    department: Some("Office".into()),
                    section: Section::EmployeeTax,
                    item: "FICA".into(),
                    amount: Money::from_minor(100),
                },
            ],
        };
        match journal_document(&run, &config) {
            Err(PayrollError::Unmapped { unmapped, .. }) => {
                assert_eq!(unmapped.len(), 2);
                assert!(unmapped[0].contains("E-DELTA"));
                assert!(unmapped[1].contains("Office"));
            }
            other => panic!("expected Unmapped, got {other:?}"),
        }
    }

    #[test]
    fn the_example_config_parses_and_maps_nobody() {
        let example = include_str!("../../../.local/config.example.toml");
        let config = PayrollConfig::parse(example).expect("template parses");
        assert_eq!(config, PayrollConfig::default());
    }

    #[test]
    fn config_rejects_unknown_keys() {
        assert!(PayrollConfig::parse("[payroll]\nbank = \"1100\"\n").is_err());
        assert_eq!(
            PayrollConfig::parse("[intuit]\nclient_id = \"x\"\n").expect("no table"),
            PayrollConfig::default()
        );
    }
}
