//! The replica importer, end to end. `LEDGER-DESIGN.md` §6.
//!
//! Like `qbo-local`'s own `tests/query.rs`, this goes through the crate's
//! public surface only, against an in-memory replica seeded with invented
//! QBO-shaped JSON (HANDOFF.md §2.6) — no real book, no network.

use chrono::{NaiveDate, TimeZone, Utc};
use ledger::import::{run, ClassSource, ImportOptions, TranslateError};
use ledger::types::{AccountId, ClassId, ContactKind, DocKind, LineKind};
use ledger_core::Money;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn company() -> ledger::types::CompanyId {
    ledger::types::CompanyId("aquamentor".to_string())
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
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

/// One replica with every shape the importer needs to prove itself against:
/// an invoice with a payload class on one line and an item default on the
/// other, a payment with two applications and an unapplied remainder, a bill
/// with a non-item expense line, a deposit with a netted fee line, a voided
/// invoice, and a document with a bad-precision amount in a field the
/// projection does not itself read.
fn seeded_store() -> Store {
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
            json!({ "Id": "INV1", "Name": "Inventory Asset", "AccountType": "Other Current Asset",
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
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "FEE1",
            json!({ "Id": "FEE1", "Name": "Bank Charges", "AccountType": "Expense",
                    "AccountSubType": "BankCharges", "Active": true }),
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
                    "Type": "Inventory", "PurchaseCost": 22.375,
                    "IncomeAccountRef": { "value": "INC1" }, "ExpenseAccountRef": { "value": "COGS1" },
                    "AssetAccountRef": { "value": "INV1" }, "ClassRef": { "value": "6" }, "Active": true }),
        ),
    );

    // Invoice: line 1 carries its own ClassRef; line 2 carries none, so it
    // falls back to item 502's own default class (ClassSource::ItemDefault).
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "INV-418",
            json!({
                "Id": "INV-418", "DocNumber": "1088", "TxnDate": "2026-07-14",
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

    // Payment: two applications and an unapplied remainder.
    seed(
        &store,
        mirrored(
            EntityType::Payment,
            "PMT-500",
            json!({
                "Id": "PMT-500", "TxnDate": "2026-07-20",
                "CustomerRef": { "value": "31" }, "TotalAmt": 900.00,
                "DepositToAccountRef": { "value": "UDF1" }, "UnappliedAmt": 50.00,
                "Line": [
                    { "Amount": 400.00,
                      "LinkedTxn": [ { "TxnId": "INV-418", "TxnType": "Invoice", "Amount": 400.00 } ] },
                    { "Amount": 450.00,
                      "LinkedTxn": [ { "TxnId": "INV-999", "TxnType": "Invoice", "Amount": 450.00 } ] }
                ]
            }),
        ),
    );

    // Bill: a non-item expense line, `AccountBasedExpenseLineDetail`.
    seed(
        &store,
        mirrored(
            EntityType::Bill,
            "BILL-700",
            json!({
                "Id": "BILL-700", "TxnDate": "2026-07-18",
                "VendorRef": { "value": "77" }, "TotalAmt": 250.00,
                "Line": [ {
                    "Amount": 250.00, "DetailType": "AccountBasedExpenseLineDetail",
                    "AccountBasedExpenseLineDetail": { "AccountRef": { "value": "EXP1" } }
                } ]
            }),
        ),
    );

    // Deposit: a customer payment grouped in, plus a netted fee line.
    seed(
        &store,
        mirrored(
            EntityType::Deposit,
            "DEP-800",
            json!({
                "Id": "DEP-800", "TxnDate": "2026-07-21",
                "DepositToAccountRef": { "value": "BANK1" }, "TotalAmt": 485.00,
                "Line": [
                    { "Amount": 500.00, "DetailType": "DepositLineDetail",
                      "DepositLineDetail": { "Entity": { "value": "31", "type": "Customer" },
                                              "AccountRef": { "value": "UDF1" } } },
                    { "Amount": -15.00, "DetailType": "DepositLineDetail",
                      "DepositLineDetail": { "AccountRef": { "value": "FEE1" } } }
                ]
            }),
        ),
    );

    // A voided invoice: still translated, never dropped (§6).
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "INV-419",
            json!({
                "Id": "INV-419", "TxnDate": "2026-07-22", "TxnStatus": "Voided",
                "CustomerRef": { "value": "31" }, "TotalAmt": 0.00
            }),
        ),
    );

    // A document whose total the projection reads fine, but whose
    // `UnappliedAmt` — a field only the importer itself reads — carries three
    // decimal places. Must fail with `TranslateError::Precision`, never
    // rounded (D9).
    seed(
        &store,
        mirrored(
            EntityType::Payment,
            "PMT-BAD",
            json!({
                "Id": "PMT-BAD", "TxnDate": "2026-07-23",
                "CustomerRef": { "value": "31" }, "TotalAmt": 100.12,
                "UnappliedAmt": 100.123,
                "Line": []
            }),
        ),
    );

    store
}

#[test]
fn the_import_translates_every_document_shape_and_reports_the_precision_failure() {
    let store = seeded_store();

    let mut collected = Vec::new();
    let report = run(
        &store,
        &realm(),
        &company(),
        &ImportOptions::default(),
        &mut |translated| {
            collected.push(translated);
            Ok(())
        },
    )
    .unwrap();

    // Six documents seeded; one (PMT-BAD) fails translation on a precision
    // error and is reported rather than handed to the sink.
    assert_eq!(report.translated, 5);
    assert_eq!(report.voided, 1);
    assert_eq!(report.precision_failures, vec!["PMT-BAD".to_string()]);
    assert!(report.unmatched_classes.is_empty());
    assert!(report.accounts_needing_mapping.is_empty());

    assert_eq!(collected.len(), 5);

    // Documents come out sorted by (txn_date, qbo_id).
    let ids: Vec<&str> = collected
        .iter()
        .map(|t| t.document.source_ref.as_deref().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["INV-418", "BILL-700", "PMT-500", "DEP-800", "INV-419"]
            .into_iter()
            .collect::<Vec<_>>()
    );

    // --- the invoice: two lines, one payload class, one item-default class ---
    let invoice = collected
        .iter()
        .find(|t| t.document.source_ref.as_deref() == Some("INV-418"))
        .unwrap();
    assert_eq!(invoice.document.document_id, "qbo:Invoice:INV-418");
    assert_eq!(invoice.document.kind, DocKind::Invoice);
    assert_eq!(invoice.document.number.as_deref(), Some("1088"));
    assert_eq!(
        invoice.document.txn_date,
        NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()
    );
    assert_eq!(
        invoice.document.contact,
        Some(ledger::types::ContactRef {
            kind: ContactKind::Customer,
            id: "31".into()
        })
    );
    assert!(!invoice.document.is_voided);

    let tax = invoice
        .document
        .tax
        .as_ref()
        .expect("invoice should carry tax detail");
    assert_eq!(tax.total_tax, Money::from_minor(3_313));
    assert_eq!(tax.taxable_base, Money::from_minor(50_000));
    assert_eq!(tax.rate, rust_decimal::Decimal::new(6625, 5));

    assert_eq!(invoice.document.lines.len(), 2);
    let line1 = &invoice.document.lines[0];
    assert_eq!(line1.kind, LineKind::Item);
    assert_eq!(line1.amount, Money::from_minor(50_000));
    assert_eq!(line1.class, Some(ClassId("foam".into())));
    assert!(line1.is_taxable);
    assert_eq!(line1.unit_cost, Some(Money::from_minor(1_850)));

    let line2 = &invoice.document.lines[1];
    assert_eq!(line2.kind, LineKind::Item);
    assert_eq!(line2.amount, Money::from_minor(30_000));
    assert_eq!(line2.class, Some(ClassId("chair".into())));
    assert!(!line2.is_taxable);
    // 22.375 carries three decimal places: a documented exception to D9 for
    // unit_cost specifically — the line survives, unit_cost comes back None,
    // and a warning names it.
    assert_eq!(line2.unit_cost, None);
    assert!(invoice
        .warnings
        .iter()
        .any(|w| w.contains("502") && w.contains("purchase cost")));

    assert_eq!(
        invoice.class_sources,
        vec![(1, ClassSource::Payload), (2, ClassSource::ItemDefault)]
    );

    // --- the payment: two applications, an unapplied remainder, deposit_to ---
    let payment = collected
        .iter()
        .find(|t| t.document.source_ref.as_deref() == Some("PMT-500"))
        .unwrap();
    assert_eq!(payment.document.kind, DocKind::Payment);
    assert_eq!(payment.document.deposit_to, Some(AccountId("1150".into())));
    assert_eq!(payment.document.unapplied, Money::from_minor(5_000));
    assert_eq!(payment.document.applications.len(), 2);
    assert_eq!(
        payment.document.applications[0].target_document_id,
        "INV-418"
    );
    assert_eq!(
        payment.document.applications[0].target_kind,
        DocKind::Invoice
    );
    assert_eq!(
        payment.document.applications[0].amount,
        Money::from_minor(40_000)
    );
    assert_eq!(
        payment.document.applications[1].target_document_id,
        "INV-999"
    );
    assert_eq!(
        payment.document.applications[1].amount,
        Money::from_minor(45_000)
    );

    // --- the bill: a non-item expense line ---
    let bill = collected
        .iter()
        .find(|t| t.document.source_ref.as_deref() == Some("BILL-700"))
        .unwrap();
    assert_eq!(bill.document.kind, DocKind::Bill);
    assert_eq!(bill.document.lines.len(), 1);
    assert_eq!(bill.document.lines[0].kind, LineKind::Account);
    assert_eq!(
        bill.document.lines[0].account,
        Some(AccountId("6100".into()))
    );
    assert_eq!(bill.document.lines[0].amount, Money::from_minor(25_000));

    // --- the deposit: a grouped payment plus a netted fee line ---
    let deposit = collected
        .iter()
        .find(|t| t.document.source_ref.as_deref() == Some("DEP-800"))
        .unwrap();
    assert_eq!(deposit.document.kind, DocKind::Deposit);
    assert_eq!(deposit.document.lines.len(), 2);
    let grouped = &deposit.document.lines[0];
    assert_eq!(grouped.kind, LineKind::Account);
    assert_eq!(grouped.account, Some(AccountId("1150".into())));
    assert_eq!(
        grouped.entity,
        Some(ledger::types::ContactRef {
            kind: ContactKind::Customer,
            id: "31".into()
        })
    );
    let fee = &deposit.document.lines[1];
    assert_eq!(fee.kind, LineKind::Account);
    assert_eq!(fee.account, Some(AccountId("6700".into())));
    assert_eq!(fee.amount, Money::from_minor(-1_500));
    assert_eq!(fee.entity, None);

    // --- the voided invoice ---
    let voided = collected
        .iter()
        .find(|t| t.document.source_ref.as_deref() == Some("INV-419"))
        .unwrap();
    assert!(voided.document.is_voided);

    // --- idempotence: a second run over the same replica is byte-identical ---
    let mut second_run = Vec::new();
    let second_report = run(
        &store,
        &realm(),
        &company(),
        &ImportOptions::default(),
        &mut |translated| {
            second_run.push(translated);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(collected, second_run);
    assert_eq!(report, second_report);
}

#[test]
fn a_line_with_no_class_from_any_source_is_reported_and_left_unclassed() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();

    seed(
        &store,
        mirrored(
            EntityType::Customer,
            "31",
            json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Item,
            "900",
            json!({ "Id": "900", "Name": "Miscellaneous Service", "Type": "Service", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Invoice,
            "INV-1",
            json!({
                "Id": "INV-1", "TxnDate": "2026-08-01",
                "CustomerRef": { "value": "31" }, "TotalAmt": 100.00,
                "Line": [ {
                    "Amount": 100.00, "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": { "ItemRef": { "value": "900" } }
                } ]
            }),
        ),
    );

    let mut collected = Vec::new();
    let report = run(
        &store,
        &realm(),
        &company(),
        &ImportOptions::default(),
        &mut |translated| {
            collected.push(translated);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(report.translated, 1);
    let translated = &collected[0];
    assert!(translated.class_sources.is_empty());
    assert_eq!(translated.document.lines[0].class, None);
    assert!(translated.warnings.iter().any(|w| w.contains("no class")));
}

#[test]
fn options_from_and_to_filter_by_txn_date() {
    let store = seeded_store();

    let mut collected = Vec::new();
    let options = ImportOptions {
        from: NaiveDate::from_ymd_opt(2026, 7, 20),
        to: NaiveDate::from_ymd_opt(2026, 7, 21),
    };
    let report = run(&store, &realm(), &company(), &options, &mut |translated| {
        collected.push(translated);
        Ok(())
    })
    .unwrap();

    let ids: Vec<&str> = collected
        .iter()
        .map(|t| t.document.source_ref.as_deref().unwrap())
        .collect();
    assert_eq!(ids, vec!["PMT-500", "DEP-800"]);
    assert_eq!(report.translated, 2);
}

#[test]
fn map_txn_type_reports_unknown_link_types_rather_than_dropping_them() {
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();

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
            EntityType::BillPayment,
            "BP-1",
            json!({
                "Id": "BP-1", "TxnDate": "2026-08-02", "VendorRef": { "value": "77" },
                "TotalAmt": 100.00,
                "Line": [ { "Amount": 100.00,
                    "LinkedTxn": [ { "TxnId": "801", "TxnType": "ReimburseCharge", "Amount": 100.00 } ] } ]
            }),
        ),
    );

    let mut collected = Vec::new();
    let _ = run(
        &store,
        &realm(),
        &company(),
        &ImportOptions::default(),
        &mut |translated| {
            collected.push(translated);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(collected.len(), 1);
    let translated = &collected[0];
    assert!(translated.document.applications.is_empty());
    assert!(translated
        .warnings
        .iter()
        .any(|w| w.contains("ReimburseCharge") && w.contains("not applied")));
}

#[test]
fn precision_error_message_names_the_field() {
    let entity = mirrored(
        EntityType::Payment,
        "PMT-BAD",
        json!({
            "Id": "PMT-BAD", "TxnDate": "2026-07-23",
            "CustomerRef": { "value": "31" }, "TotalAmt": 100.12,
            "UnappliedAmt": 100.123,
            "Line": []
        }),
    );
    let ctx = ledger::import::TranslateContext::default();
    let err = ledger::import::translate(&entity, &ctx).unwrap_err();
    match err {
        TranslateError::Precision { field, qbo_id, .. } => {
            assert_eq!(field, "UnappliedAmt");
            assert_eq!(qbo_id, "PMT-BAD");
        }
        other => panic!("expected a precision error, got {other:?}"),
    }
}
