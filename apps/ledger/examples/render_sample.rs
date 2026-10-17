//! Renders one of every D39 document from a synthetic book, for eyeballing
//! the layout: `cargo run -p ledger --example render_sample -- [--logo
//! PATH.png] [--config PATH] [--out DIR]`.
//!
//! The book is made up here (customers, items, amounts); the company
//! identity is a placeholder unless `--config` names a file with a
//! `[documents.company.aquamentor]` table. Output defaults to
//! `target/render-sample/`, which git ignores. The pay link runs against
//! `MockAuthnet`, so nothing reaches Authorize.net.

use std::path::PathBuf;

use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;

use ledger::authnet::link::MockAuthnet;
use ledger::books::{self, Contact, Item, ItemType};
use ledger::chart;
use ledger::post::{self, Posting};
use ledger::render::{self, PayLinkMode, RenderConfig};
use ledger::store::{CommandMeta, Ledger};
use ledger::types::{
    AccountId, Application, ClassId, ContactKind, DocKind, DocLine, LedgerDocument, LineKind,
    PostingConfig,
};
use ledger_core::Money;

const CO: &str = "aquamentor";

fn ymd(m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2027, m, d).unwrap()
}

fn command() -> CommandMeta {
    CommandMeta {
        actor_id: "sample".into(),
        kind: "render_sample".into(),
        hlc: Utc::now().to_rfc3339(),
    }
}

fn customer(id: &str, name: &str, bill: &str, ship: Option<&str>) -> Contact {
    Contact {
        contact_id: id.into(),
        kind: ContactKind::Customer,
        display_name: name.into(),
        company_name: None,
        email: Some("ap@example.com".into()),
        phone: None,
        bill_address: Some(bill.into()),
        ship_address: ship.map(String::from),
        terms_days: Some(30),
        due_day: None,
        terms_source_ref: None,
        is_tax_exempt: false,
        default_class: Some(ClassId("foam".into())),
        is_active: true,
        source_ref: None,
    }
}

fn item(id: &str, sku: &str, description: &str) -> Item {
    Item {
        item_id: id.into(),
        sku: Some(sku.into()),
        name: sku.into(),
        description: Some(description.into()),
        item_type: ItemType::NonInventory,
        income_account: AccountId(chart::SALES_INCOME.into()),
        expense_account: AccountId(chart::SHOP_SUPPLIES.into()),
        asset_account: None,
        default_class: Some(ClassId("foam".into())),
        sales_price: None,
        purchase_cost: None,
        unit_cost: None,
        is_taxable: true,
        is_active: true,
        source_ref: None,
    }
}

fn line(
    no: i64,
    kind: LineKind,
    item_id: Option<&str>,
    qty: i64,
    minor: i64,
    taxable: bool,
) -> DocLine {
    DocLine {
        line_no: no,
        kind,
        amount: Money::from_minor(minor),
        class: None,
        item_id: item_id.map(String::from),
        account: None,
        is_taxable: taxable,
        qty: (qty > 0).then(|| Decimal::from(qty)),
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
        document_id: id.into(),
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

fn enter(ledger: &Ledger, document: LedgerDocument) {
    let (document, context) =
        books::prepare_document(ledger, CO, document, PostingConfig::default()).unwrap();
    let version = ledger
        .save_document(CO, &document, command(), Utc::now())
        .unwrap();
    if let Posting::Entry(entry) = post::post(&document, version.version, &context).unwrap() {
        ledger.post_entry(CO, &entry, Utc::now()).unwrap();
    }
}

fn main() {
    let mut logo = None;
    let mut config_path = None;
    let mut out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/render-sample");
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().expect("flag value");
        match flag.as_str() {
            "--logo" => logo = Some(PathBuf::from(value)),
            "--config" => config_path = Some(PathBuf::from(value)),
            "--out" => out = PathBuf::from(value),
            other => panic!("unknown flag {other}"),
        }
    }
    let mut config = match &config_path {
        Some(path) => RenderConfig::load(path).unwrap(),
        None => RenderConfig::parse(
            "[documents]\n\
             [documents.company.aquamentor]\n\
             name = \"Aquamentor\"\n\
             legal_name = \"Sample Company Name Inc\"\n\
             address = [\"100 Sample Street\", \"Sampletown, NJ 07000\"]\n\
             phone = \"(555) 010-0100\"\n\
             email = \"billing@example.com\"\n\
             footer = \"Thank you for your business.\"\n",
        )
        .unwrap(),
    };
    if let (Some(logo), Some(identity)) = (logo, config.company.get_mut(CO)) {
        identity.logo = Some(logo);
    }
    let dir = render::output_dir(&config, CO, Some(&out)).unwrap();

    let ledger = Ledger::open_in_memory().unwrap();
    let now = Utc.with_ymd_and_hms(2027, 4, 20, 12, 0, 0).unwrap();
    ledger.create_company(CO, "Aquamentor", None, now).unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &customer(
            "c-harbor",
            "Blue Harbor Swim Club",
            "Attn: Accounts Payable\n12 Harbor Road\nBayville, NJ 08721",
            Some("Pool Deck Receiving\n40 Shore Ave\nBayville, NJ 08721"),
        ),
        now,
    )
    .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &customer(
            "c-parks",
            "Lakeside Parks Dept",
            "9 Municipal Plaza\nLakeside, PA 19000",
            None,
        ),
        now,
    )
    .unwrap();
    books::upsert_contact(
        &ledger,
        CO,
        &Contact {
            kind: ContactKind::Vendor,
            default_class: None,
            ..customer(
                "v-foam",
                "Sample Foam Supply",
                "77 Industrial Way\nFoamville, OH 44000",
                None,
            )
        },
        now,
    )
    .unwrap();
    for (id, sku, description) in [
        (
            "pad",
            "AKP-1712",
            "Kneeling Pad - Medium 17.5x12x1.5, navy closed-cell foam",
        ),
        (
            "tube",
            "RT-40",
            "Rescue Tube 40in, red, with stainless clip and 6ft lanyard",
        ),
        ("mat", "SM-2448", "Swim Mat 24x48x1, anti-slip texture"),
    ] {
        books::upsert_item(&ledger, CO, &item(id, sku, description), now).unwrap();
    }

    let mut open = doc(
        "inv-open",
        DocKind::Invoice,
        ymd(4, 2),
        "c-harbor",
        vec![
            line(1, LineKind::Item, Some("pad"), 10, 15_750, true),
            line(2, LineKind::Item, Some("tube"), 24, 76_800, true),
            line(3, LineKind::Item, Some("mat"), 3, 26_985, true),
            line(4, LineKind::Shipping, None, 0, 4_850, false),
        ],
    );
    open.memo = Some("Customer PO 7781. Deliver to the pool deck entrance; call ahead.".into());
    enter(&ledger, open);
    let mut payment = doc(
        "pay-1",
        DocKind::Payment,
        ymd(4, 15),
        "c-harbor",
        Vec::new(),
    );
    payment.applications = vec![Application {
        target_document_id: "inv-open".into(),
        target_kind: DocKind::Invoice,
        amount: Money::from_minor(50_000),
    }];
    enter(&ledger, payment);

    enter(
        &ledger,
        doc(
            "inv-void",
            DocKind::Invoice,
            ymd(4, 3),
            "c-parks",
            vec![line(1, LineKind::Item, Some("mat"), 2, 17_990, true)],
        ),
    );
    books::void_document(&ledger, CO, "inv-void", "entered twice", command(), now).unwrap();

    enter(
        &ledger,
        doc(
            "inv-old",
            DocKind::Invoice,
            ymd(1, 10),
            "c-harbor",
            vec![line(1, LineKind::Item, Some("tube"), 6, 19_200, true)],
        ),
    );

    let mut estimate = doc(
        "est-1",
        DocKind::Estimate,
        ymd(4, 5),
        "c-parks",
        vec![
            line(1, LineKind::Item, Some("tube"), 12, 38_400, true),
            line(2, LineKind::Item, Some("pad"), 20, 31_500, true),
        ],
    );
    estimate.due_date = Some(ymd(5, 5));
    enter(&ledger, estimate);

    enter(
        &ledger,
        doc(
            "sr-1",
            DocKind::SalesReceipt,
            ymd(4, 6),
            "c-parks",
            vec![line(1, LineKind::Item, Some("pad"), 2, 3_150, true)],
        ),
    );

    let mut credit = doc(
        "cm-1",
        DocKind::CreditMemo,
        ymd(4, 8),
        "c-harbor",
        vec![line(1, LineKind::Item, Some("tube"), 1, 3_200, true)],
    );
    credit.memo = Some("One tube returned damaged.".into());
    enter(&ledger, credit);

    let mut po = doc(
        "po-1",
        DocKind::PurchaseOrder,
        ymd(4, 9),
        "v-foam",
        vec![
            line(1, LineKind::Item, Some("pad"), 200, 120_000, false),
            line(2, LineKind::Item, Some("mat"), 40, 98_000, false),
        ],
    );
    po.due_date = Some(ymd(4, 30));
    po.memo = Some("Ship complete; reference our PO number on the packing list.".into());
    enter(&ledger, po);

    let long: Vec<DocLine> = (1..=45)
        .map(|n| {
            let id = ["pad", "tube", "mat"][(n % 3) as usize];
            line(
                n,
                LineKind::Item,
                Some(id),
                n % 7 + 1,
                (n % 7 + 1) * 1_575,
                true,
            )
        })
        .collect();
    enter(
        &ledger,
        doc("inv-long", DocKind::Invoice, ymd(4, 12), "c-parks", long),
    );

    let mut mock = MockAuthnet::new();
    for id in [
        "inv-open", "inv-void", "est-1", "sr-1", "cm-1", "po-1", "inv-long",
    ] {
        let done = render::render_document(
            &ledger,
            CO,
            id,
            &config,
            &dir,
            PayLinkMode::Client(&mut mock),
            now,
        )
        .unwrap();
        println!(
            "{} ({} page{})",
            done.rendered.path.display(),
            done.rendered.page_count,
            if done.rendered.page_count == 1 {
                ""
            } else {
                "s"
            }
        );
        for warning in done.rendered.warnings {
            println!("  warning: {warning}");
        }
    }
    let statement =
        render::render_statement(&ledger, CO, "c-harbor", ymd(4, 20), &config, &dir).unwrap();
    println!(
        "{} ({} page{}, total due {})",
        statement.rendered.path.display(),
        statement.rendered.page_count,
        if statement.rendered.page_count == 1 {
            ""
        } else {
            "s"
        },
        render::usd(statement.aging.total)
    );
}
