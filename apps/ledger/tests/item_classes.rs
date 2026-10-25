//! D33's item → class worksheet end to end, on an invented replica: the
//! suggestions and sales `ledger classes suggest` lists, the refusals and the
//! batch merge of `ledger classes apply`, and the map reaching the importer —
//! a Q1 2027 invoice the full posting gate refuses until Dan has classed its
//! item, and the slim import's items carrying the class.

use std::process::Command;

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::books;
use ledger::import::ImportOptions;
use ledger::item_classes::{self, Rules, WorksheetError, CSV_HEADER, DEFAULT_RULES};
use ledger::pipeline::import_replica;
use ledger::slim::{self, SlimOptions};
use ledger::store::Ledger;
use ledger::types::ClassId;
use ledger_core::Money;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

const CO: &str = "aquamentor";

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2027, 1, 20, 12, 0, 0).unwrap()
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn seed(store: &Store, entity_type: EntityType, id: &str, raw: Value) {
    let entity = MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "1".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    };
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();
}

fn invoice(store: &Store, id: &str, date: &str, lines: &[(&str, f64)]) {
    let total: f64 = lines.iter().map(|(_, amount)| amount).sum();
    let lines: Vec<Value> = lines
        .iter()
        .enumerate()
        .map(|(n, (item, amount))| {
            json!({ "Id": (n + 1).to_string(), "LineNum": n + 1, "Amount": amount,
                    "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": item }, "Qty": 1,
                                             "UnitPrice": amount, "TaxCodeRef": { "value": "NON" } } })
        })
        .collect();
    seed(
        store,
        EntityType::Invoice,
        id,
        json!({ "Id": id, "DocNumber": id, "TxnDate": date, "CustomerRef": { "value": "31" },
                "TotalAmt": total, "Balance": total, "Line": lines }),
    );
}

/// Items:
///
/// | id  | what                       | suggestion            |
/// |-----|----------------------------|-----------------------|
/// | 501 | rescue tube, SKU RT-50     | foam (rescue-tubes)   |
/// | 502 | "1 AM LGC 72" chair        | chair                 |
/// | 503 | UPS ground shipping        | none                  |
/// | 504 | kickboard in AEGIS:KICKBOARD | foam (aegis-brand)  |
/// | 505 | inactive tube              | not listed            |
/// | 506 | a category                 | not listed            |
///
/// Sales in the twelve months to 2026-12-31: 501 two lines, 900.00; 502 one
/// line, 1,500.00; 503 one line, 40.00. A 2025 invoice for 504 is outside
/// the window.
fn replica() -> Store {
    let store = Store::open_in_memory().unwrap();
    build(&store);
    store
}

fn build(store: &Store) {
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        store,
        EntityType::Customer,
        "31",
        json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
    );
    for (id, raw) in [
        (
            "AR1",
            json!({ "Name": "Accounts Receivable", "AccountType": "Accounts Receivable" }),
        ),
        (
            "INC1",
            json!({ "Name": "Sales", "AccountType": "Income", "AccountSubType": "SalesOfProductIncome" }),
        ),
        (
            "UPS1",
            json!({ "Name": "UPS", "AccountType": "Income", "AccountSubType": "SalesOfProductIncome" }),
        ),
        (
            "COGS1",
            json!({ "Name": "Cost of Goods Sold", "AccountType": "Cost of Goods Sold" }),
        ),
    ] {
        let mut raw = raw;
        raw["Id"] = json!(id);
        raw["Active"] = json!(true);
        seed(store, EntityType::Account, id, raw);
    }
    let item = |id: &str, name: &str, sku: Option<&str>, kind: &str, income: &str, extra: Value| {
        let mut raw = json!({ "Id": id, "Name": name, "FullyQualifiedName": name, "Type": kind,
                              "IncomeAccountRef": { "value": income },
                              "ExpenseAccountRef": { "value": "COGS1" }, "Active": true });
        if let Some(sku) = sku {
            raw["Sku"] = json!(sku);
        }
        for (key, value) in extra.as_object().unwrap() {
            raw[key] = value.clone();
        }
        seed(store, EntityType::Item, id, raw);
    };
    item(
        "501",
        "Rescue Tube 50in",
        Some("RT-50"),
        "NonInventory",
        "INC1",
        json!({}),
    );
    item(
        "502",
        "1 AM LGC 72",
        None,
        "NonInventory",
        "INC1",
        json!({}),
    );
    item("503", "SHIP-U-GND", None, "Service", "UPS1", json!({}));
    item(
        "504",
        "Kids kickboard",
        None,
        "NonInventory",
        "INC1",
        json!({ "FullyQualifiedName": "AEGIS:KICKBOARD:Kids kickboard" }),
    );
    item(
        "505",
        "Old tube",
        Some("RT-OLD"),
        "NonInventory",
        "INC1",
        json!({ "Active": false }),
    );
    item("506", "AEGIS", None, "Category", "INC1", json!({}));

    invoice(
        store,
        "1001",
        "2026-03-02",
        &[("501", 400.0), ("502", 1500.0)],
    );
    invoice(
        store,
        "1002",
        "2026-11-20",
        &[("501", 500.0), ("503", 40.0)],
    );
    invoice(store, "900", "2025-06-01", &[("504", 75.0)]);
}

fn book() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(CO, "Aquamentor", None, now())
        .unwrap();
    ledger
}

fn reviewed(rows: &str) -> Vec<item_classes::ReviewedRow> {
    item_classes::parse_reviewed(&format!("{CSV_HEADER}\n{rows}")).unwrap()
}

fn refusal(result: Result<item_classes::ApplyReport, WorksheetError>) -> Vec<String> {
    match result {
        Err(WorksheetError::Refused(problems)) => problems,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_worksheet_lists_active_items_by_sales_with_the_rule_that_fired() {
    let store = replica();
    let rules = Rules::parse(DEFAULT_RULES, CO).unwrap();
    let rows = item_classes::worksheet(&store, &realm(), &rules, ymd(2026, 12, 31)).unwrap();

    let ids: Vec<&str> = rows.iter().map(|r| r.item.qbo_id.as_str()).collect();
    assert_eq!(ids, ["502", "501", "503", "504"]);

    let by_id = |id: &str| rows.iter().find(|r| r.item.qbo_id == id).unwrap();
    let chair = by_id("502");
    assert_eq!(
        (chair.sales_lines, chair.sales_amount),
        (1, Money::from_minor(150_000))
    );
    assert_eq!(chair.suggested_class.as_deref(), Some("chair"));
    assert_eq!(chair.rule.as_deref(), Some("lifeguard-chairs"));

    let tube = by_id("501");
    assert_eq!(
        (tube.sales_lines, tube.sales_amount),
        (2, Money::from_minor(90_000))
    );
    assert_eq!(tube.rule.as_deref(), Some("rescue-tubes"));
    assert_eq!(tube.item.income_account.as_deref(), Some("Sales"));

    let shipping = by_id("503");
    assert_eq!(shipping.suggested_class, None);
    assert_eq!(shipping.rule, None);

    let kickboard = by_id("504");
    assert_eq!(
        kickboard.sales_lines, 0,
        "the 2025 sale is outside the window"
    );
    assert_eq!(kickboard.item.category.as_deref(), Some("AEGIS:KICKBOARD"));
    assert_eq!(kickboard.rule.as_deref(), Some("aegis-brand"));

    let summary = item_classes::summarize(&rows);
    assert_eq!(summary.items, 4);
    assert_eq!(summary.items_sold, 3);
    assert_eq!(summary.suggested, 3);
    assert_eq!(summary.sales_total, Money::from_minor(244_000));
    assert_eq!(summary.sales_covered, Money::from_minor(240_000));

    let csv = item_classes::render_csv(&rows);
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some(CSV_HEADER));
    assert_eq!(
        lines.next(),
        Some("502,,1 AM LGC 72,NonInventory,,Sales,1,1500.00,chair,lifeguard-chairs,")
    );
    // What suggest writes reads straight back, every class still empty.
    let back = item_classes::parse_reviewed(&csv).unwrap();
    assert_eq!(back.len(), 4);
    assert!(back.iter().all(|row| row.class.is_empty()));
}

#[test]
fn apply_refuses_the_whole_file_on_a_missing_or_unknown_class_and_writes_nothing() {
    let ledger = book();
    let rows = reviewed(
        "501,RT-50,Rescue Tube 50in,,,,,,foam,rescue-tubes,\n\
         502,,1 AM LGC 72,,,,,,chair,lifeguard-chairs,Chairs\n\
         503,,SHIP-U-GND,,,,,,,,unclassified\n\
         504,,Kids kickboard,,,,,,foam,aegis-brand,widgets\n\
         505,,Old tube,,,,,,,,foam\n\
         505,,Old tube,,,,,,,,sign\n",
    );
    let problems = refusal(item_classes::apply(&ledger, CO, &rows, "dan", now()));
    assert_eq!(problems.len(), 5, "{problems:#?}");
    assert!(problems[0].contains("line 2 (item 501 \"Rescue Tube 50in\"): no class"));
    assert!(problems[1].contains("class \"chairs\" is not in the book (one of: chair, cnc,"));
    assert!(problems[2].contains("import-only fallback"));
    assert!(problems[3].contains("class \"widgets\" is not in the book"));
    assert!(problems[4].contains("line 7 (item 505 \"Old tube\"): class \"sign\", but line 6"));
    assert!(item_classes::item_class_map(&ledger, CO)
        .unwrap()
        .is_empty());

    // The suggestion is never read: 501 suggested foam, its class is blank.
    let only_suggested = reviewed("501,,,,,,,,foam,rescue-tubes,\n");
    refusal(item_classes::apply(
        &ledger,
        CO,
        &only_suggested,
        "dan",
        now(),
    ));
}

#[test]
fn apply_writes_the_map_and_a_later_batch_merges_into_it() {
    let ledger = book();
    let first = reviewed(
        "501,,,,,,,,foam,,foam\n\
         502,,,,,,,,chair,,Chair\n",
    );
    let report = item_classes::apply(&ledger, CO, &first, "dan", now()).unwrap();
    assert_eq!((report.added, report.changed, report.unchanged), (2, 0, 0));

    let second = reviewed(
        "502,,,,,,,,,,chair\n\
         503,,,,,,,,,,drop\n\
         501,,,,,,,,,,drop\n",
    );
    let report = item_classes::apply(&ledger, CO, &second, "dan", now()).unwrap();
    assert_eq!((report.added, report.changed, report.unchanged), (1, 1, 1));
    assert_eq!(report.map_per_class.get("drop"), Some(&2));

    let map = item_classes::item_class_map(&ledger, CO).unwrap();
    assert_eq!(map.get("501"), Some(&ClassId("drop".into())));
    assert_eq!(map.get("502"), Some(&ClassId("chair".into())));
    assert_eq!(map.get("503"), Some(&ClassId("drop".into())));
}

/// D33's whole point: under the full gate (no fallback class), a 2027
/// invoice is refused while its item has no class, and posts with the
/// class Dan gave the item once the map holds it.
#[test]
fn the_q1_replay_takes_an_items_class_from_the_map() {
    let store = replica();
    invoice(&store, "2001", "2027-01-05", &[("501", 50.0)]);
    let full_gate = ImportOptions {
        from: Some(ymd(2027, 1, 1)),
        ..ImportOptions::default()
    };

    let unclassed = book();
    let report = import_replica(&unclassed, &store, &realm(), CO, &full_gate, now()).unwrap();
    assert_eq!(report.posted, 0);
    assert_eq!(report.rejected.len(), 1, "{:?}", report.rejected);

    let ledger = book();
    item_classes::apply(&ledger, CO, &reviewed("501,,,,,,,,,,foam\n"), "dan", now()).unwrap();
    let report = import_replica(&ledger, &store, &realm(), CO, &full_gate, now()).unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(report.posted, 1);
    let entries = ledger.entries_for_document(CO, "qbo:Invoice:2001").unwrap();
    let income: Vec<_> = entries[0]
        .1
        .lines
        .iter()
        .filter(|line| line.credit.minor() > 0)
        .collect();
    assert_eq!(income.len(), 1);
    assert_eq!(income[0].class, Some(ClassId("foam".into())));
}

#[test]
fn the_slim_import_brings_items_across_with_the_mapped_class() {
    let store = replica();
    let ledger = book();
    item_classes::apply(&ledger, CO, &reviewed("502,,,,,,,,,,chair\n"), "dan", now()).unwrap();
    let options = SlimOptions {
        as_of: ymd(2026, 12, 31),
        qbo_tb: Vec::new(),
        qbo_ar: None,
        qbo_ap: None,
    };
    let report = slim::import_slim(&ledger, &store, &realm(), CO, &options, now()).unwrap();
    let chair = books::item(&ledger, CO, "502").unwrap().unwrap();
    assert_eq!(chair.default_class, Some(ClassId("chair".into())));
    let tube = books::item(&ledger, CO, "501").unwrap().unwrap();
    assert_eq!(tube.default_class, None);
    // 501, 503 and 504 still have none (505 is inactive but still an item).
    assert_eq!(report.items_without_class, report.items - 1);
}

#[test]
fn the_binary_suggests_from_a_read_only_replica_and_applies_the_reviewed_class() {
    let dir = tempfile::tempdir().unwrap();
    let replica_path = dir.path().join("replica.sqlite");
    build(&Store::open(&replica_path).unwrap());
    let worksheet = dir.path().join("worksheet.csv");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ledger"))
            .args(args)
            .output()
            .unwrap()
    };

    let out = run(&[
        "classes",
        "suggest",
        "--replica",
        replica_path.to_str().unwrap(),
        "--out",
        worksheet.to_str().unwrap(),
        "--as-of",
        "2026-12-31",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("4 active items, 3 sold"), "{stdout}");
    assert!(stdout.contains("(98.4%)"), "{stdout}");
    let csv = std::fs::read_to_string(&worksheet).unwrap();
    assert!(csv.starts_with(CSV_HEADER));

    // Dan fills in only the class column (here: copies the suggestion, and
    // decides shipping himself) and applies.
    let mut filled = String::new();
    for (n, line) in csv.lines().enumerate() {
        if n == 0 {
            filled.push_str(line);
        } else {
            let suggested = line.split(',').nth(8).unwrap();
            let class = if suggested.is_empty() {
                "drop"
            } else {
                suggested
            };
            filled.push_str(&format!("{}{class}", line));
        }
        filled.push('\n');
    }
    let reviewed_path = dir.path().join("reviewed.csv");
    std::fs::write(&reviewed_path, &filled).unwrap();
    let db = dir.path().join("book.sqlite");
    let init = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        CO,
        "--name",
        "Aquamentor",
    ]);
    assert!(init.status.success());
    let out = run(&[
        "classes",
        "apply",
        "--db",
        db.to_str().unwrap(),
        "--company",
        CO,
        "--csv",
        reviewed_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("4 added"));

    // An unfilled worksheet is refused, exit 1, nothing more written.
    let out = run(&[
        "classes",
        "apply",
        "--db",
        db.to_str().unwrap(),
        "--company",
        CO,
        "--csv",
        worksheet.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no class"));
    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(item_classes::item_class_map(&ledger, CO).unwrap().len(), 4);
}
