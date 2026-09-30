//! Amazon: the Seller Central "CustomUnifiedTransaction" CSV and the
//! settlement V2 flat file. `CHANNEL_POSTING_MAP.md`, principles 1 to 3.
//!
//! Both formats are read into the same [`Settlement`] and posted by the same
//! [`build_settlement`], so one settlement produces identical postings
//! whichever file it came from (`tests/channel.rs` proves it against a
//! fixture in both formats).
//!
//! Rules:
//!
//! - One posting per settlement id, Standard and Invoiced account types alike.
//!   Activity (`Dr` clearing the settlement's net, `Cr` sales by family,
//!   shipping, `Dr` returns, promotions, fees) is the settlement; its money
//!   reaching the bank is a separate transfer posting.
//! - Marketplace facilitator tax is excluded entirely: tax collected and tax
//!   withheld must net to zero across the settlement, and that is asserted,
//!   not assumed. A settlement where it does not is quarantined.
//! - Every clearing leg comes from a column of a row. There is no balancing
//!   figure: if the legs do not balance the settlement is quarantined.
//! - **The unified report files a transfer under the *next* settlement id.**
//!   Verified on the 2026 data: the `Transfer` row inside settlement N
//!   pays settlement N-1 of the same account type. So transfers are matched
//!   to settlements by amount, not by id ([`match_transfers`]).

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, NaiveDateTime, Utc};
use ledger_core::Money;

use super::csvx::{
    cell, parse_iso_date, parse_money, parse_qty, parse_records, parse_utc, us_local_date, Table,
};
use super::db::{self, Fact};
use super::sku::SkuMapper;
use super::{
    allocate, class_weights, fmt_money, Channel, ChannelError, ChannelPosting, ImportSummary,
    Legs, PostingKind,
};
use crate::chart;
use crate::store::Ledger;

pub const PACIFIC_STD_OFFSET: i64 = -8;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Order,
    Refund,
    Other,
}

/// One order line or one non-order charge, in the unified report's columns.
/// Amounts are signed minor units exactly as Amazon states them.
#[derive(Clone, Debug)]
pub struct AmzRow {
    pub kind: RowKind,
    /// The original `type` (or V2 `transaction-type`).
    pub txn_type: String,
    pub description: String,
    pub order_id: String,
    pub sku: String,
    pub qty: i64,
    pub date: NaiveDate,
    pub product: i64,
    /// Shipping credits and gift wrap credits.
    pub shipping: i64,
    /// Promotional rebates (negative on an order).
    pub promo: i64,
    /// Every tax column, including marketplace withheld tax.
    pub tax: i64,
    /// Selling fees, other transaction fees, regulatory fee.
    pub fee_selling: i64,
    pub fee_fba: i64,
    pub other: i64,
    pub total: i64,
}

#[derive(Clone, Debug)]
pub struct Transfer {
    /// The settlement id the row was filed under (not necessarily the one it
    /// pays).
    pub filed_under: String,
    pub account_type: String,
    pub date: NaiveDate,
    pub amount: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Settlement {
    pub id: String,
    pub account_type: String,
    pub rows: Vec<AmzRow>,
    /// V2 only: the deposit the header states.
    pub deposit: Option<(NaiveDate, i64)>,
    /// V2 only: the header's `total-amount`, asserted against the rows.
    pub stated_total: Option<i64>,
}

impl Settlement {
    pub fn net(&self) -> i64 {
        self.rows.iter().map(|r| r.total).sum()
    }

    pub fn entry_date(&self) -> Option<NaiveDate> {
        self.rows.iter().map(|r| r.date).max()
    }
}

// ---------------------------------------------------------------------------
// Unified transaction CSV
// ---------------------------------------------------------------------------

fn parse_unified_datetime(raw: &str) -> Result<NaiveDate, ChannelError> {
    // "Apr 28, 2026 4:25:37 AM PDT": already local, so the date is the date.
    let trimmed = raw.trim();
    let without_tz = trimmed
        .rsplit_once(' ')
        .filter(|(_, tz)| tz.chars().all(|c| c.is_ascii_uppercase()) && tz.len() <= 4)
        .map_or(trimmed, |(head, _)| head);
    NaiveDateTime::parse_from_str(without_tz, "%b %d, %Y %I:%M:%S %p")
        .map(|d| d.date())
        .map_err(|_| ChannelError::Parse(format!("unreadable Amazon date {raw:?}")))
}

pub struct Unified {
    pub settlements: Vec<Settlement>,
    pub transfers: Vec<Transfer>,
}

pub fn parse_unified(text: &str) -> Result<Unified, ChannelError> {
    let records = parse_records(text, ',');
    let header_at = records
        .iter()
        .position(|r| r.first().is_some_and(|c| c.trim().eq_ignore_ascii_case("date/time")))
        .ok_or_else(|| ChannelError::MissingColumn("date/time".into()))?;
    let table = Table::from_records(records, header_at)?;

    let (c_date, c_sid, c_type, c_total) = (
        table.need("date/time")?,
        table.need("settlement id")?,
        table.need("type")?,
        table.need("total")?,
    );
    let c_order = table.opt("order id");
    let c_sku = table.opt("sku");
    let c_desc = table.opt("description");
    let c_qty = table.opt("quantity");
    let c_acct = table.opt("account type");
    let money = |row: &[String], name: &str| -> Result<i64, ChannelError> {
        match table.opt(name) {
            Some(i) => Ok(parse_money(cell(row, i))?.minor()),
            None => Ok(0),
        }
    };

    let mut by_id: BTreeMap<String, Settlement> = BTreeMap::new();
    let mut transfers = Vec::new();

    for r in &table.rows {
        let sid = cell(r, c_sid).to_string();
        if sid.is_empty() {
            continue;
        }
        let ty = cell(r, c_type).to_string();
        let acct = c_acct.map_or("", |i| cell(r, i)).to_string();
        let date = parse_unified_datetime(cell(r, c_date))?;
        let total = parse_money(cell(r, c_total))?.minor();

        let product = money(r, "product sales")?;
        let shipping = money(r, "shipping credits")? + money(r, "gift wrap credits")?;
        let promo = money(r, "promotional rebates")?;
        let tax = money(r, "product sales tax")?
            + money(r, "shipping credits tax")?
            + money(r, "giftwrap credits tax")?
            + money(r, "Tax On Regulatory Fee")?
            + money(r, "promotional rebates tax")?
            + money(r, "marketplace withheld tax")?;
        let fee_selling = money(r, "selling fees")?
            + money(r, "other transaction fees")?
            + money(r, "Regulatory Fee")?;
        let fee_fba = money(r, "fba fees")?;
        let other = money(r, "other")?;

        let sum = product + shipping + promo + tax + fee_selling + fee_fba + other;
        if sum != total {
            return Err(ChannelError::Parse(format!(
                "settlement {sid} {ty} row on {date}: columns add to {} but total is {}",
                fmt_money(Money::from_minor(sum)),
                fmt_money(Money::from_minor(total))
            )));
        }

        if ty.eq_ignore_ascii_case("Transfer") {
            transfers.push(Transfer {
                filed_under: sid.clone(),
                account_type: acct.clone(),
                date,
                amount: -total,
            });
            // Keep the settlement id known even if it holds only a transfer.
            by_id.entry(sid.clone()).or_insert_with(|| Settlement {
                id: sid,
                account_type: acct,
                ..Settlement::default()
            });
            continue;
        }

        let kind = match ty.as_str() {
            "Order" => RowKind::Order,
            "Refund" | "Chargeback Refund" => RowKind::Refund,
            _ => RowKind::Other,
        };
        let s = by_id.entry(sid.clone()).or_insert_with(|| Settlement {
            id: sid,
            account_type: acct,
            ..Settlement::default()
        });
        s.rows.push(AmzRow {
            kind,
            txn_type: ty,
            description: c_desc.map_or("", |i| cell(r, i)).to_string(),
            order_id: c_order.map_or("", |i| cell(r, i)).to_string(),
            sku: c_sku.map_or("", |i| cell(r, i)).to_string(),
            qty: c_qty.map_or(Ok(0), |i| parse_qty(cell(r, i)))?,
            date,
            product,
            shipping,
            promo,
            tax,
            fee_selling,
            fee_fba,
            other,
            total,
        });
    }

    Ok(Unified {
        settlements: by_id.into_values().filter(|s| !s.rows.is_empty()).collect(),
        transfers,
    })
}

// ---------------------------------------------------------------------------
// Settlement V2 flat file
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum V2Cat {
    Product,
    Shipping,
    Promo,
    Tax,
    FeeSelling,
    FeeFba,
    Other,
}

fn v2_category(amount_type: &str, desc: &str) -> V2Cat {
    let at = amount_type.to_lowercase();
    let d = desc.to_lowercase();
    match at.as_str() {
        "itemprice" => {
            if d == "principal" {
                V2Cat::Product
            } else if d.contains("tax") {
                V2Cat::Tax
            } else if d.contains("shipping") || d.contains("giftwrap") || d.contains("gift wrap") {
                V2Cat::Shipping
            } else {
                V2Cat::Other
            }
        }
        "itemwithheldtax" => V2Cat::Tax,
        "itemfees" => {
            if d.starts_with("fba") {
                V2Cat::FeeFba
            } else {
                V2Cat::FeeSelling
            }
        }
        "promotion" => {
            if d.contains("tax") {
                V2Cat::Tax
            } else {
                V2Cat::Promo
            }
        }
        _ => V2Cat::Other,
    }
}

fn local_date_v2(posted_time: &str, posted_date: &str) -> Result<NaiveDate, ChannelError> {
    if !posted_time.trim().is_empty() {
        Ok(us_local_date(parse_utc(posted_time)?, PACIFIC_STD_OFFSET))
    } else {
        parse_iso_date(posted_date)
    }
}

pub fn parse_v2(text: &str) -> Result<Vec<Settlement>, ChannelError> {
    let table = Table::from_records(parse_records(text, '\t'), 0)?;
    let c_sid = table.need("settlement-id")?;
    let c_deposit = table.need("deposit-date")?;
    let c_total = table.need("total-amount")?;
    let c_type = table.need("transaction-type")?;
    let c_amount = table.need("amount")?;
    let c_amount_type = table.need("amount-type")?;
    let c_amount_desc = table.need("amount-description")?;
    let c_order = table.opt("order-id");
    let c_sku = table.opt("sku");
    let c_qty = table.opt("quantity-purchased");
    let c_ptime = table.opt("posted-date-time");
    let c_pdate = table.opt("posted-date");
    let c_item = table.opt("order-item-code");

    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Settlement> = HashMap::new();
    // (settlement, transaction type, order id, item key) -> row index
    let mut item_index: HashMap<(String, String, String, String), usize> = HashMap::new();

    for r in &table.rows {
        let sid = cell(r, c_sid).to_string();
        if sid.is_empty() {
            continue;
        }
        if !by_id.contains_key(&sid) {
            order.push(sid.clone());
            by_id.insert(
                sid.clone(),
                Settlement {
                    id: sid.clone(),
                    account_type: "Invoiced Orders".to_string(),
                    ..Settlement::default()
                },
            );
        }
        let s = by_id.get_mut(&sid).expect("just inserted");

        // The header row: has a total and a deposit date, no transaction.
        if !cell(r, c_total).is_empty() && cell(r, c_type).is_empty() {
            let total = parse_money(cell(r, c_total))?.minor();
            s.stated_total = Some(total);
            let deposit = cell(r, c_deposit);
            if !deposit.is_empty() {
                let d = us_local_date(parse_utc(deposit)?, PACIFIC_STD_OFFSET);
                s.deposit = Some((d, total));
            }
            continue;
        }

        let ty = cell(r, c_type).to_string();
        if ty.is_empty() {
            continue;
        }
        let amount = parse_money(cell(r, c_amount))?.minor();
        let at = cell(r, c_amount_type);
        let ad = cell(r, c_amount_desc);
        let date = local_date_v2(
            c_ptime.map_or("", |i| cell(r, i)),
            c_pdate.map_or("", |i| cell(r, i)),
        )?;
        let order_id = c_order.map_or("", |i| cell(r, i)).to_string();
        let ty_norm = ty.to_lowercase();
        let kind = match ty_norm.as_str() {
            "order" => RowKind::Order,
            "refund" | "chargeback refund" => RowKind::Refund,
            _ => RowKind::Other,
        };

        let row_ix = if kind == RowKind::Other || order_id.is_empty() {
            None
        } else {
            let item = c_item.map_or("", |i| cell(r, i)).to_string();
            let sku = c_sku.map_or("", |i| cell(r, i)).to_string();
            Some((sid.clone(), ty.clone(), order_id.clone(), format!("{item}|{sku}")))
        };

        let idx = match row_ix.as_ref().and_then(|k| item_index.get(k)).copied() {
            Some(i) => i,
            None => {
                s.rows.push(AmzRow {
                    kind,
                    txn_type: ty.clone(),
                    description: if kind == RowKind::Other {
                        ad.to_string()
                    } else {
                        String::new()
                    },
                    order_id: order_id.clone(),
                    sku: c_sku.map_or("", |i| cell(r, i)).to_string(),
                    qty: c_qty.map_or(Ok(0), |i| parse_qty(cell(r, i)))?,
                    date,
                    product: 0,
                    shipping: 0,
                    promo: 0,
                    tax: 0,
                    fee_selling: 0,
                    fee_fba: 0,
                    other: 0,
                    total: 0,
                });
                let i = s.rows.len() - 1;
                if let Some(k) = row_ix {
                    item_index.insert(k, i);
                }
                i
            }
        };
        let row = &mut s.rows[idx];
        row.date = row.date.max(date);
        row.total += amount;
        if kind == RowKind::Other {
            // A non-order charge: classified by its type and description.
            row.other += amount;
        } else {
            match v2_category(at, ad) {
                V2Cat::Product => row.product += amount,
                V2Cat::Shipping => row.shipping += amount,
                V2Cat::Promo => row.promo += amount,
                V2Cat::Tax => row.tax += amount,
                V2Cat::FeeSelling => row.fee_selling += amount,
                V2Cat::FeeFba => row.fee_fba += amount,
                V2Cat::Other => row.other += amount,
            }
            if at.eq_ignore_ascii_case("itemprice") && ad.eq_ignore_ascii_case("principal") {
                let q = c_qty.map_or(Ok(0), |i| parse_qty(cell(r, i)))?;
                if q > 0 {
                    row.qty = q;
                }
            }
        }
    }

    Ok(order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .filter(|s| !s.rows.is_empty())
        .collect())
}

// ---------------------------------------------------------------------------
// Posting
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AmazonConfig {
    pub fallback_class: String,
    pub bank: String,
}

impl Default for AmazonConfig {
    fn default() -> Self {
        AmazonConfig {
            fallback_class: "foam".to_string(),
            bank: chart::CHECKING.to_string(),
        }
    }
}

/// The expense account a non-order charge belongs in (posting map:
/// advertising, subscription, FBA fees, channel labels, adjustments).
pub fn classify_other(txn_type: &str, description: &str) -> (&'static str, bool) {
    let t: String = txn_type
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    let d = description.to_lowercase();
    if d.contains("advertis") || d.contains("vine") {
        return (chart::AMAZON_ADVERTISING, false);
    }
    if d.contains("subscription") {
        return (chart::AMAZON_SUBSCRIPTION, false);
    }
    if t.starts_with("shippingservices") || d.contains("shipping label") || d.contains("postage") {
        return (chart::CHANNEL_SHIPPING_LABELS, false);
    }
    if d.contains("reimbursement") {
        return (chart::AMAZON_ADJUSTMENTS, false);
    }
    if t.starts_with("fba")
        || t == "feeadjustment"
        || d.contains("fba")
        || d.contains("awd")
        || d.contains("storage")
        || d.contains("inbound placement")
    {
        return (chart::AMAZON_FBA_FEES, false);
    }
    let known_adjustment = matches!(
        t.as_str(),
        "adjustment" | "safetreimbursement" | "debt" | "orderretrocharge" | "servicefee"
    );
    (chart::AMAZON_ADJUSTMENTS, !known_adjustment)
}

pub struct BuiltSettlement {
    pub posting: ChannelPosting,
    pub facts: Vec<Fact>,
    pub unmapped: Vec<(String, String)>,
}

/// Turns a settlement into its posting and order-line facts.
///
/// Errors are quarantine reasons: `TaxNotNetted` (facilitator tax does not
/// net to zero) and `Quarantine` (legs do not balance).
pub fn build_settlement(
    s: &Settlement,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &AmazonConfig,
) -> Result<BuiltSettlement, ChannelError> {
    let external_id = format!("amazon:{}", s.id);
    let clearing = Channel::Amazon.clearing();
    let fallback = cfg.fallback_class.as_str();

    let tax: i64 = s.rows.iter().map(|r| r.tax).sum();
    if tax != 0 {
        return Err(ChannelError::TaxNotNetted {
            external_id,
            residual: fmt_money(Money::from_minor(tax)),
        });
    }
    let net = s.net();
    if let Some(stated) = s.stated_total {
        if stated != net {
            return Err(ChannelError::ClearingMismatch {
                external_id,
                actual: fmt_money(Money::from_minor(net)),
                expected: fmt_money(Money::from_minor(stated)),
            });
        }
    }

    let class_of = |family: &str| -> String {
        classes
            .get(family)
            .cloned()
            .unwrap_or_else(|| super::sku::default_class(family).to_string())
    };

    let mut legs = Legs::default();
    let mut rev_by_class: BTreeMap<String, i64> = BTreeMap::new();
    // Non-order charges, by account, spread over classes once the
    // settlement's revenue by class is known.
    let mut overhead: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut flags: Vec<String> = Vec::new();
    let mut flagged_once: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut facts: Vec<Fact> = Vec::new();
    let mut unmapped: Vec<(String, String)> = Vec::new();
    let mut ordinal: HashMap<(String, String, &'static str), i64> = HashMap::new();

    let (mut gross, mut refunds, mut fees, mut other_sum) = (0i64, 0i64, 0i64, 0i64);

    for r in &s.rows {
        match r.kind {
            RowKind::Order | RowKind::Refund => {
                let m = mapper.resolve("amazon", &r.sku, &r.description);
                if m.is_flagged() {
                    unmapped.push((r.sku.clone(), r.description.clone()));
                }
                let class = class_of(m.family);
                let is_order = r.kind == RowKind::Order;

                if is_order {
                    legs.signed_credit(chart::SALES_INCOME, Some(&class), r.product);
                    if r.product > 0 {
                        *rev_by_class.entry(class.clone()).or_insert(0) += r.product;
                    }
                    gross += r.product + r.shipping;
                } else {
                    legs.signed_debit(chart::RETURNS_ALLOWANCES, Some(&class), -r.product);
                    refunds += -r.product;
                }
                legs.signed_credit(chart::SHIPPING_INCOME, Some(&class), r.shipping);
                legs.signed_credit(chart::DISCOUNTS_GIVEN, Some(&class), r.promo);
                legs.signed_debit(Channel::Amazon.fees(), Some(&class), -r.fee_selling);
                legs.signed_debit(chart::AMAZON_FBA_FEES, Some(&class), -r.fee_fba);
                legs.signed_debit(chart::AMAZON_ADJUSTMENTS, Some(&class), -r.other);
                fees += -(r.fee_selling + r.fee_fba);
                other_sum += -r.other;

                if r.product != 0 || r.qty != 0 {
                    let kind: &'static str = if is_order { "sale" } else { "refund" };
                    let n = ordinal
                        .entry((r.order_id.clone(), r.sku.clone(), kind))
                        .or_insert(0);
                    *n += 1;
                    facts.push(Fact {
                        channel: Channel::Amazon,
                        order_id: r.order_id.clone(),
                        kind,
                        line_key: format!("{}:{}:{}", s.id, r.sku, n),
                        order_date: r.date,
                        sku: r.sku.clone(),
                        title: r.description.clone(),
                        family: m.family.to_string(),
                        qty: r.qty.abs(),
                        amount_minor: r.product,
                    });
                }
            }
            RowKind::Other => {
                let (account, defaulted) = classify_other(&r.txn_type, &r.description);
                if defaulted && flagged_once.insert(format!("{}/{}", r.txn_type, r.description)) {
                    flags.push(format!(
                        "{:?} / {:?} has no rule: booked to Amazon adjustments",
                        r.txn_type, r.description
                    ));
                }
                let charge = r.fee_selling + r.fee_fba + r.other;
                // Revenue-shaped columns on a non-order row are not expected.
                let revenue_shaped = r.product + r.shipping + r.promo;
                if revenue_shaped != 0 {
                    flags.push(format!(
                        "{:?} row carries product/shipping/promo amounts; booked to Amazon adjustments",
                        r.txn_type
                    ));
                }
                let total_charge = charge + revenue_shaped;
                *overhead.entry(if revenue_shaped != 0 { chart::AMAZON_ADJUSTMENTS } else { account }).or_insert(0) +=
                    -total_charge;
                match account {
                    chart::AMAZON_ADJUSTMENTS => other_sum += -total_charge,
                    _ => fees += -total_charge,
                }
            }
        }
    }

    let weights = class_weights(&rev_by_class, fallback);
    for (account, amount) in &overhead {
        for (class, part) in allocate(*amount, &weights) {
            legs.signed_debit(account, Some(&class), part);
        }
    }

    legs.signed_debit(clearing, None, net);

    let entry_date = s
        .entry_date()
        .ok_or_else(|| ChannelError::Quarantine(format!("{external_id}: settlement has no rows")))?;
    let posting = ChannelPosting {
        channel: Channel::Amazon,
        external_id: external_id.clone(),
        kind: PostingKind::Settlement,
        entry_date,
        memo: format!("Amazon settlement {} ({})", s.id, s.account_type),
        legs: legs.legs,
        gross: Money::from_minor(gross),
        refunds: Money::from_minor(refunds),
        fees: Money::from_minor(fees),
        other: Money::from_minor(other_sum),
        net: Money::from_minor(net),
        payout: Money::ZERO,
        flags,
    };
    if !posting.is_balanced() {
        return Err(ChannelError::Quarantine(format!(
            "{external_id}: legs do not balance (an amount column is not booked anywhere)"
        )));
    }
    if posting.clearing_net() != net {
        return Err(ChannelError::ClearingMismatch {
            external_id,
            actual: fmt_money(Money::from_minor(posting.clearing_net())),
            expected: fmt_money(Money::from_minor(net)),
        });
    }
    Ok(BuiltSettlement {
        posting,
        facts,
        unmapped,
    })
}

/// Clearing to checking, dated the transfer.
pub fn transfer_posting(
    external_id: String,
    date: NaiveDate,
    amount: i64,
    paid: &[String],
    cfg: &AmazonConfig,
    flags: Vec<String>,
) -> ChannelPosting {
    let mut legs = Legs::default();
    legs.signed_debit(&cfg.bank, None, amount);
    legs.signed_credit(Channel::Amazon.clearing(), None, amount);
    ChannelPosting {
        channel: Channel::Amazon,
        external_id,
        kind: PostingKind::Transfer,
        entry_date: date,
        memo: if paid.is_empty() {
            format!("Amazon transfer to bank {date} (matches no settlement)")
        } else {
            format!("Amazon transfer to bank {date}, pays settlement(s) {}", paid.join(", "))
        },
        legs: legs.legs,
        gross: Money::ZERO,
        refunds: Money::ZERO,
        fees: Money::ZERO,
        other: Money::ZERO,
        net: Money::from_minor(-amount),
        payout: Money::from_minor(amount),
        flags,
    }
}

/// A transfer paired with what it paid.
#[derive(Clone, Debug)]
pub struct PayoutLink {
    pub external_id: String,
    pub date: NaiveDate,
    pub amount: i64,
    pub paid: Vec<String>,
    pub flags: Vec<String>,
}

/// Pairs unified-report transfers with settlements.
///
/// Pass 1, exact: for each transfer in date order, the earliest unpaid
/// settlement of the same account type whose net equals the transfer and
/// which ended no later than a day after it. Pass 2, groups: Amazon sometimes
/// pays one settlement in parts or several in one; walking what is left in
/// date order, a run of unpaid settlements and unmatched transfers that add
/// up to exactly zero is one reconciled group. Anything still left is
/// reported as unmatched, and stays in clearing until it is explained.
pub fn match_transfers(settlements: &[Settlement], transfers: &[Transfer]) -> Vec<PayoutLink> {
    let mut sorted: Vec<&Settlement> = settlements.iter().collect();
    sorted.sort_by_key(|s| (s.entry_date(), s.id.clone()));
    let mut ts: Vec<&Transfer> = transfers.iter().collect();
    ts.sort_by_key(|t| (t.date, t.filed_under.clone(), t.amount));

    let mut paid: Vec<bool> = vec![false; sorted.len()];
    let mut links: Vec<Option<PayoutLink>> = vec![None; ts.len()];

    for (ti, t) in ts.iter().enumerate() {
        let hit = sorted.iter().enumerate().position(|(si, s)| {
            !paid[si]
                && s.account_type == t.account_type
                && s.net() == t.amount
                && s.entry_date().is_some_and(|d| d <= t.date + chrono::Duration::days(1))
        });
        if let Some(si) = hit {
            paid[si] = true;
            links[ti] = Some(PayoutLink {
                external_id: format!("amazon:payout:{}", sorted[si].id),
                date: t.date,
                amount: t.amount,
                paid: vec![sorted[si].id.clone()],
                flags: Vec::new(),
            });
        }
    }

    // Pass 2: groups, per account type.
    let mut acct_types: Vec<&str> = sorted.iter().map(|s| s.account_type.as_str()).collect();
    acct_types.sort();
    acct_types.dedup();
    for acct in acct_types {
        enum Ev {
            S(usize),
            T(usize),
        }
        let mut events: Vec<(NaiveDate, u8, Ev)> = Vec::new();
        for (si, s) in sorted.iter().enumerate() {
            if !paid[si] && s.account_type == acct && s.net() != 0 {
                if let Some(d) = s.entry_date() {
                    events.push((d, 0, Ev::S(si)));
                }
            }
        }
        for (ti, t) in ts.iter().enumerate() {
            if links[ti].is_none() && t.account_type == acct {
                events.push((t.date, 1, Ev::T(ti)));
            }
        }
        events.sort_by_key(|e| (e.0, e.1));
        // Open events in date order. After each one, look for the shortest
        // run ending at it that adds to exactly zero and holds at least one
        // settlement and one transfer: that run is one reconciled group.
        // Older items that never resolve stay open and do not block newer
        // groups.
        let mut open: Vec<Ev> = Vec::new();
        for (_, _, ev) in events {
            open.push(ev);
            let signed = |e: &Ev| match e {
                Ev::S(si) => sorted[*si].net(),
                Ev::T(ti) => -ts[*ti].amount,
            };
            let mut sum = 0i64;
            let mut has_s = false;
            let mut has_t = false;
            let mut found: Option<usize> = None;
            for (k, e) in open.iter().enumerate().rev() {
                sum += signed(e);
                match e {
                    Ev::S(_) => has_s = true,
                    Ev::T(_) => has_t = true,
                }
                if sum == 0 && has_s && has_t {
                    found = Some(k);
                    break;
                }
            }
            if let Some(k) = found {
                let run: Vec<Ev> = open.split_off(k);
                let ids: Vec<String> = run
                    .iter()
                    .filter_map(|e| match e {
                        Ev::S(si) => Some(sorted[*si].id.clone()),
                        Ev::T(_) => None,
                    })
                    .collect();
                let first = ids[0].clone();
                let mut n = 0;
                for e in &run {
                    match e {
                        Ev::S(si) => paid[*si] = true,
                        Ev::T(ti) => {
                            n += 1;
                            links[*ti] = Some(PayoutLink {
                                external_id: format!("amazon:payout:{first}:{n}"),
                                date: ts[*ti].date,
                                amount: ts[*ti].amount,
                                paid: ids.clone(),
                                flags: vec![format!(
                                    "paid in parts or combined: settlements {} reconcile to their transfers as a group",
                                    ids.join(", ")
                                )],
                            });
                        }
                    }
                }
            }
        }
    }

    ts.iter()
        .enumerate()
        .map(|(ti, t)| {
            links[ti].clone().unwrap_or_else(|| PayoutLink {
                external_id: format!(
                    "amazon:payout:unmatched:{}:{}:{}",
                    t.filed_under, t.date, t.amount
                ),
                date: t.date,
                amount: t.amount,
                paid: Vec::new(),
                flags: vec!["transfer matches no settlement or settlement group yet".into()],
            })
        })
        .collect()
}

fn in_range(d: NaiveDate, from: Option<NaiveDate>, to: Option<NaiveDate>) -> bool {
    from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t)
}

#[allow(clippy::too_many_arguments)]
fn commit_settlement(
    ledger: &Ledger,
    company: &str,
    s: &Settlement,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &AmazonConfig,
    summary: &mut ImportSummary,
    now: chrono::DateTime<Utc>,
    unmapped_seen: &mut std::collections::BTreeSet<String>,
) -> Result<(), ChannelError> {
    match build_settlement(s, mapper, classes, cfg) {
        Ok(built) => {
            for (sku_code, title) in &built.unmapped {
                unmapped_seen.insert(format!("{sku_code}|{title}"));
            }
            for f in &built.facts {
                let m = mapper.resolve("amazon", &f.sku, &f.title);
                db::remember_sku(ledger, company, Channel::Amazon, &f.sku, &f.title, m, now)?;
            }
            super::commit_into(ledger, company, &built.posting, summary, now);
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
                Channel::Amazon,
                &format!("amazon:{}", s.id),
                reason,
                &err.to_string(),
                summary,
                now,
            );
        }
    }
    Ok(())
}

/// Imports a unified transaction CSV: settlements, their facts, and the
/// transfers matched to them.
pub fn import_unified(
    ledger: &Ledger,
    company: &str,
    text: &str,
    cfg: &AmazonConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let parsed = parse_unified(text)?;
    let mapper = db::load_mapper(ledger, company)?;
    let classes = db::family_classes(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();
    let mut unmapped_seen = std::collections::BTreeSet::new();

    for s in &parsed.settlements {
        if s.entry_date().is_some_and(|d| in_range(d, from, to)) {
            commit_settlement(
                ledger, company, s, &mapper, &classes, cfg, &mut summary, now, &mut unmapped_seen,
            )?;
        }
    }

    // Matching needs every settlement, whatever the date filter says.
    let quarantined: std::collections::HashSet<String> = summary
        .quarantined
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    for link in match_transfers(&parsed.settlements, &parsed.transfers) {
        if !in_range(link.date, from, to) {
            continue;
        }
        // A transfer paying a quarantined settlement is held with it.
        if link
            .paid
            .iter()
            .any(|id| quarantined.contains(&format!("amazon:{id}")))
        {
            summary.pending.push(link.external_id.clone());
            continue;
        }
        let posting = transfer_posting(
            link.external_id.clone(),
            link.date,
            link.amount,
            &link.paid,
            cfg,
            link.flags.clone(),
        );
        super::commit_into(ledger, company, &posting, &mut summary, now);
    }
    summary.unmapped_skus = unmapped_seen.len();
    Ok(summary)
}

/// Imports one or more V2 flat files.
pub fn import_v2(
    ledger: &Ledger,
    company: &str,
    text: &str,
    cfg: &AmazonConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let settlements = parse_v2(text)?;
    let mapper = db::load_mapper(ledger, company)?;
    let classes = db::family_classes(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();
    let mut unmapped_seen = std::collections::BTreeSet::new();

    for s in &settlements {
        if !s.entry_date().is_some_and(|d| in_range(d, from, to)) {
            continue;
        }
        commit_settlement(
            ledger, company, s, &mapper, &classes, cfg, &mut summary, now, &mut unmapped_seen,
        )?;
        let blocked = summary
            .quarantined
            .iter()
            .any(|(id, _)| *id == format!("amazon:{}", s.id));
        if let (Some((date, amount)), false) = (s.deposit, blocked) {
            if amount > 0 {
                let posting = transfer_posting(
                    format!("amazon:payout:{}", s.id),
                    date,
                    amount,
                    std::slice::from_ref(&s.id),
                    cfg,
                    Vec::new(),
                );
                super::commit_into(ledger, company, &posting, &mut summary, now);
            }
        }
    }
    summary.unmapped_skus = unmapped_seen.len();
    Ok(summary)
}
