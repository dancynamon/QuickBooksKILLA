//! CSV bank/card export parsing. `LEDGER-DESIGN.md` §10.
//!
//! A [`CsvProfile`] describes one export shape (which column is which); this
//! module never guesses a layout. Where a file carries no id of its own for
//! a transaction (true of every real Chase CSV export this crate has seen),
//! [`parse_csv`] synthesizes a stable one from the row's own content — date,
//! amount, a hash of the description, and how many identical rows came
//! before it in this same file — so that parsing the same file twice
//! produces the same ids in the same order and [`crate::store::Ledger`]'s
//! `(account, external_id)` dedupe (§10) still works even without a bank-
//! supplied id. The parser itself never dedupes; that is the store's job.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use ledger_core::{round_money, Money, RoundingPolicy};

use super::{ParseError, ParsedLine};

/// Where the amount lives in one row: a single signed column, or a separate
/// debit and credit column (money out is positive in the debit column, money
/// in is positive in the credit column; at most one of the two is non-empty
/// per row in practice, but both are summed regardless).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AmountCols {
    Single(usize),
    DebitCredit(usize, usize),
}

/// One CSV export layout: which columns are which, and how to read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvProfile {
    pub date_col: usize,
    pub description_col: usize,
    pub amount_col: AmountCols,
    pub date_format: &'static str,
    /// `None` when the file carries no id of its own for a row — the common
    /// case — in which case [`parse_csv`] synthesizes one.
    pub external_id_col: Option<usize>,
    pub skip_header: bool,
    /// Whether `(12.34)` means `-12.34` (common in accounting-style
    /// exports). A plain leading `-` is always read as negative regardless
    /// of this flag.
    pub decimal_negative_in_parens: bool,
}

impl CsvProfile {
    /// Chase's card and checking CSV export: `Transaction Date, Post Date,
    /// Description, Category, Type, Amount, Memo`. `posted_on` reads Post
    /// Date rather than Transaction Date, because Post Date is the date that
    /// actually appears on Chase's own statement, which is what the §10
    /// match window compares against.
    pub fn chase() -> Self {
        CsvProfile {
            date_col: 1,
            description_col: 2,
            amount_col: AmountCols::Single(5),
            date_format: "%m/%d/%Y",
            external_id_col: None,
            skip_header: true,
            decimal_negative_in_parens: true,
        }
    }

    /// A plain `date,description,amount,external_id` export with ISO dates —
    /// the shape "the bank" (as opposed to Chase specifically) is assumed to
    /// produce, per `LEDGER-DESIGN.md` §10's "the files the bank and Chase
    /// already generate".
    pub fn generic() -> Self {
        CsvProfile {
            date_col: 0,
            description_col: 1,
            amount_col: AmountCols::Single(2),
            date_format: "%Y-%m-%d",
            external_id_col: Some(3),
            skip_header: true,
            decimal_negative_in_parens: false,
        }
    }
}

pub fn parse_csv(text: &str, profile: &CsvProfile) -> Result<Vec<ParsedLine>, ParseError> {
    let mut lines = Vec::new();
    let mut occurrence: HashMap<(NaiveDate, i64, String), usize> = HashMap::new();

    for (index, raw_line) in text.lines().enumerate() {
        let row = index + 1;
        if profile.skip_header && index == 0 {
            continue;
        }
        if raw_line.trim().is_empty() {
            continue;
        }

        let fields = split_csv_line(raw_line);

        let date_raw = field(&fields, profile.date_col, row)?.trim().to_string();
        let posted_on =
            NaiveDate::parse_from_str(&date_raw, profile.date_format).map_err(|_| {
                ParseError::BadDate {
                    row,
                    value: date_raw,
                    format: profile.date_format,
                }
            })?;

        let description = field(&fields, profile.description_col, row)?
            .trim()
            .to_string();
        let amount = parse_amount(
            &fields,
            &profile.amount_col,
            profile.decimal_negative_in_parens,
            row,
        )?;

        let external_id = match profile.external_id_col {
            Some(col) => {
                let raw = field(&fields, col, row)?.trim();
                if raw.is_empty() {
                    None
                } else {
                    Some(raw.to_string())
                }
            }
            None => {
                let key = (posted_on, amount.minor(), description.clone());
                let seen_before = *occurrence.get(&key).unwrap_or(&0);
                occurrence.insert(key, seen_before + 1);
                Some(format!(
                    "csv:{posted_on}:{}:{:016x}:{seen_before}",
                    amount.minor(),
                    hash_str(&description)
                ))
            }
        };

        lines.push(ParsedLine {
            posted_on,
            amount,
            description,
            external_id,
        });
    }
    Ok(lines)
}

fn field(fields: &[String], col: usize, row: usize) -> Result<&str, ParseError> {
    fields
        .get(col)
        .map(String::as_str)
        .ok_or_else(|| ParseError::Malformed {
            row,
            reason: format!(
                "expected at least {} columns, got {}",
                col + 1,
                fields.len()
            ),
        })
}

fn parse_amount(
    fields: &[String],
    amount_col: &AmountCols,
    parens_negative: bool,
    row: usize,
) -> Result<Money, ParseError> {
    let (decimal, raw) = match amount_col {
        AmountCols::Single(col) => {
            let raw = field(fields, *col, row)?;
            (parse_decimal(raw, parens_negative, row)?, raw.to_string())
        }
        AmountCols::DebitCredit(debit_col, credit_col) => {
            let debit_raw = field(fields, *debit_col, row)?;
            let credit_raw = field(fields, *credit_col, row)?;
            let debit = parse_optional_decimal(debit_raw, parens_negative, row)?;
            let credit = parse_optional_decimal(credit_raw, parens_negative, row)?;
            (
                credit - debit,
                format!("credit {credit_raw:?} debit {debit_raw:?}"),
            )
        }
    };
    if decimal.scale() > 2 {
        return Err(ParseError::Precision { row, value: raw });
    }
    Ok(round_money(decimal, RoundingPolicy::MirroredAmount)?)
}

fn parse_optional_decimal(
    raw: &str,
    parens_negative: bool,
    row: usize,
) -> Result<Decimal, ParseError> {
    if raw.trim().is_empty() {
        Ok(Decimal::ZERO)
    } else {
        parse_decimal(raw, parens_negative, row)
    }
}

fn parse_decimal(raw: &str, parens_negative: bool, row: usize) -> Result<Decimal, ParseError> {
    let trimmed = raw.trim();
    let mut cleaned = trimmed.replace(['$', ','], "");
    let mut negative = false;
    if parens_negative && cleaned.starts_with('(') && cleaned.ends_with(')') {
        negative = true;
        cleaned = cleaned[1..cleaned.len() - 1].to_string();
    }
    let mut value = Decimal::from_str(cleaned.trim()).map_err(|_| ParseError::Malformed {
        row,
        reason: format!("invalid amount {trimmed:?}"),
    })?;
    if negative {
        value = -value;
    }
    Ok(value)
}

/// A minimal quoted-field CSV splitter: handles a comma inside `"..."` and a
/// doubled `""` as an escaped quote. Not a general CSV reader — this crate
/// reads bank exports specifically, which are one row per line.
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

fn hash_str(value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHASE_HEADER: &str = "Transaction Date,Post Date,Description,Category,Type,Amount,Memo\n";

    #[test]
    fn chase_parses_a_parenthesized_negative_and_a_plain_positive() {
        let text = format!(
            "{CHASE_HEADER}\
             08/01/2026,08/03/2026,UPS 1Z999AA10123456784,Shipping,Sale,(24.99),\n\
             08/02/2026,08/04/2026,SHOPIFY PAYOUT,Payment,Payment,100.00,\n"
        );
        let lines = parse_csv(&text, &CsvProfile::chase()).expect("parses");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].posted_on,
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap()
        );
        assert_eq!(lines[0].amount, Money::from_minor(-2499));
        assert_eq!(lines[0].description, "UPS 1Z999AA10123456784");
        assert_eq!(lines[1].amount, Money::from_minor(10000));
    }

    #[test]
    fn chase_rejects_more_than_two_decimal_places() {
        let text =
            format!("{CHASE_HEADER}08/01/2026,08/03/2026,ODD AMOUNT,Shipping,Sale,12.345,\n");
        let err = parse_csv(&text, &CsvProfile::chase()).unwrap_err();
        assert!(matches!(err, ParseError::Precision { .. }), "{err:?}");
    }

    #[test]
    fn chase_synthesizes_stable_external_ids_across_repeated_parses() {
        let text = format!(
            "{CHASE_HEADER}\
             08/01/2026,08/03/2026,COFFEE SHOP,Food,Sale,-5.00,\n\
             08/01/2026,08/03/2026,COFFEE SHOP,Food,Sale,-5.00,\n"
        );
        let first = parse_csv(&text, &CsvProfile::chase()).expect("parses");
        let second = parse_csv(&text, &CsvProfile::chase()).expect("parses");
        assert_eq!(
            first, second,
            "re-parsing the same file must be deterministic"
        );
        // Two genuinely identical rows still get distinct ids, so both import.
        assert_ne!(first[0].external_id, first[1].external_id);
    }

    #[test]
    fn generic_reads_its_own_external_id_column_and_does_not_dedupe() {
        let text = "date,description,amount,external_id\n\
                    2026-08-01,Vendor A,-10.00,ext-1\n\
                    2026-08-02,Vendor B,-20.00,ext-1\n";
        let lines = parse_csv(text, &CsvProfile::generic()).expect("parses");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].external_id.as_deref(), Some("ext-1"));
        assert_eq!(lines[1].external_id.as_deref(), Some("ext-1"));
    }

    #[test]
    fn debit_credit_columns_net_to_a_signed_amount() {
        let text = "date,description,debit,credit,external_id\n\
                    2026-08-01,Vendor A,15.00,,ext-1\n\
                    2026-08-02,Customer B,,250.00,ext-2\n";
        let profile = CsvProfile {
            date_col: 0,
            description_col: 1,
            amount_col: AmountCols::DebitCredit(2, 3),
            date_format: "%Y-%m-%d",
            external_id_col: Some(4),
            skip_header: true,
            decimal_negative_in_parens: false,
        };
        let lines = parse_csv(text, &profile).expect("parses");
        assert_eq!(lines[0].amount, Money::from_minor(-1500));
        assert_eq!(lines[1].amount, Money::from_minor(25000));
    }

    #[test]
    fn quoted_descriptions_with_embedded_commas_are_one_field() {
        let text = "date,description,amount,external_id\n\
                     2026-08-01,\"UPS, GROUND\",-9.99,ext-1\n";
        let lines = parse_csv(text, &CsvProfile::generic()).expect("parses");
        assert_eq!(lines[0].description, "UPS, GROUND");
    }
}
