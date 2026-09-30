//! The channel tables (migration 6) and the setup an importer needs before
//! it can post: accounts, product families, classes.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};

use super::sku::{self, SkuMapper, FAMILIES};
use super::{Channel, ChannelError, ChannelPosting};
use crate::chart;
use crate::store::Ledger;

pub struct ExistingPosting {
    pub net_minor: i64,
    pub payout_minor: i64,
}

pub fn existing_posting(
    ledger: &Ledger,
    company: &str,
    external_id: &str,
) -> Result<Option<ExistingPosting>, ChannelError> {
    Ok(ledger
        .conn()
        .query_row(
            "SELECT net_minor, payout_minor FROM channel_postings
             WHERE company_id = ?1 AND external_id = ?2",
            params![company, external_id],
            |row| {
                Ok(ExistingPosting {
                    net_minor: row.get(0)?,
                    payout_minor: row.get(1)?,
                })
            },
        )
        .optional()?)
}

pub fn insert_posting(
    ledger: &Ledger,
    company: &str,
    p: &ChannelPosting,
    now: DateTime<Utc>,
) -> Result<(), ChannelError> {
    ledger.conn().execute(
        "INSERT INTO channel_postings
             (company_id, external_id, channel, kind, document_id, entry_date,
              gross_minor, refunds_minor, fees_minor, other_minor, net_minor,
              payout_minor, flags, posted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            company,
            p.external_id,
            p.channel.as_str(),
            p.kind.as_str(),
            format!("channel:{}", p.external_id),
            p.entry_date.to_string(),
            p.gross.minor(),
            p.refunds.minor(),
            p.fees.minor(),
            p.other.minor(),
            p.net.minor(),
            p.payout.minor(),
            p.flags.join("; "),
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn quarantine(
    ledger: &Ledger,
    company: &str,
    channel: Channel,
    external_id: &str,
    reason: &str,
    detail: &str,
    now: DateTime<Utc>,
) -> Result<(), ChannelError> {
    ledger.conn().execute(
        "INSERT INTO channel_quarantine
             (company_id, external_id, channel, reason, detail, flagged_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(company_id, external_id, reason) DO UPDATE SET
             detail = excluded.detail, flagged_at = excluded.flagged_at",
        params![
            company,
            external_id,
            channel.as_str(),
            reason,
            detail,
            now.to_rfc3339()
        ],
    )?;
    Ok(())
}

/// Removes a quarantine row once the same id posts cleanly.
pub fn clear_quarantine(ledger: &Ledger, company: &str, external_id: &str) -> Result<(), ChannelError> {
    ledger.conn().execute(
        "DELETE FROM channel_quarantine WHERE company_id = ?1 AND external_id = ?2",
        params![company, external_id],
    )?;
    Ok(())
}

pub struct QuarantineRow {
    pub external_id: String,
    pub channel: String,
    pub reason: String,
    pub detail: String,
}

pub fn list_quarantine(ledger: &Ledger, company: &str) -> Result<Vec<QuarantineRow>, ChannelError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT external_id, channel, reason, detail FROM channel_quarantine
         WHERE company_id = ?1 ORDER BY external_id",
    )?;
    let rows = stmt
        .query_map(params![company], |row| {
            Ok(QuarantineRow {
                external_id: row.get(0)?,
                channel: row.get(1)?,
                reason: row.get(2)?,
                detail: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The D26 channel accounts and product families, inserted if missing and
/// never renamed (a chart the operator edited stays edited). Fails with a
/// clear message when the company has not been `ledger init`-ed.
pub fn ensure_setup(ledger: &Ledger, company: &str) -> Result<(), ChannelError> {
    let exists: i64 = ledger.conn().query_row(
        "SELECT COUNT(*) FROM companies WHERE company_id = ?1",
        params![company],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Err(ChannelError::Quarantine(format!(
            "company {company:?} does not exist; run `ledger init` first"
        )));
    }
    for seed in chart::seed_chart() {
        let is_channel = matches!(
            seed.number,
            chart::SHOPIFY_CLEARING
                | chart::AMAZON_CLEARING
                | chart::WALMART_CLEARING
                | chart::ETSY_CLEARING
                | chart::SHOPIFY_RESERVE
                | chart::AMAZON_RESERVE
                | chart::WALMART_RESERVE
                | chart::CHANNEL_UNALLOCATED
                | chart::SHOPIFY_FEES
                | chart::AMAZON_FEES
                | chart::AMAZON_FBA_FEES
                | chart::WALMART_FEES
                | chart::ETSY_FEES
                | chart::AMAZON_ADVERTISING
                | chart::AMAZON_SUBSCRIPTION
                | chart::CHANNEL_SHIPPING_LABELS
                | chart::CHANNEL_ADJUSTMENTS
                | chart::AMAZON_ADJUSTMENTS
        );
        if !is_channel {
            continue;
        }
        let debit_normal = matches!(
            seed.classification.normal_balance(),
            crate::types::Side::Debit
        ) != seed.is_contra;
        let side = if debit_normal { "Dr" } else { "Cr" };
        ledger.conn().execute(
            "INSERT INTO accounts
                 (company_id, account_id, number, name, classification, subtype,
                  parent_id, normal_balance, is_contra, is_active, needs_mapping, source_ref)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, NULL, ?5, ?6, 1, 0, NULL)
             ON CONFLICT(company_id, account_id) DO NOTHING",
            params![
                company,
                seed.number,
                seed.name,
                seed.classification.as_str(),
                side,
                i64::from(seed.is_contra),
            ],
        )?;
    }
    for (family, class, order) in FAMILIES {
        ledger.conn().execute(
            "INSERT INTO product_families (company_id, family, class_id, sort_order)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(company_id, family) DO NOTHING",
            params![company, family, class, order],
        )?;
    }
    Ok(())
}

/// family -> class for this company (operator edits win over the defaults).
pub fn family_classes(ledger: &Ledger, company: &str) -> Result<HashMap<String, String>, ChannelError> {
    let mut map: HashMap<String, String> = FAMILIES
        .iter()
        .map(|(f, c, _)| ((*f).to_string(), (*c).to_string()))
        .collect();
    let mut stmt = ledger
        .conn()
        .prepare("SELECT family, class_id FROM product_families WHERE company_id = ?1")?;
    let rows = stmt.query_map(params![company], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (f, c) = row?;
        map.insert(f, c);
    }
    Ok(map)
}

/// The operator's manual overrides.
pub fn load_mapper(ledger: &Ledger, company: &str) -> Result<SkuMapper, ChannelError> {
    let mut mapper = SkuMapper::new();
    let mut stmt = ledger.conn().prepare(
        "SELECT channel, sku, family FROM sku_map WHERE company_id = ?1 AND source = 'manual'",
    )?;
    let rows = stmt.query_map(params![company], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (channel, sku_code, family) = row?;
        mapper.set_manual(&channel, &sku_code, &family);
    }
    Ok(mapper)
}

/// Records what the mapper decided for a SKU, so it is visible and
/// correctable. Never overwrites a `manual` row.
pub fn remember_sku(
    ledger: &Ledger,
    company: &str,
    channel: Channel,
    sku_code: &str,
    title: &str,
    mapped: sku::Mapped,
    now: DateTime<Utc>,
) -> Result<(), ChannelError> {
    if sku_code.trim().is_empty() {
        return Ok(());
    }
    let source = if mapped.is_flagged() { "unmapped" } else { "rule" };
    ledger.conn().execute(
        "INSERT INTO sku_map (company_id, channel, sku, family, source, title, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(company_id, channel, sku) DO UPDATE SET
             family = excluded.family, source = excluded.source,
             title = excluded.title, updated_at = excluded.updated_at
         WHERE sku_map.source != 'manual'",
        params![
            company,
            channel.as_str(),
            sku_code.trim().to_uppercase(),
            mapped.family,
            source,
            title,
            now.to_rfc3339()
        ],
    )?;
    Ok(())
}

/// Operator override: `(channel, sku) -> family`.
pub fn set_manual_sku(
    ledger: &Ledger,
    company: &str,
    channel: &str,
    sku_code: &str,
    family: &str,
    now: DateTime<Utc>,
) -> Result<(), ChannelError> {
    if !sku::is_family(family) {
        return Err(ChannelError::Parse(format!("{family:?} is not a product family")));
    }
    if Channel::parse(channel).is_none() {
        return Err(ChannelError::Parse(format!("unknown channel {channel:?}")));
    }
    ledger.conn().execute(
        "INSERT INTO sku_map (company_id, channel, sku, family, source, title, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'manual', NULL, ?5)
         ON CONFLICT(company_id, channel, sku) DO UPDATE SET
             family = excluded.family, source = 'manual', updated_at = excluded.updated_at",
        params![company, channel, sku_code.trim().to_uppercase(), family, now.to_rfc3339()],
    )?;
    // Facts already stored under the old family follow the override.
    ledger.conn().execute(
        "UPDATE order_lines SET family = ?4
         WHERE company_id = ?1 AND channel = ?2 AND upper(sku) = ?3",
        params![company, channel, sku_code.trim().to_uppercase(), family],
    )?;
    Ok(())
}

pub struct SkuRow {
    pub channel: String,
    pub sku: String,
    pub family: String,
    pub source: String,
    pub title: Option<String>,
}

pub fn list_sku_map(
    ledger: &Ledger,
    company: &str,
    only_unmapped: bool,
) -> Result<Vec<SkuRow>, ChannelError> {
    let sql = if only_unmapped {
        "SELECT channel, sku, family, source, title FROM sku_map
         WHERE company_id = ?1 AND source = 'unmapped' ORDER BY channel, sku"
    } else {
        "SELECT channel, sku, family, source, title FROM sku_map
         WHERE company_id = ?1 ORDER BY channel, sku"
    };
    let mut stmt = ledger.conn().prepare(sql)?;
    let rows = stmt
        .query_map(params![company], |row| {
            Ok(SkuRow {
                channel: row.get(0)?,
                sku: row.get(1)?,
                family: row.get(2)?,
                source: row.get(3)?,
                title: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One `order_lines` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub channel: Channel,
    pub order_id: String,
    pub kind: &'static str,
    pub line_key: String,
    pub order_date: chrono::NaiveDate,
    pub sku: String,
    pub title: String,
    pub family: String,
    pub qty: i64,
    pub amount_minor: i64,
}

/// `INSERT OR IGNORE`: true when the row is new.
pub fn insert_fact(ledger: &Ledger, company: &str, f: &Fact) -> Result<bool, ChannelError> {
    let n = ledger.conn().execute(
        "INSERT OR IGNORE INTO order_lines
             (company_id, channel, order_id, kind, line_key, order_date, sku, title,
              family, qty, amount_minor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            company,
            f.channel.as_str(),
            f.order_id,
            f.kind,
            f.line_key,
            f.order_date.to_string(),
            f.sku,
            f.title,
            f.family,
            f.qty,
            f.amount_minor,
        ],
    )?;
    Ok(n > 0)
}

/// Unmapped-SKU counts for a company: `(channel, sku, title, lines)`, most
/// used first.
pub fn unmapped_usage(
    ledger: &Ledger,
    company: &str,
) -> Result<Vec<(String, String, String, i64)>, ChannelError> {
    let mut stmt = ledger.conn().prepare(
        "SELECT o.channel, o.sku, MAX(o.title), COUNT(*)
         FROM order_lines o
         JOIN sku_map m ON m.company_id = o.company_id AND m.channel = o.channel
                       AND m.sku = upper(o.sku)
         WHERE o.company_id = ?1 AND m.source = 'unmapped' AND o.kind = 'sale'
         GROUP BY o.channel, o.sku ORDER BY COUNT(*) DESC, o.sku",
    )?;
    let rows = stmt
        .query_map(params![company], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Revenue by class map helper for callers that keep a running map.
pub fn add_class(map: &mut BTreeMap<String, i64>, class: &str, minor: i64) {
    *map.entry(class.to_string()).or_insert(0) += minor;
}
