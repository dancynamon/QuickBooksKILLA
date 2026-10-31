//! D28's slim cutover import, end to end, against an invented replica
//! (HANDOFF.md §2.6): masters, the opening entry, the open documents as of
//! 31 December 2026, and every check — plus the re-run after a late change,
//! the Q1 replay relieving an imported invoice, and the binary's dry run.
//!
//! The book as of 2026-12-31, in QBO's terms:
//!
//! | customer 31 Blue Harbor | due        | open |
//! |-------------------------|------------|-----:|
//! | invoice 1001            | 2026-12-01 |  500 |
//! | invoice 1002 (100 paid) | 2026-10-31 |  200 |
//! | credit memo 1005        | 2026-12-20 |  -50 |
//! | journal entry 900 (AR)  | 2026-12-15 |  120 |
//!
//! | customer 32 Lakeside    | due        | open |
//! |-------------------------|------------|-----:|
//! | invoice 1003 (paid 1/5/27) | 2027-01-09 | 400 |
//! | payment, unapplied      | 2026-12-28 |  -75 |
//!
//! AR 1,095 (QBO), so 1200 = 1,170 and 2300 = -75 in the ledger. AP: bill
//! B-1, 600, vendor 77.

use std::process::Command;

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::books::{self, Book};
use ledger::chart;
use ledger::import::ImportOptions;
use ledger::nightly::{self, NightlyError};
use ledger::pipeline::import_replica;
use ledger::report::{self, QboTbRow};
use ledger::slim::{self, QboAgingRow, SlimOptions, SlimReport};
use ledger::store::Ledger;
use ledger::types::{ContactKind, DocKind, LineKind};
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

fn usd(minor: i64) -> Money {
    Money::from_minor(minor)
}

fn as_of() -> NaiveDate {
    ymd(2026, 12, 31)
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

fn item_line(item: &str, amount: f64) -> Value {
    json!({ "Id": "1", "LineNum": 1, "Amount": amount, "DetailType": "SalesItemLineDetail",
            "SalesItemLineDetail": { "ItemRef": { "value": item }, "Qty": 1, "UnitPrice": amount } })
}

fn expense_line(amount: f64) -> Value {
    json!({ "Id": "1", "LineNum": 1, "Amount": amount, "DetailType": "AccountBasedExpenseLineDetail",
            "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } } })
}

fn payment(
    store: &Store,
    id: &str,
    customer: &str,
    date: &str,
    total: f64,
    unapplied: f64,
    applied: &[(&str, f64)],
) {
    let lines: Vec<Value> = applied
        .iter()
        .map(|(invoice, amount)| {
            json!({ "Amount": amount, "LinkedTxn": [{ "TxnId": invoice, "TxnType": "Invoice" }] })
        })
        .collect();
    seed(
        store,
        EntityType::Payment,
        id,
        json!({ "Id": id, "TxnDate": date, "CustomerRef": { "value": customer },
                "TotalAmt": total, "UnappliedAmt": unapplied,
                "DepositToAccountRef": { "value": "UDF1" }, "Line": lines }),
    );
}

/// The replica described in the module docs. `invoice_1001_paid` adds a
/// payment on 30 December that settles invoice 1001: the late change Joel's
/// close might bring.
/// The replica every run starts from, with an empty, complete sales-order
/// feed applied (D42: the run fails without one).
fn build_replica(store: &Store, invoice_1001_paid: bool) {
    build_replica_without_feed(store, invoice_1001_paid);
    apply_sales_orders(store, json!([]));
}

fn apply_sales_orders(store: &Store, orders: Value) {
    let feed = serde_json::from_value(json!({
        "feed": qbo_local::sales_orders::FEED_FORMAT, "realmId": realm().as_str(),
        "capturedAt": "2027-01-02T12:00:00Z", "complete": true, "salesOrders": orders,
    }))
    .unwrap();
    store
        .apply_sales_order_feed(&realm(), &feed, "test", now())
        .unwrap();
}

fn build_replica_without_feed(store: &Store, invoice_1001_paid: bool) {
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    for (id, raw) in [
        (
            "AR1",
            json!({ "Name": "Accounts Receivable", "AccountType": "Accounts Receivable" }),
        ),
        (
            "AP1",
            json!({ "Name": "Accounts Payable", "AccountType": "Accounts Payable" }),
        ),
        (
            "BANK1",
            json!({ "Name": "Checking", "AccountType": "Bank" }),
        ),
        (
            "UDF1",
            json!({ "Name": "Undeposited Funds", "AccountType": "Other Current Asset",
                          "AccountSubType": "UndepositedFunds" }),
        ),
        (
            "INC1",
            json!({ "Name": "Sales", "AccountType": "Income",
                          "AccountSubType": "SalesOfProductIncome" }),
        ),
        (
            "COGS1",
            json!({ "Name": "Cost of Goods Sold", "AccountType": "Cost of Goods Sold" }),
        ),
        (
            "INV1",
            json!({ "Name": "Inventory Asset", "AccountType": "Other Current Asset",
                          "AccountSubType": "Inventory" }),
        ),
        (
            "EXP1",
            json!({ "Name": "Shop Supplies", "AccountType": "Expense",
                          "AccountSubType": "SuppliesMaterials" }),
        ),
        (
            "RE1",
            json!({ "Name": "Retained Earnings", "AccountType": "Equity",
                         "AccountSubType": "RetainedEarnings" }),
        ),
    ] {
        let mut raw = raw;
        raw["Id"] = json!(id);
        raw["Active"] = json!(true);
        seed(store, EntityType::Account, id, raw);
    }
    seed(
        store,
        EntityType::Class,
        "4",
        json!({ "Id": "4", "Name": "Foam Products", "Active": true }),
    );
    seed(
        store,
        EntityType::Term,
        "3",
        json!({ "Id": "3", "Name": "Net 30", "DueDays": 30, "Active": true }),
    );
    seed(
        store,
        EntityType::Term,
        "4",
        // Shaped like the live replica's date-driven terms (D41).
        json!({ "Id": "4", "Name": "10th of month", "Type": "DATE_DRIVEN", "DayOfMonthDue": 10,
                "DueNextMonthDays": 5, "DiscountDayOfMonth": 1, "DiscountPercent": 0,
                "Active": true }),
    );

    seed(
        store,
        EntityType::Customer,
        "31",
        json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true, "Taxable": true,
                "SalesTermRef": { "value": "3" },
                "BillAddr": { "Line1": "1 Harbor Rd", "City": "Toms River", "CountrySubDivisionCode": "NJ",
                              "PostalCode": "08753" } }),
    );
    seed(
        store,
        EntityType::Customer,
        "32",
        json!({ "Id": "32", "DisplayName": "Lakeside YMCA", "Active": true, "Taxable": false,
                "SalesTermRef": { "value": "4" } }),
    );
    seed(
        store,
        EntityType::Vendor,
        "77",
        json!({ "Id": "77", "DisplayName": "Foam Supply Co", "Active": true, "TermRef": { "value": "3" } }),
    );

    seed(
        store,
        EntityType::Item,
        "501",
        json!({ "Id": "501", "Name": "Rescue Tube 50in", "Sku": "RT-50", "Type": "Inventory",
                "UnitPrice": 100, "PurchaseCost": 41.5, "Active": true, "ClassRef": { "value": "4" },
                "IncomeAccountRef": { "value": "INC1" }, "ExpenseAccountRef": { "value": "COGS1" },
                "AssetAccountRef": { "value": "INV1" } }),
    );
    seed(
        store,
        EntityType::Item,
        "502",
        json!({ "Id": "502", "Name": "Kickboard", "Type": "NonInventory", "UnitPrice": 25,
                "Active": true, "IncomeAccountRef": { "value": "INC1" } }),
    );
    seed(
        store,
        EntityType::Item,
        "503",
        json!({ "Id": "503", "Name": "Foam", "Type": "Category", "Active": true }),
    );
    seed(
        store,
        EntityType::Item,
        "504",
        json!({ "Id": "504", "Name": "Rescue Tube (old)", "Sku": "RT-50", "Type": "Service", "Active": false }),
    );

    // --- receivables ---------------------------------------------------------
    let invoice = |id: &str,
                   number: &str,
                   customer: &str,
                   date: &str,
                   due: &str,
                   item: &str,
                   total: f64,
                   balance: f64| {
        seed(
            store,
            EntityType::Invoice,
            id,
            json!({ "Id": id, "DocNumber": number, "TxnDate": date, "DueDate": due,
                    "CustomerRef": { "value": customer }, "TotalAmt": total, "Balance": balance,
                    "Line": [item_line(item, total)] }),
        );
    };
    invoice(
        "I1",
        "1001",
        "31",
        "2026-11-01",
        "2026-12-01",
        "501",
        500.0,
        if invoice_1001_paid { 0.0 } else { 500.0 },
    );
    invoice(
        "I2",
        "1002",
        "31",
        "2026-10-01",
        "2026-10-31",
        "502",
        300.0,
        200.0,
    );
    invoice(
        "I3",
        "1003",
        "32",
        "2026-12-10",
        "2027-01-09",
        "501",
        400.0,
        0.0,
    );
    invoice(
        "I4",
        "1200",
        "32",
        "2026-06-01",
        "2026-07-01",
        "502",
        250.0,
        0.0,
    );
    payment(
        store,
        "P1",
        "31",
        "2026-11-15",
        100.0,
        0.0,
        &[("I2", 100.0)],
    );
    payment(
        store,
        "P2",
        "32",
        "2027-01-05",
        400.0,
        0.0,
        &[("I3", 400.0)],
    );
    payment(
        store,
        "P3",
        "32",
        "2026-07-01",
        250.0,
        0.0,
        &[("I4", 250.0)],
    );
    payment(store, "P4", "32", "2026-12-28", 75.0, 75.0, &[]);
    if invoice_1001_paid {
        payment(
            store,
            "P5",
            "31",
            "2026-12-30",
            500.0,
            0.0,
            &[("I1", 500.0)],
        );
    }
    seed(
        store,
        EntityType::CreditMemo,
        "C1",
        json!({ "Id": "C1", "DocNumber": "1005", "TxnDate": "2026-12-20", "CustomerRef": { "value": "31" },
                "TotalAmt": 50, "RemainingCredit": 50, "Line": [item_line("502", 50.0)] }),
    );
    seed(
        store,
        EntityType::JournalEntry,
        "J900",
        json!({ "Id": "J900", "DocNumber": "900", "TxnDate": "2026-12-15", "TotalAmt": 120,
                "Line": [
                    { "Id": "0", "Amount": 120, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Debit", "AccountRef": { "value": "AR1" },
                        "Entity": { "Type": "Customer", "EntityRef": { "value": "31" } } } },
                    { "Id": "1", "Amount": 120, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Credit", "AccountRef": { "value": "INC1" } } }
                ] }),
    );

    // --- payables ------------------------------------------------------------
    seed(
        store,
        EntityType::Bill,
        "B1",
        json!({ "Id": "B1", "DocNumber": "FS-778", "TxnDate": "2026-12-01", "DueDate": "2026-12-31",
                "VendorRef": { "value": "77" }, "TotalAmt": 600, "Balance": 600,
                "Line": [expense_line(600.0)] }),
    );
    seed(
        store,
        EntityType::Bill,
        "B2",
        json!({ "Id": "B2", "DocNumber": "FS-700", "TxnDate": "2026-11-01", "DueDate": "2026-12-01",
                "VendorRef": { "value": "77" }, "TotalAmt": 200, "Balance": 0,
                "Line": [expense_line(200.0)] }),
    );
    seed(
        store,
        EntityType::BillPayment,
        "BP1",
        json!({ "Id": "BP1", "TxnDate": "2026-11-20", "VendorRef": { "value": "77" }, "TotalAmt": 200,
                "PayType": "Check", "CheckPayment": { "BankAccountRef": { "value": "BANK1" } },
                "Line": [{ "Amount": 200, "LinkedTxn": [{ "TxnId": "B2", "TxnType": "Bill" }] }] }),
    );

    // --- contracts -----------------------------------------------------------
    for (id, number, status) in [("E7", "2001", "Pending"), ("E8", "2000", "Closed")] {
        seed(
            store,
            EntityType::Estimate,
            id,
            json!({ "Id": id, "DocNumber": number, "TxnDate": "2026-12-05", "TxnStatus": status,
                    "CustomerRef": { "value": "31" }, "TotalAmt": 1000, "Line": [item_line("501", 1000.0)] }),
        );
    }
    for (id, number, status) in [("PO1", "3001", "Open"), ("PO2", "3000", "Closed")] {
        seed(
            store,
            EntityType::PurchaseOrder,
            id,
            json!({ "Id": id, "DocNumber": number, "TxnDate": "2026-12-02", "POStatus": status,
                    "VendorRef": { "value": "77" }, "TotalAmt": 800,
                    "Line": [{ "Id": "1", "Amount": 800, "DetailType": "ItemBasedExpenseLineDetail",
                               "ItemBasedExpenseLineDetail": { "ItemRef": { "value": "501" }, "Qty": 20 } }] }),
        );
    }
}

fn replica(invoice_1001_paid: bool) -> Store {
    let store = Store::open_in_memory().unwrap();
    build_replica(&store, invoice_1001_paid);
    store
}

/// QBO's trial balance as of 31 December; with invoice 1001 paid, 500 moves
/// from AR to undeposited funds.
fn qbo_tb(invoice_1001_paid: bool) -> Vec<QboTbRow> {
    let (ar, udf) = if invoice_1001_paid {
        (59_500, 50_000)
    } else {
        (109_500, 0)
    };
    [
        ("BANK1", "Checking", 1_000_000),
        ("UDF1", "Undeposited Funds", udf),
        ("AR1", "Accounts Receivable", ar),
        ("INV1", "Inventory Asset", 200_000),
        ("AP1", "Accounts Payable", -60_000),
        ("INC1", "Sales", -900_000),
        ("COGS1", "Cost of Goods Sold", 300_000),
        ("EXP1", "Shop Supplies", 50_000),
        ("RE1", "Retained Earnings", -699_500),
    ]
    .into_iter()
    .map(|(id, name, minor)| QboTbRow {
        qbo_account_id: id.into(),
        name: name.into(),
        balance: usd(minor),
    })
    .collect()
}

fn aging_row(id: &str, name: &str, buckets: [i64; 5]) -> QboAgingRow {
    QboAgingRow {
        entity_id: Some(id.into()),
        name: name.into(),
        current: usd(buckets[0]),
        days_1_30: usd(buckets[1]),
        days_31_60: usd(buckets[2]),
        days_61_90: usd(buckets[3]),
        over_90: usd(buckets[4]),
        total: usd(buckets.iter().sum()),
    }
}

fn qbo_ar(invoice_1001_paid: bool) -> Vec<QboAgingRow> {
    let one_to_thirty = if invoice_1001_paid { 7_000 } else { 57_000 };
    vec![
        aging_row(
            "31",
            "Blue Harbor Swim Club",
            [0, one_to_thirty, 0, 20_000, 0],
        ),
        aging_row("32", "Lakeside YMCA", [40_000, -7_500, 0, 0, 0]),
    ]
}

fn qbo_ap() -> Vec<QboAgingRow> {
    vec![aging_row("77", "Foam Supply Co", [60_000, 0, 0, 0, 0])]
}

fn options(invoice_1001_paid: bool, with_aging: bool) -> SlimOptions {
    SlimOptions {
        as_of: as_of(),
        qbo_tb: qbo_tb(invoice_1001_paid),
        qbo_ar: with_aging.then(|| qbo_ar(invoice_1001_paid)),
        qbo_ap: with_aging.then(qbo_ap),
    }
}

fn book() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    assert!(slim::ensure_company(&ledger, CO, &realm(), now()).unwrap());
    ledger
}

fn run(ledger: &Ledger, store: &Store, options: &SlimOptions) -> SlimReport {
    slim::import_slim(ledger, store, &realm(), CO, options, now()).unwrap()
}

fn balance(ledger: &Ledger, number: &str, on: NaiveDate) -> Money {
    report::trial_balance(ledger, CO, on)
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.number == number)
        .map_or(Money::ZERO, |row| row.balance)
}

fn failed(report: &SlimReport) -> Vec<String> {
    report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {:?}", c.name, c.details))
        .collect()
}

#[test]
fn the_slim_import_brings_masters_opening_and_open_items_across_and_every_check_passes() {
    let ledger = book();
    let store = replica(false);
    let report = run(&ledger, &store, &options(false, true));
    assert!(report.passed(), "{}", report.render_text(CO));

    // Masters, keyed by QBO id, terms folded in.
    let blue = books::contact(&ledger, CO, "31").unwrap().unwrap();
    assert_eq!(blue.terms_days, Some(30));
    assert_eq!(blue.source_ref.as_deref(), Some("31"));
    assert_eq!(
        blue.bill_address.as_deref(),
        Some("1 Harbor Rd\nToms River NJ 08753")
    );
    let lakeside = books::contact(&ledger, CO, "32").unwrap().unwrap();
    assert!(lakeside.is_tax_exempt);
    assert_eq!(
        (lakeside.terms_days, lakeside.terms()),
        (
            None,
            books::Terms::DayOfMonth {
                day: 10,
                next_month_within: 5
            }
        ),
        "a date-driven term comes across as a day of the month"
    );
    assert_eq!(lakeside.terms_source_ref.as_deref(), Some("4"));
    assert_eq!(blue.terms_source_ref.as_deref(), Some("3"));
    assert!(
        !report.warnings.iter().any(|w| w.contains("term 4")),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        books::contact(&ledger, CO, "77").unwrap().unwrap().kind,
        ContactKind::Vendor
    );
    assert_eq!((report.customers, report.vendors), (2, 1));

    let tube = books::item(&ledger, CO, "501").unwrap().unwrap();
    assert_eq!(tube.item_type, books::ItemType::Inventory);
    assert_eq!(
        tube.asset_account.as_ref().map(|a| a.0.as_str()),
        Some(chart::INVENTORY_FINISHED)
    );
    assert_eq!(tube.income_account.0, chart::SALES_INCOME);
    assert_eq!(
        tube.default_class.as_ref().map(|c| c.0.as_str()),
        Some("foam")
    );
    assert_eq!(tube.unit_cost, Some(usd(4_150)));
    assert!(
        books::item(&ledger, CO, "503").unwrap().is_none(),
        "a category is not an item"
    );
    let old = books::item(&ledger, CO, "504").unwrap().unwrap();
    assert_eq!(old.sku, None, "a duplicate QBO SKU is dropped, not fatal");
    assert!(!old.is_active);
    assert_eq!(report.items, 3);
    assert_eq!(report.items_without_class, 2);

    // Numbering carries on past QBO's highest, which is on a paid invoice.
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::Invoice).unwrap(),
        "1201"
    );
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::Estimate).unwrap(),
        "2002"
    );
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::PurchaseOrder).unwrap(),
        "3002"
    );
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::CreditMemo).unwrap(),
        "1006"
    );

    // The opening entry: QBO's AR, less the unapplied payment moved to 2300.
    assert_eq!(report.unapplied_reclass, usd(7_500));
    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_RECEIVABLE, as_of()),
        usd(117_000)
    );
    assert_eq!(
        balance(&ledger, chart::CUSTOMER_DEPOSITS, as_of()),
        usd(-7_500)
    );
    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_PAYABLE, as_of()),
        usd(-60_000)
    );
    assert_eq!(
        balance(&ledger, chart::OPENING_BALANCE_EQUITY, as_of()),
        Money::ZERO
    );
    assert_eq!(balance(&ledger, chart::CHECKING, as_of()), usd(1_000_000));

    // Open items, as of the date: invoice 1003 is open though QBO's Balance
    // says paid, because its payment is dated 5 January.
    let open = books::open_documents(&ledger, CO, Book::Receivable, as_of()).unwrap();
    let rows: Vec<_> = open
        .iter()
        .map(|d| (d.document_id.as_str(), d.open.minor()))
        .collect();
    assert_eq!(
        rows,
        [
            ("qbo:Invoice:I2", 20_000),
            ("qbo:Invoice:I1", 50_000),
            ("qbo:Invoice:I3", 40_000),
            ("qbo:JournalEntry:J900", 12_000),
            ("qbo:CreditMemo:C1", -5_000),
        ]
    );
    let saved = |id: &str| {
        serde_json::from_str::<ledger::types::LedgerDocument>(
            &ledger.latest_document_payload(CO, id).unwrap().unwrap(),
        )
        .unwrap()
    };
    // A fully open invoice keeps its lines; a partly paid one is one line.
    let i1 = saved("qbo:Invoice:I1");
    assert_eq!(i1.lines[0].kind, LineKind::Item);
    assert_eq!(i1.number.as_deref(), Some("1001"));
    let i2 = saved("qbo:Invoice:I2");
    assert_eq!(i2.lines.len(), 1);
    assert_eq!(i2.lines[0].amount, usd(20_000));
    assert!(i2.lines[0]
        .description
        .as_deref()
        .unwrap()
        .contains("100.00 settled in QBO"));
    // Paid in full before the date: not brought across.
    assert!(ledger
        .latest_document_payload(CO, "qbo:Invoice:I4")
        .unwrap()
        .is_none());
    assert!(ledger
        .latest_document_payload(CO, "qbo:Bill:B2")
        .unwrap()
        .is_none());
    let unapplied = saved("qbo:Payment:P4");
    assert_eq!(unapplied.unapplied, usd(7_500));
    assert!(unapplied.applications.is_empty());
    // Open contracts only.
    assert!(ledger
        .latest_document_payload(CO, "qbo:Estimate:E7")
        .unwrap()
        .is_some());
    assert!(ledger
        .latest_document_payload(CO, "qbo:Estimate:E8")
        .unwrap()
        .is_none());
    assert!(ledger
        .latest_document_payload(CO, "qbo:PurchaseOrder:PO1")
        .unwrap()
        .is_some());
    assert!(ledger
        .latest_document_payload(CO, "qbo:PurchaseOrder:PO2")
        .unwrap()
        .is_none());

    // Nothing open posted: the only entry is the opening one.
    for id in ["qbo:Invoice:I1", "qbo:Payment:P4", "qbo:Bill:B1"] {
        assert!(
            ledger.entries_for_document(CO, id).unwrap().is_empty(),
            "{id} posted"
        );
        assert!(books::carried_by_opening(&ledger, CO, id).unwrap());
    }
    assert!(!books::carried_by_opening(&ledger, CO, "qbo:Estimate:E7").unwrap());

    let ap = books::aging(&ledger, CO, Book::Payable, as_of()).unwrap();
    assert_eq!(ap.totals.total, usd(60_000));
    assert_eq!(ap.rows[0].contact_name, "Foam Supply Co");
    assert!(report
        .checks
        .iter()
        .any(|c| c.name.contains("sales orders mirrored") && c.passed));
}

#[test]
fn without_qbo_aging_the_balance_fields_are_refused_once_later_payments_moved_them() {
    let ledger = book();
    let report = run(&ledger, &replica(false), &options(false, false));
    let failures = failed(&report);
    assert!(
        failures
            .iter()
            .any(|f| f.starts_with("AR aging by customer") && f.contains("--qbo-ar")),
        "{failures:?}"
    );
    // The ties to 1200/2000 and the trial balance hold regardless.
    for check in &report.checks {
        if check.name.contains("tie to") || check.name.contains("trial balance") {
            assert!(check.passed, "{}: {:?}", check.name, check.details);
        }
    }
}

#[test]
fn a_qbo_aging_that_disagrees_fails_the_run_and_names_the_customer() {
    let ledger = book();
    let mut opts = options(false, true);
    opts.qbo_ar.as_mut().unwrap()[0].days_61_90 = usd(25_000);
    opts.qbo_ar.as_mut().unwrap()[0].total = usd(82_000);
    let report = run(&ledger, &replica(false), &opts);
    assert!(!report.passed());
    let check = report
        .checks
        .iter()
        .find(|c| c.name.starts_with("AR aging"))
        .unwrap();
    assert!(!check.passed);
    assert!(
        check
            .details
            .iter()
            .any(|d| d.starts_with("Blue Harbor Swim Club (31)")),
        "{:?}",
        check.details
    );
}

#[test]
fn an_unbalanced_qbo_trial_balance_fails_on_the_plug_and_on_3950() {
    let ledger = book();
    let mut opts = options(false, true);
    opts.qbo_tb[0].balance = usd(1_000_100);
    let report = run(&ledger, &replica(false), &opts);
    let failures = failed(&report);
    assert!(
        failures.iter().any(|f| f.contains("3950 plug")),
        "{failures:?}"
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("3950 opening balance equity")),
        "{failures:?}"
    );
}

#[test]
fn a_rerun_is_a_no_op_and_a_rerun_after_a_late_payment_voids_the_item_and_replaces_the_opening() {
    let ledger = book();
    let first = run(&ledger, &replica(false), &options(false, true));
    assert!(first.passed());

    let again = run(&ledger, &replica(false), &options(false, true));
    assert!(again.passed(), "{}", again.render_text(CO));
    assert_eq!(again.saved, 0);
    assert_eq!(again.skipped_unchanged, first.saved);
    assert!(again.opening.as_ref().unwrap().skipped_unchanged);

    // Joel's close brings a 30 December payment for invoice 1001.
    let late = run(&ledger, &replica(true), &options(true, true));
    assert!(late.passed(), "{}", late.render_text(CO));
    assert_eq!(late.voided_stale, 1);
    assert!(late.opening.as_ref().unwrap().replaced);
    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_RECEIVABLE, as_of()),
        usd(67_000)
    );
    assert_eq!(
        balance(&ledger, chart::UNDEPOSITED_FUNDS, as_of()),
        usd(50_000)
    );
    assert!(
        books::open_documents(&ledger, CO, Book::Receivable, as_of())
            .unwrap()
            .iter()
            .all(|d| d.document_id != "qbo:Invoice:I1")
    );
}

#[test]
fn a_rerun_carries_a_date_driven_term_over_a_book_that_had_it_due_on_receipt() {
    let ledger = book();
    assert!(run(&ledger, &replica(false), &options(false, true)).passed());

    // What the import wrote before D41: Lakeside due on receipt, no term id.
    let mut was = books::contact(&ledger, CO, "32").unwrap().unwrap();
    was.set_terms(books::Terms::Net { days: 0 });
    books::upsert_contact(&ledger, CO, &was, now()).unwrap();
    assert_eq!(
        books::contact(&ledger, CO, "32").unwrap().unwrap().terms(),
        books::Terms::Net { days: 0 }
    );

    let again = run(&ledger, &replica(false), &options(false, true));
    assert!(again.passed(), "{}", again.render_text(CO));
    let lakeside = books::contact(&ledger, CO, "32").unwrap().unwrap();
    assert_eq!(
        lakeside.terms(),
        books::Terms::DayOfMonth {
            day: 10,
            next_month_within: 5
        }
    );

    // A Q1 invoice entered in the own book takes its due date from it:
    // dated 6 January, the 10th is 4 days away, so it is due 10 February.
    let mut invoice = slim_invoice("inv-q1", ymd(2027, 1, 6));
    invoice.due_date = None;
    let (prepared, _) = books::prepare_document(&ledger, CO, invoice, Default::default()).unwrap();
    assert_eq!(prepared.due_date, Some(ymd(2027, 2, 10)));
}

fn slim_invoice(id: &str, on: NaiveDate) -> ledger::types::LedgerDocument {
    ledger::types::LedgerDocument {
        document_id: id.to_string(),
        kind: DocKind::Invoice,
        number: None,
        txn_date: on,
        due_date: None,
        contact: Some(books::contact_ref(ContactKind::Customer, "32")),
        header_class: None,
        lines: Vec::new(),
        tax: None,
        deposit_to: None,
        pay_from: None,
        applications: Vec::new(),
        unapplied: Money::ZERO,
        is_voided: false,
        source_ref: None,
        memo: None,
    }
}

#[test]
fn the_q1_replay_relieves_imported_invoices_and_refuses_to_post_them_again() {
    let ledger = book();
    let store = replica(false);
    assert!(run(&ledger, &store, &options(false, true)).passed());

    // D29: Q1 2027 replayed by `ledger import` from 1 January. Payment P2
    // (5 January) pays invoice 1003, which only exists here as an open item.
    let q1 = import_replica(
        &ledger,
        &store,
        &realm(),
        CO,
        &ImportOptions::with_d28_fallback(Some(ymd(2027, 1, 1)), None),
        now(),
    )
    .unwrap();
    assert_eq!(q1.posted, 1, "{}", q1.render_text());
    let end = ymd(2027, 1, 31);
    let ar = books::aging(&ledger, CO, Book::Receivable, end).unwrap();
    assert_eq!(ar.totals.total, usd(77_000));
    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_RECEIVABLE, end),
        usd(77_000)
    );

    // A full-history import would post the 2026 documents on top of the
    // opening entry; the carried ones are refused instead.
    let full = import_replica(
        &ledger,
        &store,
        &realm(),
        CO,
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(full
        .rejected
        .iter()
        .any(|r| r.document_id == "qbo:Invoice:I1" && r.reason.contains("opening entry")));
}

// ---------------------------------------------------------------------------
// The nightly against the cutover book (D29)
// ---------------------------------------------------------------------------

/// A Q1 2027 invoice, classed as the full posting gate requires of every
/// line dated from 1 January 2027.
fn seed_q1_invoice(store: &Store) {
    seed(
        store,
        EntityType::Invoice,
        "I5",
        json!({ "Id": "I5", "DocNumber": "1201", "TxnDate": "2027-01-12", "DueDate": "2027-02-11",
                "CustomerRef": { "value": "31" }, "TotalAmt": 300, "Balance": 300,
                "Line": [{ "Id": "1", "LineNum": 1, "Amount": 300, "DetailType": "SalesItemLineDetail",
                           "SalesItemLineDetail": { "ItemRef": { "value": "501" }, "Qty": 3,
                                                    "UnitPrice": 100, "ClassRef": { "value": "4" } } }] }),
    );
}

/// QBO's trial balance at 31 January 2027: the 31 December one, plus
/// payment P2 (400 from AR to undeposited funds) and invoice I5 (300 of AR
/// and sales).
fn qbo_tb_january() -> Vec<QboTbRow> {
    let mut rows = qbo_tb(false);
    for row in &mut rows {
        let delta = match row.qbo_account_id.as_str() {
            "AR1" => -40_000 + 30_000,
            "UDF1" => 40_000,
            "INC1" => -30_000,
            _ => 0,
        };
        row.balance = usd(row.balance.minor() + delta);
    }
    rows
}

#[test]
fn the_cutover_books_nightly_replays_only_2027_and_refuses_to_run_unbounded() {
    let ledger = book();
    let store = replica(false);
    seed_q1_invoice(&store);
    assert!(run(&ledger, &store, &options(false, true)).passed());
    assert_eq!(slim::opening_as_of(&ledger, CO).unwrap(), Some(as_of()));

    let out = tempfile::tempdir().unwrap();
    let end = ymd(2027, 1, 31);
    let nightly = |from: Option<NaiveDate>| {
        nightly::run_nightly(
            &ledger,
            &store,
            &realm(),
            CO,
            &qbo_tb_january(),
            from,
            end,
            out.path(),
            now(),
        )
    };

    // Unbounded, or a window that reaches back into 2026: refused before
    // anything is imported, naming the --from to use.
    for from in [None, Some(as_of()), Some(ymd(2026, 1, 1))] {
        match nightly(from) {
            Err(err @ NightlyError::Window { .. }) => {
                assert!(err.to_string().contains("--from 2027-01-01"), "{err}")
            }
            Err(other) => panic!("{from:?}: expected a window refusal, got {other}"),
            Ok(_) => panic!("{from:?}: an unbounded nightly ran against the slim book"),
        }
    }
    assert!(ledger
        .entries_for_document(CO, "qbo:Invoice:I4")
        .unwrap()
        .is_empty());

    // The cutover book's nightly: only Q1 2027 posts, nothing is rejected,
    // and the trial balance agrees with QBO's at 31 January.
    let outcome = nightly(Some(ymd(2027, 1, 1))).unwrap();
    assert!(
        outcome.import.rejected.is_empty(),
        "{}",
        outcome.import.render_text()
    );
    assert_eq!(
        outcome.import.translated,
        2,
        "{}",
        outcome.import.render_text()
    );
    assert_eq!(outcome.import.posted, 2, "{}", outcome.import.render_text());
    assert!(outcome.passed(), "{}", outcome.diff.render_text(CO, end));
    for id in ["qbo:Payment:P2", "qbo:Invoice:I5"] {
        assert!(
            !ledger.entries_for_document(CO, id).unwrap().is_empty(),
            "{id} did not post"
        );
    }
    // 2026 documents — paid ones the opening entry does not carry as open
    // items, and the carried ones — stay unposted.
    for id in [
        "qbo:Invoice:I4",
        "qbo:Payment:P3",
        "qbo:Bill:B2",
        "qbo:BillPayment:BP1",
        "qbo:Invoice:I1",
        "qbo:Bill:B1",
    ] {
        assert!(
            ledger.entries_for_document(CO, id).unwrap().is_empty(),
            "{id} posted"
        );
    }
    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_RECEIVABLE, end),
        usd(117_000 - 40_000 + 30_000)
    );

    // Tomorrow night: the same window is a no-op.
    let again = nightly(Some(ymd(2027, 1, 1))).unwrap();
    assert!(again.passed(), "{}", again.diff.render_text(CO, end));
    assert_eq!(again.import.posted, 0);
    assert_eq!(again.import.skipped_unchanged, 2);
}

// ---------------------------------------------------------------------------
// The binary
// ---------------------------------------------------------------------------

fn write_csvs(dir: &std::path::Path, tb: &[QboTbRow]) {
    let money = Money::display;
    let mut csv = String::from("qbo_account_id,name,balance\n");
    for row in tb {
        csv.push_str(&format!(
            "{},{},{}\n",
            row.qbo_account_id,
            row.name,
            money(row.balance)
        ));
    }
    std::fs::write(dir.join("tb.csv"), csv).unwrap();
    let aging = |rows: &[QboAgingRow]| {
        let mut out = String::from("entity_id,name,current,1-30,31-60,61-90,over_90,total\n");
        for r in rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                r.entity_id.as_deref().unwrap_or(""),
                r.name,
                money(r.current),
                money(r.days_1_30),
                money(r.days_31_60),
                money(r.days_61_90),
                money(r.over_90),
                money(r.total)
            ));
        }
        out
    };
    std::fs::write(dir.join("ar.csv"), aging(&qbo_ar(false))).unwrap();
    std::fs::write(dir.join("ap.csv"), aging(&qbo_ap())).unwrap();
}

fn ledger_bin(dir: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "import-slim".to_string(),
        "--company".into(),
        CO.into(),
        "--replica".into(),
        dir.join("replica.sqlite").display().to_string(),
        "--realm".into(),
        realm().as_str().to_string(),
        "--as-of".into(),
        "2026-12-31".into(),
        "--qbo-csv".into(),
        dir.join("tb.csv").display().to_string(),
        "--qbo-ar".into(),
        dir.join("ar.csv").display().to_string(),
        "--qbo-ap".into(),
        dir.join("ap.csv").display().to_string(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(&args)
        .output()
        .unwrap()
}

#[test]
fn the_binary_rehearses_on_a_scratch_book_and_writes_only_when_every_check_passed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path().join("replica.sqlite")).unwrap();
        build_replica(&store, false);
    }
    let db = dir.path().join("ledger.sqlite");
    let db_arg = db.display().to_string();

    // An unbalanced TB: the rehearsal fails and the real book is never made.
    let mut bad = qbo_tb(false);
    bad[0].balance = usd(1_000_100);
    write_csvs(dir.path(), &bad);
    let out = ledger_bin(dir.path(), &["--db", &db_arg]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("nothing written"), "{stdout}");
    assert!(!db.exists());

    // A dry run with a kept scratch book: --db untouched, scratch inspectable.
    write_csvs(dir.path(), &qbo_tb(false));
    let scratch = dir.path().join("scratch.sqlite");
    let scratch_arg = scratch.display().to_string();
    let out = ledger_bin(
        dir.path(),
        &["--dry-run", "--db", &db_arg, "--scratch", &scratch_arg],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("RESULT: every check passed"), "{stdout}");
    assert!(!db.exists());
    let rehearsed = Ledger::open(&scratch).unwrap();
    assert_eq!(
        balance(&rehearsed, chart::ACCOUNTS_RECEIVABLE, as_of()),
        usd(117_000)
    );

    // The real run.
    let out = ledger_bin(dir.path(), &["--db", &db_arg]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("REAL RUN"), "{stdout}");
    let real = Ledger::open(&db).unwrap();
    assert_eq!(
        balance(&real, chart::ACCOUNTS_RECEIVABLE, as_of()),
        usd(117_000)
    );
    drop(real);

    // `ledger nightly` against the imported book: refused without --from,
    // and the plist it renders carries the window.
    let nightly_args = |extra: &[&str]| {
        let mut args = vec![
            "nightly".to_string(),
            "--db".into(),
            db_arg.clone(),
            "--company".into(),
            CO.into(),
            "--replica".into(),
            dir.path().join("replica.sqlite").display().to_string(),
            "--realm".into(),
            realm().as_str().to_string(),
            "--qbo-csv".into(),
            dir.path().join("tb.csv").display().to_string(),
            "--out".into(),
            dir.path().join("nightly").display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        Command::new(env!("CARGO_BIN_EXE_ledger"))
            .args(&args)
            .output()
            .unwrap()
    };
    let out = nightly_args(&["--as-of", "2027-01-31"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("--from 2027-01-01"), "{stderr}");
    assert!(!dir.path().join("nightly").exists());
    let out = nightly_args(&["--plist", "--from", "2027-01-01"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("--from 2027-01-01 --as-of"), "{stdout}");
}

#[test]
fn without_qbo_aging_and_no_later_activity_the_balance_fields_are_the_reference() {
    let store = replica(false);
    // As if run on 31 December itself: the January payment does not exist
    // yet and invoice 1003 still shows its balance.
    let gone = MirroredEntity {
        entity_type: EntityType::Payment,
        qbo_id: "P2".into(),
        sync_token: "2".into(),
        last_updated_utc: now(),
        is_deleted: true,
        raw_json: json!({ "Id": "P2" }),
    };
    store.upsert_entity(&realm(), &gone, now()).unwrap();
    seed(
        &store,
        EntityType::Invoice,
        "I3",
        json!({ "Id": "I3", "DocNumber": "1003", "TxnDate": "2026-12-10", "DueDate": "2027-01-09",
                "CustomerRef": { "value": "32" }, "TotalAmt": 400, "Balance": 400,
                "Line": [item_line("501", 400.0)] }),
    );
    let ledger = book();
    let report = run(&ledger, &store, &options(false, false));
    assert!(report.passed(), "{}", report.render_text(CO));
    assert!(report
        .checks
        .iter()
        .any(|c| c.name.contains("Balance fields") && c.passed));
}

// ---------------------------------------------------------------------------
// Shapes the first live rehearsal (30 Sep 2026, docs/CUTOVER.md) turned up
// ---------------------------------------------------------------------------

/// On top of the base replica: a deposit crediting AR whose entity type QBO
/// spells `CUSTOMER`, a payment left unapplied and then refunded by check
/// (the check's line debits AR), a card payment to a vendor posted straight
/// to AP, and a bill payment with no bill behind it. QBO states no unapplied
/// amount on a bill payment, so the last two are vendor prepayments the
/// import has to work out.
fn replica_with_rehearsal_shapes() -> Store {
    let store = replica(false);
    seed(
        &store,
        EntityType::Deposit,
        "D1",
        json!({ "Id": "D1", "TxnDate": "2026-11-10", "TotalAmt": 60,
                "DepositToAccountRef": { "value": "BANK1" },
                "Line": [{ "Id": "1", "Amount": 60, "DetailType": "DepositLineDetail",
                           "DepositLineDetail": { "AccountRef": { "value": "AR1" },
                             "Entity": { "value": "32", "name": "Lakeside YMCA", "type": "CUSTOMER" } } }] }),
    );
    payment(&store, "P6", "31", "2026-12-01", 40.0, 40.0, &[]);
    seed(
        &store,
        EntityType::Purchase,
        "K1",
        json!({ "Id": "K1", "TxnDate": "2026-12-05", "PaymentType": "Check", "TotalAmt": 40,
                "AccountRef": { "value": "BANK1" }, "EntityRef": { "value": "31", "type": "Customer" },
                "Line": [{ "Id": "1", "Amount": 40, "DetailType": "AccountBasedExpenseLineDetail",
                           "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "AR1" },
                             "CustomerRef": { "value": "31" } } }] }),
    );
    seed(
        &store,
        EntityType::Purchase,
        "K2",
        json!({ "Id": "K2", "TxnDate": "2026-12-10", "PaymentType": "CreditCard", "Credit": false,
                "TotalAmt": 100, "AccountRef": { "value": "BANK1" },
                "EntityRef": { "value": "77", "type": "Vendor" },
                "Line": [{ "Id": "1", "Amount": 100, "DetailType": "AccountBasedExpenseLineDetail",
                           "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "AP1" } } }] }),
    );
    seed(
        &store,
        EntityType::BillPayment,
        "BP2",
        json!({ "Id": "BP2", "TxnDate": "2026-12-12", "VendorRef": { "value": "77" }, "TotalAmt": 30,
                "PayType": "Check", "CheckPayment": { "BankAccountRef": { "value": "BANK1" } },
                "Line": [] }),
    );
    store
}

#[test]
fn checks_deposits_and_bill_payments_that_move_ar_or_ap_are_open_items_too() {
    let store = replica_with_rehearsal_shapes();
    // The bank pays out 40 + 100 + 30 and takes in 60; AR loses the 60
    // deposit (the refund and the payment it refunds cancel); AP is relieved
    // by the 130 of prepayments.
    let mut tb = qbo_tb(false);
    for row in &mut tb {
        let delta = match row.qbo_account_id.as_str() {
            "BANK1" => -11_000,
            "UDF1" => 4_000,
            "AR1" => -6_000,
            "AP1" => 13_000,
            _ => 0,
        };
        row.balance = usd(row.balance.minor() + delta);
    }
    let mut ar = qbo_ar(false);
    ar[1].days_31_60 = usd(-6_000);
    ar[1].total = usd(ar[1].total.minor() - 6_000);
    let ap = vec![aging_row(
        "77",
        "Foam Supply Co",
        [60_000, -13_000, 0, 0, 0],
    )];
    let opts = SlimOptions {
        as_of: as_of(),
        qbo_tb: tb,
        qbo_ar: Some(ar),
        qbo_ap: Some(ap),
    };

    let ledger = book();
    let report = run(&ledger, &store, &opts);
    assert!(report.passed(), "{}", report.render_text(CO));
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.contains("names no customer")),
        "{:?}",
        report.warnings
    );

    let ar_items = books::open_documents(&ledger, CO, Book::Receivable, as_of()).unwrap();
    let open = |items: &[books::OpenDocument], id: &str| {
        items
            .iter()
            .find(|d| d.document_id == id)
            .map(|d| d.open.minor())
    };
    assert_eq!(open(&ar_items, "qbo:Deposit:D1"), Some(-6_000));
    assert_eq!(open(&ar_items, "qbo:Purchase:K1"), Some(4_000));
    let ap_items = books::open_documents(&ledger, CO, Book::Payable, as_of()).unwrap();
    assert_eq!(open(&ap_items, "qbo:Purchase:K2"), Some(-10_000));
    assert_eq!(open(&ap_items, "qbo:BillPayment:BP2"), Some(-3_000));
    assert_eq!(report.unapplied_reclass, usd(11_500));
}

#[test]
fn the_translation_works_out_what_a_bill_payment_left_unapplied() {
    let store = replica_with_rehearsal_shapes();
    let mut unapplied = std::collections::HashMap::new();
    ledger::import::run(
        &store,
        &realm(),
        &ledger::types::CompanyId(CO.into()),
        &ImportOptions::with_d28_fallback(None, None),
        &mut |t| {
            if t.document.kind == DocKind::BillPayment {
                unapplied.insert(t.document.document_id.clone(), t.document.unapplied.minor());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(unapplied["qbo:BillPayment:BP2"], 3_000);
    assert_eq!(unapplied["qbo:BillPayment:BP1"], 0);
}

/// D42: the open sales orders of the as-of date come across as never-posted
/// SalesOrder documents carrying only what was still to be invoiced then,
/// and convert to invoices in the ledger.
#[test]
fn open_sales_orders_are_carried_as_of_the_date_and_convert_to_invoices() {
    let ledger = book();
    let store = replica(false);
    seed_q1_invoice(&store);
    apply_sales_orders(
        &store,
        json!([
            // One line still open, one invoiced (by an invoice the replica
            // does not hold, so taken as before the date).
            { "id": "1490000501", "orderNumber": "1501", "issuedAt": "2026-12-10T08:00:00Z",
              "customerId": "31", "customerPONumber": "PO-9", "orderStatus": "OPEN",
              "paymentStatus": "PARTIALLY_INVOICED", "totalAmount": "225.00",
              "lines": [
                { "id": "1", "itemId": "501", "description": "Rescue tube", "quantity": 2,
                  "rate": "100", "amount": "200.00", "taxable": true },
                { "id": "2", "itemId": "502", "quantity": 1, "rate": "25", "amount": "25.00",
                  "quantityConsumedInInvoices": 1, "invoiceIds": ["I9"] }
              ] },
            // Invoiced in full, but by I5, dated 12 January 2027: open on the date.
            { "id": "1490000502", "orderNumber": "1502", "issuedAt": "2026-12-20",
              "customerId": "32", "orderStatus": "OPEN", "paymentStatus": "INVOICED",
              "totalAmount": "100.00", "invoiceIds": ["I5"],
              "lines": [{ "itemId": "502", "quantity": 4, "rate": "25", "amount": "100",
                          "quantityConsumedInInvoices": 4, "invoiceIds": ["I5"] }] },
            // Invoiced before the date: closed.
            { "id": "1490000503", "orderNumber": "1503", "issuedAt": "2026-11-01",
              "customerId": "31", "orderStatus": "OPEN", "paymentStatus": "PAID",
              "totalAmount": "50.00",
              "lines": [{ "itemId": "502", "quantity": 2, "rate": "25", "amount": "50",
                          "quantityConsumedInInvoices": 2, "invoiceIds": ["I9"] }] },
            // Entered after the date.
            { "id": "1490000504", "orderNumber": "1504", "issuedAt": "2027-01-05",
              "customerId": "31", "orderStatus": "OPEN", "paymentStatus": "NOT_INVOICED",
              "totalAmount": "25.00",
              "lines": [{ "itemId": "502", "quantity": 1, "rate": "25", "amount": "25" }] }
        ]),
    );

    let report = run(&ledger, &store, &options(false, true));
    assert!(report.passed(), "{}", report.render_text(CO));
    assert_eq!(report.open_counts.get("sales_order"), Some(&2));
    assert!(report.next_numbers.contains(&(DocKind::SalesOrder, 1505)));

    let open = ledger::sales_orders::sales_orders(&ledger, CO, None, true).unwrap();
    assert_eq!(open.len(), 2);
    let first = &open[0];
    assert_eq!(first.document_id, "qbo:SalesOrder:1490000501");
    assert_eq!(first.number.as_deref(), Some("1501"));
    assert_eq!(first.open, usd(20_000));
    assert_eq!(first.memo.as_deref(), Some("PO PO-9"));
    assert_eq!(first.source_ref.as_deref(), Some("1490000501"));
    assert_eq!(open[1].open, usd(10_000));
    let doc = books::latest_document(&ledger, CO, &first.document_id).unwrap();
    assert_eq!(doc.lines.len(), 1);
    assert_eq!(doc.lines[0].item_id.as_deref(), Some("501"));
    assert!(doc.lines[0].is_taxable);
    // Never posted: nothing on the trial balance moved for it.
    assert!(ledger
        .live_entries_for_document(CO, &first.document_id)
        .unwrap()
        .is_empty());

    // In 2027 the carried order becomes an invoice and closes.
    let invoice = ledger::sales_orders::invoice_from_sales_order(
        &ledger,
        CO,
        &first.document_id,
        ledger::sales_orders::ConvertRequest {
            document_id: "inv-from-so".into(),
            txn_date: ymd(2027, 1, 8),
            lines: None,
            number: None,
            memo: None,
        },
    )
    .unwrap();
    let (invoice, _) = books::prepare_document(
        &ledger,
        CO,
        invoice,
        ledger::types::PostingConfig::default(),
    )
    .unwrap();
    // Past I5's 1201, the highest QBO invoice number.
    assert_eq!(invoice.number.as_deref(), Some("1202"));
    assert_eq!(invoice.contact.as_ref().map(|c| c.id.as_str()), Some("31"));
    ledger
        .save_document(
            CO,
            &invoice,
            ledger::store::CommandMeta {
                actor_id: "test".into(),
                kind: "test".into(),
                hlc: now().to_rfc3339(),
            },
            now(),
        )
        .unwrap();
    let open = ledger::sales_orders::sales_orders(&ledger, CO, None, true).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].number.as_deref(), Some("1502"));

    // A re-run with the order gone from QBO's open list voids the carried copy.
    apply_sales_orders(&store, json!([]));
    let again = run(&ledger, &store, &options(false, true));
    assert!(again.passed(), "{}", again.render_text(CO));
    assert_eq!(again.voided_stale, 2);
}

#[test]
fn without_a_sales_order_feed_the_run_fails_rather_than_drop_open_orders() {
    let ledger = book();
    let store = Store::open_in_memory().unwrap();
    build_replica_without_feed(&store, false);
    let report = run(&ledger, &store, &options(false, true));
    assert!(!report.passed());
    let failed = failed(&report);
    assert!(
        failed
            .iter()
            .any(|name| name.contains("sales orders mirrored")),
        "{failed:?}"
    );
}

/// A vendor credit, partly used inside a check bill payment before the
/// date, and a refund receipt paid out of undeposited funds. The credit's
/// remainder is an AP open item carried negative; the refund is money out
/// with no AR or AP leg, so it is in the opening entry's cash lines and
/// nowhere in either book.
#[test]
fn a_vendor_credit_is_a_negative_ap_open_item_and_a_refund_receipt_is_no_open_item() {
    let store = replica(false);
    // Bill B1 (600) is now 500 open: 100 of it paid by BP3 together with 20
    // of the 45 credit, so the check was for 80.
    seed(
        &store,
        EntityType::Bill,
        "B1",
        json!({ "Id": "B1", "DocNumber": "FS-778", "TxnDate": "2026-12-01", "DueDate": "2026-12-31",
                "VendorRef": { "value": "77" }, "TotalAmt": 600, "Balance": 500,
                "Line": [expense_line(600.0)] }),
    );
    seed(
        &store,
        EntityType::VendorCredit,
        "VC1",
        json!({ "Id": "VC1", "DocNumber": "CR-12", "TxnDate": "2026-12-18",
                "VendorRef": { "value": "77" }, "TotalAmt": 45, "Balance": 25,
                "Line": [expense_line(45.0)] }),
    );
    seed(
        &store,
        EntityType::BillPayment,
        "BP3",
        json!({ "Id": "BP3", "TxnDate": "2026-12-20", "VendorRef": { "value": "77" }, "TotalAmt": 80,
                "PayType": "Check", "CheckPayment": { "BankAccountRef": { "value": "BANK1" } },
                "Line": [
                    { "Amount": 100, "LinkedTxn": [{ "TxnId": "B1", "TxnType": "Bill" }] },
                    { "Amount": 20, "LinkedTxn": [{ "TxnId": "VC1", "TxnType": "VendorCredit" }] }
                ] }),
    );
    seed(
        &store,
        EntityType::RefundReceipt,
        "RR1",
        json!({ "Id": "RR1", "DocNumber": "R-3", "TxnDate": "2026-12-22",
                "CustomerRef": { "value": "32" }, "TotalAmt": 25,
                "DepositToAccountRef": { "value": "UDF1" },
                "Line": [item_line("502", 25.0)] }),
    );

    // QBO's side: the credit takes 45 off AP and off supplies, the payment
    // takes 80 out of the bank and 80 more off AP; the refund takes 25 out
    // of undeposited funds and back off sales.
    let mut tb = qbo_tb(false);
    for row in &mut tb {
        let delta = match row.qbo_account_id.as_str() {
            "BANK1" => -8_000,
            "UDF1" => -2_500,
            "AP1" => 12_500,
            "EXP1" => -4_500,
            "INC1" => 2_500,
            _ => 0,
        };
        row.balance = usd(row.balance.minor() + delta);
    }
    let opts = SlimOptions {
        as_of: as_of(),
        qbo_tb: tb,
        qbo_ar: Some(qbo_ar(false)),
        qbo_ap: Some(vec![aging_row(
            "77",
            "Foam Supply Co",
            [50_000, -2_500, 0, 0, 0],
        )]),
    };

    let ledger = book();
    let report = run(&ledger, &store, &opts);
    assert!(report.passed(), "{}", report.render_text(CO));

    let ap_items = books::open_documents(&ledger, CO, Book::Payable, as_of()).unwrap();
    let open = |items: &[books::OpenDocument], id: &str| {
        items
            .iter()
            .find(|d| d.document_id == id)
            .map(|d| d.open.minor())
    };
    assert_eq!(open(&ap_items, "qbo:VendorCredit:VC1"), Some(-2_500));
    assert_eq!(open(&ap_items, "qbo:Bill:B1"), Some(50_000));
    assert_eq!(open(&ap_items, "qbo:BillPayment:BP3"), None, "fully applied");
    // AP is a credit balance: 500 owed less the 25 of credit left.
    assert_eq!(balance(&ledger, chart::ACCOUNTS_PAYABLE, as_of()), usd(-47_500));

    let ar_items = books::open_documents(&ledger, CO, Book::Receivable, as_of()).unwrap();
    assert!(
        ar_items.iter().all(|d| !d.document_id.contains("RefundReceipt")),
        "{ar_items:?}"
    );
    assert!(ap_items.iter().all(|d| !d.document_id.contains("RefundReceipt")));
    // The cash it paid out is in the opening entry, as QBO had it.
    assert_eq!(balance(&ledger, chart::UNDEPOSITED_FUNDS, as_of()), usd(-2_500));
}
