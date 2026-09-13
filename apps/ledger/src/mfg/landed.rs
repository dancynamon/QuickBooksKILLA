//! Landed cost. `LEDGER-DESIGN.md` §11 "Landed cost", `DECISIONS.md` D20 (W3,
//! W12), §1's Raw material receipt row.
//!
//! A sheet costs what it cost plus the freight that brought it, and freight
//! on a mixed pallet is allocated **by board foot**, never evenly across
//! lines and never by value: splitting a mixed pallet evenly makes the
//! cheaper material look dearer and every margin downstream inherits the
//! error. Each receipt then recomputes the material's moving weighted
//! average (W12): `(old_qty × old_avg + qty × landed_unit) / (old_qty +
//! qty)`, rounded as a line extension.

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::chart;
use crate::post::{self, Posting};
use crate::store::{CommandMeta, Ledger};
use crate::types::{DocKind, DocLine, LedgerDocument, LineKind, PostingContext};

use super::{Material, MaterialUnit, MfgError};

/// One line of a freight allocation, everything [`allocate_freight`] needs
/// and nothing it has to look up — kept pure so it is trivial to test.
#[derive(Clone, Debug)]
pub struct ReceiptLine {
    pub material_id: String,
    /// Quantity received, in the material's own [`MaterialUnit`].
    pub qty: Decimal,
    pub unit: MaterialUnit,
    /// Required (and used) only when `unit` is [`MaterialUnit::Sheet`].
    pub sheet_board_feet: Option<Decimal>,
    pub base_cost: Money,
}

fn board_foot_weight(line: &ReceiptLine) -> Result<Decimal, MfgError> {
    match line.unit {
        MaterialUnit::Bf => Ok(line.qty),
        MaterialUnit::Sheet => line
            .sheet_board_feet
            .map(|bf_per_sheet| line.qty * bf_per_sheet)
            .ok_or_else(|| MfgError::MissingBoardFeet(line.material_id.clone())),
        // §11: an `ea`/`lf` line carries no board-foot weight of its own.
        // Whether it ends up allocated at all is decided by the caller: by
        // its own unit count when nothing on the receipt has board feet,
        // zero otherwise (a bag of hardware does not absorb foam's freight).
        MaterialUnit::Ea | MaterialUnit::Lf => Ok(Decimal::ZERO),
    }
}

/// How much a received quantity adds to `qty_on_hand`, in the unit that
/// material's average cost is itself expressed in. A `bf`-unit material
/// already speaks board feet; a `sheet`-unit material is bought and counted
/// by the sheet but **tracked and costed by the board foot**, so its receipt
/// quantity converts through [`Material::sheet_board_feet`] here, once, and
/// every material this system ever prices reads a single per-board-foot (or
/// per-each, or per-linear-foot) number rather than a mix of sheets and board
/// feet (the LEDGER-DESIGN §11 goal: "the single number every build sheet
/// reads"). `ea`/`lf` materials need no conversion at all.
fn tracked_qty(material: &Material, qty: Decimal) -> Result<Decimal, MfgError> {
    match material.unit {
        MaterialUnit::Bf | MaterialUnit::Ea | MaterialUnit::Lf => Ok(qty),
        MaterialUnit::Sheet => material
            .sheet_board_feet
            .map(|bf_per_sheet| qty * bf_per_sheet)
            .ok_or_else(|| MfgError::MissingBoardFeet(material.material_id.clone())),
    }
}

/// §11: freight allocated by board foot. `ea`/`lf` lines allocate by their
/// own unit count only when the receipt has no board-foot-bearing line at
/// all; otherwise they get zero, documented above and in
/// [`board_foot_weight`]. Largest-remainder rounding: every line's share is
/// floored to the cent, then the leftover cents (there are at most one per
/// line) go one each to the lines with the largest fractional remainder, so
/// the allocations sum to exactly `freight` — never a cent more, never a
/// cent short — regardless of how many lines share it.
///
/// Returns one `(line_no, freight_alloc)` pair per line, `line_no` 1-based in
/// `lines`' order. `freight` must not be negative (a receipt's freight is a
/// cost, never a credit; a freight credit is a [`crate::types::DocKind::VendorCredit`]).
pub fn allocate_freight(
    lines: &[ReceiptLine],
    freight: Money,
) -> Result<Vec<(usize, Money)>, MfgError> {
    if lines.is_empty() {
        return Ok(Vec::new());
    }
    if freight.is_zero() {
        return Ok((1..=lines.len()).map(|no| (no, Money::ZERO)).collect());
    }
    debug_assert!(!freight.is_negative(), "freight is a cost, never a credit");

    let mut weights: Vec<Decimal> = lines
        .iter()
        .map(board_foot_weight)
        .collect::<Result<_, _>>()?;
    let has_board_feet = weights.iter().any(|w| *w > Decimal::ZERO);
    if !has_board_feet {
        // Nothing on the receipt has board feet at all (an all-hardware
        // receipt): fall back to each line's own unit count.
        weights = lines.iter().map(|line| line.qty).collect();
    }
    let total_weight: Decimal = weights.iter().sum();
    let weights: Vec<Decimal> = if total_weight > Decimal::ZERO {
        weights
    } else {
        // Every candidate weight was zero (e.g. a zero-quantity line): split
        // evenly rather than divide by zero.
        vec![Decimal::ONE; lines.len()]
    };
    let total_weight: Decimal = weights.iter().sum();

    let freight_minor = freight.minor();
    let mut floors = vec![0i64; lines.len()];
    let mut remainders: Vec<(usize, Decimal)> = Vec::with_capacity(lines.len());
    let mut allocated = 0i64;
    for (i, weight) in weights.iter().enumerate() {
        let share = Decimal::from(freight_minor) * weight / total_weight;
        let floor = share.trunc();
        let floor_minor: i64 = floor.to_i64().unwrap_or(0);
        floors[i] = floor_minor;
        allocated += floor_minor;
        remainders.push((i, share - floor));
    }

    let mut leftover = freight_minor - allocated;
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut cursor = 0;
    while leftover > 0 && !remainders.is_empty() {
        let (i, _) = remainders[cursor % remainders.len()];
        floors[i] += 1;
        leftover -= 1;
        cursor += 1;
    }

    Ok(floors
        .into_iter()
        .enumerate()
        .map(|(i, minor)| (i + 1, Money::from_minor(minor)))
        .collect())
}

/// One line of a raw material receipt (`ledger mfg receive --file`).
#[derive(Clone, Debug, Deserialize)]
pub struct RawMaterialReceiptLine {
    pub material_id: String,
    pub qty: Decimal,
    pub base_cost: Money,
}

/// A raw material receipt: §1's Raw material receipt row, posted through
/// [`receive_materials`].
#[derive(Clone, Debug, Deserialize)]
pub struct RawMaterialReceipt {
    #[serde(default)]
    pub vendor_ref: Option<String>,
    #[serde(default)]
    pub po_ref: Option<String>,
    pub received_on: NaiveDate,
    pub lines: Vec<RawMaterialReceiptLine>,
    #[serde(default)]
    pub freight: Money,
}

/// What each line landed at and what it did to that material's average.
#[derive(Clone, Debug, PartialEq)]
pub struct ReceivedLine {
    pub material_id: String,
    pub lot_id: String,
    pub freight_alloc: Money,
    pub landed_cost: Money,
    pub new_avg_cost_minor_per_unit: Money,
    pub new_qty_on_hand: Decimal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReceiptResult {
    pub entry_id: String,
    pub total_landed: Money,
    pub lines: Vec<ReceivedLine>,
}

/// §1 Raw material receipt: writes the lots, recomputes each material's
/// moving weighted average and quantity (W12), and posts Dr 1300 landed
/// cost, Cr 2050 inventory received not billed — goods on the floor before
/// the bill arrives are inventory, not nothing.
pub fn receive_materials(
    ledger: &Ledger,
    company: &str,
    receipt: RawMaterialReceipt,
    now: DateTime<Utc>,
) -> Result<ReceiptResult, MfgError> {
    if receipt.lines.is_empty() {
        return Err(MfgError::EmptyReceipt);
    }

    let materials: Vec<Material> = receipt
        .lines
        .iter()
        .map(|line| ledger.material(company, &line.material_id))
        .collect::<Result<_, _>>()?;

    let allocation_lines: Vec<ReceiptLine> = receipt
        .lines
        .iter()
        .zip(&materials)
        .map(|(line, material)| ReceiptLine {
            material_id: line.material_id.clone(),
            qty: line.qty,
            unit: material.unit,
            sheet_board_feet: material.sheet_board_feet,
            base_cost: line.base_cost,
        })
        .collect();
    let allocation = allocate_freight(&allocation_lines, receipt.freight)?;

    let mut received = Vec::with_capacity(receipt.lines.len());
    let mut total_landed = Money::ZERO;

    for ((line, material), (_line_no, freight_alloc)) in
        receipt.lines.iter().zip(&materials).zip(&allocation)
    {
        let landed_cost = line.base_cost.checked_add(*freight_alloc)?;
        total_landed = total_landed.checked_add(landed_cost)?;

        let qty_tracked = tracked_qty(material, line.qty)?;
        let old_qty = material.qty_on_hand;
        let new_qty = old_qty + qty_tracked;
        // The exact running value (§11's own single number every build sheet
        // reads starts here): this receipt's landed cost, added exactly,
        // never re-derived from a rounded average. `avg_cost_minor_per_unit`
        // is then just this value's rounded per-unit reading, for pricing.
        let new_value = material.total_value_minor.checked_add(landed_cost)?;
        let new_avg = if new_qty.is_zero() {
            Money::ZERO
        } else {
            round_money(
                Decimal::new(new_value.minor(), 2) / new_qty,
                RoundingPolicy::LineExtension,
            )?
        };

        ledger.set_material_position(
            company,
            &material.material_id,
            new_avg,
            new_qty,
            new_value,
        )?;

        let lot_id = Uuid::now_v7().to_string();
        ledger.insert_material_lot(
            company,
            &lot_id,
            &material.material_id,
            receipt.received_on,
            qty_tracked,
            line.base_cost,
            *freight_alloc,
            landed_cost,
            receipt.po_ref.as_deref(),
            receipt.vendor_ref.as_deref(),
        )?;

        received.push(ReceivedLine {
            material_id: material.material_id.clone(),
            lot_id,
            freight_alloc: *freight_alloc,
            landed_cost,
            new_avg_cost_minor_per_unit: new_avg,
            new_qty_on_hand: new_qty,
        });
    }

    // §1, documented as the Settlement row documents its own convention: the
    // two `Account`-kind lines below carry the amounts `post::post` reads
    // straight off — one naming 1300 (debited), one naming 2050 (credited),
    // both the same total, since this receipt is a single landed-cost
    // transfer rather than a multi-account allocation.
    let doc = LedgerDocument {
        document_id: format!("rmr-{}", Uuid::now_v7()),
        kind: DocKind::RawMaterialReceipt,
        number: None,
        txn_date: receipt.received_on,
        due_date: None,
        contact: None,
        header_class: None,
        lines: vec![
            account_line(1, chart::INVENTORY_RAW, total_landed),
            account_line(2, chart::INVENTORY_RECEIVED_NOT_BILLED, total_landed),
        ],
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: Some(format!(
            "raw material receipt{}",
            receipt
                .po_ref
                .as_deref()
                .map(|po| format!(", {po}"))
                .unwrap_or_default()
        )),
    };

    let version = ledger.save_document(
        company,
        &doc,
        CommandMeta {
            actor_id: "dan".to_string(),
            kind: "receive_materials".to_string(),
            hlc: now.to_rfc3339(),
        },
        now,
    )?;

    let ctx = PostingContext::default();
    let entry = match post::post(&doc, version.version, &ctx)? {
        Posting::Entry(entry) => entry,
        Posting::NonPosting => unreachable!("RawMaterialReceipt always posts"),
    };
    let entry_id = ledger.post_entry(company, &entry, now)?;

    Ok(ReceiptResult {
        entry_id,
        total_landed,
        lines: received,
    })
}

fn account_line(line_no: i64, account: &str, amount: Money) -> DocLine {
    DocLine {
        line_no,
        kind: LineKind::Account,
        amount,
        class: None,
        item_id: None,
        account: Some(crate::types::AccountId(account.to_string())),
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

impl Ledger {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_material_lot(
        &self,
        company: &str,
        lot_id: &str,
        material_id: &str,
        received_on: NaiveDate,
        qty: Decimal,
        base_cost: Money,
        freight_alloc: Money,
        landed_cost: Money,
        po_ref: Option<&str>,
        vendor_ref: Option<&str>,
    ) -> Result<(), MfgError> {
        self.conn().execute(
            "INSERT INTO material_lots
                 (company_id, lot_id, material_id, received_on, qty, base_cost_minor,
                  freight_alloc_minor, landed_cost_minor, po_ref, vendor_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                company,
                lot_id,
                material_id,
                received_on.to_string(),
                super::decimal_to_text(qty),
                base_cost.minor(),
                freight_alloc.minor(),
                landed_cost.minor(),
                po_ref,
                vendor_ref,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn bf_line(id: &str, qty: Decimal, base_cost_minor: i64) -> ReceiptLine {
        ReceiptLine {
            material_id: id.into(),
            qty,
            unit: MaterialUnit::Bf,
            sheet_board_feet: None,
            base_cost: Money::from_minor(base_cost_minor),
        }
    }

    #[test]
    fn allocation_sums_to_exactly_the_freight() {
        // Three odd quantities chosen so plain division does not land on
        // whole cents, which is exactly what largest-remainder exists for.
        let lines = vec![
            bf_line("a", dec!(7), 1000),
            bf_line("b", dec!(11), 1500),
            bf_line("c", dec!(3), 500),
        ];
        let allocation = allocate_freight(&lines, Money::from_minor(1001)).expect("allocates");
        let total: i64 = allocation.iter().map(|(_, m)| m.minor()).sum();
        assert_eq!(total, 1001);
        assert_eq!(allocation.len(), 3);
    }

    #[test]
    fn allocation_weights_by_board_foot_not_by_line_count() {
        // Two lines, one with ten times the board feet of the other: it
        // should get roughly ten times the freight, not half.
        let lines = vec![bf_line("a", dec!(100), 1), bf_line("b", dec!(10), 1)];
        let allocation = allocate_freight(&lines, Money::from_minor(1100)).expect("allocates");
        assert_eq!(allocation[0].1, Money::from_minor(1000));
        assert_eq!(allocation[1].1, Money::from_minor(100));
    }

    #[test]
    fn sheet_lines_weight_by_qty_times_board_feet_per_sheet() {
        let lines = vec![
            ReceiptLine {
                material_id: "blu".into(),
                qty: dec!(2),
                unit: MaterialUnit::Sheet,
                sheet_board_feet: Some(dec!(64)),
                base_cost: Money::from_minor(45600),
            },
            ReceiptLine {
                material_id: "red".into(),
                qty: dec!(1),
                unit: MaterialUnit::Sheet,
                sheet_board_feet: Some(dec!(32)),
                base_cost: Money::from_minor(24800),
            },
        ];
        // 128 bf vs 32 bf: 128/160 and 32/160 of the freight.
        let allocation = allocate_freight(&lines, Money::from_minor(1600)).expect("allocates");
        assert_eq!(allocation[0].1, Money::from_minor(1280));
        assert_eq!(allocation[1].1, Money::from_minor(320));
    }

    #[test]
    fn hardware_lines_get_zero_weight_when_a_board_foot_line_is_present() {
        let lines = vec![
            bf_line("foam", dec!(64), 22800),
            ReceiptLine {
                material_id: "hardware".into(),
                qty: dec!(500),
                unit: MaterialUnit::Ea,
                sheet_board_feet: None,
                base_cost: Money::from_minor(9000),
            },
        ];
        let allocation = allocate_freight(&lines, Money::from_minor(1080)).expect("allocates");
        assert_eq!(allocation[0].1, Money::from_minor(1080));
        assert_eq!(allocation[1].1, Money::ZERO);
    }

    #[test]
    fn an_all_hardware_receipt_falls_back_to_unit_count() {
        let lines = vec![
            ReceiptLine {
                material_id: "buckles".into(),
                qty: dec!(300),
                unit: MaterialUnit::Ea,
                sheet_board_feet: None,
                base_cost: Money::from_minor(9000),
            },
            ReceiptLine {
                material_id: "webbing".into(),
                qty: dec!(100),
                unit: MaterialUnit::Lf,
                sheet_board_feet: None,
                base_cost: Money::from_minor(4000),
            },
        ];
        let allocation = allocate_freight(&lines, Money::from_minor(800)).expect("allocates");
        assert_eq!(allocation[0].1, Money::from_minor(600));
        assert_eq!(allocation[1].1, Money::from_minor(200));
    }

    #[test]
    fn zero_freight_allocates_nothing_to_every_line() {
        let lines = vec![bf_line("a", dec!(10), 100), bf_line("b", dec!(20), 200)];
        let allocation = allocate_freight(&lines, Money::ZERO).expect("allocates");
        assert!(allocation.iter().all(|(_, m)| m.is_zero()));
    }

    #[test]
    fn a_sheet_line_with_no_board_feet_on_file_is_an_error() {
        let lines = vec![ReceiptLine {
            material_id: "mystery".into(),
            qty: dec!(2),
            unit: MaterialUnit::Sheet,
            sheet_board_feet: None,
            base_cost: Money::from_minor(1000),
        }];
        let err = allocate_freight(&lines, Money::from_minor(100)).unwrap_err();
        assert!(matches!(err, MfgError::MissingBoardFeet(_)));
    }

    fn setup(ledger: &Ledger) {
        ledger
            .create_company("aquamentor", "Aquamentor", None, Utc::now())
            .expect("company");
        ledger
            .add_material(
                "aquamentor",
                &super::super::NewMaterial {
                    material_id: "FOAM-XLPE-BLU".into(),
                    name: "XLPE 2lb sheet, blue".into(),
                    unit: "sheet".into(),
                    sheet_board_feet: Some(dec!(64)),
                    source_ref: None,
                },
            )
            .expect("add material");
    }

    #[test]
    fn receiving_twice_at_different_landed_costs_moves_the_average() {
        let ledger = Ledger::open_in_memory().expect("open");
        setup(&ledger);

        let first = receive_materials(
            &ledger,
            "aquamentor",
            RawMaterialReceipt {
                vendor_ref: Some("Continental Foam".into()),
                po_ref: Some("PO-1".into()),
                received_on: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
                lines: vec![RawMaterialReceiptLine {
                    material_id: "FOAM-XLPE-BLU".into(),
                    qty: dec!(2),                        // 2 sheets = 128 bf
                    base_cost: Money::from_minor(45600), // $228/sheet
                }],
                freight: Money::from_minor(2160), // $10.80/sheet
            },
            Utc::now(),
        )
        .expect("first receipt posts");
        // landed = 45600 + 2160 = 47760 over 128 bf = 373.125 cents/bf,
        // rounded half-away-from-zero to 373.
        assert_eq!(
            first.lines[0].new_avg_cost_minor_per_unit,
            Money::from_minor(373)
        );
        assert_eq!(first.lines[0].new_qty_on_hand, dec!(128));

        let second = receive_materials(
            &ledger,
            "aquamentor",
            RawMaterialReceipt {
                vendor_ref: Some("Continental Foam".into()),
                po_ref: Some("PO-2".into()),
                received_on: NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
                lines: vec![RawMaterialReceiptLine {
                    material_id: "FOAM-XLPE-BLU".into(),
                    qty: dec!(1), // 1 sheet = 64 bf, priced higher this time
                    base_cost: Money::from_minor(25000),
                }],
                freight: Money::from_minor(1200),
            },
            Utc::now(),
        )
        .expect("second receipt posts");

        // Second receipt: 64 bf landed at $262.00 = $4.09375/bf.
        // (128 * 3.73 + 64 * 4.09375) / 192 = 739.44 / 192 = 3.85125 -> $3.85/bf.
        let material = ledger
            .material("aquamentor", "FOAM-XLPE-BLU")
            .expect("material");
        assert_eq!(material.qty_on_hand, dec!(192));
        assert_eq!(second.lines[0].new_qty_on_hand, dec!(192));
        assert_eq!(material.avg_cost_minor_per_unit, Money::from_minor(385));
        assert_eq!(
            material.avg_cost_minor_per_unit,
            second.lines[0].new_avg_cost_minor_per_unit
        );
    }

    #[test]
    fn an_empty_receipt_is_rejected() {
        let ledger = Ledger::open_in_memory().expect("open");
        setup(&ledger);
        let err = receive_materials(
            &ledger,
            "aquamentor",
            RawMaterialReceipt {
                vendor_ref: None,
                po_ref: None,
                received_on: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
                lines: vec![],
                freight: Money::ZERO,
            },
            Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, MfgError::EmptyReceipt));
    }
}
