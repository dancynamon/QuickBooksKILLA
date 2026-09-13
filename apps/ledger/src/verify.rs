//! `ledger verify` — an invariants sweep over one company's book.
//! `LEDGER-DESIGN.md` §4 (the invariants table), §5 (period close).
//!
//! Every check here recomputes, from a plain `SELECT`, something the schema's
//! own `CHECK` constraints and triggers already claim to guarantee (§4). That
//! is deliberate: the triggers only fire on the transitions the ordinary
//! posting path takes (`BEFORE UPDATE OF is_posted`, in particular — a raw
//! `INSERT` that sets `is_posted = 1` directly never runs it), and a second
//! connection to the same file that never sets `PRAGMA foreign_keys = ON`
//! bypasses every composite foreign key in the schema. Neither bypass is
//! reachable through [`crate::store::Ledger`]'s own API, but the ledger file
//! is a plain SQLite file, and a wrong tool, a hand fix at 2am, or a bug
//! reachable only through raw SQL should still be caught here rather than
//! surfacing as a report that is quietly wrong.
//!
//! Every check but the last is fatal: `ledger verify` prints a table and
//! exits 1 if any fired. The last — a `needs_mapping` account carrying a
//! non-zero balance — is reported, not fatal (§2: "Either it gets a real
//! mapping or Joel accepts it as its own account", a sign-off decision, not a
//! bug).

use rusqlite::params;

use crate::store::{Ledger, LedgerError};

/// How serious one [`Finding`] is. Only [`Severity::Reported`] findings can
/// coexist with [`VerifyReport::is_ok`] returning `true`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Severity {
    Fatal,
    Reported,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Fatal => "FATAL",
            Severity::Reported => "reported",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub check: &'static str,
    pub severity: Severity,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub findings: Vec<Finding>,
}

impl VerifyReport {
    /// No fatal finding. A report can still carry [`Severity::Reported`]
    /// findings and be "ok" — that is the whole point of the two tiers.
    pub fn is_ok(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|finding| finding.severity == Severity::Fatal)
    }

    pub fn render_text(&self, company: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("VERIFY   {company}\n\n"));
        if self.findings.is_empty() {
            out.push_str("no invariant failures\n");
            return out;
        }
        out.push_str(&format!("{:<10}{:<55}detail\n", "severity", "check"));
        for finding in &self.findings {
            out.push_str(&format!(
                "{:<10}{:<55}{}\n",
                finding.severity.as_str(),
                finding.check,
                finding.detail,
            ));
        }
        let fatal = self
            .findings
            .iter()
            .filter(|finding| finding.severity == Severity::Fatal)
            .count();
        let reported = self.findings.len() - fatal;
        out.push_str(&format!("\nFATAL: {fatal}        REPORTED: {reported}\n"));
        out
    }
}

/// A row identifier list is capped so one badly corrupted book prints a
/// table, not a wall of ids.
const MAX_LISTED: usize = 20;

fn finding_if_any(check: &'static str, severity: Severity, mut ids: Vec<String>) -> Option<Finding> {
    if ids.is_empty() {
        return None;
    }
    let total = ids.len();
    let detail = if total > MAX_LISTED {
        ids.truncate(MAX_LISTED);
        format!("{} (+{} more)", ids.join(", "), total - MAX_LISTED)
    } else {
        ids.join(", ")
    };
    Some(Finding {
        check,
        severity,
        detail,
    })
}

/// The full sweep, over every posted entry and every account in `company`'s
/// book — no `as_of` bound, unlike the point-in-time reports in
/// [`crate::report`]: an invariant either holds for the whole ledger or it
/// does not.
pub fn verify(ledger: &Ledger, company: &str) -> Result<VerifyReport, LedgerError> {
    let mut findings = Vec::new();

    if let Some(finding) = check_entries_balance(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_lines_have_entries(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_entries_have_document_versions(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_tb_debits_equal_credits(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_income_cogs_expense_lines_have_class(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_no_posting_after_close(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_normal_balance_consistency(ledger, company)? {
        findings.push(finding);
    }
    if let Some(finding) = check_needs_mapping_balances(ledger, company)? {
        findings.push(finding);
    }

    Ok(VerifyReport { findings })
}

/// Every posted entry balances, recomputed in Rust from `journal_lines`
/// rather than trusted from the `is_posted` trigger — which never runs at
/// all for a row a raw `INSERT` set `is_posted = 1` on directly.
fn check_entries_balance(ledger: &Ledger, company: &str) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT e.entry_id
         FROM journal_entries e
         JOIN journal_lines l ON l.company_id = e.company_id AND l.entry_id = e.entry_id
         WHERE e.company_id = ?1 AND e.is_posted = 1
         GROUP BY e.entry_id
         HAVING SUM(l.debit_minor) <> SUM(l.credit_minor)",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "every posted entry balances",
        Severity::Fatal,
        offenders,
    ))
}

/// No row in `journal_lines` whose `entry_id` has no matching
/// `journal_entries` row — the composite foreign key this would otherwise
/// rely on only holds on a connection with `PRAGMA foreign_keys = ON`.
fn check_lines_have_entries(ledger: &Ledger, company: &str) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT l.entry_id || ':' || l.line_no
         FROM journal_lines l
         LEFT JOIN journal_entries e
             ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         WHERE l.company_id = ?1 AND e.entry_id IS NULL",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "no journal line without an entry",
        Severity::Fatal,
        offenders,
    ))
}

/// No `journal_entries` row whose `(source_id, source_version)` has no
/// matching `document_versions` row — "there is no new journal entry button"
/// (§4) only holds if every entry really does trace back to a document.
fn check_entries_have_document_versions(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT e.entry_id
         FROM journal_entries e
         LEFT JOIN document_versions d
             ON d.company_id = e.company_id
            AND d.document_id = e.source_id
            AND d.version = e.source_version
         WHERE e.company_id = ?1 AND d.document_id IS NULL",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "no entry without a document version",
        Severity::Fatal,
        offenders,
    ))
}

/// Total debits equal total credits over every posted line in the company's
/// book — the trial balance invariant at the ledger level, not per entry.
fn check_tb_debits_equal_credits(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    let (debit_minor, credit_minor): (i64, i64) = ledger.conn().query_row(
        "SELECT COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         WHERE l.company_id = ?1 AND e.is_posted = 1",
        params![company],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if debit_minor == credit_minor {
        return Ok(None);
    }
    Ok(Some(Finding {
        check: "trial balance debits equal credits",
        severity: Severity::Fatal,
        detail: format!(
            "debits {} <> credits {} (minor units)",
            debit_minor, credit_minor
        ),
    }))
}

/// No posted line on an income, COGS or expense account lacking a class —
/// the §3 rule the posting gate enforces on every ordinary post, restated as
/// an independent read so a line that reached `journal_lines` some other way
/// cannot silently break it.
fn check_income_cogs_expense_lines_have_class(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT l.entry_id || ':' || l.line_no
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         JOIN accounts a ON a.company_id = l.company_id AND a.account_id = l.account_id
         WHERE l.company_id = ?1 AND e.is_posted = 1
           AND a.classification IN ('Income', 'COGS', 'Expense')
           AND l.class_id IS NULL",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "no income/COGS/expense line without a class",
        Severity::Fatal,
        offenders,
    ))
}

/// No posted entry dated inside a closed period unless it was already posted
/// before that close happened (`posted_at <= closed_at`) — the period gate
/// (§5) refuses this on every ordinary post; this check is what notices if
/// something bypassed it.
fn check_no_posting_after_close(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT e.entry_id
         FROM journal_entries e
         JOIN periods p
             ON p.company_id = e.company_id AND p.state = 'closed' AND e.entry_date <= p.period_end
         WHERE e.company_id = ?1 AND e.is_posted = 1 AND e.posted_at > p.closed_at",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "no posted entry dated inside a closed period, posted after the close",
        Severity::Fatal,
        offenders,
    ))
}

/// Every account's `normal_balance` follows from its `classification`,
/// flipped by `is_contra` (§2) — via [`Ledger::list_accounts`] rather than
/// raw SQL, since that already gives typed fields with no re-parsing.
fn check_normal_balance_consistency(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    use crate::types::Side;

    let offenders: Vec<String> = ledger
        .list_accounts(company)?
        .into_iter()
        .filter_map(|account| {
            let expected = if account.is_contra {
                match account.classification.normal_balance() {
                    Side::Debit => Side::Credit,
                    Side::Credit => Side::Debit,
                }
            } else {
                account.classification.normal_balance()
            };
            (expected != account.normal_balance).then_some(account.account_id.0)
        })
        .collect();
    Ok(finding_if_any(
        "normal_balance consistent with classification and is_contra",
        Severity::Fatal,
        offenders,
    ))
}

/// A `needs_mapping` account carrying a non-zero posted balance — reported,
/// not fatal (§2).
fn check_needs_mapping_balances(
    ledger: &Ledger,
    company: &str,
) -> Result<Option<Finding>, LedgerError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT a.number || ' ' || a.name
         FROM accounts a
         JOIN journal_lines l ON l.company_id = a.company_id AND l.account_id = a.account_id
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         WHERE a.company_id = ?1 AND a.needs_mapping = 1 AND e.is_posted = 1
         GROUP BY a.account_id
         HAVING SUM(l.debit_minor) - SUM(l.credit_minor) <> 0",
    )?;
    let offenders = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(finding_if_any(
        "needs_mapping account with a non-zero balance",
        Severity::Reported,
        offenders,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};

    use crate::chart;
    use crate::store::CommandMeta;
    use crate::types::{
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

    /// A fresh company on a real temp file — every test in this module
    /// corrupts state through raw SQL on `ledger.conn()`, which is
    /// `pub(crate)` and so reachable from this in-crate test module without
    /// a second connection.
    fn temp_ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join("ledger.sqlite")).unwrap();
        ledger
            .create_company("aquamentor", "Aquamentor LLC", None, now())
            .unwrap();
        (dir, ledger)
    }

    fn seed_document(ledger: &Ledger, document_id: &str, txn_date: NaiveDate) -> String {
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
        let version = ledger
            .save_document("aquamentor", &doc, command("save_invoice"), now())
            .unwrap();
        version.document_id
    }

    fn invoice_entry(document_id: &str, on: NaiveDate) -> JournalEntry {
        JournalEntry {
            entry_date: on,
            memo: Some("INV-9001".to_string()),
            source_type: DocKind::Invoice,
            source_id: document_id.to_string(),
            source_version: 1,
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
    fn a_clean_book_verifies_with_no_findings() {
        let (_dir, ledger) = temp_ledger();
        let doc = seed_document(&ledger, "doc-1", ymd(2026, 6, 15));
        ledger
            .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(report.is_ok(), "{report:?}");
        assert!(report.findings.is_empty(), "{report:?}");
    }

    #[test]
    fn an_unbalanced_entry_inserted_raw_is_caught() {
        let (_dir, ledger) = temp_ledger();
        let doc = seed_document(&ledger, "doc-1", ymd(2026, 6, 15));

        // Bypasses `post_entry` (and so the balance trigger, which only fires
        // on the `is_posted` 0 -> 1 *transition*, never on an INSERT that
        // sets it to 1 outright).
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_entries
                     (company_id, entry_id, entry_date, posted_at, memo, source_type,
                      source_id, source_version, is_posted, is_flagged)
                 VALUES ('aquamentor', 'bad-entry', '2026-06-15', '2026-06-15T00:00:00Z', NULL,
                         'invoice', ?1, 1, 1, 0)",
                params![doc],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'bad-entry', 1, ?1, NULL, 10000, 0)",
                params![chart::ACCOUNTS_RECEIVABLE],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'bad-entry', 2, ?1, 'foam', 0, 9000)",
                params![chart::SALES_INCOME],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report
            .findings
            .iter()
            .any(|f| f.check == "every posted entry balances" && f.detail.contains("bad-entry")));
        // The ledger-wide debit/credit check also fires off the same
        // corruption — both are real, independent invariants.
        assert!(report
            .findings
            .iter()
            .any(|f| f.check == "trial balance debits equal credits"));
    }

    #[test]
    fn a_line_with_no_entry_is_caught() {
        let (_dir, ledger) = temp_ledger();
        // Composite foreign keys are enforced per connection; turning the
        // pragma off for this one raw insert is what lets an orphan row
        // exist at all, which is exactly the bypass this check exists for.
        ledger
            .conn()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'no-such-entry', 1, ?1, NULL, 100, 0)",
                params![chart::CHECKING],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report
            .findings
            .iter()
            .any(|f| f.check == "no journal line without an entry"
                && f.detail.contains("no-such-entry:1")));
    }

    #[test]
    fn an_entry_with_no_document_version_is_caught() {
        let (_dir, ledger) = temp_ledger();
        ledger
            .conn()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_entries
                     (company_id, entry_id, entry_date, posted_at, source_type,
                      source_id, source_version, is_posted, is_flagged)
                 VALUES ('aquamentor', 'orphan-entry', '2026-06-15', '2026-06-15T00:00:00Z',
                         'invoice', 'no-such-doc', 1, 1, 0)",
                [],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'orphan-entry', 1, ?1, NULL, 100, 0)",
                params![chart::CHECKING],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'orphan-entry', 2, ?1, NULL, 0, 100)",
                params![chart::OWNER_CAPITAL],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report
            .findings
            .iter()
            .any(|f| f.check == "no entry without a document version"
                && f.detail.contains("orphan-entry")));
    }

    #[test]
    fn an_income_line_with_no_class_is_caught() {
        let (_dir, ledger) = temp_ledger();
        let doc = seed_document(&ledger, "doc-1", ymd(2026, 6, 15));

        ledger
            .conn()
            .execute(
                "INSERT INTO journal_entries
                     (company_id, entry_id, entry_date, posted_at, source_type,
                      source_id, source_version, is_posted, is_flagged)
                 VALUES ('aquamentor', 'no-class-entry', '2026-06-15', '2026-06-15T00:00:00Z',
                         'invoice', ?1, 1, 1, 0)",
                params![doc],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'no-class-entry', 1, ?1, NULL, 100, 0)",
                params![chart::ACCOUNTS_RECEIVABLE],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'no-class-entry', 2, ?1, NULL, 0, 100)",
                params![chart::SALES_INCOME],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report.findings.iter().any(|f| f.check
            == "no income/COGS/expense line without a class"
            && f.detail.contains("no-class-entry:2")));
    }

    #[test]
    fn a_posting_after_its_period_closed_is_caught() {
        let (_dir, ledger) = temp_ledger();
        let doc = seed_document(&ledger, "doc-1", ymd(2026, 6, 15));
        ledger
            .post_entry("aquamentor", &invoice_entry(&doc, ymd(2026, 6, 15)), now())
            .unwrap();
        ledger
            .close_period("aquamentor", ymd(2026, 6, 30), "dan", "June close", now())
            .unwrap();

        // The gate refuses this through `post_entry`; simulate a bypass by
        // inserting a second entry dated inside the closed period (30 June)
        // but stamped `posted_at` *after* the close happened (12 September,
        // `now()` above) — the shape only a raw insert could produce.
        let doc2 = seed_document(&ledger, "doc-2", ymd(2026, 6, 20));
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_entries
                     (company_id, entry_id, entry_date, posted_at, source_type,
                      source_id, source_version, is_posted, is_flagged)
                 VALUES ('aquamentor', 'late-entry', '2026-06-20', '2026-09-15T00:00:00Z',
                         'invoice', ?1, 1, 1, 0)",
                params![doc2],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'late-entry', 1, ?1, NULL, 100, 0)",
                params![chart::ACCOUNTS_RECEIVABLE],
            )
            .unwrap();
        ledger
            .conn()
            .execute(
                "INSERT INTO journal_lines
                     (company_id, entry_id, line_no, account_id, class_id, debit_minor, credit_minor)
                 VALUES ('aquamentor', 'late-entry', 2, ?1, 'foam', 0, 100)",
                params![chart::SALES_INCOME],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report.findings.iter().any(|f| f.check
            == "no posted entry dated inside a closed period, posted after the close"
            && f.detail.contains("late-entry")));
    }

    #[test]
    fn a_mismatched_normal_balance_is_caught() {
        let (_dir, ledger) = temp_ledger();
        ledger
            .conn()
            .execute(
                "UPDATE accounts SET normal_balance = 'Cr'
                 WHERE company_id = 'aquamentor' AND account_id = ?1",
                params![chart::CHECKING],
            )
            .unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(!report.is_ok());
        assert!(report.findings.iter().any(|f| {
            f.check == "normal_balance consistent with classification and is_contra"
                && f.detail.contains(chart::CHECKING)
        }));
    }

    #[test]
    fn a_needs_mapping_account_with_a_balance_is_reported_but_not_fatal() {
        let (_dir, ledger) = temp_ledger();
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
        let doc = seed_document(&ledger, "doc-1", ymd(2026, 6, 15));
        let mut entry = invoice_entry(&doc, ymd(2026, 6, 15));
        entry.lines[1].account = AccountId("4995".to_string());
        ledger.post_entry("aquamentor", &entry, now()).unwrap();

        let report = verify(&ledger, "aquamentor").unwrap();
        assert!(
            report.is_ok(),
            "a reported-only finding must not fail is_ok: {report:?}"
        );
        let finding = report
            .findings
            .iter()
            .find(|f| f.check == "needs_mapping account with a non-zero balance")
            .unwrap_or_else(|| panic!("expected the needs_mapping finding: {report:?}"));
        assert_eq!(finding.severity, Severity::Reported);
        assert!(finding.detail.contains("4995"));
    }

    #[test]
    fn render_text_names_fatal_and_reported_counts() {
        let report = VerifyReport {
            findings: vec![
                Finding {
                    check: "every posted entry balances",
                    severity: Severity::Fatal,
                    detail: "bad-entry".to_string(),
                },
                Finding {
                    check: "needs_mapping account with a non-zero balance",
                    severity: Severity::Reported,
                    detail: "4995 QBO Uncategorised Income".to_string(),
                },
            ],
        };
        let text = report.render_text("aquamentor");
        assert!(text.contains("FATAL: 1"));
        assert!(text.contains("REPORTED: 1"));
        assert!(text.contains("bad-entry"));
    }

    #[test]
    fn render_text_on_a_clean_report_says_so() {
        let report = VerifyReport::default();
        assert_eq!(
            report.render_text("aquamentor"),
            "VERIFY   aquamentor\n\nno invariant failures\n"
        );
    }
}
