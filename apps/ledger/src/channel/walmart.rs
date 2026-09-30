//! Walmart Marketplace: the Seller Center "Payments Reconciliation" CSV
//! (Reports > Payments > Payments Reconciliation, one row per amount).
//! `CHANNEL_POSTING_MAP.md`; D26.
//!
//! The real export (verified 2026-09-30 against Dan's account) has these
//! columns: `Period Start Date, Period End Date, Transaction Key,
//! Transaction Posted Timestamp, Transaction Type, Transaction Description,
//! Customer Order #, Customer Order line #, Purchase Order #, ..., Amount,
//! Amount Type, Ship Qty, ..., Partner Item Id, ..., Partner Item Name, ...`.
//! `Transaction Type` is `Sale` or `Release Reserve` in the data seen; an order
//! line is several rows, one per `Amount Type` (`Product Price`, `Commission
//! on Product`, `Product tax`, `Product tax withheld`). `Period Start/End`
//! were blank in the file downloaded, so periods are derived from the
//! statement cycle (below); populated columns win when present.
//!
//! Rules:
//!
//! - One posting per bi-weekly settlement period, external id
//!   `walmart:<period end>`. Activity is `Dr` clearing (1172) the period's
//!   net, `Cr` sales by product family, shipping, `Dr` returns, `Dr`
//!   commission (6613), `Cr`/`Dr` adjustments (6620) for reserve releases and
//!   anything without a rule (flagged), then the transfer to the bank on the
//!   payout date, so clearing nets to zero per posting.
//! - Marketplace facilitator tax (`Product tax` collected, `... withheld`) is
//!   excluded: it must net to zero across the period or the period is
//!   quarantined (`TaxNotNetted`). It never reaches 2200.
//! - No balancing figure. Legs come from columns; unbalanced or non-tying
//!   periods are quarantined.
//! - Periods whose payout date is after `as_of` wait in `pending`.
//!
//! **Assumption to verify with Walmart's statement pages:** cycles are 14
//! days ending on Saturdays, anchored on the statement `Sep 5 - Sep 19, 2026`
//! (next `Sep 19 - Oct 3`, paid Tuesday `Oct 6`), a boundary day belongs to
//! the period that ends on it, and the payout is the Tuesday three days after
//! the period end.

use std::collections::{BTreeMap, HashMap};

use chrono::{Duration, NaiveDate, Utc};
use ledger_core::Money;

use super::csvx::{cell, parse_money, parse_qty, parse_records, Table};
use super::db::{self, Fact};
use super::sku::SkuMapper;
use super::{
    allocate, class_weights, fmt_money, Channel, ChannelError, ChannelPosting, ImportSummary,
    Legs, PostingKind,
};
use crate::chart;
use crate::store::Ledger;

/// A period end on the statement grid (Saturday, 2026-09-19).
pub fn anchor() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 19).expect("valid date")
}

#[derive(Clone, Debug)]
pub struct WalmartConfig {
    pub fallback_class: String,
    pub bank: String,
    /// Payout dates after this are pending. Defaults to today.
    pub as_of: NaiveDate,
}

impl Default for WalmartConfig {
    fn default() -> Self {
        WalmartConfig {
            fallback_class: "foam".to_string(),
            bank: chart::CHECKING.to_string(),
            as_of: Utc::now().date_naive(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WmRow {
    pub date: NaiveDate,
    pub period: Option<(NaiveDate, NaiveDate)>,
    pub key: String,
    pub txn_type: String,
    pub order: String,
    pub line: String,
    pub amount: i64,
    pub amount_type: String,
    pub qty: i64,
    pub sku: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct Period {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub payout: NaiveDate,
    /// Whether `start`/`end` came from the file rather than the cycle grid.
    pub from_file: bool,
    pub rows: Vec<WmRow>,
}

fn parse_date(raw: &str) -> Result<NaiveDate, ChannelError> {
    let s = raw.trim();
    for fmt in ["%m/%d/%Y", "%Y-%m-%d", "%m-%d-%Y"] {
        let head: String = s.chars().take(10).collect();
        if let Ok(d) = NaiveDate::parse_from_str(&head, fmt) {
            return Ok(d);
        }
    }
    Err(ChannelError::Parse(format!("unreadable date {raw:?}")))
}

/// The grid period ending on or after `d`: `(start, end)`, `start` exclusive.
pub fn grid_period(d: NaiveDate) -> (NaiveDate, NaiveDate) {
    let diff = (d - anchor()).num_days();
    let n = -((-diff).div_euclid(14));
    let end = anchor() + Duration::days(14 * n);
    (end - Duration::days(14), end)
}

pub fn parse(text: &str) -> Result<Vec<Period>, ChannelError> {
    let table = Table::from_records(parse_records(text, ','), 0)?;
    let c_posted = table.need("Transaction Posted Timestamp")?;
    let c_type = table.need("Transaction Type")?;
    let c_amount = table.need("Amount")?;
    let c_atype = table.need("Amount Type")?;
    let c_key = table.opt("Transaction Key");
    let c_order = table.opt("Customer Order #");
    let c_line = table.opt("Customer Order line #");
    let c_qty = table.opt("Ship Qty");
    let c_sku = table.opt("Partner Item Id");
    let c_title = table.opt("Partner Item Name");
    let c_ps = table.opt("Period Start Date");
    let c_pe = table.opt("Period End Date");

    let mut by_end: BTreeMap<NaiveDate, Period> = BTreeMap::new();
    for row in &table.rows {
        let get = |i: Option<usize>| i.map_or("", |i| cell(row, i));
        let date = parse_date(cell(row, c_posted))?;
        let file_period = match (get(c_ps), get(c_pe)) {
            (s, e) if !s.is_empty() && !e.is_empty() => Some((parse_date(s)?, parse_date(e)?)),
            _ => None,
        };
        let (start, end) = file_period.unwrap_or_else(|| grid_period(date));
        let r = WmRow {
            date,
            period: file_period,
            key: get(c_key).to_string(),
            txn_type: cell(row, c_type).to_string(),
            order: get(c_order).to_string(),
            line: get(c_line).to_string(),
            amount: parse_money(cell(row, c_amount))?.minor(),
            amount_type: cell(row, c_atype).to_string(),
            qty: parse_qty(get(c_qty))?,
            sku: get(c_sku).to_string(),
            title: get(c_title).to_string(),
        };
        by_end
            .entry(end)
            .or_insert_with(|| Period {
                start,
                end,
                payout: end + Duration::days(3),
                from_file: file_period.is_some(),
                rows: Vec::new(),
            })
            .rows
            .push(r);
    }
    Ok(by_end.into_values().collect())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Comp {
    Product,
    Shipping,
    Commission,
    Tax,
    Other,
}

fn component(amount_type: &str) -> Comp {
    let t = amount_type.trim().to_lowercase();
    if t.contains("tax") {
        Comp::Tax
    } else if t.starts_with("commission") {
        Comp::Commission
    } else if t == "product price" {
        Comp::Product
    } else if t == "shipping" || t == "shipping price" {
        Comp::Shipping
    } else {
        Comp::Other
    }
}

pub struct BuiltPeriod {
    pub posting: ChannelPosting,
    pub facts: Vec<Fact>,
    pub unmapped: Vec<(String, String)>,
    /// True when the payout is after `as_of`: post nothing yet.
    pub pending: bool,
}

pub fn build_period(
    p: &Period,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &WalmartConfig,
) -> Result<BuiltPeriod, ChannelError> {
    let external_id = format!("walmart:{}", p.end);
    let clearing = Channel::Walmart.clearing();
    let fallback = cfg.fallback_class.as_str();

    let tax: i64 = p
        .rows
        .iter()
        .filter(|r| component(&r.amount_type) == Comp::Tax)
        .map(|r| r.amount)
        .sum();
    if tax != 0 {
        return Err(ChannelError::TaxNotNetted {
            external_id,
            residual: fmt_money(Money::from_minor(tax)),
        });
    }
    let net: i64 = p.rows.iter().map(|r| r.amount).sum();

    let class_of = |family: &str| -> String {
        classes
            .get(family)
            .cloned()
            .unwrap_or_else(|| super::sku::default_class(family).to_string())
    };

    let mut legs = Legs::default();
    let mut rev_by_class: BTreeMap<String, i64> = BTreeMap::new();
    let mut overhead: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut flags: Vec<String> = Vec::new();
    let mut flagged_once = std::collections::BTreeSet::new();
    let mut facts: Vec<Fact> = Vec::new();
    let mut unmapped: Vec<(String, String)> = Vec::new();
    let mut ordinal: HashMap<(String, String, &'static str), i64> = HashMap::new();
    let (mut gross, mut refunds, mut fees, mut other_sum) = (0i64, 0i64, 0i64, 0i64);

    for r in &p.rows {
        let ttype = r.txn_type.trim().to_lowercase();
        let is_refund = ttype.contains("refund") || ttype.contains("return");
        let is_order_row = ttype == "sale" || is_refund;
        let comp = component(&r.amount_type);
        if comp == Comp::Tax {
            continue;
        }
        if !is_order_row || comp == Comp::Other {
            if is_order_row && flagged_once.insert(format!("{}/{}", r.txn_type, r.amount_type)) {
                flags.push(format!(
                    "{:?} / {:?} has no rule: booked to adjustments",
                    r.txn_type, r.amount_type
                ));
            } else if ttype == "release reserve" {
                if flagged_once.insert("release reserve".to_string()) {
                    flags.push("reserve releases booked to channel adjustments (no Walmart reserve account)".into());
                }
            } else if !is_order_row && flagged_once.insert(format!("{}/{}", r.txn_type, r.amount_type)) {
                flags.push(format!(
                    "{:?} / {:?} has no rule: booked to adjustments",
                    r.txn_type, r.amount_type
                ));
            }
            *overhead.entry(chart::CHANNEL_ADJUSTMENTS).or_insert(0) += -r.amount;
            other_sum += -r.amount;
            continue;
        }
        let m = mapper.resolve("walmart", &r.sku, &r.title);
        let class = class_of(m.family);
        match comp {
            Comp::Product => {
                if is_refund {
                    legs.signed_debit(chart::RETURNS_ALLOWANCES, Some(&class), -r.amount);
                    refunds += -r.amount;
                } else {
                    legs.signed_credit(chart::SALES_INCOME, Some(&class), r.amount);
                    if r.amount > 0 {
                        *rev_by_class.entry(class.clone()).or_insert(0) += r.amount;
                    }
                    gross += r.amount;
                }
                let kind: &'static str = if is_refund { "refund" } else { "sale" };
                if m.is_flagged() {
                    unmapped.push((r.sku.clone(), r.title.clone()));
                }
                let n = ordinal
                    .entry((r.order.clone(), r.line.clone(), kind))
                    .or_insert(0);
                *n += 1;
                facts.push(Fact {
                    channel: Channel::Walmart,
                    order_id: r.order.clone(),
                    kind,
                    line_key: format!("{}:{}:{}:{}", r.key, r.order, r.line, n),
                    order_date: r.date,
                    sku: r.sku.clone(),
                    title: r.title.clone(),
                    family: m.family.to_string(),
                    qty: r.qty.abs(),
                    amount_minor: r.amount,
                });
            }
            Comp::Shipping => {
                legs.signed_credit(chart::SHIPPING_INCOME, Some(&class), r.amount);
                gross += r.amount;
            }
            Comp::Commission => {
                legs.signed_debit(Channel::Walmart.fees(), Some(&class), -r.amount);
                fees += -r.amount;
            }
            Comp::Tax | Comp::Other => unreachable!("handled above"),
        }
    }

    let weights = class_weights(&rev_by_class, fallback);
    for (account, amount) in &overhead {
        for (class, part) in allocate(*amount, &weights) {
            legs.signed_debit(account, Some(&class), part);
        }
    }

    legs.signed_debit(clearing, None, net);
    if legs.net_debit(clearing) != net {
        return Err(ChannelError::ClearingMismatch {
            external_id,
            actual: fmt_money(Money::from_minor(legs.net_debit(clearing))),
            expected: fmt_money(Money::from_minor(net)),
        });
    }
    let pending = p.payout > cfg.as_of;
    let payout = if pending { 0 } else { net };
    if !pending {
        legs.signed_debit(&cfg.bank, None, net);
        legs.signed_credit(clearing, None, net);
        if legs.net_debit(clearing) != 0 {
            return Err(ChannelError::Quarantine(format!(
                "{external_id}: clearing does not net to zero after the transfer"
            )));
        }
    }

    let posting = ChannelPosting {
        channel: Channel::Walmart,
        external_id: external_id.clone(),
        kind: PostingKind::Payout,
        entry_date: p.payout,
        memo: format!("Walmart settlement {} to {} (paid {})", p.start, p.end, p.payout),
        legs: legs.legs,
        gross: Money::from_minor(gross),
        refunds: Money::from_minor(refunds),
        fees: Money::from_minor(fees),
        other: Money::from_minor(other_sum),
        net: Money::from_minor(net),
        payout: Money::from_minor(payout),
        flags,
    };
    if !posting.is_balanced() {
        return Err(ChannelError::Quarantine(format!(
            "{external_id}: legs do not balance (an amount is not booked anywhere)"
        )));
    }
    Ok(BuiltPeriod {
        posting,
        facts,
        unmapped,
        pending,
    })
}

fn in_range(d: NaiveDate, from: Option<NaiveDate>, to: Option<NaiveDate>) -> bool {
    from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t)
}

pub fn import_recon(
    ledger: &Ledger,
    company: &str,
    text: &str,
    cfg: &WalmartConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let periods = parse(text)?;
    let mapper = db::load_mapper(ledger, company)?;
    let classes = db::family_classes(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();
    let mut unmapped_seen = std::collections::BTreeSet::new();

    for p in &periods {
        if !in_range(p.payout, from, to) {
            continue;
        }
        match build_period(p, &mapper, &classes, cfg) {
            Ok(built) if built.pending => {
                summary.pending.push(built.posting.external_id.clone());
            }
            Ok(built) => {
                for (sku, title) in &built.unmapped {
                    unmapped_seen.insert(format!("{sku}|{title}"));
                }
                for f in &built.facts {
                    let m = mapper.resolve("walmart", &f.sku, &f.title);
                    db::remember_sku(ledger, company, Channel::Walmart, &f.sku, &f.title, m, now)?;
                }
                super::commit_into(ledger, company, &built.posting, &mut summary, now);
                db::clear_quarantine(ledger, company, &built.posting.external_id)?;
                for f in &built.facts {
                    if db::insert_fact(ledger, company, f)? {
                        summary.facts_inserted += 1;
                    } else {
                        summary.facts_existing += 1;
                    }
                }
            }
            Err(err) => {
                let reason = match &err {
                    ChannelError::TaxNotNetted { .. } => "tax_not_netted",
                    ChannelError::ClearingMismatch { .. } => "clearing_mismatch",
                    _ => "unbalanced",
                };
                super::quarantine_into(
                    ledger,
                    company,
                    Channel::Walmart,
                    &format!("walmart:{}", p.end),
                    reason,
                    &err.to_string(),
                    &mut summary,
                    now,
                );
            }
        }
    }
    summary.unmapped_skus = unmapped_seen.len();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn grid_periods_end_on_the_statement_boundaries() {
        assert_eq!(grid_period(d(2026, 9, 16)), (d(2026, 9, 5), d(2026, 9, 19)));
        assert_eq!(grid_period(d(2026, 9, 19)), (d(2026, 9, 5), d(2026, 9, 19)));
        assert_eq!(grid_period(d(2026, 9, 20)), (d(2026, 9, 19), d(2026, 10, 3)));
        assert_eq!(grid_period(d(2026, 8, 8)), (d(2026, 7, 25), d(2026, 8, 8)));
        assert_eq!(grid_period(d(2025, 12, 27)).1.weekday(), chrono::Weekday::Sat);
    }
}
