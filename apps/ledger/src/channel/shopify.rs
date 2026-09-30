//! Shopify Payments: payouts, payment transactions, orders and lines.
//! `CHANNEL_POSTING_MAP.md`, principles 1 to 3 and 6.
//!
//! Inputs, all exported by hand:
//!
//! - `payouts_export_1.csv` (Payments -> Payouts -> Export): one row per
//!   payout with a `Total`, but **no payout id**.
//! - `payment_transactions_export_1.csv`: one row per charge, refund, fee
//!   bearing dispute, reserve movement, with `Payout ID`, `Payout Date`,
//!   `Amount`, `Fee`, `Net`.
//! - `shopify_orders_*.csv`, `shopify_order_lines_*.csv`,
//!   `shopify_refunds_*.csv` (Admin GraphQL exports): what each charge was
//!   for. Optional for the payout import, required for the family split.
//!
//! The export carries no payout id in the payouts file, so payouts are
//! grouped from the transactions by (`Payout ID`, `Payout Date`) and paired
//! to the payouts file by date and total. A group whose total is not in the
//! payouts file, or a payouts row with no transactions behind it, is
//! quarantined.
//!
//! A payout posts: gross charges (Cr revenue by family, shipping, tax
//! liability, Dr order-level discount) into Shopify clearing; refunds,
//! disputes, marketplace tax, reserve movement and fees out of it; then the
//! transfer to checking. Clearing is asserted to net to the transaction nets
//! and to the payouts file's total *before* the transfer, and to zero after.

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, Utc};
use ledger_core::Money;

use super::csvx::{
    cell, parse_iso_date, parse_money, parse_qty, parse_records, parse_utc, us_local_date, Table,
};
use super::db::{self, Fact};
use super::sku::{self, SkuMapper};
use super::{
    allocate, class_weights, fmt_money, Channel, ChannelError, ChannelPosting, ImportSummary,
    Legs, PostingKind,
};
use crate::chart;
use crate::store::Ledger;

pub const EASTERN_STD_OFFSET: i64 = -5;

#[derive(Clone, Debug)]
pub struct PayoutRow {
    pub date: NaiveDate,
    pub status: String,
    pub total: Money,
}

#[derive(Clone, Debug)]
pub struct TxnRow {
    pub kind: String,
    pub order: String,
    pub payout_status: String,
    pub payout_date: NaiveDate,
    pub payout_id: String,
    pub amount: Money,
    pub fee: Money,
    pub net: Money,
}

pub fn parse_payouts(text: &str) -> Result<Vec<PayoutRow>, ChannelError> {
    let table = Table::from_records(parse_records(text, ','), 0)?;
    let (date, status, total) = (
        table.need("Payout Date")?,
        table.need("Status")?,
        table.need("Total")?,
    );
    table
        .rows
        .iter()
        .map(|r| {
            Ok(PayoutRow {
                date: parse_iso_date(cell(r, date))?,
                status: cell(r, status).to_string(),
                total: parse_money(cell(r, total))?,
            })
        })
        .collect()
}

pub fn parse_transactions(text: &str) -> Result<Vec<TxnRow>, ChannelError> {
    let table = Table::from_records(parse_records(text, ','), 0)?;
    let (kind, order, pstatus, pdate, pid, amount, fee, net) = (
        table.need("Type")?,
        table.need("Order")?,
        table.need("Payout Status")?,
        table.need("Payout Date")?,
        table.need("Payout ID")?,
        table.need("Amount")?,
        table.need("Fee")?,
        table.need("Net")?,
    );
    table
        .rows
        .iter()
        .map(|r| {
            Ok(TxnRow {
                kind: cell(r, kind).to_lowercase(),
                order: cell(r, order).to_string(),
                payout_status: cell(r, pstatus).to_lowercase(),
                payout_date: parse_iso_date(cell(r, pdate))?,
                payout_id: cell(r, pid).to_string(),
                amount: parse_money(cell(r, amount))?,
                fee: parse_money(cell(r, fee))?,
                net: parse_money(cell(r, net))?,
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Order {
    pub name: String,
    pub date: NaiveDate,
    pub gateways: Vec<String>,
    pub financial_status: String,
    pub shipping: i64,
    pub tax: i64,
    pub total: i64,
}

impl Order {
    /// Marketplace Connect mirrors (posting map, principle 6): the Amazon or
    /// eBay side is the source of truth, so a Shopify order paid through
    /// either gateway is never Shopify revenue.
    pub fn is_mirror(&self) -> bool {
        self.gateways
            .iter()
            .any(|g| g == "amazon" || g == "ebay")
    }

    /// No money was captured, so there is nothing to count.
    pub fn is_uncaptured(&self) -> bool {
        matches!(
            self.financial_status.as_str(),
            "VOIDED" | "EXPIRED" | "AUTHORIZED" | "PENDING"
        )
    }
}

#[derive(Clone, Debug)]
pub struct Line {
    pub sku: String,
    pub title: String,
    pub qty: i64,
    pub amount: i64,
}

#[derive(Clone, Debug)]
pub struct Refund {
    pub order: String,
    pub date: NaiveDate,
    pub total: i64,
}

#[derive(Default, Debug)]
pub struct OrderBook {
    pub orders: HashMap<String, Order>,
    pub lines: HashMap<String, Vec<Line>>,
    pub refunds: Vec<Refund>,
}

impl OrderBook {
    pub fn parse(
        orders_text: &str,
        lines_text: &str,
        refunds_text: Option<&str>,
    ) -> Result<OrderBook, ChannelError> {
        let mut book = OrderBook::default();

        let orders = Table::from_records(parse_records(orders_text, ','), 0)?;
        let (name, created, status, gateways, shipping, tax, total) = (
            orders.need("name")?,
            orders.need("created_at")?,
            orders.need("financial_status")?,
            orders.need("gateways")?,
            orders.need("shipping")?,
            orders.need("tax")?,
            orders.need("total")?,
        );
        for r in &orders.rows {
            let created_utc = parse_utc(cell(r, created))?;
            let order = Order {
                name: cell(r, name).to_string(),
                date: us_local_date(created_utc, EASTERN_STD_OFFSET),
                gateways: cell(r, gateways)
                    .split('|')
                    .map(|g| g.trim().to_lowercase())
                    .collect(),
                financial_status: cell(r, status).to_uppercase(),
                shipping: parse_money(cell(r, shipping))?.minor(),
                tax: parse_money(cell(r, tax))?.minor(),
                total: parse_money(cell(r, total))?.minor(),
            };
            book.orders.insert(order.name.clone(), order);
        }

        let lines = Table::from_records(parse_records(lines_text, ','), 0)?;
        let (oname, sku_c, title, qty, amount) = (
            lines.need("order_name")?,
            lines.need("sku")?,
            lines.need("product_title")?,
            lines.need("quantity")?,
            lines.need("discounted_total")?,
        );
        for r in &lines.rows {
            book.lines
                .entry(cell(r, oname).to_string())
                .or_default()
                .push(Line {
                    sku: cell(r, sku_c).to_string(),
                    title: cell(r, title).to_string(),
                    qty: parse_qty(cell(r, qty))?,
                    amount: parse_money(cell(r, amount))?.minor(),
                });
        }

        if let Some(text) = refunds_text {
            let refunds = Table::from_records(parse_records(text, ','), 0)?;
            let (rname, rdate, rtotal) = (
                refunds.need("order_name")?,
                refunds.need("refund_created_at")?,
                refunds.need("refund_total")?,
            );
            for r in &refunds.rows {
                book.refunds.push(Refund {
                    order: cell(r, rname).to_string(),
                    date: us_local_date(parse_utc(cell(r, rdate))?, EASTERN_STD_OFFSET),
                    total: parse_money(cell(r, rtotal))?.minor(),
                });
            }
        }
        Ok(book)
    }
}

/// One order line after family mapping.
#[derive(Clone, Debug)]
pub struct ViewLine {
    pub sku: String,
    pub title: String,
    pub family: &'static str,
    pub flagged: bool,
    pub qty: i64,
    pub amount: i64,
}

/// An order as the poster sees it.
#[derive(Clone, Debug)]
pub struct OrderView {
    pub lines: Vec<ViewLine>,
    pub shipping: i64,
    pub tax: i64,
    pub total: i64,
    /// Order-level discount not already inside the lines'
    /// `discounted_total`: `lines + shipping + tax - total`. Negative means
    /// the lines do not account for the order total (an edited order); such
    /// an order cannot split a charge exactly, only prorate one.
    pub discount: i64,
}

impl OrderView {
    /// Lines, shipping and tax add up to the order total (less a
    /// non-negative order-level discount).
    pub fn is_consistent(&self) -> bool {
        self.discount >= 0
    }

    fn family_totals(&self) -> BTreeMap<&'static str, i64> {
        let mut m = BTreeMap::new();
        for l in &self.lines {
            *m.entry(l.family).or_insert(0) += l.amount;
        }
        m
    }
}

impl OrderBook {
    /// The order as a set of family-mapped lines, or why it cannot be used to
    /// split a charge.
    pub fn view(&self, name: &str, mapper: &SkuMapper) -> Result<OrderView, String> {
        let order = self
            .orders
            .get(name)
            .ok_or_else(|| "order not in the orders export".to_string())?;
        if order.is_mirror() {
            return Err("order is a marketplace mirror (gateway amazon/ebay)".into());
        }
        let raw = self
            .lines
            .get(name)
            .ok_or_else(|| "order has no lines in the lines export".to_string())?;
        let lines: Vec<ViewLine> = raw
            .iter()
            .map(|l| {
                let m = mapper.resolve("shopify", &l.sku, &l.title);
                ViewLine {
                    sku: l.sku.clone(),
                    title: l.title.clone(),
                    family: m.family,
                    flagged: m.is_flagged(),
                    qty: l.qty,
                    amount: l.amount,
                }
            })
            .collect();
        let lines_sum: i64 = lines.iter().map(|l| l.amount).sum();
        let discount = lines_sum + order.shipping + order.tax - order.total;
        Ok(OrderView {
            lines,
            shipping: order.shipping,
            tax: order.tax,
            total: order.total,
            discount,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ShopifyConfig {
    /// Class for revenue that could not be split by family and for costs
    /// with no revenue to follow.
    pub fallback_class: String,
    pub bank: String,
}

impl Default for ShopifyConfig {
    fn default() -> Self {
        ShopifyConfig {
            fallback_class: "foam".to_string(),
            bank: chart::CHECKING.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum PayoutOutcome {
    Posting(Box<ChannelPosting>),
    /// In transit: not posted until it lands.
    Pending(String),
    Quarantine {
        external_id: String,
        reason: &'static str,
        detail: String,
    },
}

struct Group<'a> {
    payout_id: String,
    date: NaiveDate,
    status: String,
    txns: Vec<&'a TxnRow>,
}

fn family_class<'a>(classes: &'a HashMap<String, String>, family: &str) -> &'a str {
    classes
        .get(family)
        .map(String::as_str)
        .unwrap_or_else(|| sku::default_class(family))
}

fn class_of_order_families(
    view: &OrderView,
    classes: &HashMap<String, String>,
) -> Vec<(String, i64)> {
    let mut by_class: BTreeMap<String, i64> = BTreeMap::new();
    for (family, amount) in view.family_totals() {
        *by_class
            .entry(family_class(classes, family).to_string())
            .or_insert(0) += amount.max(0);
    }
    by_class.into_iter().collect()
}

/// Books a charge equal to the order total: revenue by family, shipping,
/// tax liability, and the order-level discount as a debit.
fn charge_legs(
    legs: &mut Legs,
    view: &OrderView,
    classes: &HashMap<String, String>,
    rev_by_class: &mut BTreeMap<String, i64>,
    fallback: &str,
) {
    for (family, amount) in view.family_totals() {
        let class = family_class(classes, family);
        legs.signed_credit(chart::SALES_INCOME, Some(class), amount);
        *rev_by_class.entry(class.to_string()).or_insert(0) += amount;
    }
    let weights = class_of_order_families(view, classes);
    let weights = if weights.iter().all(|(_, w)| *w == 0) {
        vec![(fallback.to_string(), 1)]
    } else {
        weights
    };
    for (class, part) in allocate(view.shipping, &weights) {
        legs.signed_credit(chart::SHIPPING_INCOME, Some(&class), part);
    }
    legs.signed_credit(chart::SALES_TAX_PAYABLE, None, view.tax);
    for (class, part) in allocate(view.discount, &weights) {
        legs.signed_debit(chart::DISCOUNTS_GIVEN, Some(&class), part);
    }
}

/// Books a charge that is one of several payments against the same order
/// (deposit and balance, split tender): the charge is spread across the
/// order's lines, shipping and tax by what each contributed, so the parts add
/// back to the charge exactly. The order-level discount is not shown
/// separately here; it is netted into the spread.
fn prorated_charge_legs(
    legs: &mut Legs,
    view: &OrderView,
    amount: i64,
    classes: &HashMap<String, String>,
    rev_by_class: &mut BTreeMap<String, i64>,
    fallback: &str,
) {
    let mut weights: Vec<(Comp, i64)> = view
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| (Comp::Line(i), l.amount))
        .collect();
    weights.push((Comp::Shipping, view.shipping));
    weights.push((Comp::Tax, view.tax));
    let mut ship_weights = class_of_order_families(view, classes);
    if ship_weights.iter().all(|(_, w)| *w == 0) {
        ship_weights = vec![(fallback.to_string(), 1)];
    }
    let mut by_class: BTreeMap<String, i64> = BTreeMap::new();
    for (comp, part) in allocate(amount, &weights) {
        match comp {
            Comp::Line(i) => {
                let class = family_class(classes, view.lines[i].family).to_string();
                *by_class.entry(class.clone()).or_insert(0) += part;
                *rev_by_class.entry(class).or_insert(0) += part;
            }
            Comp::Shipping => {
                for (class, p) in allocate(part, &ship_weights) {
                    legs.signed_credit(chart::SHIPPING_INCOME, Some(&class), p);
                }
            }
            Comp::Tax => legs.signed_credit(chart::SALES_TAX_PAYABLE, None, part),
        }
    }
    for (class, part) in by_class {
        legs.signed_credit(chart::SALES_INCOME, Some(&class), part);
    }
}

/// A component of an order a refund is spread across.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comp {
    Line(usize),
    Shipping,
    Tax,
}

/// Spreads `amount` (positive) across the order's lines, shipping and tax in
/// proportion to what each contributed to the order.
pub fn refund_components(view: &OrderView, amount: i64) -> Vec<(Comp, i64)> {
    let mut weights: Vec<(Comp, i64)> = view
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| (Comp::Line(i), l.amount))
        .collect();
    weights.push((Comp::Shipping, view.shipping));
    weights.push((Comp::Tax, view.tax));
    allocate(amount, &weights)
}

fn refund_legs(
    legs: &mut Legs,
    view: &OrderView,
    refund: i64,
    classes: &HashMap<String, String>,
    fallback: &str,
) {
    let mut family_ship_weights: Vec<(String, i64)> = class_of_order_families(view, classes);
    if family_ship_weights.iter().all(|(_, w)| *w == 0) {
        family_ship_weights = vec![(fallback.to_string(), 1)];
    }
    for (comp, part) in refund_components(view, refund) {
        match comp {
            Comp::Line(i) => {
                let class = family_class(classes, view.lines[i].family);
                legs.signed_debit(chart::RETURNS_ALLOWANCES, Some(class), part);
            }
            Comp::Shipping => {
                for (class, p) in allocate(part, &family_ship_weights) {
                    legs.signed_debit(chart::SHIPPING_INCOME, Some(&class), p);
                }
            }
            Comp::Tax => legs.signed_debit(chart::SALES_TAX_PAYABLE, None, part),
        }
    }
}

/// Pure: turns transactions (and, when given, orders) into per-payout
/// outcomes. `from`/`to` filter on payout date.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub fn build_payouts(
    txns: &[TxnRow],
    payouts: &[PayoutRow],
    book: Option<&OrderBook>,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &ShopifyConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Vec<PayoutOutcome> {
    let in_range = |d: NaiveDate| from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t);

    // What each order was charged in total, across every payout in the file
    // (not just the ones in range): the test for a split payment.
    let mut charged_total: HashMap<String, i64> = HashMap::new();
    for t in txns.iter().filter(|t| t.kind == "charge") {
        *charged_total.entry(t.order.clone()).or_insert(0) += t.amount.minor();
    }

    // Group by (payout id, payout date), in first-seen order.
    let mut order: Vec<(String, NaiveDate)> = Vec::new();
    let mut groups: HashMap<(String, NaiveDate), Group> = HashMap::new();
    for t in txns.iter().filter(|t| in_range(t.payout_date)) {
        let key = (t.payout_id.clone(), t.payout_date);
        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                Group {
                    payout_id: t.payout_id.clone(),
                    date: t.payout_date,
                    status: t.payout_status.clone(),
                    txns: Vec::new(),
                }
            })
            .txns
            .push(t);
    }

    // Pair groups to payouts-file rows by date and total.
    let mut export_left: HashMap<NaiveDate, Vec<i64>> = HashMap::new();
    for p in payouts.iter().filter(|p| in_range(p.date)) {
        export_left.entry(p.date).or_default().push(p.total.minor());
    }

    let mut outcomes = Vec::new();
    for key in &order {
        let g = &groups[key];
        let external_id = format!("shopify:{}", g.payout_id);
        let net_sum: i64 = g.txns.iter().map(|t| t.net.minor()).sum();

        // A payout that is not posted still accounts for its payouts-file row.
        let consume = |left: &mut HashMap<NaiveDate, Vec<i64>>| {
            if let Some(v) = left.get_mut(&g.date) {
                if let Some(i) = v.iter().position(|t| *t == net_sum) {
                    v.remove(i);
                }
            }
        };
        if g.status == "in_transit" || g.status == "scheduled" {
            consume(&mut export_left);
            outcomes.push(PayoutOutcome::Pending(external_id));
            continue;
        }
        if g.status != "paid" {
            consume(&mut export_left);
            outcomes.push(PayoutOutcome::Quarantine {
                external_id,
                reason: "payout_status",
                detail: format!("status {:?}: money did not reach the bank as exported", g.status),
            });
            continue;
        }
        if g.txns.iter().any(|t| t.kind == "transfer_failure") {
            consume(&mut export_left);
            outcomes.push(PayoutOutcome::Quarantine {
                external_id,
                reason: "transfer_failure",
                detail: "payout carries a failed-transfer retry; needs a person".into(),
            });
            continue;
        }

        let export_total = if payouts.is_empty() {
            None
        } else {
            let slot = export_left.get_mut(&g.date);
            match slot.and_then(|v| v.iter().position(|t| *t == net_sum).map(|i| v.remove(i))) {
                Some(t) => Some(t),
                None => {
                    outcomes.push(PayoutOutcome::Quarantine {
                        external_id,
                        reason: "payout_total_not_in_export",
                        detail: format!(
                            "transactions net {} but no payouts row on {} has that total",
                            fmt_money(Money::from_minor(net_sum)),
                            g.date
                        ),
                    });
                    continue;
                }
            }
        };

        match build_one(
            g,
            &external_id,
            net_sum,
            export_total,
            book,
            mapper,
            classes,
            cfg,
            &charged_total,
        ) {
            Ok(p) => outcomes.push(PayoutOutcome::Posting(Box::new(p))),
            Err((reason, detail)) => outcomes.push(PayoutOutcome::Quarantine {
                external_id,
                reason,
                detail,
            }),
        }
    }

    // Payouts-file rows nothing accounted for.
    let mut dates: Vec<_> = export_left.keys().copied().collect();
    dates.sort();
    for d in dates {
        for total in &export_left[&d] {
            outcomes.push(PayoutOutcome::Quarantine {
                external_id: format!("shopify:export:{d}:{total}"),
                reason: "payout_without_transactions",
                detail: format!(
                    "payouts file has a {} payout on {d} with no transaction detail behind it",
                    fmt_money(Money::from_minor(*total))
                ),
            });
        }
    }
    outcomes
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_one(
    g: &Group<'_>,
    external_id: &str,
    net_sum: i64,
    export_total: Option<i64>,
    book: Option<&OrderBook>,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &ShopifyConfig,
    charged_total: &HashMap<String, i64>,
) -> Result<ChannelPosting, (&'static str, String)> {
    let clearing = Channel::Shopify.clearing();
    let fallback = cfg.fallback_class.as_str();
    let mut legs = Legs::default();
    let mut flags: Vec<String> = Vec::new();
    let mut rev_by_class: BTreeMap<String, i64> = BTreeMap::new();

    let mut charges = 0i64;
    let mut charge_fees = 0i64;
    let mut refunds = 0i64; // negative
    let mut dispute_net = 0i64; // amount - fee, negative is a loss
    let mut tax_amt = 0i64;
    let mut reserve = 0i64;
    let mut prorated = 0usize;

    for t in &g.txns {
        if t.amount.minor() - t.fee.minor() != t.net.minor() {
            return Err((
                "row_arithmetic",
                format!(
                    "{} {}: amount {} - fee {} is not net {}",
                    t.kind,
                    t.order,
                    fmt_money(t.amount),
                    fmt_money(t.fee),
                    fmt_money(t.net)
                ),
            ));
        }
        match t.kind.as_str() {
            "charge" => {
                let amount = t.amount.minor();
                charges += amount;
                charge_fees += t.fee.minor();
                let view = match book {
                    Some(b) => b.view(&t.order, mapper),
                    None => Err("no orders export supplied".to_string()),
                };
                match view {
                    Ok(v) if v.is_consistent() && v.total == amount => {
                        charge_legs(&mut legs, &v, classes, &mut rev_by_class, fallback);
                    }
                    Ok(v) if charged_total.get(&t.order) == Some(&v.total) => {
                        prorated += 1;
                        prorated_charge_legs(&mut legs, &v, amount, classes, &mut rev_by_class, fallback);
                    }
                    Ok(v) => {
                        flags.push(format!(
                            "charge {} of {} is not the order total {} and the order's charges do not add up to it: booked to unallocated",
                            t.order,
                            fmt_money(t.amount),
                            fmt_money(Money::from_minor(v.total))
                        ));
                        legs.signed_credit(chart::CHANNEL_UNALLOCATED, Some(fallback), amount);
                    }
                    Err(why) => {
                        flags.push(format!(
                            "charge {} of {} unallocated: {why}",
                            t.order,
                            fmt_money(t.amount)
                        ));
                        legs.signed_credit(chart::CHANNEL_UNALLOCATED, Some(fallback), amount);
                    }
                }
            }
            "refund" => {
                let amount = t.amount.minor();
                refunds += amount;
                charge_fees += t.fee.minor();
                let view = book.map(|b| b.view(&t.order, mapper));
                match view {
                    Some(Ok(v)) => refund_legs(&mut legs, &v, -amount, classes, fallback),
                    other => {
                        let why = match other {
                            Some(Err(w)) => w,
                            _ => "no orders export supplied".to_string(),
                        };
                        flags.push(format!(
                            "refund {} of {} unallocated: {why}",
                            t.order,
                            fmt_money(t.amount)
                        ));
                        legs.signed_debit(chart::CHANNEL_UNALLOCATED, Some(fallback), -amount);
                    }
                }
            }
            "chargeback" | "chargeback won" | "adjustment" => {
                dispute_net += t.net.minor();
            }
            "marketplace sales tax" => {
                tax_amt += t.amount.minor();
                charge_fees += t.fee.minor();
            }
            "reserved_funds" => {
                reserve += t.amount.minor();
                charge_fees += t.fee.minor();
            }
            other => {
                return Err((
                    "unknown_transaction_type",
                    format!("{other:?} on order {:?}", t.order),
                ));
            }
        }
    }

    // Costs with no product of their own follow the revenue this payout
    // booked; with none, the fallback class.
    let weights = class_weights(&rev_by_class, fallback);
    for (class, part) in allocate(charge_fees, &weights) {
        legs.signed_debit(chart::SHOPIFY_FEES, Some(&class), part);
    }
    for (class, part) in allocate(-dispute_net, &weights) {
        legs.signed_debit(chart::CHANNEL_ADJUSTMENTS, Some(&class), part);
    }
    legs.signed_credit(chart::SALES_TAX_PAYABLE, None, tax_amt);
    legs.signed_debit(chart::SHOPIFY_RESERVE, None, -reserve);

    // Clearing: every category's own net, no balancing figure.
    legs.signed_debit(clearing, None, charges);
    legs.signed_debit(clearing, None, refunds);
    legs.signed_debit(clearing, None, -charge_fees);
    legs.signed_debit(clearing, None, dispute_net);
    legs.signed_debit(clearing, None, tax_amt);
    legs.signed_debit(clearing, None, reserve);

    let cleared = legs.net_debit(clearing);
    if cleared != net_sum || export_total.is_some_and(|t| t != cleared) {
        return Err((
            "clearing_mismatch",
            format!(
                "clearing nets to {} but transactions net {} and the payouts file says {}",
                fmt_money(Money::from_minor(cleared)),
                fmt_money(Money::from_minor(net_sum)),
                export_total.map_or("(no payouts file)".into(), |t| fmt_money(Money::from_minor(t)))
            ),
        ));
    }

    legs.signed_debit(&cfg.bank, None, cleared);
    legs.signed_credit(clearing, None, cleared);
    if legs.net_debit(clearing) != 0 {
        return Err(("clearing_mismatch", "clearing does not net to zero after transfer".into()));
    }

    let posting = ChannelPosting {
        channel: Channel::Shopify,
        external_id: external_id.to_string(),
        kind: PostingKind::Payout,
        entry_date: g.date,
        memo: if prorated > 0 {
            format!(
                "Shopify payout {} ({}); {prorated} split-payment charge(s) prorated across their orders",
                g.payout_id, g.date
            )
        } else {
            format!("Shopify payout {} ({})", g.payout_id, g.date)
        },
        legs: legs.legs,
        gross: Money::from_minor(charges),
        refunds: Money::from_minor(-refunds),
        fees: Money::from_minor(charge_fees),
        other: Money::from_minor(dispute_net + tax_amt + reserve),
        net: Money::from_minor(cleared),
        payout: Money::from_minor(cleared),
        flags,
    };
    if !posting.is_balanced() {
        return Err(("unbalanced", "legs do not balance".into()));
    }
    Ok(posting)
}

/// Reads the exports, builds the payouts and commits each.
#[allow(clippy::too_many_arguments)]
pub fn import_payouts(
    ledger: &Ledger,
    company: &str,
    payouts_text: &str,
    txns_text: &str,
    book: Option<&OrderBook>,
    cfg: &ShopifyConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let payouts = parse_payouts(payouts_text)?;
    let txns = parse_transactions(txns_text)?;
    let mapper = db::load_mapper(ledger, company)?;
    let classes = db::family_classes(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();

    for outcome in build_payouts(&txns, &payouts, book, &mapper, &classes, cfg, from, to) {
        match outcome {
            PayoutOutcome::Posting(p) => {
                super::commit_into(ledger, company, &p, &mut summary, now);
                db::clear_quarantine(ledger, company, &p.external_id)?;
            }
            PayoutOutcome::Pending(id) => summary.pending.push(id),
            PayoutOutcome::Quarantine {
                external_id,
                reason,
                detail,
            } => super::quarantine_into(
                ledger,
                company,
                Channel::Shopify,
                &external_id,
                reason,
                &detail,
                &mut summary,
                now,
            ),
        }
    }
    Ok(summary)
}

/// Line-level facts from the orders exports, mirrors excluded.
pub fn import_orders(
    ledger: &Ledger,
    company: &str,
    book: &OrderBook,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let mapper = db::load_mapper(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();
    let in_range = |d: NaiveDate| from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t);

    let mut names: Vec<&String> = book.orders.keys().collect();
    names.sort();
    let mut unmapped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for name in names {
        let order = &book.orders[name];
        if order.is_mirror() {
            summary.excluded += 1;
            continue;
        }
        if order.is_uncaptured() {
            continue;
        }
        let Some(raw) = book.lines.get(name) else {
            continue;
        };
        if !in_range(order.date) {
            continue;
        }
        for (i, l) in raw.iter().enumerate() {
            let m = mapper.resolve("shopify", &l.sku, &l.title);
            db::remember_sku(ledger, company, Channel::Shopify, &l.sku, &l.title, m, now)?;
            if m.is_flagged() {
                unmapped.insert(format!("{}|{}", l.sku, l.title));
            }
            let fact = Fact {
                channel: Channel::Shopify,
                order_id: name.clone(),
                kind: "sale",
                line_key: (i + 1).to_string(),
                order_date: order.date,
                sku: l.sku.clone(),
                title: l.title.clone(),
                family: m.family.to_string(),
                qty: l.qty,
                amount_minor: l.amount,
            };
            if db::insert_fact(ledger, company, &fact)? {
                summary.facts_inserted += 1;
            } else {
                summary.facts_existing += 1;
            }
        }
    }

    // Refunds, spread across the order's lines by what each contributed.
    let mut per_order_seen: HashMap<String, usize> = HashMap::new();
    for r in &book.refunds {
        let n = per_order_seen.entry(r.order.clone()).or_insert(0);
        *n += 1;
        let ordinal = *n;
        let Some(order) = book.orders.get(&r.order) else {
            continue;
        };
        if order.is_mirror() || !in_range(r.date) {
            continue;
        }
        let Ok(view) = book.view(&r.order, &mapper) else {
            // Lines that do not add up to the order still carry a usable
            // product split for a refund: fall back to the raw lines.
            if let Some(raw) = book.lines.get(&r.order) {
                let weights: Vec<(usize, i64)> =
                    raw.iter().enumerate().map(|(i, l)| (i, l.amount)).collect();
                for (i, part) in allocate(r.total, &weights) {
                    let l = &raw[i];
                    let m = mapper.resolve("shopify", &l.sku, &l.title);
                    let fact = Fact {
                        channel: Channel::Shopify,
                        order_id: r.order.clone(),
                        kind: "refund",
                        line_key: format!("{ordinal}:{}", i + 1),
                        order_date: r.date,
                        sku: l.sku.clone(),
                        title: l.title.clone(),
                        family: m.family.to_string(),
                        qty: 0,
                        amount_minor: -part,
                    };
                    if db::insert_fact(ledger, company, &fact)? {
                        summary.facts_inserted += 1;
                    } else {
                        summary.facts_existing += 1;
                    }
                }
            }
            continue;
        };
        for (comp, part) in refund_components(&view, r.total) {
            let Comp::Line(i) = comp else { continue };
            let l = &view.lines[i];
            let fact = Fact {
                channel: Channel::Shopify,
                order_id: r.order.clone(),
                kind: "refund",
                line_key: format!("{ordinal}:{}", i + 1),
                order_date: r.date,
                sku: l.sku.clone(),
                title: l.title.clone(),
                family: l.family.to_string(),
                qty: 0,
                amount_minor: -part,
            };
            if db::insert_fact(ledger, company, &fact)? {
                summary.facts_inserted += 1;
            } else {
                summary.facts_existing += 1;
            }
        }
    }
    summary.unmapped_skus = unmapped.len();
    Ok(summary)
}
