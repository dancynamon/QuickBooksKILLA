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
    "stock",
    "stockLedger",
    "recordCount",
    "setReorderPoint",
    "productStock",
    "productLedger",
    "recordProductCount",
    "importCsv",
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
        "stock" => stock(store, args),
        "stockLedger" => stock_ledger(store, args),
        "recordCount" => record_count(store, args, now),
        "setReorderPoint" => set_reorder_point(store, args),
        "productStock" => product_stock(store, args),
        "productLedger" => product_ledger(store, args),
        "recordProductCount" => record_product_count(store, args, now),
        "importCsv" => import_csv(store, args, now),
        _ => return None,
    };
    Some(result)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RealmArgs {
    realm: String,
    /// Which cost a build sheet uses: the latest delivery's landed $/bf
    /// (the default, and how a quote is priced), or the moving average of
    /// what is on the shelf.
    #[serde(default)]
    basis: Option<String>,
}

/// Prices for costing on the chosen basis.
fn prices_for(store: &Store, realm: &RealmId, basis: Option<&str>) -> Result<crate::store::mfg_store::Prices, ApiError> {
    let mut prices = store.material_prices(realm)?;
    match basis.unwrap_or("latest") {
        "latest" => {}
        "average" => {
            for row in store.materials(realm)? {
                let state = mfg::stock(store.stock_events(realm, &row.material.material_id)?)?;
                match state.average_per_bf {
                    Some(avg) if state.on_hand_bf > Decimal::ZERO => {
                        prices.per_bf.insert(row.material.material_id, avg);
                    }
                    // Nothing on the shelf: the average means nothing, so the
                    // latest delivery stands in rather than a zero.
                    _ => {}
                }
            }
        }
        other => return Err(ApiError::BadArguments(format!("basis must be latest or average, not {other:?}"))),
    }
    Ok(prices)
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
    let prices = prices_for(store, &realm, args.basis.as_deref())?;
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
    #[serde(default)]
    basis: Option<String>,
}

const PRICE_MOVES: [i64; 5] = [-10, -5, 0, 5, 10];
const YIELD_MOVES: [i64; 3] = [-5, 0, 5];

fn build_sheet(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: SheetArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let items = items_by_id(store, &realm)?;
    let item = items.get(&args.item_id).ok_or(ApiError::NotFound)?;
    let components = store.build_sheet(&realm, &args.item_id)?.unwrap_or_default();
    let prices = prices_for(store, &realm, args.basis.as_deref())?;
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

fn stock(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let mut out = Vec::new();
    for row in store.materials(&realm)? {
        let id = row.material.material_id.clone();
        let state = mfg::stock(store.stock_events(&realm, &id)?)?;
        let reorder = store.reorder_point(&realm, &id)?;
        out.push(json!({
            "materialId": id,
            "name": row.material.name,
            "onHandBf": state.on_hand_bf,
            "sheetsOnHand": mfg::rate(state.on_hand_bf / row.bf_per_sheet),
            "averagePerBf": state.average_per_bf,
            "latestPerBf": row.landed_per_bf,
            "value": state.value,
            "reorderBf": reorder,
            "low": reorder.is_some_and(|r| state.on_hand_bf < r),
            "wentNegative": state.went_negative,
            "shrinkageValue": state.shrinkage_value,
        }));
    }
    to_json(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MaterialRef {
    realm: String,
    material_id: String,
}

fn stock_ledger(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: MaterialRef = parse(args)?;
    let realm = realm(&args.realm)?;
    store.material(&realm, &args.material_id)?.ok_or(ApiError::NotFound)?;
    to_json(mfg::stock(store.stock_events(&realm, &args.material_id)?)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountArgs {
    realm: String,
    material_id: String,
    counted_on: String,
    /// Board-feet, or sheets when `sheets` is set.
    amount: String,
    #[serde(default)]
    sheets: bool,
    note: Option<String>,
}

fn record_count(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: CountArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let material = store.material(&realm, &args.material_id)?.ok_or(ApiError::NotFound)?;
    let amount = mfg::decimal(&args.amount, "count")?;
    if amount < Decimal::ZERO {
        return Err(ApiError::BadArguments("a count cannot be negative".into()));
    }
    // Sheets are what a person counts on the rack; board-feet is what costs.
    let board_feet = if args.sheets {
        amount * mfg::board_feet(material.thickness_in, material.width_in, material.length_in)?
    } else {
        amount
    };
    let count_id = format!("count-{}", Uuid::now_v7().simple());
    store.record_count(&realm, &count_id, &args.material_id, &date_arg(&args.counted_on)?, board_feet, args.note.as_deref().filter(|n| !n.trim().is_empty()), now)?;
    to_json(json!({ "countId": count_id, "boardFeet": board_feet }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReorderArgs {
    realm: String,
    material_id: String,
    /// Board-feet; blank clears it.
    board_feet: Option<String>,
}

fn set_reorder_point(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: ReorderArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let value = match args.board_feet.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => {
            let v = mfg::decimal(raw, "reorder point")?;
            if v < Decimal::ZERO {
                return Err(ApiError::BadArguments("a reorder point cannot be negative".into()));
            }
            Some(v)
        }
        None => None,
    };
    store.material(&realm, &args.material_id)?.ok_or(ApiError::NotFound)?;
    store.set_reorder_point(&realm, &args.material_id, value)?;
    to_json(json!({ "materialId": args.material_id, "reorderBf": value }))
}

/// What an opening count of a product is worth per unit before any build has
/// set an average: its build sheet's cost at the latest delivery prices.
fn opening_cost(
    store: &Store,
    realm: &RealmId,
    item_id: &str,
) -> Result<Option<Decimal>, ApiError> {
    let Some(components) = store.build_sheet(realm, item_id)? else {
        return Ok(None);
    };
    Ok(mfg::unit_cost(&components, &store.material_prices(realm)?)
        .ok()
        .map(|c| c.total))
}

/// Finished-goods stock for every product with a build sheet or a count.
fn product_stock(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let items = items_by_id(store, &realm)?;
    let mut out = Vec::new();
    for (item_id, _, _) in store.build_sheets(&realm)? {
        let (events, start) = store.product_stock_events(&realm, &item_id)?;
        let item = items.get(&item_id);
        let base = json!({
            "itemId": item_id,
            "name": item.map(|i| i.name.clone()),
            "sku": item.and_then(|i| i.sku.clone()),
            "qboOnHand": item.and_then(|i| i.qty_on_hand.clone()),
        });
        let mut row = base;
        match start {
            None => {
                row["tracked"] = json!(false);
            }
            Some(start) => {
                let sold: Decimal = events
                    .iter()
                    .filter_map(|e| match e {
                        mfg::StockEvent::Consumed { board_feet, .. } => Some(*board_feet),
                        _ => None,
                    })
                    .sum();
                let state = mfg::stock_from(events, opening_cost(store, &realm, &item_id)?)?;
                row["tracked"] = json!(true);
                row["since"] = json!(start);
                row["onHand"] = json!(state.on_hand_bf);
                row["averageCost"] = json!(state.average_per_bf);
                row["value"] = json!(state.value);
                row["netSold"] = json!(sold);
                row["wentNegative"] = json!(state.went_negative);
            }
        }
        out.push(row);
    }
    out.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    to_json(out)
}

fn product_ledger(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: SheetArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let (events, start) = store.product_stock_events(&realm, &args.item_id)?;
    let state = mfg::stock_from(events, opening_cost(store, &realm, &args.item_id)?)?;
    to_json(json!({ "since": start, "stock": state }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductCountArgs {
    realm: String,
    item_id: String,
    counted_on: String,
    units: String,
    note: Option<String>,
}

fn record_product_count(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: ProductCountArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    if !items_by_id(store, &realm)?.contains_key(&args.item_id) {
        return Err(ApiError::NotFound);
    }
    let units = mfg::decimal(&args.units, "count")?;
    if units < Decimal::ZERO {
        return Err(ApiError::BadArguments("a count cannot be negative".into()));
    }
    let count_id = format!("pcount-{}", Uuid::now_v7().simple());
    store.record_product_count(
        &realm,
        &count_id,
        &args.item_id,
        &date_arg(&args.counted_on)?,
        units,
        args.note.as_deref().filter(|n| !n.trim().is_empty()),
        now,
    )?;
    to_json(json!({ "countId": count_id }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportArgs {
    realm: String,
    /// materials, buildSheets or counts.
    kind: String,
    text: String,
    /// False (the default) previews; true writes, all or nothing.
    #[serde(default)]
    commit: bool,
}

/// A failed import: the rows that are wrong, by spreadsheet line.
fn row_errors(errors: Vec<crate::mfg_import::RowError>) -> Result<Value, ApiError> {
    to_json(json!({ "ok": false, "errors": errors, "rows": [] }))
}

fn import_csv(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    use crate::mfg_import as import;
    let args: ImportArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let materials = store.materials(&realm)?;
    let material_named = |name: &str| {
        materials
            .iter()
            .find(|m| m.material.name.eq_ignore_ascii_case(name.trim()))
    };
    let items: Vec<ItemRow> = store.items(&realm)?;
    let product_named = |name: &str| {
        let name = name.trim();
        items
            .iter()
            .find(|i| {
                i.sku
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(name))
            })
            .or_else(|| items.iter().find(|i| i.name.eq_ignore_ascii_case(name)))
    };

    match args.kind.as_str() {
        "materials" => {
            let parsed = match import::materials(&args.text) {
                Ok(p) => p,
                Err(e) => return row_errors(e),
            };
            let rows: Vec<Value> = parsed
                .iter()
                .map(|m| json!({ "name": m.name, "action": if material_named(&m.name).is_some() { "update" } else { "add" },
                                 "bfPerSheet": mfg::board_feet(m.thickness_in, m.width_in, m.length_in).ok(), "reorderBf": m.reorder_bf }))
                .collect();
            if args.commit {
                store.atomically(|| -> Result<(), ApiError> {
                    for m in &parsed {
                        let id = material_named(&m.name)
                            .map(|r| r.material.material_id.clone())
                            .unwrap_or_else(|| format!("mat-{}", Uuid::now_v7().simple()));
                        store.save_material(
                            &realm,
                            &Material {
                                material_id: id.clone(),
                                name: m.name.clone(),
                                thickness_in: m.thickness_in,
                                width_in: m.width_in,
                                length_in: m.length_in,
                                color: m.color.clone(),
                            },
                            now,
                        )?;
                        if m.reorder_bf.is_some() {
                            store.set_reorder_point(&realm, &id, m.reorder_bf)?;
                        }
                    }
                    Ok(())
                })?;
            }
            to_json(json!({ "ok": true, "committed": args.commit, "rows": rows, "errors": [] }))
        }
        "buildSheets" => {
            let parsed = match import::build_sheets(&args.text) {
                Ok(p) => p,
                Err(e) => return row_errors(e),
            };
            let mut errors = Vec::new();
            let mut ready = Vec::new();
            let resolve = |name: &str| material_named(name).map(|m| m.material.material_id.clone());
            let probe: HashMap<String, Decimal> = materials
                .iter()
                .map(|m| (m.material.material_id.clone(), Decimal::ONE))
                .collect();
            for sheet in &parsed {
                let Some(item) = product_named(&sheet.product) else {
                    errors.push(import::RowError {
                        line: sheet.lines[0].line,
                        message: format!("no product with SKU or name {:?}", sheet.product),
                    });
                    continue;
                };
                match import::components(sheet, &resolve) {
                    Ok(components) => match mfg::unit_cost(&components, &probe) {
                        Ok(_) => ready.push((item, components, sheet.lines.len())),
                        Err(e) => errors.push(import::RowError {
                            line: sheet.lines[0].line,
                            message: format!("{}: {e}", sheet.product),
                        }),
                    },
                    Err(mut e) => errors.append(&mut e),
                }
            }
            if !errors.is_empty() {
                return row_errors(errors);
            }
            let rows: Vec<Value> = ready
                .iter()
                .map(|(item, _, lines)| json!({ "name": item.name, "sku": item.sku, "components": lines,
                    "action": if store.build_sheet(&realm, &item.qbo_id).ok().flatten().is_some() { "replace" } else { "add" } }))
                .collect();
            if args.commit {
                store.atomically(|| -> Result<(), ApiError> {
                    for (item, components, _) in &ready {
                        store.save_build_sheet(&realm, &item.qbo_id, components, now)?;
                    }
                    Ok(())
                })?;
            }
            to_json(json!({ "ok": true, "committed": args.commit, "rows": rows, "errors": [] }))
        }
        "counts" => {
            let parsed = match import::counts(&args.text) {
                Ok(p) => p,
                Err(e) => return row_errors(e),
            };
            enum Target {
                Material(String, Decimal),
                Product(String),
            }
            let mut errors = Vec::new();
            let mut ready = Vec::new();
            for c in &parsed {
                if let Some(m) = material_named(&c.what) {
                    let bf = match c.unit.as_str() {
                        "sheets" | "default" => c.amount * m.bf_per_sheet,
                        "bf" => c.amount,
                        _ => {
                            errors.push(import::RowError {
                                line: c.line,
                                message: format!(
                                    "{} is a material: count it in sheets or bf",
                                    c.what
                                ),
                            });
                            continue;
                        }
                    };
                    ready.push((c, Target::Material(m.material.material_id.clone(), bf)));
                } else if let Some(item) = product_named(&c.what) {
                    if !matches!(c.unit.as_str(), "units" | "default") {
                        errors.push(import::RowError {
                            line: c.line,
                            message: format!("{} is a product: count it in units", c.what),
                        });
                        continue;
                    }
                    ready.push((c, Target::Product(item.qbo_id.clone())));
                } else {
                    errors.push(import::RowError {
                        line: c.line,
                        message: format!("no material or product named {:?}", c.what),
                    });
                }
            }
            if !errors.is_empty() {
                return row_errors(errors);
            }
            let rows: Vec<Value> = ready
                .iter()
                .map(|(c, t)| match t {
                    Target::Material(_, bf) => json!({ "name": c.what, "date": c.date, "kind": "material", "amount": bf, "unit": "bf" }),
                    Target::Product(_) => json!({ "name": c.what, "date": c.date, "kind": "product", "amount": c.amount, "unit": "units" }),
                })
                .collect();
            if args.commit {
                store.atomically(|| -> Result<(), ApiError> {
                    for (c, t) in &ready {
                        match t {
                            Target::Material(id, bf) => store.record_count(
                                &realm,
                                &format!("count-{}", Uuid::now_v7().simple()),
                                id,
                                &c.date,
                                *bf,
                                Some("imported"),
                                now,
                            )?,
                            Target::Product(id) => store.record_product_count(
                                &realm,
                                &format!("pcount-{}", Uuid::now_v7().simple()),
                                id,
                                &c.date,
                                c.amount,
                                Some("imported"),
                                now,
                            )?,
                        }
                    }
                    Ok(())
                })?;
            }
            to_json(json!({ "ok": true, "committed": args.commit, "rows": rows, "errors": [] }))
        }
        other => Err(ApiError::BadArguments(format!(
            "kind must be materials, buildSheets or counts, not {other:?}"
        ))),
    }
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
    fn stock_counts_and_reorder_points() {
        let store = book();
        let r = DEMO_REALM;
        let material = call(&store, "saveMaterial", json!({ "realm": r, "name": "EVA 1 in black", "thicknessIn": "1", "widthIn": "48", "lengthIn": "96" }));
        let id = material["materialId"].as_str().unwrap().to_string();
        call(&store, "recordReceipt", json!({ "realm": r, "receivedOn": "2026-10-01", "freight": "0", "lines": [ { "materialId": id, "sheets": "10", "pricePerSheet": "48" } ] }));
        call(&store, "setReorderPoint", json!({ "realm": r, "materialId": id, "boardFeet": "300" }));
        // 10 sheets × 32 bf; a count of 8 sheets is 256 bf, under the reorder point.
        call(&store, "recordCount", json!({ "realm": r, "materialId": id, "countedOn": "2026-10-15", "amount": "8", "sheets": true }));
        let stock = call(&store, "stock", json!({ "realm": r }));
        let row = stock.as_array().unwrap().iter().find(|m| m["materialId"] == id.as_str()).unwrap();
        assert_eq!(row["onHandBf"], "256");
        assert_eq!(row["sheetsOnHand"], "8");
        assert_eq!(row["averagePerBf"], "1.5");
        assert_eq!(row["low"], true);
        assert_eq!(row["shrinkageValue"], 9_600);
        let ledger = call(&store, "stockLedger", json!({ "realm": r, "materialId": id }));
        assert_eq!(ledger["moves"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_build_sheet_can_cost_on_the_moving_average() {
        let store = book();
        let r = DEMO_REALM;
        let sheets = call(&store, "buildSheets", json!({ "realm": r }));
        let latest = call(&store, "buildSheets", json!({ "realm": r, "basis": "latest" }));
        assert_eq!(sheets, latest, "latest is the default");
        let average = call(&store, "buildSheets", json!({ "realm": r, "basis": "average" }));
        // Seeded prices rose across deliveries, so the shelf average sits below the latest.
        let foam = |v: &serde_json::Value| v.as_array().unwrap().iter().find(|s| s["materialCost"] != "0").cloned().unwrap();
        let (l, a) = (foam(&latest), foam(&average));
        assert!(a["unitCost"].as_str().unwrap().parse::<f64>().unwrap() < l["unitCost"].as_str().unwrap().parse::<f64>().unwrap());
        assert_eq!(dispatch_at(&store, "buildSheets", json!({ "realm": r, "basis": "fifo" }), now()).unwrap_err().status(), 400);
    }

    #[test]
    fn an_import_previews_then_commits_all_or_nothing() {
        let store = book();
        let r = DEMO_REALM;
        let csv = "name,thickness,width,length,reorder\nEVA 1 in black,1,48,96,300\nXLPE 2 in blue,2,48,96,640\n";
        let preview = call(
            &store,
            "importCsv",
            json!({ "realm": r, "kind": "materials", "text": csv }),
        );
        assert_eq!(preview["ok"], true);
        assert_eq!(preview["rows"][0]["action"], "add");
        assert_eq!(
            preview["rows"][1]["action"], "update",
            "the demo book already has blue"
        );
        let before = call(&store, "materials", json!({ "realm": r }))
            .as_array()
            .unwrap()
            .len();
        assert_eq!(
            call(&store, "materials", json!({ "realm": r }))
                .as_array()
                .unwrap()
                .len(),
            before,
            "a preview writes nothing"
        );
        call(
            &store,
            "importCsv",
            json!({ "realm": r, "kind": "materials", "text": csv, "commit": true }),
        );
        assert_eq!(
            call(&store, "materials", json!({ "realm": r }))
                .as_array()
                .unwrap()
                .len(),
            before + 1
        );

        let item = store
            .items(&crate::domain::RealmId::parse(r).unwrap())
            .unwrap()
            .remove(0);
        let sku = item.sku.clone().unwrap();
        let sheets = format!("product,kind,what,amount,rate\n{sku},material,EVA 1 in black,1,80\n{sku},labour,Pack,3,28\nNOPE-1,part,Strap,1,1\n");
        let bad = call(
            &store,
            "importCsv",
            json!({ "realm": r, "kind": "buildSheets", "text": sheets, "commit": true }),
        );
        assert_eq!(bad["ok"], false);
        assert_eq!(bad["errors"][0]["line"], 4);
        let good = format!("product,kind,what,amount,rate\n{sku},material,EVA 1 in black,1,80\n{sku},labour,Pack,3,28\n");
        let done = call(
            &store,
            "importCsv",
            json!({ "realm": r, "kind": "buildSheets", "text": good, "commit": true }),
        );
        assert_eq!(done["rows"][0]["action"], "replace");

        let counts =
            format!("what,date,amount\nEVA 1 in black,2026-10-31,5\n{sku},2026-10-31,40\n");
        call(
            &store,
            "importCsv",
            json!({ "realm": r, "kind": "counts", "text": counts, "commit": true }),
        );
        let stock = call(&store, "productStock", json!({ "realm": r }));
        let row = stock
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["itemId"] == item.qbo_id.as_str())
            .unwrap();
        assert_eq!(row["tracked"], true);
        assert_eq!(row["onHand"], "40");
    }

    #[test]
    fn finished_goods_come_in_from_builds_and_go_out_on_sales() {
        let store = book();
        let r = DEMO_REALM;
        let realm = crate::domain::RealmId::parse(r).unwrap();
        // The demo seeds builds; the first built product is tracked from its first build.
        let built = store.builds(&realm, None).unwrap().pop().unwrap();
        let ledger = call(
            &store,
            "productLedger",
            json!({ "realm": r, "itemId": built.item_id }),
        );
        assert_eq!(ledger["since"], built.built_on.as_str());
        assert_eq!(ledger["stock"]["moves"][0]["kind"], "received");
        call(
            &store,
            "recordProductCount",
            json!({ "realm": r, "itemId": built.item_id, "countedOn": "2026-12-31", "units": "12" }),
        );
        let after = call(
            &store,
            "productLedger",
            json!({ "realm": r, "itemId": built.item_id }),
        );
        assert_eq!(after["stock"]["onHandBf"], "12");
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
