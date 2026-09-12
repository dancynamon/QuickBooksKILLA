//! Bank statement import, matching, unmatched proposals and the
//! per-statement close. `LEDGER-DESIGN.md` §10; the Bank line, Deposit and
//! Settlement posting rows are §1; D15 (Authorize.net at cutover) and D17
//! (bank and card feeds are a per-statement close, no aggregator) are why
//! this exists at all.
//!
//! The shape mirrors the rest of the crate: [`csv`] and [`ofx`] are pure
//! parsers (file bytes to [`ParsedLine`]s, no database); everything that
//! touches the store is a method on [`crate::store::Ledger`], added here
//! rather than in `store.rs`, because matching and the close are policy
//! (§10), not schema. `store.rs` owns only the migration and the plain
//! reads and writes this module is built on.
//!
//! A bank statement line never posts by itself (§1's Bank line row: "Non
//! posting"). [`Ledger::match_statement`] only ever *proposes* an account for
//! a line it cannot match to something already posted, and the only thing
//! that turns a proposal into a transaction is [`Ledger::confirm_proposal`],
//! called with a human's explicit choice of account. Nothing here builds a
//! [`LedgerDocument`] from an unconfirmed proposal.

pub mod csv;
pub mod ofx;

pub use csv::{parse_csv, AmountCols, CsvProfile};
pub use ofx::{parse_ofx, parse_ofx_ledger_balance, parse_ofx_statement_period};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use thiserror::Error;
use uuid::Uuid;

use ledger_core::Money;

use crate::chart;
use crate::post::{self, Posting};
use crate::store::{BankLineRow, CommandMeta, EntryId, Ledger, LedgerError};
use crate::types::{
    AccountId, ClassId, DocKind, DocLine, LedgerDocument, LineKind, PostingContext, Side,
};

/// A parser's output for one transaction, before it is a bank line: pure
/// data, no company, no statement, no matching state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    pub posted_on: NaiveDate,
    /// Signed: positive is money in, negative is money out.
    pub amount: Money,
    pub description: String,
    /// The bank's own id for the transaction, when the file carries one
    /// (OFX's `FITID`; a CSV export commonly has none). Drives the §10
    /// dedupe on re-import.
    pub external_id: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("row {row}: {reason}")]
    Malformed { row: usize, reason: String },
    /// D9: an amount carrying more than two decimal places is quarantined,
    /// never rounded silently.
    #[error("row {row}: amount {value:?} carries more than two decimal places")]
    Precision { row: usize, value: String },
    #[error("row {row}: invalid date {value:?}, expected format {format}")]
    BadDate {
        row: usize,
        value: String,
        format: &'static str,
    },
    #[error(transparent)]
    Money(#[from] ledger_core::MoneyError),
}

/// What [`Ledger::import_statement`] needs beyond the parsed lines: the
/// statement's own stated boundaries, which the §10 close checks the lines
/// against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewStatement {
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    pub opening_balance: Money,
    pub closing_balance: Money,
    pub lines: Vec<ParsedLine>,
}

/// What one call to [`Ledger::import_statement`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementImportReport {
    pub statement_id: String,
    pub inserted: usize,
    pub skipped_duplicate: usize,
}

/// §10's match rules, parameterised. `date_window_days` is the "configurable
/// window (default four days either side)" of the design document; this
/// crate's default is 3, per the concrete build spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchRules {
    pub date_window_days: i64,
}

impl Default for MatchRules {
    fn default() -> Self {
        MatchRules {
            date_window_days: 3,
        }
    }
}

/// Which side of the ledger a confirmed proposal will post: money out
/// (`Purchase`-shaped) or money in (`Deposit`-shaped). §1's Bank line row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalKind {
    Expense,
    Deposit,
}

/// An unmatched line's suggested resolution. Never posted on its own — see
/// the module docs — only returned for a human to accept or override via
/// [`Ledger::confirm_proposal`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub line_id: String,
    pub kind: ProposalKind,
    pub suggested_account: AccountId,
    pub reason: String,
}

/// What one call to [`Ledger::match_statement`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchReport {
    pub exact: usize,
    pub settlement: usize,
    pub proposals: Vec<Proposal>,
}

/// What a successful [`Ledger::close_statement`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementClose {
    pub statement_id: String,
    pub closed_at: DateTime<Utc>,
}

const SETTLEMENT_SOURCE_KINDS: &[&str] = &["settlement", "deposit"];

/// §10 rule 2's own words: "against an unmatched payment, deposit, bill
/// payment, or purchase". Deliberately excludes `settlement` so a
/// Settlement entry's bank leg is left for rule 3 to claim instead.
const EXACT_MATCH_SOURCE_KINDS: &[&str] = &["payment", "deposit", "bill_payment", "purchase"];

impl Ledger {
    /// §10: read a parsed statement into `bank_statements`/`bank_lines`. A
    /// line whose `(account, external_id)` this ledger has already seen is
    /// skipped rather than duplicated — see the migration's unique index —
    /// so importing the same file twice (statements overlap at the edges)
    /// costs nothing.
    pub fn import_statement(
        &self,
        company: &str,
        account: &AccountId,
        statement: NewStatement,
        now: DateTime<Utc>,
    ) -> Result<StatementImportReport, LedgerError> {
        let statement_id = Uuid::now_v7().to_string();
        self.create_bank_statement(
            company,
            &statement_id,
            account,
            statement.period_start,
            statement.period_end,
            statement.opening_balance,
            statement.closing_balance,
            now,
        )?;

        let mut inserted = 0usize;
        let mut skipped_duplicate = 0usize;
        for parsed in &statement.lines {
            let line_id = Uuid::now_v7().to_string();
            let was_inserted = self.insert_bank_line_if_new(
                company,
                &line_id,
                &statement_id,
                account,
                parsed.posted_on,
                parsed.amount,
                &parsed.description,
                parsed.external_id.as_deref(),
            )?;
            if was_inserted {
                inserted += 1;
            } else {
                skipped_duplicate += 1;
            }
        }

        Ok(StatementImportReport {
            statement_id,
            inserted,
            skipped_duplicate,
        })
    }

    /// §10's match rules, in order, over every line of `statement_id` that
    /// has not yet been resolved (a second call skips anything already
    /// `exact`, `settlement`, `manual` or `proposed`, so re-running after a
    /// partial confirm is safe). A closed statement's lines cannot be
    /// re-matched.
    pub fn match_statement(
        &self,
        company: &str,
        statement_id: &str,
        rules: &MatchRules,
        now: DateTime<Utc>,
    ) -> Result<MatchReport, LedgerError> {
        let statement = self.bank_statement(company, statement_id)?;
        if statement.closed_at.is_some() {
            return Err(LedgerError::StatementClosed {
                statement_id: statement_id.to_string(),
            });
        }

        let mut report = MatchReport::default();
        for line in self.list_bank_lines(company, statement_id)? {
            if line.match_kind.is_some() {
                continue;
            }
            self.match_one_line(company, &line, rules, now, &mut report)?;
        }
        Ok(report)
    }

    fn match_one_line(
        &self,
        company: &str,
        line: &BankLineRow,
        rules: &MatchRules,
        now: DateTime<Utc>,
        report: &mut MatchReport,
    ) -> Result<(), LedgerError> {
        let window = Duration::days(rules.date_window_days);
        let from = line.posted_on - window;
        let to = line.posted_on + window;
        let is_money_in = !line.amount.is_negative();
        let amount_abs = if is_money_in {
            line.amount
        } else {
            line.amount.checked_neg()?
        };
        // A bank-in line matches a debit to the account (the account went
        // up); a bank-out line matches a credit.
        let side = if is_money_in {
            Side::Debit
        } else {
            Side::Credit
        };

        let mut candidates = self.uncleared_lines_matching(
            company,
            &line.account_id,
            side,
            amount_abs,
            EXACT_MATCH_SOURCE_KINDS,
            from,
            to,
        )?;
        candidates.sort_by_key(|(_, _, date)| (*date - line.posted_on).num_days().abs());
        if let Some((entry_id, line_no, _)) = candidates.into_iter().next() {
            self.clear_journal_line(company, &entry_id, line_no, now)?;
            self.set_bank_line_match(
                company,
                &line.line_id,
                "exact",
                Some(&entry_id),
                Some(line_no),
                now,
            )?;
            report.exact += 1;
            return Ok(());
        }

        if is_money_in && self.try_settlement_match(company, line, amount_abs, from, to, now)? {
            report.settlement += 1;
            return Ok(());
        }

        let (suggested_account, reason) = suggest_account(&line.description);
        let kind = if is_money_in {
            ProposalKind::Deposit
        } else {
            ProposalKind::Expense
        };
        self.set_bank_line_match(company, &line.line_id, "proposed", None, None, now)?;
        report.proposals.push(Proposal {
            line_id: line.line_id.clone(),
            kind,
            suggested_account,
            reason,
        });
        Ok(())
    }

    /// §10 rule 3: a deposit line matches a Settlement (or Deposit) entry
    /// whose credits to 1160 sum to the line's amount within the window.
    /// "The settlement import has already created the entry, so this rule
    /// matches to that entry rather than proposing a new one" — the entry's
    /// own leg on this statement's bank account is what gets `cleared_at`.
    fn try_settlement_match(
        &self,
        company: &str,
        line: &BankLineRow,
        amount: Money,
        from: NaiveDate,
        to: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<bool, LedgerError> {
        let entries = self.entries_with_credit_sum(
            company,
            chart::AUTHNET_CLEARING,
            amount,
            SETTLEMENT_SOURCE_KINDS,
            from,
            to,
        )?;

        for entry_id in entries {
            let (entry, _posted) = self.entry(company, &entry_id)?;
            let bank_leg = entry.lines.iter().find(|entry_line| {
                entry_line.account == line.account_id && !entry_line.debit.is_zero()
            });
            let Some(bank_leg) = bank_leg else { continue };
            if self.is_line_cleared(company, &entry_id, bank_leg.line_no)? {
                continue;
            }
            self.clear_journal_line(company, &entry_id, bank_leg.line_no, now)?;
            self.set_bank_line_match(
                company,
                &line.line_id,
                "settlement",
                Some(&entry_id),
                Some(bank_leg.line_no),
                now,
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Turns a confirmed proposal into a `BankLine` document: one `Account`
    /// line naming `account` (and `class`, when the account needs one),
    /// saved and posted through the same [`crate::post::post`] every other
    /// document goes through, then marks the bank line `manual`-matched and
    /// clears its bank-side journal line. Refuses a line that is not
    /// currently a pending proposal, so a proposal can be confirmed once.
    pub fn confirm_proposal(
        &self,
        company: &str,
        line_id: &str,
        account: &AccountId,
        class: Option<ClassId>,
        now: DateTime<Utc>,
    ) -> Result<EntryId, LedgerError> {
        let line = self.bank_line(company, line_id)?;
        if line.match_kind.as_deref() != Some("proposed") {
            return Err(LedgerError::NotAProposal {
                line_id: line_id.to_string(),
            });
        }
        let statement = self.bank_statement(company, &line.statement_id)?;
        if statement.closed_at.is_some() {
            return Err(LedgerError::StatementClosed {
                statement_id: line.statement_id.clone(),
            });
        }

        let is_money_in = !line.amount.is_negative();
        let amount_abs = if is_money_in {
            line.amount
        } else {
            line.amount.checked_neg()?
        };

        let doc_line = DocLine {
            line_no: 1,
            kind: LineKind::Account,
            amount: amount_abs,
            class,
            item_id: None,
            account: Some(account.clone()),
            is_taxable: false,
            qty: None,
            unit_cost: None,
            description: Some(line.description.clone()),
            posting: None,
            entity: None,
        };
        let mut doc = LedgerDocument {
            document_id: format!("bankline-{}", line.line_id),
            kind: DocKind::BankLine,
            number: None,
            txn_date: line.posted_on,
            due_date: None,
            contact: None,
            header_class: None,
            lines: vec![doc_line],
            tax: None,
            deposit_to: None,
            pay_from: None,
            applications: Vec::new(),
            unapplied: Money::ZERO,
            is_voided: false,
            source_ref: None,
            memo: Some(format!("bank line {}: {}", line.line_id, line.description)),
        };
        if is_money_in {
            doc.deposit_to = Some(line.account_id.clone());
        } else {
            doc.pay_from = Some(line.account_id.clone());
        }

        let version = self.save_document(
            company,
            &doc,
            CommandMeta {
                actor_id: "dan".to_string(),
                kind: "confirm_bank_proposal".to_string(),
                hlc: now.to_rfc3339(),
            },
            now,
        )?;

        let ctx = PostingContext::default();
        let entry = match post::post(&doc, version.version, &ctx)? {
            Posting::Entry(entry) => entry,
            Posting::NonPosting => unreachable!("BankLine always posts, DocKind::posts()"),
        };

        let bank_line_no = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == line.account_id)
            .map(|entry_line| entry_line.line_no)
            .ok_or(LedgerError::NotFound)?;

        let entry_id = self.post_entry(company, &entry, now)?;
        self.clear_journal_line(company, &entry_id, bank_line_no, now)?;
        self.set_bank_line_match(
            company,
            line_id,
            "manual",
            Some(&entry_id),
            Some(bank_line_no),
            now,
        )?;
        Ok(entry_id)
    }

    /// §10's per-statement close: `opening_balance + sum(matched lines) ==
    /// closing_balance`, and `sum(unmatched lines) == 0`. Either failing
    /// changes nothing and reports the exact shortfall and how many lines
    /// remain unmatched. Closing is idempotent: closing an already-closed
    /// statement a second time simply reports its existing `closed_at`.
    pub fn close_statement(
        &self,
        company: &str,
        statement_id: &str,
        now: DateTime<Utc>,
    ) -> Result<StatementClose, LedgerError> {
        let statement = self.bank_statement(company, statement_id)?;
        if let Some(closed_at) = &statement.closed_at {
            let closed_at = DateTime::parse_from_rfc3339(closed_at)
                .map_err(|_| LedgerError::NotFound)?
                .with_timezone(&Utc);
            return Ok(StatementClose {
                statement_id: statement_id.to_string(),
                closed_at,
            });
        }

        let lines = self.list_bank_lines(company, statement_id)?;
        let (matched, unmatched): (Vec<_>, Vec<_>) = lines
            .iter()
            .partition(|line| is_resolved(line.match_kind.as_deref()));

        let matched_sum = Money::checked_sum(matched.iter().map(|line| line.amount))?;
        let unmatched_sum = Money::checked_sum(unmatched.iter().map(|line| line.amount))?;
        let expected_closing = statement.opening_balance.checked_add(matched_sum)?;

        if expected_closing != statement.closing_balance || !unmatched_sum.is_zero() {
            let difference = statement.closing_balance.checked_sub(expected_closing)?;
            return Err(LedgerError::StatementDoesNotClose {
                difference,
                unmatched: unmatched.len(),
            });
        }

        self.close_bank_statement(company, statement_id, now)?;
        Ok(StatementClose {
            statement_id: statement_id.to_string(),
            closed_at: now,
        })
    }
}

fn is_resolved(match_kind: Option<&str>) -> bool {
    matches!(
        match_kind,
        Some("exact") | Some("settlement") | Some("manual")
    )
}

/// §10's small keyword table for an unmatched line's suggested account.
///
/// ⚠️ W28 (open, `LEDGER-DESIGN.md` §10): the default account for a money-out
/// line the description does not resolve. This uses 6900 "other operating
/// expense" with a review-flagging reason rather than a suspense account,
/// which is the recommendation pending Dan's confirmation — if a suspense
/// account is chosen instead, this default and its reason string are the one
/// place to change.
pub fn suggest_account(description: &str) -> (AccountId, String) {
    let upper = description.to_ascii_uppercase();

    const KEYWORDS: &[(&str, &str, &str)] = &[
        ("UPS", chart::OTHER_OPERATING, "unknown vendor"),
        ("FEDEX", chart::OTHER_OPERATING, "unknown vendor"),
        (
            "INTUIT",
            chart::ADVERTISING_CHANNEL_FEES,
            "channel/software fee",
        ),
        (
            "SHOPIFY",
            chart::ADVERTISING_CHANNEL_FEES,
            "channel/software fee",
        ),
        (
            "AMAZON",
            chart::ADVERTISING_CHANNEL_FEES,
            "channel/software fee",
        ),
        (
            "AUTHNET",
            chart::AUTHNET_CLEARING,
            "possible settlement, unresolved (W28)",
        ),
        (
            "AUTHORIZE",
            chart::AUTHNET_CLEARING,
            "possible settlement, unresolved (W28)",
        ),
        (
            "SUREPAYROLL",
            chart::PAYROLL_LIABILITIES,
            "payroll liability",
        ),
    ];
    for (keyword, account, reason) in KEYWORDS {
        if upper.contains(keyword) {
            return (AccountId((*account).to_string()), (*reason).to_string());
        }
    }
    if upper.contains("CHASE") && upper.contains("PAYMENT") {
        return (
            AccountId(chart::CREDIT_CARD.to_string()),
            "credit card payment".to_string(),
        );
    }
    (
        AccountId(chart::OTHER_OPERATING.to_string()),
        "unknown vendor".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_account_matches_the_keyword_table() {
        assert_eq!(
            suggest_account("UPS 1Z999AA1").0,
            AccountId(chart::OTHER_OPERATING.to_string())
        );
        assert_eq!(
            suggest_account("SHOPIFY FEES").0,
            AccountId(chart::ADVERTISING_CHANNEL_FEES.to_string())
        );
        assert_eq!(
            suggest_account("AUTHORIZE.NET SETTLEMENT").0,
            AccountId(chart::AUTHNET_CLEARING.to_string())
        );
        assert_eq!(
            suggest_account("SUREPAYROLL PPD").0,
            AccountId(chart::PAYROLL_LIABILITIES.to_string())
        );
        assert_eq!(
            suggest_account("CHASE CREDIT CRD PAYMENT").0,
            AccountId(chart::CREDIT_CARD.to_string())
        );
        assert_eq!(
            suggest_account("SOME UNKNOWN VENDOR LLC"),
            (
                AccountId(chart::OTHER_OPERATING.to_string()),
                "unknown vendor".to_string()
            )
        );
    }

    #[test]
    fn match_rules_default_is_a_three_day_window() {
        assert_eq!(MatchRules::default().date_window_days, 3);
    }
}
