//! Master data, numbering and open items: what lets the ledger stand on its
//! own (D28) instead of borrowing every name, number and balance from QBO.
//!
//! - Contacts (customers and vendors) and items, each with the defaults a
//!   document needs: terms, tax exemption, default class, item accounts.
//! - Document numbers allocated from a per-type sequence that never reissues
//!   a number already saved.
//! - [`prepare_document`]: the standalone entry path. Checks every reference
//!   against the master data, fills the number, due date, header class and
//!   sales tax the caller left blank, and builds the [`PostingContext`]
//!   [`crate::post::post`] needs from the stored items rather than from the
//!   caller.
//! - Open documents and aging for receivables and payables, derived from the
//!   saved documents and their applications, never stored. The slim cutover
//!   import (D28) checks its AR and AP against QBO's aging with these.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension, Row};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::store::{Ledger, LedgerError};
use crate::types::{
    AccountId, ClassId, ContactKind, ContactRef, CustomerPosting, DocKind, ItemAccounts,
    LedgerDocument, LineKind, PostingConfig, PostingContext, TaxDetail,
};

#[derive(Debug, thiserror::Error)]
pub enum BooksError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Money(#[from] ledger_core::MoneyError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unknown {kind} {id}")]
    UnknownContact { kind: &'static str, id: String },
    #[error("{0} is inactive")]
    InactiveContact(String),
    #[error("a {doc} needs a {expected}, but {id} is a {actual}")]
    WrongContactKind {
        doc: &'static str,
        expected: &'static str,
        actual: &'static str,
        id: String,
    },
    #[error("a {0} needs a contact")]
    MissingContact(&'static str),
    #[error("unknown item {0}")]
    UnknownItem(String),
    #[error("item {0} is inactive")]
    InactiveItem(String),
    #[error("{kind} number {number} is already used by document {other}")]
    DuplicateNumber {
        kind: &'static str,
        number: String,
        other: String,
    },
    #[error(
        "an inventory item needs an asset account, and only an inventory item may have one: {0}"
    )]
    AssetAccountMismatch(String),
    #[error("stored value {0:?} does not parse")]
    BadStoredValue(String),
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Contact {
    pub contact_id: String,
    pub kind: ContactKind,
    pub display_name: String,
    #[serde(default)]
    pub company_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub bill_address: Option<String>,
    #[serde(default)]
    pub ship_address: Option<String>,
    /// Net days. `None` means due on receipt.
    #[serde(default)]
    pub terms_days: Option<i64>,
    #[serde(default)]
    pub is_tax_exempt: bool,
    #[serde(default)]
    pub default_class: Option<ClassId>,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// The QBO id when imported. Kept forever.
    #[serde(default)]
    pub source_ref: Option<String>,
}

fn yes() -> bool {
    true
}

const fn contact_kind_str(kind: ContactKind) -> &'static str {
    match kind {
        ContactKind::Customer => "customer",
        ContactKind::Vendor => "vendor",
    }
}

fn parse_contact_kind(raw: &str) -> Result<ContactKind, BooksError> {
    match raw {
        "customer" => Ok(ContactKind::Customer),
        "vendor" => Ok(ContactKind::Vendor),
        other => Err(BooksError::BadStoredValue(other.to_string())),
    }
}

const CONTACT_COLUMNS: &str = "contact_id, kind, display_name, company_name, email, phone,
     bill_address, ship_address, terms_days, is_tax_exempt, default_class, is_active,
     source_ref";

fn contact_from_row(row: &Row<'_>) -> rusqlite::Result<(Contact, String)> {
    let kind: String = row.get(1)?;
    Ok((
        Contact {
            contact_id: row.get(0)?,
            // Placeholder until the caller parses `kind`; rusqlite's closure
            // cannot return our error type.
            kind: ContactKind::Customer,
            display_name: row.get(2)?,
            company_name: row.get(3)?,
            email: row.get(4)?,
            phone: row.get(5)?,
            bill_address: row.get(6)?,
            ship_address: row.get(7)?,
            terms_days: row.get(8)?,
            is_tax_exempt: row.get::<_, i64>(9)? != 0,
            default_class: row.get::<_, Option<String>>(10)?.map(ClassId),
            is_active: row.get::<_, i64>(11)? != 0,
            source_ref: row.get(12)?,
        },
        kind,
    ))
}

fn finish_contact((mut contact, kind): (Contact, String)) -> Result<Contact, BooksError> {
    contact.kind = parse_contact_kind(&kind)?;
    Ok(contact)
}

/// Inserts or replaces a contact. The display name is unique per kind, as it
/// is in QBO, so a second customer named the same is refused by the index.
pub fn upsert_contact(
    ledger: &Ledger,
    company: &str,
    contact: &Contact,
    now: DateTime<Utc>,
) -> Result<(), BooksError> {
    ledger.conn().execute(
        "INSERT INTO contacts (company_id, contact_id, kind, display_name, company_name, email,
             phone, bill_address, ship_address, terms_days, is_tax_exempt, default_class,
             is_active, source_ref, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (company_id, contact_id) DO UPDATE SET
             kind = excluded.kind, display_name = excluded.display_name,
             company_name = excluded.company_name, email = excluded.email,
             phone = excluded.phone, bill_address = excluded.bill_address,
             ship_address = excluded.ship_address, terms_days = excluded.terms_days,
             is_tax_exempt = excluded.is_tax_exempt, default_class = excluded.default_class,
             is_active = excluded.is_active,
             source_ref = COALESCE(excluded.source_ref, contacts.source_ref),
             updated_at = excluded.updated_at",
        params![
            company,
            contact.contact_id,
            contact_kind_str(contact.kind),
            contact.display_name,
            contact.company_name,
            contact.email,
            contact.phone,
            contact.bill_address,
            contact.ship_address,
            contact.terms_days,
            contact.is_tax_exempt as i64,
            contact.default_class.as_ref().map(|c| c.0.clone()),
            contact.is_active as i64,
            contact.source_ref,
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn contact(
    ledger: &Ledger,
    company: &str,
    contact_id: &str,
) -> Result<Option<Contact>, BooksError> {
    ledger
        .conn()
        .query_row(
            &format!(
                "SELECT {CONTACT_COLUMNS} FROM contacts WHERE company_id = ?1 AND contact_id = ?2"
            ),
            params![company, contact_id],
            contact_from_row,
        )
        .optional()?
        .map(finish_contact)
        .transpose()
}

/// Contacts by name, optionally of one kind and matching `query` anywhere in
/// the display name, company name or email (case-insensitive). Inactive ones
/// only when asked for.
pub fn list_contacts(
    ledger: &Ledger,
    company: &str,
    kind: Option<ContactKind>,
    query: Option<&str>,
    include_inactive: bool,
) -> Result<Vec<Contact>, BooksError> {
    let pattern = query.map(|q| format!("%{}%", q.to_lowercase()));
    let mut stmt = ledger.conn().prepare(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contacts
         WHERE company_id = ?1
           AND (?2 IS NULL OR kind = ?2)
           AND (?3 IS NULL OR lower(display_name) LIKE ?3
                OR lower(COALESCE(company_name, '')) LIKE ?3
                OR lower(COALESCE(email, '')) LIKE ?3)
           AND (?4 OR is_active = 1)
         ORDER BY display_name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(
        params![
            company,
            kind.map(contact_kind_str),
            pattern,
            include_inactive
        ],
        contact_from_row,
    )?;
    rows.map(|row| finish_contact(row?)).collect()
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Inventory,
    NonInventory,
    Service,
}

impl ItemType {
    const fn as_str(self) -> &'static str {
        match self {
            ItemType::Inventory => "inventory",
            ItemType::NonInventory => "non_inventory",
            ItemType::Service => "service",
        }
    }

    fn parse(raw: &str) -> Result<ItemType, BooksError> {
        match raw {
            "inventory" => Ok(ItemType::Inventory),
            "non_inventory" => Ok(ItemType::NonInventory),
            "service" => Ok(ItemType::Service),
            other => Err(BooksError::BadStoredValue(other.to_string())),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Item {
    pub item_id: String,
    #[serde(default)]
    pub sku: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub item_type: ItemType,
    pub income_account: AccountId,
    pub expense_account: AccountId,
    /// Inventory items only, and every inventory item has one.
    #[serde(default)]
    pub asset_account: Option<AccountId>,
    #[serde(default)]
    pub default_class: Option<ClassId>,
    #[serde(default)]
    pub sales_price: Option<Money>,
    #[serde(default)]
    pub purchase_cost: Option<Money>,
    /// Moving-average unit cost (W12), for the COGS leg.
    #[serde(default)]
    pub unit_cost: Option<Money>,
    #[serde(default = "yes")]
    pub is_taxable: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub source_ref: Option<String>,
}

impl Item {
    pub fn accounts(&self) -> ItemAccounts {
        ItemAccounts {
            income: self.income_account.clone(),
            expense: self.expense_account.clone(),
            asset: self.asset_account.clone(),
            default_class: self.default_class.clone(),
            unit_cost: self.unit_cost,
        }
    }
}

const ITEM_COLUMNS: &str = "item_id, sku, name, description, item_type, income_account,
     expense_account, asset_account, default_class, sales_price_minor, purchase_cost_minor,
     unit_cost_minor, is_taxable, is_active, source_ref";

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<(Item, String)> {
    let item_type: String = row.get(4)?;
    let money = |idx: usize| -> rusqlite::Result<Option<Money>> {
        Ok(row.get::<_, Option<i64>>(idx)?.map(Money::from_minor))
    };
    Ok((
        Item {
            item_id: row.get(0)?,
            sku: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            item_type: ItemType::Service,
            income_account: AccountId(row.get(5)?),
            expense_account: AccountId(row.get(6)?),
            asset_account: row.get::<_, Option<String>>(7)?.map(AccountId),
            default_class: row.get::<_, Option<String>>(8)?.map(ClassId),
            sales_price: money(9)?,
            purchase_cost: money(10)?,
            unit_cost: money(11)?,
            is_taxable: row.get::<_, i64>(12)? != 0,
            is_active: row.get::<_, i64>(13)? != 0,
            source_ref: row.get(14)?,
        },
        item_type,
    ))
}

fn finish_item((mut item, item_type): (Item, String)) -> Result<Item, BooksError> {
    item.item_type = ItemType::parse(&item_type)?;
    Ok(item)
}

pub fn upsert_item(
    ledger: &Ledger,
    company: &str,
    item: &Item,
    now: DateTime<Utc>,
) -> Result<(), BooksError> {
    if (item.item_type == ItemType::Inventory) != item.asset_account.is_some() {
        return Err(BooksError::AssetAccountMismatch(item.item_id.clone()));
    }
    ledger.conn().execute(
        "INSERT INTO items (company_id, item_id, sku, name, description, item_type,
             income_account, expense_account, asset_account, default_class, sales_price_minor,
             purchase_cost_minor, unit_cost_minor, is_taxable, is_active, source_ref, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT (company_id, item_id) DO UPDATE SET
             sku = excluded.sku, name = excluded.name, description = excluded.description,
             item_type = excluded.item_type, income_account = excluded.income_account,
             expense_account = excluded.expense_account, asset_account = excluded.asset_account,
             default_class = excluded.default_class, sales_price_minor = excluded.sales_price_minor,
             purchase_cost_minor = excluded.purchase_cost_minor,
             unit_cost_minor = excluded.unit_cost_minor, is_taxable = excluded.is_taxable,
             is_active = excluded.is_active,
             source_ref = COALESCE(excluded.source_ref, items.source_ref),
             updated_at = excluded.updated_at",
        params![
            company,
            item.item_id,
            item.sku,
            item.name,
            item.description,
            item.item_type.as_str(),
            item.income_account.0,
            item.expense_account.0,
            item.asset_account.as_ref().map(|a| a.0.clone()),
            item.default_class.as_ref().map(|c| c.0.clone()),
            item.sales_price.map(Money::minor),
            item.purchase_cost.map(Money::minor),
            item.unit_cost.map(Money::minor),
            item.is_taxable as i64,
            item.is_active as i64,
            item.source_ref,
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn item(ledger: &Ledger, company: &str, item_id: &str) -> Result<Option<Item>, BooksError> {
    ledger
        .conn()
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM items WHERE company_id = ?1 AND item_id = ?2"),
            params![company, item_id],
            item_from_row,
        )
        .optional()?
        .map(finish_item)
        .transpose()
}

/// Items by name, matching `query` anywhere in the SKU, name or description
/// (case-insensitive). Inactive ones only when asked for.
pub fn list_items(
    ledger: &Ledger,
    company: &str,
    query: Option<&str>,
    include_inactive: bool,
) -> Result<Vec<Item>, BooksError> {
    let pattern = query.map(|q| format!("%{}%", q.to_lowercase()));
    let mut stmt = ledger.conn().prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM items
         WHERE company_id = ?1
           AND (?2 IS NULL OR lower(COALESCE(sku, '')) LIKE ?2 OR lower(name) LIKE ?2
                OR lower(COALESCE(description, '')) LIKE ?2)
           AND (?3 OR is_active = 1)
         ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![company, pattern, include_inactive], item_from_row)?;
    rows.map(|row| finish_item(row?)).collect()
}

// ---------------------------------------------------------------------------
// Numbering
// ---------------------------------------------------------------------------

/// The first number a type gets when nothing of that type has been saved.
pub const FIRST_NUMBER: i64 = 1001;

/// The document types the book numbers itself. Bills and vendor credits
/// carry the vendor's own number; payments and deposits carry none.
pub const fn is_numbered(kind: DocKind) -> bool {
    matches!(
        kind,
        DocKind::Estimate
            | DocKind::Invoice
            | DocKind::SalesReceipt
            | DocKind::CreditMemo
            | DocKind::RefundReceipt
            | DocKind::PurchaseOrder
    )
}

/// Allocates the next number for `kind`. The first call per type seeds the
/// sequence from the highest numeric number already saved (so imported
/// documents carry on where QBO left off), else [`FIRST_NUMBER`]. A number
/// already used by a saved document of the same type is skipped, never
/// reissued. An allocated number that is never saved is a gap, as in QBO.
pub fn next_number(ledger: &Ledger, company: &str, kind: DocKind) -> Result<String, BooksError> {
    let conn = ledger.conn();
    let tx = conn.unchecked_transaction()?;
    let stored: Option<i64> = tx
        .query_row(
            "SELECT next_number FROM doc_sequences WHERE company_id = ?1 AND doc_type = ?2",
            params![company, kind.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let mut candidate = match stored {
        Some(next) => next,
        None => {
            let highest: Option<i64> = tx.query_row(
                "SELECT MAX(CAST(doc_number AS INTEGER)) FROM document_versions
                 WHERE company_id = ?1 AND doc_type = ?2
                   AND doc_number <> '' AND doc_number NOT GLOB '*[^0-9]*'",
                params![company, kind.as_str()],
                |row| row.get(0),
            )?;
            highest.map_or(FIRST_NUMBER, |n| n + 1)
        }
    };
    while number_owner(&tx, company, kind, &candidate.to_string())?.is_some() {
        candidate += 1;
    }
    tx.execute(
        "INSERT INTO doc_sequences (company_id, doc_type, next_number) VALUES (?1, ?2, ?3)
         ON CONFLICT (company_id, doc_type) DO UPDATE SET next_number = excluded.next_number",
        params![company, kind.as_str(), candidate + 1],
    )?;
    tx.commit()?;
    Ok(candidate.to_string())
}

/// The document of type `kind` that holds `number` in its newest version.
fn number_owner(
    conn: &rusqlite::Connection,
    company: &str,
    kind: DocKind,
    number: &str,
) -> Result<Option<String>, BooksError> {
    Ok(conn
        .query_row(
            "SELECT dv.document_id FROM document_versions dv
             WHERE dv.company_id = ?1 AND dv.doc_type = ?2 AND dv.doc_number = ?3
               AND dv.version = (SELECT MAX(version) FROM document_versions
                                 WHERE company_id = dv.company_id
                                   AND document_id = dv.document_id)
             LIMIT 1",
            params![company, kind.as_str(), number],
            |row| row.get(0),
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// The standalone entry path
// ---------------------------------------------------------------------------

/// Which contact kind a document type names, if it names one at all.
const fn contact_kind_for(kind: DocKind) -> Option<ContactKind> {
    match kind {
        DocKind::Estimate
        | DocKind::Invoice
        | DocKind::SalesReceipt
        | DocKind::CreditMemo
        | DocKind::RefundReceipt
        | DocKind::Payment => Some(ContactKind::Customer),
        DocKind::PurchaseOrder | DocKind::Bill | DocKind::BillPayment | DocKind::VendorCredit => {
            Some(ContactKind::Vendor)
        }
        _ => None,
    }
}

/// Sales documents whose tax this path computes when the caller sends none.
const fn is_taxed_sale(kind: DocKind) -> bool {
    matches!(
        kind,
        DocKind::Estimate
            | DocKind::Invoice
            | DocKind::SalesReceipt
            | DocKind::CreditMemo
            | DocKind::RefundReceipt
    )
}

/// Gets a document ready to save and post from master data alone, so a
/// caller (a screen, a skill) sends only what a person would type:
///
/// - The contact must exist, be active and be the right kind for the
///   document; every item line's item must exist and be active.
/// - A numbered type with no number gets [`next_number`]; a number another
///   document of the same type already holds is refused.
/// - An invoice or bill with no due date gets `txn_date + terms_days`.
/// - No header class takes the contact's default class.
/// - A taxed sale with no tax gets one: the taxable base (taxable lines less
///   taxable discounts) times the configured rate, banker's rounding
///   (`RoundingPolicy::TaxCalculation`), zero for an exempt customer.
///
/// Returns the completed document and the posting context built from the
/// stored items and the contact's exemption.
pub fn prepare_document(
    ledger: &Ledger,
    company: &str,
    mut doc: LedgerDocument,
    config: PostingConfig,
) -> Result<(LedgerDocument, PostingContext), BooksError> {
    let mut customers = HashMap::new();
    let mut contact_row = None;
    if let Some(expected) = contact_kind_for(doc.kind) {
        let contact_ref = doc
            .contact
            .clone()
            .ok_or(BooksError::MissingContact(doc.kind.as_str()))?;
        let found = contact(ledger, company, &contact_ref.id)?.ok_or_else(|| {
            BooksError::UnknownContact {
                kind: contact_kind_str(expected),
                id: contact_ref.id.clone(),
            }
        })?;
        if found.kind != expected || contact_ref.kind != expected {
            return Err(BooksError::WrongContactKind {
                doc: doc.kind.as_str(),
                expected: contact_kind_str(expected),
                actual: contact_kind_str(found.kind),
                id: found.contact_id,
            });
        }
        if !found.is_active {
            return Err(BooksError::InactiveContact(found.display_name));
        }
        if expected == ContactKind::Customer {
            customers.insert(
                found.contact_id.clone(),
                CustomerPosting {
                    is_tax_exempt: found.is_tax_exempt,
                },
            );
        }
        contact_row = Some(found);
    }

    let mut items = HashMap::new();
    for line in &doc.lines {
        let Some(item_id) = &line.item_id else {
            continue;
        };
        if items.contains_key(item_id) {
            continue;
        }
        let found = item(ledger, company, item_id)?
            .ok_or_else(|| BooksError::UnknownItem(item_id.clone()))?;
        if !found.is_active {
            return Err(BooksError::InactiveItem(found.name));
        }
        items.insert(item_id.clone(), found);
    }

    if is_numbered(doc.kind) {
        match &doc.number {
            None => doc.number = Some(next_number(ledger, company, doc.kind)?),
            Some(number) => {
                if let Some(other) = number_owner(ledger.conn(), company, doc.kind, number)? {
                    if other != doc.document_id {
                        return Err(BooksError::DuplicateNumber {
                            kind: doc.kind.as_str(),
                            number: number.clone(),
                            other,
                        });
                    }
                }
            }
        }
    }

    if let Some(found) = &contact_row {
        if doc.due_date.is_none() && matches!(doc.kind, DocKind::Invoice | DocKind::Bill) {
            doc.due_date =
                Some(doc.txn_date + chrono::Days::new(found.terms_days.unwrap_or(0).max(0) as u64));
        }
        if doc.header_class.is_none() {
            doc.header_class = found.default_class.clone();
        }
    }

    if doc.tax.is_none() && is_taxed_sale(doc.kind) {
        let exempt = contact_row.as_ref().is_some_and(|c| c.is_tax_exempt);
        doc.tax = sales_tax(&doc, config.sales_tax_rate, exempt)?;
    }

    let context = PostingContext {
        items: items
            .into_iter()
            .map(|(id, item)| (id, item.accounts()))
            .collect(),
        customers,
        config,
    };
    Ok((doc, context))
}

/// The tax on a sale: `None` when no line is taxable.
fn sales_tax(
    doc: &LedgerDocument,
    rate: Decimal,
    exempt: bool,
) -> Result<Option<TaxDetail>, BooksError> {
    let mut base = Money::ZERO;
    let mut any = false;
    for line in doc.lines.iter().filter(|line| line.is_taxable) {
        match line.kind {
            LineKind::Item | LineKind::Shipping | LineKind::Account => {
                base = base.checked_add(line.amount)?;
                any = true;
            }
            LineKind::Discount => base = base.checked_sub(line.amount)?,
            LineKind::Description | LineKind::Journal => {}
        }
    }
    if !any {
        return Ok(None);
    }
    let total_tax = if exempt {
        Money::ZERO
    } else {
        round_money(
            Decimal::new(base.minor(), 2) * rate,
            RoundingPolicy::TaxCalculation,
        )?
    };
    Ok(Some(TaxDetail {
        total_tax,
        taxable_base: base,
        rate,
    }))
}

/// What a document is worth to its counterparty: lines less discounts, plus
/// tax. The amount an invoice puts on AR and a bill on AP.
pub fn document_total(doc: &LedgerDocument) -> Result<Money, ledger_core::MoneyError> {
    let mut total = Money::ZERO;
    for line in &doc.lines {
        match line.kind {
            LineKind::Item | LineKind::Shipping | LineKind::Account => {
                total = total.checked_add(line.amount)?;
            }
            LineKind::Discount => total = total.checked_sub(line.amount)?,
            LineKind::Description | LineKind::Journal => {}
        }
    }
    if let Some(tax) = &doc.tax {
        total = total.checked_add(tax.total_tax)?;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Open documents and aging
// ---------------------------------------------------------------------------

/// Which side of the book: what customers owe, or what is owed to vendors.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Book {
    Receivable,
    Payable,
}

impl Book {
    /// Documents that put an amount on the book, and whether it counts
    /// against it (a credit memo or vendor credit is a negative open item).
    const fn opens(self, kind: DocKind) -> Option<bool> {
        match (self, kind) {
            (Book::Receivable, DocKind::Invoice) | (Book::Payable, DocKind::Bill) => Some(false),
            (Book::Receivable, DocKind::CreditMemo) | (Book::Payable, DocKind::VendorCredit) => {
                Some(true)
            }
            _ => None,
        }
    }

    /// Documents whose applications settle open items on this book.
    const fn settles(self, kind: DocKind) -> bool {
        match self {
            Book::Receivable => matches!(kind, DocKind::Payment | DocKind::CreditMemo),
            Book::Payable => matches!(kind, DocKind::BillPayment | DocKind::VendorCredit),
        }
    }

    fn doc_types(self) -> &'static [DocKind] {
        match self {
            Book::Receivable => &[DocKind::Invoice, DocKind::CreditMemo, DocKind::Payment],
            Book::Payable => &[DocKind::Bill, DocKind::VendorCredit, DocKind::BillPayment],
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct OpenDocument {
    pub document_id: String,
    pub kind: DocKind,
    pub number: Option<String>,
    pub contact_id: String,
    pub contact_name: String,
    pub txn_date: NaiveDate,
    pub due_date: NaiveDate,
    /// Signed: negative for a credit memo or vendor credit.
    pub total: Money,
    pub applied: Money,
    /// Signed like `total`. Never zero in a returned row.
    pub open: Money,
}

/// The newest version of every non-voided document of the given types dated
/// on or before `as_of`.
fn latest_documents(
    ledger: &Ledger,
    company: &str,
    kinds: &[DocKind],
    as_of: NaiveDate,
) -> Result<Vec<LedgerDocument>, BooksError> {
    let types: Vec<String> = kinds.iter().map(|k| format!("'{}'", k.as_str())).collect();
    let mut stmt = ledger.conn().prepare(&format!(
        "SELECT dv.payload_json FROM document_versions dv
         WHERE dv.company_id = ?1 AND dv.doc_type IN ({}) AND dv.txn_date <= ?2
           AND dv.version = (SELECT MAX(version) FROM document_versions
                             WHERE company_id = dv.company_id AND document_id = dv.document_id)
         ORDER BY dv.txn_date, dv.document_id",
        types.join(",")
    ))?;
    let rows = stmt.query_map(params![company, as_of.to_string()], |row| {
        row.get::<_, String>(0)
    })?;
    let mut docs = Vec::new();
    for payload in rows {
        let doc: LedgerDocument = serde_json::from_str(&payload?)?;
        if !doc.is_voided {
            docs.push(doc);
        }
    }
    Ok(docs)
}

fn contact_names(ledger: &Ledger, company: &str) -> Result<HashMap<String, String>, BooksError> {
    let mut stmt = ledger
        .conn()
        .prepare("SELECT contact_id, display_name FROM contacts WHERE company_id = ?1")?;
    let rows = stmt.query_map(params![company], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every document with something still open on `book` as of `as_of`, oldest
/// first. Open = total less every application to it from a settling document
/// dated on or before `as_of`. Due date is the document's own, else its date.
pub fn open_documents(
    ledger: &Ledger,
    company: &str,
    book: Book,
    as_of: NaiveDate,
) -> Result<Vec<OpenDocument>, BooksError> {
    let docs = latest_documents(ledger, company, book.doc_types(), as_of)?;
    let mut applied: HashMap<&str, Money> = HashMap::new();
    for doc in docs.iter().filter(|d| book.settles(d.kind)) {
        for application in &doc.applications {
            let entry = applied
                .entry(application.target_document_id.as_str())
                .or_insert(Money::ZERO);
            *entry = entry.checked_add(application.amount)?;
        }
        // A credit memo's own applications use it up as well.
        if book.opens(doc.kind) == Some(true) {
            let used = Money::checked_sum(doc.applications.iter().map(|a| a.amount))?;
            let entry = applied
                .entry(doc.document_id.as_str())
                .or_insert(Money::ZERO);
            *entry = entry.checked_add(used)?;
        }
    }

    let names = contact_names(ledger, company)?;
    let mut open = Vec::new();
    for doc in &docs {
        let Some(is_credit) = book.opens(doc.kind) else {
            continue;
        };
        let gross = document_total(doc)?;
        let used = applied
            .get(doc.document_id.as_str())
            .copied()
            .unwrap_or(Money::ZERO);
        let remaining = gross.checked_sub(used)?;
        if remaining.is_zero() {
            continue;
        }
        let sign = |m: Money| if is_credit { m.checked_neg() } else { Ok(m) };
        let contact_id = doc
            .contact
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        open.push(OpenDocument {
            document_id: doc.document_id.clone(),
            kind: doc.kind,
            number: doc.number.clone(),
            contact_name: names
                .get(&contact_id)
                .cloned()
                .unwrap_or_else(|| contact_id.clone()),
            contact_id,
            txn_date: doc.txn_date,
            due_date: doc.due_date.unwrap_or(doc.txn_date),
            total: sign(gross)?,
            applied: used,
            open: sign(remaining)?,
        });
    }
    Ok(open)
}

/// One contact's open balance by days past due, QBO's buckets.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize)]
pub struct AgingRow {
    pub contact_id: String,
    pub contact_name: String,
    pub current: Money,
    pub days_1_30: Money,
    pub days_31_60: Money,
    pub days_61_90: Money,
    pub over_90: Money,
    pub total: Money,
}

impl AgingRow {
    fn add(&mut self, days_past_due: i64, amount: Money) -> Result<(), ledger_core::MoneyError> {
        let bucket = match days_past_due {
            i64::MIN..=0 => &mut self.current,
            1..=30 => &mut self.days_1_30,
            31..=60 => &mut self.days_31_60,
            61..=90 => &mut self.days_61_90,
            _ => &mut self.over_90,
        };
        *bucket = bucket.checked_add(amount)?;
        self.total = self.total.checked_add(amount)?;
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct Aging {
    pub book: Book,
    pub as_of: NaiveDate,
    /// By contact name.
    pub rows: Vec<AgingRow>,
    pub totals: AgingRow,
}

/// AR or AP aging as of a date, one row per contact with anything open.
pub fn aging(
    ledger: &Ledger,
    company: &str,
    book: Book,
    as_of: NaiveDate,
) -> Result<Aging, BooksError> {
    let mut by_contact: HashMap<String, AgingRow> = HashMap::new();
    let mut totals = AgingRow {
        contact_name: "Total".to_string(),
        ..AgingRow::default()
    };
    for doc in open_documents(ledger, company, book, as_of)? {
        let days = (as_of - doc.due_date).num_days();
        by_contact
            .entry(doc.contact_id.clone())
            .or_insert_with(|| AgingRow {
                contact_id: doc.contact_id.clone(),
                contact_name: doc.contact_name.clone(),
                ..AgingRow::default()
            })
            .add(days, doc.open)?;
        totals.add(days, doc.open)?;
    }
    let mut rows: Vec<AgingRow> = by_contact
        .into_values()
        .filter(|row| !row.total.is_zero())
        .collect();
    rows.sort_by(|a, b| {
        a.contact_name
            .to_lowercase()
            .cmp(&b.contact_name.to_lowercase())
            .then_with(|| a.contact_id.cmp(&b.contact_id))
    });
    Ok(Aging {
        book,
        as_of,
        rows,
        totals,
    })
}

/// The contact a document names, as the document records it.
pub fn contact_ref(kind: ContactKind, id: impl Into<String>) -> ContactRef {
    ContactRef {
        kind,
        id: id.into(),
    }
}
