//! The pipeline end to end: replica in, posted entries out. `LEDGER-DESIGN.md`
//! §6. Fixture-building helpers (`mirrored`/`seed`) follow the pattern in
//! `tests/import.rs`; the documents here are a fresh, self-contained set
//! chosen so every pipeline assertion (AR, tax, AP, the void, re-run
//! idempotence, re-run replacement) has one clean number to check.

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
    Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
}

fn later_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 12, 13, 0, 0).unwrap()
}

fn mirrored(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
    MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "1".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    }
}

fn seed(store: &Store, entity: MirroredEntity) {
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();
}

/// A replica with: an unmapped account (so `accounts_needing_mapping` names
/// it), two items, a customer, an invoice with tax (two lines — one item
/// carries its own class, the other falls back to its item's default), a
/// payment applying part of it, a bill with an `Account` line, and a voided
/// invoice.
fn seeded_replica() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();

    seed(
        &store,
        mirrored(
            EntityType::Customer,
            "31",
            json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true, "Taxable": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Vendor,
            "77",
            json!({ "Id": "77", "DisplayName": "Foam Supply Co", "Active": true }),
        ),
    );

    seed(
        &store,
        mirrored(
            EntityType::Class,
            "4",
            json!({ "Id": "4", "Name": "Foam Products", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Class,
            "6",
            json!({ "Id": "6", "Name": "Lifeguard Chairs", "Active": true }),
        ),
    );

    seed(
        &store,
        mirrored(
            EntityType::Account,
            "AR1",
            json!({ "Id": "AR1", "Name": "Accounts Receivable", "AccountType": "Accounts Receivable", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "AP1",
            json!({ "Id": "AP1", "Name": "Accounts Payable", "AccountType": "Accounts Payable", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "BANK1",
            json!({ "Id": "BANK1", "Name": "Checking", "AccountType": "Bank", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "UDF1",
            json!({ "Id": "UDF1", "Name": "Undeposited Funds", "AccountType": "Other Current Asset",
                    "AccountSubType": "UndepositedFunds", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "INC1",
            json!({ "Id": "INC1", "Name": "Sales", "AccountType": "Income",
                    "AccountSubType": "SalesOfProductIncome", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "COGS1",
            json!({ "Id": "COGS1", "Name": "Cost of Goods Sold", "AccountType": "Cost of Goods Sold", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "INV1",
            json!({ "Id": "INV1", "Name": "Inventory Finished", "AccountType": "Other Current Asset",
                    "AccountSubType": "Inventory", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "EXP1",
            json!({ "Id": "EXP1", "Name": "Shop Supplies", "AccountType": "Expense",
                    "AccountSubType": "SuppliesMaterials", "Active": true }),
        ),
    );
    // Unmapped: no `AccountType`/`AccountSubType` this crate's chart mapping
    // recognizes. Lands in the 1990 slot, `needs_mapping = true`.
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "UNK1",
            json!({ "Id": "UNK1", "Name": "Ask My Accountant", "AccountType": "Other Current Asset",
                    "Active": true }),
        ),
    );

    seed(
        &store,
        mirrored(
            EntityType::Item,
            "501",
            json!({ "Id": "501", "Name": "Rescue Tube 50in", "Sku": "RT-50",
                    "Type": "Inventory", "PurchaseCost": 18.50,
                    "IncomeAccountRef": { "value": "INC1" }, "ExpenseAccountRef": { "value": "COGS1" },
                    "AssetAccountRef": { "value": "INV1" }, "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Item,
            "502",
            json!({ "Id": "502", "Name": "Kickboard", "Sku": "KB-01",
                    "Type": "Inventory", "PurchaseCost": 22.00,
                    "IncomeAccountRef": { "value": "INC1" }, "ExpenseAccountRef": { "value": "COGS1" },
                    "AssetAccountRef": { "value": "INV1" }, "ClassRef": { "value": "6" }, "Active": true }),
        ),
    );

    // Invoice: line 1 carries its own class; line 2 has none and falls back
    // to item 502's own default class (§3's default chain).
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "INV-1",
            json!({
                "Id": "INV-1", "DocNumber": "1001", "TxnDate": "2026-07-14",
                "CustomerRef": { "value": "31" }, "TotalAmt": 833.13,
                "TxnTaxDetail": {
                    "TotalTax": 33.13,
                    "TaxLine": [ {
                        "TaxLineDetail": { "NetAmountTaxable": 500.00, "TaxPercent": 6.625,
                                           "TaxRateRef": { "value": "1" } }
                    } ]
                },
                "Line": [
                    {
                        "Id": "1", "LineNum": 1, "Description": "Rescue tube, 50 inch",
                        "Amount": 500.00, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": {
                            "ItemRef": { "value": "501" }, "Qty": 10, "UnitPrice": 50.00,
                            "ClassRef": { "value": "4" }, "TaxCodeRef": { "value": "TAX" }
                        }
                    },
                    {
                        "Id": "2", "LineNum": 2, "Description": "Kickboard",
                        "Amount": 300.00, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": {
                            "ItemRef": { "value": "502" }, "Qty": 5, "UnitPrice": 60.00,
                            "TaxCodeRef": { "value": "NON" }
                        }
                    },
                    { "Amount": 833.13, "DetailType": "SubTotalLineDetail", "SubTotalLineDetail": {} }
                ]
            }),
        ),
    );

    // Payment: $400.00 of the $833.13 invoice, nothing unapplied.
    seed(
        &store,
        mirrored(
            EntityType::Payment,
            "PMT-1",
            json!({
                "Id": "PMT-1", "TxnDate": "2026-07-20",
                "CustomerRef": { "value": "31" }, "TotalAmt": 400.00,
                "DepositToAccountRef": { "value": "UDF1" },
                "Line": [
                    { "Amount": 400.00,
                      "LinkedTxn": [ { "TxnId": "INV-1", "TxnType": "Invoice", "Amount": 400.00 } ] }
                ]
            }),
        ),
    );

    // Bill: a non-item expense line, with a header class so the 6xxx line
    // resolves one through §3's default chain.
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "BILL-1",
            json!({
                "Id": "BILL-1", "TxnDate": "2026-07-18", "ClassRef": { "value": "4" },
                "VendorRef": { "value": "77" }, "TotalAmt": 250.00,
                "Line": [ {
                    "Amount": 250.00, "DetailType": "AccountBasedExpenseLineDetail",
                    "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } }
                } ]
            }),
        ),
    );

    // A voided invoice, still translated and posted — as a reversal (§6).
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "INV-2V",
            json!({
                "Id": "INV-2V", "TxnDate": "2026-07-22", "TxnStatus": "Voided",
                "CustomerRef": { "value": "31" }, "TotalAmt": 100.00,
                "Line": [
                    {
                        "Id": "1", "LineNum": 1, "Amount": 100.00,
                        "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": {
                            "ItemRef": { "value": "501" }, "Qty": 2, "UnitPrice": 50.00,
                            "ClassRef": { "value": "4" }, "TaxCodeRef": { "value": "NON" }
                        }
                    },
                    { "Amount": 100.00, "DetailType": "SubTotalLineDetail", "SubTotalLineDetail": {} }
                ]
            }),
        ),
    );

    store
}

fn new_ledger() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(
            "aquamentor",
            "Aquamentor LLC",
            Some(realm().as_str()),
            now(),
        )
        .unwrap();
    ledger
}

fn tb(ledger: &Ledger, as_of: NaiveDate) -> TrialBalance {
    report::trial_balance(ledger, "aquamentor", as_of).unwrap()
}

fn balance(tb: &TrialBalance, number: &str) -> Money {
    tb.rows
        .iter()
        .find(|row| row.number == number)
        .map(|row| row.balance)
        .unwrap_or(Money::ZERO)
}

#[test]
fn import_replica_posts_translates_and_reports_correctly() {
    let replica = seeded_replica();
    let ledger = new_ledger();
    let as_of = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();

    let report = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();

    // Four documents post an entry (invoice, payment, bill, voided invoice);
    // none are non-posting kinds and nothing is rejected.
    assert_eq!(report.translated, 4);
    assert_eq!(report.posted, 4);
    assert_eq!(report.non_posting, 0);
    assert_eq!(report.skipped_unchanged, 0);
    assert_eq!(report.replaced, 0);
    assert!(
        report.rejected.is_empty(),
        "rejected: {:?}",
        report.rejected
    );

    // The unmapped account materialised in the 1990 slot and is named.
    assert_eq!(report.accounts_created, 1);
    assert!(report
        .accounts_needing_mapping
        .iter()
        .any(|name| name.contains("Ask My Accountant")));
    // The two classes the invoice/bill use were already in the seed chart.
    assert_eq!(report.classes_created, 0);

    let trial_balance = tb(&ledger, as_of);
    assert_eq!(
        trial_balance.total_debits, trial_balance.total_credits,
        "the trial balance must balance"
    );
    // 1310 finished-goods inventory legitimately shows on the "wrong" side
    // here: this fixture sells inventory it never records a purchase for, so
    // the account goes negative. That is a property of the fixture, not a
    // bug the pipeline should be judged on.

    // AR = the invoice total (833.13), less the payment applied (400.00),
    // less the voided invoice's own (flipped) AR credit (100.00): 333.13.
    assert_eq!(balance(&trial_balance, "1200"), Money::from_minor(33_313));

    // 2200 sales tax payable = the invoice's tax, exactly (no other document
    // touches it).
    assert_eq!(balance(&trial_balance, "2200"), Money::from_minor(-3_313));

    // 2000 accounts payable = the bill (the only thing that touches it).
    assert_eq!(balance(&trial_balance, "2000"), Money::from_minor(-25_000));

    // The voided invoice posted a reversal: memo prefixed, AR on the
    // opposite side from a normal invoice of the same shape.
    let entries = ledger
        .entries_for_document("aquamentor", "qbo:Invoice:INV-2V")
        .unwrap();
    assert_eq!(entries.len(), 1);
    let (_, void_entry, is_posted) = &entries[0];
    assert!(is_posted);
    assert!(void_entry
        .memo
        .as_deref()
        .is_some_and(|memo| memo.starts_with("VOID:")));
    let ar_line = void_entry
        .lines
        .iter()
        .find(|line| line.account.0 == "1200")
        .expect("the voided invoice's entry still carries an AR line");
    assert_eq!(ar_line.credit, Money::from_minor(10_000));
    assert_eq!(ar_line.debit, Money::ZERO);

    // Seed-chart accounts the mapping landed on carry the QBO id, so the §7
    // diff can join 1200 and 1100 to QBO's rows rather than only the
    // accounts the import had to create.
    let ar = ledger
        .account_by_source_ref("aquamentor", "AR1")
        .unwrap()
        .expect("AR1 maps onto the seed AR account");
    assert_eq!(ar.number, "1200");
    let bank = ledger
        .account_by_source_ref("aquamentor", "BANK1")
        .unwrap()
        .expect("BANK1 maps onto the seed checking account");
    assert_eq!(bank.number, "1100");
}

#[test]
fn a_second_run_over_an_unchanged_replica_skips_everything_and_posts_nothing_new() {
    let replica = seeded_replica();
    let ledger = new_ledger();

    let first = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();

    let second = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();

    assert_eq!(second.translated, first.translated);
    assert_eq!(
        second.skipped_unchanged,
        first.posted + first.non_posting,
        "everything posted (or non-posting) the first time round should be skipped as unchanged"
    );
    assert_eq!(second.posted, 0, "nothing new should be posted");
    assert_eq!(second.replaced, 0);
    assert!(second.rejected.is_empty());
}

#[test]
fn changing_one_entity_and_re_running_reverses_and_reposts_only_that_document() {
    let replica = seeded_replica();
    let ledger = new_ledger();
    let as_of = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();

    let first = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();
    assert_eq!(first.posted, 4);

    // The bill's amount changes from 250.00 to 300.00, applied through
    // `apply_batch` with a later `last_updated_utc` — a real CDC-style
    // update, not the `seed` helper's direct upsert.
    let updated_bill = MirroredEntity {
        entity_type: EntityType::Bill,
        qbo_id: "BILL-1".to_string(),
        sync_token: "2".into(),
        last_updated_utc: later_now(),
        is_deleted: false,
        raw_json: json!({
            "Id": "BILL-1", "TxnDate": "2026-07-18", "ClassRef": { "value": "4" },
            "VendorRef": { "value": "77" }, "TotalAmt": 300.00,
            "Line": [ {
                "Amount": 300.00, "DetailType": "AccountBasedExpenseLineDetail",
                "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } }
            } ]
        }),
    };
    replica
        .apply_batch(
            &realm(),
            std::slice::from_ref(&updated_bill),
            None,
            later_now(),
        )
        .unwrap();

    let second = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        later_now(),
    )
    .unwrap();

    assert_eq!(second.replaced, 1, "only the changed bill should replace");
    assert_eq!(second.posted, 1);
    assert_eq!(second.skipped_unchanged, 3, "the other three are unchanged");
    assert!(second.rejected.is_empty());

    let trial_balance = tb(&ledger, as_of);
    assert_eq!(
        trial_balance.total_debits, trial_balance.total_credits,
        "the trial balance must still balance after the replacement"
    );
    // 2000 and 6100 both now reflect the new $300.00 bill, not the old
    // $250.00 one — the reversal cancelled the old entry exactly.
    assert_eq!(balance(&trial_balance, "2000"), Money::from_minor(-30_000));
    assert_eq!(balance(&trial_balance, "6100"), Money::from_minor(30_000));

    let entries = ledger
        .entries_for_document("aquamentor", "qbo:Bill:BILL-1")
        .unwrap();
    // The original entry, its reversal, and the new entry.
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|(_, _, posted)| *posted));
}
