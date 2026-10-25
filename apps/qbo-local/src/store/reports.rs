//! Read-only reports over the projection: A/R and A/P aging, sales by
//! customer, and the item list. `DESIGN.md` §9: accrual only in v1.
//!
//! Every figure is computed from mirrored documents, never from a stored
//! total, so a report and the registers behind it cannot disagree.
//!
//! One limitation is stated rather than hidden: aging uses each document's
//! balance as of the last sync, bucketed by days past due on the as-of date.
//! QuickBooks' own as-of aging replays payments to show the balance on that
//! date; for today's date the two agree, for a past date they can differ.

use ledger_core::Money;
use rusqlite::params;

use super::{Store, StoreError};
use crate::domain::RealmId;

/// Which side of the book an aging report covers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AgingSide {
    /// Open invoices: what customers owe.
    Receivable,
    /// Open bills: what is owed to vendors.
    Payable,
}

impl AgingSide {
    const fn doc_type(self) -> &'static str {
        match self {
            AgingSide::Receivable => "Invoice",
            AgingSide::Payable => "Bill",
        }
    }
}

/// One contact's open balance split by days past due.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgingRow {
    pub contact_id: String,
    pub contact_name: Option<String>,
    pub current: Money,
    pub days_1_30: Money,
    pub days_31_60: Money,
    pub days_61_90: Money,
    pub days_over_90: Money,
    pub total: Money,
    pub documents: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SalesRow {
    pub contact_id: String,
    pub contact_name: Option<String>,
    /// Line amounts on invoices and sales receipts, less credit memos and
    /// refund receipts. Lines, not totals, so sales tax is excluded the way
    /// QuickBooks' sales-by-customer report excludes it.
    pub net_sales: Money,
    pub documents: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRow {
    pub qbo_id: String,
    pub name: String,
    pub sku: Option<String>,
    pub item_type: Option<String>,
    pub unit_price: Option<String>,
    pub purchase_cost: Option<String>,
    pub qty_on_hand: Option<String>,
    pub is_active: bool,
    /// Documents with at least one line on this item.
    pub used_on: i64,
}

impl Store {
    /// Aging as of `as_of` (`YYYY-MM-DD`), largest balance first.
    pub fn aging(&self, realm: &RealmId, side: AgingSide, as_of: &str) -> Result<Vec<AgingRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "WITH open_docs AS (
                 SELECT d.contact_id, d.balance_minor AS balance,
                        CAST(julianday(?3) - julianday(COALESCE(d.due_date, d.txn_date)) AS INTEGER) AS late
                 FROM documents d
                 WHERE d.realm_id = ?1 AND d.doc_type = ?2 AND d.is_deleted = 0
                   AND d.balance_minor IS NOT NULL AND d.balance_minor != 0
                   AND d.txn_date <= ?3 AND d.contact_id IS NOT NULL
             )
             SELECT o.contact_id, c.display_name,
                    SUM(CASE WHEN late <= 0 THEN balance ELSE 0 END),
                    SUM(CASE WHEN late BETWEEN 1 AND 30 THEN balance ELSE 0 END),
                    SUM(CASE WHEN late BETWEEN 31 AND 60 THEN balance ELSE 0 END),
                    SUM(CASE WHEN late BETWEEN 61 AND 90 THEN balance ELSE 0 END),
                    SUM(CASE WHEN late > 90 THEN balance ELSE 0 END),
                    SUM(balance), COUNT(*)
             FROM open_docs o
             LEFT JOIN contacts c ON c.realm_id = ?1 AND c.contact_type = ?4 AND c.qbo_id = o.contact_id
             GROUP BY o.contact_id
             ORDER BY SUM(balance) DESC, c.display_name",
        )?;
        let contact_type = match side {
            AgingSide::Receivable => "Customer",
            AgingSide::Payable => "Vendor",
        };
        let rows = statement.query_map(params![realm.as_str(), side.doc_type(), as_of, contact_type], |row| {
            Ok(AgingRow {
                contact_id: row.get(0)?,
                contact_name: row.get(1)?,
                current: Money::from_minor(row.get(2)?),
                days_1_30: Money::from_minor(row.get(3)?),
                days_31_60: Money::from_minor(row.get(4)?),
                days_61_90: Money::from_minor(row.get(5)?),
                days_over_90: Money::from_minor(row.get(6)?),
                total: Money::from_minor(row.get(7)?),
                documents: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Net sales per customer for documents dated `from..=to`, largest first.
    pub fn sales_by_customer(&self, realm: &RealmId, from: &str, to: &str) -> Result<Vec<SalesRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT d.contact_id, c.display_name,
                    SUM(CASE WHEN d.doc_type IN ('CreditMemo', 'RefundReceipt') THEN -l.amount_minor
                             ELSE l.amount_minor END),
                    COUNT(DISTINCT d.qbo_id)
             FROM documents d
             JOIN document_lines l ON l.realm_id = d.realm_id AND l.doc_qbo_id = d.qbo_id
             LEFT JOIN contacts c ON c.realm_id = d.realm_id AND c.contact_type = 'Customer' AND c.qbo_id = d.contact_id
             WHERE d.realm_id = ?1 AND d.is_deleted = 0 AND d.contact_id IS NOT NULL
               AND d.doc_type IN ('Invoice', 'SalesReceipt', 'CreditMemo', 'RefundReceipt')
               AND d.txn_date BETWEEN ?2 AND ?3
             GROUP BY d.contact_id
             ORDER BY 3 DESC, c.display_name",
        )?;
        let rows = statement.query_map(params![realm.as_str(), from, to], |row| {
            Ok(SalesRow {
                contact_id: row.get(0)?,
                contact_name: row.get(1)?,
                net_sales: Money::from_minor(row.get(2)?),
                documents: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every item, by name, with how many documents use it.
    pub fn items(&self, realm: &RealmId) -> Result<Vec<ItemRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT i.qbo_id, i.name, i.sku, i.item_type, i.unit_price, i.purchase_cost,
                    i.qty_on_hand, i.is_active,
                    (SELECT COUNT(DISTINCT l.doc_qbo_id) FROM document_lines l
                      WHERE l.realm_id = i.realm_id AND l.item_id = i.qbo_id)
             FROM items i
             WHERE i.realm_id = ?1 AND i.is_deleted = 0
             ORDER BY i.name COLLATE NOCASE",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok(ItemRow {
                qbo_id: row.get(0)?,
                name: row.get(1)?,
                sku: row.get(2)?,
                item_type: row.get(3)?,
                unit_price: row.get(4)?,
                purchase_cost: row.get(5)?,
                qty_on_hand: row.get(6)?,
                is_active: row.get::<_, i64>(7)? != 0,
                used_on: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::EntityType;
    use crate::store::MirroredEntity;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn entity(entity_type: EntityType, id: &str, raw: serde_json::Value) -> MirroredEntity {
        MirroredEntity {
            entity_type,
            qbo_id: id.into(),
            sync_token: "0".into(),
            last_updated_utc: Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap(),
            is_deleted: false,
            raw_json: raw,
        }
    }

    fn invoice(id: &str, due: &str, total: f64, balance: f64) -> MirroredEntity {
        entity(EntityType::Invoice, id, json!({
            "Id": id, "DocNumber": format!("T{id}"), "TxnDate": "2026-05-01", "DueDate": due,
            "CustomerRef": { "value": "c1" }, "TotalAmt": total, "Balance": balance,
            "Line": [ { "LineNum": 1, "Amount": total, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "i1" }, "Qty": 1, "UnitPrice": total } } ]
        }))
    }

    fn book() -> Store {
        let store = Store::open_in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        store.register_realm(&realm(), "Test", now).unwrap();
        let entities = vec![
            entity(EntityType::Customer, "c1", json!({ "Id": "c1", "DisplayName": "Blue Harbor Swim Club", "Active": true })),
            entity(EntityType::Item, "i1", json!({ "Id": "i1", "Name": "Rescue tube", "Sku": "RT-1", "Type": "Inventory", "UnitPrice": 49.25, "Active": true })),
            invoice("1", "2026-10-15", 100.00, 100.00), // not yet due on 10-01
            invoice("2", "2026-09-20", 200.00, 50.00),  // 11 days late, partly paid
            invoice("3", "2026-07-01", 300.00, 300.00), // 92 days late
            invoice("4", "2026-08-01", 400.00, 0.00),   // paid: not aged
            entity(EntityType::CreditMemo, "5", json!({
                "Id": "5", "TxnDate": "2026-06-01", "CustomerRef": { "value": "c1" }, "TotalAmt": 25.0,
                "Line": [ { "LineNum": 1, "Amount": 25.0, "DetailType": "SalesItemLineDetail",
                            "SalesItemLineDetail": { "ItemRef": { "value": "i1" } } } ]
            })),
        ];
        store.apply_batch(&realm(), &entities, None, now).unwrap();
        store
    }

    #[test]
    fn open_invoices_land_in_the_right_buckets() {
        let rows = book().aging(&realm(), AgingSide::Receivable, "2026-10-01").unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.contact_name.as_deref(), Some("Blue Harbor Swim Club"));
        assert_eq!(row.current, Money::from_minor(10_000));
        assert_eq!(row.days_1_30, Money::from_minor(5_000));
        assert_eq!(row.days_over_90, Money::from_minor(30_000));
        assert_eq!(row.total, Money::from_minor(45_000));
        assert_eq!(row.documents, 3, "the paid invoice is not aged");
    }

    #[test]
    fn nothing_dated_after_the_as_of_date_is_aged() {
        assert!(book().aging(&realm(), AgingSide::Receivable, "2026-04-30").unwrap().is_empty());
        assert!(book().aging(&realm(), AgingSide::Payable, "2026-10-01").unwrap().is_empty());
    }

    #[test]
    fn sales_net_out_credit_memos() {
        let rows = book().sales_by_customer(&realm(), "2026-01-01", "2026-12-31").unwrap();
        assert_eq!(rows[0].net_sales, Money::from_minor(100_000 - 2_500));
        assert_eq!(rows[0].documents, 5);
    }

    #[test]
    fn items_report_where_they_are_used() {
        let items = book().items(&realm()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sku.as_deref(), Some("RT-1"));
        assert_eq!(items[0].used_on, 5);
    }
}
