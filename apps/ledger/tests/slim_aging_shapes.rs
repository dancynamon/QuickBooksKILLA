//! QBO document shapes the slim import's aging missed on the practice run
//! (docs/PRACTICE-RUN.md, D40): each one is an AR or AP open item QBO ages,
//! and the 6/30/26 rehearsal against the live replica failed until all four
//! were read. The book as of 2026-06-30:
//!
//! | contact | what | QBO aging |
//! |---|---|---:|
//! | vendor 77 | card purchase coded to AP, 2026-06-01 | -100.00 (1-30) |
//! | vendor 78 | bill payment with no lines, 2023-12-31 | -50.00 (over 90) |
//! | vendor 79 | AP journal line offset by an AP-coded purchase in a $0 bill payment | 0 |
//! | customer 31 | deposit line to AR, entity type "CUSTOMER", 2026-02-27 | -200.00 (over 90) |

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::report::QboTbRow;
use ledger::slim::{self, QboAgingRow, SlimOptions};
use ledger::store::Ledger;
use ledger_core::Money;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

const CO: &str = "aquamentor";

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
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

fn account(store: &Store, id: &str, name: &str, kind: &str) {
    seed(
        store,
        EntityType::Account,
        id,
        json!({ "Id": id, "Name": name, "AccountType": kind, "Active": true }),
    );
}

fn ap_purchase(id: &str, date: &str, vendor: &str, amount: f64) -> Value {
    json!({
        "Id": id, "TxnDate": date, "PaymentType": "CreditCard", "Credit": false,
        "AccountRef": { "value": "CARD1" },
        "EntityRef": { "value": vendor, "type": "Vendor" },
        "TotalAmt": amount,
        "Line": [{ "Id": "1", "Amount": amount, "DetailType": "AccountBasedExpenseLineDetail",
                   "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "AP1" } } }]
    })
}

fn replica() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    for (id, name) in [("77", "Card Vendor"), ("78", "Old Vendor"), ("79", "Offset Vendor")] {
        seed(&store, EntityType::Vendor, id, json!({ "Id": id, "DisplayName": name, "Active": true }));
    }
    seed(
        &store,
        EntityType::Customer,
        "31",
        json!({ "Id": "31", "DisplayName": "Swim Club", "Active": true }),
    );
    account(&store, "BANK1", "Checking", "Bank");
    account(&store, "CARD1", "Business card", "Credit Card");
    account(&store, "AR1", "Accounts Receivable", "Accounts Receivable");
    account(&store, "AP1", "Accounts Payable", "Accounts Payable");
    account(&store, "EXP1", "Purchasing", "Expense");

    seed(&store, EntityType::Purchase, "501", ap_purchase("501", "2026-06-01", "77", 100.00));
    seed(
        &store,
        EntityType::BillPayment,
        "502",
        json!({ "Id": "502", "TxnDate": "2023-12-31", "TotalAmt": 50.00, "PayType": "Check",
                "VendorRef": { "value": "78" },
                "CheckPayment": { "BankAccountRef": { "value": "BANK1" } }, "Line": [] }),
    );
    seed(
        &store,
        EntityType::JournalEntry,
        "503",
        json!({ "Id": "503", "DocNumber": "CC-2022-5", "TxnDate": "2022-12-31", "Line": [
            { "Id": "0", "Amount": 7.34, "DetailType": "JournalEntryLineDetail",
              "JournalEntryLineDetail": { "PostingType": "Debit", "AccountRef": { "value": "EXP1" },
                  "Entity": { "Type": "Vendor", "EntityRef": { "value": "79" } } } },
            { "Id": "1", "Amount": 7.34, "DetailType": "JournalEntryLineDetail",
              "JournalEntryLineDetail": { "PostingType": "Credit", "AccountRef": { "value": "AP1" },
                  "Entity": { "Type": "Vendor", "EntityRef": { "value": "79" } } } }
        ] }),
    );
    seed(&store, EntityType::Purchase, "504", ap_purchase("504", "2019-11-22", "79", 7.34));
    seed(
        &store,
        EntityType::BillPayment,
        "505",
        json!({ "Id": "505", "TxnDate": "2023-12-31", "TotalAmt": 0, "PayType": "Check",
                "VendorRef": { "value": "79" },
                "CheckPayment": { "BankAccountRef": { "value": "BANK1" } }, "Line": [
            { "Amount": 7.34, "LinkedTxn": [{ "TxnId": "503", "TxnType": "JournalEntry" }] },
            { "Amount": 7.34, "LinkedTxn": [{ "TxnId": "504", "TxnType": "Purchase" }] }
        ] }),
    );
    seed(
        &store,
        EntityType::Deposit,
        "506",
        json!({ "Id": "506", "TxnDate": "2026-02-27", "TotalAmt": 200.00,
                "DepositToAccountRef": { "value": "BANK1" }, "Line": [
            { "Amount": 200.00, "DepositLineDetail": {
                "AccountRef": { "value": "AR1" },
                "Entity": { "name": "Swim Club", "type": "CUSTOMER", "value": "31" } } }
        ] }),
    );
    store
}

fn tb() -> Vec<QboTbRow> {
    [
        ("BANK1", "Checking", 15_000),
        ("CARD1", "Business card", -10_734),
        ("AP1", "Accounts Payable", 15_000),
        ("AR1", "Accounts Receivable", -20_000),
        ("EXP1", "Purchasing", 734),
    ]
    .into_iter()
    .map(|(id, name, minor)| QboTbRow {
        qbo_account_id: id.into(),
        name: name.into(),
        balance: Money::from_minor(minor),
    })
    .collect()
}

fn row(id: &str, buckets: [i64; 5]) -> QboAgingRow {
    QboAgingRow {
        entity_id: Some(id.into()),
        name: id.into(),
        current: Money::from_minor(buckets[0]),
        days_1_30: Money::from_minor(buckets[1]),
        days_31_60: Money::from_minor(buckets[2]),
        days_61_90: Money::from_minor(buckets[3]),
        over_90: Money::from_minor(buckets[4]),
        total: Money::from_minor(buckets.iter().sum()),
    }
}

#[test]
fn ap_coded_purchases_lineless_bill_payments_and_capitalised_deposit_entities_are_aged() {
    let store = replica();
    let ledger = Ledger::open_in_memory().unwrap();
    assert!(slim::ensure_company(&ledger, CO, &realm(), now()).unwrap());
    let options = SlimOptions {
        as_of: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
        qbo_tb: tb(),
        qbo_ar: Some(vec![row("31", [0, 0, 0, 0, -20_000])]),
        qbo_ap: Some(vec![
            row("77", [0, -10_000, 0, 0, 0]),
            row("78", [0, 0, 0, 0, -5_000]),
        ]),
    };
    let report = slim::import_slim(&ledger, &store, &realm(), CO, &options, now()).unwrap();
    let failed: Vec<_> = report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {:?}", c.name, c.details))
        .collect();
    assert!(failed.is_empty(), "{failed:#?}");
}
