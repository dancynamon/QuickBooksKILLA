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
//! - Corrections (`LEDGER-DESIGN.md` §4/§6): [`update_document`],
//!   [`void_document`] and [`apply_credit`] each save a new document version
//!   and, when what the document posts has changed, reverse the old entry and
//!   post the new one — all in one [`Ledger::atomically`] unit, so a refusal
//!   part way (a closed period, a missing class) leaves the book untouched.
//!   Nothing is ever deleted: every version stays in `document_versions`.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension, Row};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use ledger_core::{round_money, Money, RoundingPolicy};

use crate::post::{self, Posting};
use crate::store::{CommandMeta, EntryId, Ledger, LedgerError};
use crate::types::{
    AccountId, Application, ClassId, ContactKind, ContactRef, CustomerPosting, DocKind,
    ItemAccounts, JournalEntry, LedgerDocument, LineKind, PostingConfig, PostingContext, TaxDetail,
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
    #[error(transparent)]
    Post(#[from] crate::post::PostError),
    #[error("no saved document {0}")]
    UnknownDocument(String),
    #[error("{0} is voided; a voided document cannot be changed")]
    Voided(String),
    #[error("{id} is a {from}; an edit cannot make it a {to}")]
    KindChanged {
        id: String,
        from: &'static str,
        to: &'static str,
    },
    #[error("{id} names a {from}; an edit cannot make it name a {to}")]
    ContactTypeChanged {
        id: String,
        from: &'static str,
        to: &'static str,
    },
    #[error("{kind} {id} keeps its number {kept}; an edit cannot renumber it to {requested}")]
    NumberChanged {
        kind: &'static str,
        id: String,
        kept: String,
        requested: String,
    },
    #[error("an edit cannot void a document; use void_document")]
    VoidThroughEdit,
    #[error("{applied} is applied against {id}; its total cannot fall to {total}")]
    Overapplied {
        id: String,
        applied: String,
        total: String,
    },
    #[error("{id} has {applied} applied against it; void or edit those payments or credits first")]
    HasApplications { id: String, applied: String },
    #[error("cannot apply the credit: {0}")]
    BadApplication(String),
    #[error("bad terms: {0}")]
    BadTerms(String),
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
    /// Net days. `None` means due on receipt, unless `due_day` is set.
    #[serde(default)]
    pub terms_days: Option<i64>,
    /// Date-driven terms (D41): due on a day of the month. When set it
    /// decides the due date and `terms_days` must be `None`.
    #[serde(default)]
    pub due_day: Option<DueDay>,
    #[serde(default)]
    pub is_tax_exempt: bool,
    #[serde(default)]
    pub default_class: Option<ClassId>,
    #[serde(default = "yes")]
    pub is_active: bool,
    /// The QBO id when imported. Kept forever.
    #[serde(default)]
    pub source_ref: Option<String>,
    /// The QBO term id the terms came from when imported. Kept like
    /// `source_ref`, so converting terms later can still say where each
    /// contact's started.
    #[serde(default)]
    pub terms_source_ref: Option<String>,
}

fn yes() -> bool {
    true
}

/// QBO's date-driven term (Term `Type` `DATE_DRIVEN`, D41): due on `day` of
/// the month, the first such day on or after the document date, moved a
/// month later when that day is `next_month_within` days or fewer away. A
/// day past the month's end is its last day (31 in February is the 28th or
/// 29th).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DueDay {
    /// 1 to 31 (QBO `DayOfMonthDue`).
    pub day: u32,
    /// 0 to 31 (QBO `DueNextMonthDays`).
    #[serde(default)]
    pub next_month_within: u32,
}

/// A contact's terms, whichever way they are stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Terms {
    /// Due `days` after the document date; 0 is due on receipt.
    Net { days: i64 },
    DayOfMonth {
        day: u32,
        #[serde(default)]
        next_month_within: u32,
    },
}

impl Terms {
    /// The due date of a document dated `txn_date`.
    pub fn due_date(self, txn_date: NaiveDate) -> NaiveDate {
        match self {
            Terms::Net { days } => txn_date + chrono::Days::new(days.max(0) as u64),
            Terms::DayOfMonth {
                day,
                next_month_within,
            } => {
                let mut due = day_in_month(txn_date.year(), txn_date.month(), day);
                if due < txn_date {
                    due = day_in_next_month(due, day);
                }
                if (due - txn_date).num_days() <= i64::from(next_month_within) {
                    due = day_in_next_month(due, day);
                }
                due
            }
        }
    }

    /// How a person reads them: "Net 30", "Due on receipt", "Due the 1st",
    /// "Due the last of the month".
    pub fn label(self) -> String {
        match self {
            Terms::Net { days } if days <= 0 => "Due on receipt".to_string(),
            Terms::Net { days } => format!("Net {days}"),
            Terms::DayOfMonth { day, .. } if day >= 31 => "Due the last of the month".to_string(),
            Terms::DayOfMonth { day, .. } => format!("Due the {}", ordinal(day)),
        }
    }

    pub fn check(self) -> Result<(), BooksError> {
        match self {
            Terms::Net { days } if days < 0 => {
                Err(BooksError::BadTerms(format!("net days {days} is negative")))
            }
            Terms::DayOfMonth { day, .. } if !(1..=31).contains(&day) => Err(BooksError::BadTerms(
                format!("day of month {day} is not 1 to 31"),
            )),
            Terms::DayOfMonth {
                next_month_within, ..
            } if next_month_within > 31 => Err(BooksError::BadTerms(format!(
                "next month within {next_month_within} days is more than 31"
            ))),
            _ => Ok(()),
        }
    }
}

/// `day` of the month, or the month's last day when it has fewer.
fn day_in_month(year: i32, month: u32, day: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    let last = (first + chrono::Months::new(1))
        .pred_opt()
        .expect("in range");
    NaiveDate::from_ymd_opt(year, month, day.clamp(1, last.day())).expect("clamped day")
}

fn day_in_next_month(date: NaiveDate, day: u32) -> NaiveDate {
    let next = date.with_day(1).expect("first") + chrono::Months::new(1);
    day_in_month(next.year(), next.month(), day)
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

impl Contact {
    /// The terms that fill a due date: the day of the month when set, else
    /// net days (none is due on receipt, net 0).
    pub fn terms(&self) -> Terms {
        match self.due_day {
            Some(DueDay {
                day,
                next_month_within,
            }) => Terms::DayOfMonth {
                day,
                next_month_within,
            },
            None => Terms::Net {
                days: self.terms_days.unwrap_or(0),
            },
        }
    }

    /// Stores `terms`: net 0 as due on receipt (`terms_days` `None`).
    pub fn set_terms(&mut self, terms: Terms) {
        match terms {
            Terms::Net { days } => {
                self.terms_days = (days != 0).then_some(days);
                self.due_day = None;
            }
            Terms::DayOfMonth {
                day,
                next_month_within,
            } => {
                self.terms_days = None;
                self.due_day = Some(DueDay {
                    day,
                    next_month_within,
                });
            }
        }
    }
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
     source_ref, due_day, due_next_month_within, terms_source_ref";

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
            due_day: row
                .get::<_, Option<u32>>(13)?
                .map(|day| -> rusqlite::Result<DueDay> {
                    Ok(DueDay {
                        day,
                        next_month_within: row.get::<_, Option<u32>>(14)?.unwrap_or(0),
                    })
                })
                .transpose()?,
            terms_source_ref: row.get(15)?,
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
    if contact.terms_days.is_some() && contact.due_day.is_some() {
        return Err(BooksError::BadTerms(format!(
            "{} has both net days and a day of the month",
            contact.display_name
        )));
    }
    contact.terms().check()?;
    ledger.conn().execute(
        "INSERT INTO contacts (company_id, contact_id, kind, display_name, company_name, email,
             phone, bill_address, ship_address, terms_days, is_tax_exempt, default_class,
             is_active, source_ref, updated_at, due_day, due_next_month_within,
             terms_source_ref)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18)
         ON CONFLICT (company_id, contact_id) DO UPDATE SET
             kind = excluded.kind, display_name = excluded.display_name,
             company_name = excluded.company_name, email = excluded.email,
             phone = excluded.phone, bill_address = excluded.bill_address,
             ship_address = excluded.ship_address, terms_days = excluded.terms_days,
             is_tax_exempt = excluded.is_tax_exempt, default_class = excluded.default_class,
             is_active = excluded.is_active,
             source_ref = COALESCE(excluded.source_ref, contacts.source_ref),
             updated_at = excluded.updated_at, due_day = excluded.due_day,
             due_next_month_within = excluded.due_next_month_within,
             terms_source_ref = COALESCE(excluded.terms_source_ref, contacts.terms_source_ref)",
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
            contact.due_day.map(|d| d.day),
            contact.due_day.map(|d| d.next_month_within),
            contact.terms_source_ref,
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
            | DocKind::SalesOrder
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
    let tx = ledger.write_tx()?;
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

/// Makes sure the next number [`next_number`] hands out for `kind` is at
/// least `at_least`, and never lowers it. The slim cutover import (D28) uses
/// this to carry numbering on past QBO's highest number, which usually sits
/// on a paid document the import never brings across, so seeding from the
/// saved documents alone would reissue it.
pub fn ensure_next_number(
    ledger: &Ledger,
    company: &str,
    kind: DocKind,
    at_least: i64,
) -> Result<i64, BooksError> {
    let conn = ledger.conn();
    let tx = conn.unchecked_transaction()?;
    let stored: Option<i64> = tx
        .query_row(
            "SELECT next_number FROM doc_sequences WHERE company_id = ?1 AND doc_type = ?2",
            params![company, kind.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let saved_next: i64 = tx
        .query_row(
            "SELECT MAX(CAST(doc_number AS INTEGER)) FROM document_versions
             WHERE company_id = ?1 AND doc_type = ?2
               AND doc_number <> '' AND doc_number NOT GLOB '*[^0-9]*'",
            params![company, kind.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .map_or(FIRST_NUMBER, |n| n + 1);
    let next = stored.unwrap_or(saved_next).max(saved_next).max(at_least);
    tx.execute(
        "INSERT INTO doc_sequences (company_id, doc_type, next_number) VALUES (?1, ?2, ?3)
         ON CONFLICT (company_id, doc_type) DO UPDATE SET next_number = excluded.next_number",
        params![company, kind.as_str(), next],
    )?;
    tx.commit()?;
    Ok(next)
}

/// The oplog kind the slim cutover import (`crate::slim`) saves an open
/// document under. Such a document's AR or AP sits in the opening entry, not
/// in an entry of its own, so it must never be posted: see
/// [`carried_by_opening`].
pub const SLIM_OPEN_ITEM_KIND: &str = "import_slim_open_item";

/// Whether `document_id` was brought across by the slim cutover import as an
/// open item whose balance the opening entry carries (D28). Posting such a
/// document, or a new version of it, would put its AR or AP on the books a
/// second time; the writers that post refuse it and say why.
pub fn carried_by_opening(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
) -> Result<bool, BooksError> {
    Ok(ledger
        .conn()
        .query_row(
            "SELECT 1 FROM document_versions dv JOIN oplog o
               ON o.company_id = dv.company_id AND o.command_id = dv.command_id
             WHERE dv.company_id = ?1 AND dv.document_id = ?2 AND o.kind = ?3
             LIMIT 1",
            params![company, document_id, SLIM_OPEN_ITEM_KIND],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
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
        | DocKind::SalesOrder
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
            | DocKind::SalesOrder
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
/// - An invoice or bill with no due date gets one from the contact's
///   [`Terms`]: `txn_date + terms_days`, or the day of the month (D41).
/// - No header class takes the contact's default class.
/// - A taxed sale with no tax gets one: the taxable base (taxable lines less
///   taxable discounts) times the configured rate, rounded half up as QBO does (W17)
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
            doc.due_date = Some(found.terms().due_date(doc.txn_date));
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
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
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
pub(crate) fn latest_documents(
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

pub(crate) fn contact_names(
    ledger: &Ledger,
    company: &str,
) -> Result<HashMap<String, String>, BooksError> {
    let mut stmt = ledger
        .conn()
        .prepare("SELECT contact_id, display_name FROM contacts WHERE company_id = ?1")?;
    let rows = stmt.query_map(params![company], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every document with something still open on `book` as of `as_of`, oldest
/// first. See [`open_items`] for what "open" means.
/// Later than any document date, and still a four-digit year, so it compares
/// correctly against `txn_date` as text.
const END_OF_TIME: NaiveDate = match NaiveDate::from_ymd_opt(9999, 12, 31) {
    Some(date) => date,
    None => panic!("9999-12-31 is a date"),
};

pub fn open_documents(
    ledger: &Ledger,
    company: &str,
    book: Book,
    as_of: NaiveDate,
) -> Result<Vec<OpenDocument>, BooksError> {
    // Every document, not only those dated by `as_of`: `open_items` keeps to
    // the window itself, and needs the later ones' dates to tell a payment
    // applied to a bill that does not exist yet on `as_of`.
    let docs = latest_documents(ledger, company, book.doc_types(), END_OF_TIME)?;
    let names = contact_names(ledger, company)?;
    Ok(open_items(&docs, book, as_of, &names)?)
}

/// The open items on `book` among `docs` as of `as_of`, oldest first. Pure,
/// so the slim cutover import (`crate::slim`) can work out what was open in
/// QBO on a date from translated replica documents with exactly the rule the
/// ledger applies to its own.
///
/// - An invoice or bill is open for its total less every application to it
///   from a settling document dated on or before `as_of`.
/// - A credit memo or vendor credit is a negative open item, used up by its
///   own applications and by applications to it inside a payment. Inside a
///   payment those carry the sign of what the payment takes out of AR or AP
///   (`crate::import::translate`: negative for a credit), so a payment's
///   application to a credit adds to what is used rather than to what is
///   left.
/// - A bill payment's unapplied remainder is a negative open item on
///   payables: `crate::post` debits it to 2000 like an application, so AP
///   aging has to carry it to tie. A customer payment's unapplied remainder
///   parks in 2300 (W8), not 1200, and is never an AR open item.
///
/// Due date is the document's own, else its date. `names` maps contact ids
/// to display names; an id with no name shows as itself.
pub fn open_items(
    docs: &[LedgerDocument],
    book: Book,
    as_of: NaiveDate,
    names: &HashMap<String, String>,
) -> Result<Vec<OpenDocument>, ledger_core::MoneyError> {
    let in_window = |doc: &&LedgerDocument| doc.txn_date <= as_of && !doc.is_voided;
    let dates: HashMap<&str, NaiveDate> = docs
        .iter()
        .map(|doc| (doc.document_id.as_str(), doc.txn_date))
        .collect();
    // Applications from settling documents, by target, signed as recorded.
    let mut paid: HashMap<&str, Money> = HashMap::new();
    // A credit's own applications, by the credit itself.
    let mut consumed: HashMap<&str, Money> = HashMap::new();
    // A link to an estimate, sales order or purchase order says where a
    // line came from; it moves no money (D42).
    let moves_money = |a: &&crate::types::Application| !a.target_kind.is_contract();
    for doc in docs
        .iter()
        .filter(in_window)
        .filter(|d| book.settles(d.kind))
    {
        for application in doc.applications.iter().filter(moves_money) {
            let entry = paid
                .entry(application.target_document_id.as_str())
                .or_insert(Money::ZERO);
            *entry = entry.checked_add(application.amount)?;
        }
        if book.opens(doc.kind) == Some(true) {
            let used = Money::checked_sum(
                doc.applications
                    .iter()
                    .filter(moves_money)
                    .map(|a| a.amount),
            )?;
            let entry = consumed
                .entry(doc.document_id.as_str())
                .or_insert(Money::ZERO);
            *entry = entry.checked_add(used)?;
        }
    }

    let mut open = Vec::new();
    for doc in docs.iter().filter(in_window) {
        let contact_id = doc
            .contact
            .as_ref()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        let contact_name = names
            .get(&contact_id)
            .cloned()
            .unwrap_or_else(|| contact_id.clone());

        if book == Book::Payable && doc.kind == DocKind::BillPayment {
            // A payment applied to a bill dated after `as_of` was not applied
            // to anything yet on that date: QBO ages it as a vendor credit
            // until the bill exists (practice run, 17 Oct 2026: a bill
            // payment dated 7/16 against a bill dated 9/2).
            let mut not_yet_applied = doc.unapplied;
            for application in doc.applications.iter().filter(moves_money) {
                if dates
                    .get(application.target_document_id.as_str())
                    .is_some_and(|date| *date > as_of)
                {
                    not_yet_applied = not_yet_applied.checked_add(application.amount)?;
                }
            }
            if !not_yet_applied.is_zero() {
                let prepaid = not_yet_applied.checked_neg()?;
                open.push(OpenDocument {
                    document_id: doc.document_id.clone(),
                    kind: doc.kind,
                    number: doc.number.clone(),
                    contact_id,
                    contact_name,
                    txn_date: doc.txn_date,
                    due_date: doc.txn_date,
                    total: prepaid,
                    applied: Money::ZERO,
                    open: prepaid,
                });
            }
            continue;
        }

        let Some(is_credit) = book.opens(doc.kind) else {
            continue;
        };
        let gross = document_total(doc)?;
        let from_payments = paid
            .get(doc.document_id.as_str())
            .copied()
            .unwrap_or(Money::ZERO);
        let used = if is_credit {
            let own = consumed
                .get(doc.document_id.as_str())
                .copied()
                .unwrap_or(Money::ZERO);
            // A payment records its use of a credit as a negative amount.
            own.checked_sub(from_payments)?
        } else {
            from_payments
        };
        let remaining = gross.checked_sub(used)?;
        if remaining.is_zero() {
            continue;
        }
        let sign = |m: Money| if is_credit { m.checked_neg() } else { Ok(m) };
        open.push(OpenDocument {
            document_id: doc.document_id.clone(),
            kind: doc.kind,
            number: doc.number.clone(),
            contact_id,
            contact_name,
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
    /// Adds `amount` to the bucket `days_past_due` falls in, and to the total.
    pub fn add(
        &mut self,
        days_past_due: i64,
        amount: Money,
    ) -> Result<(), ledger_core::MoneyError> {
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

// ---------------------------------------------------------------------------
// Corrections: edit, void, apply a credit (LEDGER-DESIGN.md §4/§6)
// ---------------------------------------------------------------------------

/// Later than any document's date, for "everything saved so far". Not
/// `NaiveDate::MAX`, whose string form (`+262142-12-31`) sorts before
/// `2027-01-01` in the `txn_date <= ?` text comparison.
pub(crate) fn far_future() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date")
}

/// What an edit, a void or a credit application did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Revision {
    /// The document as now saved, newest version.
    pub document: LedgerDocument,
    pub version: i64,
    /// The reversals posted against the entries the old version had live.
    /// Empty when the posting did not change (a memo edit, a credit applied)
    /// or the document never posts.
    pub reversals: Vec<EntryId>,
    /// The entry the new version posted, when it posted one.
    pub entry: Option<(EntryId, JournalEntry)>,
}

/// The newest saved version of a document.
pub fn latest_document(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
) -> Result<LedgerDocument, BooksError> {
    let raw = ledger
        .latest_document_payload(company, document_id)?
        .ok_or_else(|| BooksError::UnknownDocument(document_id.to_string()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// The same journal effect, whichever version it was posted from.
fn same_posting(a: &JournalEntry, b: &JournalEntry) -> bool {
    a.entry_date == b.entry_date
        && a.memo == b.memo
        && a.source_type == b.source_type
        && a.is_flagged == b.is_flagged
        && a.lines == b.lines
}

fn check_unlocked(ledger: &Ledger, company: &str, date: NaiveDate) -> Result<(), BooksError> {
    if let Some(locked) = ledger.locked_through(company)? {
        if date <= locked {
            return Err(LedgerError::PeriodClosed { period_end: locked }.into());
        }
    }
    Ok(())
}

/// The book a kind opens an item on.
fn book_of(kind: DocKind) -> Option<Book> {
    [Book::Receivable, Book::Payable]
        .into_iter()
        .find(|book| book.opens(kind).is_some())
}

/// Everything saved documents other than `excluding` apply against
/// `document_id`.
fn applied_by_others(
    ledger: &Ledger,
    company: &str,
    book: Book,
    document_id: &str,
    excluding: &str,
) -> Result<Money, BooksError> {
    let mut total = Money::ZERO;
    for doc in latest_documents(ledger, company, book.doc_types(), far_future())? {
        if doc.document_id == excluding || !book.settles(doc.kind) {
            continue;
        }
        for application in &doc.applications {
            if application.target_document_id == document_id {
                total = total.checked_add(application.amount)?;
            }
        }
    }
    Ok(total)
}

/// What the new version does to the journal.
enum Reposting<'a> {
    /// Post what `post::post` derives from the new version with this context,
    /// reversing the old entry only if that differs.
    Post(&'a PostingContext),
    /// Reverse every live entry; post nothing new. The reason goes on the
    /// reversals' memo.
    Void(&'a str),
    /// The posting cannot have changed (only applications moved on a credit
    /// memo, which posts the same either way): save the version alone.
    Keep,
}

/// Saves `doc` as the next version of `old` and brings the journal in line,
/// as one unit. Both the old and the new date must be in an open period: an
/// edit restates the document where it stood, so every reversal is dated
/// the entry it reverses, and nothing in a closed period may move.
fn commit_revision(
    ledger: &Ledger,
    company: &str,
    old: &LedgerDocument,
    doc: LedgerDocument,
    reposting: Reposting<'_>,
    command: CommandMeta,
    now: DateTime<Utc>,
) -> Result<Revision, BooksError> {
    check_unlocked(ledger, company, old.txn_date)?;
    check_unlocked(ledger, company, doc.txn_date)?;
    let live = ledger.live_entries_for_document(company, &doc.document_id)?;
    let next_version = ledger.document_version_count(company, &doc.document_id)? + 1;

    let (new_entry, reverse) = match reposting {
        Reposting::Keep => (None, false),
        Reposting::Void(_) => (None, true),
        Reposting::Post(ctx) => match post::post(&doc, next_version, ctx)? {
            Posting::NonPosting => (None, true),
            Posting::Entry(entry) => {
                let unchanged =
                    matches!(&live[..], [(_, old_entry)] if same_posting(old_entry, &entry));
                if unchanged {
                    (None, false)
                } else {
                    (Some(entry), true)
                }
            }
        },
    };
    let void_reason = match reposting {
        Reposting::Void(reason) => Some(reason),
        _ => None,
    };

    ledger.atomically(|| {
        let mut reversals = Vec::new();
        if reverse {
            for (entry_id, entry) in &live {
                let mut reversal = entry.reversed(entry.entry_date, entry_id);
                if let Some(reason) = void_reason {
                    let original = entry.memo.clone().unwrap_or_default();
                    reversal.memo = Some(format!("VOID: {original} ({reason})"));
                }
                reversals.push(ledger.post_entry(company, &reversal, now)?);
            }
        }
        let version = ledger.save_document(company, &doc, command, now)?;
        let entry = match new_entry {
            Some(mut entry) => {
                entry.source_version = version.version;
                let entry_id = ledger.post_entry(company, &entry, now)?;
                Some((entry_id, entry))
            }
            None => None,
        };
        Ok(Revision {
            document: doc,
            version: version.version,
            reversals,
            entry,
        })
    })
}

/// Edits a saved document: the same checks and fills as
/// [`prepare_document`], then a new version, with the old entry reversed and
/// the new one posted when what it posts changed. Refused: a voided
/// document; a change of kind or of contact type; renumbering a document the
/// book numbers (left blank, the number is kept); a total that falls below
/// what payments and credits already apply against it; anything dated in a
/// closed period, before or after the edit.
pub fn update_document(
    ledger: &Ledger,
    company: &str,
    mut doc: LedgerDocument,
    config: PostingConfig,
    command: CommandMeta,
    now: DateTime<Utc>,
) -> Result<Revision, BooksError> {
    let old = latest_document(ledger, company, &doc.document_id)?;
    if old.is_voided {
        return Err(BooksError::Voided(doc.document_id));
    }
    if doc.is_voided {
        return Err(BooksError::VoidThroughEdit);
    }
    if doc.kind != old.kind {
        return Err(BooksError::KindChanged {
            id: doc.document_id,
            from: old.kind.as_str(),
            to: doc.kind.as_str(),
        });
    }
    if let (Some(was), Some(now_names)) = (&old.contact, &doc.contact) {
        if was.kind != now_names.kind {
            return Err(BooksError::ContactTypeChanged {
                id: doc.document_id,
                from: contact_kind_str(was.kind),
                to: contact_kind_str(now_names.kind),
            });
        }
    }
    match (&doc.number, &old.number) {
        (None, _) => doc.number = old.number.clone(),
        (Some(requested), kept) if is_numbered(doc.kind) && Some(requested) != kept.as_ref() => {
            return Err(BooksError::NumberChanged {
                kind: doc.kind.as_str(),
                id: doc.document_id,
                kept: kept.clone().unwrap_or_default(),
                requested: requested.clone(),
            });
        }
        _ => {}
    }
    if doc.source_ref.is_none() {
        doc.source_ref = old.source_ref.clone();
    }

    let (doc, ctx) = prepare_document(ledger, company, doc, config)?;
    if let Some(book) = book_of(doc.kind) {
        let mut applied =
            applied_by_others(ledger, company, book, &doc.document_id, &doc.document_id)?;
        if book.opens(doc.kind) == Some(true) {
            applied = applied.checked_add(Money::checked_sum(
                doc.applications.iter().map(|a| a.amount),
            )?)?;
        }
        let total = document_total(&doc)?;
        if applied > total {
            return Err(BooksError::Overapplied {
                id: doc.document_id,
                applied: Money::display(applied),
                total: Money::display(total),
            });
        }
    }
    for application in &doc.applications {
        let target = latest_document(ledger, company, &application.target_document_id)?;
        // A link to a contract (the sales order an invoice was made from)
        // moves no money; the contract may since have been voided (D42).
        if target.kind.is_contract() {
            continue;
        }
        if target.is_voided {
            return Err(BooksError::Voided(target.document_id));
        }
        let Some(book) = book_of(target.kind) else {
            continue;
        };
        let applied =
            applied_by_others(ledger, company, book, &target.document_id, &doc.document_id)?
                .checked_add(application.amount)?;
        let total = document_total(&target)?;
        if applied > total {
            return Err(BooksError::Overapplied {
                id: target.document_id,
                applied: Money::display(applied),
                total: Money::display(total),
            });
        }
    }
    commit_revision(
        ledger,
        company,
        &old,
        doc,
        Reposting::Post(&ctx),
        command,
        now,
    )
}

/// Voids a saved document (§6: it posts a reversal rather than vanishing):
/// a new version with `is_voided` set and `reason` on its memo, and every
/// live entry reversed as of its own date. A voided invoice or bill drops off
/// the open items; a voided payment or credit releases what it applied.
/// Refused: a document already voided; an invoice or bill something is still
/// applied against (void or edit that first); a closed period.
pub fn void_document(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    reason: &str,
    command: CommandMeta,
    now: DateTime<Utc>,
) -> Result<Revision, BooksError> {
    let old = latest_document(ledger, company, document_id)?;
    if old.is_voided {
        return Err(BooksError::Voided(document_id.to_string()));
    }
    if let Some(book) = book_of(old.kind) {
        let applied = applied_by_others(ledger, company, book, document_id, document_id)?;
        if !applied.is_zero() {
            return Err(BooksError::HasApplications {
                id: document_id.to_string(),
                applied: Money::display(applied),
            });
        }
    }
    let mut doc = old.clone();
    doc.is_voided = true;
    doc.memo = Some(match &old.memo {
        Some(memo) if !memo.is_empty() => format!("Voided: {reason}. {memo}"),
        _ => format!("Voided: {reason}"),
    });
    commit_revision(
        ledger,
        company,
        &old,
        doc,
        Reposting::Void(reason),
        command,
        now,
    )
}

/// Applies an open credit memo to an open invoice of the same customer (or
/// an open vendor credit to an open bill): a new version of the credit with
/// the application on it, which [`open_documents`] and [`aging`] already net
/// on both sides. The credit's posting does not depend on its applications,
/// so nothing is reversed or reposted. `amount` defaults to the smaller of
/// the two open balances, and may not exceed either.
pub fn apply_credit(
    ledger: &Ledger,
    company: &str,
    credit_id: &str,
    target_id: &str,
    amount: Option<Money>,
    command: CommandMeta,
    now: DateTime<Utc>,
) -> Result<Revision, BooksError> {
    let credit = latest_document(ledger, company, credit_id)?;
    let target_kind = match credit.kind {
        DocKind::CreditMemo => DocKind::Invoice,
        DocKind::VendorCredit => DocKind::Bill,
        other => {
            return Err(BooksError::BadApplication(format!(
                "{credit_id} is a {}, not a credit memo or vendor credit",
                other.as_str()
            )))
        }
    };
    if credit.is_voided {
        return Err(BooksError::Voided(credit_id.to_string()));
    }
    let target = latest_document(ledger, company, target_id)?;
    if target.kind != target_kind {
        return Err(BooksError::BadApplication(format!(
            "a {} applies to a {}, and {target_id} is a {}",
            credit.kind.as_str(),
            target_kind.as_str(),
            target.kind.as_str()
        )));
    }
    if target.is_voided {
        return Err(BooksError::Voided(target_id.to_string()));
    }
    let contact_of = |doc: &LedgerDocument| doc.contact.as_ref().map(|c| c.id.clone());
    if contact_of(&credit) != contact_of(&target) {
        return Err(BooksError::BadApplication(format!(
            "{credit_id} and {target_id} belong to different contacts"
        )));
    }

    let book = book_of(credit.kind).expect("a credit opens on a book");
    let open = open_documents(ledger, company, book, far_future())?;
    let open_of = |id: &str| {
        open.iter()
            .find(|d| d.document_id == id)
            .map_or(Ok(Money::ZERO), |d| {
                if d.open < Money::ZERO {
                    d.open.checked_neg()
                } else {
                    Ok(d.open)
                }
            })
    };
    let credit_open = open_of(credit_id)?;
    let target_open = open_of(target_id)?;
    let amount = amount.unwrap_or_else(|| credit_open.min(target_open));
    if amount <= Money::ZERO {
        return Err(BooksError::BadApplication(format!(
            "nothing to apply: {credit_id} has {} open and {target_id} has {} open",
            Money::display(credit_open),
            Money::display(target_open)
        )));
    }
    if amount > credit_open || amount > target_open {
        return Err(BooksError::BadApplication(format!(
            "{} is more than is open: {credit_id} has {}, {target_id} has {}",
            Money::display(amount),
            Money::display(credit_open),
            Money::display(target_open)
        )));
    }

    let mut doc = credit.clone();
    match doc
        .applications
        .iter_mut()
        .find(|a| a.target_document_id == target_id)
    {
        Some(existing) => existing.amount = existing.amount.checked_add(amount)?,
        None => doc.applications.push(Application {
            target_document_id: target_id.to_string(),
            target_kind,
            amount,
        }),
    }
    commit_revision(ledger, company, &credit, doc, Reposting::Keep, command, now)
}

/// The contact a document names, as the document records it.
pub fn contact_ref(kind: ContactKind, id: impl Into<String>) -> ContactRef {
    ContactRef {
        kind,
        id: id.into(),
    }
}
