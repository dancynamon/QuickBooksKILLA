//! The Authorize.net Merchant Interface "Settled Batch" / "Transaction
//! Detail" CSV export: Reports -> Transaction Details -> Download to file,
//! comma-separated with a header row.
//!
//! Header matching is case-insensitive and tolerant of the handful of names
//! Authorize.net has shipped for the same column over the years ([`ALIASES`]
//! below); an unrecognised extra column is ignored, and a column this parser
//! actually needs that the file does not carry is
//! [`ParseError::MissingColumn`] rather than a panic or a silent default
//! (D9). This module only reads the file into [`TransactionRow`]s — grouping
//! rows into batches is [`crate::authnet::group_batches`].

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use thiserror::Error;

use ledger_core::{round_money, Money, RoundingPolicy};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// A column this parser needs is not in the file under any known alias.
    #[error("missing required column {0:?}")]
    MissingColumn(&'static str),
    #[error("row {row}: {reason}")]
    Malformed { row: usize, reason: String },
    #[error("row {row}: invalid date {value:?}, expected format {format}")]
    BadDate {
        row: usize,
        value: String,
        format: &'static str,
    },
    /// D9: an amount carrying more than two decimal places is quarantined,
    /// never rounded silently.
    #[error("row {row}: amount {value:?} carries more than two decimal places")]
    Precision { row: usize, value: String },
    #[error(transparent)]
    Money(#[from] ledger_core::MoneyError),
}

/// The `Transaction Status` column. Only `SettledSuccessfully` and
/// `RefundSettledSuccessfully` rows carry money that
/// [`crate::authnet::group_batches`] turns into a batch; `Voided` and
/// `Declined` rows still parse (so a genuinely malformed one still fails
/// loudly) and are then dropped, since nothing settled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    SettledSuccessfully,
    RefundSettledSuccessfully,
    Voided,
    Declined,
}

/// One row of the export. Columns this crate has no use for (address,
/// AVS/CVV response, and the rest of the ~30 columns a real export carries)
/// are read past and discarded — modelling every column QBO's own export
/// never uses would just be more surface to keep in sync with Authorize.net's
/// own changes to the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionRow {
    pub transaction_id: String,
    pub status: TransactionStatus,
    /// When the transaction was authorized/captured, if the file carries the
    /// column. Not used by [`crate::authnet::group_batches`] — batching goes
    /// by `settlement_date` — but real enough to be worth keeping.
    pub submit_date: Option<NaiveDate>,
    /// The date this row's batch settled. Every row of one batch is expected
    /// to carry the same date, since that is what "batch" means.
    pub settlement_date: NaiveDate,
    pub batch_id: String,
    /// `Settlement Amount` when the file has it, else `Total Amount`/`Amount`
    /// — see [`ALIASES`]. Always non-negative: a refund's sign is carried by
    /// `status`, not by this field, so a file that reports refunds as
    /// negative amounts and one that reports them as positive both read the
    /// same.
    pub amount: Money,
    pub invoice_number: Option<String>,
    pub customer_id: Option<String>,
    pub card_type: Option<String>,
    /// `auth_capture`, `credit`, `void`, and whatever else Authorize.net has
    /// used; kept as the file's own word rather than an enum this crate would
    /// have to keep exhaustive against a report it does not control (D11's
    /// reasoning, applied here).
    pub transaction_type: Option<String>,
}

/// Canonical column key to the header names Authorize.net has shipped for
/// it. Matching is case-insensitive; the first alias to match a header cell
/// wins ties are not expected in practice.
const ALIASES: &[(&str, &[&str])] = &[
    (
        "transaction_id",
        &["Transaction ID", "Trans ID", "Transaction Id"],
    ),
    ("transaction_status", &["Transaction Status", "Status"]),
    (
        "submit_time",
        &["Submit Date/Time", "Submit Date", "Submit Time"],
    ),
    (
        "settlement_time",
        &[
            "Settlement Date/Time",
            "Settlement Date",
            "Batch Settlement Date/Time",
        ],
    ),
    ("batch_id", &["Batch ID", "Settlement Batch ID", "Batch Id"]),
    ("settlement_amount", &["Settlement Amount"]),
    ("total_amount", &["Total Amount", "Amount"]),
    (
        "invoice_number",
        &["Invoice Number", "Invoice No", "Invoice #", "Invoice"],
    ),
    ("customer_id", &["Customer ID", "Customer Id", "Cust ID"]),
    ("card_type", &["Card Type", "Payment Method", "Method"]),
    ("transaction_type", &["Transaction Type", "Trans Type"]),
];

fn header_index(header_line: &str) -> HashMap<&'static str, usize> {
    let fields = split_csv_line(header_line);
    let mut map = HashMap::new();
    for (idx, raw) in fields.iter().enumerate() {
        let trimmed = raw.trim();
        for (key, names) in ALIASES {
            if names.iter().any(|name| name.eq_ignore_ascii_case(trimmed)) {
                map.entry(*key).or_insert(idx);
            }
        }
    }
    map
}

fn require_column(
    columns: &HashMap<&'static str, usize>,
    key: &'static str,
    display: &'static str,
) -> Result<usize, ParseError> {
    columns
        .get(key)
        .copied()
        .ok_or(ParseError::MissingColumn(display))
}

const DATE_FORMATS: &[&str] = &[
    "%m/%d/%Y %I:%M:%S %p",
    "%m/%d/%Y %H:%M:%S",
    "%m/%d/%Y",
    "%Y-%m-%d",
];

fn parse_date_cell(raw: &str, row: usize) -> Result<NaiveDate, ParseError> {
    let trimmed = raw.trim();
    for format in DATE_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Ok(dt.date());
        }
        if let Ok(date) = NaiveDate::parse_from_str(trimmed, format) {
            return Ok(date);
        }
    }
    Err(ParseError::BadDate {
        row,
        value: trimmed.to_string(),
        format: DATE_FORMATS[0],
    })
}

fn parse_status(raw: &str, row: usize) -> Result<TransactionStatus, ParseError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "settled successfully" => Ok(TransactionStatus::SettledSuccessfully),
        "refund settled successfully" => Ok(TransactionStatus::RefundSettledSuccessfully),
        "voided" => Ok(TransactionStatus::Voided),
        "declined" => Ok(TransactionStatus::Declined),
        other => Err(ParseError::Malformed {
            row,
            reason: format!("unrecognised transaction status {other:?}"),
        }),
    }
}

/// Parses the amount, then takes its absolute value: a refund's direction is
/// `status`, not the sign on this column, and Authorize.net exports have
/// shown both conventions.
fn parse_amount(raw: &str, row: usize) -> Result<Money, ParseError> {
    let trimmed = raw.trim();
    let cleaned = trimmed.replace(['$', ','], "");
    let decimal = Decimal::from_str(cleaned.trim()).map_err(|_| ParseError::Malformed {
        row,
        reason: format!("invalid amount {trimmed:?}"),
    })?;
    if decimal.scale() > 2 {
        return Err(ParseError::Precision {
            row,
            value: trimmed.to_string(),
        });
    }
    let money = round_money(decimal.abs(), RoundingPolicy::MirroredAmount)?;
    Ok(money)
}

/// Reads every row of the export into [`TransactionRow`]s. Grouping into
/// batches, dropping `Voided`/`Declined` rows, and everything downstream is
/// [`crate::authnet::group_batches`] — this function only reads the file.
pub fn parse_transactions(text: &str) -> Result<Vec<TransactionRow>, ParseError> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("");
    let columns = header_index(header);

    let transaction_id_col = require_column(&columns, "transaction_id", "Transaction ID")?;
    let status_col = require_column(&columns, "transaction_status", "Transaction Status")?;
    let settlement_time_col = require_column(&columns, "settlement_time", "Settlement Date/Time")?;
    let batch_id_col = require_column(&columns, "batch_id", "Batch ID")?;
    let amount_col = columns
        .get("settlement_amount")
        .or_else(|| columns.get("total_amount"))
        .copied()
        .ok_or(ParseError::MissingColumn(
            "Settlement Amount or Total Amount",
        ))?;
    let submit_time_col = columns.get("submit_time").copied();
    let invoice_col = columns.get("invoice_number").copied();
    let customer_col = columns.get("customer_id").copied();
    let card_type_col = columns.get("card_type").copied();
    let transaction_type_col = columns.get("transaction_type").copied();

    let mut rows = Vec::new();
    for (index, raw_line) in lines.enumerate() {
        let row = index + 2; // the header occupies row 1.
        if raw_line.trim().is_empty() {
            continue;
        }
        let fields = split_csv_line(raw_line);
        let get = |col: usize| -> Result<&str, ParseError> {
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
        };
        let optional = |col: Option<usize>| -> Result<Option<String>, ParseError> {
            match col {
                Some(col) => {
                    let raw = get(col)?.trim();
                    Ok(if raw.is_empty() {
                        None
                    } else {
                        Some(raw.to_string())
                    })
                }
                None => Ok(None),
            }
        };

        let transaction_id = get(transaction_id_col)?.trim().to_string();
        let status = parse_status(get(status_col)?, row)?;
        let settlement_date = parse_date_cell(get(settlement_time_col)?, row)?;
        let batch_id = get(batch_id_col)?.trim().to_string();
        let amount = parse_amount(get(amount_col)?, row)?;
        let submit_date = submit_time_col
            .map(get)
            .transpose()?
            .map(|raw| parse_date_cell(raw, row))
            .transpose()?;

        rows.push(TransactionRow {
            transaction_id,
            status,
            submit_date,
            settlement_date,
            batch_id,
            amount,
            invoice_number: optional(invoice_col)?,
            customer_id: optional(customer_col)?,
            card_type: optional(card_type_col)?,
            transaction_type: optional(transaction_type_col)?,
        });
    }
    Ok(rows)
}

/// A minimal quoted-field CSV splitter, the same shape as
/// `crate::bank::csv`'s: handles a comma inside `"..."` and a doubled `""` as
/// an escaped quote. Kept as its own copy rather than reaching into that
/// private helper — this module reads a different export with different
/// columns and no reason to share more than the algorithm.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic export: two settled sales, one refund, one voided
    /// authorization that never settled. Fake names and a fake last-four, as
    /// a real export would carry a card type and a masked card number.
    const SAMPLE: &str = "\
Transaction ID,Transaction Status,Submit Date/Time,Settlement Date/Time,Batch ID,Settlement Amount,Invoice Number,Customer ID,Card Type,Transaction Type
40001,Settled Successfully,9/10/2026 9:12:03 AM,9/10/2026 11:00:00 PM,900100,100.00,1001,cust-priya-shah,Visa xxxx4242,auth_capture
40002,Settled Successfully,9/10/2026 10:45:51 AM,9/10/2026 11:00:00 PM,900100,150.00,1002,cust-marcus-liu,Mastercard xxxx5588,auth_capture
40003,Refund Settled Successfully,9/10/2026 1:30:00 PM,9/10/2026 11:00:00 PM,900100,-40.00,1001,cust-priya-shah,Visa xxxx4242,credit
40004,Voided,9/10/2026 2:00:00 PM,9/10/2026 11:00:00 PM,900100,25.00,,cust-dana-osei,Visa xxxx7711,void
";

    #[test]
    fn parses_the_sample_export_and_normalises_refund_sign() {
        let rows = parse_transactions(SAMPLE).expect("parses");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].transaction_id, "40001");
        assert_eq!(rows[0].status, TransactionStatus::SettledSuccessfully);
        assert_eq!(rows[0].batch_id, "900100");
        assert_eq!(rows[0].amount, Money::from_minor(10_000));
        assert_eq!(rows[0].invoice_number.as_deref(), Some("1001"));
        assert_eq!(rows[0].customer_id.as_deref(), Some("cust-priya-shah"));
        assert_eq!(
            rows[0].settlement_date,
            NaiveDate::from_ymd_opt(2026, 9, 10).unwrap()
        );

        // The refund's amount is normalised to positive; direction is status.
        assert_eq!(rows[2].status, TransactionStatus::RefundSettledSuccessfully);
        assert_eq!(rows[2].amount, Money::from_minor(4_000));

        // The voided row still parses (row 4: no invoice number).
        assert_eq!(rows[3].status, TransactionStatus::Voided);
        assert_eq!(rows[3].invoice_number, None);
    }

    #[test]
    fn header_aliases_are_case_insensitive_and_order_independent() {
        let text = "\
settlement batch id,amount,trans id,status,settlement date\n\
900200,275.50,50001,settled successfully,9/11/2026\n";
        let rows = parse_transactions(text).expect("parses with aliased headers");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].batch_id, "900200");
        assert_eq!(rows[0].amount, Money::from_minor(27_550));
        assert_eq!(rows[0].transaction_id, "50001");
        assert_eq!(rows[0].invoice_number, None);
    }

    #[test]
    fn a_missing_required_column_is_reported_by_name() {
        let text = "Transaction ID,Transaction Status,Settlement Date/Time,Settlement Amount\n\
                    40001,Settled Successfully,9/10/2026,100.00\n";
        let err = parse_transactions(text).unwrap_err();
        assert_eq!(err, ParseError::MissingColumn("Batch ID"));
    }

    #[test]
    fn an_amount_with_three_decimal_places_is_rejected() {
        let text = "\
Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount\n\
40001,Settled Successfully,9/10/2026,900100,19.995\n";
        let err = parse_transactions(text).unwrap_err();
        assert!(matches!(err, ParseError::Precision { .. }), "{err:?}");
    }

    #[test]
    fn an_unrecognised_status_is_malformed_not_silently_dropped() {
        let text = "\
Transaction ID,Transaction Status,Settlement Date/Time,Batch ID,Settlement Amount\n\
40001,Pending Settlement,9/10/2026,900100,100.00\n";
        let err = parse_transactions(text).unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }), "{err:?}");
    }
}
