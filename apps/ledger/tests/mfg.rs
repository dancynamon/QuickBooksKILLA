//! Manufacturing costing, end to end. `LEDGER-DESIGN.md` §11.
//!
//! Two receipts with freight, a build sheet, a build that overruns its
//! recipe, a second build that consumes exactly what the sheet called for,
//! and a physical count — checked against the trial balance to the cent at
//! every step. Names and figures are invented.

use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;

use ledger::chart;
use ledger::mfg::build::{self, NewAdjustment, NewBuild};
use ledger::mfg::landed::{self, RawMaterialReceipt, RawMaterialReceiptLine};
use ledger::mfg::report;
use ledger::mfg::sheet::{self, NewBuildSheet, NewBuildSheetLine};
use ledger::mfg::NewMaterial;
use ledger::report::trial_balance;
use ledger::store::Ledger;
use ledger::types::ClassId;
use ledger_core::Money;

const COMPANY: &str = "aquamentor";

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn tb_balance(ledger: &Ledger, as_of: NaiveDate, account: &str) -> Money {
    let tb = trial_balance(ledger, COMPANY, as_of).expect("trial balance");
    tb.rows
        .iter()
        .find(|row| row.number == account)
        .map(|row| row.balance)
        .unwrap_or(Money::ZERO)
}

fn setup() -> Ledger {
    let ledger = Ledger::open_in_memory().expect("open");
    ledger
        .create_company(COMPANY, "Aquamentor", None, Utc::now())
        .expect("company");
    ledger
        .add_material(
            COMPANY,
            &NewMaterial {
                material_id: "FOAM-XLPE-BLU".into(),
                name: "XLPE 2lb sheet, blue, 4x8x2\"".into(),
                unit: "bf".into(),
                sheet_board_feet: None,
                source_ref: None,
            },
        )
        .expect("add foam");
    ledger
        .add_material(
            COMPANY,
            &NewMaterial {
                material_id: "TOWLINE-6FT".into(),
                name: "6ft towline, spliced".into(),
                unit: "ea".into(),
                sheet_board_feet: None,
                source_ref: None,
            },
        )
        .expect("add towline");
    ledger
}

#[test]
fn landed_cost_build_sheet_and_build_all_agree_to_the_cent() {
    let ledger = setup();

    // -- Two raw material receipts, each with freight -----------------
    let receipt_one = landed::receive_materials(
        &ledger,
        COMPANY,
        RawMaterialReceipt {
            vendor_ref: Some("Continental Foam".into()),
            po_ref: Some("PO-4469".into()),
            received_on: ymd(2026, 8, 1),
            lines: vec![RawMaterialReceiptLine {
                material_id: "FOAM-XLPE-BLU".into(),
                qty: Decimal::from(128),
                base_cost: Money::from_minor(45600),
            }],
            freight: Money::from_minor(2160),
        },
        Utc::now(),
    )
    .expect("first receipt posts");

    let receipt_two = landed::receive_materials(
        &ledger,
        COMPANY,
        RawMaterialReceipt {
            vendor_ref: Some("Continental Foam".into()),
            po_ref: Some("PO-4501".into()),
            received_on: ymd(2026, 8, 10),
            lines: vec![
                RawMaterialReceiptLine {
                    material_id: "FOAM-XLPE-BLU".into(),
                    qty: Decimal::from(64),
                    base_cost: Money::from_minor(25000),
                },
                // A hardware line on the same receipt: no board feet of its
                // own, so it takes none of this receipt's freight (§11).
                RawMaterialReceiptLine {
                    material_id: "TOWLINE-6FT".into(),
                    qty: Decimal::from(100),
                    base_cost: Money::from_minor(62000),
                },
            ],
            freight: Money::from_minor(1200),
        },
        Utc::now(),
    )
    .expect("second receipt posts");

    let total_landed = receipt_one
        .total_landed
        .checked_add(receipt_two.total_landed)
        .unwrap();
    assert_eq!(
        tb_balance(&ledger, ymd(2026, 8, 10), chart::INVENTORY_RAW),
        total_landed,
        "1300 is exactly the two receipts' landed cost, to the cent"
    );
    assert_eq!(
        tb_balance(&ledger, ymd(2026, 8, 10), chart::INVENTORY_RECEIVED_NOT_BILLED),
        total_landed.checked_neg().unwrap(),
        "2050 carries the same total as a credit balance"
    );

    let towline_avg = ledger
        .material(COMPANY, "TOWLINE-6FT")
        .expect("towline")
        .avg_cost_minor_per_unit;
    assert_eq!(towline_avg, Money::from_minor(620)); // $620.00 / 100 = $6.20 each, no freight.

    // -- A build sheet: yield-divided foam plus a bought towline --------
    ledger
        .add_build_sheet(
            COMPANY,
            &NewBuildSheet {
                sheet_id: "XRT-50-STD".into(),
                item_id: "XRT-50-STD".into(),
                version: 1,
                name: "50in rescue tube".into(),
                yield_pct: Decimal::new(78, 2),
                labour_minutes: Decimal::from(6),
                labour_rate_minor_per_hour: Money::from_minor(4800),
                overhead_minor: Money::from_minor(150),
                lines: vec![
                    NewBuildSheetLine {
                        material_id: Some("FOAM-XLPE-BLU".into()),
                        part_item_id: None,
                        qty_per_unit: Decimal::new(26, 1),
                        unit: "bf".into(),
                        note: Some("round blank".into()),
                    },
                    NewBuildSheetLine {
                        material_id: None,
                        part_item_id: Some("TOWLINE-6FT".into()),
                        qty_per_unit: Decimal::ONE,
                        unit: "ea".into(),
                        note: None,
                    },
                ],
            },
            Utc::now(),
        )
        .expect("sheet");

    let sheet_full = ledger.build_sheet(COMPANY, "XRT-50-STD").expect("sheet");
    let mut materials = std::collections::HashMap::new();
    for line in &sheet_full.lines {
        let id = line.component_id().unwrap();
        materials.insert(id.to_string(), ledger.material(COMPANY, id).unwrap());
    }
    let standard = sheet::standard_cost(&sheet_full, &materials).expect("standard cost");
    assert!(standard.total.minor() > 0);
    // Five points of yield beats a five percent foam discount on this
    // recipe (`prototype/README.md` "Manufacturing").
    let scenarios = sheet::sensitivity(&sheet_full, &materials, Money::from_minor(4995))
        .expect("sensitivity");
    let yield_up = scenarios.iter().find(|s| s.name == "Yield +5 points").unwrap();
    let foam_down = scenarios.iter().find(|s| s.name == "Foam price -5%").unwrap();
    assert!(yield_up.delta_vs_base > foam_down.delta_vs_base);

    // -- A build that overruns its recipe -------------------------------
    let overrun = build::complete_build(
        &ledger,
        COMPANY,
        NewBuild {
            sheet_id: "XRT-50-STD".into(),
            qty_built: Decimal::from(35),
            // Standard for 35 units is 35 * 2.6 / 0.78 = 116.67 bf; this
            // build actually used more (an overrun, so 5100 debits).
            consumptions: vec![("FOAM-XLPE-BLU".into(), Decimal::new(1216, 1))],
            labour_minutes_actual: Decimal::from(210),
            completed_on: ymd(2026, 8, 19),
            started_on: None,
            class: ClassId("foam".into()),
            note: Some("BLD-0912".into()),
        },
        Utc::now(),
    )
    .expect("overrun build posts");
    assert!(overrun.variance.is_negative(), "an overrun debits 5100");

    let as_of = ymd(2026, 8, 19);
    assert_eq!(
        tb_balance(&ledger, as_of, chart::INVENTORY_FINISHED),
        overrun.standard_cost,
        "1310 is the build's standard cost, to the cent"
    );
    assert_eq!(
        tb_balance(&ledger, as_of, chart::INVENTORY_RAW),
        total_landed.checked_sub(overrun.actual_material).unwrap(),
        "1300 is landed cost minus what the build actually consumed"
    );
    // 5300 is a credit-only account; TbRow.balance is debit-positive, so a
    // pure-credit balance reads as the negative of what was applied.
    assert_eq!(
        tb_balance(&ledger, as_of, chart::PARTS_LABOUR_APPLIED),
        overrun.applied.checked_neg().unwrap(),
        "5300 carries exactly what was applied at standard"
    );
    assert_eq!(
        tb_balance(&ledger, as_of, chart::MANUFACTURING_VARIANCE),
        overrun.variance.checked_neg().unwrap(),
        "5100 carries the overrun as a debit (negated in the debit-positive reading)"
    );

    // -- material_on_hand agrees with 1300 to the cent -------------------
    let onhand = report::material_on_hand(&ledger, COMPANY).expect("onhand");
    assert_eq!(
        onhand.total_value,
        tb_balance(&ledger, as_of, chart::INVENTORY_RAW)
    );
    assert_eq!(onhand.total_value, onhand.account_1300_balance);

    // -- A second build, exact consumption: zero variance ----------------
    let standard_bf_per_unit = Decimal::new(26, 1) / Decimal::new(78, 2);
    let variance_before = tb_balance(&ledger, ymd(2026, 8, 20), chart::MANUFACTURING_VARIANCE);
    let exact = build::complete_build(
        &ledger,
        COMPANY,
        NewBuild {
            sheet_id: "XRT-50-STD".into(),
            qty_built: Decimal::ONE,
            consumptions: vec![("FOAM-XLPE-BLU".into(), standard_bf_per_unit)],
            labour_minutes_actual: Decimal::from(6),
            completed_on: ymd(2026, 8, 20),
            started_on: None,
            class: ClassId("foam".into()),
            note: Some("BLD-0913".into()),
        },
        Utc::now(),
    )
    .expect("exact build posts");
    assert_eq!(exact.variance, Money::ZERO);
    assert_eq!(
        tb_balance(&ledger, ymd(2026, 8, 20), chart::MANUFACTURING_VARIANCE),
        variance_before,
        "a zero-variance build posts no 5100 leg at all"
    );

    // -- material_on_hand still agrees with 1300 -------------------------
    let onhand_after = report::material_on_hand(&ledger, COMPANY).expect("onhand");
    assert_eq!(onhand_after.total_value, onhand_after.account_1300_balance);

    // -- variance_by_item sees both builds --------------------------------
    let variances = report::variance_by_item(&ledger, COMPANY, ymd(2026, 8, 1), ymd(2026, 8, 31))
        .expect("variance report");
    let row = variances
        .iter()
        .find(|row| row.item_id == "XRT-50-STD")
        .expect("item present");
    assert_eq!(row.build_count, 2);

    // -- An inventory adjustment moves 5150 -------------------------------
    let shrinkage_before = tb_balance(&ledger, ymd(2026, 8, 31), chart::INVENTORY_ADJUSTMENT);
    build::adjust_inventory(
        &ledger,
        COMPANY,
        NewAdjustment {
            account: chart::INVENTORY_RAW.to_string(),
            amount: Money::from_minor(-1250),
            class: ClassId("foam".into()),
            adjusted_on: ymd(2026, 8, 31),
            reason: "August physical count, foam room".into(),
        },
        Utc::now(),
    )
    .expect("adjustment posts");
    let shrinkage_after = tb_balance(&ledger, ymd(2026, 8, 31), chart::INVENTORY_ADJUSTMENT);
    assert_eq!(
        shrinkage_after.checked_sub(shrinkage_before).unwrap(),
        Money::from_minor(1250)
    );

    // Every entry this test posted balances by construction; the trial
    // balance's own total debits/credits agreeing is the final proof.
    let final_tb = trial_balance(&ledger, COMPANY, ymd(2026, 8, 31)).expect("final tb");
    assert_eq!(final_tb.total_debits, final_tb.total_credits);
    assert!(final_tb.wrong_side.is_empty());
}
