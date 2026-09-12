//! Accountant mode. `LEDGER-DESIGN.md` §8, `DECISIONS.md` D7, D17.
//!
//! Four things live here, in the order §8 lists them:
//!
//! 1. **GL detail and the audit trail** ([`general_ledger`], [`audit_trail`]):
//!    read-only reports over what [`crate::store`] and [`crate::report`]
//!    already hold. Nothing here writes.
//! 2. **CSV exports** ([`export_csv`], [`export_pack`]): a stable, two-decimal
//!    CSV per report, and a five-file pack for a year end handoff. **No PDF**
//!    — that arrives with the UI (§8 "Exports": "the PDF is the one that goes
//!    in the file"), and there is no UI yet.
//! 3. **The adjusting-entry request queue** ([`propose_adjustment`],
//!    [`propose_reclassify`], [`list_adjustments`], [`decide_adjustment`]):
//!    the one write path this module has. An accountant proposes; Dan
//!    decides. Rejecting records a decision and nothing else. Approving
//!    builds a `JournalEntry`-kind [`crate::types::LedgerDocument`] from the
//!    request's lines, saves it, and posts it flagged through
//!    [`crate::store::Ledger::post_entry`] — the same period gate, the same
//!    class rule, the same immutability that governs every other posting, so
//!    accountant mode adds no second way to get money into the book. A
//!    correction is never modelled as editing a posted entry: a reclassify
//!    proposes a balanced two-line entry that reverses the old leg and
//!    reposts it (§4 "Corrections are reversing entries").
//! 4. **[`AccountantView`]**: the type Joel's process actually gets. It wraps
//!    a `&Ledger` and exposes only reads plus the `propose_*` functions above
//!    — there is no `post_entry`, `close_period`, `reopen_period` or
//!    `save_document` method on it, so a period-locked, read-only role is a
//!    compile-time fact about the type rather than a runtime permission check
//!    that something could bypass. **Open (W24):** whether Joel reaches this
//!    through a local read-only SQLite copy or a hosted view is a deployment
//!    question, not an API one, and is unresolved — see `LEDGER-DESIGN.md`
//!    §8 "How Joel reaches it".
//!
//! Every proposal is validated here before it is stored, so a rejection is
//! immediate rather than discovered at approval time (§8: "checked here so
//! Joel sees the rejection immediately"). That check — balance, and the §3
//! class rule — mirrors `crate::store`'s private `check_class_rule`
//! deliberately rather than sharing it: this one is advisory (a proposal that
//! passes it can still fail differently at approval, e.g. because the period
//! has since closed), while the store's is authoritative and enforced again,
//! unconditionally, by [`crate::store::Ledger::post_entry`] itself.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rust_decimal::Decimal;
use thiserror::Error;
use uuid::Uuid;

use crate::post::{self, Posting};
use crate::report::{self, BalanceSheet, Pnl, SalesTaxLines, TrialBalance};
use crate::store::{
    AdjustmentRequestRow, AdjustmentState, CloseHistoryRow, CommandMeta, Ledger, LedgerError,
};
use crate::types::{
    AccountId, ClassId, ContactRef, DocKind, DocLine, JournalEntry, JournalLine, LedgerDocument,
    LineKind, PostingContext, Side,
};
use ledger_core::Money;

#[derive(Debug, Error)]
pub enum AccountantError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Post(#[from] post::PostError),
    #[error("adjustment lines do not balance")]
    Unbalanced,
    #[error("line {line_no} touches income, COGS or expense and requires a class")]
    MissingClass { line_no: i64 },
    #[error("line {line_no} is a balance-sheet line and may not carry a class")]
    UnexpectedClass { line_no: i64 },
    #[error("unknown account: {0}")]
    UnknownAccount(String),
    #[error("unknown class: {0}")]
    UnknownClass(String),
    #[error("adjustment request not found")]
    NotFound,
    #[error("adjustment request {request_id} has already been decided")]
    AlreadyDecided { request_id: String },
    #[error("entry {entry_id} has no line {line_no}")]
    LineNotFound { entry_id: String, line_no: i64 },
    #[error("stored adjustment request lines are not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("writing export pack: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// 1. GL detail and audit trail
// ---------------------------------------------------------------------------

/// One posted line within [`GlAccountSection`], in date order.
#[derive(Clone, Debug, PartialEq)]
pub struct GlLine {
    pub entry_id: String,
    pub entry_date: NaiveDate,
    pub source_type: DocKind,
    pub source_id: String,
    pub memo: Option<String>,
    pub class: Option<ClassId>,
    pub entity: Option<ContactRef>,
    pub is_flagged: bool,
    pub reversal_of: Option<String>,
    pub debit: Money,
    pub credit: Money,
    /// Debit positive, same convention as [`crate::report::TbRow::balance`]:
    /// the last line's running balance equals that account's trial-balance
    /// balance as of `to`.
    pub running_balance: Money,
}

/// One account's slice of the general ledger between `from` and `to`.
#[derive(Clone, Debug, PartialEq)]
pub struct GlAccountSection {
    pub account_id: AccountId,
    pub number: String,
    pub name: String,
    /// The balance carried in from before `from` (debit positive).
    pub opening_balance: Money,
    pub lines: Vec<GlLine>,
    pub closing_balance: Money,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GlDetail {
    pub sections: Vec<GlAccountSection>,
}

/// Every posted line between `from` and `to` inclusive, grouped by account
/// (or just `account` when given), each with a running balance that starts
/// from the balance carried in from before `from` (§8 "GL detail").
pub fn general_ledger(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
    account: Option<&AccountId>,
) -> Result<GlDetail, AccountantError> {
    let rows = ledger.gl_lines(company, to, account)?;

    let mut sections: Vec<GlAccountSection> = Vec::new();
    for row in rows {
        if sections.last().map(|s| &s.account_id) != Some(&row.account_id) {
            sections.push(GlAccountSection {
                account_id: row.account_id.clone(),
                number: row.number.clone(),
                name: row.name.clone(),
                opening_balance: Money::ZERO,
                lines: Vec::new(),
                closing_balance: Money::ZERO,
            });
        }
        let section = sections.last_mut().expect("just pushed if empty");
        let amount = row
            .debit
            .checked_sub(row.credit)
            .map_err(LedgerError::from)?;

        if row.entry_date < from {
            section.opening_balance = section
                .opening_balance
                .checked_add(amount)
                .map_err(LedgerError::from)?;
            section.closing_balance = section.opening_balance;
            continue;
        }

        section.closing_balance = section
            .closing_balance
            .checked_add(amount)
            .map_err(LedgerError::from)?;
        section.lines.push(GlLine {
            entry_id: row.entry_id,
            entry_date: row.entry_date,
            source_type: row.source_type,
            source_id: row.source_id,
            memo: row.line_memo.or(row.entry_memo),
            class: row.class_id,
            entity: row.entity,
            is_flagged: row.is_flagged,
            reversal_of: row.reversal_of,
            debit: row.debit,
            credit: row.credit,
            running_balance: section.closing_balance,
        });
    }

    Ok(GlDetail { sections })
}

/// One posting since the audit window opened, with who posted it and when
/// (§8 "Audit trail").
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRow {
    pub entry_id: String,
    pub entry_date: NaiveDate,
    pub source_type: DocKind,
    pub source_id: String,
    pub source_version: i64,
    pub memo: Option<String>,
    pub is_flagged: bool,
    pub reversal_of: Option<String>,
    pub actor: String,
    pub command_kind: String,
    pub at: DateTime<Utc>,
}

/// Every posting since `since` (or since the last close, when `since` is
/// `None`), flagged entries first. Filtering to manual and imported entries
/// only — "the list a CPA actually reads" (§8) — is `is_flagged` on the
/// result, since every manual and imported entry carries that flag (§4, W18).
pub fn audit_trail(
    ledger: &Ledger,
    company: &str,
    since: Option<NaiveDate>,
) -> Result<Vec<AuditRow>, AccountantError> {
    let cutoff = match since {
        Some(date) => date,
        None => ledger.locked_through(company)?.unwrap_or(NaiveDate::MIN),
    };
    let rows = ledger.posted_entries_since(company, cutoff)?;
    Ok(rows
        .into_iter()
        .map(|row| AuditRow {
            entry_id: row.entry_id,
            entry_date: row.entry.entry_date,
            source_type: row.entry.source_type,
            source_id: row.entry.source_id,
            source_version: row.entry.source_version,
            memo: row.entry.memo,
            is_flagged: row.entry.is_flagged,
            reversal_of: row.entry.reversal_of,
            actor: row.actor_id,
            command_kind: row.command_kind,
            at: row.command_at,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 2. Exports
// ---------------------------------------------------------------------------

/// A report that renders as one CSV with a stable header row. Implemented
/// for [`TrialBalance`], [`Pnl`], [`BalanceSheet`] and [`GlDetail`] — the
/// four §8 "Exports" names this crate's `report` module has. There is no
/// `AgingReport` in [`crate::report`] today (AR/AP aging is listed in §8's
/// scope table but not yet built), so it is skipped here rather than
/// invented; add an `impl ToCsv for AgingReport` alongside it when it exists.
pub trait ToCsv {
    fn to_csv(&self) -> String;
}

/// `export_csv(&report)` for any of the four types above.
pub fn export_csv<T: ToCsv + ?Sized>(report: &T) -> String {
    report.to_csv()
}

fn csv_field(raw: &str) -> String {
    if raw.contains(',') || raw.contains('"') || raw.contains('\n') {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

fn csv_money(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

impl ToCsv for TrialBalance {
    fn to_csv(&self) -> String {
        let mut out =
            String::from("account_number,account_name,classification,debit,credit,balance\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                csv_field(&row.number),
                csv_field(&row.name),
                row.classification.as_str(),
                csv_money(row.debit),
                csv_money(row.credit),
                csv_money(row.balance),
            ));
        }
        out.push_str(&format!(
            "TOTAL,,,{},{},\n",
            csv_money(self.total_debits),
            csv_money(self.total_credits),
        ));
        out
    }
}

impl ToCsv for Pnl {
    fn to_csv(&self) -> String {
        let mut out = String::from("section,account,amount\n");
        for (section, rows) in [
            ("income", &self.income.by_account),
            ("cogs", &self.cogs.by_account),
            ("expense", &self.expense.by_account),
        ] {
            for (account, amount) in rows {
                out.push_str(&format!(
                    "{section},{},{}\n",
                    csv_field(&account.0),
                    csv_money(*amount)
                ));
            }
        }
        out.push_str(&format!(
            "summary,gross_margin,{}\n",
            csv_money(self.gross_margin)
        ));
        out.push_str(&format!(
            "summary,net_income,{}\n",
            csv_money(self.net_income)
        ));
        out
    }
}

impl ToCsv for BalanceSheet {
    fn to_csv(&self) -> String {
        let mut out = String::from("section,account,name,amount\n");
        for (section, sec) in [
            ("asset", &self.assets),
            ("liability", &self.liabilities),
            ("equity", &self.equity),
        ] {
            for row in &sec.rows {
                out.push_str(&format!(
                    "{section},{},{},{}\n",
                    row.account_id.as_ref().map(|a| a.0.as_str()).unwrap_or(""),
                    csv_field(&row.name),
                    csv_money(row.balance),
                ));
            }
        }
        out.push_str(&format!(
            "summary,,total_liabilities_and_equity,{}\n",
            csv_money(self.total_liabilities_and_equity),
        ));
        out
    }
}

impl ToCsv for GlDetail {
    fn to_csv(&self) -> String {
        let mut out = String::from(
            "account_number,account_name,entry_id,entry_date,source_type,source_id,memo,class,\
             entity_kind,entity_id,is_flagged,reversal_of,debit,credit,running_balance\n",
        );
        for section in &self.sections {
            for line in &section.lines {
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                    csv_field(&section.number),
                    csv_field(&section.name),
                    line.entry_id,
                    line.entry_date,
                    line.source_type.as_str(),
                    csv_field(&line.source_id),
                    csv_field(line.memo.as_deref().unwrap_or("")),
                    line.class.as_ref().map(|c| c.0.as_str()).unwrap_or(""),
                    line.entity.as_ref().map(entity_kind_str).unwrap_or(""),
                    line.entity.as_ref().map(|e| e.id.as_str()).unwrap_or(""),
                    line.is_flagged,
                    line.reversal_of.as_deref().unwrap_or(""),
                    csv_money(line.debit),
                    csv_money(line.credit),
                    csv_money(line.running_balance),
                ));
            }
        }
        out
    }
}

fn entity_kind_str(entity: &ContactRef) -> &'static str {
    match entity.kind {
        crate::types::ContactKind::Customer => "customer",
        crate::types::ContactKind::Vendor => "vendor",
    }
}

fn close_history_csv(rows: &[CloseHistoryRow]) -> String {
    let mut out = String::from("seq,at,actor,moved_from,moved_to,is_reopen,note\n");
    for row in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            row.seq,
            row.at.to_rfc3339(),
            csv_field(&row.actor),
            row.moved_from,
            row.moved_to,
            row.is_reopen,
            csv_field(&row.note),
        ));
    }
    out
}

/// Writes the year end handoff pack: `tb-<as_of>.csv`, `pnl-<fy>.csv`,
/// `bs-<as_of>.csv`, `gl-<fy>.csv` and `close-history.csv` into `dir`
/// (created if it does not exist), and returns the paths written, in that
/// order. `<fy>` is `as_of`'s calendar year (D7: fiscal year is calendar
/// year for both entities). PNL and GL cover that whole year to date; the
/// trial balance and balance sheet are as of `as_of`. **No PDF** — see the
/// module doc.
pub fn export_pack(
    ledger: &Ledger,
    company: &str,
    as_of: NaiveDate,
    dir: &Path,
) -> Result<Vec<PathBuf>, AccountantError> {
    std::fs::create_dir_all(dir)?;

    let fy = as_of.year();
    let year_start =
        NaiveDate::from_ymd_opt(fy, 1, 1).ok_or_else(|| LedgerError::BadDate(as_of.to_string()))?;

    let tb = report::trial_balance(ledger, company, as_of)?;
    let pnl = report::profit_and_loss(ledger, company, year_start, as_of)?;
    let bs = report::balance_sheet(ledger, company, as_of)?;
    let gl = general_ledger(ledger, company, year_start, as_of, None)?;
    let history = ledger.close_history(company)?;

    let mut paths = Vec::new();
    write_csv(
        dir,
        &format!("tb-{as_of}.csv"),
        &export_csv(&tb),
        &mut paths,
    )?;
    write_csv(dir, &format!("pnl-{fy}.csv"), &export_csv(&pnl), &mut paths)?;
    write_csv(
        dir,
        &format!("bs-{as_of}.csv"),
        &export_csv(&bs),
        &mut paths,
    )?;
    write_csv(dir, &format!("gl-{fy}.csv"), &export_csv(&gl), &mut paths)?;
    write_csv(
        dir,
        "close-history.csv",
        &close_history_csv(&history),
        &mut paths,
    )?;

    Ok(paths)
}

fn write_csv(
    dir: &Path,
    name: &str,
    content: &str,
    paths: &mut Vec<PathBuf>,
) -> Result<(), AccountantError> {
    let path = dir.join(name);
    std::fs::write(&path, content)?;
    paths.push(path);
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. The adjusting-entry request queue
// ---------------------------------------------------------------------------

/// One `adjustment_requests` row, with its stored lines deserialised and
/// [`Self::period_closed`] recomputed against the ledger's *current*
/// `locked_through` — it is not stored, because whether the request's
/// effective date sits in a closed period can change after the request was
/// made (§8: "a request dated into a closed period is accepted as a
/// proposal but flagged `period_closed: true`... because approving it will
/// need a reopen").
#[derive(Clone, Debug, PartialEq)]
pub struct AdjustmentRequest {
    pub request_id: String,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub description: String,
    pub lines: Vec<JournalLine>,
    pub state: AdjustmentState,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    pub posted_entry_id: Option<String>,
    /// Whether `requested_at`'s date falls on or before the company's
    /// current `locked_through`. Approving such a request will hit the same
    /// period gate `post_entry` applies to everything else, and needs a
    /// reopen first.
    pub period_closed: bool,
}

fn row_to_request(
    row: AdjustmentRequestRow,
    locked_through: Option<NaiveDate>,
) -> Result<AdjustmentRequest, AccountantError> {
    let lines: Vec<JournalLine> = serde_json::from_str(&row.lines_json)?;
    let period_closed = locked_through
        .map(|locked| row.requested_at.date_naive() <= locked)
        .unwrap_or(false);
    Ok(AdjustmentRequest {
        request_id: row.request_id,
        requested_by: row.requested_by,
        requested_at: row.requested_at,
        description: row.description,
        lines,
        state: row.state,
        decided_by: row.decided_by,
        decided_at: row.decided_at,
        decision_note: row.decision_note,
        posted_entry_id: row.posted_entry_id,
        period_closed,
    })
}

/// The §3 class rule plus the balance check, run over a *proposed* set of
/// lines rather than a posted [`JournalEntry`] — see the module doc for why
/// this duplicates rather than calls `crate::store`'s private check.
fn validate_lines(
    ledger: &Ledger,
    company: &str,
    lines: &[JournalLine],
) -> Result<(), AccountantError> {
    if lines.is_empty() {
        return Err(AccountantError::Unbalanced);
    }

    let probe = JournalEntry {
        entry_date: NaiveDate::MIN,
        memo: None,
        source_type: DocKind::JournalEntry,
        source_id: String::new(),
        source_version: 0,
        reversal_of: None,
        is_flagged: true,
        lines: lines.to_vec(),
    };
    if !probe.is_balanced() {
        return Err(AccountantError::Unbalanced);
    }

    let accounts: HashMap<String, crate::store::Account> = ledger
        .list_accounts(company)?
        .into_iter()
        .map(|a| (a.account_id.0.clone(), a))
        .collect();
    let classes: HashSet<String> = ledger
        .list_classes(company)?
        .into_iter()
        .map(|c| c.class_id.0)
        .collect();

    for line in lines {
        let account = accounts
            .get(&line.account.0)
            .ok_or_else(|| AccountantError::UnknownAccount(line.account.0.clone()))?;
        if let Some(class) = &line.class {
            if !classes.contains(&class.0) {
                return Err(AccountantError::UnknownClass(class.0.clone()));
            }
        }
        let needs_class = account
            .number
            .chars()
            .next()
            .map(|c| matches!(c, '4' | '5' | '6'))
            .unwrap_or(false);
        if needs_class && line.class.is_none() {
            return Err(AccountantError::MissingClass {
                line_no: line.line_no,
            });
        }
        if !needs_class && line.class.is_some() {
            return Err(AccountantError::UnexpectedClass {
                line_no: line.line_no,
            });
        }
    }
    Ok(())
}

/// Proposes an adjusting entry: date, lines, accounts, classes, memo, reason
/// (§8). `now` is both the request timestamp and the entry's effective date
/// if approved — there is no separate "as of" date, since the request *is*
/// the transaction the accountant is asking for. Lines must balance and pass
/// the §3 class rule; both are checked here, so a rejection is immediate
/// rather than discovered at approval time.
pub fn propose_adjustment(
    ledger: &Ledger,
    company: &str,
    requested_by: &str,
    description: &str,
    lines: Vec<JournalLine>,
    now: DateTime<Utc>,
) -> Result<AdjustmentRequest, AccountantError> {
    validate_lines(ledger, company, &lines)?;

    let request_id = Uuid::now_v7().to_string();
    let lines_json = serde_json::to_string(&lines)?;
    ledger.insert_adjustment_request(
        company,
        &request_id,
        requested_by,
        now,
        description,
        &lines_json,
    )?;

    let period_closed = ledger
        .locked_through(company)?
        .map(|locked| now.date_naive() <= locked)
        .unwrap_or(false);

    Ok(AdjustmentRequest {
        request_id,
        requested_by: requested_by.to_string(),
        requested_at: now,
        description: description.to_string(),
        lines,
        state: AdjustmentState::Proposed,
        decided_by: None,
        decided_at: None,
        decision_note: None,
        posted_entry_id: None,
        period_closed,
    })
}

/// Selects one posted line by `(entry_id, line_no)` and proposes a balanced
/// two-line adjusting entry that reverses it on its original account/class
/// and reposts the same amount to `new_account`/`new_class` (§8 "Reclassify
/// tool"). It never touches the original entry — a reclassify is always a
/// proposal, same as any other adjustment.
#[allow(clippy::too_many_arguments)]
pub fn propose_reclassify(
    ledger: &Ledger,
    company: &str,
    entry_id: &str,
    line_no: i64,
    new_account: AccountId,
    new_class: Option<ClassId>,
    requested_by: &str,
    now: DateTime<Utc>,
) -> Result<AdjustmentRequest, AccountantError> {
    let (entry, is_posted) = ledger.entry(company, entry_id)?;
    if !is_posted {
        return Err(AccountantError::NotFound);
    }
    let original = entry
        .lines
        .iter()
        .find(|line| line.line_no == line_no)
        .ok_or_else(|| AccountantError::LineNotFound {
            entry_id: entry_id.to_string(),
            line_no,
        })?;

    let reverse = JournalLine {
        line_no: 1,
        account: original.account.clone(),
        class: original.class.clone(),
        debit: original.credit,
        credit: original.debit,
        memo: Some(format!("reclassify: reverse {entry_id} line {line_no}")),
        entity: original.entity.clone(),
    };
    let repost = JournalLine {
        line_no: 2,
        account: new_account.clone(),
        class: new_class,
        debit: original.debit,
        credit: original.credit,
        memo: Some(format!(
            "reclassify: {entry_id} line {line_no} to {}",
            new_account.0
        )),
        entity: original.entity.clone(),
    };

    let description = format!(
        "reclassify entry {entry_id} line {line_no} from {} to {}",
        original.account.0, new_account.0
    );

    propose_adjustment(
        ledger,
        company,
        requested_by,
        &description,
        vec![reverse, repost],
        now,
    )
}

pub fn list_adjustments(
    ledger: &Ledger,
    company: &str,
    state: Option<AdjustmentState>,
) -> Result<Vec<AdjustmentRequest>, AccountantError> {
    let locked = ledger.locked_through(company)?;
    ledger
        .list_adjustment_requests(company, state)?
        .into_iter()
        .map(|row| row_to_request(row, locked))
        .collect()
}

/// Builds the `JournalEntry`-kind document an approved request posts as
/// (§8). One `DocLine` per requested line, `LineKind::Journal` so
/// `crate::post::post` carries every account, side, class and entity
/// straight through with no default chain — the request already said
/// exactly what it wants posted.
fn build_journal_document(row: &AdjustmentRequestRow, lines: &[JournalLine]) -> LedgerDocument {
    let doc_lines = lines
        .iter()
        .map(|line| {
            let (amount, side) = if !line.debit.is_zero() {
                (line.debit, Side::Debit)
            } else {
                (line.credit, Side::Credit)
            };
            DocLine {
                line_no: line.line_no,
                kind: LineKind::Journal,
                amount,
                class: line.class.clone(),
                item_id: None,
                account: Some(line.account.clone()),
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: line.memo.clone(),
                posting: Some(side),
                entity: line.entity.clone(),
            }
        })
        .collect();

    LedgerDocument {
        document_id: format!("adjustment-{}", row.request_id),
        kind: DocKind::JournalEntry,
        number: None,
        txn_date: row.requested_at.date_naive(),
        due_date: None,
        contact: None,
        header_class: None,
        lines: doc_lines,
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: Some(row.description.clone()),
    }
}

/// Decides a proposed request once (§8). Rejecting records the decision
/// only. Approving saves the request's lines as a `JournalEntry` document
/// and posts it flagged through [`crate::store::Ledger::post_entry`] — which
/// means approving a request whose effective date falls in a closed period
/// fails with the ordinary period-closed error, exactly as the design note
/// says: "approving it will need a reopen". A request already decided
/// refuses rather than overwriting the earlier decision.
pub fn decide_adjustment(
    ledger: &Ledger,
    company: &str,
    request_id: &str,
    decided_by: &str,
    approve: bool,
    note: &str,
    now: DateTime<Utc>,
) -> Result<AdjustmentRequest, AccountantError> {
    let row = ledger
        .adjustment_request(company, request_id)?
        .ok_or(AccountantError::NotFound)?;
    if row.state != AdjustmentState::Proposed {
        return Err(AccountantError::AlreadyDecided {
            request_id: request_id.to_string(),
        });
    }

    if !approve {
        ledger.decide_adjustment_request(
            company,
            request_id,
            decided_by,
            now,
            note,
            AdjustmentState::Rejected,
            None,
        )?;
    } else {
        let lines: Vec<JournalLine> = serde_json::from_str(&row.lines_json)?;
        let doc = build_journal_document(&row, &lines);
        let version_id = ledger.save_document(
            company,
            &doc,
            CommandMeta {
                actor_id: decided_by.to_string(),
                kind: "approve_adjustment".to_string(),
                hlc: Uuid::now_v7().to_string(),
            },
            now,
        )?;
        let posting = post::post(&doc, version_id.version, &PostingContext::default())?;
        let entry = match posting {
            Posting::Entry(entry) => entry,
            Posting::NonPosting => {
                unreachable!("DocKind::JournalEntry always produces Posting::Entry")
            }
        };
        let entry_id = ledger.post_entry(company, &entry, now)?;
        ledger.decide_adjustment_request(
            company,
            request_id,
            decided_by,
            now,
            note,
            AdjustmentState::Posted,
            Some(entry_id.as_str()),
        )?;
    }

    let updated = ledger
        .adjustment_request(company, request_id)?
        .ok_or(AccountantError::NotFound)?;
    row_to_request(updated, ledger.locked_through(company)?)
}

// ---------------------------------------------------------------------------
// 4. AccountantView
// ---------------------------------------------------------------------------

/// The role Joel's process gets (§8, D17, W25): every report, the exports,
/// and `propose_*` — nothing else. There is no `post_entry`, `close_period`,
/// `reopen_period` or `save_document` method here; the period-locked,
/// read-only guarantee is the absence of those methods from this type's
/// surface, checked by the compiler wherever `AccountantView` is used,
/// rather than a runtime role check that a bug could route around.
///
/// **Open (W24):** how Joel's process reaches this — a local install reading
/// a read-only SQLite copy, versus a hosted view — is unresolved; see
/// `LEDGER-DESIGN.md` §8 "How Joel reaches it". This type is the same either
/// way; only what process constructs it, and against what copy of the file,
/// differs.
pub struct AccountantView<'a> {
    ledger: &'a Ledger,
    company: String,
}

impl<'a> AccountantView<'a> {
    pub fn new(ledger: &'a Ledger, company: impl Into<String>) -> Self {
        AccountantView {
            ledger,
            company: company.into(),
        }
    }

    pub fn trial_balance(&self, as_of: NaiveDate) -> Result<TrialBalance, LedgerError> {
        report::trial_balance(self.ledger, &self.company, as_of)
    }

    pub fn profit_and_loss(&self, from: NaiveDate, to: NaiveDate) -> Result<Pnl, LedgerError> {
        report::profit_and_loss(self.ledger, &self.company, from, to)
    }

    pub fn balance_sheet(&self, as_of: NaiveDate) -> Result<BalanceSheet, LedgerError> {
        report::balance_sheet(self.ledger, &self.company, as_of)
    }

    pub fn sales_tax_lines(
        &self,
        quarter: (i32, u32),
        rate: Decimal,
    ) -> Result<SalesTaxLines, LedgerError> {
        report::sales_tax_lines(self.ledger, &self.company, quarter, rate)
    }

    pub fn general_ledger(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        account: Option<&AccountId>,
    ) -> Result<GlDetail, AccountantError> {
        general_ledger(self.ledger, &self.company, from, to, account)
    }

    pub fn audit_trail(&self, since: Option<NaiveDate>) -> Result<Vec<AuditRow>, AccountantError> {
        audit_trail(self.ledger, &self.company, since)
    }

    pub fn close_history(&self) -> Result<Vec<CloseHistoryRow>, LedgerError> {
        self.ledger.close_history(&self.company)
    }

    pub fn list_adjustments(
        &self,
        state: Option<AdjustmentState>,
    ) -> Result<Vec<AdjustmentRequest>, AccountantError> {
        list_adjustments(self.ledger, &self.company, state)
    }

    pub fn propose_adjustment(
        &self,
        requested_by: &str,
        description: &str,
        lines: Vec<JournalLine>,
        now: DateTime<Utc>,
    ) -> Result<AdjustmentRequest, AccountantError> {
        propose_adjustment(
            self.ledger,
            &self.company,
            requested_by,
            description,
            lines,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn propose_reclassify(
        &self,
        entry_id: &str,
        line_no: i64,
        new_account: AccountId,
        new_class: Option<ClassId>,
        requested_by: &str,
        now: DateTime<Utc>,
    ) -> Result<AdjustmentRequest, AccountantError> {
        propose_reclassify(
            self.ledger,
            &self.company,
            entry_id,
            line_no,
            new_account,
            new_class,
            requested_by,
            now,
        )
    }
}
