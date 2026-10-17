//! D37: the three QBO entity types the replica did not mirror until the
//! practice parallel run found them — Transfer, CreditCardPayment and
//! InventoryAdjustment — each taken from a replica fixture through
//! `import_replica` to the trial balance. Fixture helpers follow
//! `tests/pipeline.rs`.

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::import::ImportOptions;
use ledger::pipeline::{import_replica, PipelineReport};
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
        sync_token: "0".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    };
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();
}

/// Checking, one card, an equity account, finished inventory, QBO's own
/// "Inventory Shrinkage" COGS account, one class and one inventory item that
/// carries that class.
fn replica_with_masters() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();
    seed(
        &store,
        EntityType::Class,
        "4",
        json!({ "Id": "4", "Name": "Foam Products", "Active": true }),
    );
    for (id, name, account_type, subtype) in [
        ("BANK1", "Checking", "Bank", "Checking"),
        ("CC1", "Amex", "Credit Card", "CreditCard"),
        (
            "EQ1",
            "Shareholder Distributions",
            "Equity",
            "PartnerDistributions",
        ),
        (
            "INV1",
            "Inventory Asset",
            "Other Current Asset",
            "Inventory",
        ),
        (
            "SHR1",
            "Inventory Shrinkage",
            "Cost of Goods Sold",
            "SuppliesMaterialsCogs",
        ),
    ] {
        seed(
            &store,
            EntityType::Account,
            id,
            json!({ "Id": id, "Name": name, "AccountType": account_type,
                    "AccountSubType": subtype, "Active": true }),
        );
    }
    seed(
        &store,
        EntityType::Item,
        "501",
        json!({ "Id": "501", "Name": "Rescue Tube 50in", "Sku": "RT-50",
                "Type": "Inventory", "PurchaseCost": 25.00,
                "AssetAccountRef": { "value": "INV1" }, "ClassRef": { "value": "4" },
                "Active": true }),
    );
    store
}

fn run(replica: &Store) -> (PipelineReport, TrialBalance) {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(
            "aquamentor",
            "Aquamentor LLC",
            Some(realm().as_str()),
            now(),
        )
        .unwrap();
    let report = import_replica(
        &ledger,
        replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();
    let as_of = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let tb = report::trial_balance(&ledger, "aquamentor", as_of).unwrap();
    assert_eq!(
        tb.total_debits, tb.total_credits,
        "the trial balance balances"
    );
    (report, tb)
}

fn balance(tb: &TrialBalance, number: &str) -> Money {
    tb.rows
        .iter()
        .find(|row| row.number == number)
        .map(|row| row.balance)
        .unwrap_or(Money::ZERO)
}

fn assert_posted_cleanly(report: &PipelineReport) {
    assert_eq!(report.translated, 1);
    assert_eq!(report.posted, 1);
    assert!(
        report.rejected.is_empty(),
        "rejected: {:?}",
        report.rejected
    );
}

#[test]
fn a_transfer_debits_the_to_account_and_credits_the_from_account() {
    let replica = replica_with_masters();
    // The practice run's one transfer to shareholder distributions.
    seed(
        &replica,
        EntityType::Transfer,
        "T1",
        json!({ "Id": "T1", "TxnDate": "2026-10-16", "Amount": 1000.00,
                "FromAccountRef": { "value": "BANK1", "name": "Checking" },
                "ToAccountRef": { "value": "EQ1", "name": "Shareholder Distributions" },
                "PrivateNote": "Q3 distribution" }),
    );

    let (report, tb) = run(&replica);
    assert_posted_cleanly(&report);
    assert_eq!(balance(&tb, "1100"), Money::from_minor(-100_000));
    assert_eq!(balance(&tb, "3000"), Money::from_minor(100_000));
}

#[test]
fn a_credit_card_payment_debits_the_card_and_credits_the_bank() {
    let replica = replica_with_masters();
    // Intuit's read and query responses wrap this entity as
    // `CreditCardPaymentTxn`; the stored payload is the inner object.
    seed(
        &replica,
        EntityType::CreditCardPayment,
        "29",
        json!({ "Id": "29", "TxnDate": "2026-10-16", "Amount": 2500.00,
                "BankAccountRef": { "value": "BANK1", "name": "Checking" },
                "CreditCardAccountRef": { "value": "CC1", "name": "Amex" },
                "VendorRef": { "value": "77", "name": "American Express" } }),
    );

    let (report, tb) = run(&replica);
    assert_posted_cleanly(&report);
    assert_eq!(balance(&tb, "1100"), Money::from_minor(-250_000));
    assert_eq!(balance(&tb, "2100"), Money::from_minor(250_000));
}

#[test]
fn an_inventory_adjustment_values_qty_diff_at_purchase_cost_against_5150() {
    let replica = replica_with_masters();
    // Four tubes short at 25.00: the practice run's 100.00 shrinkage.
    seed(
        &replica,
        EntityType::InventoryAdjustment,
        "1455000081",
        json!({ "Id": "1455000081", "DocNumber": "27", "TxnDate": "2026-10-16",
                "AdjustAccountRef": { "value": "SHR1", "name": "Inventory Shrinkage" },
                "Line": [ { "Id": "1", "DetailType": "ItemAdjustmentLineDetail",
                            "ItemAdjustmentLineDetail": {
                                "QtyDiff": -4, "ItemRef": { "value": "501" } } } ] }),
    );

    let (report, tb) = run(&replica);
    assert_posted_cleanly(&report);
    assert!(
        !report.warnings.iter().any(|w| w.contains("adjust account")),
        "QBO's shrinkage account maps to 5150: {:?}",
        report.warnings
    );
    assert_eq!(balance(&tb, "1310"), Money::from_minor(-10_000));
    assert_eq!(balance(&tb, "5150"), Money::from_minor(10_000));
}

#[test]
fn an_inventory_adjustment_on_an_item_with_no_cost_is_refused_not_posted_at_zero() {
    let replica = replica_with_masters();
    seed(
        &replica,
        EntityType::Item,
        "502",
        json!({ "Id": "502", "Name": "Kickboard", "Type": "Inventory",
                "AssetAccountRef": { "value": "INV1" }, "ClassRef": { "value": "4" },
                "Active": true }),
    );
    seed(
        &replica,
        EntityType::InventoryAdjustment,
        "1455000082",
        json!({ "Id": "1455000082", "TxnDate": "2026-10-16",
                "AdjustAccountRef": { "value": "SHR1" },
                "Line": [ { "Id": "1", "DetailType": "ItemAdjustmentLineDetail",
                            "ItemAdjustmentLineDetail": {
                                "QtyDiff": -2, "ItemRef": { "value": "502" } } } ] }),
    );

    let (report, tb) = run(&replica);
    assert_eq!(report.posted, 0);
    assert_eq!(report.rejected.len(), 1, "{:?}", report.rejected);
    assert_eq!(balance(&tb, "1310"), Money::ZERO);
}

#[test]
fn a_transfer_and_a_journal_entry_with_the_same_qbo_id_both_post() {
    // QBO numbers each entity type on its own, and a transfer translates to a
    // journal: its ledger id must still name the Transfer, or the journal
    // entry would replace it as a new version of the same document.
    let replica = replica_with_masters();
    seed(
        &replica,
        EntityType::Transfer,
        "7",
        json!({ "Id": "7", "TxnDate": "2026-10-16", "Amount": 1000.00,
                "FromAccountRef": { "value": "BANK1" }, "ToAccountRef": { "value": "EQ1" } }),
    );
    seed(
        &replica,
        EntityType::JournalEntry,
        "7",
        json!({ "Id": "7", "TxnDate": "2026-10-16", "TotalAmt": 250.00, "Line": [
            { "Amount": 250.00, "DetailType": "JournalEntryLineDetail",
              "JournalEntryLineDetail": { "PostingType": "Debit", "AccountRef": { "value": "EQ1" } } },
            { "Amount": 250.00, "DetailType": "JournalEntryLineDetail",
              "JournalEntryLineDetail": { "PostingType": "Credit", "AccountRef": { "value": "BANK1" } } }
        ] }),
    );

    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company("aquamentor", "Aquamentor LLC", Some(realm().as_str()), now())
        .unwrap();
    let report = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();
    assert!(report.rejected.is_empty(), "rejected: {:?}", report.rejected);
    assert_eq!(report.posted, 2);
    let as_of = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
    let tb = report::trial_balance(&ledger, "aquamentor", as_of).unwrap();
    assert_eq!(balance(&tb, "1100"), Money::from_minor(-125_000));
    assert_eq!(balance(&tb, "3000"), Money::from_minor(125_000));
}
