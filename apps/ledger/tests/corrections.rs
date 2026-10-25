//! `LEDGER-DESIGN.md` §4/§6 on the own book: editing a saved document,
//! voiding one, and applying a credit memo to an invoice. A correction is a
//! new version plus a reversal, never an overwrite; nothing in a closed
//! period moves; and a refusal part way leaves the book as it was.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use ledger::books::{self, Book, BooksError, Contact, Item, ItemType};
use ledger::chart;
use ledger::post::{self, Posting};
use ledger::report;
use ledger::store::{CommandMeta, Ledger, LedgerError};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, DocKind, DocLine, LedgerDocument, LineKind,
    PostingConfig,
};
use ledger_core::Money;

const CO: &str = "aquamentor";

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

fn usd(minor: i64) -> Money {
    Money::from_minor(minor)
}

fn command() -> CommandMeta {
    CommandMeta {
        actor_id: "dan".to_string(),
        kind: "test".to_string(),
        hlc: now().to_rfc3339(),
    }
}

fn customer(id: &str, name: &str) -> Contact {
    Contact {
        contact_id: id.to_string(),
        kind: ContactKind::Customer,
        display_name: name.to_string(),
        company_name: None,
        email: None,
        phone: None,
        bill_address: None,
        ship_address: None,
        terms_days: Some(30),
        due_day: None,
        terms_source_ref: None,
        is_tax_exempt: false,
        default_class: Some(ClassId("foam".to_string())),
        is_active: true,
        source_ref: None,
    }
}

fn book() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(CO, "Aquamentor", None, now())
        .unwrap();
    books::upsert_contact(&ledger, CO, &customer("c-pool", "Pool Co"), now()).unwrap();
    books::upsert_contact(&ledger, CO, &customer("c-city", "City Parks"), now()).unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &Contact {
            kind: ContactKind::Vendor,
            default_class: None,
            ..customer("v-foam", "Foam Supplier")
        },
        now(),
    )
    .unwrap();
    books::upsert_item(
        &ledger,
        CO,
        &Item {
            item_id: "mat".to_string(),
            sku: Some("MAT".to_string()),
            name: "Swim mat".to_string(),
            description: None,
            item_type: ItemType::NonInventory,
            income_account: AccountId(chart::SALES_INCOME.to_string()),
            expense_account: AccountId(chart::SHOP_SUPPLIES.to_string()),
            asset_account: None,
            default_class: Some(ClassId("foam".to_string())),
            sales_price: Some(usd(2500)),
            purchase_cost: None,
            unit_cost: None,
            is_taxable: false,
            is_active: true,
            source_ref: None,
        },
        now(),
    )
    .unwrap();
    ledger
}

fn mat_line(minor: i64) -> DocLine {
    DocLine {
        line_no: 1,
        kind: LineKind::Item,
        amount: usd(minor),
        class: None,
        item_id: Some("mat".to_string()),
        account: None,
        is_taxable: false,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

fn doc(id: &str, kind: DocKind, on: NaiveDate, contact: &str) -> LedgerDocument {
    let contact_kind = if contact.starts_with("v-") {
        ContactKind::Vendor
    } else {
        ContactKind::Customer
    };
    LedgerDocument {
        document_id: id.to_string(),
        kind,
        number: None,
        txn_date: on,
        due_date: None,
        contact: Some(books::contact_ref(contact_kind, contact)),
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

fn invoice(id: &str, on: NaiveDate, contact: &str, minor: i64) -> LedgerDocument {
    LedgerDocument {
        lines: vec![mat_line(minor)],
        ..doc(id, DocKind::Invoice, on, contact)
    }
}

/// Prepare, save and post: the standalone entry path end to end.
fn enter(ledger: &Ledger, document: LedgerDocument) -> LedgerDocument {
    let (document, context) =
        books::prepare_document(ledger, CO, document, PostingConfig::default()).unwrap();
    let version = ledger
        .save_document(CO, &document, command(), now())
        .unwrap();
    if let Posting::Entry(entry) = post::post(&document, version.version, &context).unwrap() {
        ledger.post_entry(CO, &entry, now()).unwrap();
    }
    document
}

fn edit(ledger: &Ledger, document: LedgerDocument) -> Result<books::Revision, BooksError> {
    books::update_document(
        ledger,
        CO,
        document,
        PostingConfig::default(),
        command(),
        now(),
    )
}

fn ar(ledger: &Ledger) -> Money {
    report::trial_balance(ledger, CO, ymd(2026, 12, 31))
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.account_id.0 == chart::ACCOUNTS_RECEIVABLE)
        .map_or(Money::ZERO, |row| row.balance)
}

fn open(ledger: &Ledger) -> Vec<(String, i64)> {
    books::open_documents(ledger, CO, Book::Receivable, ymd(2026, 12, 31))
        .unwrap()
        .into_iter()
        .map(|d| (d.document_id, d.open.minor()))
        .collect()
}

/// Everything about a document the book holds: versions and entries.
fn footprint(ledger: &Ledger, id: &str) -> (i64, usize) {
    (
        ledger.document_version_count(CO, id).unwrap(),
        ledger.entries_for_document(CO, id).unwrap().len(),
    )
}

#[test]
fn an_edit_saves_a_version_reverses_the_old_entry_and_posts_the_new() {
    let ledger = book();
    let saved = enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    let number = saved.number.clone().unwrap();

    let mut changed = invoice("inv-1", ymd(2026, 9, 3), "c-pool", 15000);
    changed.number = None; // left blank: the number is kept
    let revision = edit(&ledger, changed).unwrap();

    assert_eq!(revision.version, 2);
    assert_eq!(revision.document.number.as_deref(), Some(number.as_str()));
    assert_eq!(revision.reversals.len(), 1);
    let (_, entry) = revision.entry.as_ref().unwrap();
    assert_eq!(entry.source_version, 2);
    assert_eq!(entry.entry_date, ymd(2026, 9, 3));

    let entries = ledger.entries_for_document(CO, "inv-1").unwrap();
    assert_eq!(entries.len(), 3);
    let reversal = entries
        .iter()
        .find(|(id, _, _)| *id == revision.reversals[0])
        .unwrap();
    // Dated as the entry it reverses, so nothing is left at the old figure.
    assert_eq!(reversal.1.entry_date, ymd(2026, 9, 1));
    assert!(reversal.1.reversal_of.is_some());

    assert_eq!(ar(&ledger), usd(15000));
    assert_eq!(open(&ledger), [("inv-1".to_string(), 15000)]);
    // Every version stays.
    assert_eq!(ledger.document_version_count(CO, "inv-1").unwrap(), 2);

    // A second edit reverses only the live entry, not the first one again.
    let revision = edit(&ledger, invoice("inv-1", ymd(2026, 9, 3), "c-pool", 12000)).unwrap();
    assert_eq!(revision.reversals.len(), 1);
    assert_eq!(ar(&ledger), usd(12000));
}

#[test]
fn an_edit_that_posts_the_same_leaves_the_entry_standing() {
    let ledger = book();
    let saved = enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    let mut same = saved.clone();
    same.due_date = Some(ymd(2026, 12, 1));
    let revision = edit(&ledger, same).unwrap();
    assert_eq!(revision.version, 2);
    assert!(revision.reversals.is_empty());
    assert!(revision.entry.is_none());
    assert_eq!(footprint(&ledger, "inv-1"), (2, 1));
    assert_eq!(
        books::latest_document(&ledger, CO, "inv-1")
            .unwrap()
            .due_date,
        Some(ymd(2026, 12, 1))
    );
}

#[test]
fn an_edit_may_not_change_kind_contact_type_or_number() {
    let ledger = book();
    let saved = enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));

    let mut as_receipt = saved.clone();
    as_receipt.kind = DocKind::SalesReceipt;
    assert!(matches!(
        edit(&ledger, as_receipt),
        Err(BooksError::KindChanged { .. })
    ));

    let mut to_vendor = saved.clone();
    to_vendor.contact = Some(books::contact_ref(ContactKind::Vendor, "v-foam"));
    assert!(matches!(
        edit(&ledger, to_vendor),
        Err(BooksError::ContactTypeChanged { .. })
    ));

    let mut renumbered = saved.clone();
    renumbered.number = Some("99999".to_string());
    assert!(matches!(
        edit(&ledger, renumbered),
        Err(BooksError::NumberChanged { .. })
    ));

    let mut voiding = saved.clone();
    voiding.is_voided = true;
    assert!(matches!(
        edit(&ledger, voiding),
        Err(BooksError::VoidThroughEdit)
    ));

    assert!(matches!(
        edit(
            &ledger,
            invoice("never-saved", ymd(2026, 9, 1), "c-pool", 1)
        ),
        Err(BooksError::UnknownDocument(_))
    ));

    // Another customer is fine: same contact type.
    let mut other_customer = saved;
    other_customer.contact = Some(books::contact_ref(ContactKind::Customer, "c-city"));
    edit(&ledger, other_customer).unwrap();
    assert_eq!(footprint(&ledger, "inv-1"), (2, 3));
}

#[test]
fn nothing_in_a_closed_period_moves() {
    let ledger = book();
    let saved = enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    let later = enter(&ledger, invoice("inv-2", ymd(2026, 10, 5), "c-pool", 5000));
    ledger
        .close_period(CO, ymd(2026, 9, 30), "dan", "September", now())
        .unwrap();

    // The old date is closed.
    let mut moved_out = saved.clone();
    moved_out.txn_date = ymd(2026, 10, 2);
    assert!(matches!(
        edit(&ledger, moved_out),
        Err(BooksError::Ledger(LedgerError::PeriodClosed { .. }))
    ));
    // The new date is closed.
    let mut moved_in = later.clone();
    moved_in.txn_date = ymd(2026, 9, 20);
    assert!(matches!(
        edit(&ledger, moved_in),
        Err(BooksError::Ledger(LedgerError::PeriodClosed { .. }))
    ));
    // Voiding one in the closed period.
    assert!(matches!(
        books::void_document(&ledger, CO, "inv-1", "duplicate", command(), now()),
        Err(BooksError::Ledger(LedgerError::PeriodClosed { .. }))
    ));

    assert_eq!(footprint(&ledger, "inv-1"), (1, 1));
    assert_eq!(footprint(&ledger, "inv-2"), (1, 1));
    assert_eq!(ar(&ledger), usd(15000));
}

#[test]
fn a_refusal_part_way_leaves_the_book_as_it_was() {
    let ledger = book();
    enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    // The reversal posts, then the new entry names a class the book does not
    // have: the store refuses it, and the reversal and version go with it.
    let mut bad = invoice("inv-1", ymd(2026, 9, 1), "c-pool", 20000);
    bad.lines[0].class = Some(ClassId("no-such-class".to_string()));
    let err = edit(&ledger, bad).unwrap_err();
    assert!(
        matches!(err, BooksError::Ledger(LedgerError::UnknownClass(_))),
        "{err:?}"
    );
    assert_eq!(footprint(&ledger, "inv-1"), (1, 1));
    assert_eq!(ar(&ledger), usd(10000));

    // And the connection is usable afterwards.
    edit(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 20000)).unwrap();
    assert_eq!(ar(&ledger), usd(20000));
}

#[test]
fn an_edit_may_not_cut_a_total_below_what_is_applied_or_overapply_a_payment() {
    let ledger = book();
    enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    let mut payment = doc("pay-1", DocKind::Payment, ymd(2026, 9, 10), "c-pool");
    payment.applications = vec![Application {
        target_document_id: "inv-1".to_string(),
        target_kind: DocKind::Invoice,
        amount: usd(8000),
    }];
    let payment = enter(&ledger, payment);

    assert!(matches!(
        edit(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 5000)),
        Err(BooksError::Overapplied { .. })
    ));
    let mut greedy = payment.clone();
    greedy.applications[0].amount = usd(12000);
    assert!(matches!(
        edit(&ledger, greedy),
        Err(BooksError::Overapplied { .. })
    ));

    // Paying it off exactly is fine, and reposts the payment.
    let mut exact = payment;
    exact.applications[0].amount = usd(10000);
    let revision = edit(&ledger, exact).unwrap();
    assert_eq!(revision.reversals.len(), 1);
    assert!(open(&ledger).is_empty());
    assert_eq!(ar(&ledger), Money::ZERO);
}

#[test]
fn a_void_reverses_the_entry_and_the_document_leaves_the_aging() {
    let ledger = book();
    enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    let mut payment = doc("pay-1", DocKind::Payment, ymd(2026, 9, 10), "c-pool");
    payment.applications = vec![Application {
        target_document_id: "inv-1".to_string(),
        target_kind: DocKind::Invoice,
        amount: usd(4000),
    }];
    enter(&ledger, payment);

    // A payment is still applied to it.
    assert!(matches!(
        books::void_document(&ledger, CO, "inv-1", "duplicate", command(), now()),
        Err(BooksError::HasApplications { .. })
    ));

    // Void the payment: it releases what it applied.
    let revision = books::void_document(&ledger, CO, "pay-1", "bounced", command(), now()).unwrap();
    assert!(revision.document.is_voided);
    assert_eq!(revision.reversals.len(), 1);
    assert!(revision.entry.is_none());
    assert_eq!(open(&ledger), [("inv-1".to_string(), 10000)]);

    let revision =
        books::void_document(&ledger, CO, "inv-1", "duplicate", command(), now()).unwrap();
    assert_eq!(revision.version, 2);
    assert_eq!(revision.document.memo.as_deref(), Some("Voided: duplicate"));
    let entries = ledger.entries_for_document(CO, "inv-1").unwrap();
    let (_, reversal, _) = entries
        .iter()
        .find(|(id, _, _)| *id == revision.reversals[0])
        .unwrap();
    assert!(reversal.memo.as_deref().unwrap().starts_with("VOID: "));
    assert!(reversal.memo.as_deref().unwrap().ends_with("(duplicate)"));
    assert!(open(&ledger).is_empty());
    assert_eq!(ar(&ledger), Money::ZERO);

    // Voided stays voided.
    assert!(matches!(
        books::void_document(&ledger, CO, "inv-1", "again", command(), now()),
        Err(BooksError::Voided(_))
    ));
    assert!(matches!(
        edit(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 1)),
        Err(BooksError::Voided(_))
    ));
}

#[test]
fn a_credit_memo_applies_to_an_open_invoice_and_both_net_in_the_aging() {
    let ledger = book();
    enter(&ledger, invoice("inv-1", ymd(2026, 9, 1), "c-pool", 10000));
    enter(
        &ledger,
        invoice("inv-city", ymd(2026, 9, 1), "c-city", 10000),
    );
    let credit = LedgerDocument {
        lines: vec![mat_line(4000)],
        ..doc("cm-1", DocKind::CreditMemo, ymd(2026, 9, 5), "c-pool")
    };
    enter(&ledger, credit);
    assert_eq!(ar(&ledger), usd(16000));

    // Another customer's invoice, or not an invoice at all.
    assert!(matches!(
        books::apply_credit(&ledger, CO, "cm-1", "inv-city", None, command(), now()),
        Err(BooksError::BadApplication(_))
    ));
    assert!(matches!(
        books::apply_credit(&ledger, CO, "inv-1", "inv-1", None, command(), now()),
        Err(BooksError::BadApplication(_))
    ));
    // More than the credit has open.
    assert!(matches!(
        books::apply_credit(
            &ledger,
            CO,
            "cm-1",
            "inv-1",
            Some(usd(5000)),
            command(),
            now()
        ),
        Err(BooksError::BadApplication(_))
    ));

    let revision = books::apply_credit(
        &ledger,
        CO,
        "cm-1",
        "inv-1",
        Some(usd(1500)),
        command(),
        now(),
    )
    .unwrap();
    assert_eq!(revision.version, 2);
    assert!(revision.reversals.is_empty() && revision.entry.is_none());
    // The rest, by default.
    let revision =
        books::apply_credit(&ledger, CO, "cm-1", "inv-1", None, command(), now()).unwrap();
    assert_eq!(revision.document.applications.len(), 1);
    assert_eq!(revision.document.applications[0].amount, usd(4000));

    assert_eq!(
        open(&ledger),
        [("inv-1".to_string(), 6000), ("inv-city".to_string(), 10000)]
    );
    // The credit posted once and still stands; AR ties to the aging.
    assert_eq!(footprint(&ledger, "cm-1"), (3, 1));
    let aging = books::aging(&ledger, CO, Book::Receivable, ymd(2026, 12, 31)).unwrap();
    assert_eq!(aging.totals.total, ar(&ledger));
    assert_eq!(aging.totals.total, usd(16000));

    // Used up.
    assert!(matches!(
        books::apply_credit(&ledger, CO, "cm-1", "inv-1", None, command(), now()),
        Err(BooksError::BadApplication(_))
    ));
}
