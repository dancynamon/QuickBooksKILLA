//! Channel reports. Read-only.
//!
//! - [`channel_sales`]: units, gross, refunds and net by channel x family
//!   (or SKU) x month, from `order_lines` (dated by order date, refunds by
//!   refund date). This is what feeds the rescue tube and manufactured
//!   product reports.
//! - [`channel_recon`]: per channel x month, what the postings booked (gross
//!   product sales, shipping, refunds, fees, net), what reached the bank,
//!   and the clearing balance at month end. Dated by posting date (payout
//!   date for Shopify, last activity date for an Amazon settlement).

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::params;

use super::{fmt_money, Channel, ChannelError};
use crate::store::Ledger;
use ledger_core::Money;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum By {
    Family,
    Sku,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SalesRow {
    pub channel: String,
    pub month: String,
    pub family: String,
    /// Only with `By::Sku`.
    pub sku: Option<String>,
    pub units: i64,
    pub refund_units: i64,
    pub gross_minor: i64,
    /// Negative.
    pub refunds_minor: i64,
}

impl SalesRow {
    pub fn net_minor(&self) -> i64 {
        self.gross_minor + self.refunds_minor
    }
}

pub fn channel_sales(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
    by: By,
    channel: Option<Channel>,
) -> Result<Vec<SalesRow>, ChannelError> {
    let sku_col = match by {
        By::Family => "''",
        By::Sku => "upper(sku)",
    };
    let sql = format!(
        "SELECT channel, substr(order_date, 1, 7) AS month, family, {sku_col} AS sku,
                COALESCE(SUM(CASE WHEN kind = 'sale'   THEN qty END), 0),
                COALESCE(SUM(CASE WHEN kind = 'refund' THEN qty END), 0),
                COALESCE(SUM(CASE WHEN kind = 'sale'   THEN amount_minor END), 0),
                COALESCE(SUM(CASE WHEN kind = 'refund' THEN amount_minor END), 0)
         FROM order_lines
         WHERE company_id = ?1 AND order_date >= ?2 AND order_date <= ?3
           AND (?4 IS NULL OR channel = ?4)
         GROUP BY channel, month, family, sku
         ORDER BY channel, month, family, sku"
    );
    let mut stmt = ledger.conn().prepare(&sql)?;
    let rows = stmt
        .query_map(
            params![
                company,
                from.to_string(),
                to.to_string(),
                channel.map(Channel::as_str)
            ],
            |row| {
                let sku: String = row.get(3)?;
                Ok(SalesRow {
                    channel: row.get(0)?,
                    month: row.get(1)?,
                    family: row.get(2)?,
                    sku: if by == By::Sku { Some(sku) } else { None },
                    units: row.get(4)?,
                    refund_units: row.get(5)?,
                    gross_minor: row.get(6)?,
                    refunds_minor: row.get(7)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn dec(minor: i64) -> String {
    fmt_money(Money::from_minor(minor))
}

pub fn sales_csv(rows: &[SalesRow], by: By) -> String {
    let mut out = String::new();
    out.push_str("channel,month,family,");
    if by == By::Sku {
        out.push_str("sku,");
    }
    out.push_str("units,refund_units,gross,refunds,net\n");
    let q = |s: &str| {
        if s.contains(',') || s.contains('"') || s.contains('\n') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    for r in rows {
        out.push_str(&format!("{},{},{},", r.channel, r.month, q(&r.family)));
        if let Some(sku) = &r.sku {
            out.push_str(&format!("{},", q(sku)));
        }
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            r.units,
            r.refund_units,
            dec(r.gross_minor),
            dec(r.refunds_minor),
            dec(r.net_minor())
        ));
    }
    out
}

pub fn sales_json(rows: &[SalesRow], from: NaiveDate, to: NaiveDate, by: By) -> serde_json::Value {
    use serde_json::json;
    let total = |f: fn(&SalesRow) -> i64| rows.iter().map(f).sum::<i64>();
    json!({
        "from": from.to_string(),
        "to": to.to_string(),
        "by": match by { By::Family => "family", By::Sku => "sku" },
        "rows": rows.iter().map(|r| json!({
            "channel": r.channel,
            "month": r.month,
            "family": r.family,
            "sku": r.sku,
            "units": r.units,
            "refund_units": r.refund_units,
            "gross": dec(r.gross_minor),
            "refunds": dec(r.refunds_minor),
            "net": dec(r.net_minor()),
            "gross_minor": r.gross_minor,
            "refunds_minor": r.refunds_minor,
            "net_minor": r.net_minor(),
        })).collect::<Vec<_>>(),
        "totals": {
            "units": total(|r| r.units),
            "refund_units": total(|r| r.refund_units),
            "gross": dec(total(|r| r.gross_minor)),
            "refunds": dec(total(|r| r.refunds_minor)),
            "net": dec(total(|r| r.net_minor())),
        },
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconRow {
    pub channel: String,
    pub month: String,
    /// Credits less debits on 4100.
    pub gross_product_sales: i64,
    /// Credits less debits on 4300.
    pub shipping: i64,
    /// Debits less credits on 4900.
    pub discounts: i64,
    /// Debits less credits on 4950.
    pub refunds: i64,
    /// Credits less debits on 4985: sales that could not be split by family.
    pub unallocated: i64,
    /// Debits less credits on the channel's fee, advertising, subscription
    /// and label accounts (6610 to 6617).
    pub fees: i64,
    /// Debits less credits on 6620 and 6621.
    pub adjustments: i64,
    /// What the postings put into clearing, before transfers.
    pub net: i64,
    /// Transfers to checking.
    pub payouts: i64,
    /// Clearing balance at month end, from the first posting imported.
    pub clearing_balance: i64,
}

/// Postings by channel and month, from the journal itself.
pub fn channel_recon(
    ledger: &Ledger,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<ReconRow>, ChannelError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT p.channel, substr(e.entry_date, 1, 7), l.account_id,
                SUM(l.debit_minor), SUM(l.credit_minor)
         FROM channel_postings p
         JOIN journal_entries e ON e.company_id = p.company_id
              AND e.source_id = p.document_id AND e.is_posted = 1
         JOIN journal_lines l ON l.company_id = e.company_id AND l.entry_id = e.entry_id
         WHERE p.company_id = ?1 AND e.entry_date <= ?3
         GROUP BY p.channel, substr(e.entry_date, 1, 7), l.account_id
         ORDER BY p.channel, substr(e.entry_date, 1, 7)",
    )?;
    let raw = stmt
        .query_map(
            params![company, from.to_string(), to.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    let mut months: BTreeMap<(String, String), ReconRow> = BTreeMap::new();
    let mut clearing_running: BTreeMap<String, i64> = BTreeMap::new();
    let mut clearing_by_month: BTreeMap<(String, String), i64> = BTreeMap::new();
    let from_month = from.format("%Y-%m").to_string();
    for (channel, month, account, debit, credit) in raw {
        let clearing_account = Channel::parse(&channel).map(Channel::clearing);
        if Some(account.as_str()) == clearing_account {
            *clearing_by_month
                .entry((channel.clone(), month.clone()))
                .or_insert(0) += debit - credit;
        }
        if month < from_month {
            continue;
        }
        let row = months
            .entry((channel.clone(), month.clone()))
            .or_insert_with(|| ReconRow {
                channel: channel.clone(),
                month: month.clone(),
                ..ReconRow::default()
            });
        match account.as_str() {
            "4100" => row.gross_product_sales += credit - debit,
            "4300" => row.shipping += credit - debit,
            "4900" => row.discounts += debit - credit,
            "4950" => row.refunds += debit - credit,
            "4985" => row.unallocated += credit - debit,
            "6610" | "6611" | "6612" | "6613" | "6614" | "6615" | "6616" | "6617" => {
                row.fees += debit - credit;
            }
            "6620" | "6621" => row.adjustments += debit - credit,
            "1100" => row.payouts += debit - credit,
            _ => {}
        }
    }

    // Net into clearing per month = clearing movement + payouts.
    for ((channel, month), row) in &mut months {
        let moved = clearing_by_month
            .get(&(channel.clone(), month.clone()))
            .copied()
            .unwrap_or(0);
        row.net = moved + row.payouts;
    }
    // Running clearing balance, month by month, from the first posting.
    let mut all_months: Vec<(String, String)> = clearing_by_month.keys().cloned().collect();
    all_months.sort();
    let mut balance_at: BTreeMap<(String, String), i64> = BTreeMap::new();
    for key in all_months {
        let run = clearing_running.entry(key.0.clone()).or_insert(0);
        *run += clearing_by_month[&key];
        balance_at.insert(key, *run);
    }
    for ((channel, month), row) in &mut months {
        row.clearing_balance = balance_at
            .get(&(channel.clone(), month.clone()))
            .copied()
            .unwrap_or(0);
    }
    let to_month = to.format("%Y-%m").to_string();
    Ok(months
        .into_values()
        .filter(|r| r.month <= to_month)
        .collect())
}

pub fn recon_json(rows: &[ReconRow]) -> serde_json::Value {
    use serde_json::json;
    json!({
        "rows": rows.iter().map(|r| json!({
            "channel": r.channel,
            "month": r.month,
            "gross_product_sales": dec(r.gross_product_sales),
            "shipping": dec(r.shipping),
            "discounts": dec(r.discounts),
            "refunds": dec(r.refunds),
            "unallocated": dec(r.unallocated),
            "fees": dec(r.fees),
            "adjustments": dec(r.adjustments),
            "net": dec(r.net),
            "payouts": dec(r.payouts),
            "clearing_balance": dec(r.clearing_balance),
        })).collect::<Vec<_>>(),
    })
}

pub fn recon_csv(rows: &[ReconRow]) -> String {
    let mut out = String::from(
        "channel,month,gross_product_sales,shipping,discounts,refunds,unallocated,fees,adjustments,net,payouts,clearing_balance\n",
    );
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.channel,
            r.month,
            dec(r.gross_product_sales),
            dec(r.shipping),
            dec(r.discounts),
            dec(r.refunds),
            dec(r.unallocated),
            dec(r.fees),
            dec(r.adjustments),
            dec(r.net),
            dec(r.payouts),
            dec(r.clearing_balance)
        ));
    }
    out
}
