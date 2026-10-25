//! The replica's sales-order tables (migration 3). `crate::sales_orders`
//! holds the feed format and every rule about what is open; this module only
//! reads and writes rows. DECISIONS.md D42.

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::Money;
use rusqlite::{params, OptionalExtension, Row};
use rust_decimal::Decimal;

use super::{Store, StoreError};
use crate::domain::RealmId;
use crate::sales_orders::{SalesOrder, SalesOrderFeed, SalesOrderLine};

/// The last feed applied to a realm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SalesOrderSnapshot {
    pub captured_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub source: String,
    pub order_count: i64,
    pub complete: bool,
}

/// What applying one feed did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SalesOrderSyncSummary {
    pub seen: usize,
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// In the replica, missing from a complete feed: marked deleted.
    pub deleted: usize,
    /// Open after the sync.
    pub open: usize,
}

const ORDER_COLUMNS: &str = "qbo_id, doc_number, txn_date, customer_id, customer_name, \
     po_number, due_date, ship_date, order_status, invoice_status, fulfillment_status, \
     total_minor, open_minor, is_open, version, memo, invoice_ids, is_deleted";

fn parse_date(raw: Option<String>) -> Option<NaiveDate> {
    raw.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
}

fn parse_decimal(raw: Option<String>) -> Option<Decimal> {
    raw.and_then(|s| Decimal::from_str(&s).ok())
}

fn ids(raw: String) -> Vec<String> {
    serde_json::from_str(&raw).unwrap_or_default()
}

fn order_from_row(row: &Row<'_>) -> rusqlite::Result<SalesOrder> {
    let txn_date: String = row.get(2)?;
    Ok(SalesOrder {
        qbo_id: row.get(0)?,
        doc_number: row.get(1)?,
        txn_date: NaiveDate::parse_from_str(&txn_date, "%Y-%m-%d").map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
        })?,
        customer_id: row.get(3)?,
        customer_name: row.get(4)?,
        po_number: row.get(5)?,
        due_date: parse_date(row.get(6)?),
        ship_date: parse_date(row.get(7)?),
        order_status: row.get(8)?,
        invoice_status: row.get(9)?,
        fulfillment_status: row.get(10)?,
        total: Money::from_minor(row.get(11)?),
        open_amount: Money::from_minor(row.get(12)?),
        is_open: row.get::<_, i64>(13)? != 0,
        version: row.get(14)?,
        memo: row.get(15)?,
        invoice_ids: ids(row.get(16)?),
        is_deleted: row.get::<_, i64>(17)? != 0,
        lines: Vec::new(),
    })
}

fn line_from_row(row: &Row<'_>) -> rusqlite::Result<SalesOrderLine> {
    Ok(SalesOrderLine {
        line_no: row.get(0)?,
        line_id: row.get(1)?,
        item_id: row.get(2)?,
        description: row.get(3)?,
        qty: parse_decimal(row.get(4)?),
        unit_price: parse_decimal(row.get(5)?),
        amount: Money::from_minor(row.get(6)?),
        is_taxable: row.get::<_, i64>(7)? != 0,
        qty_invoiced: parse_decimal(Some(row.get(8)?)).unwrap_or(Decimal::ZERO),
        qty_open: parse_decimal(row.get(9)?),
        open_amount: Money::from_minor(row.get(10)?),
        invoice_ids: ids(row.get(11)?),
    })
}

impl Store {
    /// Whether this replica has the sales-order tables at all. A replica
    /// still on schema 2 (opened read-only, so never migrated) has none, and
    /// every read below answers as if no feed had been applied.
    fn has_sales_orders(&self) -> Result<bool, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sales_orders'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Apply one feed in one transaction: upsert every order (an order whose
    /// raw payload is unchanged is left alone), and, when the feed is
    /// complete, mark every order it lacks deleted. Records the snapshot.
    /// Every order is projected before anything is written, so a feed with
    /// one bad order changes nothing.
    pub fn apply_sales_order_feed(
        &self,
        realm: &RealmId,
        feed: &SalesOrderFeed,
        source: &str,
        now: DateTime<Utc>,
    ) -> Result<SalesOrderSyncSummary, StoreError> {
        let mut projected = Vec::with_capacity(feed.sales_orders.len());
        for order in &feed.sales_orders {
            let so = SalesOrder::from_feed(order)
                .map_err(|err| StoreError::SalesOrder(err.to_string()))?;
            let raw = serde_json::to_string(order)
                .map_err(|err| StoreError::SalesOrder(err.to_string()))?;
            projected.push((so, raw));
        }

        let mut summary = SalesOrderSyncSummary {
            seen: projected.len(),
            ..SalesOrderSyncSummary::default()
        };
        let tx = self.connection.unchecked_transaction()?;
        let now_text = now.to_rfc3339();
        for (so, raw) in &projected {
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT raw_json, is_deleted FROM sales_orders WHERE realm_id = ?1 AND qbo_id = ?2",
                    params![realm.as_str(), so.qbo_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match &existing {
                Some((old_raw, 0)) if old_raw == raw => {
                    summary.unchanged += 1;
                    continue;
                }
                Some(_) => summary.updated += 1,
                None => summary.inserted += 1,
            }
            tx.execute(
                "DELETE FROM sales_order_lines WHERE realm_id = ?1 AND so_qbo_id = ?2",
                params![realm.as_str(), so.qbo_id],
            )?;
            let invoice_ids =
                serde_json::to_string(&so.invoice_ids).unwrap_or_else(|_| "[]".into());
            tx.execute(
                "INSERT INTO sales_orders (realm_id, qbo_id, doc_number, txn_date, customer_id,
                     customer_name, po_number, due_date, ship_date, order_status, invoice_status,
                     fulfillment_status, total_minor, open_minor, is_open, version, memo,
                     invoice_ids, is_deleted, raw_json, mirrored_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, 0, ?19, ?20)
                 ON CONFLICT (realm_id, qbo_id) DO UPDATE SET
                     doc_number = excluded.doc_number, txn_date = excluded.txn_date,
                     customer_id = excluded.customer_id, customer_name = excluded.customer_name,
                     po_number = excluded.po_number, due_date = excluded.due_date,
                     ship_date = excluded.ship_date, order_status = excluded.order_status,
                     invoice_status = excluded.invoice_status,
                     fulfillment_status = excluded.fulfillment_status,
                     total_minor = excluded.total_minor, open_minor = excluded.open_minor,
                     is_open = excluded.is_open, version = excluded.version,
                     memo = excluded.memo, invoice_ids = excluded.invoice_ids, is_deleted = 0,
                     raw_json = excluded.raw_json, mirrored_at = excluded.mirrored_at",
                params![
                    realm.as_str(),
                    so.qbo_id,
                    so.doc_number,
                    so.txn_date.to_string(),
                    so.customer_id,
                    so.customer_name,
                    so.po_number,
                    so.due_date.map(|d| d.to_string()),
                    so.ship_date.map(|d| d.to_string()),
                    so.order_status,
                    so.invoice_status,
                    so.fulfillment_status,
                    so.total.minor(),
                    so.open_amount.minor(),
                    so.is_open as i64,
                    so.version,
                    so.memo,
                    invoice_ids,
                    raw,
                    now_text,
                ],
            )?;
            for line in &so.lines {
                tx.execute(
                    "INSERT INTO sales_order_lines (realm_id, so_qbo_id, line_no, line_id, item_id,
                         description, qty, unit_price, amount_minor, is_taxable, qty_invoiced,
                         qty_open, open_minor, invoice_ids)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        realm.as_str(),
                        so.qbo_id,
                        line.line_no,
                        line.line_id,
                        line.item_id,
                        line.description,
                        line.qty.map(|d| d.to_string()),
                        line.unit_price.map(|d| d.to_string()),
                        line.amount.minor(),
                        line.is_taxable as i64,
                        line.qty_invoiced.to_string(),
                        line.qty_open.map(|d| d.to_string()),
                        line.open_amount.minor(),
                        serde_json::to_string(&line.invoice_ids).unwrap_or_else(|_| "[]".into()),
                    ],
                )?;
            }
        }

        if feed.complete {
            let present: std::collections::HashSet<&str> =
                projected.iter().map(|(so, _)| so.qbo_id.as_str()).collect();
            let held: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT qbo_id FROM sales_orders WHERE realm_id = ?1 AND is_deleted = 0",
                )?;
                let rows = stmt.query_map(params![realm.as_str()], |row| row.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            for qbo_id in held.iter().filter(|id| !present.contains(id.as_str())) {
                tx.execute(
                    "UPDATE sales_orders SET is_deleted = 1, is_open = 0, mirrored_at = ?3
                     WHERE realm_id = ?1 AND qbo_id = ?2",
                    params![realm.as_str(), qbo_id, now_text],
                )?;
                summary.deleted += 1;
            }
        }

        tx.execute(
            "INSERT INTO sales_order_snapshots (realm_id, captured_at, synced_at, source,
                 order_count, complete)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (realm_id) DO UPDATE SET captured_at = excluded.captured_at,
                 synced_at = excluded.synced_at, source = excluded.source,
                 order_count = excluded.order_count, complete = excluded.complete",
            params![
                realm.as_str(),
                feed.captured_at.to_rfc3339(),
                now_text,
                source,
                projected.len() as i64,
                feed.complete as i64,
            ],
        )?;
        summary.open = tx.query_row(
            "SELECT COUNT(*) FROM sales_orders WHERE realm_id = ?1 AND is_open = 1 AND is_deleted = 0",
            params![realm.as_str()],
            |row| row.get::<_, i64>(0),
        )? as usize;
        tx.commit()?;
        Ok(summary)
    }

    /// The last feed applied to `realm`, if any.
    pub fn sales_order_snapshot(
        &self,
        realm: &RealmId,
    ) -> Result<Option<SalesOrderSnapshot>, StoreError> {
        if !self.has_sales_orders()? {
            return Ok(None);
        }
        let row: Option<(String, String, String, i64, i64)> = self
            .connection
            .query_row(
                "SELECT captured_at, synced_at, source, order_count, complete
                 FROM sales_order_snapshots WHERE realm_id = ?1",
                params![realm.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let parse = |raw: &str| {
            DateTime::parse_from_rfc3339(raw)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|err| StoreError::SalesOrder(format!("bad snapshot time {raw:?}: {err}")))
        };
        row.map(|(captured, synced, source, order_count, complete)| {
            Ok(SalesOrderSnapshot {
                captured_at: parse(&captured)?,
                synced_at: parse(&synced)?,
                source,
                order_count,
                complete: complete != 0,
            })
        })
        .transpose()
    }

    /// Every mirrored sales order of `realm` with its lines, deleted ones
    /// left out; only the open ones when `open_only`. Oldest first.
    pub fn list_sales_orders(
        &self,
        realm: &RealmId,
        open_only: bool,
    ) -> Result<Vec<SalesOrder>, StoreError> {
        if !self.has_sales_orders()? {
            return Ok(Vec::new());
        }
        let mut stmt = self.connection.prepare(&format!(
            "SELECT {ORDER_COLUMNS} FROM sales_orders
             WHERE realm_id = ?1 AND is_deleted = 0 AND (?2 = 0 OR is_open = 1)
             ORDER BY txn_date, doc_number, qbo_id"
        ))?;
        let orders = stmt
            .query_map(params![realm.as_str(), open_only as i64], order_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut line_stmt = self.connection.prepare(
            "SELECT line_no, line_id, item_id, description, qty, unit_price, amount_minor,
                    is_taxable, qty_invoiced, qty_open, open_minor, invoice_ids
             FROM sales_order_lines WHERE realm_id = ?1 AND so_qbo_id = ?2 ORDER BY line_no",
        )?;
        let mut out = Vec::with_capacity(orders.len());
        for mut order in orders {
            order.lines = line_stmt
                .query_map(params![realm.as_str(), order.qbo_id], line_from_row)?
                .collect::<Result<_, _>>()?;
            out.push(order);
        }
        Ok(out)
    }

    /// One sales order by its commerce id, deleted or not.
    pub fn get_sales_order(
        &self,
        realm: &RealmId,
        qbo_id: &str,
    ) -> Result<Option<SalesOrder>, StoreError> {
        if !self.has_sales_orders()? {
            return Ok(None);
        }
        let order = self
            .connection
            .query_row(
                &format!(
                    "SELECT {ORDER_COLUMNS} FROM sales_orders WHERE realm_id = ?1 AND qbo_id = ?2"
                ),
                params![realm.as_str(), qbo_id],
                order_from_row,
            )
            .optional()?;
        let Some(mut order) = order else {
            return Ok(None);
        };
        let mut stmt = self.connection.prepare(
            "SELECT line_no, line_id, item_id, description, qty, unit_price, amount_minor,
                    is_taxable, qty_invoiced, qty_open, open_minor, invoice_ids
             FROM sales_order_lines WHERE realm_id = ?1 AND so_qbo_id = ?2 ORDER BY line_no",
        )?;
        order.lines = stmt
            .query_map(params![realm.as_str(), qbo_id], line_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(Some(order))
    }
}
