//! Sales-channel ingest, end to end on synthetic fixtures
//! (`tests/fixtures/channel/`, made by `tools/gen-channel-fixtures.py`).
//! D26, `docs/CHANNEL.md`. Offline; every name, id and figure is invented.

use std::fs;
use std::path::PathBuf;

use chrono::NaiveDate;

use ledger::channel::amazon::{self, AmazonConfig};
use ledger::channel::report::{self, By};
use ledger::channel::shopify::{self, OrderBook, ShopifyConfig};
use ledger::channel::sku::map_rule;
use ledger::channel::stubs::{ChannelParser, EtsyParser, WalmartReconParser};
use ledger::channel::{Channel, ImportSummary};
use ledger::store::Ledger;

const COMPANY: &str = "aquamentor";

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/channel")
        .join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn fresh() -> Ledger {
    let l = Ledger::open_in_memory().unwrap();
    l.create_company(COMPANY, "Aquamentor LLC", None, chrono::Utc::now()).unwrap();
    l
}

fn shopify_import(l: &Ledger) -> ImportSummary {
    let book = OrderBook::parse(
        &fixture("shopify_orders.csv"),
        &fixture("shopify_order_lines.csv"),
        Some(&fixture("shopify_refunds.csv")),
    )
    .unwrap();
    shopify::import_payouts(
        l,
        COMPANY,
        &fixture("shopify_payouts.csv"),
        &fixture("shopify_payment_transactions.csv"),
        Some(&book),
        &ShopifyConfig::default(),
        None,
        None,
    )
    .unwrap()
}

fn recon(l: &Ledger, ch: &str) -> Vec<report::ReconRow> {
    report::channel_recon(l, COMPANY, ymd(2026, 1, 1), ymd(2026, 12, 31))
        .unwrap()
        .into_iter()
        .filter(|r| r.channel == ch)
        .collect()
}

fn amz(l: &Ledger, text: &str) -> ImportSummary {
    amazon::import_unified(l, COMPANY, text, &AmazonConfig::default(), None, None).unwrap()
}

fn tb_balance(l: &Ledger, number: &str) -> i64 {
    let tb = ledger::report::trial_balance(l, COMPANY, ymd(2026, 12, 31)).unwrap();
    tb.rows
        .iter()
        .find(|r| r.number == number)
        .map(|r| r.balance.minor())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- shopify

#[test]
fn shopify_payouts_post_and_clearing_nets_to_payouts() {
    let l = fresh();
    let s = shopify_import(&l);
    assert_eq!(s.posted, 2, "{}", s.render_text());
    assert_eq!(s.pending.len(), 1, "the in_transit payout waits");
    assert!(s.quarantined.is_empty() && s.rejected.is_empty());
    let r = recon(&l, "shopify");
    assert_eq!(r.len(), 1);
    let m = &r[0];
    assert_eq!(m.net, m.payouts, "every payout nets to its bank transfer");
    assert_eq!(m.clearing_balance, 0);
    // Revenue by family, after the order-level discount; refunds separate.
    assert_eq!(m.gross_product_sales, 79_995);
    assert_eq!(m.discounts, 1_390);
    assert_eq!(m.refunds, 4_314);
    assert_eq!(m.fees, 2_504);
    assert_eq!(m.adjustments, 14_010, "the chargeback, fee included");
    assert_eq!(m.payouts, 59_367);
}

#[test]
fn shopify_reimport_is_idempotent() {
    let l = fresh();
    shopify_import(&l);
    let before = tb_balance(&l, "1170");
    let again = shopify_import(&l);
    assert_eq!(again.posted, 0);
    assert_eq!(again.skipped, 2);
    assert_eq!(again.changed, 0);
    assert_eq!(recon(&l, "shopify")[0].payouts, 59_367);
    assert_eq!(tb_balance(&l, "1170"), before);
}

#[test]
fn shopify_gateway_amazon_orders_are_excluded() {
    let book = OrderBook::parse(
        &fixture("shopify_orders.csv"),
        &fixture("shopify_order_lines.csv"),
        None,
    )
    .unwrap();
    assert!(book.orders["#9004"].is_mirror(), "gateway amazon is a mirror");
    assert!(!book.orders["#9001"].is_mirror());
    let l = fresh();
    let s = shopify::import_orders(&l, COMPANY, &book, None, None).unwrap();
    assert!(s.excluded >= 1);
    let rows = report::channel_sales(&l, COMPANY, ymd(2026, 1, 1), ymd(2026, 12, 31), By::Sku, Some(Channel::Shopify)).unwrap();
    assert!(rows.iter().all(|r| r.sku.as_deref() != Some("XRT-40")), "the mirrored 40-inch tube never counts");
}

#[test]
fn shopify_uncaptured_orders_are_not_facts() {
    let book = OrderBook::parse(
        &fixture("shopify_orders.csv"),
        &fixture("shopify_order_lines.csv"),
        None,
    )
    .unwrap();
    assert!(book.orders["#9007"].is_uncaptured(), "voided");
    let l = fresh();
    shopify::import_orders(&l, COMPANY, &book, None, None).unwrap();
    let rows = report::channel_sales(&l, COMPANY, ymd(2026, 1, 1), ymd(2026, 12, 31), By::Family, None).unwrap();
    let units: i64 = rows.iter().map(|r| r.units).sum();
    assert_eq!(units, 2 + 1 + 1 + 1 + 1, "9001 (2 tubes, 1 kickboard), 9002, 9003, 9005; not 9004 or 9007");
}

#[test]
fn shopify_orders_import_is_idempotent() {
    let book = OrderBook::parse(
        &fixture("shopify_orders.csv"),
        &fixture("shopify_order_lines.csv"),
        Some(&fixture("shopify_refunds.csv")),
    )
    .unwrap();
    let l = fresh();
    let a = shopify::import_orders(&l, COMPANY, &book, None, None).unwrap();
    assert!(a.facts_inserted > 0);
    let b = shopify::import_orders(&l, COMPANY, &book, None, None).unwrap();
    assert_eq!(b.facts_inserted, 0);
    assert_eq!(b.facts_existing, a.facts_inserted);
}

#[test]
fn shopify_payout_that_does_not_tie_is_quarantined_not_plugged() {
    let l = fresh();
    // Break the first payout's stated total by a cent.
    let txt = fixture("shopify_payouts.csv");
    let first_total = txt.lines().nth(1).unwrap().split(',').nth(10).unwrap().to_string();
    let bumped = format!("{:.2}", first_total.parse::<f64>().unwrap() + 0.01);
    let payouts = txt.replacen(&first_total, &bumped, 1);
    let book = OrderBook::parse(
        &fixture("shopify_orders.csv"),
        &fixture("shopify_order_lines.csv"),
        Some(&fixture("shopify_refunds.csv")),
    )
    .unwrap();
    let s = shopify::import_payouts(
        &l,
        COMPANY,
        &payouts,
        &fixture("shopify_payment_transactions.csv"),
        Some(&book),
        &ShopifyConfig::default(),
        None,
        None,
    )
    .unwrap();
    assert!(!s.quarantined.is_empty(), "{}", s.render_text());
    assert_eq!(s.posted, 1, "the other payout still posts");
    // Nothing was invented to make it tie: the quarantined payout's
    // clearing activity is simply absent.
    let m = &recon(&l, "shopify")[0];
    assert_eq!(m.clearing_balance, 0);
}

// ----------------------------------------------------------------- amazon

#[test]
fn amazon_unified_posts_settlements_and_leaves_open_ones_in_clearing() {
    let l = fresh();
    let s = amz(&l, &fixture("amazon_unified.csv"));
    assert!(s.quarantined.is_empty(), "{}", s.render_text());
    assert!(s.rejected.is_empty());
    let m = &recon(&l, "amazon")[0];
    assert_eq!(m.gross_product_sales, 127_592);
    assert_eq!(m.refunds, 13_900);
    assert_eq!(m.shipping, 1_250);
    assert_eq!(m.discounts, 500);
    // Settlements 2 and 4 (170.00 and 55.24 net) are not paid inside the
    // file; that, and only that, is left in clearing.
    assert_eq!(m.net - m.payouts, 22_524);
    assert_eq!(m.clearing_balance, 22_524);
}

#[test]
fn amazon_facilitator_tax_is_excluded_and_asserted_zero() {
    let l = fresh();
    amz(&l, &fixture("amazon_unified.csv"));
    assert_eq!(tb_balance(&l, "2200"), 0, "facilitator tax never reaches sales tax payable");
    let parsed = amazon::parse_unified(&fixture("amazon_unified.csv")).unwrap();
    for s in &parsed.settlements {
        let tax: i64 = s.rows.iter().map(|r| r.tax).sum();
        assert_eq!(tax, 0, "settlement {} collected and withheld tax must cancel", s.id);
    }
}

#[test]
fn amazon_tax_that_does_not_net_is_quarantined() {
    let mut parsed = amazon::parse_unified(&fixture("amazon_unified.csv")).unwrap();
    let l = fresh();
    ledger::channel::db::ensure_setup(&l, COMPANY).unwrap();
    let mapper = ledger::channel::db::load_mapper(&l, COMPANY).unwrap();
    let classes = ledger::channel::db::family_classes(&l, COMPANY).unwrap();
    let s = &mut parsed.settlements[0];
    // One cent of withheld tax goes missing (the row's total moves with it).
    let row = s.rows.iter_mut().find(|r| r.order_id.starts_with("111-0000001")).unwrap();
    row.tax += 1;
    row.total += 1;
    let Err(err) = amazon::build_settlement(s, &mapper, &classes, &AmazonConfig::default()) else { panic!("built") };
    assert!(matches!(err, ledger::channel::ChannelError::TaxNotNetted { .. }), "{err}");
}

#[test]
fn amazon_unified_reimport_is_idempotent() {
    let l = fresh();
    let a = amz(&l, &fixture("amazon_unified.csv"));
    let b = amz(&l, &fixture("amazon_unified.csv"));
    assert_eq!(b.posted, 0);
    assert_eq!(b.skipped, a.posted);
    assert_eq!(b.changed, 0);
    assert_eq!(recon(&l, "amazon")[0].clearing_balance, 22_524);
}

#[test]
fn amazon_v2_and_unified_agree_on_the_same_settlement() {
    let v2 = amazon::parse_v2(&fixture("amazon_settlement_v2.txt")).unwrap();
    let uni = amazon::parse_unified(&fixture("amazon_unified.csv")).unwrap();
    let a = &v2[0];
    let b = uni.settlements.iter().find(|s| s.id == a.id).unwrap();
    assert_eq!(a.net(), b.net());
    assert_eq!(a.entry_date(), b.entry_date(), "UTC posted dates fold to Pacific: 9 March");
    let sum = |s: &amazon::Settlement, f: fn(&amazon::AmzRow) -> i64| s.rows.iter().map(f).sum::<i64>();
    assert_eq!(sum(a, |r| r.product), sum(b, |r| r.product));
    assert_eq!(sum(a, |r| r.shipping), sum(b, |r| r.shipping));
    assert_eq!(sum(a, |r| r.fee_selling), sum(b, |r| r.fee_selling));
    assert_eq!(sum(a, |r| r.tax), 0);

    // Same postings: importing V2 after unified changes nothing.
    let l = fresh();
    amz(&l, &fixture("amazon_unified.csv"));
    let s = amazon::import_v2(&l, COMPANY, &fixture("amazon_settlement_v2.txt"), &AmazonConfig::default(), None, None).unwrap();
    assert_eq!(s.changed, 0, "{}", s.render_text());
    assert!(s.quarantined.is_empty());
    assert_eq!(
        s.skipped + s.posted,
        2,
        "the settlement is skipped; only its payout may be new"
    );
    assert!(s.skipped >= 1, "{}", s.render_text());
}

#[test]
fn amazon_v2_stated_total_mismatch_is_quarantined() {
    let l = fresh();
    let text = fixture("amazon_settlement_v2.txt").replacen("775.80", "775.81", 1);
    let s = amazon::import_v2(&l, COMPANY, &text, &AmazonConfig::default(), None, None).unwrap();
    assert_eq!(s.quarantined.len(), 1, "{}", s.render_text());
    assert_eq!(s.posted, 0);
    assert!(recon(&l, "amazon").is_empty());
}

#[test]
fn amazon_classifies_non_order_rows() {
    use ledger::chart::*;
    let c = amazon::classify_other;
    assert_eq!(c("Service Fee", "Cost of Advertising").0, AMAZON_ADVERTISING);
    assert_eq!(c("Service Fee", "Subscription").0, AMAZON_SUBSCRIPTION);
    assert_eq!(c("Shipping Services", "Shipping Label Purchased through Amazon").0, CHANNEL_SHIPPING_LABELS);
    assert_eq!(c("FBA Inventory Fee", "FBA storage fee").0, AMAZON_FBA_FEES);
    assert_eq!(c("Adjustment", "FBA Inventory Reimbursement - Lost:Warehouse").0, AMAZON_ADJUSTMENTS);
}

// ------------------------------------------------------ sku / report / etc

#[test]
fn sku_mapper_covers_the_tube_families() {
    assert_eq!(map_rule("XRT-50", "Aquamentor 50\" ExoTube").family, "Rescue Tube 50\"");
    assert_eq!(map_rule("XRT-40RD", "").family, "Rescue Tube 40\"");
    let hoop = map_rule("ZZ-HOOP-1", "Splash Pool Basketball Hoop");
    assert!(hoop.is_flagged(), "unknown goods are Custom / Other and flagged");
}

#[test]
fn unmapped_skus_are_counted_and_listed() {
    let l = fresh();
    let s = amz(&l, &fixture("amazon_unified.csv"));
    assert_eq!(s.unmapped_skus, 1);
    let rows = ledger::channel::db::list_sku_map(&l, COMPANY, true).unwrap();
    assert!(rows.iter().any(|r| r.sku.eq_ignore_ascii_case("ZZ-HOOP-1")));
}

#[test]
fn report_totals_tie_to_the_postings_and_render() {
    let l = fresh();
    shopify_import(&l);
    amz(&l, &fixture("amazon_unified.csv"));
    let from = ymd(2026, 3, 1);
    let to = ymd(2026, 3, 31);
    let fam = report::channel_sales(&l, COMPANY, from, to, By::Family, None).unwrap();
    let sku = report::channel_sales(&l, COMPANY, from, to, By::Sku, None).unwrap();
    let t = |rows: &[report::SalesRow]| (rows.iter().map(|r| r.gross_minor).sum::<i64>(), rows.iter().map(|r| r.refunds_minor).sum::<i64>(), rows.iter().map(|r| r.units).sum::<i64>());
    assert_eq!(t(&fam), t(&sku), "family and sku views total alike");
    // Order facts are from the Amazon settlements and the payout-linked
    // Shopify orders; their product totals equal the journal's.
    let ship_only = report::channel_sales(&l, COMPANY, from, to, By::Family, Some(Channel::Amazon)).unwrap();
    assert_eq!(ship_only.iter().map(|r| r.gross_minor).sum::<i64>(), 127_592);
    let csv = report::sales_csv(&fam, By::Family);
    assert!(csv.lines().next().unwrap().starts_with("channel,month,family"));
    let json = report::sales_json(&fam, from, to, By::Family);
    assert!(json.is_object());
    // Outside the window: nothing.
    assert!(report::channel_sales(&l, COMPANY, ymd(2027, 1, 1), ymd(2027, 1, 31), By::Family, None).unwrap().is_empty());
}

#[test]
fn walmart_and_etsy_are_honest_stubs() {
    assert!(matches!(WalmartReconParser.parse("x"), Err(ledger::channel::ChannelError::NotImplemented(_))));
    assert!(matches!(EtsyParser.parse("x"), Err(ledger::channel::ChannelError::NotImplemented(_))));
}

#[test]
fn channel_accounts_are_in_the_seed_chart() {
    let l = fresh();
    for n in ["1170", "1171", "1172", "1173", "1175", "1176", "4985", "6610", "6611", "6612", "6613", "6614", "6615", "6616", "6617", "6620", "6621"] {
        let tb = ledger::report::trial_balance(&l, COMPANY, ymd(2026, 12, 31)).unwrap();
        assert!(tb.rows.iter().any(|r| r.number == n), "account {n} missing from the seed chart");
    }
}

// -------------------------------------------------------------------- cli

fn cli(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(args)
        .output()
        .expect("run ledger")
}

#[test]
fn cli_imports_reports_and_reimports_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.sqlite");
    let db = db.to_str().unwrap();
    let fx = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/channel");
    let f = |n: &str| fx.join(n).to_str().unwrap().to_string();
    let init = cli(&["init", "--db", db, "--company", COMPANY, "--name", "Aquamentor LLC"]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    let run = |a: &[&str]| cli(a);
    let p = f("shopify_payouts.csv");
    let t = f("shopify_payment_transactions.csv");
    let o = f("shopify_orders.csv");
    let l = f("shopify_order_lines.csv");
    let r = f("shopify_refunds.csv");
    let args = ["import", "shopify-payouts", "--db", db, "--company", COMPANY, "--payouts", &p,
        "--transactions", &t, "--orders", &o, "--lines", &l, "--refunds", &r];
    let first = run(&args);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let second = run(&args);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("posted            : 0")
        || String::from_utf8_lossy(&second.stdout).contains("postings posted    : 0"),
        "{}", String::from_utf8_lossy(&second.stdout));

    let a = f("amazon_unified.csv");
    let amz = cli(&["import", "amazon-unified", "--db", db, "--company", COMPANY, "--file", &a]);
    assert!(amz.status.success(), "{}", String::from_utf8_lossy(&amz.stderr));

    let rep = cli(&["report", "channel-sales", "--db", db, "--company", COMPANY, "--from", "2026-03-01",
        "--to", "2026-03-31", "--by", "family", "--format", "json"]);
    assert!(rep.status.success(), "{}", String::from_utf8_lossy(&rep.stderr));
    let v: serde_json::Value = serde_json::from_slice(&rep.stdout).unwrap();
    assert!(v.is_object());
    let csv = cli(&["report", "channel-sales", "--db", db, "--company", COMPANY, "--from", "2026-03-01",
        "--to", "2026-03-31", "--channel", "amazon", "--format", "csv"]);
    assert!(csv.status.success());
    assert!(String::from_utf8_lossy(&csv.stdout).starts_with("channel,month"));

    // Stubs refuse loudly.
    let w = cli(&["import", "walmart-recon", "--db", db, "--company", COMPANY, "--file", &a]);
    assert!(!w.status.success());
}
