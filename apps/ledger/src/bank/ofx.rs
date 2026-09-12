//! OFX/QFX (SGML) bank and card export parsing. `LEDGER-DESIGN.md` §10.
//!
//! OFX is not XML: a tag commonly has no closing counterpart at all
//! (`<DTPOSTED>20260805120000` simply ends at the next tag or line break).
//! [`tag_value`] reads exactly that shape — from just after the opening tag
//! to the next `<` or line break, whichever comes first — so a well-formed
//! `<TAG>value</TAG>` and a bare `<TAG>value` both parse the same way. This
//! is deliberately not a general SGML or XML parser; it reads the one
//! structure this crate needs (`<STMTTRN>` blocks, plus `<LEDGERBAL>` and the
//! statement's `<DTSTART>`/`<DTEND>`) and nothing else.

use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use ledger_core::{round_money, Money, RoundingPolicy};

use super::{ParseError, ParsedLine};

/// Every `<STMTTRN>...</STMTTRN>` transaction in `text`. Duplicate `FITID`s
/// across transactions are preserved as-is — this parser never dedupes; that
/// is [`crate::store::Ledger::import_statement`]'s job, via the `(account,
/// external_id)` unique index (§10).
pub fn parse_ofx(text: &str) -> Result<Vec<ParsedLine>, ParseError> {
    let mut lines = Vec::new();

    for (index, block) in extract_blocks(text, "<STMTTRN>", "</STMTTRN>")
        .into_iter()
        .enumerate()
    {
        let row = index + 1;

        let dtposted = tag_value(block, "DTPOSTED").ok_or_else(|| ParseError::Malformed {
            row,
            reason: "STMTTRN missing DTPOSTED".to_string(),
        })?;
        let posted_on = parse_ofx_date(&dtposted, row)?;

        let trnamt = tag_value(block, "TRNAMT").ok_or_else(|| ParseError::Malformed {
            row,
            reason: "STMTTRN missing TRNAMT".to_string(),
        })?;
        let amount = parse_trnamt(&trnamt, row)?;

        let description = tag_value(block, "NAME")
            .or_else(|| tag_value(block, "MEMO"))
            .unwrap_or_default();
        let external_id = tag_value(block, "FITID");

        lines.push(ParsedLine {
            posted_on,
            amount,
            description,
            external_id,
        });
    }

    Ok(lines)
}

/// `<LEDGERBAL><BALAMT>`, the statement's own stated ending balance, when the
/// file carries one. Separate from [`parse_ofx`] because that function's
/// return is transactions only; a caller wiring this into `ledger bank
/// import` can use this to pre-fill `--closing` instead of typing it in.
pub fn parse_ofx_ledger_balance(text: &str) -> Option<Money> {
    let block = extract_blocks(text, "<LEDGERBAL>", "</LEDGERBAL>")
        .into_iter()
        .next()?;
    let raw = tag_value(block, "BALAMT")?;
    let decimal = Decimal::from_str(raw.trim()).ok()?;
    round_money(decimal, RoundingPolicy::MirroredAmount).ok()
}

/// The statement's own `<DTSTART>`/`<DTEND>`, when both are present.
pub fn parse_ofx_statement_period(text: &str) -> Option<(NaiveDate, NaiveDate)> {
    let start = parse_ofx_date(&tag_value(text, "DTSTART")?, 0).ok()?;
    let end = parse_ofx_date(&tag_value(text, "DTEND")?, 0).ok()?;
    Some((start, end))
}

fn parse_trnamt(raw: &str, row: usize) -> Result<Money, ParseError> {
    let decimal = Decimal::from_str(raw.trim()).map_err(|_| ParseError::Malformed {
        row,
        reason: format!("invalid TRNAMT {raw:?}"),
    })?;
    if decimal.scale() > 2 {
        return Err(ParseError::Precision {
            row,
            value: raw.to_string(),
        });
    }
    Ok(round_money(decimal, RoundingPolicy::MirroredAmount)?)
}

/// OFX dates are `YYYYMMDD[hhmmss[.xxx[gmt offset[:tz name]]]]`; only the
/// first eight digits (the calendar date) matter here.
fn parse_ofx_date(raw: &str, row: usize) -> Result<NaiveDate, ParseError> {
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 8 {
        return Err(ParseError::BadDate {
            row,
            value: raw.to_string(),
            format: "YYYYMMDD",
        });
    }
    NaiveDate::parse_from_str(&digits[..8], "%Y%m%d").map_err(|_| ParseError::BadDate {
        row,
        value: raw.to_string(),
        format: "YYYYMMDD",
    })
}

/// Every `open_tag ... close_tag` slice of `text`. OFX/QFX does not nest
/// `<STMTTRN>` (or `<LEDGERBAL>`) blocks within themselves, so a simple
/// find-the-next-close-tag walk is enough — no stack needed.
fn extract_blocks<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(open_tag) {
        let after_open = &rest[start + open_tag.len()..];
        let Some(end) = after_open.find(close_tag) else {
            break;
        };
        blocks.push(&after_open[..end]);
        rest = &after_open[end + close_tag.len()..];
    }
    blocks
}

/// The value of `<TAG>` inside `text`: from just after the opening tag to
/// the next `<` or line break, whichever comes first. Tolerant of a tag with
/// no closing counterpart, which real OFX/QFX files are full of.
fn tag_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let rest = &text[start..];
    let next_angle = rest.find('<').unwrap_or(rest.len());
    let next_newline = rest.find(['\n', '\r']).unwrap_or(rest.len());
    let value = rest[..next_angle.min(next_newline)].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three transactions (the third deliberately reusing the second's
    /// FITID, to show the parser preserves rather than dedupes), a
    /// `LEDGERBAL`, and tags left unclosed exactly as real bank exports
    /// leave them.
    const FIXTURE: &str = "\
OFXHEADER:100
DATA:OFXSGML
VERSION:102

<OFX>
<BANKMSGSRSV1>
<STMTTRNRS>
<STMTRS>
<CURDEF>USD
<BANKTRANLIST>
<DTSTART>20260801000000
<DTEND>20260831235959
<STMTTRN>
<TRNTYPE>DEBIT
<DTPOSTED>20260803120000[-5:EST]
<TRNAMT>-24.99
<FITID>2026080300001
<NAME>UPS 1Z999AA10123456784
</STMTTRN>
<STMTTRN>
<TRNTYPE>CREDIT
<DTPOSTED>20260805120000
<TRNAMT>100.00
<FITID>2026080500002
<NAME>SHOPIFY PAYOUT
</STMTTRN>
<STMTTRN>
<TRNTYPE>DEBIT
<DTPOSTED>20260807120000
<TRNAMT>-9.99
<FITID>2026080500002
<MEMO>DUPLICATE FITID CASE
</STMTTRN>
</BANKTRANLIST>
<LEDGERBAL>
<BALAMT>4523.10
<DTASOF>20260831235959
</LEDGERBAL>
</STMTRS>
</STMTTRNRS>
</BANKMSGSRSV1>
</OFX>
";

    #[test]
    fn reads_three_transactions_tolerating_unclosed_tags() {
        let lines = parse_ofx(FIXTURE).expect("parses");
        assert_eq!(lines.len(), 3);

        assert_eq!(
            lines[0].posted_on,
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap()
        );
        assert_eq!(lines[0].amount, Money::from_minor(-2499));
        assert_eq!(lines[0].description, "UPS 1Z999AA10123456784");
        assert_eq!(lines[0].external_id.as_deref(), Some("2026080300001"));

        assert_eq!(lines[1].amount, Money::from_minor(10000));
        assert_eq!(lines[1].description, "SHOPIFY PAYOUT");
    }

    #[test]
    fn a_duplicate_fitid_is_preserved_not_deduped() {
        let lines = parse_ofx(FIXTURE).expect("parses");
        assert_eq!(lines[1].external_id, lines[2].external_id);
        assert_eq!(lines[2].description, "DUPLICATE FITID CASE");
    }

    #[test]
    fn reads_the_ledger_balance_and_statement_period() {
        let balance = parse_ofx_ledger_balance(FIXTURE).expect("balance present");
        assert_eq!(balance, Money::from_minor(452310));

        let (start, end) = parse_ofx_statement_period(FIXTURE).expect("period present");
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 8, 31).unwrap());
    }

    #[test]
    fn a_stmttrn_missing_trnamt_is_malformed() {
        let text = "<STMTTRN>\n<DTPOSTED>20260101\n<FITID>1\n</STMTTRN>";
        let err = parse_ofx(text).unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }), "{err:?}");
    }

    #[test]
    fn an_amount_with_more_than_two_decimals_is_a_precision_error() {
        let text = "<STMTTRN>\n<DTPOSTED>20260101\n<TRNAMT>-1.234\n<FITID>1\n</STMTTRN>";
        let err = parse_ofx(text).unwrap_err();
        assert!(matches!(err, ParseError::Precision { .. }), "{err:?}");
    }
}
