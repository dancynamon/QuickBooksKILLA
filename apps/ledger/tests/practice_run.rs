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
