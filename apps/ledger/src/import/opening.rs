//! Opening balances and the boundary-year walk. `LEDGER-DESIGN.md` §6, "Full
//! history from where, opening balances before".

use chrono::NaiveDate;
use ledger_core::Money;

use crate::chart;
use crate::types::{
    AccountId, ContactRef, DocKind, DocLine, JournalEntry, JournalLine, LedgerDocument, LineKind,
};

use super::accounts::AccountMapping;
use super::driver::ImportError;

/// One row of a QBO `TrialBalance` report as of a given date — signed,
/// debit-positive.
///
/// `crate::report` is being built in parallel and will define its own
/// `QboTbRow` for the same purpose (the nightly §7 diff, rather than the §6
/// opening entry); a later commit unifies the two rather than one importing
/// from the other while both are still in flight.
#[derive(Clone, Debug, PartialEq)]
pub struct QboTbRow {
    pub qbo_account_id: String,
    pub balance: Money,
}

/// Whether a replayed year's trial balance agreed with QBO's own, on the §7
/// tolerance rules — the boundary walk only needs the verdict, not the diff
/// itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TbAgreement {
    pub agrees: bool,
}

/// One `Journal`-kind document dated `as_of`, one line per non-zero QBO trial
/// balance row mapped onto the §2 chart, balanced by 3950 opening balance
/// equity as the plug. Flagged (§4: manual and imported journal entries carry
/// `is_flagged`).
pub fn opening_balance_entry(
    company: &crate::types::CompanyId,
    as_of: NaiveDate,
    qbo_tb: &[QboTbRow],
    mapping: &AccountMapping,
) -> Result<(LedgerDocument, JournalEntry), ImportError> {
    let mut rows: Vec<&QboTbRow> = qbo_tb.iter().filter(|row| !row.balance.is_zero()).collect();
    rows.sort_by(|a, b| a.qbo_account_id.cmp(&b.qbo_account_id));

    let mut journal_lines = Vec::with_capacity(rows.len() + 1);
    let mut doc_lines = Vec::with_capacity(rows.len());
    let mut debit_total = Money::ZERO;
    let mut credit_total = Money::ZERO;
    let mut line_no: i64 = 1;

    for row in rows {
        let mapped = mapping
            .by_source_ref(&row.qbo_account_id)
            .ok_or_else(|| ImportError::UnmappedAccount(row.qbo_account_id.clone()))?;

        let is_debit = !row.balance.is_negative();
        let amount = if is_debit {
            row.balance
        } else {
            row.balance.checked_neg()?
        };

        journal_lines.push(if is_debit {
            debit_total = debit_total.checked_add(amount)?;
            JournalLine::debit(line_no, mapped.ledger_number.clone(), amount)
        } else {
            credit_total = credit_total.checked_add(amount)?;
            JournalLine::credit(line_no, mapped.ledger_number.clone(), amount)
        });

        doc_lines.push(DocLine {
            line_no,
            kind: LineKind::Account,
            amount,
            class: None,
            item_id: None,
            account: Some(mapped.ledger_number.clone()),
            is_taxable: false,
            qty: None,
            unit_cost: None,
            description: Some(format!("Opening balance: {}", mapped.name)),
            posting: None,
            entity: None::<ContactRef>,
        });
        line_no += 1;
    }

    let plug_account = AccountId(chart::OPENING_BALANCE_EQUITY.to_string());
    let diff = debit_total.checked_sub(credit_total)?;
    if !diff.is_zero() {
        if diff.is_negative() {
            let amount = diff.checked_neg()?;
            journal_lines.push(JournalLine::debit(line_no, plug_account.clone(), amount));
            doc_lines.push(DocLine {
                line_no,
                kind: LineKind::Account,
                amount,
                class: None,
                item_id: None,
                account: Some(plug_account),
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: Some("Opening balance equity, plug".to_string()),
                posting: None,
                entity: None,
            });
        } else {
            journal_lines.push(JournalLine::credit(line_no, plug_account.clone(), diff));
            doc_lines.push(DocLine {
                line_no,
                kind: LineKind::Account,
                amount: diff,
                class: None,
                item_id: None,
                account: Some(plug_account),
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: Some("Opening balance equity, plug".to_string()),
                posting: None,
                entity: None,
            });
        }
    }

    let document_id = format!("opening-balance:{}:{}", company.0, as_of);
    let memo = Some(format!(
        "Opening balance as of {as_of} (LEDGER-DESIGN.md §6)"
    ));

    let document = LedgerDocument {
        document_id: document_id.clone(),
        kind: DocKind::JournalEntry,
        number: None,
        txn_date: as_of,
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
        memo: memo.clone(),
    };

    let entry = JournalEntry {
        entry_date: as_of,
        memo,
        source_type: DocKind::JournalEntry,
        source_id: document_id,
        source_version: 1,
        reversal_of: None,
        is_flagged: true,
        lines: journal_lines,
    };

    Ok((document, entry))
}

/// The §6 boundary walk: starting from the most recent year and stepping
/// backwards, the boundary is the earliest year such that it and every later
/// year agree with QBO. The first disagreement (walking backwards) stops the
/// walk — everything before it is not examined, because it will become the
/// single opening balance entry rather than replayed history.
pub fn boundary_year(years: &[(i32, TbAgreement)]) -> Option<i32> {
    let mut ordered: Vec<&(i32, TbAgreement)> = years.iter().collect();
    ordered.sort_by_key(|(year, _)| *year);

    let mut boundary = None;
    for (year, agreement) in ordered.iter().rev() {
        if agreement.agrees {
            boundary = Some(*year);
        } else {
            break;
        }
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::accounts::MappedAccount;
    use crate::types::CompanyId;

    fn mapping() -> AccountMapping {
        AccountMapping {
            mapped: vec![
                MappedAccount {
                    source_ref: "35".into(),
                    ledger_number: AccountId(chart::CHECKING.into()),
                    name: "Checking".into(),
                    classification: chart::Classification::Asset,
                    needs_mapping: false,
                },
                MappedAccount {
                    source_ref: "79".into(),
                    ledger_number: AccountId(chart::ACCOUNTS_RECEIVABLE.into()),
                    name: "Accounts Receivable".into(),
                    classification: chart::Classification::Asset,
                    needs_mapping: false,
                },
                MappedAccount {
                    source_ref: "33".into(),
                    ledger_number: AccountId(chart::ACCOUNTS_PAYABLE.into()),
                    name: "Accounts Payable".into(),
                    classification: chart::Classification::Liability,
                    needs_mapping: false,
                },
            ],
        }
    }

    #[test]
    fn opening_balance_entry_balances_with_the_3950_plug() {
        let rows = vec![
            QboTbRow {
                qbo_account_id: "35".into(),
                balance: Money::from_minor(1_000_000),
            },
            QboTbRow {
                qbo_account_id: "79".into(),
                balance: Money::from_minor(250_000),
            },
            QboTbRow {
                qbo_account_id: "33".into(),
                balance: Money::from_minor(-400_000),
            },
        ];
        let company = CompanyId("aquamentor".into());
        let as_of = NaiveDate::from_ymd_opt(2023, 12, 31).unwrap();

        let (document, entry) = opening_balance_entry(&company, as_of, &rows, &mapping()).unwrap();

        assert!(entry.is_balanced());
        assert_eq!(entry.entry_date, as_of);
        assert!(entry.is_flagged);
        assert_eq!(document.txn_date, as_of);
        assert_eq!(document.kind, DocKind::JournalEntry);

        let plug_line = entry
            .lines
            .iter()
            .find(|line| line.account == AccountId(chart::OPENING_BALANCE_EQUITY.into()))
            .expect("a non-zero net should produce a 3950 plug line");
        // 1,000,000 + 250,000 (Dr) - 400,000 (Cr) = 850,000 net debit, so the
        // plug credits 3950 for 850,000 to bring the entry to zero.
        assert_eq!(plug_line.credit, Money::from_minor(850_000));
        assert_eq!(plug_line.debit, Money::ZERO);
    }

    #[test]
    fn zero_balance_rows_are_skipped() {
        let rows = vec![QboTbRow {
            qbo_account_id: "35".into(),
            balance: Money::ZERO,
        }];
        let company = CompanyId("aquamentor".into());
        let as_of = NaiveDate::from_ymd_opt(2023, 12, 31).unwrap();
        let (document, entry) = opening_balance_entry(&company, as_of, &rows, &mapping()).unwrap();
        assert!(document.lines.is_empty());
        assert!(entry.lines.is_empty());
        assert!(entry.is_balanced() || entry.lines.is_empty());
    }

    #[test]
    fn an_unmapped_qbo_account_is_an_error_not_a_silent_skip() {
        let rows = vec![QboTbRow {
            qbo_account_id: "999".into(),
            balance: Money::from_minor(100),
        }];
        let company = CompanyId("aquamentor".into());
        let as_of = NaiveDate::from_ymd_opt(2023, 12, 31).unwrap();
        let err = opening_balance_entry(&company, as_of, &rows, &mapping()).unwrap_err();
        assert!(matches!(err, ImportError::UnmappedAccount(id) if id == "999"));
    }

    #[test]
    fn boundary_year_is_the_earliest_year_that_agrees_through_the_present() {
        let years = vec![
            (2020, TbAgreement { agrees: false }),
            (2021, TbAgreement { agrees: false }),
            (2022, TbAgreement { agrees: true }),
            (2023, TbAgreement { agrees: true }),
            (2024, TbAgreement { agrees: true }),
        ];
        assert_eq!(boundary_year(&years), Some(2022));
    }

    #[test]
    fn boundary_year_stops_at_the_first_disagreement_walking_backwards() {
        let years = vec![
            (2021, TbAgreement { agrees: true }),
            (2022, TbAgreement { agrees: false }),
            (2023, TbAgreement { agrees: true }),
        ];
        // 2023 agrees but 2022 does not, so the boundary cannot reach back to
        // 2021 even though 2021 itself agreed.
        assert_eq!(boundary_year(&years), Some(2023));
    }

    #[test]
    fn boundary_year_is_none_when_the_most_recent_year_disagrees() {
        let years = vec![(2023, TbAgreement { agrees: false })];
        assert_eq!(boundary_year(&years), None);
    }

    #[test]
    fn boundary_year_of_an_empty_history_is_none() {
        assert_eq!(boundary_year(&[]), None);
    }
}
