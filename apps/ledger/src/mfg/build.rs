//! Builds, variance, and the inventory-adjustment row. `LEDGER-DESIGN.md` §11
//! "Builds and variance", `DECISIONS.md` D20 (W4), §1's Build/assembly, Build
//! variance and Inventory adjustment rows.
//!
//! A build prices two things against the same sheet: what it *should* have
//! cost (standard, from [`crate::mfg::sheet::standard_cost`]) and what the
//! material actually consumed cost, at each material's current average. The
//! difference is the plug to 5100 — a build that consumed more than the
//! sheet allowed debits it (a cost), a good nest credits it (W4: the plug is
//! never spread back over unit cost, so it stays a signal rather than
//! getting averaged away). Labour and overhead post at standard only,
//! because what varies build to build, the thing worth watching, is the
//! nest — "one overrun is noise; the same product every time is a recipe
//! that is lying to you."

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::chart;
use crate::post::{self, Posting};
use crate::store::{CommandMeta, Ledger};
use crate::types::{
    AccountId, ClassId, DocKind, DocLine, LedgerDocument, LineKind, PostingContext,
};

use super::sheet::{self, StandardCost};
use super::MfgError;

fn to_decimal(money: Money) -> Decimal {
    Decimal::new(money.minor(), 2)
}

fn account_line(line_no: i64, account: &str, amount: Money) -> DocLine {
    DocLine {
        line_no,
        kind: LineKind::Account,
        amount,
        class: None,
        item_id: None,
        account: Some(AccountId(account.to_string())),
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

/// A build to post (`ledger mfg build --file`).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct NewBuild {
    pub sheet_id: String,
    pub qty_built: Decimal,
    /// `(material_id, qty actually consumed)`, one entry per material this
    /// build drew from. Every id here must be a component of `sheet_id`
    /// ([`MfgError::ConsumptionNotOnSheet`]) — a build cannot consume a
    /// material its own recipe never named.
    pub consumptions: Vec<(String, Decimal)>,
    #[serde(default)]
    pub labour_minutes_actual: Decimal,
    pub completed_on: NaiveDate,
    #[serde(default)]
    pub started_on: Option<NaiveDate>,
    /// §1's Notes column: a Build's class is carried "from the finished
    /// item". This ledger has no item table to derive that from (see
    /// `crate::mfg` module docs), so it is supplied explicitly.
    pub class: ClassId,
    #[serde(default)]
    pub note: Option<String>,
}

/// What [`complete_build`] priced and posted.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildResult {
    pub build_id: String,
    pub entry_id: String,
    /// Dr 1310: `qty_built × sheet.standard_cost.per_unit`.
    pub standard_cost: Money,
    /// Cr 1300: consumptions priced at each material's current average.
    pub actual_material: Money,
    /// Cr 5300: `standard_cost - (standard material extended for qty_built)`
    /// — labour, overhead and bought parts, at standard, for the quantity
    /// built.
    pub applied: Money,
    /// The 5100 plug: positive credits it (consumed less than standard),
    /// negative debits it (an overrun).
    pub variance: Money,
    /// Recorded for comparison against standard labour; no journal line of
    /// its own (W4: only the material side of a build is measured at
    /// actual).
    pub actual_labour: Money,
}

/// §1 Build/assembly + Build variance, in one entry: Dr 1310 at standard, Cr
/// 1300 for board feet actually consumed (priced at each material's current
/// average — W12), Cr 5300 for labour/overhead/parts at standard, and the
/// difference plugged to 5100 (W4).
pub fn complete_build(
    ledger: &Ledger,
    company: &str,
    build: NewBuild,
    now: DateTime<Utc>,
) -> Result<BuildResult, MfgError> {
    let sheet_full = ledger.build_sheet(company, &build.sheet_id)?;

    let mut materials = HashMap::new();
    for line in &sheet_full.lines {
        if let Some(id) = line.component_id() {
            if !materials.contains_key(id) {
                materials.insert(id.to_string(), ledger.material(company, id)?);
            }
        }
    }
    for (material_id, _) in &build.consumptions {
        if !materials.contains_key(material_id) {
            return Err(MfgError::ConsumptionNotOnSheet(material_id.clone()));
        }
    }

    let standard: StandardCost = sheet::standard_cost(&sheet_full, &materials)?;

    let finished = round_money(
        build.qty_built * to_decimal(standard.total),
        RoundingPolicy::LineExtension,
    )?;
    let material_extended = round_money(
        build.qty_built * to_decimal(standard.material),
        RoundingPolicy::LineExtension,
    )?;
    // Derived as a remainder, not extended independently, so
    // `finished == material_extended + applied` exactly — the entry balances
    // by construction rather than by hoping two roundings agree.
    let applied = finished.checked_sub(material_extended)?;

    let mut actual_material = Money::ZERO;
    let mut consumptions = Vec::with_capacity(build.consumptions.len());
    for (material_id, qty_actual) in &build.consumptions {
        let material = &materials[material_id];
        let cost = round_money(
            *qty_actual * to_decimal(material.avg_cost_minor_per_unit),
            RoundingPolicy::LineExtension,
        )?;
        actual_material = actual_material.checked_add(cost)?;
        consumptions.push((material_id.clone(), *qty_actual, cost));
    }

    // = material_extended - actual_material, since applied cancels: the
    // whole plug is a material-side (board-foot) number, exactly what §11
    // means by "what was actually cut against what the sheet said".
    let variance = finished
        .checked_sub(actual_material)?
        .checked_sub(applied)?;

    let actual_labour = round_money(
        build.labour_minutes_actual / Decimal::from(60)
            * to_decimal(sheet_full.header.labour_rate_minor_per_hour),
        RoundingPolicy::LineExtension,
    )?;

    // §1, documented as the Settlement row documents its own convention:
    // three `Account`-kind lines carry the amounts `post::post` reads
    // straight off (1310 debited, 1300 and 5300 credited); the variance leg
    // is `post`'s own plug, computed from those three rather than passed in,
    // so the stored `builds.variance_minor` and the posted 5100 amount can
    // never drift apart.
    let doc = LedgerDocument {
        document_id: format!("build-{}", Uuid::now_v7()),
        kind: DocKind::Build,
        number: None,
        txn_date: build.completed_on,
        due_date: None,
        contact: None,
        header_class: Some(build.class.clone()),
        lines: vec![
            account_line(1, chart::INVENTORY_FINISHED, finished),
            account_line(2, chart::INVENTORY_RAW, actual_material),
            account_line(3, chart::PARTS_LABOUR_APPLIED, applied),
        ],
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: build
            .note
            .clone()
            .or_else(|| Some(format!("build against {}", build.sheet_id))),
    };

    let version = ledger.save_document(
        company,
        &doc,
        CommandMeta {
            actor_id: "dan".to_string(),
            kind: "complete_build".to_string(),
            hlc: now.to_rfc3339(),
        },
        now,
    )?;
    let entry = match post::post(&doc, version.version, &PostingContext::default())? {
        Posting::Entry(entry) => entry,
        Posting::NonPosting => unreachable!("Build always posts"),
    };
    let entry_id = ledger.post_entry(company, &entry, now)?;

    for (material_id, qty_actual, cost) in &consumptions {
        let material = &materials[material_id];
        let new_qty = material.qty_on_hand - qty_actual;
        // Relieve exactly `cost` — the same amount this build just credited
        // 1300 for — from the material's exact running value, never a
        // qty-times-average recomputation (`Material::total_value_minor`'s
        // doc), so `material_on_hand` keeps agreeing with 1300 to the cent.
        let new_value = material.total_value_minor.checked_sub(*cost)?;
        let new_avg = if new_qty.is_zero() {
            Money::ZERO
        } else {
            round_money(
                Decimal::new(new_value.minor(), 2) / new_qty,
                RoundingPolicy::LineExtension,
            )?
        };
        ledger.set_material_position(company, material_id, new_avg, new_qty, new_value)?;
    }

    let build_id = Uuid::now_v7().to_string();
    ledger.insert_build(
        company,
        &build_id,
        &build.sheet_id,
        &sheet_full.header.item_id,
        build.qty_built,
        build.started_on,
        build.completed_on,
        finished,
        actual_material,
        actual_labour,
        variance,
        Some(&entry_id),
        build.note.as_deref(),
    )?;
    for (i, (material_id, qty_actual, cost)) in consumptions.iter().enumerate() {
        ledger.insert_build_consumption(
            company,
            &build_id,
            i as i64 + 1,
            material_id,
            *qty_actual,
            *cost,
        )?;
    }

    Ok(BuildResult {
        build_id,
        entry_id,
        standard_cost: finished,
        actual_material,
        applied,
        variance,
        actual_labour,
    })
}

/// A physical count (`ledger mfg adjust`).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct NewAdjustment {
    /// `"1300"` or `"1310"` — raw material or finished goods.
    pub account: String,
    /// Signed: positive is a count **up** (Dr the inventory account, Cr
    /// 5150), negative a count **down** (Dr 5150, Cr the inventory account).
    pub amount: Money,
    /// §1: "required, from the item." No item table to derive it from here
    /// (see `crate::mfg` module docs) — supplied explicitly.
    pub class: ClassId,
    pub adjusted_on: NaiveDate,
    pub reason: String,
}

/// §1 Inventory adjustment: shrinkage, damage, a physical count, one reason
/// code per line.
pub fn adjust_inventory(
    ledger: &Ledger,
    company: &str,
    adjustment: NewAdjustment,
    now: DateTime<Utc>,
) -> Result<String, MfgError> {
    if adjustment.account != chart::INVENTORY_RAW && adjustment.account != chart::INVENTORY_FINISHED
    {
        return Err(MfgError::InvalidAdjustmentAccount(adjustment.account));
    }

    let doc = LedgerDocument {
        document_id: format!("adj-{}", Uuid::now_v7()),
        kind: DocKind::InventoryAdjustment,
        number: None,
        txn_date: adjustment.adjusted_on,
        due_date: None,
        contact: None,
        header_class: Some(adjustment.class),
        lines: vec![account_line(1, &adjustment.account, adjustment.amount)],
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: Some(adjustment.reason),
    };

    let version = ledger.save_document(
        company,
        &doc,
        CommandMeta {
            actor_id: "dan".to_string(),
            kind: "adjust_inventory".to_string(),
            hlc: now.to_rfc3339(),
        },
        now,
    )?;
    let entry = match post::post(&doc, version.version, &PostingContext::default())? {
        Posting::Entry(entry) => entry,
        Posting::NonPosting => unreachable!("InventoryAdjustment always posts"),
    };
    Ok(ledger.post_entry(company, &entry, now)?)
}

const BUILD_COLUMNS: &str = "build_id, sheet_id, item_id, qty_built, started_on, completed_on, \
     standard_cost_minor, actual_material_minor, actual_labour_minor, variance_minor, entry_id, note";

/// One row of `builds`, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildRow {
    pub build_id: String,
    pub sheet_id: String,
    pub item_id: String,
    pub qty_built: Decimal,
    pub started_on: Option<NaiveDate>,
    pub completed_on: NaiveDate,
    pub standard_cost: Money,
    pub actual_material: Money,
    pub actual_labour: Money,
    pub variance: Money,
    pub entry_id: Option<String>,
    pub note: Option<String>,
}

fn row_to_build(row: &rusqlite::Row<'_>) -> rusqlite::Result<BuildRow> {
    let qty_raw: String = row.get(3)?;
    let started_raw: Option<String> = row.get(4)?;
    let completed_raw: String = row.get(5)?;
    Ok(BuildRow {
        build_id: row.get(0)?,
        sheet_id: row.get(1)?,
        item_id: row.get(2)?,
        qty_built: qty_raw.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(3, "qty_built".into(), rusqlite::types::Type::Text)
        })?,
        started_on: started_raw
            .map(|raw| raw.parse())
            .transpose()
            .map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    4,
                    "started_on".into(),
                    rusqlite::types::Type::Text,
                )
            })?,
        completed_on: completed_raw.parse().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                5,
                "completed_on".into(),
                rusqlite::types::Type::Text,
            )
        })?,
        standard_cost: Money::from_minor(row.get(6)?),
        actual_material: Money::from_minor(row.get(7)?),
        actual_labour: Money::from_minor(row.get(8)?),
        variance: Money::from_minor(row.get(9)?),
        entry_id: row.get(10)?,
        note: row.get(11)?,
    })
}

impl Ledger {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_build(
        &self,
        company: &str,
        build_id: &str,
        sheet_id: &str,
        item_id: &str,
        qty_built: Decimal,
        started_on: Option<NaiveDate>,
        completed_on: NaiveDate,
        standard_cost: Money,
        actual_material: Money,
        actual_labour: Money,
        variance: Money,
        entry_id: Option<&str>,
        note: Option<&str>,
    ) -> Result<(), MfgError> {
        self.conn().execute(
            "INSERT INTO builds
                 (company_id, build_id, sheet_id, item_id, qty_built, started_on,
                  completed_on, standard_cost_minor, actual_material_minor,
                  actual_labour_minor, variance_minor, entry_id, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                company,
                build_id,
                sheet_id,
                item_id,
                super::decimal_to_text(qty_built),
                started_on.map(|d| d.to_string()),
                completed_on.to_string(),
                standard_cost.minor(),
                actual_material.minor(),
                actual_labour.minor(),
                variance.minor(),
                entry_id,
                note,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_build_consumption(
        &self,
        company: &str,
        build_id: &str,
        line_no: i64,
        material_id: &str,
        qty_actual: Decimal,
        cost: Money,
    ) -> Result<(), MfgError> {
        self.conn().execute(
            "INSERT INTO build_consumptions
                 (company_id, build_id, line_no, material_id, qty_actual, cost_minor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                company,
                build_id,
                line_no,
                material_id,
                super::decimal_to_text(qty_actual),
                cost.minor(),
            ],
        )?;
        Ok(())
    }

    pub fn list_builds(&self, company: &str) -> Result<Vec<BuildRow>, MfgError> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {BUILD_COLUMNS} FROM builds WHERE company_id = ?1 ORDER BY completed_on, build_id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![company], row_to_build)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mfg::landed::{receive_materials, RawMaterialReceipt, RawMaterialReceiptLine};
    use crate::mfg::sheet::{NewBuildSheet, NewBuildSheetLine};
    use crate::mfg::NewMaterial;

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
            .expect("material");
        receive_materials(
            &ledger,
            "aquamentor",
            RawMaterialReceipt {
                vendor_ref: Some("Continental Foam".into()),
                po_ref: Some("PO-1".into()),
                received_on: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
                lines: vec![RawMaterialReceiptLine {
                    material_id: "FOAM-XLPE-RED".into(),
                    qty: Decimal::from(200),
                    base_cost: Money::from_minor(80000),
                }],
                freight: Money::from_minor(0),
            },
            Utc::now(),
        )
        .expect("receipt posts");
        // avg cost is now 80000/200 = 400 cents/bf.

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
                    overhead_minor: Money::from_minor(0),
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
            .expect("sheet");
        ledger
    }

    #[test]
    fn a_build_that_overruns_debits_variance() {
        let ledger = setup();
        // Standard bf for 1 unit: 2.6/0.78 = 3.3333 bf, at 400c/bf = $13.33.
        let result = complete_build(
            &ledger,
            "aquamentor",
            NewBuild {
                sheet_id: "XRT-50-STD".into(),
                qty_built: Decimal::ONE,
                consumptions: vec![("FOAM-XLPE-RED".into(), Decimal::from(4))], // more than standard
                labour_minutes_actual: Decimal::from(7),
                completed_on: NaiveDate::from_ymd_opt(2026, 8, 19).unwrap(),
                started_on: None,
                class: ClassId("foam".into()),
                note: Some("BLD-0001".into()),
            },
            Utc::now(),
        )
        .expect("build posts");

        assert!(result.variance.is_negative(), "an overrun debits 5100");
        assert_eq!(result.actual_material, Money::from_minor(1600)); // 4 bf * 400c
        assert!(result.standard_cost.minor() > 0);

        let material = ledger
            .material("aquamentor", "FOAM-XLPE-RED")
            .expect("material");
        assert_eq!(material.qty_on_hand, Decimal::from(196)); // 200 - 4
    }

    #[test]
    fn exact_consumption_posts_zero_variance() {
        let ledger = setup();
        let standard_bf = Decimal::new(26, 1) / Decimal::new(78, 2); // 3.333...

        let result = complete_build(
            &ledger,
            "aquamentor",
            NewBuild {
                sheet_id: "XRT-50-STD".into(),
                qty_built: Decimal::ONE,
                consumptions: vec![("FOAM-XLPE-RED".into(), standard_bf)],
                labour_minutes_actual: Decimal::from(6),
                completed_on: NaiveDate::from_ymd_opt(2026, 8, 19).unwrap(),
                started_on: None,
                class: ClassId("foam".into()),
                note: None,
            },
            Utc::now(),
        )
        .expect("build posts");

        assert_eq!(result.variance, Money::ZERO);
    }

    #[test]
    fn a_consumption_not_on_the_sheet_is_rejected() {
        let ledger = setup();
        let err = complete_build(
            &ledger,
            "aquamentor",
            NewBuild {
                sheet_id: "XRT-50-STD".into(),
                qty_built: Decimal::ONE,
                consumptions: vec![("NOT-A-COMPONENT".into(), Decimal::ONE)],
                labour_minutes_actual: Decimal::from(6),
                completed_on: NaiveDate::from_ymd_opt(2026, 8, 19).unwrap(),
                started_on: None,
                class: ClassId("foam".into()),
                note: None,
            },
            Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, MfgError::ConsumptionNotOnSheet(_)));
    }

    #[test]
    fn an_inventory_adjustment_posts_to_5150() {
        let ledger = setup();
        let entry_id = adjust_inventory(
            &ledger,
            "aquamentor",
            NewAdjustment {
                account: chart::INVENTORY_RAW.to_string(),
                amount: Money::from_minor(-500),
                class: ClassId("foam".into()),
                adjusted_on: NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
                reason: "August physical count, foam room".into(),
            },
            Utc::now(),
        )
        .expect("adjustment posts");
        let (entry, _posted) = ledger.entry("aquamentor", &entry_id).expect("entry");
        assert!(entry.is_balanced());
        let shrink = entry
            .lines
            .iter()
            .find(|l| l.account.0 == chart::INVENTORY_ADJUSTMENT)
            .expect("5150 line");
        assert_eq!(shrink.debit, Money::from_minor(500));
    }

    #[test]
    fn an_invalid_adjustment_account_is_rejected() {
        let ledger = setup();
        let err = adjust_inventory(
            &ledger,
            "aquamentor",
            NewAdjustment {
                account: chart::CHECKING.to_string(),
                amount: Money::from_minor(500),
                class: ClassId("foam".into()),
                adjusted_on: NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
                reason: "nonsense".into(),
            },
            Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, MfgError::InvalidAdjustmentAccount(_)));
    }
}
