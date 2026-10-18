//! A synthetic book. `DESIGN.md` §11: "a synthetic realm generator produces the
//! book so these can be measured without hammering Intuit."
//!
//! Also what the UI is developed against: a replica with realistic shape —
//! estimate to invoice to payment, PO to bill to bill payment, classes on every
//! line — and **no real names, numbers or balances** (`HANDOFF.md` §2.6). Every
//! name is assembled from word lists; every amount from a seeded generator, so
//! the same seed always produces the same book.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};

use crate::domain::{EntityType, RealmId};
use crate::store::{MirroredEntity, Store, StoreError};

/// The realm id the demo book is registered under. Not a real company.
pub const DEMO_REALM: &str = "9130000000000001";

#[derive(Copy, Clone, Debug)]
pub struct DemoSize {
    pub customers: usize,
    pub vendors: usize,
    pub items: usize,
    pub invoices: usize,
    pub purchase_orders: usize,
}

impl Default for DemoSize {
    fn default() -> Self {
        DemoSize {
            customers: 200,
            vendors: 30,
            items: 120,
            invoices: 2_000,
            purchase_orders: 300,
        }
    }
}

/// SplitMix64: small, seedable, good enough for invented invoices.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const PLACES: &[&str] = &[
    "Harbor", "Lakeside", "Pinecrest", "Riverbend", "Cedar Point", "Maple Grove", "Bayview",
    "Northfield", "Stonebridge", "Willow Creek", "Eastgate", "Sandy Hook", "Clearwater",
    "Fox Hollow", "Oak Ridge", "Silver Lake", "Brookside", "Highland", "Seaside", "Elm Park",
];
const KINDS: &[&str] = &[
    "Swim Club", "Aquatic Center", "YMCA", "Parks & Rec", "Country Club", "Swim School",
    "High School Athletics", "Community Pool", "Waterpark", "Beach Patrol", "Rec Center",
];
const SUPPLIERS: &[&str] = &[
    "Foam Supply", "Plastics", "Fasteners", "Corrugated", "Webbing Co", "Hardware",
    "Industrial", "Freight Lines", "Coatings", "Polymers",
];
const PRODUCTS: &[(&str, &str, i64)] = &[
    ("Rescue tube", "RT", 4925),
    ("Kickboard", "KB", 1150),
    ("Dock bumper", "DB", 3800),
    ("Pool noodle", "PN", 450),
    ("Swim barbell", "SB", 2600),
    ("Lane line float", "LL", 1975),
    ("Anti-fatigue mat", "AM", 8900),
    ("Lifeguard chair", "LGC", 189_900),
    ("Safety sign", "SG", 5400),
    ("UV printed panel", "UV", 12_500),
];
const COLORS: &[&str] = &["red", "blue", "yellow", "orange", "green", "black", "white"];
const CLASSES: &[(&str, &str)] = &[
    ("1", "Aquatic Safety"),
    ("2", "Foam Fabrication"),
    ("3", "CNC Job Work"),
    ("4", "UV Printing"),
];

fn money(minor: i64) -> Value {
    // Two-decimal JSON numbers, exactly as QBO serialises amounts.
    json!((minor as f64) / 100.0)
}

fn entity(entity_type: EntityType, id: &str, updated: DateTime<Utc>, raw: Value) -> MirroredEntity {
    MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "0".to_string(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: raw,
    }
}

/// Generate the book. Pure: no store involved, so a benchmark can time the
/// write separately from the generation.
pub fn generate(size: DemoSize, seed: u64, now: DateTime<Utc>) -> Vec<MirroredEntity> {
    let mut rng = Rng(seed);
    let mut out = Vec::new();
    let start = NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date");
    let span_days = (now.date_naive() - start).num_days().max(1) as usize;
    let mut next_id = 1000u64;
    let mut id = || {
        next_id += 1;
        next_id.to_string()
    };

    for (class_id, name) in CLASSES {
        out.push(entity(EntityType::Class, class_id, now, json!({ "Id": class_id, "Name": name, "FullyQualifiedName": name, "Active": true })));
    }

    let mut customers = Vec::new();
    for n in 0..size.customers {
        let customer_id = id();
        let name = format!("{} {}", rng.pick(PLACES), rng.pick(KINDS));
        // Two clubs drawing the same words get a number, as QBO's unique
        // DisplayName rule would force a bookkeeper to do.
        let display = format!("{name} #{}", n + 1);
        out.push(entity(EntityType::Customer, &customer_id, now, json!({
            "Id": customer_id, "SyncToken": "0", "DisplayName": display, "CompanyName": name,
            "PrimaryEmailAddr": { "Address": format!("ap{}@example.invalid", n + 1) },
            "PrimaryPhone": { "FreeFormNumber": format!("(555) 01{:02}-{:04}", n % 100, rng.below(10_000)) },
            "Balance": 0, "Active": true
        })));
        customers.push(customer_id);
    }

    let mut vendors = Vec::new();
    for n in 0..size.vendors {
        let vendor_id = id();
        let name = format!("{} {}", rng.pick(PLACES), rng.pick(SUPPLIERS));
        out.push(entity(EntityType::Vendor, &vendor_id, now, json!({
            "Id": vendor_id, "SyncToken": "0", "DisplayName": format!("{name} #{}", n + 1),
            "CompanyName": name, "Balance": 0, "Active": true
        })));
        vendors.push(vendor_id);
    }

    let mut items = Vec::new();
    for n in 0..size.items {
        let item_id = id();
        let (product, prefix, base) = PRODUCTS[n % PRODUCTS.len()];
        let color = COLORS[(n / PRODUCTS.len()) % COLORS.len()];
        let price = base + (rng.below(20) as i64 - 10) * base / 100;
        let class = CLASSES[n % CLASSES.len()].0;
        out.push(entity(EntityType::Item, &item_id, now, json!({
            "Id": item_id, "SyncToken": "0",
            "Name": format!("{product}, {color} {}", n / (PRODUCTS.len() * COLORS.len()) + 1),
            "Sku": format!("{prefix}-{}-{:03}", color[..3].to_uppercase(), n),
            "Type": "Inventory", "UnitPrice": money(price), "PurchaseCost": money(price * 45 / 100),
            "QtyOnHand": rng.below(400), "Active": true
        })));
        items.push((item_id, format!("{product}, {color}"), price, class));
    }

    let lines = |rng: &mut Rng, detail: &str| -> (Vec<Value>, i64) {
        let count = 1 + rng.below(5);
        let mut total = 0;
        let mut out = Vec::new();
        for line_no in 1..=count {
            let (item_id, description, price, class) = &items[rng.below(items.len())];
            let qty = 1 + rng.below(40) as i64;
            let amount = price * qty;
            total += amount;
            let mut line_detail = json!({
                "ItemRef": { "value": item_id }, "Qty": qty, "UnitPrice": money(*price),
                "ClassRef": { "value": class }
            });
            if detail == "SalesItemLineDetail" {
                line_detail["TaxCodeRef"] = json!({ "value": if rng.chance(30) { "TAX" } else { "NON" } });
            }
            out.push(json!({
                "LineNum": line_no, "Description": description, "Amount": money(amount),
                "DetailType": detail, detail: line_detail
            }));
        }
        (out, total)
    };

    let date = |rng: &mut Rng| start + Duration::days(rng.below(span_days) as i64);

    let mut estimate_number = 3000;
    for n in 0..size.invoices {
        let customer = &customers[rng.below(customers.len())];
        let txn_date = date(&mut rng);
        let class = CLASSES[rng.below(CLASSES.len())].0;
        let (line_values, total) = lines(&mut rng, "SalesItemLineDetail");
        let updated = now - Duration::days((now.date_naive() - txn_date).num_days().max(0));
        let customer_po = rng.chance(40).then(|| format!("PO-{}", 40_000 + rng.below(60_000)));

        let mut linked = Vec::new();
        if rng.chance(35) {
            estimate_number += 1;
            let estimate_id = id();
            out.push(entity(EntityType::Estimate, &estimate_id, updated, json!({
                "Id": estimate_id, "SyncToken": "0", "DocNumber": estimate_number.to_string(),
                "TxnDate": (txn_date - Duration::days(7)).to_string(),
                "CustomerRef": { "value": customer }, "ClassRef": { "value": class },
                "TotalAmt": money(total), "TxnStatus": "Closed", "Line": line_values.clone()
            })));
            linked.push(json!({ "TxnId": estimate_id, "TxnType": "Estimate" }));
        }

        // Older invoices are mostly paid; recent ones mostly open.
        let age = (now.date_naive() - txn_date).num_days();
        let paid = (age > 45 && rng.chance(92)) || (age > 15 && rng.chance(50));
        let invoice_id = id();
        let mut invoice = json!({
            "Id": invoice_id, "SyncToken": "0", "DocNumber": (9_000 + n).to_string(),
            "TxnDate": txn_date.to_string(), "DueDate": (txn_date + Duration::days(30)).to_string(),
            "CustomerRef": { "value": customer }, "ClassRef": { "value": class },
            "TotalAmt": money(total), "Balance": money(if paid { 0 } else { total }),
            "Line": line_values, "LinkedTxn": linked
        });
        if let Some(po) = &customer_po {
            invoice["CustomField"] = json!([{ "DefinitionId": "1", "Name": "P.O. Number", "Type": "StringType", "StringValue": po }]);
        }
        if rng.chance(10) {
            invoice["PrivateNote"] = json!("Ship with next pallet");
        }
        out.push(entity(EntityType::Invoice, &invoice_id, updated, invoice));

        if paid {
            let payment_id = id();
            out.push(entity(EntityType::Payment, &payment_id, updated, json!({
                "Id": payment_id, "SyncToken": "0",
                "TxnDate": (txn_date + Duration::days(10 + rng.below(30) as i64)).to_string(),
                "CustomerRef": { "value": customer }, "TotalAmt": money(total),
                "Line": [ { "Amount": money(total), "LinkedTxn": [ { "TxnId": invoice_id, "TxnType": "Invoice" } ] } ]
            })));
        }
    }

    for n in 0..size.purchase_orders {
        let vendor = &vendors[rng.below(vendors.len())];
        let txn_date = date(&mut rng);
        let (line_values, total) = lines(&mut rng, "ItemBasedExpenseLineDetail");
        let age = (now.date_naive() - txn_date).num_days();
        let received = age > 14 && rng.chance(85);
        let po_id = id();
        out.push(entity(EntityType::PurchaseOrder, &po_id, now, json!({
            "Id": po_id, "SyncToken": "0", "DocNumber": (5_000 + n).to_string(),
            "TxnDate": txn_date.to_string(), "VendorRef": { "value": vendor },
            "TotalAmt": money(total), "POStatus": if received { "Closed" } else { "Open" },
            "Line": line_values.clone()
        })));
        if !received {
            continue;
        }
        let bill_id = id();
        let bill_paid = age > 40;
        out.push(entity(EntityType::Bill, &bill_id, now, json!({
            "Id": bill_id, "SyncToken": "0", "DocNumber": format!("V{}-{}", n % 97, 10_000 + n),
            "TxnDate": (txn_date + Duration::days(9)).to_string(),
            "DueDate": (txn_date + Duration::days(39)).to_string(),
            "VendorRef": { "value": vendor }, "TotalAmt": money(total),
            "Balance": money(if bill_paid { 0 } else { total }),
            "LinkedTxn": [ { "TxnId": po_id, "TxnType": "PurchaseOrder" } ],
            "Line": line_values
        })));
        if bill_paid {
            let payment_id = id();
            out.push(entity(EntityType::BillPayment, &payment_id, now, json!({
                "Id": payment_id, "SyncToken": "0",
                "TxnDate": (txn_date + Duration::days(35)).to_string(),
                "VendorRef": { "value": vendor }, "TotalAmt": money(total),
                "Line": [ { "Amount": money(total), "LinkedTxn": [ { "TxnId": bill_id, "TxnType": "Bill" } ] } ]
            })));
        }
    }

    out
}

/// Register the demo realm and write the book into `store`, 500 per commit.
pub fn seed(store: &Store, size: DemoSize, seed: u64, now: DateTime<Utc>) -> Result<usize, StoreError> {
    let realm = RealmId::parse(DEMO_REALM).expect("constant is a valid realm id");
    store.register_realm(&realm, "Demo Company (synthetic)", now)?;
    // Synthetic, so it is treated as a sandbox: writes can be switched on for
    // it to exercise the write path without any soak period.
    store.set_realm_environment(&realm, crate::store::outbox_store::Environment::Sandbox)?;
    let book = generate(size, seed, now);
    for chunk in book.chunks(500) {
        store.apply_batch(&realm, chunk, None, now)?;
    }
    seed_manufacturing(store, &realm, now)?;
    Ok(book.len())
}

/// Net board-feet in one finished unit of each foam product; the rest are
/// bought in or assembled, and cost by parts and labour only.
const FOAM_NET_BF: &[(&str, &str)] = &[
    ("Rescue tube", "2.1"),
    ("Kickboard", "0.9"),
    ("Dock bumper", "3.2"),
    ("Pool noodle", "0.6"),
    ("Swim barbell", "1.1"),
    ("Lane line float", "0.5"),
    ("Anti-fatigue mat", "5"),
];

/// Materials by colour, three receipts with freight, a build sheet for every
/// product, and a few runs — one of them over its sheet.
fn seed_manufacturing(store: &Store, realm: &RealmId, now: DateTime<Utc>) -> Result<(), StoreError> {
    use std::str::FromStr;

    use ledger_core::Money;
    use rust_decimal::Decimal;

    use crate::mfg::{self, ActualMaterial, Component, ReceiptLine};
    use crate::store::mfg_store::Material;

    let d = |raw: &str| Decimal::from_str(raw).expect("constant decimal");
    for color in COLORS {
        store.save_material(realm, &Material {
            material_id: format!("xlpe-2-{color}"),
            name: format!("XLPE 2 in {color}"),
            thickness_in: d("2"),
            width_in: d("48"),
            length_in: d("96"),
            color: Some(color.to_string()),
        }, now)?;
    }
    // Three deliveries: prices creep up, freight varies, colours mix.
    let deliveries = [("2026-06-10", "86.40", "412.00"), ("2026-08-14", "89.60", "455.50"), ("2026-10-02", "92.80", "398.25")];
    for (n, (on, price, freight)) in deliveries.iter().enumerate() {
        let lines: Vec<ReceiptLine> = COLORS
            .iter()
            .enumerate()
            .map(|(i, color)| ReceiptLine {
                material_id: format!("xlpe-2-{color}"),
                sheets: Decimal::from(8 + ((i + n) % 4) as i64 * 4),
                bf_per_sheet: d("64"),
                price_per_sheet: d(price) + Decimal::from((i % 3) as i64),
                surcharge_per_sheet: d("2.75"),
            })
            .collect();
        let freight = Money::from_minor((d(freight) * Decimal::from(100)).try_into().expect("cents"));
        let landed = mfg::landed_cost(&lines, freight).expect("valid constant receipt");
        store.record_receipt(realm, &format!("demo-rcpt-{n}"), on, None, Some(&format!("PO-{}", 4400 + n)), freight, &lines, &landed, now)?;
    }

    let prices = store.material_prices(realm)?;
    let mut runs = 0;
    for item in store.items(realm)? {
        let product = FOAM_NET_BF.iter().find(|(p, _)| item.name.starts_with(p));
        let color = COLORS.iter().find(|c| item.name.contains(*c)).copied().unwrap_or("blue");
        let mut components = Vec::new();
        if let Some((_, net)) = product {
            components.push(Component::Material { material_id: format!("xlpe-2-{color}"), net_bf: d(net), yield_pct: d("78") });
            components.push(Component::Labour { name: "CNC cut".into(), minutes: d("4"), rate_per_hour: d("45") });
        } else {
            components.push(Component::Part { name: "Bought-in components".into(), item_id: None, qty: d("1"), unit_cost: d("12.50") });
        }
        components.push(Component::Labour { name: "Finish and pack".into(), minutes: d("6"), rate_per_hour: d("28") });
        store.save_build_sheet(realm, &item.qbo_id, &components, now)?;

        if product.is_some() && runs < 6 {
            // Every third run uses 9% more foam than the sheet allows.
            let quantity = Decimal::from(50 + runs * 25);
            let standard = mfg::build_variance(&components, &prices, quantity, &[]).expect("valid sheet");
            let actual: Vec<ActualMaterial> = standard
                .lines
                .iter()
                .map(|l| ActualMaterial {
                    material_id: l.material_id.clone(),
                    board_feet: mfg::rate(l.standard_bf * if runs % 3 == 2 { d("1.09") } else { d("0.99") }),
                })
                .collect();
            let variance = mfg::build_variance(&components, &prices, quantity, &actual).expect("valid run");
            let on = format!("2026-10-{:02}", 3 + runs * 2);
            store.record_build(realm, &format!("demo-build-{runs}"), &item.qbo_id, &on, &components, &actual, &variance, None, now)?;
            runs += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::DocumentType;
    use crate::store::ProjectedTable;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap()
    }

    fn small() -> DemoSize {
        DemoSize { customers: 20, vendors: 5, items: 30, invoices: 150, purchase_orders: 40 }
    }

    #[test]
    fn the_same_seed_produces_the_same_book() {
        assert_eq!(generate(small(), 7, now()), generate(small(), 7, now()));
        assert_ne!(generate(small(), 7, now()), generate(small(), 8, now()));
    }

    #[test]
    fn the_book_projects_cleanly() {
        let store = Store::open_in_memory().unwrap();
        let written = seed(&store, small(), 7, now()).unwrap();
        let realm = RealmId::parse(DEMO_REALM).unwrap();
        assert!(written > 150);
        assert!(store.quarantined(&realm).unwrap().is_empty(), "{:?}", store.quarantined(&realm));
        assert_eq!(store.list_documents(&realm, DocumentType::Invoice, 1_000).unwrap().len(), 150);
        assert!(store.count_projected(&realm, ProjectedTable::DocumentLinks).unwrap() > 0);
        assert_eq!(store.orphaned_lines(&realm).unwrap(), 0);

        // Every product is costed, and the seeded runs include an overrun.
        let prices = store.material_prices(&realm).unwrap();
        for (_, components, _) in store.build_sheets(&realm).unwrap() {
            crate::mfg::unit_cost(&components, &prices).unwrap();
        }
        assert!(store.builds(&realm, None).unwrap().iter().any(|b| b.flagged));
    }

    #[test]
    fn nothing_in_the_book_looks_like_a_real_contact() {
        // Emails are on the reserved .invalid TLD, so none can ever deliver.
        for entity in generate(small(), 7, now()) {
            if let Some(email) = entity.raw_json.pointer("/PrimaryEmailAddr/Address") {
                assert!(email.as_str().unwrap().ends_with("@example.invalid"));
            }
        }
    }
}
