//! `post`, end to end, against the crate's public surface only. Each test
//! proves one row of `LEDGER-DESIGN.md` §1: the exact accounts, sides,
//! amounts and classes a document produces. Every name, number and figure
//! below is invented for the test.

use std::collections::HashMap;

use chrono::NaiveDate;
use ledger::chart;
use ledger::post::{post, PostError, Posting};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, DocKind, DocLine, ItemAccounts,
    JournalLine, LedgerDocument, LineKind, PostingConfig, PostingContext, Side, TaxDetail,
};
use ledger_core::Money;
use proptest::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn acct(number: &str) -> AccountId {
    AccountId(number.to_string())
}

fn class(name: &str) -> ClassId {
    ClassId(name.to_string())
}

fn customer(id: &str) -> ContactRef {
    ContactRef {
        kind: ContactKind::Customer,
        id: id.to_string(),
    }
}

fn vendor(id: &str) -> ContactRef {
    ContactRef {
        kind: ContactKind::Vendor,
        id: id.to_string(),
    }
}

fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 12).expect("valid date")
}

/// A bare line at the given kind and amount, everything else empty. Tests
/// set only the fields their scenario needs.
fn line(no: i64, kind: LineKind, amount_minor: i64) -> DocLine {
    DocLine {
        line_no: no,
        kind,
        amount: Money::from_minor(amount_minor),
        class: None,
        item_id: None,
        account: None,
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

/// A document with every field empty except its kind and date. Tests fill
/// in `lines`, `contact`, `tax` and the rest as needed.
fn doc(kind: DocKind) -> LedgerDocument {
    LedgerDocument {
        document_id: "doc-1".into(),
        kind,
        number: None,
        txn_date: date(),
        due_date: None,
        contact: None,
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

fn ctx_with_items(items: Vec<(&str, ItemAccounts)>) -> PostingContext {
    let mut map = HashMap::new();
    for (id, accounts) in items {
        map.insert(id.to_string(), accounts);
    }
    PostingContext {
        items: map,
        customers: HashMap::new(),
        config: PostingConfig::default(),
    }
}

fn service_item(income: &str, expense: &str, default_class: Option<ClassId>) -> ItemAccounts {
    ItemAccounts {
        income: acct(income),
        expense: acct(expense),
        asset: None,
        default_class,
        unit_cost: None,
    }
}

fn tracked_item(
    income: &str,
    expense: &str,
    asset: &str,
    default_class: Option<ClassId>,
    unit_cost_minor: i64,
) -> ItemAccounts {
    ItemAccounts {
        income: acct(income),
        expense: acct(expense),
        asset: Some(acct(asset)),
        default_class,
        unit_cost: Some(Money::from_minor(unit_cost_minor)),
    }
}

fn entry_of(posting: Posting) -> ledger::types::JournalEntry {
    match posting {
        Posting::Entry(entry) => entry,
        Posting::NonPosting => panic!("expected a posting entry"),
    }
}

fn find<'a>(lines: &'a [JournalLine], account: &AccountId) -> &'a JournalLine {
    lines
        .iter()
        .find(|line| &line.account == account)
        .unwrap_or_else(|| panic!("no line for account {}", account.0))
}

// ---------------------------------------------------------------------
// Invoice
// ---------------------------------------------------------------------

#[test]
fn invoice_posts_ar_sales_shipping_discount_tax_and_cogs() {
    let ctx = ctx_with_items(vec![(
        "tube",
        tracked_item(
            chart::SALES_INCOME,
            chart::COGS,
            chart::INVENTORY_FINISHED,
            Some(class("foam")),
            1200,
        ),
    )]);

    let mut item_line = line(1, LineKind::Item, 10000);
    item_line.item_id = Some("tube".into());
    item_line.qty = Some(dec!(4));

    let mut shipping_line = line(2, LineKind::Shipping, 2499);
    shipping_line.class = Some(class("foam"));

    let mut discount_line = line(3, LineKind::Discount, 500);
    discount_line.class = Some(class("foam"));

    let mut invoice = doc(DocKind::Invoice);
    invoice.contact = Some(customer("cust-1"));
    invoice.lines = vec![item_line, shipping_line, discount_line];
    invoice.tax = Some(TaxDetail {
        total_tax: Money::from_minor(773),
        taxable_base: Money::from_minor(12499),
        rate: dec!(0.06625),
    });

    let entry = entry_of(post(&invoice, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    assert_eq!(entry.source_type, DocKind::Invoice);
    assert_eq!(entry.source_id, "doc-1");
    assert_eq!(entry.source_version, 1);

    let ar = find(&entry.lines, &acct(chart::ACCOUNTS_RECEIVABLE));
    // 10000 sales + 2499 shipping - 500 discount + 773 tax
    assert_eq!(ar.debit, Money::from_minor(12772));
    assert_eq!(ar.credit, Money::ZERO);
    assert_eq!(ar.class, None);
    assert_eq!(ar.entity, Some(customer("cust-1")));

    let sales = find(&entry.lines, &acct(chart::SALES_INCOME));
    assert_eq!(sales.credit, Money::from_minor(10000));
    assert_eq!(sales.class, Some(class("foam")));

    let shipping = find(&entry.lines, &acct(chart::SHIPPING_INCOME));
    assert_eq!(shipping.credit, Money::from_minor(2499));
    assert_eq!(shipping.class, Some(class("foam")));

    let discount = find(&entry.lines, &acct(chart::DISCOUNTS_GIVEN));
    assert_eq!(discount.debit, Money::from_minor(500));
    assert_eq!(discount.class, Some(class("foam")));

    let tax = find(&entry.lines, &acct(chart::SALES_TAX_PAYABLE));
    assert_eq!(tax.credit, Money::from_minor(773));
    assert_eq!(tax.class, None);

    // Inventory line: qty 4 x unit cost 12.00 = 48.00.
    let cogs = find(&entry.lines, &acct(chart::COGS));
    assert_eq!(cogs.debit, Money::from_minor(4800));
    assert_eq!(cogs.class, Some(class("foam")));
    let asset = find(&entry.lines, &acct(chart::INVENTORY_FINISHED));
    assert_eq!(asset.credit, Money::from_minor(4800));
    assert_eq!(asset.class, None);
}

#[test]
fn invoice_line_unit_cost_overrides_the_item_default() {
    let ctx = ctx_with_items(vec![(
        "tube",
        tracked_item(
            chart::SALES_INCOME,
            chart::COGS,
            chart::INVENTORY_FINISHED,
            Some(class("foam")),
            1200,
        ),
    )]);
    let mut item_line = line(1, LineKind::Item, 10000);
    item_line.item_id = Some("tube".into());
    item_line.qty = Some(dec!(2));
    item_line.unit_cost = Some(Money::from_minor(900)); // overrides the item's 12.00

    let mut invoice = doc(DocKind::Invoice);
    invoice.lines = vec![item_line];

    let entry = entry_of(post(&invoice, 1, &ctx).expect("posts"));
    let cogs = find(&entry.lines, &acct(chart::COGS));
    assert_eq!(cogs.debit, Money::from_minor(1800)); // 2 x 9.00
}

#[test]
fn invoice_service_line_posts_no_cogs() {
    let ctx = ctx_with_items(vec![(
        "labor",
        service_item(chart::SALES_INCOME, chart::COGS, Some(class("cnc"))),
    )]);
    let mut item_line = line(1, LineKind::Item, 5000);
    item_line.item_id = Some("labor".into());
    let mut invoice = doc(DocKind::Invoice);
    invoice.lines = vec![item_line];

    let entry = entry_of(post(&invoice, 1, &ctx).expect("posts"));
    assert!(entry.lines.iter().all(|l| l.account != acct(chart::COGS)));
    assert_eq!(entry.lines.len(), 2); // AR and sales only
}

// ---------------------------------------------------------------------
// SalesReceipt
// ---------------------------------------------------------------------

#[test]
fn sales_receipt_debits_deposit_to_with_no_ar_leg() {
    let ctx = ctx_with_items(vec![]);
    let mut item_line = line(1, LineKind::Item, 5995);
    item_line.class = Some(class("foam"));
    let mut receipt = doc(DocKind::SalesReceipt);
    receipt.deposit_to = Some(acct(chart::CHECKING));
    receipt.lines = vec![item_line];

    let entry = entry_of(post(&receipt, 1, &ctx).expect("posts"));
    assert!(entry
        .lines
        .iter()
        .all(|l| l.account != acct(chart::ACCOUNTS_RECEIVABLE)));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.debit, Money::from_minor(5995));
    assert_eq!(bank.entity, None);
}

#[test]
fn sales_receipt_defaults_to_undeposited_funds() {
    let ctx = ctx_with_items(vec![]);
    let mut item_line = line(1, LineKind::Item, 1000);
    item_line.class = Some(class("foam"));
    let mut receipt = doc(DocKind::SalesReceipt);
    receipt.lines = vec![item_line];

    let entry = entry_of(post(&receipt, 1, &ctx).expect("posts"));
    let bank = find(&entry.lines, &acct(chart::UNDEPOSITED_FUNDS));
    assert_eq!(bank.debit, Money::from_minor(1000));
}

// ---------------------------------------------------------------------
// CreditMemo
// ---------------------------------------------------------------------

#[test]
fn credit_memo_debits_returns_and_credits_ar_with_inventory_return() {
    let ctx = ctx_with_items(vec![(
        "tube",
        tracked_item(
            chart::SALES_INCOME,
            chart::COGS,
            chart::INVENTORY_FINISHED,
            Some(class("foam")),
            1200,
        ),
    )]);
    let mut item_line = line(1, LineKind::Item, 2400);
    item_line.item_id = Some("tube".into());
    item_line.qty = Some(dec!(2));

    let mut memo = doc(DocKind::CreditMemo);
    memo.contact = Some(customer("cust-1"));
    memo.lines = vec![item_line];
    memo.tax = Some(TaxDetail {
        total_tax: Money::from_minor(159),
        taxable_base: Money::from_minor(2400),
        rate: dec!(0.06625),
    });

    let entry = entry_of(post(&memo, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());

    let returns = find(&entry.lines, &acct(chart::RETURNS_ALLOWANCES));
    assert_eq!(returns.debit, Money::from_minor(2400));
    assert_eq!(returns.class, Some(class("foam")));

    let tax = find(&entry.lines, &acct(chart::SALES_TAX_PAYABLE));
    assert_eq!(tax.debit, Money::from_minor(159));

    let ar = find(&entry.lines, &acct(chart::ACCOUNTS_RECEIVABLE));
    assert_eq!(ar.credit, Money::from_minor(2559));
    assert_eq!(ar.entity, Some(customer("cust-1")));

    // Physical return: 2 x 12.00 back onto the shelf.
    let asset = find(&entry.lines, &acct(chart::INVENTORY_FINISHED));
    assert_eq!(asset.debit, Money::from_minor(2400));
    let cogs = find(&entry.lines, &acct(chart::COGS));
    assert_eq!(cogs.credit, Money::from_minor(2400));
}

// ---------------------------------------------------------------------
// RefundReceipt
// ---------------------------------------------------------------------

#[test]
fn refund_receipt_credits_pay_from_with_no_ar_leg() {
    let ctx = ctx_with_items(vec![]);
    let mut item_line = line(1, LineKind::Item, 1995);
    item_line.class = Some(class("foam"));
    let mut refund = doc(DocKind::RefundReceipt);
    refund.pay_from = Some(acct(chart::CHECKING));
    refund.lines = vec![item_line];

    let entry = entry_of(post(&refund, 1, &ctx).expect("posts"));
    assert!(entry
        .lines
        .iter()
        .all(|l| l.account != acct(chart::ACCOUNTS_RECEIVABLE)));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.credit, Money::from_minor(1995));
}

// ---------------------------------------------------------------------
// Payment
// ---------------------------------------------------------------------

#[test]
fn payment_splits_between_ar_and_unapplied() {
    let ctx = ctx_with_items(vec![]);
    let mut payment = doc(DocKind::Payment);
    payment.contact = Some(customer("cust-1"));
    payment.deposit_to = Some(acct(chart::CHECKING));
    payment.applications = vec![Application {
        target_document_id: "inv-1".into(),
        target_kind: DocKind::Invoice,
        amount: Money::from_minor(8000),
    }];
    payment.unapplied = Money::from_minor(2000);

    let entry = entry_of(post(&payment, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.debit, Money::from_minor(10000));
    let ar = find(&entry.lines, &acct(chart::ACCOUNTS_RECEIVABLE));
    assert_eq!(ar.credit, Money::from_minor(8000));
    assert_eq!(ar.entity, Some(customer("cust-1")));
    let deposits = find(&entry.lines, &acct(chart::CUSTOMER_DEPOSITS));
    assert_eq!(deposits.credit, Money::from_minor(2000));
}

#[test]
fn payment_defaults_to_undeposited_funds() {
    let ctx = ctx_with_items(vec![]);
    let mut payment = doc(DocKind::Payment);
    payment.applications = vec![Application {
        target_document_id: "inv-1".into(),
        target_kind: DocKind::Invoice,
        amount: Money::from_minor(500),
    }];

    let entry = entry_of(post(&payment, 1, &ctx).expect("posts"));
    let bank = find(&entry.lines, &acct(chart::UNDEPOSITED_FUNDS));
    assert_eq!(bank.debit, Money::from_minor(500));
}

#[test]
fn payment_that_does_not_add_up_errors() {
    let ctx = ctx_with_items(vec![]);
    let mut payment = doc(DocKind::Payment);
    payment.applications = vec![Application {
        target_document_id: "inv-1".into(),
        target_kind: DocKind::Invoice,
        amount: Money::from_minor(8000),
    }];
    payment.unapplied = Money::from_minor(2000);
    // A deposit slip line stating a different amount than applications +
    // unapplied actually cover.
    payment.lines = vec![line(1, LineKind::Description, 9000)];

    let error = post(&payment, 1, &ctx).expect_err("should not add up");
    match error {
        PostError::PaymentDoesNotAddUp {
            applications,
            unapplied,
            stated,
            ..
        } => {
            assert_eq!(applications, Money::from_minor(8000));
            assert_eq!(unapplied, Money::from_minor(2000));
            assert_eq!(stated, Money::from_minor(9000));
        }
        other => panic!("expected PaymentDoesNotAddUp, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Bill
// ---------------------------------------------------------------------

#[test]
fn bill_posts_asset_and_expense_lines_to_ap() {
    let ctx = ctx_with_items(vec![
        (
            "foam-sheet",
            tracked_item(
                chart::SALES_INCOME,
                chart::COGS,
                chart::INVENTORY_RAW,
                None,
                0,
            ),
        ),
        (
            "fuel",
            service_item(chart::SALES_INCOME, chart::VEHICLE_FUEL, None),
        ),
    ]);

    let mut material_line = line(1, LineKind::Item, 30000);
    material_line.item_id = Some("foam-sheet".into());

    let mut expense_line = line(2, LineKind::Item, 4000);
    expense_line.item_id = Some("fuel".into());
    expense_line.class = Some(class("cnc"));

    let mut account_line = line(3, LineKind::Account, 1500);
    account_line.account = Some(acct(chart::SHOP_SUPPLIES));
    account_line.class = Some(class("foam"));

    let mut bill = doc(DocKind::Bill);
    bill.contact = Some(vendor("vend-1"));
    bill.lines = vec![material_line, expense_line, account_line];

    let entry = entry_of(post(&bill, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());

    let raw = find(&entry.lines, &acct(chart::INVENTORY_RAW));
    assert_eq!(raw.debit, Money::from_minor(30000));
    assert_eq!(raw.class, None); // balance sheet leg

    let fuel = find(&entry.lines, &acct(chart::VEHICLE_FUEL));
    assert_eq!(fuel.debit, Money::from_minor(4000));
    assert_eq!(fuel.class, Some(class("cnc")));

    let supplies = find(&entry.lines, &acct(chart::SHOP_SUPPLIES));
    assert_eq!(supplies.debit, Money::from_minor(1500));
    assert_eq!(supplies.class, Some(class("foam")));

    let ap = find(&entry.lines, &acct(chart::ACCOUNTS_PAYABLE));
    assert_eq!(ap.credit, Money::from_minor(35500));
    assert_eq!(ap.entity, Some(vendor("vend-1")));

    // No 2200 leg: vendor tax is part of the cost, never recoverable (§1).
    assert!(entry
        .lines
        .iter()
        .all(|l| l.account != acct(chart::SALES_TAX_PAYABLE)));
}

// ---------------------------------------------------------------------
// BillPayment
// ---------------------------------------------------------------------

#[test]
fn bill_payment_debits_ap_for_applications_and_unapplied() {
    let ctx = ctx_with_items(vec![]);
    let mut payment = doc(DocKind::BillPayment);
    payment.contact = Some(vendor("vend-1"));
    payment.pay_from = Some(acct(chart::CHECKING));
    payment.applications = vec![Application {
        target_document_id: "bill-1".into(),
        target_kind: DocKind::Bill,
        amount: Money::from_minor(5000),
    }];
    payment.unapplied = Money::from_minor(1000); // a prepayment

    let entry = entry_of(post(&payment, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let ap = find(&entry.lines, &acct(chart::ACCOUNTS_PAYABLE));
    assert_eq!(ap.debit, Money::from_minor(6000));
    assert_eq!(ap.entity, Some(vendor("vend-1")));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.credit, Money::from_minor(6000));
}

// ---------------------------------------------------------------------
// VendorCredit
// ---------------------------------------------------------------------

#[test]
fn vendor_credit_reduces_material_cost_never_other_income() {
    let ctx = ctx_with_items(vec![(
        "foam-sheet",
        tracked_item(
            chart::SALES_INCOME,
            chart::COGS,
            chart::INVENTORY_RAW,
            None,
            0,
        ),
    )]);
    let mut item_line = line(1, LineKind::Item, 2000);
    item_line.item_id = Some("foam-sheet".into());

    let mut credit = doc(DocKind::VendorCredit);
    credit.contact = Some(vendor("vend-1"));
    credit.lines = vec![item_line];

    let entry = entry_of(post(&credit, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let ap = find(&entry.lines, &acct(chart::ACCOUNTS_PAYABLE));
    assert_eq!(ap.debit, Money::from_minor(2000));
    let material = find(&entry.lines, &acct(chart::INVENTORY_RAW));
    assert_eq!(material.credit, Money::from_minor(2000));
    assert!(entry
        .lines
        .iter()
        .all(|l| l.account != acct(chart::OTHER_INCOME)));
}

// ---------------------------------------------------------------------
// Purchase
// ---------------------------------------------------------------------

#[test]
fn purchase_debits_lines_and_credits_pay_from() {
    let ctx = ctx_with_items(vec![]);
    let mut account_line = line(1, LineKind::Account, 8500);
    account_line.account = Some(acct(chart::SHOP_SUPPLIES));
    account_line.class = Some(class("foam"));

    let mut purchase = doc(DocKind::Purchase);
    purchase.pay_from = Some(acct(chart::CREDIT_CARD));
    purchase.lines = vec![account_line];

    let entry = entry_of(post(&purchase, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let supplies = find(&entry.lines, &acct(chart::SHOP_SUPPLIES));
    assert_eq!(supplies.debit, Money::from_minor(8500));
    let card = find(&entry.lines, &acct(chart::CREDIT_CARD));
    assert_eq!(card.credit, Money::from_minor(8500));
}

#[test]
fn purchase_defaults_to_the_default_bank_account() {
    let ctx = ctx_with_items(vec![]);
    let mut account_line = line(1, LineKind::Account, 100);
    account_line.account = Some(acct(chart::SHOP_SUPPLIES));
    account_line.class = Some(class("foam"));
    let mut purchase = doc(DocKind::Purchase);
    purchase.lines = vec![account_line];

    let entry = entry_of(post(&purchase, 1, &ctx).expect("posts"));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.credit, Money::from_minor(100));
}

// ---------------------------------------------------------------------
// Deposit
// ---------------------------------------------------------------------

#[test]
fn deposit_nets_a_fee_against_the_grouped_payments() {
    let ctx = ctx_with_items(vec![]);
    let mut grouped = line(1, LineKind::Account, 10000);
    grouped.account = Some(acct(chart::UNDEPOSITED_FUNDS));

    let mut fee = line(2, LineKind::Account, -300);
    fee.account = Some(acct(chart::MERCHANT_FEES));
    fee.class = Some(class("foam"));

    let mut deposit = doc(DocKind::Deposit);
    deposit.deposit_to = Some(acct(chart::CHECKING));
    deposit.lines = vec![grouped, fee];

    let entry = entry_of(post(&deposit, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.debit, Money::from_minor(9700));
    let undeposited = find(&entry.lines, &acct(chart::UNDEPOSITED_FUNDS));
    assert_eq!(undeposited.credit, Money::from_minor(10000));
    let merchant_fee = find(&entry.lines, &acct(chart::MERCHANT_FEES));
    assert_eq!(merchant_fee.debit, Money::from_minor(300));
}

// ---------------------------------------------------------------------
// JournalEntry
// ---------------------------------------------------------------------

#[test]
fn journal_entry_posts_each_line_on_its_own_side_and_is_flagged() {
    let ctx = ctx_with_items(vec![]);
    let mut debit_line = line(1, LineKind::Journal, 5000);
    debit_line.account = Some(acct(chart::RENT_UTILITIES));
    debit_line.class = Some(class("foam"));
    debit_line.posting = Some(Side::Debit);

    let mut credit_line = line(2, LineKind::Journal, 5000);
    credit_line.account = Some(acct(chart::CHECKING));
    credit_line.posting = Some(Side::Credit);

    let mut je = doc(DocKind::JournalEntry);
    je.lines = vec![debit_line, credit_line];

    let entry = entry_of(post(&je, 1, &ctx).expect("posts"));
    assert!(entry.is_flagged);
    assert!(entry.is_balanced());
    let rent = find(&entry.lines, &acct(chart::RENT_UTILITIES));
    assert_eq!(rent.debit, Money::from_minor(5000));
    let checking = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(checking.credit, Money::from_minor(5000));
}

// ---------------------------------------------------------------------
// BankLine
// ---------------------------------------------------------------------

#[test]
fn bank_line_money_in_debits_the_bank() {
    let ctx = ctx_with_items(vec![]);
    let mut account_line = line(1, LineKind::Account, 15000);
    account_line.account = Some(acct(chart::OTHER_INCOME));
    account_line.class = Some(class("drop"));

    let mut bank_line = doc(DocKind::BankLine);
    bank_line.deposit_to = Some(acct(chart::CHECKING));
    bank_line.lines = vec![account_line];

    let entry = entry_of(post(&bank_line, 1, &ctx).expect("posts"));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.debit, Money::from_minor(15000));
    let other = find(&entry.lines, &acct(chart::OTHER_INCOME));
    assert_eq!(other.credit, Money::from_minor(15000));
}

#[test]
fn bank_line_money_out_credits_the_bank() {
    let ctx = ctx_with_items(vec![]);
    let mut account_line = line(1, LineKind::Account, -4200);
    account_line.account = Some(acct(chart::SHOP_SUPPLIES));
    account_line.class = Some(class("foam"));

    let mut bank_line = doc(DocKind::BankLine);
    bank_line.pay_from = Some(acct(chart::CHECKING));
    bank_line.lines = vec![account_line];

    let entry = entry_of(post(&bank_line, 1, &ctx).expect("posts"));
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.credit, Money::from_minor(4200));
    let supplies = find(&entry.lines, &acct(chart::SHOP_SUPPLIES));
    assert_eq!(supplies.debit, Money::from_minor(4200));
}

// ---------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------

#[test]
fn settlement_moves_the_batch_to_the_bank_net_of_the_fee() {
    let ctx = ctx_with_items(vec![]);
    let mut gross = line(1, LineKind::Account, 100000);
    gross.account = Some(acct(chart::AUTHNET_CLEARING));
    let mut fee = line(2, LineKind::Account, 295);
    fee.account = Some(acct(chart::MERCHANT_FEES));

    let mut settlement = doc(DocKind::Settlement);
    settlement.deposit_to = Some(acct(chart::CHECKING));
    settlement.lines = vec![gross, fee];

    let entry = entry_of(post(&settlement, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let bank = find(&entry.lines, &acct(chart::CHECKING));
    assert_eq!(bank.debit, Money::from_minor(99705));
    let clearing = find(&entry.lines, &acct(chart::AUTHNET_CLEARING));
    assert_eq!(clearing.credit, Money::from_minor(100000));
    let merchant_fee = find(&entry.lines, &acct(chart::MERCHANT_FEES));
    assert_eq!(merchant_fee.debit, Money::from_minor(295));
}

// ---------------------------------------------------------------------
// Estimate / PurchaseOrder: non posting
// ---------------------------------------------------------------------

#[test]
fn estimate_never_posts() {
    let ctx = ctx_with_items(vec![]);
    let estimate = doc(DocKind::Estimate);
    assert_eq!(
        post(&estimate, 1, &ctx).expect("posts"),
        Posting::NonPosting
    );
}

#[test]
fn purchase_order_never_posts() {
    let ctx = ctx_with_items(vec![]);
    let po = doc(DocKind::PurchaseOrder);
    assert_eq!(post(&po, 1, &ctx).expect("posts"), Posting::NonPosting);
}

// ---------------------------------------------------------------------
// Manufacturing rows (§11): landed cost, builds, inventory adjustments.
// `crate::mfg` puts the amounts on `Account`-kind lines, the same
// convention Settlement uses for its own two accounts.
// ---------------------------------------------------------------------

fn account_line(no: i64, number: &str, amount_minor: i64) -> DocLine {
    let mut docline = line(no, LineKind::Account, amount_minor);
    docline.account = Some(acct(number));
    docline
}

#[test]
fn raw_material_receipt_posts_1300_and_2050() {
    let ctx = ctx_with_items(vec![]);
    let mut receipt = doc(DocKind::RawMaterialReceipt);
    receipt.lines = vec![
        account_line(1, chart::INVENTORY_RAW, 47760),
        account_line(2, chart::INVENTORY_RECEIVED_NOT_BILLED, 47760),
    ];
    let entry = entry_of(post(&receipt, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let raw = find(&entry.lines, &acct(chart::INVENTORY_RAW));
    assert_eq!(raw.debit, Money::from_minor(47760));
    let payable = find(&entry.lines, &acct(chart::INVENTORY_RECEIVED_NOT_BILLED));
    assert_eq!(payable.credit, Money::from_minor(47760));
}

#[test]
fn a_build_that_ran_under_standard_credits_variance() {
    let ctx = ctx_with_items(vec![]);
    let mut build = doc(DocKind::Build);
    build.header_class = Some(class("foam"));
    build.lines = vec![
        account_line(1, chart::INVENTORY_FINISHED, 2000),
        account_line(2, chart::INVENTORY_RAW, 1300),
        account_line(3, chart::PARTS_LABOUR_APPLIED, 620),
    ];
    let entry = entry_of(post(&build, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let variance = find(&entry.lines, &acct(chart::MANUFACTURING_VARIANCE));
    assert_eq!(variance.credit, Money::from_minor(80));
    assert_eq!(variance.class, Some(class("foam")));
}

#[test]
fn a_build_that_overran_debits_variance() {
    let ctx = ctx_with_items(vec![]);
    let mut build = doc(DocKind::Build);
    build.header_class = Some(class("foam"));
    build.lines = vec![
        account_line(1, chart::INVENTORY_FINISHED, 2000),
        account_line(2, chart::INVENTORY_RAW, 1700),
        account_line(3, chart::PARTS_LABOUR_APPLIED, 620),
    ];
    let entry = entry_of(post(&build, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    let variance = find(&entry.lines, &acct(chart::MANUFACTURING_VARIANCE));
    assert_eq!(variance.debit, Money::from_minor(320));
}

#[test]
fn a_build_with_no_class_is_rejected() {
    let ctx = ctx_with_items(vec![]);
    let mut build = doc(DocKind::Build);
    build.lines = vec![
        account_line(1, chart::INVENTORY_FINISHED, 2000),
        account_line(2, chart::INVENTORY_RAW, 1300),
        account_line(3, chart::PARTS_LABOUR_APPLIED, 620),
    ];
    assert_eq!(
        post(&build, 1, &ctx),
        Err(PostError::MissingClass { line_no: 0 })
    );
}

#[test]
fn inventory_adjustment_count_up_and_down() {
    let ctx = ctx_with_items(vec![]);

    let mut up = doc(DocKind::InventoryAdjustment);
    up.header_class = Some(class("foam"));
    up.lines = vec![account_line(1, chart::INVENTORY_RAW, 500)];
    let entry = entry_of(post(&up, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    assert_eq!(
        find(&entry.lines, &acct(chart::INVENTORY_RAW)).debit,
        Money::from_minor(500)
    );
    assert_eq!(
        find(&entry.lines, &acct(chart::INVENTORY_ADJUSTMENT)).credit,
        Money::from_minor(500)
    );

    let mut down = doc(DocKind::InventoryAdjustment);
    down.header_class = Some(class("foam"));
    down.lines = vec![account_line(1, chart::INVENTORY_FINISHED, -500)];
    let entry = entry_of(post(&down, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    assert_eq!(
        find(&entry.lines, &acct(chart::INVENTORY_ADJUSTMENT)).debit,
        Money::from_minor(500)
    );
    assert_eq!(
        find(&entry.lines, &acct(chart::INVENTORY_FINISHED)).credit,
        Money::from_minor(500)
    );
}

// ---------------------------------------------------------------------
// Void
// ---------------------------------------------------------------------

#[test]
fn a_voided_invoice_posts_the_reversal() {
    let ctx = ctx_with_items(vec![]);
    let mut item_line = line(1, LineKind::Item, 5000);
    item_line.class = Some(class("foam"));
    let mut invoice = doc(DocKind::Invoice);
    invoice.contact = Some(customer("cust-1"));
    invoice.lines = vec![item_line];
    invoice.is_voided = true;
    invoice.memo = Some("original sale".into());

    let entry = entry_of(post(&invoice, 1, &ctx).expect("posts"));
    assert!(entry.is_balanced());
    assert_eq!(entry.memo, Some("VOID: original sale".to_string()));
    let ar = find(&entry.lines, &acct(chart::ACCOUNTS_RECEIVABLE));
    assert_eq!(ar.credit, Money::from_minor(5000));
    assert_eq!(ar.debit, Money::ZERO);
    let sales = find(&entry.lines, &acct(chart::SALES_INCOME));
    assert_eq!(sales.debit, Money::from_minor(5000));
    assert_eq!(sales.credit, Money::ZERO);
}

// ---------------------------------------------------------------------
// Unknown item
// ---------------------------------------------------------------------

#[test]
fn an_unknown_item_is_rejected() {
    let ctx = ctx_with_items(vec![]);
    let mut item_line = line(1, LineKind::Item, 100);
    item_line.item_id = Some("ghost".into());
    item_line.class = Some(class("foam"));
    let mut invoice = doc(DocKind::Invoice);
    invoice.lines = vec![item_line];

    assert_eq!(
        post(&invoice, 1, &ctx),
        Err(PostError::UnknownItem {
            item_id: "ghost".into()
        })
    );
}

// ---------------------------------------------------------------------
// Proptest: any invoice balances, and AR = subtotal - discounts + tax
// ---------------------------------------------------------------------

fn arb_amount_minor() -> impl Strategy<Value = i64> {
    1i64..50_000
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn random_invoices_balance_and_ar_matches_the_formula(
        line_amounts in proptest::collection::vec(arb_amount_minor(), 1..=8),
        has_tax in any::<bool>(),
    ) {
        let ctx = ctx_with_items(vec![]);
        let mut lines = Vec::new();
        let mut subtotal = Money::ZERO;
        let mut discounts = Money::ZERO;

        for (i, amount) in line_amounts.iter().enumerate() {
            let no = (i + 1) as i64;
            // Every third line is a discount against the running subtotal;
            // every other line is a plain item sale.
            if no % 3 == 0 {
                // Capped by what is left of the running subtotal, not just
                // this line's own amount: two discounts each capped only
                // against their own moment in time can still sum to more
                // than the final subtotal, which would make the invoice
                // itself invalid data (an AR debit cannot go negative) not
                // a bug in `post`, so the generator never produces one.
                let remaining = subtotal.checked_sub(discounts).unwrap().minor().max(0);
                let discount_amount = (*amount).min(remaining);
                if discount_amount == 0 {
                    continue;
                }
                let mut discount_line = line(no, LineKind::Discount, discount_amount);
                discount_line.class = Some(class("foam"));
                lines.push(discount_line);
                discounts = discounts.checked_add(Money::from_minor(discount_amount)).unwrap();
            } else {
                let mut item_line = line(no, LineKind::Item, *amount);
                item_line.class = Some(class("foam"));
                lines.push(item_line);
                subtotal = subtotal.checked_add(Money::from_minor(*amount)).unwrap();
            }
        }

        let tax_amount = if has_tax {
            let rate = dec!(0.06625);
            let taxable = Decimal::new(subtotal.minor(), 2);
            ledger_core::round_money(taxable * rate, ledger_core::RoundingPolicy::TaxCalculation).unwrap()
        } else {
            Money::ZERO
        };

        let mut invoice = doc(DocKind::Invoice);
        invoice.contact = Some(customer("cust-1"));
        invoice.lines = lines;
        invoice.tax = if has_tax {
            Some(TaxDetail { total_tax: tax_amount, taxable_base: subtotal, rate: dec!(0.06625) })
        } else {
            None
        };

        let entry = entry_of(post(&invoice, 1, &ctx).expect("posts"));
        prop_assert!(entry.is_balanced());

        // A fully-discounted invoice has nothing left to debit AR for, and
        // `finalize` drops a zero-amount leg rather than posting one (§4:
        // `debit + credit > 0`), so no AR line at all is the correct
        // answer in that case.
        let ar_debit = entry
            .lines
            .iter()
            .find(|entry_line| entry_line.account == acct(chart::ACCOUNTS_RECEIVABLE))
            .map(|entry_line| entry_line.debit)
            .unwrap_or(Money::ZERO);
        let expected = subtotal
            .checked_sub(discounts)
            .unwrap()
            .checked_add(tax_amount)
            .unwrap();
        prop_assert_eq!(ar_debit, expected);
    }
}
