//! Manufacturing commands: materials, receipts, build sheets, builds.
//!
//! These write the replica's own tables, never QuickBooks, so they are not
//! behind the QuickBooks write gate: costing a product changes nothing in the
//! book of record.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{parse, realm, to_json, ApiError};
use crate::domain::RealmId;
use crate::mfg::{self, ActualMaterial, Component, CostError, ReceiptLine};
use crate::store::mfg_store::Material;
use crate::store::reports::ItemRow;
use crate::store::Store;

pub const MFG_COMMANDS: &[&str] = &[
    "materials",
    "saveMaterial",
    "receipts",
    "recordReceipt",
    "buildSheets",
    "buildSheet",
    "saveBuildSheet",
    "builds",
    "recordBuild",
];

impl From<CostError> for ApiError {
    fn from(error: CostError) -> Self {
        ApiError::BadArguments(error.to_string())
    }
}

fn date_arg(raw: &str) -> Result<String, ApiError> {
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(|d| d.to_string())
        .map_err(|_| ApiError::BadArguments(format!("not a YYYY-MM-DD date: {raw:?}")))
}

pub(super) fn dispatch(store: &Store, command: &str, args: Value, now: DateTime<Utc>) -> Option<Result<Value, ApiError>> {
    let result = match command {
        "materials" => materials(store, args),
        "saveMaterial" => save_material(store, args, now),
        "receipts" => receipts(store, args),
        "recordReceipt" => record_receipt(store, args, now),
        "buildSheets" => build_sheets(store, args),
        "buildSheet" => build_sheet(store, args),
        "saveBuildSheet" => save_build_sheet(store, args, now),
        "builds" => builds(store, args),
        "recordBuild" => record_build(store, args, now),
        _ => return None,
    };
    Some(result)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RealmArgs {
    realm: String,
}

fn items_by_id(store: &Store, realm: &RealmId) -> Result<HashMap<String, ItemRow>, ApiError> {
    Ok(store.items(realm)?.into_iter().map(|i| (i.qbo_id.clone(), i)).collect())
}

fn price_of(item: Option<&ItemRow>) -> Option<Decimal> {
    item.and_then(|i| i.unit_price.as_deref()).and_then(|p| mfg::decimal(p, "price").ok())
}

fn materials(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmArgs = parse(args)?;
    to_json(store.materials(&realm(&args.realm)?)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MaterialArgs {
    realm: String,
    /// Absent for a new material.
    material_id: Option<String>,
    name: String,
    thickness_in: String,
    width_in: String,
    length_in: String,
    color: Option<String>,
}

fn save_material(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: MaterialArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadArguments("a material needs a name".into()));
    }
    let material = Material {
        material_id: args.material_id.unwrap_or_else(|| format!("mat-{}", Uuid::now_v7().simple())),
        name: name.to_string(),
        thickness_in: mfg::decimal(&args.thickness_in, "thickness")?,
        width_in: mfg::decimal(&args.width_in, "width")?,
        length_in: mfg::decimal(&args.length_in, "length")?,
        color: args.color.filter(|c| !c.trim().is_empty()),
    };
    let bf = mfg::board_feet(material.thickness_in, material.width_in, material.length_in)?;
    store.save_material(&realm, &material, now)?;
    to_json(json!({ "materialId": material.material_id, "bfPerSheet": bf }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiptsArgs {
    realm: String,
    material_id: Option<String>,
}

fn receipts(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: ReceiptsArgs = parse(args)?;
    to_json(store.receipts(&realm(&args.realm)?, args.material_id.as_deref())?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiptLineArgs {
    material_id: String,
    sheets: String,
    price_per_sheet: String,
    #[serde(default)]
    surcharge_per_sheet: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiptArgs {
    realm: String,
    received_on: String,
    vendor_id: Option<String>,
    reference: Option<String>,
    freight: String,
    lines: Vec<ReceiptLineArgs>,
}

fn record_receipt(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: ReceiptArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let received_on = date_arg(&args.received_on)?;
    let freight_decimal = mfg::decimal(&args.freight, "freight")?;
    let freight = ledger_core::round_money(freight_decimal, ledger_core::RoundingPolicy::MirroredAmount)
        .map_err(|e| ApiError::BadArguments(e.to_string()))?;
    if Decimal::new(freight.minor(), 2) != freight_decimal {
        return Err(ApiError::BadArguments("freight is an amount billed: at most two decimal places".into()));
    }
    let mut lines = Vec::new();
    for line in &args.lines {
        let material = store
            .material(&realm, &line.material_id)?
            .ok_or_else(|| ApiError::BadArguments(format!("no material {:?}", line.material_id)))?;
        lines.push(ReceiptLine {
            material_id: material.material_id,
            sheets: mfg::decimal(&line.sheets, "sheets")?,
            bf_per_sheet: mfg::board_feet(material.thickness_in, material.width_in, material.length_in)?,
            price_per_sheet: mfg::decimal(&line.price_per_sheet, "price per sheet")?,
            surcharge_per_sheet: match line.surcharge_per_sheet.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(raw) => mfg::decimal(raw, "surcharge")?,
                None => Decimal::ZERO,
            },
        });
    }
    let landed = mfg::landed_cost(&lines, freight)?;
    let receipt_id = format!("rcpt-{}", Uuid::now_v7().simple());
    store.record_receipt(
        &realm,
        &receipt_id,
        &received_on,
        args.vendor_id.as_deref().filter(|v| !v.is_empty()),
        args.reference.as_deref().filter(|r| !r.trim().is_empty()),
        freight,
        &lines,
        &landed,
        now,
    )?;
    to_json(json!({ "receiptId": receipt_id, "lines": landed }))
}

fn build_sheets(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let items = items_by_id(store, &realm)?;
    let prices = store.material_prices(&realm)?;
    let mut out = Vec::new();
    for (item_id, components, updated) in store.build_sheets(&realm)? {
        let item = items.get(&item_id);
        let price = price_of(item);
        let (cost, problem) = match mfg::unit_cost(&components, &prices) {
            Ok(cost) => (Some(cost), None),
            Err(e) => (None, Some(e.to_string())),
        };
        out.push(json!({
            "itemId": item_id,
            "name": item.map(|i| i.name.clone()).unwrap_or_else(|| format!("Item {item_id}")),
            "sku": item.and_then(|i| i.sku.clone()),
            "price": price,
            "unitCost": cost.as_ref().map(|c| c.total),
            "materialCost": cost.as_ref().map(|c| c.material),
            "wasteCost": cost.as_ref().map(|c| c.waste),
            "marginPct": cost.as_ref().and_then(|c| mfg::margin_pct(price, c.total)),
            "problem": problem,
            "components": components.len(),
            "updatedAt": updated,
        }));
    }
    out.sort_by(|a, b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()));
    to_json(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SheetArgs {
    realm: String,
    item_id: String,
}

const PRICE_MOVES: [i64; 5] = [-10, -5, 0, 5, 10];
const YIELD_MOVES: [i64; 3] = [-5, 0, 5];

fn build_sheet(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: SheetArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let items = items_by_id(store, &realm)?;
    let item = items.get(&args.item_id).ok_or(ApiError::NotFound)?;
    let components = store.build_sheet(&realm, &args.item_id)?.unwrap_or_default();
    let prices = store.material_prices(&realm)?;
    let price = price_of(Some(item));
    let (cost, problem, grid) = if components.is_empty() {
        (None, None, vec![])
    } else {
        match mfg::unit_cost(&components, &prices) {
            Ok(cost) => {
                let grid = mfg::sensitivity(
                    &components,
                    &prices,
                    price,
                    &PRICE_MOVES.map(Decimal::from),
                    &YIELD_MOVES.map(Decimal::from),
                )?;
                (Some(cost), None, grid)
            }
            Err(e) => (None, Some(e.to_string()), vec![]),
        }
    };
    let margin = cost.as_ref().and_then(|c| mfg::margin_pct(price, c.total));
    to_json(json!({
        "item": { "qboId": item.qbo_id, "name": item.name, "sku": item.sku, "unitPrice": item.unit_price },
        "components": components,
        "cost": cost,
        "marginPct": margin,
        "problem": problem,
        "sensitivity": grid,
        "builds": store.builds(&realm, Some(&args.item_id))?,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSheetArgs {
    realm: String,
    item_id: String,
    components: Vec<Component>,
}

fn save_build_sheet(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: SaveSheetArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    if !items_by_id(store, &realm)?.contains_key(&args.item_id) {
        return Err(ApiError::BadArguments(format!("no product {}", args.item_id)));
    }
    let materials: Vec<String> = store.materials(&realm)?.into_iter().map(|m| m.material.material_id).collect();
    for component in &args.components {
        if let Component::Material { material_id, .. } = component {
            if !materials.contains(material_id) {
                return Err(ApiError::BadArguments(format!("no material {material_id:?}")));
            }
        }
    }
    // Checked against a price list in which every material costs one dollar,
    // so a sheet with impossible yields or quantities is refused even before
    // its materials have receipts.
    let probe: HashMap<String, Decimal> = materials.iter().map(|m| (m.clone(), Decimal::ONE)).collect();
    mfg::unit_cost(&args.components, &probe)?;
    store.save_build_sheet(&realm, &args.item_id, &args.components, now)?;
    to_json(json!({ "itemId": args.item_id }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildsArgs {
    realm: String,
    item_id: Option<String>,
}

fn builds(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: BuildsArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let items = items_by_id(store, &realm)?;
    let rows: Vec<Value> = store
        .builds(&realm, args.item_id.as_deref())?
        .into_iter()
        .map(|b| {
            let mut v = serde_json::to_value(&b).unwrap_or(Value::Null);
            v["name"] = json!(items.get(&b.item_id).map(|i| i.name.clone()));
            v
        })
        .collect();
    to_json(rows)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildArgs {
    realm: String,
    item_id: String,
    built_on: String,
    quantity: String,
    #[serde(default)]
    actual: Vec<ActualArgs>,
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActualArgs {
    material_id: String,
    board_feet: String,
}

fn record_build(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: BuildArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let components = store
        .build_sheet(&realm, &args.item_id)?
        .ok_or_else(|| ApiError::BadArguments("this product has no build sheet; write one first".into()))?;
    let actual = args
        .actual
        .iter()
        .filter(|a| !a.board_feet.trim().is_empty())
        .map(|a| Ok(ActualMaterial { material_id: a.material_id.clone(), board_feet: mfg::decimal(&a.board_feet, "board-feet")? }))
        .collect::<Result<Vec<_>, CostError>>()?;
    let prices = store.material_prices(&realm)?;
    let variance = mfg::build_variance(&components, &prices, mfg::decimal(&args.quantity, "quantity")?, &actual)?;
    let build_id = format!("build-{}", Uuid::now_v7().simple());
    store.record_build(
        &realm,
        &build_id,
        &args.item_id,
        &date_arg(&args.built_on)?,
        &components,
        &actual,
        &variance,
        args.note.as_deref().filter(|n| !n.trim().is_empty()),
        now,
    )?;
    to_json(json!({ "buildId": build_id, "variance": variance }))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::super::dispatch_at;
    use crate::demo::{self, DemoSize, DEMO_REALM};
    use crate::store::Store;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 10, 18, 9, 0, 0).unwrap()
    }

    fn book() -> Store {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 4, vendors: 2, items: 6, invoices: 5, purchase_orders: 1 };
        demo::seed(&store, size, 9, now()).unwrap();
        store
    }

    fn call(store: &Store, command: &str, args: serde_json::Value) -> serde_json::Value {
        dispatch_at(store, command, args, now()).unwrap_or_else(|e| panic!("{command}: {e}"))
    }

    #[test]
    fn a_material_receipt_and_a_build_sheet_cost_a_product() {
        let store = book();
        let r = DEMO_REALM;
        let material = call(&store, "saveMaterial", json!({ "realm": r, "name": "XLPE 2 in red", "thicknessIn": "2", "widthIn": "48", "lengthIn": "96", "color": "red" }));
        let id = material["materialId"].as_str().unwrap().to_string();
        assert_eq!(material["bfPerSheet"], "64");

        let receipt = call(&store, "recordReceipt", json!({ "realm": r, "receivedOn": "2026-10-01", "freight": "64.00", "reference": "PO-5001",
            "lines": [ { "materialId": id, "sheets": "10", "pricePerSheet": "89.60" } ] }));
        assert_eq!(receipt["lines"][0]["landedPerBf"], "1.5");

        let item = store.items(&crate::domain::RealmId::parse(r).unwrap()).unwrap().remove(0);
        call(&store, "saveBuildSheet", json!({ "realm": r, "itemId": item.qbo_id, "components": [
            { "kind": "material", "materialId": id, "netBf": "2.1", "yieldPct": "78" },
            { "kind": "labour", "name": "CNC", "minutes": "6", "ratePerHour": "30" }
        ] }));
        let sheet = call(&store, "buildSheet", json!({ "realm": r, "itemId": item.qbo_id }));
        assert_eq!(sheet["cost"]["total"], "7.0385");
        assert_eq!(sheet["sensitivity"].as_array().unwrap().len(), 15);
        let list = call(&store, "buildSheets", json!({ "realm": r }));
        assert_eq!(list[0]["unitCost"], "7.0385");

        let build = call(&store, "recordBuild", json!({ "realm": r, "itemId": item.qbo_id, "builtOn": "2026-10-10", "quantity": "100",
            "actual": [ { "materialId": id, "boardFeet": "290" } ] }));
        assert_eq!(build["variance"]["flagged"], true);
        let listed = call(&store, "builds", json!({ "realm": r, "itemId": item.qbo_id }));
        let ours = listed.as_array().unwrap().iter().find(|b| b["buildId"] == build["buildId"]).unwrap();
        assert_eq!(ours["varianceMinor"], 3_115);
    }

    #[test]
    fn bad_input_is_refused_with_a_reason() {
        let store = book();
        let r = DEMO_REALM;
        let bad = dispatch_at(&store, "saveMaterial", json!({ "realm": r, "name": "x", "thicknessIn": "0", "widthIn": "1", "lengthIn": "1" }), now());
        assert_eq!(bad.unwrap_err().status(), 400);
        let item = store.items(&crate::domain::RealmId::parse(r).unwrap()).unwrap().remove(0).qbo_id;
        let impossible = dispatch_at(&store, "saveBuildSheet", json!({ "realm": r, "itemId": item, "components": [
            { "kind": "labour", "name": "x", "minutes": "-1", "ratePerHour": "1" } ] }), now());
        assert_eq!(impossible.unwrap_err().status(), 400);
        let no_sheet = dispatch_at(&store, "recordBuild", json!({ "realm": r, "itemId": "no-such-item", "builtOn": "2026-10-10", "quantity": "1" }), now());
        assert!(no_sheet.unwrap_err().to_string().contains("no build sheet"));
        let fractional_freight = dispatch_at(&store, "recordReceipt", json!({ "realm": r, "receivedOn": "2026-10-01", "freight": "1.005", "lines": [] }), now());
        assert_eq!(fractional_freight.unwrap_err().status(), 400);
    }
}
