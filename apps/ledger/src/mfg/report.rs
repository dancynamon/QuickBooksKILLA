//! Manufacturing reports. `LEDGER-DESIGN.md` §11: "one overrun is noise; the
//! same product every time is a recipe that is lying to you."
//!
//! [`material_on_hand`] is the report this module is proudest of: it values
//! `materials` at the same moving average every build and receipt already
//! maintains, and that total must agree with 1300's own trial-balance
//! reading to the cent, every time, by construction — the invariant
//! `tests/mfg.rs` checks after every receipt and build.

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::params;
use rust_decimal::Decimal;

use ledger_core::Money;

use crate::chart;
use crate::store::Ledger;

use super::build::BuildRow;
use super::sheet::{self, StandardCost};
use super::MfgError;

/// One item's build variance over a date range: how many builds, how much
/// they varied from standard in total, and as a percentage of what standard
/// said they should have cost. A single bad build barely moves this; the
/// same item losing money on every build is what this report exists to show.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemVariance {
    pub item_id: String,
    pub build_count: i64,
    pub total_standard_material: Money,
    /// Sum of `builds.variance_minor` — positive is favourable (net credits
    /// to 5100), negative is unfavourable (net debits).
    pub total_variance: Money,
    /// `total_variance / total_standard_material`, `None` when the item had
    /// no standard material cost to divide by (a zero-material sheet).
    pub variance_pct: Option<Decimal>,
}

/// §11 "one overrun is noise; the same product every time is a recipe that
/// is lying to you": every item with a build completed in `[from, to]`,
/// worst variance percentage first, so the recipe worth questioning is the
/// one at the top rather than one this report leaves Dan to notice.
pub fn variance_by_item(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<ItemVariance>, MfgError> {
    // §11's `variance = material_extended - actual_material` (`build`'s own
    // doc comment), so the standard material a build's quantity called for
    // is `variance + actual_material` — summed here rather than re-derived
    // from `standard_cost_minor`, which also carries labour, overhead and
    // parts.
    let mut stmt = ledger.conn().prepare(
        "SELECT item_id, COUNT(*), SUM(variance_minor + actual_material_minor), SUM(variance_minor) \
         FROM builds \
         WHERE company_id = ?1 AND completed_on BETWEEN ?2 AND ?3 \
         GROUP BY item_id",
    )?;
    let rows = stmt
        .query_map(params![company, from.to_string(), to.to_string()], |row| {
            let item_id: String = row.get(0)?;
            let build_count: i64 = row.get(1)?;
            let total_standard_material: i64 = row.get(2)?;
            let total_variance: i64 = row.get(3)?;
            Ok((
                item_id,
                build_count,
                total_standard_material,
                total_variance,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut result: Vec<ItemVariance> = rows
        .into_iter()
        .map(
            |(item_id, build_count, total_standard_material, total_variance)| {
                let total_standard_material = Money::from_minor(total_standard_material);
                let total_variance = Money::from_minor(total_variance);
                let variance_pct = if total_standard_material.is_zero() {
                    None
                } else {
                    Some(
                        Decimal::new(total_variance.minor(), 2)
                            / Decimal::new(total_standard_material.minor(), 2),
                    )
                };
                ItemVariance {
                    item_id,
                    build_count,
                    total_standard_material,
                    total_variance,
                    variance_pct,
                }
            },
        )
        .collect();

    result.sort_by(|a, b| {
        let key = |v: &ItemVariance| v.variance_pct.map(|p| p.abs()).unwrap_or(Decimal::ZERO);
        key(b).cmp(&key(a)).then_with(|| a.item_id.cmp(&b.item_id))
    });
    Ok(result)
}

/// One material's value at its current moving average.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterialOnHand {
    pub material_id: String,
    pub qty_on_hand: Decimal,
    pub avg_cost_minor_per_unit: Money,
    pub value: Money,
}

/// §11: `materials` valued at average, next to 1300's own trial-balance
/// reading — the pairing `tests/mfg.rs` proves agrees to the cent after
/// every receipt and build.
#[derive(Clone, Debug, PartialEq)]
pub struct OnHandReport {
    pub materials: Vec<MaterialOnHand>,
    pub total_value: Money,
    pub account_1300_balance: Money,
}

pub fn material_on_hand(ledger: &Ledger, company: &str) -> Result<OnHandReport, MfgError> {
    let materials = ledger.list_materials(company)?;
    let mut rows = Vec::with_capacity(materials.len());
    let mut total_value = Money::ZERO;
    for material in &materials {
        // `total_value_minor`, exactly — not `qty_on_hand *
        // avg_cost_minor_per_unit`, which rounds at every receipt and would
        // drift from 1300 by a cent or two after enough of them (see
        // `Material::total_value_minor`'s doc).
        let value = material.total_value_minor;
        total_value = total_value.checked_add(value)?;
        rows.push(MaterialOnHand {
            material_id: material.material_id.clone(),
            qty_on_hand: material.qty_on_hand,
            avg_cost_minor_per_unit: material.avg_cost_minor_per_unit,
            value,
        });
    }

    let account_1300_balance = account_balance(ledger, company, chart::INVENTORY_RAW)?;

    Ok(OnHandReport {
        materials: rows,
        total_value,
        account_1300_balance,
    })
}

/// `SUM(debit) - SUM(credit)` over posted lines on one account — the same
/// reading `LEDGER-DESIGN.md` §7's reconciliation SQL takes, duplicated here
/// (rather than reused from `crate::report`, which this module does not
/// touch) so `material_on_hand` can check itself against 1300 without a
/// second store round trip through a different module's shape.
fn account_balance(ledger: &Ledger, company: &str, account: &str) -> Result<Money, MfgError> {
    let (debit, credit): (i64, i64) = ledger.conn().query_row(
        "SELECT COALESCE(SUM(l.debit_minor), 0), COALESCE(SUM(l.credit_minor), 0)
         FROM journal_lines l
         JOIN journal_entries e ON e.company_id = l.company_id AND e.entry_id = l.entry_id
         WHERE l.company_id = ?1 AND l.account_id = ?2 AND e.is_posted = 1",
        params![company, account],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Money::from_minor(debit).checked_sub(Money::from_minor(credit))?)
}

/// A build sheet's standard cost next to what its builds have actually done,
/// so "what should this cost" and "what has it been costing" sit side by
/// side.
#[derive(Clone, Debug, PartialEq)]
pub struct CostRollup {
    pub item_id: String,
    pub sheet_id: String,
    pub standard: StandardCost,
    pub build_count: i64,
    pub total_actual_material: Money,
    pub total_variance: Money,
}

pub fn cost_rollup(ledger: &Ledger, company: &str, item_id: &str) -> Result<CostRollup, MfgError> {
    let sheet_id: String = ledger
        .conn()
        .query_row(
            "SELECT sheet_id FROM build_sheets \
             WHERE company_id = ?1 AND item_id = ?2 AND is_active = 1 \
             ORDER BY version DESC LIMIT 1",
            params![company, item_id],
            |row| row.get(0),
        )
        .map_err(|_| MfgError::UnknownSheet(item_id.to_string()))?;

    let sheet_full = ledger.build_sheet(company, &sheet_id)?;
    let mut materials = HashMap::new();
    for line in &sheet_full.lines {
        if let Some(id) = line.component_id() {
            if !materials.contains_key(id) {
                materials.insert(id.to_string(), ledger.material(company, id)?);
            }
        }
    }
    let standard = sheet::standard_cost(&sheet_full, &materials)?;

    let builds: Vec<BuildRow> = ledger
        .list_builds(company)?
        .into_iter()
        .filter(|build| build.item_id == item_id)
        .collect();
    let total_actual_material = Money::checked_sum(builds.iter().map(|b| b.actual_material))?;
    let total_variance = Money::checked_sum(builds.iter().map(|b| b.variance))?;

    Ok(CostRollup {
        item_id: item_id.to_string(),
        sheet_id,
        standard,
        build_count: builds.len() as i64,
        total_actual_material,
        total_variance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mfg::landed::{receive_materials, RawMaterialReceipt, RawMaterialReceiptLine};
    use crate::mfg::sheet::{NewBuildSheet, NewBuildSheetLine};
    use crate::mfg::NewMaterial;
    use crate::store::Ledger;
    use crate::types::ClassId;
    use chrono::Utc;

    fn setup() -> Ledger {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .create_company("aquamentor", "Aquamentor", None, Utc::now())
            .expect("company");
        ledger
            .add_material(
                "aquamentor",
                &NewMaterial {
                    material_id: "FOAM-XLPE-RED".into(),
                    name: "XLPE 2lb sheet, red".into(),
                    unit: "bf".into(),
                    sheet_board_feet: None,
                    source_ref: None,
                },
            )
            .unwrap();
        receive_materials(
            &ledger,
            "aquamentor",
            RawMaterialReceipt {
                vendor_ref: None,
                po_ref: None,
                received_on: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
                lines: vec![RawMaterialReceiptLine {
                    material_id: "FOAM-XLPE-RED".into(),
                    qty: Decimal::from(200),
                    base_cost: Money::from_minor(80000),
                }],
                freight: Money::ZERO,
            },
            Utc::now(),
        )
        .unwrap();
        ledger
            .add_build_sheet(
                "aquamentor",
                &NewBuildSheet {
                    sheet_id: "XRT-50-STD".into(),
                    item_id: "XRT-50-STD".into(),
                    version: 1,
                    name: "50in rescue tube".into(),
                    yield_pct: Decimal::new(78, 2),
                    labour_minutes: Decimal::from(6),
                    labour_rate_minor_per_hour: Money::from_minor(4800),
                    overhead_minor: Money::ZERO,
                    lines: vec![NewBuildSheetLine {
                        material_id: Some("FOAM-XLPE-RED".into()),
                        part_item_id: None,
                        qty_per_unit: Decimal::new(26, 1),
                        unit: "bf".into(),
                        note: None,
                    }],
                },
                Utc::now(),
            )
            .unwrap();
        ledger
    }

    #[test]
    fn material_on_hand_agrees_with_1300_to_the_cent() {
        let ledger = setup();
        let report = material_on_hand(&ledger, "aquamentor").expect("report");
        assert_eq!(report.total_value, report.account_1300_balance);
        assert_eq!(report.total_value, Money::from_minor(80000));

        super::super::build::complete_build(
            &ledger,
            "aquamentor",
            super::super::build::NewBuild {
                sheet_id: "XRT-50-STD".into(),
                qty_built: Decimal::from(10),
                consumptions: vec![("FOAM-XLPE-RED".into(), Decimal::from(34))],
                labour_minutes_actual: Decimal::from(60),
                completed_on: NaiveDate::from_ymd_opt(2026, 8, 19).unwrap(),
                started_on: None,
                class: ClassId("foam".into()),
                note: None,
            },
            Utc::now(),
        )
        .expect("build posts");

        let after = material_on_hand(&ledger, "aquamentor").expect("report");
        assert_eq!(after.total_value, after.account_1300_balance);
    }

    #[test]
    fn cost_rollup_finds_the_items_active_sheet() {
        let ledger = setup();
        let rollup = cost_rollup(&ledger, "aquamentor", "XRT-50-STD").expect("rollup");
        assert_eq!(rollup.sheet_id, "XRT-50-STD");
        assert_eq!(rollup.build_count, 0);
    }

    #[test]
    fn variance_by_item_sums_and_sorts_by_worst_first() {
        let ledger = setup();
        for (consumed, day) in [(Decimal::from(4), 19), (Decimal::from(3), 20)] {
            super::super::build::complete_build(
                &ledger,
                "aquamentor",
                super::super::build::NewBuild {
                    sheet_id: "XRT-50-STD".into(),
                    qty_built: Decimal::ONE,
                    consumptions: vec![("FOAM-XLPE-RED".into(), consumed)],
                    labour_minutes_actual: Decimal::from(6),
                    completed_on: NaiveDate::from_ymd_opt(2026, 8, day).unwrap(),
                    started_on: None,
                    class: ClassId("foam".into()),
                    note: None,
                },
                Utc::now(),
            )
            .unwrap();
        }
        let report = variance_by_item(
            &ledger,
            "aquamentor",
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        )
        .expect("report");
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].build_count, 2);
    }
}
