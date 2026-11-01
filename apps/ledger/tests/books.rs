//! D28: master data, numbering, the standalone entry path, open documents
//! and aging — and that the aging ties to the ledger's own AR and AP.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;

use ledger::books::{self, Book, Contact, DueDay, Item, ItemType, Terms};
use ledger::chart;
use ledger::post::{self, Posting};
use ledger::report;
use ledger::store::{CommandMeta, Ledger};
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

fn acct(id: &str) -> AccountId {
    AccountId(id.to_string())
}

fn customer(id: &str, name: &str, terms: Option<i64>, exempt: bool) -> Contact {
    Contact {
        contact_id: id.to_string(),
        kind: ContactKind::Customer,
        display_name: name.to_string(),
        company_name: None,
        email: Some(format!("{id}@example.test")),
        phone: None,
        bill_address: None,
        ship_address: None,
        terms_days: terms,
        due_day: None,
        terms_source_ref: None,
        is_tax_exempt: exempt,
        default_class: Some(ClassId("foam".to_string())),
        is_active: true,
        source_ref: None,
    }
}

fn vendor(id: &str, name: &str, terms: Option<i64>) -> Contact {
    Contact {
        kind: ContactKind::Vendor,
        default_class: None,
        ..customer(id, name, terms, false)
    }
}

fn service_item(id: &str, name: &str) -> Item {
    Item {
        item_id: id.to_string(),
        sku: Some(id.to_uppercase()),
        name: name.to_string(),
        description: None,
        item_type: ItemType::NonInventory,
        income_account: acct(chart::SALES_INCOME),
        expense_account: acct(chart::SHOP_SUPPLIES),
        asset_account: None,
        default_class: Some(ClassId("foam".to_string())),
        sales_price: Some(usd(2500)),
        purchase_cost: None,
        unit_cost: None,
        is_taxable: true,
        is_active: true,
        source_ref: None,
    }
}

fn line(no: i64, kind: LineKind, item: Option<&str>, minor: i64, taxable: bool) -> DocLine {
    DocLine {
        line_no: no,
        kind,
        amount: usd(minor),
        class: None,
        item_id: item.map(str::to_string),
        account: None,
        is_taxable: taxable,
        qty: None,
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

fn doc(
    id: &str,
    kind: DocKind,
    on: NaiveDate,
    contact: Option<(ContactKind, &str)>,
) -> LedgerDocument {
    LedgerDocument {
        document_id: id.to_string(),
        kind,
        number: None,
        txn_date: on,
        due_date: None,
        contact: contact.map(|(k, c)| books::contact_ref(k, c)),
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

fn apply(target: &str, kind: DocKind, minor: i64) -> Application {
    Application {
        target_document_id: target.to_string(),
        target_kind: kind,
        amount: usd(minor),
    }
}

fn book() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(CO, "Aquamentor", None, now())
        .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &customer("c-pool", "Pool Co", Some(30), false),
        now(),
    )
    .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &customer("c-city", "City Parks", Some(0), true),
        now(),
    )
    .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &vendor("v-foam", "Foam Supplier", Some(15)),
        now(),
    )
    .unwrap();
    books::upsert_item(&ledger, CO, &service_item("mat", "Swim mat"), now()).unwrap();
    books::upsert_item(&ledger, CO, &service_item("ship", "Freight"), now()).unwrap();
    ledger
}

/// Prepare, save and post: the standalone entry path end to end.
fn enter(ledger: &Ledger, document: LedgerDocument) -> LedgerDocument {
    let (document, context) =
        books::prepare_document(ledger, CO, document, PostingConfig::default()).unwrap();
    let version = ledger
        .save_document(
            CO,
            &document,
            CommandMeta {
                actor_id: "dan".to_string(),
                kind: "test".to_string(),
                hlc: now().to_rfc3339(),
            },
            now(),
        )
        .unwrap();
    if let Posting::Entry(entry) = post::post(&document, version.version, &context).unwrap() {
        ledger.post_entry(CO, &entry, now()).unwrap();
    }
    document
}

fn balance(ledger: &Ledger, account: &str, as_of: NaiveDate) -> Money {
    report::trial_balance(ledger, CO, as_of)
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.account_id.0 == account)
        .map_or(Money::ZERO, |row| row.balance)
}

#[test]
fn contacts_and_items_round_trip_and_search() {
    let ledger = book();
    let pool = books::contact(&ledger, CO, "c-pool").unwrap().unwrap();
    assert_eq!(pool.terms_days, Some(30));
    assert_eq!(pool.kind, ContactKind::Customer);

    let customers =
        books::list_contacts(&ledger, CO, Some(ContactKind::Customer), None, false).unwrap();
    let names: Vec<_> = customers.iter().map(|c| c.display_name.as_str()).collect();
    assert_eq!(names, ["City Parks", "Pool Co"]);
    let found = books::list_contacts(&ledger, CO, None, Some("SUPPLIER"), false).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, ContactKind::Vendor);

    let mut inactive = customer("c-pool", "Pool Co", Some(45), false);
    inactive.is_active = false;
    books::upsert_contact(&ledger, CO, &inactive, now()).unwrap();
    assert_eq!(
        books::list_contacts(&ledger, CO, Some(ContactKind::Customer), None, false)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        books::list_contacts(&ledger, CO, Some(ContactKind::Customer), None, true)
            .unwrap()
            .len(),
        2
    );

    let items = books::list_items(&ledger, CO, Some("mat"), false).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].sales_price, Some(usd(2500)));
}

#[test]
fn a_customer_name_is_unique_per_kind() {
    let ledger = book();
    let clash = customer("c-other", "Pool Co", None, false);
    assert!(books::upsert_contact(&ledger, CO, &clash, now()).is_err());
    // A vendor may share a customer's name, as in QBO.
    books::upsert_contact(&ledger, CO, &vendor("v-pool", "Pool Co", None), now()).unwrap();
}

#[test]
fn an_inventory_item_needs_an_asset_account_and_only_it() {
    let ledger = book();
    let mut item = service_item("tube", "Rescue tube");
    item.item_type = ItemType::Inventory;
    assert!(books::upsert_item(&ledger, CO, &item, now()).is_err());
    item.asset_account = Some(acct(chart::INVENTORY_FINISHED));
    books::upsert_item(&ledger, CO, &item, now()).unwrap();
    let mut service = service_item("labour", "Labour");
    service.asset_account = Some(acct(chart::INVENTORY_FINISHED));
    assert!(books::upsert_item(&ledger, CO, &service, now()).is_err());
}

#[test]
fn an_item_naming_an_unknown_account_is_refused() {
    let ledger = book();
    let mut item = service_item("odd", "Odd");
    item.income_account = acct("4999-not-there");
    assert!(books::upsert_item(&ledger, CO, &item, now()).is_err());
}

#[test]
fn prepare_fills_number_due_date_class_and_tax() {
    let ledger = book();
    let mut invoice = doc(
        "inv-a",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        Some((ContactKind::Customer, "c-pool")),
    );
    invoice.lines = vec![
        line(1, LineKind::Item, Some("mat"), 10000, true),
        line(2, LineKind::Shipping, Some("ship"), 1550, false),
    ];
    let (prepared, context) =
        books::prepare_document(&ledger, CO, invoice, PostingConfig::default()).unwrap();
    assert_eq!(prepared.number.as_deref(), Some("1001"));
    assert_eq!(prepared.due_date, Some(ymd(2026, 10, 1)));
    assert_eq!(prepared.header_class, Some(ClassId("foam".to_string())));
    let tax = prepared.tax.clone().unwrap();
    assert_eq!(tax.taxable_base, usd(10000));
    assert_eq!(tax.total_tax, usd(663)); // 6.625 rounds half up, as QBO does (W17): 6.63
    assert_eq!(tax.rate, Decimal::new(6625, 5));
    assert!(context.items.contains_key("mat"));
    assert!(!context.customers["c-pool"].is_tax_exempt);
    assert_eq!(
        books::document_total(&prepared).unwrap(),
        usd(10000 + 1550 + 663)
    );
}

#[test]
fn an_exempt_customer_is_charged_no_tax() {
    let ledger = book();
    let mut invoice = doc(
        "inv-b",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        Some((ContactKind::Customer, "c-city")),
    );
    invoice.lines = vec![line(1, LineKind::Item, Some("mat"), 10000, true)];
    let (prepared, _) =
        books::prepare_document(&ledger, CO, invoice, PostingConfig::default()).unwrap();
    assert_eq!(prepared.tax.unwrap().total_tax, Money::ZERO);
    assert_eq!(prepared.due_date, Some(ymd(2026, 9, 1)));
}

#[test]
fn prepare_refuses_unknown_wrong_kind_and_inactive_references() {
    let ledger = book();
    let mut invoice = doc(
        "inv-c",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        Some((ContactKind::Customer, "nobody")),
    );
    invoice.lines = vec![line(1, LineKind::Item, Some("mat"), 100, false)];
    let err = books::prepare_document(&ledger, CO, invoice.clone(), PostingConfig::default())
        .unwrap_err();
    assert!(err.to_string().contains("unknown customer nobody"), "{err}");

    invoice.contact = Some(books::contact_ref(ContactKind::Vendor, "v-foam"));
    let err = books::prepare_document(&ledger, CO, invoice.clone(), PostingConfig::default())
        .unwrap_err();
    assert!(err.to_string().contains("needs a customer"), "{err}");

    invoice.contact = Some(books::contact_ref(ContactKind::Customer, "c-pool"));
    invoice.lines[0].item_id = Some("ghost".to_string());
    let err = books::prepare_document(&ledger, CO, invoice.clone(), PostingConfig::default())
        .unwrap_err();
    assert!(err.to_string().contains("unknown item ghost"), "{err}");

    invoice.contact = None;
    assert!(books::prepare_document(&ledger, CO, invoice, PostingConfig::default()).is_err());
}

#[test]
fn numbers_are_sequential_never_reused_and_duplicates_are_refused() {
    let ledger = book();
    let mut first = doc(
        "inv-1",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        Some((ContactKind::Customer, "c-pool")),
    );
    first.lines = vec![line(1, LineKind::Item, Some("mat"), 100, false)];
    let first = enter(&ledger, first);
    assert_eq!(first.number.as_deref(), Some("1001"));

    // Someone types 1002 by hand; the sequence steps over it.
    let mut typed = doc(
        "inv-typed",
        DocKind::Invoice,
        ymd(2026, 9, 2),
        Some((ContactKind::Customer, "c-pool")),
    );
    typed.number = Some("1002".to_string());
    typed.lines = first.lines.clone();
    enter(&ledger, typed);

    let mut third = doc(
        "inv-3",
        DocKind::Invoice,
        ymd(2026, 9, 3),
        Some((ContactKind::Customer, "c-pool")),
    );
    third.lines = first.lines.clone();
    assert_eq!(enter(&ledger, third).number.as_deref(), Some("1003"));

    // Estimates number on their own sequence.
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::Estimate).unwrap(),
        "1001"
    );

    let mut dup = doc(
        "inv-dup",
        DocKind::Invoice,
        ymd(2026, 9, 4),
        Some((ContactKind::Customer, "c-pool")),
    );
    dup.number = Some("1001".to_string());
    dup.lines = first.lines.clone();
    let err = books::prepare_document(&ledger, CO, dup, PostingConfig::default()).unwrap_err();
    assert!(
        err.to_string().contains("already used by document inv-1"),
        "{err}"
    );

    // Re-saving the same document under its own number is an edit, not a duplicate.
    books::prepare_document(&ledger, CO, first, PostingConfig::default()).unwrap();
}

#[test]
fn the_sequence_seeds_past_the_highest_imported_number() {
    let ledger = book();
    let mut imported = doc(
        "inv-q",
        DocKind::Invoice,
        ymd(2026, 1, 5),
        Some((ContactKind::Customer, "c-pool")),
    );
    imported.number = Some("21234".to_string());
    imported.lines = vec![line(1, LineKind::Item, Some("mat"), 100, false)];
    enter(&ledger, imported);
    let mut odd = doc(
        "inv-odd",
        DocKind::Invoice,
        ymd(2026, 1, 6),
        Some((ContactKind::Customer, "c-pool")),
    );
    odd.number = Some("99999-A".to_string());
    odd.lines = vec![line(1, LineKind::Item, Some("mat"), 100, false)];
    enter(&ledger, odd);
    assert_eq!(
        books::next_number(&ledger, CO, DocKind::Invoice).unwrap(),
        "21235"
    );
}

#[test]
fn ar_aging_buckets_by_days_past_due_and_ties_to_1200() {
    let ledger = book();
    let as_of = ymd(2026, 10, 15);
    let make = |id: &str, on: NaiveDate, minor: i64| {
        let mut invoice = doc(
            id,
            DocKind::Invoice,
            on,
            Some((ContactKind::Customer, "c-pool")),
        );
        invoice.lines = vec![line(1, LineKind::Item, Some("mat"), minor, false)];
        enter(&ledger, invoice)
    };
    make("cur", ymd(2026, 10, 1), 10000); // due 10/31: current
    make("d20", ymd(2026, 8, 26), 20000); // due 9/25: 20 days
    make("d45", ymd(2026, 8, 1), 30000); // due 8/31: 45 days
    make("d120", ymd(2026, 5, 18), 40000); // due 6/17: 120 days
    make("later", ymd(2026, 10, 20), 99900); // after as_of: not in the aging

    // A partial payment on the 45-day invoice and one paying the current one off.
    let mut payment = doc(
        "pay-1",
        DocKind::Payment,
        ymd(2026, 10, 10),
        Some((ContactKind::Customer, "c-pool")),
    );
    payment.applications = vec![
        apply("d45", DocKind::Invoice, 12500),
        apply("cur", DocKind::Invoice, 10000),
    ];
    enter(&ledger, payment);

    // A credit memo, half applied to the 120-day invoice, half left open.
    let mut credit = doc(
        "cm-1",
        DocKind::CreditMemo,
        ymd(2026, 10, 12),
        Some((ContactKind::Customer, "c-pool")),
    );
    credit.lines = vec![line(1, LineKind::Item, Some("mat"), 5000, false)];
    credit.applications = vec![apply("d120", DocKind::Invoice, 2500)];
    enter(&ledger, credit);

    let open = books::open_documents(&ledger, CO, Book::Receivable, as_of).unwrap();
    let ids: Vec<_> = open
        .iter()
        .map(|d| (d.document_id.as_str(), d.open.minor()))
        .collect();
    assert_eq!(
        ids,
        [
            ("d120", 37500),
            ("d45", 17500),
            ("d20", 20000),
            ("cm-1", -2500)
        ]
    );

    let aging = books::aging(&ledger, CO, Book::Receivable, as_of).unwrap();
    assert_eq!(aging.rows.len(), 1);
    let row = &aging.totals;
    assert_eq!(row.current, Money::ZERO);
    // The 20-day invoice, less the open credit (dated 10/12, due on its date: 3 days).
    assert_eq!(row.days_1_30, usd(17500));
    assert_eq!(row.days_31_60, usd(17500));
    assert_eq!(row.days_61_90, Money::ZERO);
    assert_eq!(row.over_90, usd(37500));
    assert_eq!(row.total, usd(72500));
    assert_eq!(aging.rows[0].contact_name, "Pool Co");

    assert_eq!(
        balance(&ledger, chart::ACCOUNTS_RECEIVABLE, as_of),
        row.total
    );
}

#[test]
fn a_voided_invoice_leaves_the_aging() {
    let ledger = book();
    let mut invoice = doc(
        "inv-v",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        Some((ContactKind::Customer, "c-pool")),
    );
    invoice.lines = vec![line(1, LineKind::Item, Some("mat"), 10000, false)];
    let mut invoice = enter(&ledger, invoice);
    assert_eq!(
        books::open_documents(&ledger, CO, Book::Receivable, ymd(2026, 10, 1))
            .unwrap()
            .len(),
        1
    );
    invoice.is_voided = true;
    enter(&ledger, invoice);
    assert!(
        books::open_documents(&ledger, CO, Book::Receivable, ymd(2026, 10, 1))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ap_aging_ties_to_2000() {
    let ledger = book();
    let as_of = ymd(2026, 10, 15);
    let mut bill = doc(
        "bill-1",
        DocKind::Bill,
        ymd(2026, 9, 1),
        Some((ContactKind::Vendor, "v-foam")),
    );
    bill.number = Some("FS-778".to_string());
    bill.header_class = Some(ClassId("foam".to_string()));
    bill.lines = vec![line(1, LineKind::Item, Some("mat"), 50000, false)];
    let bill = enter(&ledger, bill);
    assert_eq!(bill.due_date, Some(ymd(2026, 9, 16)));
    assert_eq!(bill.number.as_deref(), Some("FS-778"));

    let mut payment = doc(
        "bp-1",
        DocKind::BillPayment,
        ymd(2026, 10, 1),
        Some((ContactKind::Vendor, "v-foam")),
    );
    payment.applications = vec![apply("bill-1", DocKind::Bill, 20000)];
    enter(&ledger, payment);

    let aging = books::aging(&ledger, CO, Book::Payable, as_of).unwrap();
    assert_eq!(aging.totals.days_1_30, usd(30000)); // due 9/16, 29 days
    assert_eq!(aging.totals.total, usd(30000));
    let ap = balance(&ledger, chart::ACCOUNTS_PAYABLE, as_of);
    assert_eq!(ap.checked_neg().unwrap(), aging.totals.total);
}

// ---------------------------------------------------------------------------
// Date-driven terms (D41)
// ---------------------------------------------------------------------------

fn day_of_month(day: u32, next_month_within: u32) -> Terms {
    Terms::DayOfMonth {
        day,
        next_month_within,
    }
}

#[test]
fn the_31st_is_the_last_day_of_a_shorter_month() {
    let end = day_of_month(31, 0);
    assert_eq!(end.due_date(ymd(2027, 2, 3)), ymd(2027, 2, 28));
    assert_eq!(end.due_date(ymd(2026, 4, 10)), ymd(2026, 4, 30));
    assert_eq!(end.due_date(ymd(2026, 1, 10)), ymd(2026, 1, 31));
    // Rolled out of January into February, still clamped.
    assert_eq!(
        day_of_month(31, 5).due_date(ymd(2027, 1, 28)),
        ymd(2027, 2, 28)
    );
    // The 30th in February, too.
    assert_eq!(
        day_of_month(30, 0).due_date(ymd(2027, 2, 1)),
        ymd(2027, 2, 28)
    );
}

#[test]
fn a_leap_february_ends_on_the_29th() {
    assert_eq!(
        day_of_month(31, 0).due_date(ymd(2028, 2, 3)),
        ymd(2028, 2, 29)
    );
    assert_eq!(
        day_of_month(29, 0).due_date(ymd(2027, 2, 3)),
        ymd(2027, 2, 28)
    );
    assert_eq!(
        day_of_month(29, 0).due_date(ymd(2028, 2, 3)),
        ymd(2028, 2, 29)
    );
    assert_eq!(
        day_of_month(31, 5).due_date(ymd(2028, 1, 28)),
        ymd(2028, 2, 29)
    );
    // Dated on the due day itself: 0 days away is within even a 0-day
    // window (the window is inclusive, D41), so it rolls a month.
    assert_eq!(
        day_of_month(31, 0).due_date(ymd(2028, 2, 29)),
        ymd(2028, 3, 31)
    );
}

#[test]
fn a_document_dated_within_the_window_is_due_a_month_later() {
    // QBO's "due on the 1st, next month if issued within 5 days" (term 37 in
    // the replica), checked against invoices QBO itself dated.
    let first = day_of_month(1, 5);
    assert_eq!(first.due_date(ymd(2026, 10, 7)), ymd(2026, 11, 1));
    assert_eq!(first.due_date(ymd(2026, 10, 26)), ymd(2026, 11, 1)); // 6 days out
    assert_eq!(first.due_date(ymd(2026, 10, 27)), ymd(2026, 12, 1)); // 5 days: rolls
    assert_eq!(first.due_date(ymd(2026, 2, 25)), ymd(2026, 4, 1)); // QBO: 2026-04-01
    assert_eq!(first.due_date(ymd(2026, 12, 28)), ymd(2027, 2, 1)); // across the year
    assert_eq!(first.due_date(ymd(2026, 11, 1)), ymd(2026, 12, 1)); // dated on the day
                                                                    // Term 47: the 31st, within 5 days (QBO: 2023-04-25 due 2023-05-31).
    let end = day_of_month(31, 5);
    assert_eq!(end.due_date(ymd(2023, 4, 25)), ymd(2023, 5, 31));
    assert_eq!(end.due_date(ymd(2023, 4, 24)), ymd(2023, 4, 30));
    assert_eq!(end.due_date(ymd(2022, 11, 30)), ymd(2022, 12, 31));
}

#[test]
fn terms_read_as_people_say_them() {
    assert_eq!(Terms::Net { days: 30 }.label(), "Net 30");
    assert_eq!(Terms::Net { days: 0 }.label(), "Due on receipt");
    assert_eq!(day_of_month(1, 5).label(), "Due the 1st");
    assert_eq!(day_of_month(2, 0).label(), "Due the 2nd");
    assert_eq!(day_of_month(13, 0).label(), "Due the 13th");
    assert_eq!(day_of_month(22, 0).label(), "Due the 22nd");
    assert_eq!(day_of_month(31, 5).label(), "Due the last of the month");
}

#[test]
fn a_day_of_month_customer_round_trips_and_fills_the_due_date_on_entry_and_edit() {
    let ledger = book();
    let mut club = customer("c-club", "Swim Club", None, false);
    club.due_day = Some(DueDay {
        day: 1,
        next_month_within: 5,
    });
    club.terms_source_ref = Some("37".to_string());
    books::upsert_contact(&ledger, CO, &club, now()).unwrap();
    let stored = books::contact(&ledger, CO, "c-club").unwrap().unwrap();
    assert_eq!(stored, club);
    assert_eq!(stored.terms(), day_of_month(1, 5));

    let mut invoice = doc(
        "inv-club",
        DocKind::Invoice,
        ymd(2026, 10, 28),
        Some((ContactKind::Customer, "c-club")),
    );
    invoice.lines = vec![line(1, LineKind::Item, Some("mat"), 10000, false)];
    let saved = enter(&ledger, invoice);
    assert_eq!(saved.due_date, Some(ymd(2026, 12, 1)));

    // An edit that clears the due date and moves the date refills it.
    let mut edited = saved.clone();
    edited.txn_date = ymd(2026, 10, 2);
    edited.due_date = None;
    let revision = books::update_document(
        &ledger,
        CO,
        edited,
        PostingConfig::default(),
        CommandMeta {
            actor_id: "dan".to_string(),
            kind: "test".to_string(),
            hlc: now().to_rfc3339(),
        },
        now(),
    )
    .unwrap();
    assert_eq!(revision.document.due_date, Some(ymd(2026, 11, 1)));

    // The aging reads the stored due date: 1 Nov 2026, 30 days past due on 1 Dec.
    let row = books::open_documents(&ledger, CO, Book::Receivable, ymd(2026, 12, 1))
        .unwrap()
        .into_iter()
        .find(|d| d.document_id == "inv-club")
        .unwrap();
    assert_eq!(row.due_date, ymd(2026, 11, 1));

    // Switching the customer back to net days drops the day of the month,
    // and keeps the QBO term id it came from.
    let mut net = stored.clone();
    net.set_terms(Terms::Net { days: 15 });
    net.terms_source_ref = None;
    books::upsert_contact(&ledger, CO, &net, now()).unwrap();
    let back = books::contact(&ledger, CO, "c-club").unwrap().unwrap();
    assert_eq!((back.terms_days, back.due_day), (Some(15), None));
    assert_eq!(back.terms_source_ref.as_deref(), Some("37"));
}

#[test]
fn terms_that_are_both_or_out_of_range_are_refused() {
    let ledger = book();
    let mut both = customer("c-both", "Both", Some(30), false);
    both.due_day = Some(DueDay {
        day: 1,
        next_month_within: 0,
    });
    assert!(books::upsert_contact(&ledger, CO, &both, now()).is_err());
    for (day, within) in [(0, 0), (32, 0), (1, 32)] {
        let mut bad = customer("c-bad", "Bad", None, false);
        bad.due_day = Some(DueDay {
            day,
            next_month_within: within,
        });
        assert!(
            books::upsert_contact(&ledger, CO, &bad, now()).is_err(),
            "{day}/{within}"
        );
    }
}
