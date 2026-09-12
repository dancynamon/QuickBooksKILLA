//! The pipeline end to end: replica in, posted entries out. `LEDGER-DESIGN.md`
//! §6. Fixture-building helpers (`mirrored`/`seed`) follow the pattern in
//! `tests/import.rs`; the documents here are a fresh, self-contained set
//! chosen so every pipeline assertion (AR, tax, AP, the void, re-run
//! idempotence, re-run replacement) has one clean number to check.

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::chart;
use ledger::import::ImportOptions;
use ledger::pipeline::{apply_opening_balance, boundary_walk, import_replica};
use ledger::report::{self, TierRules, TrialBalance};
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

// ---------------------------------------------------------------------------
// Non-posting documents on re-run (§6): an unchanged Estimate is skipped,
// never re-saved as a fresh version.
// ---------------------------------------------------------------------------

/// A minimal replica with one non-posting document: an Estimate for a
/// service item, nothing else. Chosen small and separate from
/// `seeded_replica` so this test's counts do not entangle with the posting
/// documents there.
fn estimate_replica() -> Store {
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
            EntityType::Class,
            "4",
            json!({ "Id": "4", "Name": "Foam Products", "Active": true }),
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
            EntityType::Item,
            "999",
            json!({ "Id": "999", "Name": "Custom Job", "Type": "Service",
                    "IncomeAccountRef": { "value": "INC1" }, "ClassRef": { "value": "4" }, "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Estimate,
            "EST-1",
            json!({
                "Id": "EST-1", "DocNumber": "1001", "TxnDate": "2026-07-14",
                "CustomerRef": { "value": "31" }, "TotalAmt": 500.00,
                "Line": [
                    {
                        "Id": "1", "LineNum": 1, "Amount": 500.00,
                        "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": {
                            "ItemRef": { "value": "999" }, "Qty": 1, "UnitPrice": 500.00
                        }
                    },
                    { "Amount": 500.00, "DetailType": "SubTotalLineDetail", "SubTotalLineDetail": {} }
                ]
            }),
        ),
    );

    store
}

#[test]
fn a_second_run_does_not_resave_an_unchanged_estimate() {
    let replica = estimate_replica();
    let ledger = new_ledger();
    let document_id = "qbo:Estimate:EST-1";

    let first = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();
    assert_eq!(first.translated, 1);
    assert_eq!(first.non_posting, 1);
    assert_eq!(first.posted, 0);
    assert_eq!(first.skipped_unchanged, 0);
    assert!(first.rejected.is_empty(), "rejected: {:?}", first.rejected);
    assert_eq!(
        ledger
            .document_version_count("aquamentor", document_id)
            .unwrap(),
        1
    );

    let second = import_replica(
        &ledger,
        &replica,
        &realm(),
        "aquamentor",
        &ImportOptions::default(),
        now(),
    )
    .unwrap();
    assert_eq!(second.translated, 1);
    assert_eq!(
        second.non_posting, 0,
        "the unchanged estimate is skipped, not re-saved"
    );
    assert_eq!(second.skipped_unchanged, 1);
    assert!(second.rejected.is_empty());

    assert_eq!(
        ledger
            .document_version_count("aquamentor", document_id)
            .unwrap(),
        1,
        "an unchanged estimate must not gain a second document_versions row"
    );
}

// ---------------------------------------------------------------------------
// Opening balances wired into the pipeline (§6)
// ---------------------------------------------------------------------------

#[test]
fn apply_opening_balance_is_idempotent_and_replaces_on_change() {
    let ledger = new_ledger();
    ledger
        .set_account_source_ref("aquamentor", chart::CHECKING, "35")
        .unwrap();
    ledger
        .set_account_source_ref("aquamentor", chart::ACCOUNTS_RECEIVABLE, "79")
        .unwrap();

    let as_of = NaiveDate::from_ymd_opt(2022, 12, 31).unwrap();
    let rows = vec![
        report::QboTbRow {
            qbo_account_id: "35".to_string(),
            name: "Checking".to_string(),
            balance: Money::from_minor(1_000_000),
        },
        report::QboTbRow {
            qbo_account_id: "79".to_string(),
            name: "Accounts Receivable".to_string(),
            balance: Money::from_minor(250_000),
        },
    ];

    let first = apply_opening_balance(&ledger, "aquamentor", as_of, &rows, now()).unwrap();
    assert!(!first.skipped_unchanged);
    assert!(!first.replaced);
    assert_eq!(
        ledger
            .document_version_count("aquamentor", &first.document_id)
            .unwrap(),
        1
    );

    // Re-run, identical rows: skipped, no new version, nothing reversed.
    let second = apply_opening_balance(&ledger, "aquamentor", as_of, &rows, now()).unwrap();
    assert!(second.skipped_unchanged);
    assert!(!second.replaced);
    assert_eq!(second.document_id, first.document_id);
    assert_eq!(
        ledger
            .document_version_count("aquamentor", &second.document_id)
            .unwrap(),
        1,
        "an unchanged re-run must not add a document_versions row"
    );

    // Re-run, one row changed: reversed and reposted as a new version.
    let changed_rows = vec![
        report::QboTbRow {
            qbo_account_id: "35".to_string(),
            name: "Checking".to_string(),
            balance: Money::from_minor(1_100_000),
        },
        report::QboTbRow {
            qbo_account_id: "79".to_string(),
            name: "Accounts Receivable".to_string(),
            balance: Money::from_minor(250_000),
        },
    ];
    let third =
        apply_opening_balance(&ledger, "aquamentor", as_of, &changed_rows, later_now()).unwrap();
    assert!(third.replaced);
    assert!(!third.skipped_unchanged);
    assert_eq!(
        ledger
            .document_version_count("aquamentor", &third.document_id)
            .unwrap(),
        2
    );

    let trial_balance = tb(&ledger, as_of);
    assert_eq!(
        balance(&trial_balance, chart::CHECKING),
        Money::from_minor(1_100_000)
    );
}

// ---------------------------------------------------------------------------
// The boundary-year walk (§6)
// ---------------------------------------------------------------------------

/// Two years of activity on the same account, so a snapshot per year is
/// enough to drive the walk: a $500.00 journal entry in 2024 and a $300.00
/// one in 2025, each debiting Accounts Receivable and crediting an account
/// mapped onto 3950 opening balance equity — a tier the §7 diff always skips
/// (`LEDGER-DESIGN.md` §7 "expected to differ, not diffed"), so the fixture's
/// AR side is the only thing the diff can possibly disagree on.
fn two_year_replica() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();

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
            "OBE1",
            json!({ "Id": "OBE1", "Name": "Opening Balance Equity", "AccountType": "Equity",
                    "AccountSubType": "OpeningBalanceEquity", "Active": true }),
        ),
    );

    seed(
        &store,
        mirrored(
            EntityType::JournalEntry,
            "JE-2024",
            json!({
                "Id": "JE-2024", "TxnDate": "2024-06-01", "TotalAmt": 500.00,
                "Line": [
                    { "Amount": 500.00, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Debit", "AccountRef": { "value": "AR1" } } },
                    { "Amount": 500.00, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Credit", "AccountRef": { "value": "OBE1" } } }
                ]
            }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::JournalEntry,
            "JE-2025",
            json!({
                "Id": "JE-2025", "TxnDate": "2025-06-01", "TotalAmt": 300.00,
                "Line": [
                    { "Amount": 300.00, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Debit", "AccountRef": { "value": "AR1" } } },
                    { "Amount": 300.00, "DetailType": "JournalEntryLineDetail",
                      "JournalEntryLineDetail": { "PostingType": "Credit", "AccountRef": { "value": "OBE1" } } }
                ]
            }),
        ),
    );

    store
}

#[test]
fn boundary_walk_stops_at_the_first_disagreement_and_names_the_boundary_year() {
    let replica = two_year_replica();
    let real_ledger = new_ledger();
    let tiers = TierRules::default();

    // 2024 ("year 1"): QBO reports AR at $999.00 as of 31 Dec 2024 —
    // deliberately wrong. Replaying 2024 alone (no prior-year opening
    // balance is given) posts exactly $500.00 to AR, so 2024 disagrees.
    let snap_2024 = vec![report::QboTbRow {
        qbo_account_id: "AR1".to_string(),
        name: "Accounts Receivable".to_string(),
        balance: Money::from_minor(99_900),
    }];
    // 2025 ("year 2"): replaying 2025 on top of an opening balance taken
    // from the (deliberately wrong) 2024 QBO figure gives AR = 999.00 +
    // 300.00 = 1,299.00 — exactly what this snapshot reports, so 2025 agrees.
    let snap_2025 = vec![report::QboTbRow {
        qbo_account_id: "AR1".to_string(),
        name: "Accounts Receivable".to_string(),
        balance: Money::from_minor(129_900),
    }];
    let snapshots = vec![(2024, snap_2024.clone()), (2025, snap_2025)];

    let walk_report = boundary_walk(
        &real_ledger,
        &replica,
        &realm(),
        "aquamentor",
        &snapshots,
        &tiers,
        now(),
    )
    .unwrap();

    assert_eq!(
        walk_report.years.len(),
        2,
        "the walk examines both years before stopping"
    );
    let year_2025 = walk_report
        .years
        .iter()
        .find(|year| year.year == 2025)
        .unwrap();
    let year_2024 = walk_report
        .years
        .iter()
        .find(|year| year.year == 2024)
        .unwrap();
    assert!(
        year_2025.agrees,
        "2025 should agree: {:?}",
        year_2025.diff.must_failures
    );
    assert!(!year_2024.agrees, "2024 should disagree on AR");
    assert!(
        year_2024
            .diff
            .must_failures
            .iter()
            .any(|failure| failure.contains("Accounts receivable")),
        "expected an AR must-failure for 2024, got {:?}",
        year_2024.diff.must_failures
    );
    assert_eq!(walk_report.boundary_year, Some(2025));

    // The walk never touched `real_ledger` (it only borrowed it): nothing
    // has been created against "aquamentor" there yet.
    assert_eq!(real_ledger.oplog_len("aquamentor").unwrap(), 0);

    // Opening balance re-run is skipped: applying the year-1 snapshot as the
    // real ledger's own opening balance twice only ever posts once.
    real_ledger
        .set_account_source_ref("aquamentor", chart::ACCOUNTS_RECEIVABLE, "AR1")
        .unwrap();
    let as_of = NaiveDate::from_ymd_opt(2024, 12, 31).unwrap();
    let first =
        apply_opening_balance(&real_ledger, "aquamentor", as_of, &snap_2024, now()).unwrap();
    assert!(!first.skipped_unchanged);
    let second =
        apply_opening_balance(&real_ledger, "aquamentor", as_of, &snap_2024, now()).unwrap();
    assert!(
        second.skipped_unchanged,
        "re-applying the same opening balance a second time must be a no-op"
    );
    assert_eq!(
        real_ledger
            .document_version_count("aquamentor", &second.document_id)
            .unwrap(),
        1,
        "the re-run must not have saved a new document version"
    );
}
