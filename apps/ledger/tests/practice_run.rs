//! Regressions found by the practice parallel run (docs/PRACTICE-RUN.md):
//! each test is one document shape the live replica carried and the posting
//! path got wrong. Fixture helpers follow `tests/pipeline.rs`.

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::import::ImportOptions;
use ledger::pipeline::import_replica;
use ledger::report::{self, TrialBalance};
use ledger::store::Ledger;
use ledger_core::Money;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

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

/// The balance of the ledger account a QBO account id was mapped onto.
fn balance_of(tb: &TrialBalance, qbo_account_id: &str) -> Money {
    tb.rows
        .iter()
        .find(|row| row.source_ref.as_deref() == Some(qbo_account_id))
        .map(|row| row.balance)
        .unwrap_or_else(|| panic!("no ledger account carries QBO account {qbo_account_id}"))
}

fn card_purchase(id: &str, amount: f64, credit: bool) -> Value {
    json!({
        "Id": id,
        "TxnDate": "2026-07-14",
        "PaymentType": "CreditCard",
        "Credit": credit,
        "AccountRef": { "value": "CARD1" },
        "EntityRef": { "value": "77", "type": "Vendor" },
        "TotalAmt": amount,
        "Line": [{
            "Id": "1",
            "Amount": amount,
            "DetailType": "AccountBasedExpenseLineDetail",
            "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } }
        }]
    })
}

#[test]
fn a_card_credit_debits_the_card_and_credits_the_expense() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Vendor,
        "77",
        json!({ "Id": "77", "DisplayName": "Supply Co", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "CARD1",
        json!({ "Id": "CARD1", "Name": "Business card", "AccountType": "Credit Card", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "EXP1",
        json!({ "Id": "EXP1", "Name": "Supplies", "AccountType": "Expense", "Active": true }),
    );
    seed(&store, EntityType::Purchase, "501", card_purchase("501", 100.00, false));
    seed(&store, EntityType::Purchase, "502", card_purchase("502", 30.00, true));

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(report.posted, 2);

    let tb = report::trial_balance(&ledger, "aquamentor", NaiveDate::from_ymd_opt(2026, 7, 31).unwrap())
        .unwrap();
    // 100 charged, 30 refunded: the card owes 70, the expense is 70.
    assert_eq!(balance_of(&tb, "CARD1"), Money::from_minor(-7_000));
    assert_eq!(balance_of(&tb, "EXP1"), Money::from_minor(7_000));
}

#[test]
fn a_deposit_line_linked_to_a_payment_sweeps_undeposited_funds() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Customer,
        "31",
        json!({ "Id": "31", "DisplayName": "Swim Club", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "BANK1",
        json!({ "Id": "BANK1", "Name": "Checking", "AccountType": "Bank", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "UDF1",
        json!({ "Id": "UDF1", "Name": "Undeposited Funds", "AccountType": "Other Current Asset",
                "AccountSubType": "UndepositedFunds", "Active": true }),
    );
    seed(
        &store,
        EntityType::Payment,
        "601",
        json!({
            "Id": "601", "TxnDate": "2026-07-20", "TotalAmt": 2500.00, "UnappliedAmt": 2500.00,
            "CustomerRef": { "value": "31" },
            "DepositToAccountRef": { "value": "UDF1" },
            "Line": []
        }),
    );
    // QBO's own shape: the line names the payment it sweeps, and no account.
    seed(
        &store,
        EntityType::Deposit,
        "602",
        json!({
            "Id": "602", "TxnDate": "2026-07-21", "TotalAmt": 2500.00,
            "DepositToAccountRef": { "value": "BANK1" },
            "Line": [{
                "Id": "1", "Amount": 2500.00,
                "DepositLineDetail": { "PaymentMethodRef": { "value": "52" } },
                "LinkedTxn": [{ "TxnId": "601", "TxnLineId": "0", "TxnType": "Payment" }]
            }]
        }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let tb = report::trial_balance(&ledger, "aquamentor", NaiveDate::from_ymd_opt(2026, 7, 31).unwrap())
        .unwrap();
    assert_eq!(balance_of(&tb, "UDF1"), Money::ZERO);
    assert_eq!(balance_of(&tb, "BANK1"), Money::from_minor(250_000));
}

#[test]
fn a_bill_line_for_an_item_named_shipping_is_a_cost_not_a_shipping_charge() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Vendor,
        "77",
        json!({ "Id": "77", "DisplayName": "Box Co", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "AP1",
        json!({ "Id": "AP1", "Name": "Accounts Payable", "AccountType": "Accounts Payable", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "EXP2",
        json!({ "Id": "EXP2", "Name": "Shipping supplies", "AccountType": "Expense", "Active": true }),
    );
    seed(
        &store,
        EntityType::Item,
        "3471",
        json!({ "Id": "3471", "Name": "SHIPPING SUPPLIES", "Type": "NonInventory", "Active": true,
                "ExpenseAccountRef": { "value": "EXP2" } }),
    );
    seed(
        &store,
        EntityType::Bill,
        "701",
        json!({
            "Id": "701", "TxnDate": "2026-08-10", "TotalAmt": 133.66,
            "VendorRef": { "value": "77" },
            "APAccountRef": { "value": "AP1" },
            "Line": [{
                "Id": "1", "Amount": 133.66,
                "DetailType": "ItemBasedExpenseLineDetail",
                "ItemBasedExpenseLineDetail": { "ItemRef": { "value": "3471" }, "Qty": 1, "UnitPrice": 133.66 }
            }]
        }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let tb = report::trial_balance(&ledger, "aquamentor", NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
        .unwrap();
    assert_eq!(balance_of(&tb, "AP1"), Money::from_minor(-13_366));
    assert_eq!(balance_of(&tb, "EXP2"), Money::from_minor(13_366));
}

#[test]
fn a_bill_payment_with_nothing_applied_posts_its_total_as_an_ap_debit() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Vendor,
        "78",
        json!({ "Id": "78", "DisplayName": "Old Vendor", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "BANK1",
        json!({ "Id": "BANK1", "Name": "Checking", "AccountType": "Bank", "Active": true }),
    );
    seed(
        &store,
        EntityType::Account,
        "AP1",
        json!({ "Id": "AP1", "Name": "Accounts Payable", "AccountType": "Accounts Payable", "Active": true }),
    );
    // QBO's own shape: no UnappliedAmt field on a BillPayment, and no lines.
    seed(
        &store,
        EntityType::BillPayment,
        "801",
        json!({ "Id": "801", "TxnDate": "2026-07-20", "TotalAmt": 154.40, "PayType": "Check",
                "VendorRef": { "value": "78" },
                "CheckPayment": { "BankAccountRef": { "value": "BANK1" } }, "Line": [] }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let tb = report::trial_balance(&ledger, "aquamentor", NaiveDate::from_ymd_opt(2026, 7, 31).unwrap())
        .unwrap();
    assert_eq!(balance_of(&tb, "BANK1"), Money::from_minor(-15_440));
    assert_eq!(balance_of(&tb, "AP1"), Money::from_minor(15_440));
}

#[test]
fn an_inventory_adjustment_posts_to_the_items_own_inventory_account_when_it_is_not_1310() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    // Two QBO inventory asset accounts: the first takes 1310, the second 1311.
    for (id, name) in [("INV1", "998-Inventory Asset"), ("INV2", "Inventory Asset")] {
        seed(
            &store,
            EntityType::Account,
            id,
            json!({ "Id": id, "Name": name, "AccountType": "Other Current Asset",
                    "AccountSubType": "Inventory", "Active": true }),
        );
    }
    seed(
        &store,
        EntityType::Account,
        "SHR1",
        json!({ "Id": "SHR1", "Name": "Inventory Shrinkage", "AccountType": "Cost of Goods Sold", "Active": true }),
    );
    seed(
        &store,
        EntityType::Item,
        "5066",
        json!({ "Id": "5066", "Name": "Kickboard", "Type": "Inventory", "Active": false,
                "PurchaseCost": 50, "AssetAccountRef": { "value": "INV2" } }),
    );
    seed(
        &store,
        EntityType::InventoryAdjustment,
        "901",
        json!({ "Id": "901", "DocNumber": "2715", "TxnDate": "2026-10-14",
                "AdjustAccountRef": { "value": "SHR1" },
                "Line": [{ "Id": "1", "DetailType": "ItemAdjustmentLineDetail",
                           "ItemAdjustmentLineDetail": { "ItemRef": { "value": "5066" }, "QtyDiff": 2 } }] }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let tb = report::trial_balance(&ledger, "aquamentor", NaiveDate::from_ymd_opt(2026, 10, 16).unwrap())
        .unwrap();
    assert_eq!(balance_of(&tb, "INV2"), Money::from_minor(10_000));
    assert_eq!(balance_of(&tb, "SHR1"), Money::from_minor(-10_000));
}

#[test]
fn a_bill_payment_dated_before_its_bill_ages_as_a_vendor_credit_until_the_bill_exists() {
    use ledger::books::{self, Book};

    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Vendor,
        "77",
        json!({ "Id": "77", "DisplayName": "Pool Parts", "Active": true }),
    );
    for (id, name, kind) in [
        ("BANK1", "Checking", "Bank"),
        ("AP1", "Accounts Payable", "Accounts Payable"),
        ("EXP1", "Purchasing", "Expense"),
    ] {
        seed(
            &store,
            EntityType::Account,
            id,
            json!({ "Id": id, "Name": name, "AccountType": kind, "Active": true }),
        );
    }
    seed(
        &store,
        EntityType::BillPayment,
        "1001",
        json!({ "Id": "1001", "TxnDate": "2026-07-16", "TotalAmt": 631.51, "PayType": "Check",
                "VendorRef": { "value": "77" },
                "CheckPayment": { "BankAccountRef": { "value": "BANK1" } },
                "Line": [{ "Amount": 631.51, "LinkedTxn": [{ "TxnId": "1002", "TxnType": "Bill" }] }] }),
    );
    seed(
        &store,
        EntityType::Bill,
        "1002",
        json!({ "Id": "1002", "TxnDate": "2026-09-02", "TotalAmt": 631.51,
                "VendorRef": { "value": "77" }, "APAccountRef": { "value": "AP1" },
                "Line": [{ "Id": "1", "Amount": 631.51, "DetailType": "AccountBasedExpenseLineDetail",
                           "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } } }] }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &store,
        &realm(),
        "aquamentor",
        &ImportOptions::with_d28_fallback(None, None),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);

    let july = books::aging(&ledger, "aquamentor", Book::Payable, NaiveDate::from_ymd_opt(2026, 7, 31).unwrap())
        .unwrap();
    assert_eq!(july.totals.total, Money::from_minor(-63_151), "{july:?}");
    let september =
        books::aging(&ledger, "aquamentor", Book::Payable, NaiveDate::from_ymd_opt(2026, 9, 30).unwrap())
            .unwrap();
    assert_eq!(september.totals.total, Money::ZERO, "{september:?}");
}
