//! Parsing QBO's Reports API JSON, and the CSV shape `ledger tbdiff` /
//! `ledger boundary` read. `LEDGER-DESIGN.md` §6, §7.
//!
//! The Reports API returns `Header`, `Columns` and `Rows`, where `Rows` is a
//! tree: a leaf ("Data") row carries its values directly under `ColData`; a
//! group ("Section") row instead carries a `Header` (the section title), a
//! nested `Rows.Row[]`, and a `Summary` (the section subtotal) — neither of
//! which is itself a row this module reads. Walking the tree and keeping only
//! the nodes with a top-level `ColData` is what "ignoring summary rows"
//! means: a `Summary` or section `Header` never has `ColData` at that level,
//! so it is never visited in the first place, not filtered out after the
//! fact.
//!
//! Every amount goes through [`round_money`] under
//! [`RoundingPolicy::MirroredAmount`] — this module mirrors what QBO already
//! computed rather than computing anything itself, so a value carrying a
//! third decimal place is a misread, not a rounding decision (`project.rs`'s
//! `optional_money`, D8/D9).

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};

use crate::client::{QboClient, QboError, ReportName, ReportParams};
use crate::domain::RealmId;

#[derive(Debug, Error)]
pub enum ReportsError {
    #[error("qbo: {0}")]
    Qbo(#[from] QboError),
    #[error("unexpected report shape: {0}")]
    Shape(String),
    /// D8/D9: an amount with more than two decimal places is a misread, not
    /// something to round away.
    #[error("{field}: {found:?} carries more than two decimal places — an error, not a rounding")]
    Precision { field: String, found: String },
    #[error("money: {0}")]
    Money(#[from] MoneyError),
    #[error("io error writing {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Walking the row tree
// ---------------------------------------------------------------------------

/// Every leaf ("Data") row under `node`'s `Rows.Row[]`, found by recursing
/// into any row that itself has a nested `Rows.Row[]`. A row is a leaf the
/// walk collects when it carries `ColData` directly; a `Summary` or a
/// section's own `Header` sit beside `Rows`, never inside it, so they are
/// never reached at all.
fn collect_data_rows<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    let Some(rows) = node
        .get("Rows")
        .and_then(|rows| rows.get("Row"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for row in rows {
        if row.get("ColData").is_some() {
            out.push(row);
        }
        collect_data_rows(row, out);
    }
}

fn col_str(row: &Value, index: usize, context: &str) -> Result<String, ReportsError> {
    row.pointer(&format!("/ColData/{index}/value"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ReportsError::Shape(format!("{context}: missing ColData[{index}].value")))
}

fn col_id(row: &Value, index: usize) -> Option<String> {
    row.pointer(&format!("/ColData/{index}/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// An empty string is QBO's spelling of zero on a report cell (a debit column
/// on a credit-balance row, and so on); anything else must parse as a decimal
/// of at most two places.
fn parse_amount(text: &str, field: &str) -> Result<Money, ReportsError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Money::ZERO);
    }
    let decimal = Decimal::from_str(trimmed)
        .map_err(|_| ReportsError::Shape(format!("{field}: not a number: {text:?}")))?;
    if decimal.scale() > 2 {
        return Err(ReportsError::Precision {
            field: field.to_string(),
            found: text.to_string(),
        });
    }
    Ok(round_money(decimal, RoundingPolicy::MirroredAmount)?)
}

// ---------------------------------------------------------------------------
// Trial balance
// ---------------------------------------------------------------------------

/// One data row of a `TrialBalance` report: `ColData[0]` (name, and its
/// account id when QBO gives one), `ColData[1]` (debit), `ColData[2]`
/// (credit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrialBalanceRow {
    pub qbo_account_id: Option<String>,
    pub name: String,
    pub debit: Money,
    pub credit: Money,
}

pub fn parse_trial_balance(value: &Value) -> Result<Vec<TrialBalanceRow>, ReportsError> {
    let mut leaves = Vec::new();
    collect_data_rows(value, &mut leaves);
    leaves
        .into_iter()
        .map(|row| {
            Ok(TrialBalanceRow {
                qbo_account_id: col_id(row, 0),
                name: col_str(row, 0, "trial balance name")?,
                debit: parse_amount(&col_str(row, 1, "trial balance debit")?, "debit")?,
                credit: parse_amount(&col_str(row, 2, "trial balance credit")?, "credit")?,
            })
        })
        .collect()
}

/// Saturating rather than checked: real trial-balance figures never approach
/// `i64::MAX` minor units, and both `trial_balance_csv` and the row types
/// above are infallible by design — a genuine overflow would show up
/// immediately as an absurd number rather than silently wrapping, which is
/// the direction that matters.
fn signed_balance(debit: Money, credit: Money) -> Money {
    Money::from_minor(debit.minor().saturating_sub(credit.minor()))
}

fn money_to_csv(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// `qbo_account_id,name,balance` — exactly the shape `ledger tbdiff` /
/// `ledger opening` / `ledger boundary` read (`apps/ledger/src/main.rs`'s
/// `read_qbo_csv`), balance signed with debit positive. No quoting, matching
/// that reader; a comma inside an account name is replaced with a space
/// rather than corrupting the column count.
pub fn trial_balance_csv(rows: &[TrialBalanceRow]) -> String {
    let mut out = String::from("qbo_account_id,name,balance\n");
    for row in rows {
        let balance = signed_balance(row.debit, row.credit);
        out.push_str(&format!(
            "{},{},{}\n",
            row.qbo_account_id.as_deref().unwrap_or(""),
            row.name.replace(',', " "),
            money_to_csv(balance),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Aging (AR/AP)
// ---------------------------------------------------------------------------

/// One data row of an `AgedReceivables`/`AgedPayables` summary report:
/// `ColData[0]` (customer/vendor name and id), then current, 1-30, 31-60,
/// 61-90, over-90 and a total column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgingRow {
    pub entity_id: Option<String>,
    pub name: String,
    pub current: Money,
    pub d1_30: Money,
    pub d31_60: Money,
    pub d61_90: Money,
    pub over_90: Money,
    pub total: Money,
}

pub fn parse_aging(value: &Value) -> Result<Vec<AgingRow>, ReportsError> {
    let mut leaves = Vec::new();
    collect_data_rows(value, &mut leaves);
    leaves
        .into_iter()
        .map(|row| {
            Ok(AgingRow {
                entity_id: col_id(row, 0),
                name: col_str(row, 0, "aging name")?,
                current: parse_amount(&col_str(row, 1, "aging current")?, "current")?,
                d1_30: parse_amount(&col_str(row, 2, "aging 1-30")?, "1-30")?,
                d31_60: parse_amount(&col_str(row, 3, "aging 31-60")?, "31-60")?,
                d61_90: parse_amount(&col_str(row, 4, "aging 61-90")?, "61-90")?,
                over_90: parse_amount(&col_str(row, 5, "aging over 90")?, "over 90")?,
                total: parse_amount(&col_str(row, 6, "aging total")?, "total")?,
            })
        })
        .collect()
}

/// `entity_id,name,current,1-30,31-60,61-90,over_90,total` — this module's
/// own shape (nothing downstream reads an aging CSV back yet), for
/// `qbo-local report --name ar-aging|ap-aging`.
pub fn aging_csv(rows: &[AgingRow]) -> String {
    let mut out = String::from("entity_id,name,current,1-30,31-60,61-90,over_90,total\n");
    for row in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            row.entity_id.as_deref().unwrap_or(""),
            row.name.replace(',', " "),
            money_to_csv(row.current),
            money_to_csv(row.d1_30),
            money_to_csv(row.d31_60),
            money_to_csv(row.d61_90),
            money_to_csv(row.over_90),
            money_to_csv(row.total),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Year-end snapshots for `ledger boundary` (§6)
// ---------------------------------------------------------------------------

/// Pull a `TrialBalance` as of 31 December of every year in `years` and write
/// each as `<dir>/tb-<year>.csv` — the exact filename and CSV shape
/// `ledger boundary --snapshots DIR` reads. Accrual basis, per §6 step 1.
pub fn write_year_end_snapshots<C: QboClient>(
    client: &mut C,
    realm: &RealmId,
    years: RangeInclusive<i32>,
    dir: &Path,
) -> Result<Vec<PathBuf>, ReportsError> {
    fs::create_dir_all(dir).map_err(|source| ReportsError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut written = Vec::new();
    for year in years {
        let end = NaiveDate::from_ymd_opt(year, 12, 31).expect("31 December is always valid");
        let params = ReportParams {
            start_date: None,
            end_date: Some(end),
            accounting_method: Some("Accrual".to_string()),
            date_macro: None,
            aging_method: None,
        };
        let value = client.report(realm, ReportName::TrialBalance, &params)?;
        let rows = parse_trial_balance(&value)?;
        let csv = trial_balance_csv(&rows);
        let path = dir.join(format!("tb-{year}.csv"));
        fs::write(&path, csv).map_err(|source| ReportsError::Io {
            path: path.clone(),
            source,
        })?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockQbo;
    use chrono::Utc;
    use serde_json::json;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    /// A realistic (invented) shape: two sections, each with a nested data
    /// row and a `Summary` this walk must never mistake for a third row.
    fn trial_balance_json() -> Value {
        json!({
            "Header": {
                "ReportName": "TrialBalance",
                "StartPeriod": "2026-01-01",
                "EndPeriod": "2026-09-10",
            },
            "Columns": {
                "Column": [
                    {"ColTitle": "", "ColType": "Account"},
                    {"ColTitle": "Debit", "ColType": "Money"},
                    {"ColTitle": "Credit", "ColType": "Money"},
                ]
            },
            "Rows": {
                "Row": [
                    {
                        "type": "Section",
                        "group": "TB",
                        "Header": {"ColData": [{"value": "ASSETS"}, {"value": ""}, {"value": ""}]},
                        "Rows": {
                            "Row": [
                                {
                                    "type": "Data",
                                    "ColData": [
                                        {"value": "Checking", "id": "35"},
                                        {"value": "412884.19"},
                                        {"value": ""}
                                    ]
                                },
                                {
                                    "type": "Data",
                                    "ColData": [
                                        {"value": "Accounts Receivable", "id": "42"},
                                        {"value": "12345.67"},
                                        {"value": ""}
                                    ]
                                }
                            ]
                        },
                        "Summary": {"ColData": [{"value": "Total Assets"}, {"value": "425229.86"}, {"value": ""}]}
                    },
                    {
                        "type": "Section",
                        "group": "TB",
                        "Header": {"ColData": [{"value": "LIABILITIES AND EQUITY"}, {"value": ""}, {"value": ""}]},
                        "Rows": {
                            "Row": [
                                {
                                    "type": "Data",
                                    "ColData": [
                                        {"value": "Sales Tax Payable", "id": "61"},
                                        {"value": ""},
                                        {"value": "8412.06"}
                                    ]
                                }
                            ]
                        },
                        "Summary": {"ColData": [{"value": "Total Liabilities and Equity"}, {"value": ""}, {"value": "8412.06"}]}
                    }
                ]
            }
        })
    }

    #[test]
    fn parse_trial_balance_reads_leaves_and_ignores_summaries() {
        let rows = parse_trial_balance(&trial_balance_json()).unwrap();
        assert_eq!(rows.len(), 3, "the two section Summary rows must not appear: {rows:?}");

        assert_eq!(rows[0].qbo_account_id.as_deref(), Some("35"));
        assert_eq!(rows[0].name, "Checking");
        assert_eq!(rows[0].debit, Money::from_minor(41_288_419));
        assert_eq!(rows[0].credit, Money::ZERO);

        assert_eq!(rows[2].name, "Sales Tax Payable");
        assert_eq!(rows[2].debit, Money::ZERO);
        assert_eq!(rows[2].credit, Money::from_minor(841_206));
    }

    #[test]
    fn parse_trial_balance_rejects_a_third_decimal_place() {
        let value = json!({
            "Rows": { "Row": [
                { "type": "Data", "ColData": [
                    {"value": "Checking", "id": "35"}, {"value": "1.005"}, {"value": ""}
                ] }
            ] }
        });
        let err = parse_trial_balance(&value).unwrap_err();
        assert!(matches!(err, ReportsError::Precision { .. }), "{err:?}");
    }

    #[test]
    fn trial_balance_csv_matches_the_shape_ledger_tbdiff_reads() {
        let rows = parse_trial_balance(&trial_balance_json()).unwrap();
        let csv = trial_balance_csv(&rows);
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("qbo_account_id,name,balance"));
        assert_eq!(lines.next(), Some("35,Checking,412884.19"));
        assert_eq!(lines.next(), Some("42,Accounts Receivable,12345.67"));
        assert_eq!(lines.next(), Some("61,Sales Tax Payable,-8412.06"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn trial_balance_csv_strips_commas_from_names() {
        let rows = vec![TrialBalanceRow {
            qbo_account_id: Some("1".to_string()),
            name: "Fees, Bank".to_string(),
            debit: Money::from_minor(100),
            credit: Money::ZERO,
        }];
        let csv = trial_balance_csv(&rows);
        assert_eq!(
            csv,
            "qbo_account_id,name,balance\n1,Fees  Bank,1.00\n"
        );
    }

    fn aging_json() -> Value {
        json!({
            "Header": { "ReportName": "AgedReceivables" },
            "Rows": {
                "Row": [
                    {
                        "type": "Data",
                        "ColData": [
                            {"value": "Blue Harbor Swim Club", "id": "31"},
                            {"value": "1200.00"},
                            {"value": "300.00"},
                            {"value": ""},
                            {"value": ""},
                            {"value": ""},
                            {"value": "1500.00"}
                        ]
                    },
                    {
                        "type": "Total",
                        "group": "GrandTotal",
                        "Summary": {"ColData": [
                            {"value": "TOTAL"}, {"value": "1200.00"}, {"value": "300.00"},
                            {"value": ""}, {"value": ""}, {"value": ""}, {"value": "1500.00"}
                        ]}
                    }
                ]
            }
        })
    }

    #[test]
    fn parse_aging_reads_the_seven_columns_and_ignores_the_grand_total() {
        let rows = parse_aging(&aging_json()).unwrap();
        assert_eq!(rows.len(), 1, "the trailing Total row has no ColData of its own: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.entity_id.as_deref(), Some("31"));
        assert_eq!(row.name, "Blue Harbor Swim Club");
        assert_eq!(row.current, Money::from_minor(120_000));
        assert_eq!(row.d1_30, Money::from_minor(30_000));
        assert_eq!(row.d31_60, Money::ZERO);
        assert_eq!(row.total, Money::from_minor(150_000));
    }

    #[test]
    fn aging_csv_renders_every_bucket() {
        let rows = parse_aging(&aging_json()).unwrap();
        let csv = aging_csv(&rows);
        assert_eq!(
            csv,
            "entity_id,name,current,1-30,31-60,61-90,over_90,total\n\
             31,Blue Harbor Swim Club,1200.00,300.00,0.00,0.00,0.00,1500.00\n"
        );
    }

    #[test]
    fn write_year_end_snapshots_writes_one_tb_csv_per_year() {
        // `MockQbo` never overrides `report` (the trait default), so this
        // proves the plumbing end to end with a client that replays a fixed
        // JSON value in place of a real report — wired through a thin
        // wrapper rather than MockQbo itself.
        struct CannedReports {
            value: Value,
        }
        impl QboClient for CannedReports {
            fn query(
                &mut self,
                _: &RealmId,
                _: crate::domain::EntityType,
                _: Option<crate::client::UpdatedRange>,
                _: usize,
                _: usize,
            ) -> Result<Vec<crate::client::EntityPayload>, QboError> {
                Ok(Vec::new())
            }
            fn cdc(
                &mut self,
                _: &RealmId,
                _: &[crate::domain::EntityType],
                _: chrono::DateTime<Utc>,
            ) -> Result<Vec<crate::client::EntityPayload>, QboError> {
                Ok(Vec::new())
            }
            fn create(
                &mut self,
                _: &RealmId,
                _: crate::domain::EntityType,
                _: &Value,
                _: uuid::Uuid,
            ) -> Result<crate::client::EntityPayload, QboError> {
                Err(QboError::Validation("unused in this test".to_string()))
            }
            fn update(
                &mut self,
                _: &RealmId,
                _: crate::domain::EntityType,
                _: &str,
                _: &Value,
                _: &str,
                _: uuid::Uuid,
            ) -> Result<crate::client::EntityPayload, QboError> {
                Err(QboError::Validation("unused in this test".to_string()))
            }
            fn find_by_name(
                &mut self,
                _: &RealmId,
                _: crate::domain::EntityType,
                _: &str,
            ) -> Result<Option<crate::client::EntityPayload>, QboError> {
                Ok(None)
            }
            fn fetch(
                &mut self,
                _: &RealmId,
                _: crate::domain::EntityType,
                _: &str,
            ) -> Result<Option<crate::client::EntityPayload>, QboError> {
                Ok(None)
            }
            fn report(
                &mut self,
                _: &RealmId,
                _: ReportName,
                _: &ReportParams,
            ) -> Result<Value, QboError> {
                Ok(self.value.clone())
            }
        }

        let mut client = CannedReports {
            value: trial_balance_json(),
        };
        let directory = tempfile::tempdir().unwrap();
        let written = write_year_end_snapshots(&mut client, &realm(), 2024..=2025, directory.path())
            .unwrap();

        assert_eq!(written.len(), 2);
        for (year, path) in [(2024, &written[0]), (2025, &written[1])] {
            assert_eq!(path, &directory.path().join(format!("tb-{year}.csv")));
            let contents = fs::read_to_string(path).unwrap();
            assert!(contents.starts_with("qbo_account_id,name,balance\n"));
            assert!(contents.contains("35,Checking,412884.19"));
        }
        // `MockQbo` really does refuse — confirms the default this whole
        // module leans on has not quietly grown a report implementation.
        let mut mock = MockQbo::new(Utc::now());
        assert!(mock
            .report(&realm(), ReportName::TrialBalance, &ReportParams::default())
            .is_err());
    }
}
