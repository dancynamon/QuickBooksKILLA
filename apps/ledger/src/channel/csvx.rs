//! A small RFC 4180 reader and the money/date cell parsers the channel
//! importers share. No `csv` crate: same no-new-dependencies discipline as
//! `authnet::csv`, but this one handles quoted newlines and doubled quotes,
//! which Amazon and Shopify exports both contain.

use std::collections::HashMap;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Weekday};
use ledger_core::Money;

use super::ChannelError;

/// Splits `text` into records of fields. `delimiter` is `,` or `\t`.
/// A leading BOM is dropped; `\r\n` and `\n` both end a record; a quoted
/// field may contain the delimiter, newlines and `""`. Blank records are
/// skipped.
pub fn parse_records(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_started = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                in_quotes = true;
                field_started = true;
            }
            c if c == delimiter => {
                record.push(std::mem::take(&mut field));
                field_started = false;
            }
            '\r' => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                field_started = false;
                if !(record.len() == 1 && record[0].is_empty()) {
                    records.push(std::mem::take(&mut record));
                } else {
                    record.clear();
                }
            }
            c => {
                field.push(c);
                field_started = true;
            }
        }
    }
    if field_started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        if !(record.len() == 1 && record[0].is_empty()) {
            records.push(record);
        }
    }
    records
}

/// A header row plus data rows, looked up by case-insensitive column name.
pub struct Table {
    index: HashMap<String, usize>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// Builds a table from `records`, taking the record at `header_at` as the
    /// header and everything after it as data.
    pub fn from_records(records: Vec<Vec<String>>, header_at: usize) -> Result<Table, ChannelError> {
        let mut iter = records.into_iter().skip(header_at);
        let header = iter
            .next()
            .ok_or_else(|| ChannelError::Parse("no header row".into()))?;
        let mut index = HashMap::new();
        for (i, name) in header.iter().enumerate() {
            index.entry(name.trim().to_lowercase()).or_insert(i);
        }
        Ok(Table {
            index,
            rows: iter.collect(),
        })
    }

    pub fn has(&self, column: &str) -> bool {
        self.index.contains_key(&column.to_lowercase())
    }

    /// The column's position, or `MissingColumn`.
    pub fn need(&self, column: &str) -> Result<usize, ChannelError> {
        self.index
            .get(&column.to_lowercase())
            .copied()
            .ok_or_else(|| ChannelError::MissingColumn(column.to_string()))
    }

    pub fn opt(&self, column: &str) -> Option<usize> {
        self.index.get(&column.to_lowercase()).copied()
    }
}

/// The trimmed cell at `idx`, `""` when the row is short.
pub fn cell(row: &[String], idx: usize) -> &str {
    row.get(idx).map_or("", |s| s.trim())
}

/// `"1,234.56"`, `"-3.5"`, `"(2.00)"`, `""` (zero) to [`Money`]. More than
/// two decimal places is an error (D9: never round silently).
pub fn parse_money(raw: &str) -> Result<Money, ChannelError> {
    let mut s = raw.trim().replace(',', "");
    s = s.replace('$', "");
    if s.is_empty() {
        return Ok(Money::ZERO);
    }
    let mut negative = false;
    if s.starts_with('(') && s.ends_with(')') {
        negative = true;
        s = s[1..s.len() - 1].to_string();
    }
    if let Some(rest) = s.strip_prefix('-') {
        negative = !negative;
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest.to_string();
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w.to_string(), f.to_string()),
        None => (s.clone(), String::new()),
    };
    let bad = || ChannelError::Parse(format!("unreadable amount {raw:?}"));
    if whole.is_empty() && frac.is_empty() {
        return Err(bad());
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    if frac.len() > 2 {
        // "12.500" is fine, "12.505" is not.
        if !frac[2..].chars().all(|c| c == '0') {
            return Err(ChannelError::Parse(format!(
                "amount {raw:?} has more than two decimal places"
            )));
        }
    }
    let frac2: String = format!("{frac:0<2}").chars().take(2).collect();
    let whole_n: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| bad())?
    };
    let frac_n: i64 = frac2.parse().map_err(|_| bad())?;
    let minor = whole_n
        .checked_mul(100)
        .and_then(|v| v.checked_add(frac_n))
        .ok_or_else(bad)?;
    Ok(Money::from_minor(if negative { -minor } else { minor }))
}

/// Integer quantity; blank is 0.
pub fn parse_qty(raw: &str) -> Result<i64, ChannelError> {
    let s = raw.trim().replace(',', "");
    if s.is_empty() {
        return Ok(0);
    }
    s.parse::<i64>()
        .map_err(|_| ChannelError::Parse(format!("unreadable quantity {raw:?}")))
}

pub fn parse_iso_date(raw: &str) -> Result<NaiveDate, ChannelError> {
    let head: String = raw.trim().chars().take(10).collect();
    NaiveDate::parse_from_str(&head, "%Y-%m-%d")
        .map_err(|_| ChannelError::Parse(format!("unreadable date {raw:?}")))
}

fn nth_sunday(year: i32, month: u32, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    let offset = (7 + Weekday::Sun.num_days_from_monday() as i64
        - first.weekday().num_days_from_monday() as i64)
        % 7;
    first + Duration::days(offset + 7 * (i64::from(n) - 1))
}

/// Converts a UTC instant to the calendar date in a US zone whose standard
/// offset from UTC is `std_offset_hours` (Eastern -5, Pacific -8), applying
/// the US DST rule (second Sunday of March 02:00 to first Sunday of November
/// 02:00 local). Amazon reports local Pacific times; Shopify reports UTC.
pub fn us_local_date(utc: NaiveDateTime, std_offset_hours: i64) -> NaiveDate {
    let year = utc.date().year();
    let dst_start = nth_sunday(year, 3, 2)
        .and_hms_opt(2, 0, 0)
        .expect("valid time")
        - Duration::hours(std_offset_hours);
    let dst_end = nth_sunday(year, 11, 1)
        .and_hms_opt(2, 0, 0)
        .expect("valid time")
        - Duration::hours(std_offset_hours + 1);
    let offset = if utc >= dst_start && utc < dst_end {
        std_offset_hours + 1
    } else {
        std_offset_hours
    };
    (utc + Duration::hours(offset)).date()
}

/// `2026-04-28T11:25:37Z` or `2026-04-28 11:25:37 UTC` to a UTC instant.
pub fn parse_utc(raw: &str) -> Result<NaiveDateTime, ChannelError> {
    let s = raw.trim().trim_end_matches("UTC").trim().trim_end_matches('Z');
    let s = s.replace('T', " ");
    let s = s.split('.').next().unwrap_or(&s).trim().to_string();
    NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
        .map_err(|_| ChannelError::Parse(format!("unreadable timestamp {raw:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_handle_quotes_newlines_and_bom() {
        let text = "\u{feff}a,b,c\r\n1,\"x, \"\"y\"\"\",3\n\"multi\nline\",,z\n\n";
        let r = parse_records(text, ',');
        assert_eq!(r.len(), 3);
        assert_eq!(r[1], vec!["1", "x, \"y\"", "3"]);
        assert_eq!(r[2], vec!["multi\nline", "", "z"]);
    }

    #[test]
    fn records_tab_delimited_keeps_empty_trailing_cells() {
        let r = parse_records("a\tb\t\n1\t\t\n", '\t');
        assert_eq!(r[0], vec!["a", "b", ""]);
        assert_eq!(r[1], vec!["1", "", ""]);
    }

    #[test]
    fn money_parses_signs_commas_and_rejects_third_decimal() {
        assert_eq!(parse_money("1,234.56").unwrap().minor(), 123_456);
        assert_eq!(parse_money("-0.5").unwrap().minor(), -50);
        assert_eq!(parse_money("(2.00)").unwrap().minor(), -200);
        assert_eq!(parse_money("").unwrap().minor(), 0);
        assert_eq!(parse_money("7").unwrap().minor(), 700);
        assert_eq!(parse_money("12.500").unwrap().minor(), 1250);
        assert!(parse_money("12.505").is_err());
        assert!(parse_money("abc").is_err());
    }

    #[test]
    fn us_local_date_applies_dst() {
        let t = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        // 03:32 UTC on 8 May is 20:32 PDT on 7 May.
        assert_eq!(
            us_local_date(t("2026-05-08 03:32:34"), -8),
            NaiveDate::from_ymd_opt(2026, 5, 7).unwrap()
        );
        // In January the same instant is 19:32 PST the previous day.
        assert_eq!(
            us_local_date(t("2026-01-08 03:32:34"), -8),
            NaiveDate::from_ymd_opt(2026, 1, 7).unwrap()
        );
        // Eastern, summer: 02:00 UTC is 22:00 EDT the day before.
        assert_eq!(
            us_local_date(t("2026-07-02 02:00:00"), -5),
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
        );
        // DST edges 2026: starts 8 March, ends 1 November.
        assert_eq!(nth_sunday(2026, 3, 2), NaiveDate::from_ymd_opt(2026, 3, 8).unwrap());
        assert_eq!(nth_sunday(2026, 11, 1), NaiveDate::from_ymd_opt(2026, 11, 1).unwrap());
    }
}
