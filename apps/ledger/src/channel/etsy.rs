//! Etsy: the Payment account "Monthly statement" CSV (Finances > Monthly
//! statements > Download CSV). `CHANNEL_POSTING_MAP.md`; D26.
//!
//! Real format (verified 2026-09-30 on Dan's shop): `Date,Type,Title,Info,
//! Currency,Amount,"Fees & Taxes",Net,"Tax Details"`, dates like `January 31,
//! 2026`, money like `$67.35` / `-$0.55`, `--` for blank, newest row first.
//! `Type` is `Sale`, `Fee`, `Tax`, `Marketing`, `Shipping`, `Refund`,
//! `Deposit` and possibly others. A `Sale` row's Amount includes the sales
//! tax the buyer paid; a separate `Tax` row on the same order removes it
//! ("Sales tax paid by buyer", remitted by Etsy). A `Deposit` row's title is
//! `$44.08 sent to your bank account` and its money is everything since the
//! previous deposit.
//!
//! Rules:
//!
//! - One posting per deposit (external id `etsy:<date>:<n>`). Activity since
//!   the previous deposit is `Dr` clearing (1173) its net, `Cr` sales by
//!   family, `Dr` returns, `Dr` Etsy fees (6614), advertising (6600),
//!   shipping labels (6617), adjustments (6620, flagged) for unknown types and
//!   for `Payment` rows (the owner's card topping up a negative balance).
//!   The deposit is the transfer to the bank: clearing nets to zero.
//! - Facilitator tax is excluded: revenue is the sale less the tax row of the
//!   same order. Tax with no sale or refund beside it, or more tax than sale,
//!   is `TaxNotNetted` and the deposit is quarantined.
//! - The stated deposit must equal the activity's net. Otherwise it is
//!   quarantined, never plugged. The first deposit of a run whose activity
//!   started in a month that was not supplied lands here: give the importer
//!   every month, in one call, so carries resolve.
//! - Activity after the last deposit is `pending` (Etsy balance not yet paid).

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, Utc};
use ledger_core::Money;

use super::csvx::{cell, parse_records, Table};
use super::db::{self, Fact};
use super::sku::SkuMapper;
use super::{
    allocate, class_weights, fmt_money, Channel, ChannelError, ChannelPosting, ImportSummary,
    Legs, PostingKind,
};
use crate::chart;
use crate::store::Ledger;

#[derive(Clone, Debug)]
pub struct EtsyConfig {
    pub fallback_class: String,
    pub bank: String,
}

impl Default for EtsyConfig {
    fn default() -> Self {
        EtsyConfig {
            fallback_class: "foam".to_string(),
            bank: chart::CHECKING.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EtsyRow {
    pub date: NaiveDate,
    pub kind: String,
    pub title: String,
    pub info: String,
    pub amount: i64,
    pub net: i64,
}

fn money_cell(raw: &str) -> Result<i64, ChannelError> {
    let t = raw.trim();
    if t.is_empty() || t == "--" {
        return Ok(0);
    }
    Ok(super::csvx::parse_money(t)?.minor())
}

/// One statement file, oldest row first.
pub fn parse_statement(text: &str) -> Result<Vec<EtsyRow>, ChannelError> {
    let table = Table::from_records(parse_records(text, ','), 0)?;
    let c_date = table.need("Date")?;
    let c_type = table.need("Type")?;
    let c_title = table.need("Title")?;
    let c_info = table.need("Info")?;
    let c_amount = table.need("Amount")?;
    let c_net = table.need("Net")?;
    let mut rows = Vec::new();
    for row in &table.rows {
        let raw = cell(row, c_date);
        let date = NaiveDate::parse_from_str(raw, "%B %d, %Y")
            .map_err(|_| ChannelError::Parse(format!("unreadable date {raw:?}")))?;
        rows.push(EtsyRow {
            date,
            kind: cell(row, c_type).to_string(),
            title: cell(row, c_title).to_string(),
            info: cell(row, c_info).to_string(),
            amount: money_cell(cell(row, c_amount))?,
            net: money_cell(cell(row, c_net))?,
        });
    }
    rows.reverse();
    Ok(rows)
}

/// Several statements as one chronological run (files ordered by their
/// earliest row, rows oldest first).
pub fn parse_statements(texts: &[&str]) -> Result<Vec<EtsyRow>, ChannelError> {
    let mut files: Vec<Vec<EtsyRow>> = texts
        .iter()
        .map(|t| parse_statement(t))
        .collect::<Result<_, _>>()?;
    files.sort_by_key(|f| f.first().map(|r| r.date));
    Ok(files.into_iter().flatten().collect())
}

/// `$44.08 sent to your bank account` to minor units.
pub fn deposit_amount(title: &str) -> Option<i64> {
    let after = title.split('$').nth(1)?;
    let num: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    super::csvx::parse_money(&num).ok().map(|m| m.minor())
}

fn order_id(row: &EtsyRow) -> Option<String> {
    for s in [&row.info, &row.title] {
        if let Some(pos) = s.find("Order #") {
            let id: String = s[pos + 7..].chars().take_while(char::is_ascii_digit).collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// A deposit and the activity it paid.
#[derive(Clone, Debug)]
pub struct Batch {
    pub external_id: String,
    pub date: NaiveDate,
    pub stated: i64,
    pub rows: Vec<EtsyRow>,
}

/// Splits a chronological run into deposits and the trailing open balance.
pub fn batches(rows: &[EtsyRow]) -> (Vec<Batch>, Vec<EtsyRow>) {
    let mut out = Vec::new();
    let mut acc: Vec<EtsyRow> = Vec::new();
    let mut per_day: HashMap<NaiveDate, usize> = HashMap::new();
    for r in rows {
        if r.kind.eq_ignore_ascii_case("deposit") {
            let n = per_day.entry(r.date).or_insert(0);
            *n += 1;
            out.push(Batch {
                external_id: format!("etsy:{}:{}", r.date, n),
                date: r.date,
                stated: deposit_amount(&r.title).unwrap_or(0),
                rows: std::mem::take(&mut acc),
            });
        } else {
            acc.push(r.clone());
        }
    }
    (out, acc)
}

pub struct BuiltDeposit {
    pub posting: ChannelPosting,
    pub facts: Vec<Fact>,
    pub unmapped: Vec<(String, String)>,
}

#[derive(Default)]
struct OrderAcc {
    title: String,
    sale: i64,
    refund: i64,
    tax: i64,
}

pub fn build_deposit(
    b: &Batch,
    mapper: &SkuMapper,
    classes: &HashMap<String, String>,
    cfg: &EtsyConfig,
) -> Result<BuiltDeposit, ChannelError> {
    let external_id = b.external_id.clone();
    let clearing = Channel::Etsy.clearing();
    let fallback = cfg.fallback_class.as_str();

    if b.rows.is_empty() {
        return Err(ChannelError::Quarantine(format!(
            "{external_id}: deposit of {} with no activity before it in the files supplied (opening carry)",
            fmt_money(Money::from_minor(b.stated))
        )));
    }
    let net: i64 = b.rows.iter().map(|r| r.net).sum();
    if net != b.stated {
        return Err(ChannelError::ClearingMismatch {
            external_id,
            actual: fmt_money(Money::from_minor(net)),
            expected: fmt_money(Money::from_minor(b.stated)),
        });
    }

    let mut orders: BTreeMap<String, OrderAcc> = BTreeMap::new();
    let mut fees = 0i64;
    let mut adverts = 0i64;
    let mut labels = 0i64;
    let mut other = 0i64;
    let mut flags: Vec<String> = Vec::new();
    let mut flagged_once = std::collections::BTreeSet::new();

    for r in &b.rows {
        let kind = r.kind.to_lowercase();
        let oid = order_id(r);
        match kind.as_str() {
            "sale" => {
                let o = orders.entry(oid.unwrap_or_else(|| "?".into())).or_default();
                o.sale += r.amount;
            }
            "refund" => {
                let o = orders.entry(oid.unwrap_or_else(|| "?".into())).or_default();
                o.refund += r.amount;
            }
            "tax" => {
                let o = orders.entry(oid.unwrap_or_else(|| "?".into())).or_default();
                o.tax += r.net;
            }
            "fee" => {
                if let (Some(id), Some(t)) = (oid, r.title.strip_prefix("Transaction fee:")) {
                    orders.entry(id).or_default().title = t.trim().to_string();
                }
                fees += -r.net;
            }
            "marketing" => adverts += -r.net,
            "shipping" => labels += -r.net,
            "payment" => {
                // "Card Payment": the owner's card topping up a negative balance.
                // Not revenue and not a refund of fees; the card charge that funds it
                // is expensed elsewhere, so this is held in adjustments for review.
                if flagged_once.insert(kind.clone()) {
                    flags.push(
                        "card payments into the Etsy balance booked to adjustments (6620); the funding card charge belongs against it, not as a second fee"
                            .to_string(),
                    );
                }
                other += -r.net;
            }
            _ => {
                if flagged_once.insert(kind.clone()) {
                    flags.push(format!("{:?} rows have no rule: booked to adjustments", r.kind));
                }
                other += -r.net;
            }
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
    let mut facts = Vec::new();
    let mut unmapped = Vec::new();
    let (mut gross, mut refunds) = (0i64, 0i64);

    for (id, o) in &orders {
        // Tax paid by the buyer is negative on a sale, positive on a refund.
        let (sale_tax, refund_tax) = if o.tax < 0 { (o.tax, 0) } else { (0, o.tax) };
        let bad_sale = sale_tax != 0 && (o.sale == 0 || o.sale + sale_tax < 0);
        let bad_refund = refund_tax != 0 && (o.refund == 0 || o.refund + refund_tax > 0);
        if bad_sale || bad_refund {
            return Err(ChannelError::TaxNotNetted {
                external_id,
                residual: format!("order {id}: tax {}", fmt_money(Money::from_minor(o.tax))),
            });
        }
        let m = mapper.resolve("etsy", &o.title, &o.title);
        let class = class_of(m.family);
        if m.is_flagged() {
            unmapped.push((o.title.clone(), o.title.clone()));
        }
        let product = o.sale + sale_tax;
        let returned = -(o.refund + refund_tax);
        legs.signed_credit(chart::SALES_INCOME, Some(&class), product);
        legs.signed_debit(chart::RETURNS_ALLOWANCES, Some(&class), returned);
        if product > 0 {
            *rev_by_class.entry(class.clone()).or_insert(0) += product;
        }
        gross += product;
        refunds += returned;
        for (kind, amt) in [("sale", product), ("refund", -returned)] {
            if amt != 0 {
                facts.push(Fact {
                    channel: Channel::Etsy,
                    order_id: id.clone(),
                    kind,
                    line_key: format!("{id}:{kind}"),
                    order_date: b.rows.first().map_or(b.date, |r| r.date),
                    sku: o.title.clone(),
                    title: o.title.clone(),
                    family: m.family.to_string(),
                    qty: 1,
                    amount_minor: amt,
                });
            }
        }
    }

    let weights = class_weights(&rev_by_class, fallback);
    for (account, amount) in [
        (Channel::Etsy.fees(), fees),
        (chart::ADVERTISING_CHANNEL_FEES, adverts),
        (chart::CHANNEL_SHIPPING_LABELS, labels),
        (chart::CHANNEL_ADJUSTMENTS, other),
    ] {
        for (class, part) in allocate(amount, &weights) {
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
    legs.signed_debit(&cfg.bank, None, net);
    legs.signed_credit(clearing, None, net);
    if legs.net_debit(clearing) != 0 {
        return Err(ChannelError::Quarantine(format!(
            "{external_id}: clearing does not net to zero after the deposit"
        )));
    }

    let posting = ChannelPosting {
        channel: Channel::Etsy,
        external_id: external_id.clone(),
        kind: PostingKind::Payout,
        entry_date: b.date,
        memo: format!("Etsy deposit {} ({})", b.date, fmt_money(Money::from_minor(b.stated))),
        legs: legs.legs,
        gross: Money::from_minor(gross),
        refunds: Money::from_minor(refunds),
        fees: Money::from_minor(fees + adverts + labels),
        other: Money::from_minor(other),
        net: Money::from_minor(net),
        payout: Money::from_minor(net),
        flags,
    };
    if !posting.is_balanced() {
        return Err(ChannelError::Quarantine(format!(
            "{external_id}: legs do not balance (an amount is not booked anywhere)"
        )));
    }
    Ok(BuiltDeposit {
        posting,
        facts,
        unmapped,
    })
}

fn in_range(d: NaiveDate, from: Option<NaiveDate>, to: Option<NaiveDate>) -> bool {
    from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t)
}

/// Imports one or more monthly statements as a single run.
pub fn import_statements(
    ledger: &Ledger,
    company: &str,
    texts: &[&str],
    cfg: &EtsyConfig,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<ImportSummary, ChannelError> {
    db::ensure_setup(ledger, company)?;
    let rows = parse_statements(texts)?;
    let (deposits, open) = batches(&rows);
    let mapper = db::load_mapper(ledger, company)?;
    let classes = db::family_classes(ledger, company)?;
    let now = Utc::now();
    let mut summary = ImportSummary::default();
    let mut unmapped_seen = std::collections::BTreeSet::new();

    for b in &deposits {
        if !in_range(b.date, from, to) {
            continue;
        }
        match build_deposit(b, &mapper, &classes, cfg) {
            Ok(built) => {
                for (sku, title) in &built.unmapped {
                    unmapped_seen.insert(format!("{sku}|{title}"));
                }
                for f in &built.facts {
                    let m = mapper.resolve("etsy", &f.sku, &f.title);
                    db::remember_sku(ledger, company, Channel::Etsy, &f.sku, &f.title, m, now)?;
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
                    Channel::Etsy,
                    &b.external_id,
                    reason,
                    &err.to_string(),
                    &mut summary,
                    now,
                );
            }
        }
    }
    if !open.is_empty() {
        summary.pending.push(format!(
            "etsy:open-balance ({} rows since the last deposit)",
            open.len()
        ));
    }
    summary.unmapped_skus = unmapped_seen.len();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_titles_parse() {
        assert_eq!(deposit_amount("$44.08 sent to your bank account"), Some(4408));
        assert_eq!(deposit_amount("$1,044.08 sent to your bank account"), Some(104_408));
        assert_eq!(deposit_amount("no money"), None);
    }
}
