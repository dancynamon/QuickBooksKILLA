//! Manufacturing costing. `LEDGER-DESIGN.md` §11, `DECISIONS.md` D20 (W2, W3,
//! W4, W12), `prototype/bunzbooks.html`'s manufacturing block.
//!
//! Built after cutover (ROADMAP §H), designed here because §1's posting-rules
//! table already names the three rows it produces (`RawMaterialReceipt`,
//! `Build`, `InventoryAdjustment`). The idea, straight from the prototype:
//! cost flows from the sheet to the finished product automatically, so moving
//! a foam price re-prices every product that uses it.
//!
//! - [`landed`]  — a sheet's true cost is base plus freight, freight
//!   allocated **by board foot** (not by line count, not by value), and the
//!   moving weighted average (W12) that follows from it.
//! - [`sheet`]   — build sheets, where **yield divides rather than
//!   subtracts**, and the sensitivity a foam price move or a yield point is
//!   worth.
//! - [`build`]   — what was actually cut against what the sheet said, priced,
//!   with the variance plugged to 5100 (W4) rather than smeared into unit
//!   cost; and the inventory-adjustment row (a physical count).
//! - [`report`]  — variance by item (one overrun is noise, the same product
//!   every time is a recipe that is lying), material on hand valued at
//!   average (must agree with 1300 to the cent), and a cost rollup by item.
//!
//! This module owns six tables (`materials`, `material_lots`, `build_sheets`,
//! `build_sheet_lines`, `builds`, `build_consumptions`; `store.rs` migration
//! 5) and, like [`crate::bank`], adds its accessors as `impl Ledger` blocks in
//! its own files using [`crate::store::Ledger::conn`] rather than growing
//! `store.rs` itself — the same discipline this crate already keeps.
//!
//! **Why classes are explicit inputs here, not looked up.** §1's Notes column
//! says a Build's class comes "from the finished item" and an
//! `InventoryAdjustment`'s is "required, from the item". This ledger has no
//! item catalogue table of its own (`PostingContext.items` is assembled
//! externally, from the QBO replica, for [`crate::import`]); there is nothing
//! in this database to look the item's default class up in. Callers supply
//! it explicitly ([`build::NewBuild::class`], [`build::NewAdjustment::class`])
//! until an item table exists to derive it from.

pub mod build;
pub mod landed;
pub mod report;
pub mod sheet;

use std::str::FromStr;

use rusqlite::{params, OptionalExtension, Row};
use rust_decimal::Decimal;

use ledger_core::Money;

use crate::store::{Ledger, LedgerError};

#[derive(Debug, thiserror::Error)]
pub enum MfgError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Money(#[from] ledger_core::MoneyError),
    #[error(transparent)]
    Post(#[from] crate::post::PostError),
    #[error("unknown material {0}")]
    UnknownMaterial(String),
    #[error("unknown build sheet {0}")]
    UnknownSheet(String),
    #[error("build sheet {0} has no lines")]
    EmptySheet(String),
    #[error("receipt has no lines")]
    EmptyReceipt,
    #[error("build sheet line {sheet_id}#{line_no} names neither a material nor a bought part")]
    LineNamesNothing { sheet_id: String, line_no: i64 },
    #[error("build named a material {0} that is not a component of its sheet")]
    ConsumptionNotOnSheet(String),
    #[error("{0} is not 1300 or 1310: an inventory adjustment moves one of those two")]
    InvalidAdjustmentAccount(String),
    #[error("stored value {value:?} in {field} is not a valid decimal")]
    BadDecimal { field: &'static str, value: String },
    #[error("material {0} has unit 'sheet' but no sheet_board_feet on file")]
    MissingBoardFeet(String),
}

/// One of the four units a material is bought and stocked in (`materials.unit`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MaterialUnit {
    /// Board feet: the material is already tracked the way it is consumed.
    Bf,
    /// Whole sheets: [`Material::sheet_board_feet`] converts to board feet
    /// for freight allocation (§11 "by board foot").
    Sheet,
    /// Each: hardware, a bought part.
    Ea,
    /// Linear feet: webbing, strapping.
    Lf,
}

impl MaterialUnit {
    pub const fn as_str(self) -> &'static str {
        match self {
            MaterialUnit::Bf => "bf",
            MaterialUnit::Sheet => "sheet",
            MaterialUnit::Ea => "ea",
            MaterialUnit::Lf => "lf",
        }
    }

    pub fn parse(raw: &str) -> Option<MaterialUnit> {
        match raw {
            "bf" => Some(MaterialUnit::Bf),
            "sheet" => Some(MaterialUnit::Sheet),
            "ea" => Some(MaterialUnit::Ea),
            "lf" => Some(MaterialUnit::Lf),
            _ => None,
        }
    }
}

/// One row of `materials`. `unit` says how the material is **bought**; a
/// `bf` or `sheet` material is always **tracked and costed by the board
/// foot** regardless (`qty_on_hand` and `avg_cost_minor_per_unit` are board
/// feet and $/bf for both), because that is the one number every build sheet
/// reads (§11). `sheet_board_feet` is the sheet-to-board-foot conversion a
/// `sheet` material's receipts go through on the way in
/// ([`landed::receive_materials`]); an `ea`/`lf` material tracks and costs
/// in its own unit and never touches board feet at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub material_id: String,
    pub name: String,
    pub unit: MaterialUnit,
    /// Board feet per sheet. `Some` only for a `unit: Sheet` material —
    /// required there ([`MfgError::MissingBoardFeet`]), meaningless for the
    /// other three units.
    pub sheet_board_feet: Option<Decimal>,
    /// Moving weighted average (W12), `round(total_value_minor / qty_on_hand)`:
    /// $/bf for `bf`/`sheet`, $/each for `ea`, $/linear-foot for `lf`. Used
    /// to price a build sheet and a build's consumption — a display and
    /// costing figure, not what [`report::material_on_hand`] values on
    /// hand at (that reads `total_value_minor` directly, see its doc).
    pub avg_cost_minor_per_unit: Money,
    /// Board feet on hand for `bf`/`sheet`, native units for `ea`/`lf`.
    pub qty_on_hand: Decimal,
    /// The exact running value: every receipt's landed cost added, every
    /// build's consumption cost relieved, in the same minor units posted to
    /// 1300. `avg_cost_minor_per_unit` rounds every time it is recomputed;
    /// this never does, which is what lets `material_on_hand` agree with
    /// 1300 to the cent regardless of how many receipts and builds came
    /// before it.
    pub total_value_minor: Money,
    pub is_active: bool,
    pub source_ref: Option<String>,
}

/// A new material (`ledger mfg material add`).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct NewMaterial {
    pub material_id: String,
    pub name: String,
    pub unit: String,
    #[serde(default)]
    pub sheet_board_feet: Option<Decimal>,
    #[serde(default)]
    pub source_ref: Option<String>,
}

pub(crate) fn decimal_to_text(value: Decimal) -> String {
    value.normalize().to_string()
}

fn row_to_material(row: &Row<'_>) -> rusqlite::Result<Material> {
    let unit_raw: String = row.get(2)?;
    let unit = MaterialUnit::parse(&unit_raw).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(2, "unit".into(), rusqlite::types::Type::Text)
    })?;
    let sheet_board_feet: Option<String> = row.get(3)?;
    let sheet_board_feet = sheet_board_feet
        .map(|raw| {
            Decimal::from_str(&raw).map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    3,
                    "sheet_board_feet".into(),
                    rusqlite::types::Type::Text,
                )
            })
        })
        .transpose()?;
    let qty_raw: String = row.get(5)?;
    let qty_on_hand = Decimal::from_str(&qty_raw).map_err(|_| {
        rusqlite::Error::InvalidColumnType(5, "qty_on_hand".into(), rusqlite::types::Type::Text)
    })?;
    Ok(Material {
        material_id: row.get(0)?,
        name: row.get(1)?,
        unit,
        sheet_board_feet,
        avg_cost_minor_per_unit: Money::from_minor(row.get(4)?),
        qty_on_hand,
        total_value_minor: Money::from_minor(row.get(6)?),
        is_active: row.get::<_, i64>(7)? != 0,
        source_ref: row.get(8)?,
    })
}

const MATERIAL_COLUMNS: &str = "material_id, name, unit, sheet_board_feet, \
     avg_cost_minor_per_unit, qty_on_hand, total_value_minor, is_active, source_ref";

impl Ledger {
    /// `ledger mfg material add`: inserts a material at zero cost and zero
    /// on hand — it starts to mean something the first time it is received
    /// ([`landed::receive_materials`]).
    pub fn add_material(&self, company: &str, new: &NewMaterial) -> Result<(), MfgError> {
        let unit = MaterialUnit::parse(&new.unit).ok_or_else(|| MfgError::BadDecimal {
            field: "unit",
            value: new.unit.clone(),
        })?;
        if unit == MaterialUnit::Sheet && new.sheet_board_feet.is_none() {
            return Err(MfgError::MissingBoardFeet(new.material_id.clone()));
        }
        self.conn().execute(
            "INSERT INTO materials
                 (company_id, material_id, name, unit, sheet_board_feet,
                  avg_cost_minor_per_unit, qty_on_hand, total_value_minor, is_active, source_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, '0', 0, 1, ?6)",
            params![
                company,
                new.material_id,
                new.name,
                unit.as_str(),
                new.sheet_board_feet.map(decimal_to_text),
                new.source_ref,
            ],
        )?;
        Ok(())
    }

    pub fn material(&self, company: &str, material_id: &str) -> Result<Material, MfgError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {MATERIAL_COLUMNS} FROM materials \
                     WHERE company_id = ?1 AND material_id = ?2"
                ),
                params![company, material_id],
                row_to_material,
            )
            .optional()?
            .ok_or_else(|| MfgError::UnknownMaterial(material_id.to_string()))
    }

    pub fn list_materials(&self, company: &str) -> Result<Vec<Material>, MfgError> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {MATERIAL_COLUMNS} FROM materials \
             WHERE company_id = ?1 ORDER BY material_id"
        ))?;
        let rows = stmt
            .query_map(params![company], row_to_material)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Writes back a material's moving average, quantity and exact running
    /// value after a receipt or a build consumes it (W12). `total_value`
    /// must be the caller's own running total (old value plus a receipt's
    /// exact landed cost, or minus a consumption's exact relieved cost) —
    /// never re-derived from `qty_on_hand * avg_cost_minor_per_unit`, which
    /// is exactly the rounding [`Material::total_value_minor`]'s doc
    /// explains this column exists to avoid. Internal to [`landed`] and
    /// [`build`].
    pub(crate) fn set_material_position(
        &self,
        company: &str,
        material_id: &str,
        avg_cost_minor_per_unit: Money,
        qty_on_hand: Decimal,
        total_value: Money,
    ) -> Result<(), MfgError> {
        self.conn().execute(
            "UPDATE materials \
             SET avg_cost_minor_per_unit = ?1, qty_on_hand = ?2, total_value_minor = ?3 \
             WHERE company_id = ?4 AND material_id = ?5",
            params![
                avg_cost_minor_per_unit.minor(),
                decimal_to_text(qty_on_hand),
                total_value.minor(),
                company,
                material_id,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Ledger;

    #[test]
    fn add_list_and_fetch_a_material() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .create_company("aquamentor", "Aquamentor", None, chrono::Utc::now())
            .expect("company");
        ledger
            .add_material(
                "aquamentor",
                &NewMaterial {
                    material_id: "FOAM-XLPE-BLU".into(),
                    name: "XLPE 2lb sheet, blue, 4x8x2\"".into(),
                    unit: "sheet".into(),
                    sheet_board_feet: Some(Decimal::from(64)),
                    source_ref: None,
                },
            )
            .expect("add material");

        let listed = ledger.list_materials("aquamentor").expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].material_id, "FOAM-XLPE-BLU");
        assert_eq!(listed[0].qty_on_hand, Decimal::ZERO);
        assert_eq!(listed[0].avg_cost_minor_per_unit, Money::ZERO);

        let fetched = ledger
            .material("aquamentor", "FOAM-XLPE-BLU")
            .expect("fetch");
        assert_eq!(fetched.sheet_board_feet, Some(Decimal::from(64)));
    }

    #[test]
    fn sheet_material_without_board_feet_is_rejected() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .create_company("aquamentor", "Aquamentor", None, chrono::Utc::now())
            .expect("company");
        let err = ledger
            .add_material(
                "aquamentor",
                &NewMaterial {
                    material_id: "FOAM-XLPE-BLU".into(),
                    name: "sheet with no bf".into(),
                    unit: "sheet".into(),
                    sheet_board_feet: None,
                    source_ref: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, MfgError::MissingBoardFeet(_)));
    }
}
