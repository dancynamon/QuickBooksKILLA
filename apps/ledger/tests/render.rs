//! D39: customer and vendor documents rendered from the own book. Every
//! check reads the PDF's own bytes back ([`pdf::extract_text`]) rather than
//! trusting the layout's bookkeeping; the statement must tie to
//! `books::aging`; and nothing here reaches the network — the pay link runs
//! against `MockAuthnet` or not at all.

use std::path::Path;
use std::process::Command;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use ledger::authnet::link::MockAuthnet;
use ledger::books::{self, Book, Contact, Item, ItemType};
use ledger::chart;
use ledger::mcp::{self, DocumentsSetup};
use ledger::post::{self, Posting};
use ledger::render::{
    self, pdf, CompanyIdentity, PayLinkMode, PayLinkOutcome, RenderConfig, RenderError,
};
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

fn command() -> CommandMeta {
    CommandMeta {
        actor_id: "dan".to_string(),
        kind: "test".to_string(),
        hlc: now().to_rfc3339(),
    }
}

fn contact(id: &str, kind: ContactKind, name: &str) -> Contact {
    Contact {
        contact_id: id.to_string(),
        kind,
        display_name: name.to_string(),
        company_name: None,
        email: Some("ap@example.com".to_string()),
        phone: None,
        bill_address: Some("12 Harbor Road\nBayville, NJ 08721".to_string()),
        ship_address: None,
        terms_days: Some(30),
        is_tax_exempt: false,
        default_class: (kind == ContactKind::Customer).then(|| ClassId("foam".to_string())),
        is_active: true,
        source_ref: None,
    }
}

fn item(id: &str, sku: &str, name: &str, description: &str) -> Item {
    Item {
        item_id: id.to_string(),
        sku: Some(sku.to_string()),
        name: name.to_string(),
        description: Some(description.to_string()),
        item_type: ItemType::NonInventory,
        income_account: AccountId(chart::SALES_INCOME.to_string()),
        expense_account: AccountId(chart::SHOP_SUPPLIES.to_string()),
        asset_account: None,
        default_class: Some(ClassId("foam".to_string())),
        sales_price: None,
        purchase_cost: None,
        unit_cost: None,
        is_taxable: true,
        is_active: true,
        source_ref: None,
    }
}

/// A synthetic book: one customer with both addresses, one without a ship
/// address, a vendor, and two items.
fn book_at(ledger: Ledger) -> Ledger {
    ledger
        .create_company(CO, "Aquamentor Test Books", None, now())
        .unwrap();
    let mut pool = contact("c-pool", ContactKind::Customer, "Blue Harbor Swim Club");
    pool.ship_address = Some("Pool Deck Receiving\n40 Shore Ave\nBayville, NJ 08721".to_string());
    books::upsert_contact(&ledger, CO, &pool, now()).unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &contact("c-city", ContactKind::Customer, "City Parks Dept"),
        now(),
    )
    .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &contact("v-foam", ContactKind::Vendor, "Foam Supplier Co"),
        now(),
    )
    .unwrap();
    books::upsert_item(
        &ledger,
        CO,
        &item(
            "mat",
            "AKP-1712",
            "Kneeling pad",
            "Kneeling Pad - Medium 17.5x12x1.5",
        ),
        now(),
    )
    .unwrap();
    books::upsert_item(
        &ledger,
        CO,
        &item("tube", "RT-40", "Rescue tube", "Rescue Tube 40in, red"),
        now(),
    )
    .unwrap();
    ledger
}

fn book() -> Ledger {
    book_at(Ledger::open_in_memory().unwrap())
}

fn line(no: i64, kind: LineKind, item_id: Option<&str>, qty: Option<i64>, minor: i64) -> DocLine {
    DocLine {
        line_no: no,
        kind,
        amount: usd(minor),
        class: None,
        item_id: item_id.map(String::from),
        account: None,
        is_taxable: kind == LineKind::Item,
        qty: qty.map(Decimal::from),
        unit_cost: None,
        description: None,
        posting: None,
        entity: None,
    }
}

fn doc(id: &str, kind: DocKind, on: NaiveDate, who: &str, lines: Vec<DocLine>) -> LedgerDocument {
    let contact_kind = if who.starts_with("v-") {
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
        contact: Some(books::contact_ref(contact_kind, who)),
        header_class: None,
        lines,
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

/// Prepare, save and post: the standalone entry path.
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

/// Invoice 1001: 10 pads at 15.75, 4 tubes at 32.00, shipping 25.00, NJ tax
/// on the item lines, and a memo.
fn invoice_1001(ledger: &Ledger) -> LedgerDocument {
    let mut invoice = doc(
        "inv-1001",
        DocKind::Invoice,
        ymd(2026, 9, 1),
        "c-pool",
        vec![
            line(1, LineKind::Item, Some("mat"), Some(10), 15_750),
            line(2, LineKind::Item, Some("tube"), Some(4), 12_800),
            line(3, LineKind::Shipping, None, None, 2_500),
        ],
    );
    invoice.memo = Some("PO 7781. Deliver to the pool deck.".to_string());
    enter(ledger, invoice)
}

fn pay(ledger: &Ledger, id: &str, on: NaiveDate, target: &str, minor: i64) {
    let mut payment = doc(id, DocKind::Payment, on, "c-pool", Vec::new());
    payment.applications = vec![Application {
        target_document_id: target.to_string(),
        target_kind: DocKind::Invoice,
        amount: usd(minor),
    }];
    enter(ledger, payment);
}

fn identity_config() -> RenderConfig {
    RenderConfig::parse(
        "[documents]\n\
         [documents.company.aquamentor]\n\
         name = \"Example Aquatics\"\n\
         legal_name = \"Example Aquatics Inc\"\n\
         address = [\"1 Test Plaza\", \"Testville, NJ 07000\"]\n\
         phone = \"(555) 010-0000\"\n\
         email = \"billing@example.com\"\n\
         footer = \"Thank you for your business.\"\n",
    )
    .unwrap()
}

fn text_of(path: &Path) -> (String, usize) {
    let bytes = std::fs::read(path).unwrap();
    assert!(
        bytes.starts_with(b"%PDF-1.4"),
        "not a PDF: {}",
        path.display()
    );
    let pages = pdf::page_count(&bytes).unwrap();
    (pdf::extract_text(&bytes).join("\n"), pages)
}

fn has_line(text: &str, wanted: &str) -> bool {
    text.lines().any(|l| l == wanted)
}

/// The value printed right after `label` in the totals stack.
fn value_after(text: &str, label: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .rposition(|l| *l == label)
        .unwrap_or_else(|| panic!("no {label:?} in:\n{text}"));
    lines[at + 1].to_string()
}

#[test]
fn an_open_invoice_prints_number_lines_totals_and_balance_due() {
    let ledger = book();
    let invoice = invoice_1001(&ledger);
    assert_eq!(invoice.number.as_deref(), Some("1001"));
    pay(&ledger, "pay-1", ymd(2026, 9, 20), "inv-1001", 10_000);
    let dir = tempfile::tempdir().unwrap();

    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &identity_config(),
        dir.path(),
        PayLinkMode::Unconfigured,
        now(),
    )
    .unwrap();
    assert_eq!(
        done.rendered.path.file_name().unwrap(),
        "Invoice 1001 - Blue Harbor Swim Club.pdf"
    );
    let (text, pages) = text_of(&done.rendered.path);
    assert_eq!(pages, 1);
    assert_eq!(done.rendered.page_count, 1);

    // 157.50 + 128.00 = 285.50 taxable; tax 6.625% = 18.91; + 25.00 shipping.
    for wanted in [
        "INVOICE",
        "1001",
        "09/01/2026",
        "Net 30",
        "10/01/2026",
        "Example Aquatics",
        "1 Test Plaza",
        "BILL TO",
        "SHIP TO",
        "Blue Harbor Swim Club",
        "40 Shore Ave",
        "AKP-1712",
        "Kneeling Pad - Medium 17.5x12x1.5",
        "10",
        "$15.75",
        "$157.50",
        "RT-40",
        "$32.00",
        "Shipping",
        "$25.00",
        "PO 7781. Deliver to the pool deck.",
        "Thank you for your business.",
        "Page 1 of 1",
    ] {
        assert!(has_line(&text, wanted), "missing {wanted:?} in:\n{text}");
    }
    assert_eq!(value_after(&text, "Subtotal"), "$310.50");
    assert_eq!(value_after(&text, "Sales Tax (6.625%)"), "$18.91");
    assert_eq!(value_after(&text, "TOTAL"), "$329.41");
    assert_eq!(value_after(&text, "Amount Paid"), "$100.00");
    assert_eq!(value_after(&text, "BALANCE DUE"), "$229.41");
    assert!(!text.contains("VOID"));
    assert!(!text.contains("PAY ONLINE"));
    assert_eq!(done.figures.balance, usd(22_941));
    assert!(matches!(done.pay_link, PayLinkOutcome::NotConfigured(_)));
}

#[test]
fn a_configured_gateway_adds_a_pay_box_and_the_page_beside_the_pdf() {
    let ledger = book();
    invoice_1001(&ledger);
    pay(&ledger, "pay-1", ymd(2026, 9, 20), "inv-1001", 10_000);
    let dir = tempfile::tempdir().unwrap();
    let mut mock = MockAuthnet::new();
    let handle = mock.clone();

    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &identity_config(),
        dir.path(),
        PayLinkMode::Client(&mut mock),
        now(),
    )
    .unwrap();
    let PayLinkOutcome::Attached { link, page } = &done.pay_link else {
        panic!("{:?}", done.pay_link);
    };
    assert_eq!(link.amount, usd(22_941));
    assert_eq!(handle.requests()[0].invoice_number, "1001");
    let html = std::fs::read_to_string(page).unwrap();
    assert!(html.contains("mock-token-1"));
    assert_eq!(
        page.file_name().unwrap(),
        "Invoice 1001 - Blue Harbor Swim Club.pay.html"
    );
    let (text, _) = text_of(&done.rendered.path);
    assert!(has_line(&text, "PAY ONLINE"), "{text}");
    assert!(text.contains("Use the secure payment link sent with this invoice."));
    // The 15-minute token never goes on the PDF.
    assert!(
        !String::from_utf8_lossy(&std::fs::read(&done.rendered.path).unwrap())
            .contains("mock-token")
    );

    // With a durable pay page configured, the box prints and links it.
    let mut config = identity_config();
    config.pay_url = Some("https://pay.example.com/i/{number}".to_string());
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &config,
        dir.path(),
        PayLinkMode::Client(&mut mock),
        now(),
    )
    .unwrap();
    let bytes = std::fs::read(&done.rendered.path).unwrap();
    let text = pdf::extract_text(&bytes).join("\n");
    assert!(has_line(&text, "https://pay.example.com/i/1001"), "{text}");
    assert!(String::from_utf8_lossy(&bytes).contains("/URI (https://pay.example.com/i/1001)"));

    // A gateway refusal leaves the box off and says so.
    handle.fail_next(ledger::authnet::link::LinkError::Http(503));
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &identity_config(),
        dir.path(),
        PayLinkMode::Client(&mut mock),
        now(),
    )
    .unwrap();
    assert!(matches!(done.pay_link, PayLinkOutcome::Failed(_)));
    assert!(!text_of(&done.rendered.path).0.contains("PAY ONLINE"));
    assert!(done.rendered.warnings.iter().any(|w| w.contains("503")));
}

#[test]
fn a_paid_invoice_gets_no_pay_link_and_shows_zero_due() {
    let ledger = book();
    invoice_1001(&ledger);
    pay(&ledger, "pay-1", ymd(2026, 9, 20), "inv-1001", 32_941);
    let dir = tempfile::tempdir().unwrap();
    let mut mock = MockAuthnet::new();
    let handle = mock.clone();
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &identity_config(),
        dir.path(),
        PayLinkMode::Client(&mut mock),
        now(),
    )
    .unwrap();
    assert_eq!(done.pay_link, PayLinkOutcome::NotApplicable);
    assert!(handle.requests().is_empty());
    let (text, _) = text_of(&done.rendered.path);
    assert_eq!(value_after(&text, "Amount Paid"), "$329.41");
    assert_eq!(value_after(&text, "BALANCE DUE"), "$0.00");
}

#[test]
fn a_voided_invoice_is_watermarked_void_and_owes_nothing() {
    let ledger = book();
    invoice_1001(&ledger);
    books::void_document(&ledger, CO, "inv-1001", "duplicate", command(), now()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut mock = MockAuthnet::new();
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &identity_config(),
        dir.path(),
        PayLinkMode::Client(&mut mock),
        now(),
    )
    .unwrap();
    assert_eq!(
        done.rendered.path.file_name().unwrap(),
        "Invoice 1001 - Blue Harbor Swim Club (VOID).pdf"
    );
    assert_eq!(done.pay_link, PayLinkOutcome::NotApplicable);
    let (text, _) = text_of(&done.rendered.path);
    // The watermark is the first thing drawn on the page, and the meta says
    // so too.
    assert_eq!(text.lines().next(), Some("VOID"));
    assert_eq!(value_after(&text, "Status:"), "VOID");
    assert_eq!(value_after(&text, "TOTAL"), "$329.41");
    assert_eq!(value_after(&text, "BALANCE DUE"), "$0.00");
    assert!(text.contains("Voided: duplicate"));
}

#[test]
fn estimate_receipt_credit_memo_and_purchase_order_each_render() {
    let ledger = book();
    let dir = tempfile::tempdir().unwrap();
    let config = identity_config();
    let render = |id: &str| {
        let done = render::render_document(
            &ledger,
            CO,
            id,
            &config,
            dir.path(),
            PayLinkMode::Off,
            now(),
        )
        .unwrap();
        text_of(&done.rendered.path).0
    };

    let mut estimate = doc(
        "est-1",
        DocKind::Estimate,
        ymd(2026, 9, 3),
        "c-city",
        vec![line(1, LineKind::Item, Some("tube"), Some(12), 38_400)],
    );
    estimate.due_date = Some(ymd(2026, 10, 3));
    enter(&ledger, estimate);
    let text = render("est-1");
    assert!(has_line(&text, "ESTIMATE"));
    assert_eq!(value_after(&text, "Estimate #:"), "1001");
    assert_eq!(value_after(&text, "Expires:"), "10/03/2026");
    assert_eq!(value_after(&text, "TOTAL"), "$409.44");
    assert!(!text.contains("BALANCE DUE"));
    assert!(!text.contains("SHIP TO"), "City Parks has no ship address");

    let receipt = doc(
        "sr-1",
        DocKind::SalesReceipt,
        ymd(2026, 9, 4),
        "c-city",
        vec![line(1, LineKind::Item, Some("mat"), Some(2), 3_150)],
    );
    enter(&ledger, receipt);
    let text = render("sr-1");
    assert!(has_line(&text, "SALES RECEIPT"));
    assert_eq!(value_after(&text, "TOTAL"), "$33.59");
    assert_eq!(value_after(&text, "Amount Paid"), "$33.59");
    assert_eq!(value_after(&text, "BALANCE DUE"), "$0.00");

    invoice_1001(&ledger);
    let credit = doc(
        "cm-1",
        DocKind::CreditMemo,
        ymd(2026, 9, 10),
        "c-pool",
        vec![line(1, LineKind::Item, Some("mat"), Some(1), 1_575)],
    );
    enter(&ledger, credit);
    books::apply_credit(
        &ledger,
        CO,
        "cm-1",
        "inv-1001",
        Some(usd(1_000)),
        command(),
        now(),
    )
    .unwrap();
    let text = render("cm-1");
    assert!(has_line(&text, "CREDIT MEMO"));
    assert_eq!(value_after(&text, "TOTAL CREDIT"), "$16.79");
    assert_eq!(value_after(&text, "Applied"), "$10.00");
    assert_eq!(value_after(&text, "REMAINING CREDIT"), "$6.79");
    // And the invoice it went against shows it as paid.
    assert_eq!(value_after(&render("inv-1001"), "Amount Paid"), "$10.00");

    let mut po = doc(
        "po-1",
        DocKind::PurchaseOrder,
        ymd(2026, 9, 5),
        "v-foam",
        vec![line(1, LineKind::Item, Some("mat"), Some(100), 50_000)],
    );
    po.lines[0].is_taxable = false;
    po.memo = Some("Ship by 9/20.".to_string());
    enter(&ledger, po);
    let text = render("po-1");
    assert!(has_line(&text, "PURCHASE ORDER"));
    assert_eq!(value_after(&text, "P.O. #:"), "1001");
    assert!(has_line(&text, "VENDOR"));
    assert!(has_line(&text, "Foam Supplier Co"));
    // Ship to is us, from the config.
    let ship = text.lines().position(|l| l == "SHIP TO").unwrap();
    assert_eq!(text.lines().nth(ship + 1), Some("Example Aquatics"));
    assert_eq!(value_after(&text, "TOTAL"), "$500.00");
    assert_eq!(value_after(&text, "$5.00"), "$500.00", "rate then amount");
    assert!(text.contains("Ship by 9/20."));
}

#[test]
fn documents_that_are_not_customer_facing_are_refused() {
    let ledger = book();
    invoice_1001(&ledger);
    pay(&ledger, "pay-1", ymd(2026, 9, 20), "inv-1001", 10_000);
    let dir = tempfile::tempdir().unwrap();
    let err = render::render_document(
        &ledger,
        CO,
        "pay-1",
        &identity_config(),
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap_err();
    assert!(
        matches!(err, RenderError::NotRenderable("payment")),
        "{err}"
    );
    let err = render::render_document(
        &ledger,
        CO,
        "nope",
        &identity_config(),
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("nope"));
}

/// Four invoices across the buckets, a partial payment and an open credit.
fn statement_book() -> Ledger {
    let ledger = book();
    for (id, on, minor) in [
        ("inv-a", ymd(2026, 5, 1), 40_000), // due 5/31: 137 days late on 10/15
        ("inv-b", ymd(2026, 7, 20), 25_000), // due 8/19: 57 days
        ("inv-c", ymd(2026, 8, 25), 12_000), // due 9/24: 21 days
        ("inv-d", ymd(2026, 10, 1), 9_000), // due 10/31: current
    ] {
        let mut invoice = doc(
            id,
            DocKind::Invoice,
            on,
            "c-pool",
            vec![line(1, LineKind::Item, Some("mat"), Some(1), minor)],
        );
        invoice.lines[0].is_taxable = false;
        enter(&ledger, invoice);
    }
    pay(&ledger, "pay-a", ymd(2026, 9, 1), "inv-a", 15_000);
    let mut credit = doc(
        "cm-x",
        DocKind::CreditMemo,
        ymd(2026, 10, 5),
        "c-pool",
        vec![line(1, LineKind::Item, Some("tube"), Some(1), 2_000)],
    );
    credit.lines[0].is_taxable = false;
    enter(&ledger, credit);
    // Another customer's invoice must not appear.
    enter(
        &ledger,
        doc(
            "inv-other",
            DocKind::Invoice,
            ymd(2026, 9, 1),
            "c-city",
            vec![line(1, LineKind::Item, Some("mat"), Some(1), 7_700)],
        ),
    );
    // Nor an invoice dated after the statement.
    enter(
        &ledger,
        doc(
            "inv-later",
            DocKind::Invoice,
            ymd(2026, 10, 20),
            "c-pool",
            vec![line(1, LineKind::Item, Some("mat"), Some(1), 1_100)],
        ),
    );
    ledger
}

#[test]
fn a_statement_lists_open_items_and_ties_to_the_aging() {
    let ledger = statement_book();
    let as_of = ymd(2026, 10, 15);
    let dir = tempfile::tempdir().unwrap();
    let done =
        render::render_statement(&ledger, CO, "c-pool", as_of, &identity_config(), dir.path())
            .unwrap();
    assert_eq!(
        done.rendered.path.file_name().unwrap(),
        "Statement - Blue Harbor Swim Club - 2026-10-15.pdf"
    );

    let aging = books::aging(&ledger, CO, Book::Receivable, as_of).unwrap();
    let row = aging
        .rows
        .iter()
        .find(|r| r.contact_id == "c-pool")
        .unwrap();
    assert_eq!(&done.aging, row, "the statement is the aging's own row");
    assert_eq!(row.total, usd(25_000 + 25_000 + 12_000 + 9_000 - 2_000));
    assert_eq!(done.open_items, 5);

    let (text, pages) = text_of(&done.rendered.path);
    assert_eq!(pages, 1);
    assert!(has_line(&text, "STATEMENT"));
    assert_eq!(value_after(&text, "Statement Date:"), "10/15/2026");
    for (label, value) in [
        ("Current", row.current),
        ("1-30 Days Past Due", row.days_1_30),
        ("31-60 Days Past Due", row.days_31_60),
        ("61-90 Days Past Due", row.days_61_90),
        ("Over 90 Days Past Due", row.over_90),
        ("TOTAL DUE", row.total),
    ] {
        assert_eq!(value_after(&text, label), render::usd(value), "{label}");
    }
    // The credit is due the day it was issued, so it ages like an invoice.
    assert_eq!(value_after(&text, "Current"), "$90.00");
    assert_eq!(
        value_after(&text, "1-30 Days Past Due"),
        "$100.00",
        "120.00 less the credit"
    );
    assert_eq!(value_after(&text, "Over 90 Days Past Due"), "$250.00");
    assert_eq!(value_after(&text, "Total Due:"), "$690.00");
    for wanted in [
        "Invoice #1001",
        "137 days past due",
        "$400.00",
        "Credit Memo #1001",
        "Credit available",
        "-$20.00",
    ] {
        assert!(has_line(&text, wanted), "missing {wanted:?} in:\n{text}");
    }
    assert!(!text.contains("$77.00"), "another customer's invoice");
    assert!(
        !text.contains("$11.00"),
        "an invoice after the statement date"
    );

    // A customer with nothing open still gets a statement saying so.
    let empty = render::render_statement(
        &ledger,
        CO,
        "c-city",
        ymd(2026, 8, 1),
        &identity_config(),
        dir.path(),
    )
    .unwrap();
    let (text, _) = text_of(&empty.rendered.path);
    assert!(text.contains("No open items as of 08/01/2026."));
    assert_eq!(value_after(&text, "TOTAL DUE"), "$0.00");

    let err =
        render::render_statement(&ledger, CO, "v-foam", as_of, &identity_config(), dir.path())
            .unwrap_err();
    assert!(matches!(err, RenderError::NotACustomer(_)), "{err}");
    assert!(matches!(
        render::render_statement(
            &ledger,
            CO,
            "c-nobody",
            as_of,
            &identity_config(),
            dir.path()
        ),
        Err(RenderError::UnknownContact(_))
    ));
}

#[test]
fn a_long_invoice_paginates_with_the_header_repeated_and_totals_last() {
    let ledger = book();
    let lines: Vec<DocLine> = (1..=70)
        .map(|n| {
            line(
                n,
                LineKind::Item,
                Some(if n % 2 == 0 { "mat" } else { "tube" }),
                Some(1),
                1_000,
            )
        })
        .collect();
    enter(
        &ledger,
        doc(
            "inv-long",
            DocKind::Invoice,
            ymd(2026, 9, 1),
            "c-pool",
            lines,
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let done = render::render_document(
        &ledger,
        CO,
        "inv-long",
        &identity_config(),
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap();
    let bytes = std::fs::read(&done.rendered.path).unwrap();
    let pages = pdf::extract_text(&bytes);
    assert!(pages.len() >= 2, "{} pages", pages.len());
    assert_eq!(pdf::page_count(&bytes), Some(pages.len()));
    assert_eq!(done.rendered.page_count, pages.len());
    let last = pages.last().unwrap();
    for (i, page) in pages.iter().enumerate() {
        assert!(
            has_line(page, &format!("Page {} of {}", i + 1, pages.len())),
            "page {i}"
        );
        assert!(has_line(page, "DESCRIPTION"), "table header on page {i}");
    }
    assert!(has_line(&pages[1], "Invoice #: 1001 (continued)"));
    assert_eq!(
        value_after(last, "TOTAL"),
        "$746.38",
        "700.00 plus 46.38 tax"
    );
    assert!(!pages[0].contains("BALANCE DUE"));
    // Every line made it, once.
    let all = pages.join("\n");
    assert_eq!(all.lines().filter(|l| *l == "$10.00").count(), 70 * 2);
}

fn tiny_png(path: &Path) {
    fn chunk(out: &mut Vec<u8>, kind: &[u8], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&[0, 0, 0, 0]);
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 4u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    let row = [
        0u8, 27, 58, 91, 255, 27, 58, 91, 255, 0, 0, 0, 0, 27, 58, 91, 128,
    ];
    let raw = [row, row].concat();
    chunk(
        &mut png,
        b"IDAT",
        &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6),
    );
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png).unwrap();
}

#[test]
fn the_logo_comes_from_config_and_falls_back_to_the_name() {
    let ledger = book();
    invoice_1001(&ledger);
    let dir = tempfile::tempdir().unwrap();
    let logo = dir.path().join("logo.png");
    tiny_png(&logo);
    let mut config = identity_config();
    config.company.get_mut(CO).unwrap().logo = Some(logo.clone());
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &config,
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap();
    assert!(
        done.rendered.warnings.is_empty(),
        "{:?}",
        done.rendered.warnings
    );
    let bytes = std::fs::read(&done.rendered.path).unwrap();
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("/Subtype /Image /Width 4 /Height 2"));
    assert!(
        raw.contains("/SMask"),
        "the transparent pixels keep their alpha"
    );
    // With a logo the name is not drawn as a wordmark; the legal name is.
    let text = pdf::extract_text(&bytes).join("\n");
    assert!(!has_line(&text, "Example Aquatics"));
    assert!(has_line(&text, "Example Aquatics Inc"), "{text}");

    config.company.get_mut(CO).unwrap().logo = Some(dir.path().join("missing.png"));
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &config,
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap();
    assert!(done.rendered.warnings[0].contains("missing.png"));
    assert!(has_line(
        &text_of(&done.rendered.path).0,
        "Example Aquatics"
    ));
}

#[test]
fn with_no_identity_configured_the_ledger_company_name_prints() {
    let ledger = book();
    invoice_1001(&ledger);
    let dir = tempfile::tempdir().unwrap();
    let identity = render::company_identity(&ledger, CO, &RenderConfig::default()).unwrap();
    assert_eq!(
        identity,
        CompanyIdentity {
            name: "Aquamentor Test Books".to_string(),
            ..CompanyIdentity::default()
        }
    );
    let done = render::render_document(
        &ledger,
        CO,
        "inv-1001",
        &RenderConfig::default(),
        dir.path(),
        PayLinkMode::Off,
        now(),
    )
    .unwrap();
    let (text, _) = text_of(&done.rendered.path);
    assert!(has_line(&text, "Aquamentor Test Books"));
    assert!(!text.contains("Example Aquatics"));
}

// ---------------------------------------------------------------------------
// The MCP tools and the CLI
// ---------------------------------------------------------------------------

fn call_tool(server: &mut mcp::Server, id: i64, name: &str, arguments: Value) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                       "params": { "name": name, "arguments": arguments } })
    .to_string();
    serde_json::from_str(&server.handle_line(&line).unwrap()).unwrap()
}

#[test]
fn the_render_tools_write_the_pdf_and_report_it() {
    let ledger = statement_book();
    let dir = tempfile::tempdir().unwrap();
    let setup = DocumentsSetup {
        config: identity_config(),
        out_dir: Some(dir.path().to_path_buf()),
    };
    let mock = MockAuthnet::new();
    let mut server = mcp::new_server_with_documents(
        &ledger,
        CO.to_string(),
        Some(Box::new(mock.clone())),
        setup.clone(),
    );

    let response = call_tool(
        &mut server,
        1,
        "render_document",
        json!({ "document_id": "inv-d" }),
    );
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    let out = &response["result"]["structuredContent"];
    assert_eq!(out["number"], json!("1004"));
    assert_eq!(out["kind"], json!("invoice"));
    assert_eq!(out["page_count"], json!(1));
    assert_eq!(out["total"], json!("90.00"));
    assert_eq!(out["balance_due"], json!("90.00"));
    assert_eq!(out["pay_link"]["status"], json!("attached"));
    assert_eq!(out["pay_link"]["amount"], json!("90.00"));
    assert!(
        out["pay_link"].get("token").is_none(),
        "the token stays in the page file"
    );
    let path = Path::new(out["path"].as_str().unwrap());
    assert!(path.starts_with(dir.path()));
    assert!(has_line(&text_of(path).0, "1004"));
    assert!(Path::new(out["pay_link"]["page"].as_str().unwrap()).exists());

    let response = call_tool(
        &mut server,
        2,
        "render_document",
        json!({ "document_id": "inv-d", "pay_link": false }),
    );
    assert_eq!(
        response["result"]["structuredContent"]["pay_link"]["status"],
        json!("skipped")
    );
    assert_eq!(mock.requests().len(), 1);

    let response = call_tool(
        &mut server,
        3,
        "render_statement",
        json!({ "contact_id": "c-pool", "as_of": "2026-10-15" }),
    );
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    let out = &response["result"]["structuredContent"];
    assert_eq!(out["aging"]["total"], json!("690.00"));
    assert_eq!(out["open_items"], json!(5));
    let aging = call_tool(
        &mut server,
        4,
        "aging",
        json!({ "book": "receivable", "as_of": "2026-10-15" }),
    );
    let row = aging["result"]["structuredContent"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["contact_id"] == json!("c-pool"))
        .unwrap()
        .clone();
    assert_eq!(out["aging"], row);

    let response = call_tool(
        &mut server,
        5,
        "render_statement",
        json!({ "contact_id": "v-foam", "as_of": "2026-10-15" }),
    );
    assert_eq!(response["result"]["isError"], json!(true));

    // No gateway at all: the invoice still renders, without the box.
    let mut bare = mcp::new_server_with_documents(&ledger, CO.to_string(), None, setup);
    let response = call_tool(
        &mut bare,
        6,
        "render_document",
        json!({ "document_id": "inv-d" }),
    );
    assert_eq!(
        response["result"]["structuredContent"]["pay_link"]["status"],
        json!("not_configured")
    );
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(args)
        // Never let a developer's environment send the CLI to a gateway or
        // into their own output folder.
        .env_remove("AUTHNET_API_LOGIN_ID")
        .env_remove("AUTHNET_TRANSACTION_KEY")
        .env_remove("LEDGER_DOCUMENTS_DIR")
        .output()
        .expect("spawn the ledger binary")
}

#[test]
fn ledger_render_writes_documents_and_statements_from_a_file_book() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    {
        let ledger = book_at(Ledger::open(&db).unwrap());
        invoice_1001(&ledger);
    }
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[documents]\n[documents.company.aquamentor]\nname = \"Example Aquatics\"\n",
    )
    .unwrap();
    let out = dir.path().join("pdfs");
    let (db_s, config_s, out_s) = (
        db.to_str().unwrap(),
        config.to_str().unwrap(),
        out.to_str().unwrap(),
    );

    let output = run(&[
        "render",
        "document",
        "--db",
        db_s,
        "--company",
        CO,
        "--document",
        "inv-1001",
        "--out",
        out_s,
        "--config",
        config_s,
        "--no-pay-link",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("pages       : 1"), "{stdout}");
    assert!(stdout.contains("balance due : 329.41"), "{stdout}");
    let pdf_path = out.join("Invoice 1001 - Blue Harbor Swim Club.pdf");
    assert!(has_line(&text_of(&pdf_path).0, "Example Aquatics"));

    let output = run(&[
        "render",
        "statement",
        "--db",
        db_s,
        "--company",
        CO,
        "--customer",
        "c-pool",
        "--as-of",
        "2026-10-15",
        "--out",
        out_s,
        "--config",
        config_s,
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("total due   : 329.41"), "{stdout}");
    assert!(out
        .join("Statement - Blue Harbor Swim Club - 2026-10-15.pdf")
        .exists());

    // Usage errors exit 2.
    let output = run(&["render", "statement", "--db", db_s, "--company", CO]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(&[
        "render",
        "document",
        "--db",
        db_s,
        "--company",
        CO,
        "--as-of",
        "2026-10-15",
    ]);
    assert_eq!(output.status.code(), Some(2));
}
