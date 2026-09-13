//! Build sheets. `LEDGER-DESIGN.md` §11 "Build sheets, and yield as a
//! divisor" and "Sensitivity", `prototype/bunzbooks.html`'s `recipeCost`.
//!
//! **Yield divides, it does not subtract.** To ship `qty_per_unit` board feet
//! of finished part at a nesting yield of `yield_pct` you must buy
//! `qty_per_unit / yield_pct` board feet, and you paid for all of it — the
//! waste, the difference between the two, is a column on every costing view
//! ([`StandardCost::material_gross_bf`]) rather than a footnote.
//!
//! [`sensitivity`] is a pure function over the same data, no schema of its
//! own: what a foam price move does to margin, and what a yield point is
//! worth, so "five points of yield beats a five percent foam discount"
//! (`prototype/README.md` "Manufacturing") is a row Dan reads rather than
//! arithmetic he has to do himself.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rusqlite::{params, OptionalExtension, Row};
use serde::Deserialize;

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::store::Ledger;

use super::{Material, MfgError};

/// A `build_sheet_lines.unit`: board feet (yield-divided, foam) or each
/// (bought parts, hardware — yield never applies to something you buy
/// already finished).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SheetLineUnit {
    Bf,
    Ea,
}

impl SheetLineUnit {
    pub const fn as_str(self) -> &'static str {
        match self {
            SheetLineUnit::Bf => "bf",
            SheetLineUnit::Ea => "ea",
        }
    }

    pub fn parse(raw: &str) -> Option<SheetLineUnit> {
        match raw {
            "bf" => Some(SheetLineUnit::Bf),
            "ea" => Some(SheetLineUnit::Ea),
            _ => None,
        }
    }
}

/// One row of `build_sheets`.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildSheet {
    pub sheet_id: String,
    pub item_id: String,
    pub version: i64,
    pub name: String,
    pub yield_pct: Decimal,
    /// Total shop minutes per unit at standard.
    pub labour_minutes: Decimal,
    pub labour_rate_minor_per_hour: Money,
    /// Flat overhead applied per finished unit.
    pub overhead_minor: Money,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

/// One row of `build_sheet_lines`. Exactly one of `material_id` and
/// `part_item_id` is set — a board-foot line against `materials` (foam, cut
/// to a nesting yield) or a bought-part line against the general item
/// catalogue (hardware, priced at its own standard cost, never yield-divided)
/// — enforced by the migration's `CHECK` and again by [`standard_cost`].
#[derive(Clone, Debug, PartialEq)]
pub struct BuildSheetLine {
    pub line_no: i64,
    pub material_id: Option<String>,
    pub part_item_id: Option<String>,
    pub qty_per_unit: Decimal,
    pub unit: SheetLineUnit,
    pub note: Option<String>,
}

impl BuildSheetLine {
    /// Whichever of `material_id`/`part_item_id` is set — both resolve
    /// against the same `materials` table (a bought part is simply a
    /// material stocked `ea`), so every caller of [`standard_cost`] looks
    /// this one id up rather than branching on which column it came from.
    pub fn component_id(&self) -> Option<&str> {
        self.material_id.as_deref().or(self.part_item_id.as_deref())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuildSheetFull {
    pub header: BuildSheet,
    pub lines: Vec<BuildSheetLine>,
}

/// `ledger mfg sheet add --file`.
#[derive(Clone, Debug, Deserialize)]
pub struct NewBuildSheetLine {
    #[serde(default)]
    pub material_id: Option<String>,
    #[serde(default)]
    pub part_item_id: Option<String>,
    pub qty_per_unit: Decimal,
    pub unit: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewBuildSheet {
    pub sheet_id: String,
    pub item_id: String,
    #[serde(default)]
    pub version: i64,
    pub name: String,
    pub yield_pct: Decimal,
    pub labour_minutes: Decimal,
    pub labour_rate_minor_per_hour: Money,
    #[serde(default)]
    pub overhead_minor: Money,
    pub lines: Vec<NewBuildSheetLine>,
}

/// The standard unit cost a build sheet prices to, at each component
/// material's *current* average (§11: moving a foam price re-prices every
/// product that uses it, because nothing here caches a cost).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct StandardCost {
    /// Board-foot material only, yield-divided — what §1's Build row credits
    /// 1300 for at standard, and what the variance in [`crate::mfg::build`]
    /// compares actual consumption against.
    pub material: Money,
    pub labour: Money,
    pub overhead: Money,
    /// `material + labour + overhead +` every bought-part (`ea`) line —
    /// §1's "5300 parts and labour applied" is `total - material`.
    pub total: Money,
    /// Equal to `total`: a build sheet's standard cost is inherently
    /// per-unit, and callers that scale it by quantity built read this name.
    pub per_unit: Money,
    /// Total board feet you must buy to get one finished unit at this
    /// sheet's yield — `sum(qty_per_unit / yield_pct)` over the `bf` lines.
    /// The waste is `material_gross_bf` minus the sum of the lines'
    /// `qty_per_unit`, a column rather than a footnote.
    pub material_gross_bf: Decimal,
}

fn to_decimal(money: Money) -> Decimal {
    Decimal::new(money.minor(), 2)
}

/// §11: `qty_per_unit / yield_pct` for every `bf` line, priced at the
/// material's current average; every `ea` line at its own average, no yield.
/// Rounded once per bucket (material, labour, the `ea` total) as a line
/// extension, then summed — the same discipline `post::extend_unit_cost`
/// uses, so a sheet's standard cost is never off by the kind of double
/// rounding that creeps in from rounding every component twice.
pub fn standard_cost(
    sheet: &BuildSheetFull,
    materials: &HashMap<String, Material>,
) -> Result<StandardCost, MfgError> {
    if sheet.lines.is_empty() {
        return Err(MfgError::EmptySheet(sheet.header.sheet_id.clone()));
    }

    let mut material_dollars = Decimal::ZERO;
    let mut parts_dollars = Decimal::ZERO;
    let mut material_gross_bf = Decimal::ZERO;

    for line in &sheet.lines {
        let component_id = line.component_id().ok_or_else(|| MfgError::LineNamesNothing {
            sheet_id: sheet.header.sheet_id.clone(),
            line_no: line.line_no,
        })?;
        let material = materials
            .get(component_id)
            .ok_or_else(|| MfgError::UnknownMaterial(component_id.to_string()))?;
        let unit_cost = to_decimal(material.avg_cost_minor_per_unit);

        match line.unit {
            SheetLineUnit::Bf => {
                let gross_bf = line.qty_per_unit / sheet.header.yield_pct;
                material_gross_bf += gross_bf;
                material_dollars += gross_bf * unit_cost;
            }
            SheetLineUnit::Ea => {
                parts_dollars += line.qty_per_unit * unit_cost;
            }
        }
    }

    let labour_rate = to_decimal(sheet.header.labour_rate_minor_per_hour);
    let labour_dollars = sheet.header.labour_minutes / Decimal::from(60) * labour_rate;

    let material = round_money(material_dollars, RoundingPolicy::LineExtension)?;
    let labour = round_money(labour_dollars, RoundingPolicy::LineExtension)?;
    let parts = round_money(parts_dollars, RoundingPolicy::LineExtension)?;
    let overhead = sheet.header.overhead_minor;

    let total = material
        .checked_add(labour)?
        .checked_add(overhead)?
        .checked_add(parts)?;

    Ok(StandardCost {
        material,
        labour,
        overhead,
        total,
        per_unit: total,
        material_gross_bf,
    })
}

/// One row of the sensitivity table: what a scenario does to margin, and how
/// that compares to the sheet as it stands today.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    pub name: String,
    /// `(sell_price - standard_cost) / sell_price`. Negative when the sheet
    /// loses money at this price.
    pub margin_pct: Decimal,
    /// `margin_pct` under the scenario minus the base sheet's margin —
    /// signed, so a bigger positive number is unambiguously the better lever.
    pub delta_vs_base: Decimal,
}

fn margin_pct(total: Money, sell_price: Money) -> Decimal {
    if sell_price.is_zero() {
        return Decimal::ZERO;
    }
    let price = to_decimal(sell_price);
    (price - to_decimal(total)) / price
}

fn scenario(name: &str, total: Money, sell_price: Money, base_margin: Decimal) -> Scenario {
    let margin = margin_pct(total, sell_price);
    Scenario {
        name: name.to_string(),
        margin_pct: margin,
        delta_vs_base: margin - base_margin,
    }
}

/// §11 "Sensitivity": foam price ±5%, yield ±5 points, labour ±10%, each
/// against the sheet as it stands today. A pure function — it never touches
/// the store, so it costs nothing to run for every price Dan is curious
/// about.
pub fn sensitivity(
    sheet: &BuildSheetFull,
    materials: &HashMap<String, Material>,
    sell_price: Money,
) -> Result<Vec<Scenario>, MfgError> {
    let base = standard_cost(sheet, materials)?;
    let base_margin = margin_pct(base.total, sell_price);
    let mut scenarios = Vec::with_capacity(6);

    let apply_material_factor = |factor: Decimal| -> Result<Money, MfgError> {
        let adjusted = round_money(to_decimal(base.material) * factor, RoundingPolicy::LineExtension)?;
        Ok(base.total.checked_sub(base.material)?.checked_add(adjusted)?)
    };
    scenarios.push(scenario(
        "Foam price +5%",
        apply_material_factor(Decimal::new(105, 2))?,
        sell_price,
        base_margin,
    ));
    scenarios.push(scenario(
        "Foam price -5%",
        apply_material_factor(Decimal::new(95, 2))?,
        sell_price,
        base_margin,
    ));

    let yield_bounds = (Decimal::new(1, 2), Decimal::new(99, 2));
    for (label, delta) in [
        ("Yield +5 points", Decimal::new(5, 2)),
        ("Yield -5 points", Decimal::new(-5, 2)),
    ] {
        let mut shifted = sheet.clone();
        let candidate = shifted.header.yield_pct + delta;
        shifted.header.yield_pct = candidate.clamp(yield_bounds.0, yield_bounds.1);
        let cost = standard_cost(&shifted, materials)?;
        scenarios.push(scenario(label, cost.total, sell_price, base_margin));
    }

    let apply_labour_factor = |factor: Decimal| -> Result<Money, MfgError> {
        let adjusted = round_money(to_decimal(base.labour) * factor, RoundingPolicy::LineExtension)?;
        Ok(base.total.checked_sub(base.labour)?.checked_add(adjusted)?)
    };
    scenarios.push(scenario(
        "Labour +10%",
        apply_labour_factor(Decimal::new(110, 2))?,
        sell_price,
        base_margin,
    ));
    scenarios.push(scenario(
        "Labour -10%",
        apply_labour_factor(Decimal::new(90, 2))?,
        sell_price,
        base_margin,
    ));

    Ok(scenarios)
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const SHEET_COLUMNS: &str = "sheet_id, item_id, version, name, yield_pct, labour_minutes, \
     labour_rate_minor_per_hour, overhead_minor, is_active, created_at";

fn row_to_sheet(row: &Row<'_>) -> rusqlite::Result<BuildSheet> {
    let yield_raw: String = row.get(4)?;
    let labour_minutes_raw: String = row.get(5)?;
    let created_at_raw: String = row.get(9)?;
    Ok(BuildSheet {
        sheet_id: row.get(0)?,
        item_id: row.get(1)?,
        version: row.get(2)?,
        name: row.get(3)?,
        yield_pct: yield_raw.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(4, "yield_pct".into(), rusqlite::types::Type::Text)
        })?,
        labour_minutes: labour_minutes_raw.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                5,
                "labour_minutes".into(),
                rusqlite::types::Type::Text,
            )
        })?,
        labour_rate_minor_per_hour: Money::from_minor(row.get(6)?),
        overhead_minor: Money::from_minor(row.get(7)?),
        is_active: row.get::<_, i64>(8)? != 0,
        created_at: DateTime::parse_from_rfc3339(&created_at_raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    9,
                    "created_at".into(),
                    rusqlite::types::Type::Text,
                )
            })?,
    })
}

const SHEET_LINE_COLUMNS: &str = "line_no, material_id, part_item_id, qty_per_unit, unit, note";

fn row_to_sheet_line(row: &Row<'_>) -> rusqlite::Result<BuildSheetLine> {
    let qty_raw: String = row.get(3)?;
    let unit_raw: String = row.get(4)?;
    Ok(BuildSheetLine {
        line_no: row.get(0)?,
        material_id: row.get(1)?,
        part_item_id: row.get(2)?,
        qty_per_unit: qty_raw.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                3,
                "qty_per_unit".into(),
                rusqlite::types::Type::Text,
            )
        })?,
        unit: SheetLineUnit::parse(&unit_raw).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(4, "unit".into(), rusqlite::types::Type::Text)
        })?,
        note: row.get(5)?,
    })
}

impl Ledger {
    /// `ledger mfg sheet add`: replaces the sheet's lines wholesale when
    /// `sheet_id` already exists (a new recipe version is a deliberate,
    /// whole-sheet edit, never a line-by-line patch).
    pub fn add_build_sheet(
        &self,
        company: &str,
        new: &NewBuildSheet,
        now: DateTime<Utc>,
    ) -> Result<BuildSheetFull, MfgError> {
        if new.lines.is_empty() {
            return Err(MfgError::EmptySheet(new.sheet_id.clone()));
        }
        let mut units = Vec::with_capacity(new.lines.len());
        for line in &new.lines {
            let unit = SheetLineUnit::parse(&line.unit).ok_or_else(|| MfgError::BadDecimal {
                field: "unit",
                value: line.unit.clone(),
            })?;
            if line.material_id.is_some() == line.part_item_id.is_some() {
                return Err(MfgError::LineNamesNothing {
                    sheet_id: new.sheet_id.clone(),
                    line_no: units.len() as i64 + 1,
                });
            }
            units.push(unit);
        }

        let conn = self.conn();
        conn.execute(
            "INSERT INTO build_sheets
                 (company_id, sheet_id, item_id, version, name, yield_pct, labour_minutes,
                  labour_rate_minor_per_hour, overhead_minor, is_active, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10)
             ON CONFLICT(company_id, sheet_id) DO UPDATE SET
                 item_id = excluded.item_id, version = excluded.version, name = excluded.name,
                 yield_pct = excluded.yield_pct, labour_minutes = excluded.labour_minutes,
                 labour_rate_minor_per_hour = excluded.labour_rate_minor_per_hour,
                 overhead_minor = excluded.overhead_minor, is_active = 1",
            params![
                company,
                new.sheet_id,
                new.item_id,
                new.version,
                new.name,
                super::decimal_to_text(new.yield_pct),
                super::decimal_to_text(new.labour_minutes),
                new.labour_rate_minor_per_hour.minor(),
                new.overhead_minor.minor(),
                now.to_rfc3339(),
            ],
        )?;
        conn.execute(
            "DELETE FROM build_sheet_lines WHERE company_id = ?1 AND sheet_id = ?2",
            params![company, new.sheet_id],
        )?;
        for (i, (line, unit)) in new.lines.iter().zip(&units).enumerate() {
            conn.execute(
                "INSERT INTO build_sheet_lines
                     (company_id, sheet_id, line_no, material_id, part_item_id, qty_per_unit,
                      unit, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    company,
                    new.sheet_id,
                    i as i64 + 1,
                    line.material_id,
                    line.part_item_id,
                    super::decimal_to_text(line.qty_per_unit),
                    unit.as_str(),
                    line.note,
                ],
            )?;
        }

        self.build_sheet(company, &new.sheet_id)
    }

    pub fn build_sheet(&self, company: &str, sheet_id: &str) -> Result<BuildSheetFull, MfgError> {
        let header = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {SHEET_COLUMNS} FROM build_sheets \
                     WHERE company_id = ?1 AND sheet_id = ?2"
                ),
                params![company, sheet_id],
                row_to_sheet,
            )
            .optional()?
            .ok_or_else(|| MfgError::UnknownSheet(sheet_id.to_string()))?;

        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SHEET_LINE_COLUMNS} FROM build_sheet_lines \
             WHERE company_id = ?1 AND sheet_id = ?2 ORDER BY line_no"
        ))?;
        let lines = stmt
            .query_map(params![company, sheet_id], row_to_sheet_line)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BuildSheetFull { header, lines })
    }

    pub fn list_build_sheets(&self, company: &str) -> Result<Vec<BuildSheet>, MfgError> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SHEET_COLUMNS} FROM build_sheets \
             WHERE company_id = ?1 ORDER BY sheet_id"
        ))?;
        let rows = stmt
            .query_map(params![company], row_to_sheet)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(id: &str, avg_cost_minor: i64) -> Material {
        Material {
            material_id: id.into(),
            name: id.into(),
            unit: super::super::MaterialUnit::Bf,
            sheet_board_feet: None,
            avg_cost_minor_per_unit: Money::from_minor(avg_cost_minor),
            qty_on_hand: Decimal::ZERO,
            total_value_minor: Money::ZERO,
            is_active: true,
            source_ref: None,
        }
    }

    /// The worked rescue-tube example (`docs/MFG.md`): a 50" tube cut from 2
    /// lb XLPE at 78% nesting yield, plus hardware and six minutes of shop
    /// time at $48/hour.
    fn rescue_tube_sheet() -> BuildSheetFull {
        BuildSheetFull {
            header: BuildSheet {
                sheet_id: "XRT-50-STD".into(),
                item_id: "XRT-50-STD".into(),
                version: 1,
                name: "50in rescue tube".into(),
                yield_pct: Decimal::new(78, 2),
                labour_minutes: Decimal::from(6),
                labour_rate_minor_per_hour: Money::from_minor(4800),
                overhead_minor: Money::from_minor(150),
                is_active: true,
                created_at: Utc::now(),
            },
            lines: vec![
                BuildSheetLine {
                    line_no: 1,
                    material_id: Some("FOAM-XLPE-RED".into()),
                    part_item_id: None,
                    qty_per_unit: Decimal::new(26, 1), // 2.6 bf
                    unit: SheetLineUnit::Bf,
                    note: Some("round blank".into()),
                },
                BuildSheetLine {
                    line_no: 2,
                    material_id: None,
                    part_item_id: Some("TOWLINE-6FT".into()),
                    qty_per_unit: Decimal::ONE,
                    unit: SheetLineUnit::Ea,
                    note: None,
                },
            ],
        }
    }

    fn rescue_tube_materials() -> HashMap<String, Material> {
        let mut map = HashMap::new();
        map.insert("FOAM-XLPE-RED".to_string(), material("FOAM-XLPE-RED", 404));
        map.insert("TOWLINE-6FT".to_string(), material("TOWLINE-6FT", 620));
        map
    }

    #[test]
    fn yield_divides_rather_than_subtracts() {
        let sheet = rescue_tube_sheet();
        let materials = rescue_tube_materials();
        let cost = standard_cost(&sheet, &materials).expect("costs");
        // 2.6 / 0.78 = 3.3333... bf bought for 2.6 bf shipped.
        assert!(cost.material_gross_bf > Decimal::new(333, 2));
        assert!(cost.material_gross_bf < Decimal::new(334, 2));
        // Buying more than you ship means gross bf strictly exceeds the
        // line's own qty_per_unit — the waste LEDGER-DESIGN insists is a
        // column, not a footnote.
        assert!(cost.material_gross_bf > Decimal::new(26, 1));
    }

    #[test]
    fn standard_cost_prices_at_the_materials_current_average() {
        let sheet = rescue_tube_sheet();
        let materials = rescue_tube_materials();
        let cost = standard_cost(&sheet, &materials).expect("costs");
        // material = 3.3333 bf * $4.04/bf = $13.4667 -> rounds to $13.47.
        assert_eq!(cost.material, Money::from_minor(1347));
        // labour = 6/60 hr * $48/hr = $4.80.
        assert_eq!(cost.labour, Money::from_minor(480));
        assert_eq!(cost.overhead, Money::from_minor(150));
        // total = material + labour + overhead + towline ($6.20).
        assert_eq!(cost.total, Money::from_minor(1347 + 480 + 150 + 620));
        assert_eq!(cost.per_unit, cost.total);

        // Reprice the foam and nothing else changes by hand: the same sheet,
        // read again, costs differently.
        let mut repriced = materials.clone();
        repriced.insert("FOAM-XLPE-RED".to_string(), material("FOAM-XLPE-RED", 500));
        let recost = standard_cost(&sheet, &repriced).expect("costs");
        assert!(recost.material > cost.material);
        assert_eq!(recost.labour, cost.labour);
    }

    #[test]
    fn a_sheet_with_no_lines_is_rejected() {
        let mut sheet = rescue_tube_sheet();
        sheet.lines.clear();
        let err = standard_cost(&sheet, &rescue_tube_materials()).unwrap_err();
        assert!(matches!(err, MfgError::EmptySheet(_)));
    }

    #[test]
    fn five_points_of_yield_beats_a_five_percent_foam_discount() {
        let sheet = rescue_tube_sheet();
        let materials = rescue_tube_materials();
        let sell_price = Money::from_minor(4995);
        let scenarios = sensitivity(&sheet, &materials, sell_price).expect("sensitivity");

        let foam_discount = scenarios
            .iter()
            .find(|s| s.name == "Foam price -5%")
            .expect("foam scenario");
        let yield_gain = scenarios
            .iter()
            .find(|s| s.name == "Yield +5 points")
            .expect("yield scenario");

        // Both scenarios improve margin (the delta is positive)...
        assert!(foam_discount.delta_vs_base > Decimal::ZERO);
        assert!(yield_gain.delta_vs_base > Decimal::ZERO);
        // ...but on this recipe, yield is the bigger lever.
        assert!(
            yield_gain.delta_vs_base > foam_discount.delta_vs_base,
            "yield {} should beat foam {}",
            yield_gain.delta_vs_base,
            foam_discount.delta_vs_base
        );
    }

    #[test]
    fn sensitivity_returns_all_six_scenarios() {
        let sheet = rescue_tube_sheet();
        let materials = rescue_tube_materials();
        let scenarios = sensitivity(&sheet, &materials, Money::from_minor(4995)).expect("ok");
        assert_eq!(scenarios.len(), 6);
    }
}
