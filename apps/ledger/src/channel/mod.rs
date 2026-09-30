//! Sales-channel ingest. `DECISIONS.md` D26; the approved posting map is
//! `Channel Sync 2026/CHANNEL_POSTING_MAP.md`.
//!
//! One posting unit per payout (Shopify), settlement (Amazon) or bank
//! transfer. Revenue is gross, fees are a separate expense, marketplace
//! facilitator tax (Amazon, Walmart, Etsy) is excluded outright, and every
//! channel clearing account nets to zero per payout. Unit and SKU detail
//! lives in `order_lines`; the ledger only ever sees product *families*.
//!
//! Nothing in this module talks to a channel's API or to QuickBooks. Every
//! importer reads a file the operator exported by hand, exactly as
//! [`crate::authnet`] does, and posts through the same [`crate::post::post`]
//! and [`crate::store::Ledger::post_entry`] every other document uses, so the
//! period gate, class rule and balance triggers all apply.
//!
//! **A mismatch is quarantined, never plugged.** If a payout's own
//! arithmetic fails (clearing does not net to the payout, facilitator tax
//! does not net to zero, a status this module does not understand), nothing
//! is posted for it and a `channel_quarantine` row says why.

pub mod amazon;
pub mod csvx;
pub mod db;
pub mod report;
pub mod shopify;
pub mod sku;
pub mod stubs;

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::{Money, MoneyError};
use thiserror::Error;

use crate::chart;
use crate::post::{self, PostError, Posting};
use crate::store::{CommandMeta, Ledger, LedgerError};
use crate::types::{
    AccountId, ClassId, DocKind, DocLine, LedgerDocument, LineKind, PostingContext, Side,
};

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("{0}")]
    Parse(String),
    #[error("missing required column {0:?}")]
    MissingColumn(String),
    #[error("{external_id}: clearing account nets to {actual} but the payout is {expected}")]
    ClearingMismatch {
        external_id: String,
        actual: String,
        expected: String,
    },
    #[error("{external_id}: facilitator tax does not net to zero (residual {residual})")]
    TaxNotNetted {
        external_id: String,
        residual: String,
    },
    #[error("{0}")]
    Quarantine(String),
    #[error("{0}")]
    NotImplemented(String),
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Post(#[from] PostError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    Shopify,
    Amazon,
    Walmart,
    Etsy,
}

impl Channel {
    pub const ALL: [Channel; 4] = [
        Channel::Shopify,
        Channel::Amazon,
        Channel::Walmart,
        Channel::Etsy,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Channel::Shopify => "shopify",
            Channel::Amazon => "amazon",
            Channel::Walmart => "walmart",
            Channel::Etsy => "etsy",
        }
    }

    pub fn parse(raw: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.as_str() == raw)
    }

    /// The channel's clearing account (posting map, "Clearing" row).
    pub const fn clearing(self) -> &'static str {
        match self {
            Channel::Shopify => chart::SHOPIFY_CLEARING,
            Channel::Amazon => chart::AMAZON_CLEARING,
            Channel::Walmart => chart::WALMART_CLEARING,
            Channel::Etsy => chart::ETSY_CLEARING,
        }
    }

    /// Selling / processing fees (posting map, "Selling/processing fees").
    pub const fn fees(self) -> &'static str {
        match self {
            Channel::Shopify => chart::SHOPIFY_FEES,
            Channel::Amazon => chart::AMAZON_FEES,
            Channel::Walmart => chart::WALMART_FEES,
            Channel::Etsy => chart::ETSY_FEES,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PostingKind {
    /// A Shopify payout: activity plus the transfer to the bank.
    Payout,
    /// An Amazon settlement: activity only. Its money reaches the bank as a
    /// separate [`PostingKind::Transfer`].
    Settlement,
    /// Clearing to checking.
    Transfer,
}

impl PostingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            PostingKind::Payout => "payout",
            PostingKind::Settlement => "settlement",
            PostingKind::Transfer => "transfer",
        }
    }
}

/// One leg before it becomes a [`DocLine`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub account: String,
    pub class: Option<String>,
    pub side: Side,
    pub amount: Money,
}

/// A payout, settlement or transfer, ready to post.
#[derive(Clone, Debug)]
pub struct ChannelPosting {
    pub channel: Channel,
    /// `<channel>:<id>`, unique per company. The idempotency key.
    pub external_id: String,
    pub kind: PostingKind,
    pub entry_date: NaiveDate,
    pub memo: String,
    pub legs: Vec<Leg>,
    pub gross: Money,
    pub refunds: Money,
    pub fees: Money,
    pub other: Money,
    /// What the legs put into (or, negative, took out of) clearing before any
    /// transfer leg. Asserted equal to the payout by the builder.
    pub net: Money,
    /// Money to the bank: zero for a settlement, the payout for the rest.
    pub payout: Money,
    pub flags: Vec<String>,
}

/// Accumulates legs, merging nothing (the poster merges), skipping zeros and
/// attaching a class only where §3 wants one.
#[derive(Default, Debug)]
pub struct Legs {
    pub legs: Vec<Leg>,
}

impl Legs {
    fn push(&mut self, account: &str, class: Option<&str>, side: Side, amount: Money) {
        if amount.is_zero() {
            return;
        }
        let class = if post::requires_class(&AccountId(account.to_string())) {
            class.map(str::to_string)
        } else {
            None
        };
        self.legs.push(Leg {
            account: account.to_string(),
            class,
            side,
            amount,
        });
    }

    pub fn debit(&mut self, account: &str, class: Option<&str>, amount: Money) {
        self.push(account, class, Side::Debit, amount);
    }

    pub fn credit(&mut self, account: &str, class: Option<&str>, amount: Money) {
        self.push(account, class, Side::Credit, amount);
    }

    /// `amount` positive credits the account, negative debits its absolute
    /// value. Income accounts and liabilities read naturally this way.
    pub fn signed_credit(&mut self, account: &str, class: Option<&str>, amount_minor: i64) {
        let m = Money::from_minor(amount_minor.abs());
        if amount_minor >= 0 {
            self.credit(account, class, m);
        } else {
            self.debit(account, class, m);
        }
    }

    /// `amount` positive debits the account, negative credits its absolute
    /// value. Expenses and contra-income read naturally this way.
    pub fn signed_debit(&mut self, account: &str, class: Option<&str>, amount_minor: i64) {
        self.signed_credit(account, class, -amount_minor);
    }

    /// Debits minus credits on `account`, in minor units.
    pub fn net_debit(&self, account: &str) -> i64 {
        self.legs
            .iter()
            .filter(|l| l.account == account)
            .map(|l| match l.side {
                Side::Debit => l.amount.minor(),
                Side::Credit => -l.amount.minor(),
            })
            .sum()
    }
}

/// Splits `total_minor` across `weights` in proportion, by largest remainder,
/// so the parts always add back to the total exactly. Sign is preserved.
/// `weights` with a zero sum allocate nothing (caller decides what that
/// means).
pub fn allocate<K: Clone>(total_minor: i64, weights: &[(K, i64)]) -> Vec<(K, i64)> {
    let sum: i128 = weights.iter().map(|(_, w)| i128::from((*w).max(0))).sum();
    if sum == 0 || total_minor == 0 {
        return Vec::new();
    }
    let sign: i128 = if total_minor < 0 { -1 } else { 1 };
    let abs = i128::from(total_minor).abs();
    let mut parts: Vec<(usize, i128, i128)> = weights
        .iter()
        .enumerate()
        .map(|(i, (_, w))| {
            let w = i128::from((*w).max(0));
            (i, abs * w / sum, abs * w % sum)
        })
        .collect();
    let allocated: i128 = parts.iter().map(|p| p.1).sum();
    let mut left = abs - allocated;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by(|a, b| parts[*b].2.cmp(&parts[*a].2).then(a.cmp(b)));
    for i in order {
        if left == 0 {
            break;
        }
        if weights[i].1 > 0 {
            parts[i].1 += 1;
            left -= 1;
        }
    }
    parts
        .into_iter()
        .filter(|p| p.1 != 0)
        .map(|(i, v, _)| (weights[i].0.clone(), (sign * v) as i64))
        .collect()
}

/// Weights by class for spreading a cost that has no product of its own
/// (fees, chargebacks, advertising): the revenue the same posting booked to
/// each class, or `fallback` when it booked none.
pub fn class_weights(revenue_by_class: &BTreeMap<String, i64>, fallback: &str) -> Vec<(String, i64)> {
    let w: Vec<(String, i64)> = revenue_by_class
        .iter()
        .filter(|(_, v)| **v > 0)
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    if w.is_empty() {
        vec![(fallback.to_string(), 1)]
    } else {
        w
    }
}

impl ChannelPosting {
    /// `clearing` net, debits minus credits, over the legs.
    pub fn clearing_net(&self) -> i64 {
        let acct = self.channel.clearing();
        self.legs
            .iter()
            .filter(|l| l.account == acct)
            .map(|l| match l.side {
                Side::Debit => l.amount.minor(),
                Side::Credit => -l.amount.minor(),
            })
            .sum()
    }

    /// Whether the legs balance.
    pub fn is_balanced(&self) -> bool {
        let mut d = 0i64;
        let mut c = 0i64;
        for l in &self.legs {
            match l.side {
                Side::Debit => d += l.amount.minor(),
                Side::Credit => c += l.amount.minor(),
            }
        }
        d == c && d > 0
    }

    /// The document [`crate::post::post`] turns into an entry.
    pub fn to_document(&self) -> LedgerDocument {
        let lines = self
            .legs
            .iter()
            .enumerate()
            .map(|(i, l)| DocLine {
                line_no: i as i64 + 1,
                kind: LineKind::Journal,
                amount: l.amount,
                class: l.class.clone().map(ClassId),
                item_id: None,
                account: Some(AccountId(l.account.clone())),
                is_taxable: false,
                qty: None,
                unit_cost: None,
                description: None,
                posting: Some(l.side),
                entity: None,
            })
            .collect();
        LedgerDocument {
            document_id: format!("channel:{}", self.external_id),
            kind: DocKind::ChannelPosting,
            number: None,
            txn_date: self.entry_date,
            due_date: None,
            contact: None,
            header_class: None,
            lines,
            tax: None,
            deposit_to: None,
            pay_from: None,
            applications: Vec::new(),
            unapplied: Money::ZERO,
            is_voided: false,
            source_ref: Some(self.external_id.clone()),
            memo: Some(self.memo.clone()),
        }
    }
}

/// What committing one posting did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Committed {
    Posted,
    /// The external id was already in `channel_postings`: no-op.
    Skipped,
    /// Already imported, but the same id now produces different amounts.
    /// Nothing is replaced; a quarantine row records the difference.
    Changed,
}

/// Saves and posts `posting` unless its external id is already known.
pub fn commit(
    ledger: &Ledger,
    company: &str,
    posting: &ChannelPosting,
    now: DateTime<Utc>,
) -> Result<Committed, ChannelError> {
    if let Some(existing) = db::existing_posting(ledger, company, &posting.external_id)? {
        if existing.net_minor != posting.net.minor() || existing.payout_minor != posting.payout.minor() {
            db::quarantine(
                ledger,
                company,
                posting.channel,
                &posting.external_id,
                "changed_since_import",
                &format!(
                    "imported net {} payout {}, now net {} payout {}",
                    existing.net_minor,
                    existing.payout_minor,
                    posting.net.minor(),
                    posting.payout.minor()
                ),
                now,
            )?;
            return Ok(Committed::Changed);
        }
        return Ok(Committed::Skipped);
    }

    let doc = posting.to_document();
    let version = ledger.save_document(
        company,
        &doc,
        CommandMeta {
            actor_id: "channel-import".to_string(),
            kind: format!("channel_import_{}", posting.channel.as_str()),
            hlc: now.to_rfc3339(),
        },
        now,
    )?;
    let ctx = PostingContext::default();
    match post::post(&doc, version.version, &ctx)? {
        Posting::Entry(entry) => {
            ledger.post_entry(company, &entry, now)?;
        }
        Posting::NonPosting => unreachable!("channel postings always post"),
    }
    db::insert_posting(ledger, company, posting, now)?;
    Ok(Committed::Posted)
}

/// Totals an importer prints and a caller can assert on.
#[derive(Default, Debug, Clone)]
pub struct ImportSummary {
    pub posted: usize,
    pub skipped: usize,
    pub changed: usize,
    /// Held back with a reason (also in `channel_quarantine`).
    pub quarantined: Vec<(String, String)>,
    /// Posted, but with something a person should look at.
    pub flagged: Vec<(String, String)>,
    /// Refused by the ledger (closed period, unknown account).
    pub rejected: Vec<(String, String)>,
    /// Payouts not posted because they have not landed yet.
    pub pending: Vec<String>,
    pub facts_inserted: usize,
    pub facts_existing: usize,
    /// Rows deliberately left out (gateway amazon/ebay mirrors).
    pub excluded: usize,
    pub unmapped_skus: usize,
}

impl ImportSummary {
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let mut line = |s: String| {
            out.push_str(&s);
            out.push('\n');
        };
        line(format!("postings posted    : {}", self.posted));
        line(format!("skipped (already imported): {}", self.skipped));
        if self.changed > 0 {
            line(format!("changed since import (not replaced): {}", self.changed));
        }
        if !self.pending.is_empty() {
            line(format!("pending (not yet paid out): {}", self.pending.len()));
        }
        if self.facts_inserted + self.facts_existing > 0 {
            line(format!(
                "order lines        : {} new, {} already present",
                self.facts_inserted, self.facts_existing
            ));
        }
        if self.excluded > 0 {
            line(format!("excluded (marketplace mirrors): {}", self.excluded));
        }
        if self.unmapped_skus > 0 {
            line(format!("unmapped SKUs (Custom / Other, flagged): {}", self.unmapped_skus));
        }
        line(format!("flagged            : {}", self.flagged.len()));
        for (id, why) in &self.flagged {
            line(format!("  - {id}: {why}"));
        }
        line(format!("quarantined        : {}", self.quarantined.len()));
        for (id, why) in &self.quarantined {
            line(format!("  - {id}: {why}"));
        }
        line(format!("rejected           : {}", self.rejected.len()));
        for (id, why) in &self.rejected {
            line(format!("  - {id}: {why}"));
        }
        out
    }
}

/// Commits `posting`, folding the outcome into `summary`. Quarantines and
/// ledger rejections are recorded, not raised, so one bad payout never stops
/// the rest of a file.
pub fn commit_into(
    ledger: &Ledger,
    company: &str,
    posting: &ChannelPosting,
    summary: &mut ImportSummary,
    now: DateTime<Utc>,
) {
    match commit(ledger, company, posting, now) {
        Ok(Committed::Posted) => {
            summary.posted += 1;
            for flag in &posting.flags {
                summary.flagged.push((posting.external_id.clone(), flag.clone()));
            }
        }
        Ok(Committed::Skipped) => summary.skipped += 1,
        Ok(Committed::Changed) => {
            summary.changed += 1;
            summary
                .quarantined
                .push((posting.external_id.clone(), "changed_since_import".into()));
        }
        Err(err) => summary
            .rejected
            .push((posting.external_id.clone(), err.to_string())),
    }
}

/// Records a quarantine and mirrors it into `summary`.
#[allow(clippy::too_many_arguments)]
pub fn quarantine_into(
    ledger: &Ledger,
    company: &str,
    channel: Channel,
    external_id: &str,
    reason: &str,
    detail: &str,
    summary: &mut ImportSummary,
    now: DateTime<Utc>,
) {
    let _ = db::quarantine(ledger, company, channel, external_id, reason, detail, now);
    summary
        .quarantined
        .push((external_id.to_string(), format!("{reason}: {detail}")));
}

pub fn fmt_money(m: Money) -> String {
    let minor = m.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_sums_exactly_and_keeps_sign() {
        let parts = allocate(100, &[("a", 1), ("b", 1), ("c", 1)]);
        assert_eq!(parts.iter().map(|p| p.1).sum::<i64>(), 100);
        let neg = allocate(-101, &[("a", 3), ("b", 7)]);
        assert_eq!(neg.iter().map(|p| p.1).sum::<i64>(), -101);
        assert!(neg.iter().all(|p| p.1 < 0));
        assert!(allocate(50, &[("a", 0)]).is_empty());
        // Largest remainder: 10 over 1:2 is 3 and 7.
        assert_eq!(allocate(10, &[("a", 1), ("b", 2)]), vec![("a", 3), ("b", 7)]);
    }

    #[test]
    fn class_weights_fall_back_when_no_revenue() {
        let empty = BTreeMap::new();
        assert_eq!(class_weights(&empty, "foam"), vec![("foam".to_string(), 1)]);
        let mut m = BTreeMap::new();
        m.insert("chair".to_string(), 500);
        m.insert("foam".to_string(), 0);
        assert_eq!(class_weights(&m, "foam"), vec![("chair".to_string(), 500)]);
    }

    #[test]
    fn legs_skip_zero_and_class_only_where_required() {
        let mut l = Legs::default();
        l.debit("1170", Some("foam"), Money::from_minor(100));
        l.credit("4100", Some("foam"), Money::from_minor(100));
        l.credit("4100", Some("foam"), Money::ZERO);
        assert_eq!(l.legs.len(), 2);
        assert_eq!(l.legs[0].class, None, "balance sheet legs carry no class");
        assert_eq!(l.legs[1].class.as_deref(), Some("foam"));
        l.signed_debit("6610", Some("foam"), -50);
        assert_eq!(l.legs[2].side, Side::Credit);
        assert_eq!(l.net_debit("6610"), -50);
    }
}
