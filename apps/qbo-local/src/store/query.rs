//! The read-only query API. `DESIGN.md` §11, `ROADMAP.md` §B1.
//!
//! Everything the M1 UI's Tauri commands and the ledger import need is a read
//! over the projected tables — nothing here writes. It exists as its own
//! module for the same reason [`super::search`] and [`super::lineage`] do:
//! the connection stays private to `store`, so a caller two crates away can
//! only ever reach it through a function that takes `&RealmId` first.
//!
//! This module sits on top of the reads `store` and its siblings already
//! expose — [`Store::get_document`], [`Store::document_lines`],
//! [`Store::lineage`], [`Store::documents_for_contact`],
//! [`Store::documents_for_item`] — rather than re-querying what they already
//! answer.

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use ledger_core::Money;
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;

use super::lineage::Lineage;
use super::{document_row, DocumentRow, LineRow, Store, StoreError, DOCUMENT_SELECT};
use crate::domain::{ContactType, DocumentType, EntityType, RealmId};

/// A lineage walk is bounded by depth (`DESIGN.md` §3.4); this is generous for
/// every chain the prototype shows — estimate to invoice to payment, PO to
/// bill to bill payment, progress invoicing off one estimate — without
/// walking an unbounded graph on every document open.
const DETAIL_LINEAGE_DEPTH: usize = 8;

/// Paging for a list read. `Default` is 0/200; every caller's `limit` is
/// capped at [`MAX_PAGE_LIMIT`] regardless of what it asks for, so a UI bug or
/// a hostile Tauri argument cannot turn a register into a full scan.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
}

/// The hard cap on any [`Page::limit`], enforced by every paged query here —
/// not by trusting the caller to respect it.
pub const MAX_PAGE_LIMIT: usize = 1000;
const DEFAULT_PAGE_LIMIT: usize = 200;

impl Default for Page {
    fn default() -> Self {
        Page {
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
        }
    }
}

impl Page {
    fn capped_limit(self) -> i64 {
        self.limit.min(MAX_PAGE_LIMIT) as i64
    }

    fn sql_offset(self) -> i64 {
        self.offset as i64
    }
}

// ---------------------------------------------------------------------------
// Document detail
// ---------------------------------------------------------------------------

/// A document with its lines and its lineage — the single read a document
/// viewer opens with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentDetail {
    pub document: DocumentRow,
    pub lines: Vec<LineRow>,
    pub lineage: Lineage,
}

// ---------------------------------------------------------------------------
// Contact detail
// ---------------------------------------------------------------------------

/// A customer or vendor, projected — the `contacts` row on its own, without
/// its document history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContactRow {
    pub contact_type: ContactType,
    pub qbo_id: String,
    pub display_name: String,
    pub company_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub balance: Option<Money>,
    pub is_active: bool,
}

/// A contact page: who they are, what is still owed, and what happened
/// recently. `open_documents` is a subset of `recent_documents` in spirit —
/// balance-carrying rather than merely recent — and the two answer different
/// questions, so both are returned rather than making the caller re-derive
/// one from the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContactDetail {
    pub contact: ContactRow,
    /// Not deleted, balance > 0 — every document still carrying a balance.
    pub open_documents: Vec<DocumentRow>,
    /// Newest 50 regardless of balance — the activity feed.
    pub recent_documents: Vec<DocumentRow>,
}

const RECENT_DOCUMENTS_LIMIT: i64 = 50;

// ---------------------------------------------------------------------------
// Item detail
// ---------------------------------------------------------------------------

/// An `items` row, projected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRow {
    pub qbo_id: String,
    pub name: String,
    pub sku: Option<String>,
    pub description: Option<String>,
    pub item_type: Option<String>,
    /// Decimal as text, exactly as stored (§3.2) — prices carry more
    /// precision than `Money` keeps.
    pub unit_price: Option<String>,
    pub purchase_cost: Option<String>,
    pub qty_on_hand: Option<String>,
    pub income_account_id: Option<String>,
    pub expense_account_id: Option<String>,
    pub asset_account_id: Option<String>,
    pub is_active: bool,
}

/// A SKU page: price, cost and where it has actually been used. The
/// prototype's own words: "a SKU page that cannot answer 'where has this been
/// used' is not worth opening."
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDetail {
    pub item: ItemRow,
    pub where_used: Vec<DocumentRow>,
    /// Summed `document_lines.qty` off every Invoice and SalesReceipt line
    /// for this item. `qty` is decimal-as-text; a line whose text will not
    /// parse is skipped rather than failing the whole read — a corrupt qty on
    /// one line should not make every other line's history unreadable.
    pub units_sold: Decimal,
}

// ---------------------------------------------------------------------------
// Aging
// ---------------------------------------------------------------------------

/// The five buckets AR/AP aging reports in, `DESIGN.md` §9's v1 report list.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AgingBuckets {
    pub current: Money,
    pub d1_30: Money,
    pub d31_60: Money,
    pub d61_90: Money,
    pub over_90: Money,
}

impl AgingBuckets {
    /// Arithmetic is checked, per `DESIGN.md` §1 — but a bucket total that
    /// overflows `i64` minor units is not a business outcome this replica
    /// will ever see (Aquamentor and WaterLine both measured in the low
    /// millions, §0), so a violation here means the aging query itself is
    /// broken, not that a real total was too large. Panicking says so loudly
    /// rather than silently returning a wrong figure.
    fn add(&mut self, bucket: AgingBucket, amount: Money) {
        let slot = match bucket {
            AgingBucket::Current => &mut self.current,
            AgingBucket::Days1To30 => &mut self.d1_30,
            AgingBucket::Days31To60 => &mut self.d31_60,
            AgingBucket::Days61To90 => &mut self.d61_90,
            AgingBucket::Over90 => &mut self.over_90,
        };
        *slot = slot
            .checked_add(amount)
            .expect("aging bucket total overflowed i64 minor units");
    }

    fn total(&self) -> Money {
        Money::checked_sum([
            self.current,
            self.d1_30,
            self.d31_60,
            self.d61_90,
            self.over_90,
        ])
        .expect("aging total overflowed i64 minor units")
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AgingBucket {
    Current,
    Days1To30,
    Days31To60,
    Days61To90,
    Over90,
}

/// Days past due decide the bucket. Not yet due — including due today — is
/// `Current`; QBO's own aging reports draw the same line.
fn bucket_for(as_of: NaiveDate, due: NaiveDate) -> AgingBucket {
    let days_past_due = (as_of - due).num_days();
    match days_past_due {
        d if d <= 0 => AgingBucket::Current,
        1..=30 => AgingBucket::Days1To30,
        31..=60 => AgingBucket::Days31To60,
        61..=90 => AgingBucket::Days61To90,
        _ => AgingBucket::Over90,
    }
}

/// One contact's aging line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgingRow {
    pub contact_id: String,
    pub contact_name: String,
    pub buckets: AgingBuckets,
    pub total: Money,
}

/// AR or AP aging as of one date. `totals` is always the sum of `rows`'
/// buckets — computed the same way, not restated — so the two cannot drift
/// apart the way a stored total could.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgingReport {
    pub as_of: NaiveDate,
    pub rows: Vec<AgingRow>,
    pub totals: AgingBuckets,
}

// ---------------------------------------------------------------------------
// Sync status
// ---------------------------------------------------------------------------

/// One entity type's sync state — one row of the chrome's sync panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitySyncStatus {
    pub entity_type: EntityType,
    pub mirrored: i64,
    pub last_cdc_cursor: Option<DateTime<Utc>>,
    pub last_full_sweep: Option<DateTime<Utc>>,
    pub quarantined: i64,
}

/// Everything the app chrome shows about a realm's sync health in one read
/// (`DESIGN.md` §6.6: "last successful sync per realm... always visible").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    pub write_enabled: bool,
    pub quarantined_total: i64,
    pub entities: Vec<EntitySyncStatus>,
}

// ---------------------------------------------------------------------------
// Masters for the ledger import
// ---------------------------------------------------------------------------

/// A `classes` row, projected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassRow {
    pub qbo_id: String,
    pub name: String,
    pub fully_qualified_name: Option<String>,
    pub parent_id: Option<String>,
    pub is_active: bool,
}

/// An `accounts` row, projected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub qbo_id: String,
    pub name: String,
    pub acct_num: Option<String>,
    pub account_type: Option<String>,
    pub account_subtype: Option<String>,
    pub classification: Option<String>,
    pub balance: Option<Money>,
    pub is_active: bool,
}

impl Store {
    /// A document with its lines and its lineage in one read — the document
    /// viewer's whole query.
    pub fn document_detail(
        &self,
        realm: &RealmId,
        qbo_id: &str,
    ) -> Result<Option<DocumentDetail>, StoreError> {
        let Some(document) = self.get_document(realm, qbo_id)? else {
            return Ok(None);
        };
        let lines = self.document_lines(realm, qbo_id)?;
        let lineage = self.lineage(realm, qbo_id, DETAIL_LINEAGE_DEPTH)?;
        Ok(Some(DocumentDetail {
            document,
            lines,
            lineage,
        }))
    }

    /// A customer or vendor's page: who they are, what is open, and what
    /// happened recently.
    pub fn contact_detail(
        &self,
        realm: &RealmId,
        contact_type: ContactType,
        qbo_id: &str,
    ) -> Result<Option<ContactDetail>, StoreError> {
        let Some(contact) = self.contact_row(realm, contact_type, qbo_id)? else {
            return Ok(None);
        };
        let open_documents = self.open_documents_for_contact(realm, contact_type, qbo_id)?;
        let recent_documents =
            self.documents_for_contact(realm, contact_type, qbo_id, RECENT_DOCUMENTS_LIMIT)?;
        Ok(Some(ContactDetail {
            contact,
            open_documents,
            recent_documents,
        }))
    }

    fn contact_row(
        &self,
        realm: &RealmId,
        contact_type: ContactType,
        qbo_id: &str,
    ) -> Result<Option<ContactRow>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT contact_type, qbo_id, display_name, company_name, email, phone,
                        balance_minor, is_active
                 FROM contacts
                 WHERE realm_id = ?1 AND contact_type = ?2 AND qbo_id = ?3 AND is_deleted = 0",
                params![realm.as_str(), contact_type.as_str(), qbo_id],
                contact_row,
            )
            .optional()?)
    }

    fn open_documents_for_contact(
        &self,
        realm: &RealmId,
        contact_type: ContactType,
        contact_id: &str,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.contact_type = ?2 AND d.contact_id = ?3
               AND d.is_deleted = 0 AND d.balance_minor > 0
             ORDER BY d.txn_date DESC, d.qbo_id DESC"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), contact_type.as_str(), contact_id],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// An item's page: price, cost, and where it has actually been used.
    pub fn item_detail(
        &self,
        realm: &RealmId,
        qbo_id: &str,
    ) -> Result<Option<ItemDetail>, StoreError> {
        let Some(item) = self.item_row(realm, qbo_id)? else {
            return Ok(None);
        };
        let where_used = self.documents_for_item(realm, qbo_id, MAX_PAGE_LIMIT as i64)?;
        let units_sold = self.units_sold(realm, qbo_id)?;
        Ok(Some(ItemDetail {
            item,
            where_used,
            units_sold,
        }))
    }

    fn item_row(&self, realm: &RealmId, qbo_id: &str) -> Result<Option<ItemRow>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT qbo_id, name, sku, description, item_type, unit_price,
                        purchase_cost, qty_on_hand, income_account_id, expense_account_id,
                        asset_account_id, is_active
                 FROM items
                 WHERE realm_id = ?1 AND qbo_id = ?2 AND is_deleted = 0",
                params![realm.as_str(), qbo_id],
                item_row,
            )
            .optional()?)
    }

    /// Sum of `qty` off every Invoice and SalesReceipt line for this item.
    ///
    /// Reads the stored text and parses it here rather than trusting the
    /// column: `document_lines.qty` is a `TEXT` column with no format
    /// constraint (§3.2), so this is the one place that decides what "not a
    /// number" means, and it means "skip", never "panic" or "abort the read".
    fn units_sold(&self, realm: &RealmId, item_id: &str) -> Result<Decimal, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT l.qty
             FROM document_lines l
             JOIN documents d ON d.realm_id = l.realm_id AND d.qbo_id = l.doc_qbo_id
             WHERE l.realm_id = ?1 AND l.item_id = ?2 AND d.is_deleted = 0
               AND d.doc_type IN ('Invoice', 'SalesReceipt')",
        )?;
        let rows = statement.query_map(params![realm.as_str(), item_id], |row| {
            row.get::<_, Option<String>>(0)
        })?;

        let mut total = Decimal::ZERO;
        for row in rows {
            if let Some(qty) = row?.as_deref().and_then(parse_qty) {
                total += qty;
            }
        }
        Ok(total)
    }

    /// Invoices open (not deleted, balance > 0) as of `as_of`, bucketed by
    /// days past due and grouped by customer.
    pub fn ar_aging(&self, realm: &RealmId, as_of: NaiveDate) -> Result<AgingReport, StoreError> {
        self.aging_report(realm, DocumentType::Invoice, as_of)
    }

    /// The vendor-facing mirror of [`Store::ar_aging`], over open bills.
    pub fn ap_aging(&self, realm: &RealmId, as_of: NaiveDate) -> Result<AgingReport, StoreError> {
        self.aging_report(realm, DocumentType::Bill, as_of)
    }

    fn aging_report(
        &self,
        realm: &RealmId,
        doc_type: DocumentType,
        as_of: NaiveDate,
    ) -> Result<AgingReport, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.doc_type = ?2 AND d.is_deleted = 0
               AND d.balance_minor > 0"
        ))?;
        let rows = statement.query_map(params![realm.as_str(), doc_type.as_str()], document_row)?;

        let mut rows_by_contact: Vec<AgingRow> = Vec::new();
        let mut totals = AgingBuckets::default();

        for row in rows {
            let document = row?;
            let balance = document.balance.unwrap_or(Money::ZERO);

            // Due date first; a document with none falls back to its
            // transaction date (the task brief's fallback rule) rather than
            // being excluded — an undated bill is still owed.
            let due = document
                .due_date
                .as_deref()
                .and_then(parse_date)
                .or_else(|| parse_date(&document.txn_date));
            let bucket = due.map_or(AgingBucket::Current, |due| bucket_for(as_of, due));

            let contact_id = document.contact_id.clone().unwrap_or_default();
            let contact_name = document
                .contact_name
                .clone()
                .unwrap_or_else(|| contact_id.clone());

            let entry = match rows_by_contact
                .iter_mut()
                .find(|row| row.contact_id == contact_id)
            {
                Some(entry) => entry,
                None => {
                    rows_by_contact.push(AgingRow {
                        contact_id,
                        contact_name,
                        buckets: AgingBuckets::default(),
                        total: Money::ZERO,
                    });
                    rows_by_contact.last_mut().unwrap()
                }
            };
            entry.buckets.add(bucket, balance);
            entry.total = entry.buckets.total();
            totals.add(bucket, balance);
        }

        rows_by_contact.sort_by(|a, b| {
            b.total
                .cmp(&a.total)
                .then_with(|| a.contact_id.cmp(&b.contact_id))
        });

        Ok(AgingReport {
            as_of,
            rows: rows_by_contact,
            totals,
        })
    }

    /// Open documents of one type.
    ///
    /// **Open-status vocabulary**, matched against `project::parse_document`'s
    /// `doc_status` column (`TxnStatus` for sales documents, `POStatus` for
    /// purchase orders — whichever the payload carries):
    ///
    /// - `PurchaseOrder`: QBO's `POStatus` is `"Open"` or `"Closed"`. Open
    ///   means `"Open"`.
    /// - `Estimate`: QBO's `TxnStatus` is `"Pending"`, `"Accepted"`,
    ///   `"Closed"` or `"Rejected"`. Open means `"Pending"` or `"Accepted"` —
    ///   an accepted estimate is still open until fully invoiced, which is
    ///   exactly the progress-invoicing case the prototype's README
    ///   documents.
    /// - Every other document type carries no status vocabulary reliable
    ///   enough to call "open" on its own, so this falls back to
    ///   `balance > 0` for them — and the fallback also catches a PO or
    ///   Estimate whose status is stale but whose balance is not, since the
    ///   two conditions are `OR`ed rather than switched on type.
    pub fn open_documents(
        &self,
        realm: &RealmId,
        doc_type: DocumentType,
        page: Page,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.doc_type = ?2 AND d.is_deleted = 0
               AND (d.balance_minor > 0
                    OR (?2 = 'PurchaseOrder' AND d.doc_status = 'Open')
                    OR (?2 = 'Estimate' AND d.doc_status IN ('Pending', 'Accepted')))
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = statement.query_map(
            params![
                realm.as_str(),
                doc_type.as_str(),
                page.capped_limit(),
                page.sql_offset()
            ],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Documents carrying one class — the register behind a class filter.
    pub fn documents_by_class(
        &self,
        realm: &RealmId,
        class_id: &str,
        page: Page,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.class_id = ?2 AND d.is_deleted = 0
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = statement.query_map(
            params![
                realm.as_str(),
                class_id,
                page.capped_limit(),
                page.sql_offset()
            ],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Documents whose `txn_date` falls in `[from, to]`, optionally narrowed
    /// to one type — the register's date filter, and the ledger import's
    /// window walk.
    pub fn documents_in_range(
        &self,
        realm: &RealmId,
        doc_type: Option<DocumentType>,
        from: NaiveDate,
        to: NaiveDate,
        page: Page,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let from = from.format("%Y-%m-%d").to_string();
        let to = to.format("%Y-%m-%d").to_string();

        let type_predicate = if doc_type.is_some() {
            "AND d.doc_type = ?2"
        } else {
            ""
        };
        let sql = format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.is_deleted = 0 {type_predicate}
               AND d.txn_date >= ?3 AND d.txn_date <= ?4
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?5 OFFSET ?6"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                realm.as_str(),
                doc_type.map(DocumentType::as_str).unwrap_or_default(),
                from,
                to,
                page.capped_limit(),
                page.sql_offset()
            ],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Everything the chrome shows about a realm's sync health: per entity
    /// type, how much is mirrored, the cursor, and the quarantine count; plus
    /// the realm-level write flag and quarantine total.
    pub fn sync_status(&self, realm: &RealmId) -> Result<SyncStatus, StoreError> {
        let write_enabled = self.is_write_enabled(realm)?;

        let mut entities = Vec::with_capacity(EntityType::ALL.len());
        for &entity_type in EntityType::ALL {
            let mirrored = self.count_entities(realm, entity_type)?;
            let cursor = self.load_cursor(realm, entity_type)?;
            let quarantined = self.connection.query_row(
                "SELECT COUNT(*) FROM quarantine_entities WHERE realm_id = ?1 AND entity_type = ?2",
                params![realm.as_str(), entity_type.as_str()],
                |row| row.get(0),
            )?;
            entities.push(EntitySyncStatus {
                entity_type,
                mirrored,
                last_cdc_cursor: cursor.last_cdc_cursor,
                last_full_sweep: cursor.last_full_sweep,
                quarantined,
            });
        }

        let quarantined_total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM quarantine_entities WHERE realm_id = ?1",
            params![realm.as_str()],
            |row| row.get(0),
        )?;

        Ok(SyncStatus {
            write_enabled,
            quarantined_total,
            entities,
        })
    }

    /// Every class, for the class filter and the ledger import's taxonomy.
    pub fn class_tree(&self, realm: &RealmId) -> Result<Vec<ClassRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, name, fully_qualified_name, parent_id, is_active
             FROM classes
             WHERE realm_id = ?1 AND is_deleted = 0
             ORDER BY fully_qualified_name, name",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok(ClassRow {
                qbo_id: row.get(0)?,
                name: row.get(1)?,
                fully_qualified_name: row.get(2)?,
                parent_id: row.get(3)?,
                is_active: row.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The full chart of accounts, for the ledger import.
    pub fn chart_of_accounts(&self, realm: &RealmId) -> Result<Vec<AccountRow>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, name, acct_num, account_type, account_subtype, classification,
                    balance_minor, is_active
             FROM accounts
             WHERE realm_id = ?1 AND is_deleted = 0
             ORDER BY acct_num, name",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| {
            Ok(AccountRow {
                qbo_id: row.get(0)?,
                name: row.get(1)?,
                acct_num: row.get(2)?,
                account_type: row.get(3)?,
                account_subtype: row.get(4)?,
                classification: row.get(5)?,
                balance: row.get::<_, Option<i64>>(6)?.map(Money::from_minor),
                is_active: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn parse_date(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// `document_lines.qty` as stored — decimal text, never validated by a
/// schema constraint. Garbage in that column (a hand-edited replica, a
/// future writer with a bug) is skipped rather than propagated as an error:
/// one bad line should not make an item's whole sales history unreadable.
fn parse_qty(raw: &str) -> Option<Decimal> {
    Decimal::from_str(raw.trim()).ok()
}

fn contact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactRow> {
    let contact_type: String = row.get(0)?;
    Ok(ContactRow {
        contact_type: ContactType::parse(&contact_type).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(crate::domain::DomainError::UnknownEntityType(contact_type)),
            )
        })?,
        qbo_id: row.get(1)?,
        display_name: row.get(2)?,
        company_name: row.get(3)?,
        email: row.get(4)?,
        phone: row.get(5)?,
        balance: row.get::<_, Option<i64>>(6)?.map(Money::from_minor),
        is_active: row.get::<_, i64>(7)? != 0,
    })
}

fn item_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    Ok(ItemRow {
        qbo_id: row.get(0)?,
        name: row.get(1)?,
        sku: row.get(2)?,
        description: row.get(3)?,
        item_type: row.get(4)?,
        unit_price: row.get(5)?,
        purchase_cost: row.get(6)?,
        qty_on_hand: row.get(7)?,
        income_account_id: row.get(8)?,
        expense_account_id: row.get(9)?,
        asset_account_id: row.get(10)?,
        is_active: row.get::<_, i64>(11)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_default_is_zero_offset_two_hundred_limit() {
        assert_eq!(
            Page::default(),
            Page {
                offset: 0,
                limit: 200
            }
        );
    }

    #[test]
    fn page_limit_is_capped_regardless_of_what_is_asked_for() {
        let page = Page {
            offset: 0,
            limit: 50_000,
        };
        assert_eq!(page.capped_limit(), MAX_PAGE_LIMIT as i64);
    }

    #[test]
    fn a_due_date_on_the_boundary_is_still_current() {
        let as_of = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();
        assert_eq!(bucket_for(as_of, as_of), AgingBucket::Current);
    }

    #[test]
    fn bucket_boundaries_land_where_the_brief_says() {
        let as_of = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let days_before = |d: i64| as_of - chrono::Duration::days(d);

        assert_eq!(bucket_for(as_of, days_before(1)), AgingBucket::Days1To30);
        assert_eq!(bucket_for(as_of, days_before(30)), AgingBucket::Days1To30);
        assert_eq!(bucket_for(as_of, days_before(31)), AgingBucket::Days31To60);
        assert_eq!(bucket_for(as_of, days_before(60)), AgingBucket::Days31To60);
        assert_eq!(bucket_for(as_of, days_before(61)), AgingBucket::Days61To90);
        assert_eq!(bucket_for(as_of, days_before(90)), AgingBucket::Days61To90);
        assert_eq!(bucket_for(as_of, days_before(91)), AgingBucket::Over90);
    }

    #[test]
    fn a_non_numeric_qty_is_skipped_rather_than_panicking_or_erroring() {
        // document_lines.qty is unvalidated decimal-as-text (§3.2); this is
        // the function that decides what "not a number" means for it, and it
        // must never panic on hand-edited or corrupted data.
        for garbage in ["", "N/A", "twelve", "12.5.3", "  "] {
            assert_eq!(
                parse_qty(garbage),
                None,
                "should not have parsed {garbage:?}"
            );
        }
        assert_eq!(parse_qty("12.5"), Some(Decimal::from_str("12.5").unwrap()));
        assert_eq!(parse_qty(" 3 "), Some(Decimal::from(3)));
    }
}
