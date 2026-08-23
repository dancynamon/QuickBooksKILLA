//! Turning mirrored QBO payloads into the parsed projection. `DESIGN.md` §3.2.
//!
//! `entities` holds the raw JSON and is the durable truth. Everything here is
//! derived from it, which is what makes a parser bug cheap: fix the parse, run
//! [`crate::store::Store::reproject_all`], and the corrected projection comes
//! back off local disk with no Intuit round-trip.
//!
//! This module is **pure**. It reads JSON and returns values; it never touches
//! SQLite. That keeps the realm-scoping invariant of §2.1 intact — the
//! connection stays private to `store` — and it means every parsing rule below
//! is testable against a payload literal with no database in the way.

use std::str::FromStr;

use ledger_core::{round_money, Money, MoneyError, RoundingPolicy};
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

use crate::domain::{ContactType, DocumentType, EntityType};
use crate::store::MirroredEntity;

/// Beyond this magnitude an `f64` can no longer represent consecutive integers,
/// so a JSON number's decimal literal may not survive the round trip. Real
/// amounts are nowhere near it; a payload that is means something is wrong.
const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_992.0; // 2^53

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectError {
    #[error("{entity} {id}: required field `{field}` is missing or not readable")]
    MissingField {
        entity: &'static str,
        id: String,
        field: &'static str,
    },
    #[error("{field} is not a number this build will read as money: {found}")]
    NotAnAmount { field: &'static str, found: String },
    #[error("{field} carries more than two decimal places: {found}")]
    AmountPrecision { field: &'static str, found: String },
    #[error("money conversion failed: {0}")]
    Money(#[from] MoneyError),
}

/// Whether a mirrored entity produced a projection, and if not, why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Projection {
    /// Parsed and ready to write.
    Parsed(Box<ParsedEntity>),
    /// This entity type has no projected form — `CompanyInfo`, `Preferences`,
    /// tax tables. The raw JSON in `entities` remains its only representation.
    NotProjected,
    /// The payload was readable JSON but not readable as this entity. The row
    /// is quarantined rather than dropped (§7): the raw JSON is still on disk,
    /// so a later parser fix can recover it.
    Quarantine(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedEntity {
    Contact(ParsedContact),
    Item(ParsedItem),
    Account(ParsedAccount),
    Class(ParsedClass),
    Document(ParsedDocument),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedContact {
    pub contact_type: ContactType,
    pub qbo_id: String,
    pub display_name: String,
    pub company_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub balance: Option<Money>,
    pub is_active: bool,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedItem {
    pub qbo_id: String,
    pub name: String,
    pub sku: Option<String>,
    pub description: Option<String>,
    pub item_type: Option<String>,
    pub unit_price: Option<Decimal>,
    pub purchase_cost: Option<Decimal>,
    pub qty_on_hand: Option<Decimal>,
    pub income_account_id: Option<String>,
    pub expense_account_id: Option<String>,
    pub asset_account_id: Option<String>,
    pub is_active: bool,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAccount {
    pub qbo_id: String,
    pub name: String,
    pub acct_num: Option<String>,
    pub account_type: Option<String>,
    pub account_subtype: Option<String>,
    pub classification: Option<String>,
    pub balance: Option<Money>,
    pub is_active: bool,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedClass {
    pub qbo_id: String,
    pub name: String,
    pub fully_qualified_name: Option<String>,
    pub parent_id: Option<String>,
    pub is_active: bool,
    pub is_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedDocument {
    pub qbo_id: String,
    pub doc_type: DocumentType,
    pub doc_number: Option<String>,
    pub txn_date: String,
    pub due_date: Option<String>,
    pub contact_id: Option<String>,
    pub contact_type: Option<ContactType>,
    pub class_id: Option<String>,
    pub total: Money,
    pub balance: Option<Money>,
    pub doc_status: Option<String>,
    pub po_number: Option<String>,
    pub private_note: Option<String>,
    pub customer_memo: Option<String>,
    pub currency: Option<String>,
    pub is_deleted: bool,
    pub lines: Vec<ParsedLine>,
    pub links: Vec<ParsedLink>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    pub line_no: i64,
    pub item_id: Option<String>,
    pub description: Option<String>,
    pub qty: Option<Decimal>,
    pub unit_price: Option<Decimal>,
    pub amount: Money,
    pub class_id: Option<String>,
    pub is_taxable: bool,
}

/// One edge of QBO's `LinkedTxn` graph, flattened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLink {
    pub to_qbo_id: String,
    pub to_type: String,
    /// `None` when the link is on the document header rather than a line.
    pub line_no: Option<i64>,
}

/// Parse a mirrored entity into its projected form.
pub fn parse(entity: &MirroredEntity) -> Projection {
    let raw = &entity.raw_json;
    let result = match entity.entity_type {
        EntityType::Customer => parse_contact(raw, entity, ContactType::Customer).map(|c| {
            Some(ParsedEntity::Contact(c))
        }),
        EntityType::Vendor => parse_contact(raw, entity, ContactType::Vendor).map(|c| {
            Some(ParsedEntity::Contact(c))
        }),
        EntityType::Item => parse_item(raw, entity).map(|i| Some(ParsedEntity::Item(i))),
        EntityType::Account => parse_account(raw, entity).map(|a| Some(ParsedEntity::Account(a))),
        EntityType::Class => parse_class(raw, entity).map(|c| Some(ParsedEntity::Class(c))),
        other => match other.as_document() {
            Some(doc_type) => {
                parse_document(raw, entity, doc_type).map(|d| Some(ParsedEntity::Document(d)))
            }
            None => Ok(None),
        },
    };

    match result {
        Ok(Some(parsed)) => Projection::Parsed(Box::new(parsed)),
        Ok(None) => Projection::NotProjected,
        Err(err) => Projection::Quarantine(err.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Masters
// ---------------------------------------------------------------------------

fn parse_contact(
    raw: &Value,
    entity: &MirroredEntity,
    contact_type: ContactType,
) -> Result<ParsedContact, ProjectError> {
    Ok(ParsedContact {
        contact_type,
        qbo_id: entity.qbo_id.clone(),
        display_name: required_string(raw, "DisplayName", entity)?,
        company_name: text(raw, "CompanyName"),
        email: nested_text(raw, "PrimaryEmailAddr", "Address"),
        phone: nested_text(raw, "PrimaryPhone", "FreeFormNumber"),
        balance: optional_money(raw, "Balance")?,
        is_active: flag(raw, "Active").unwrap_or(true),
        is_deleted: entity.is_deleted,
    })
}

fn parse_item(raw: &Value, entity: &MirroredEntity) -> Result<ParsedItem, ProjectError> {
    Ok(ParsedItem {
        qbo_id: entity.qbo_id.clone(),
        name: required_string(raw, "Name", entity)?,
        sku: text(raw, "Sku"),
        description: text(raw, "Description"),
        item_type: text(raw, "Type"),
        unit_price: optional_decimal(raw, "UnitPrice")?,
        purchase_cost: optional_decimal(raw, "PurchaseCost")?,
        qty_on_hand: optional_decimal(raw, "QtyOnHand")?,
        income_account_id: reference(raw, "IncomeAccountRef"),
        expense_account_id: reference(raw, "ExpenseAccountRef"),
        asset_account_id: reference(raw, "AssetAccountRef"),
        is_active: flag(raw, "Active").unwrap_or(true),
        is_deleted: entity.is_deleted,
    })
}

fn parse_account(raw: &Value, entity: &MirroredEntity) -> Result<ParsedAccount, ProjectError> {
    Ok(ParsedAccount {
        qbo_id: entity.qbo_id.clone(),
        name: required_string(raw, "Name", entity)?,
        acct_num: text(raw, "AcctNum"),
        account_type: text(raw, "AccountType"),
        account_subtype: text(raw, "AccountSubType"),
        classification: text(raw, "Classification"),
        balance: optional_money(raw, "CurrentBalance")?,
        is_active: flag(raw, "Active").unwrap_or(true),
        is_deleted: entity.is_deleted,
    })
}

fn parse_class(raw: &Value, entity: &MirroredEntity) -> Result<ParsedClass, ProjectError> {
    Ok(ParsedClass {
        qbo_id: entity.qbo_id.clone(),
        name: required_string(raw, "Name", entity)?,
        fully_qualified_name: text(raw, "FullyQualifiedName"),
        parent_id: reference(raw, "ParentRef"),
        is_active: flag(raw, "Active").unwrap_or(true),
        is_deleted: entity.is_deleted,
    })
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

fn parse_document(
    raw: &Value,
    entity: &MirroredEntity,
    doc_type: DocumentType,
) -> Result<ParsedDocument, ProjectError> {
    let (contact_type, contact_id) = contact_of(raw, doc_type);

    // A total we could not read is not zero. Refusing here sends the row to
    // quarantine with its raw JSON intact, which is recoverable; writing a
    // wrong number into the projection is not.
    let total = required_money(raw, "TotalAmt", entity)?;

    let lines = parse_lines(raw)?;
    let links = parse_links(raw);

    Ok(ParsedDocument {
        qbo_id: entity.qbo_id.clone(),
        doc_type,
        doc_number: text(raw, "DocNumber"),
        txn_date: required_string(raw, "TxnDate", entity)?,
        due_date: text(raw, "DueDate"),
        contact_id,
        contact_type,
        class_id: reference(raw, "ClassRef"),
        total,
        balance: optional_money(raw, "Balance")?,
        doc_status: text(raw, "TxnStatus").or_else(|| text(raw, "POStatus")),
        po_number: po_number(raw),
        private_note: text(raw, "PrivateNote"),
        customer_memo: nested_text(raw, "CustomerMemo", "value"),
        currency: reference(raw, "CurrencyRef"),
        is_deleted: entity.is_deleted,
        lines,
        links,
    })
}

/// Which side of the book a document faces, and who it points at.
///
/// `Purchase` is the awkward one: its `EntityRef` may be a vendor, a customer
/// or an employee, so the payload's own `type` decides rather than the document
/// type. `Deposit` and `JournalEntry` face nobody.
fn contact_of(raw: &Value, doc_type: DocumentType) -> (Option<ContactType>, Option<String>) {
    use DocumentType as D;
    match doc_type {
        D::Estimate
        | D::Invoice
        | D::SalesReceipt
        | D::CreditMemo
        | D::RefundReceipt
        | D::Payment => (Some(ContactType::Customer), reference(raw, "CustomerRef")),
        D::PurchaseOrder | D::Bill | D::BillPayment | D::VendorCredit => {
            (Some(ContactType::Vendor), reference(raw, "VendorRef"))
        }
        D::Purchase => {
            let id = reference(raw, "EntityRef");
            let kind = nested_text(raw, "EntityRef", "type")
                .as_deref()
                .and_then(ContactType::parse);
            match id {
                Some(id) => (kind, Some(id)),
                None => (None, None),
            }
        }
        D::Deposit | D::JournalEntry => (None, None),
    }
}

fn parse_lines(raw: &Value) -> Result<Vec<ParsedLine>, ProjectError> {
    let Some(lines) = raw.get("Line").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let detail_type = text(line, "DetailType");
        // A subtotal restates lines above it. Projecting it would double every
        // total computed off `document_lines`.
        if detail_type.as_deref() == Some("SubTotalLineDetail") {
            continue;
        }

        let detail = detail_object(line, detail_type.as_deref());

        parsed.push(ParsedLine {
            line_no: line_number(line, index),
            item_id: detail.and_then(|d| reference(d, "ItemRef")),
            description: text(line, "Description"),
            qty: detail.map(|d| optional_decimal(d, "Qty")).transpose()?.flatten(),
            unit_price: detail
                .map(|d| optional_decimal(d, "UnitPrice"))
                .transpose()?
                .flatten(),
            // A description-only line carries no `Amount` and genuinely
            // contributes nothing, so zero is the honest reading here — unlike
            // a missing document total, which is not.
            amount: optional_money(line, "Amount")?.unwrap_or(Money::ZERO),
            class_id: detail.and_then(|d| reference(d, "ClassRef")),
            is_taxable: detail.is_some_and(is_taxable),
        });
    }
    Ok(parsed)
}

/// QBO nests a line's specifics under a key named for its detail type —
/// `SalesItemLineDetail`, `ItemBasedExpenseLineDetail`, and so on. Rather than
/// enumerate every one, take the payload at its word and follow `DetailType`,
/// falling back to whichever key ends in `LineDetail`.
fn detail_object<'a>(line: &'a Value, detail_type: Option<&str>) -> Option<&'a Value> {
    if let Some(key) = detail_type {
        if let Some(found) = line.get(key) {
            return Some(found);
        }
    }
    line.as_object()?
        .iter()
        .find(|(key, _)| key.ends_with("LineDetail"))
        .map(|(_, value)| value)
}

fn line_number(line: &Value, index: usize) -> i64 {
    line.get("LineNum")
        .and_then(json_decimal_value)
        .and_then(|d| i64::try_from(d.trunc()).ok())
        .unwrap_or_else(|| index as i64 + 1)
}

/// US files use the sentinel code `NON` for a non-taxable line; other locales
/// carry a real tax code id. Either way, the absence of taxability is what is
/// stated explicitly, so treat any other code as taxable.
fn is_taxable(detail: &Value) -> bool {
    match reference(detail, "TaxCodeRef") {
        Some(code) => !code.eq_ignore_ascii_case("NON"),
        None => false,
    }
}

/// Header links first, then per-line links, in payload order. Both directions
/// of the graph are read from this one list; see `idx_links_to`.
fn parse_links(raw: &Value) -> Vec<ParsedLink> {
    let mut links = Vec::new();

    if let Some(header) = raw.get("LinkedTxn").and_then(Value::as_array) {
        links.extend(header.iter().filter_map(|link| linked_txn(link, None)));
    }

    if let Some(lines) = raw.get("Line").and_then(Value::as_array) {
        for (index, line) in lines.iter().enumerate() {
            let Some(on_line) = line.get("LinkedTxn").and_then(Value::as_array) else {
                continue;
            };
            let line_no = line_number(line, index);
            links.extend(
                on_line
                    .iter()
                    .filter_map(|link| linked_txn(link, Some(line_no))),
            );
        }
    }

    links
}

fn linked_txn(link: &Value, line_no: Option<i64>) -> Option<ParsedLink> {
    Some(ParsedLink {
        to_qbo_id: text(link, "TxnId")?,
        to_type: text(link, "TxnType")?,
        line_no,
    })
}

/// QBO has no first-class PO field on a sales document. The customer's PO
/// number lives in a custom field whose name the file's owner chose, so match
/// on the normalised name rather than an exact string.
fn po_number(raw: &Value) -> Option<String> {
    raw.get("CustomField")?
        .as_array()?
        .iter()
        .find_map(|field| {
            let name: String = text(field, "Name")?
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            matches!(name.as_str(), "ponumber" | "po" | "purchaseordernumber")
                .then(|| text(field, "StringValue"))
                .flatten()
        })
}

// ---------------------------------------------------------------------------
// Field readers
// ---------------------------------------------------------------------------

/// Empty strings are QBO's way of saying "unset"; treat them as absent so the
/// projection does not carry blanks that look like data.
fn text(value: &Value, key: &str) -> Option<String> {
    let found = value.get(key)?;
    let string = match found {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!string.is_empty()).then_some(string)
}

fn nested_text(value: &Value, outer: &str, inner: &str) -> Option<String> {
    text(value.get(outer)?, inner)
}

fn reference(value: &Value, key: &str) -> Option<String> {
    nested_text(value, key, "value")
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    value.get(key)?.as_bool()
}

fn required_string(
    value: &Value,
    key: &'static str,
    entity: &MirroredEntity,
) -> Result<String, ProjectError> {
    text(value, key).ok_or_else(|| ProjectError::MissingField {
        entity: entity.entity_type.as_str(),
        id: entity.qbo_id.clone(),
        field: key,
    })
}

/// A JSON number as an exact `Decimal`.
///
/// `serde_json` parses a fractional literal into `f64`, and `Number`'s own
/// `Display` prints the shortest string that round-trips back to that `f64` —
/// so `1234.56` comes back as `"1234.56"`, not `1234.5599999999999`. That
/// holds below 2^53; past it consecutive integers are no longer distinct, so
/// this refuses rather than guesses.
fn json_decimal_value(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                return Some(Decimal::from(int));
            }
            let float = number.as_f64()?;
            if !float.is_finite() || float.abs() >= MAX_EXACT_F64_INTEGER {
                return None;
            }
            Decimal::from_str(&number.to_string()).ok()
        }
        // Some QBO fields arrive quoted. A decimal string is exact as written.
        Value::String(string) => Decimal::from_str(string.trim()).ok(),
        _ => None,
    }
}

fn optional_decimal(value: &Value, key: &'static str) -> Result<Option<Decimal>, ProjectError> {
    let Some(found) = value.get(key) else {
        return Ok(None);
    };
    if found.is_null() {
        return Ok(None);
    }
    json_decimal_value(found)
        .map(Some)
        .ok_or_else(|| ProjectError::NotAnAmount {
            field: key,
            found: found.to_string(),
        })
}

/// Amounts are two decimal places in QBO. Anything longer means this build has
/// misread the field, so it refuses rather than rounding a number it does not
/// understand — prices, which legitimately carry more, stay `Decimal`.
fn optional_money(value: &Value, key: &'static str) -> Result<Option<Money>, ProjectError> {
    let Some(decimal) = optional_decimal(value, key)? else {
        return Ok(None);
    };
    if decimal.scale() > 2 {
        return Err(ProjectError::AmountPrecision {
            field: key,
            found: decimal.to_string(),
        });
    }
    Ok(Some(round_money(decimal, RoundingPolicy::MirroredAmount)?))
}

fn required_money(
    value: &Value,
    key: &'static str,
    entity: &MirroredEntity,
) -> Result<Money, ProjectError> {
    optional_money(value, key)?.ok_or_else(|| ProjectError::MissingField {
        entity: entity.entity_type.as_str(),
        id: entity.qbo_id.clone(),
        field: key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    // Fixtures are invented, and stay that way — HANDOFF.md §2.6.
    fn entity(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
        MirroredEntity {
            entity_type,
            qbo_id: id.to_string(),
            sync_token: "0".into(),
            last_updated_utc: Utc::now(),
            is_deleted: false,
            raw_json: raw,
        }
    }

    fn document_of(entity: &MirroredEntity) -> ParsedDocument {
        match parse(entity) {
            Projection::Parsed(parsed) => match *parsed {
                ParsedEntity::Document(document) => document,
                other => panic!("expected a document, got {other:?}"),
            },
            other => panic!("expected a projection, got {other:?}"),
        }
    }

    fn invoice_payload() -> Value {
        json!({
            "Id": "418",
            "DocNumber": "1088",
            "TxnDate": "2026-07-14",
            "DueDate": "2026-08-13",
            "CustomerRef": { "value": "31", "name": "Blue Harbor Swim Club" },
            "ClassRef": { "value": "4" },
            "CurrencyRef": { "value": "USD" },
            "TotalAmt": 1234.56,
            "Balance": 234.56,
            "PrivateNote": "Rush — needs the dock bumpers first",
            "CustomerMemo": { "value": "Thank you for your business" },
            "CustomField": [
                { "DefinitionId": "1", "Name": "P.O. Number", "Type": "StringType",
                  "StringValue": "BH-99214" }
            ],
            "LinkedTxn": [ { "TxnId": "377", "TxnType": "Estimate" } ],
            "Line": [
                {
                    "Id": "1", "LineNum": 1,
                    "Description": "Rescue tube, 50 inch, red",
                    "Amount": 985.00,
                    "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": {
                        "ItemRef": { "value": "12" },
                        "UnitPrice": 49.25,
                        "Qty": 20,
                        "ClassRef": { "value": "4" },
                        "TaxCodeRef": { "value": "TAX" }
                    }
                },
                {
                    "Id": "2", "LineNum": 2,
                    "Description": "Freight",
                    "Amount": 249.56,
                    "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": {
                        "ItemRef": { "value": "3" },
                        "TaxCodeRef": { "value": "NON" }
                    }
                },
                {
                    "Amount": 1234.56,
                    "DetailType": "SubTotalLineDetail",
                    "SubTotalLineDetail": {}
                }
            ]
        })
    }

    #[test]
    fn an_invoice_projects_its_header() {
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));

        assert_eq!(document.doc_type, DocumentType::Invoice);
        assert_eq!(document.doc_number.as_deref(), Some("1088"));
        assert_eq!(document.txn_date, "2026-07-14");
        assert_eq!(document.due_date.as_deref(), Some("2026-08-13"));
        assert_eq!(document.contact_type, Some(ContactType::Customer));
        assert_eq!(document.contact_id.as_deref(), Some("31"));
        assert_eq!(document.class_id.as_deref(), Some("4"));
        assert_eq!(document.total, Money::from_minor(123_456));
        assert_eq!(document.balance, Some(Money::from_minor(23_456)));
        assert_eq!(document.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn the_customer_po_comes_out_of_the_custom_field() {
        // QBO has no first-class PO field on a sales document; it is whatever
        // the file's owner named the custom field.
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));
        assert_eq!(document.po_number.as_deref(), Some("BH-99214"));

        for name in ["PO Number", "ponumber", "P.O. NUMBER", "Purchase Order Number"] {
            let mut payload = invoice_payload();
            payload["CustomField"][0]["Name"] = json!(name);
            let document = document_of(&entity(EntityType::Invoice, "418", payload));
            assert_eq!(
                document.po_number.as_deref(),
                Some("BH-99214"),
                "should have matched custom field named {name:?}"
            );
        }
    }

    #[test]
    fn a_custom_field_that_is_not_a_po_is_left_alone() {
        let mut payload = invoice_payload();
        payload["CustomField"][0]["Name"] = json!("Sales Rep");
        let document = document_of(&entity(EntityType::Invoice, "418", payload));
        assert_eq!(document.po_number, None);
    }

    #[test]
    fn a_subtotal_line_is_not_projected() {
        // Projecting it would double every total computed off document_lines.
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));
        assert_eq!(document.lines.len(), 2);
        assert_eq!(
            Money::checked_sum(document.lines.iter().map(|line| line.amount)).unwrap(),
            document.total
        );
    }

    #[test]
    fn a_line_carries_its_item_quantity_and_rate() {
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));
        let line = &document.lines[0];

        assert_eq!(line.line_no, 1);
        assert_eq!(line.item_id.as_deref(), Some("12"));
        assert_eq!(line.qty, Some(Decimal::from(20)));
        assert_eq!(line.unit_price, Some(Decimal::from_str("49.25").unwrap()));
        assert_eq!(line.amount, Money::from_minor(98_500));
        assert_eq!(line.class_id.as_deref(), Some("4"));
    }

    #[test]
    fn taxability_is_read_from_the_tax_code_not_guessed() {
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));
        assert!(document.lines[0].is_taxable, "TAX means taxable");
        assert!(!document.lines[1].is_taxable, "NON means it is not");
    }

    #[test]
    fn a_header_link_carries_no_line_number() {
        let document = document_of(&entity(EntityType::Invoice, "418", invoice_payload()));
        assert_eq!(
            document.links,
            vec![ParsedLink {
                to_qbo_id: "377".into(),
                to_type: "Estimate".into(),
                line_no: None,
            }]
        );
    }

    #[test]
    fn a_bill_payment_links_per_line() {
        // Each line settles one bill, so the link has to say which line.
        let payload = json!({
            "Id": "902",
            "TxnDate": "2026-07-31",
            "VendorRef": { "value": "77" },
            "TotalAmt": 4100.00,
            "Line": [
                { "Amount": 2500.00, "LinkedTxn": [ { "TxnId": "801", "TxnType": "Bill" } ] },
                { "Amount": 1600.00, "LinkedTxn": [ { "TxnId": "812", "TxnType": "Bill" } ] }
            ]
        });
        let document = document_of(&entity(EntityType::BillPayment, "902", payload));

        assert_eq!(document.contact_type, Some(ContactType::Vendor));
        assert_eq!(
            document.links,
            vec![
                ParsedLink { to_qbo_id: "801".into(), to_type: "Bill".into(), line_no: Some(1) },
                ParsedLink { to_qbo_id: "812".into(), to_type: "Bill".into(), line_no: Some(2) },
            ]
        );
    }

    #[test]
    fn a_description_only_line_is_zero_rather_than_a_failure() {
        let payload = json!({
            "Id": "5", "TxnDate": "2026-05-02", "TotalAmt": 0,
            "CustomerRef": { "value": "31" },
            "Line": [ { "Description": "See attached drawing", "DetailType": "DescriptionOnly",
                        "DescriptionLineDetail": {} } ]
        });
        let document = document_of(&entity(EntityType::Invoice, "5", payload));
        assert_eq!(document.lines[0].amount, Money::ZERO);
        assert_eq!(document.lines[0].description.as_deref(), Some("See attached drawing"));
    }

    #[test]
    fn a_total_we_cannot_read_is_quarantined_not_treated_as_zero() {
        let mut payload = invoice_payload();
        payload.as_object_mut().unwrap().remove("TotalAmt");

        match parse(&entity(EntityType::Invoice, "418", payload)) {
            Projection::Quarantine(reason) => assert!(
                reason.contains("TotalAmt"),
                "reason should name the field, got {reason:?}"
            ),
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn an_amount_with_sub_cent_precision_is_quarantined() {
        // Not rounded silently: three decimals means this build has misread the
        // field, and the raw JSON is still on disk to re-read once it knows how.
        let mut payload = invoice_payload();
        payload["TotalAmt"] = json!(1234.567);

        match parse(&entity(EntityType::Invoice, "418", payload)) {
            Projection::Quarantine(reason) => assert!(reason.contains("decimal")),
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn amounts_survive_json_without_float_drift() {
        for (literal, minor) in [
            (json!(1234.56), 123_456_i64),
            (json!(0.07), 7),
            (json!(8.10), 810),
            (json!(123456789.99), 12_345_678_999),
            (json!(-45.30), -4_530),
            (json!(100), 10_000),
        ] {
            let mut payload = invoice_payload();
            payload["TotalAmt"] = literal.clone();
            let document = document_of(&entity(EntityType::Invoice, "418", payload));
            assert_eq!(
                document.total,
                Money::from_minor(minor),
                "{literal} should be {minor} minor units"
            );
        }
    }

    #[test]
    fn an_amount_past_exact_integer_range_is_refused_rather_than_guessed() {
        let mut payload = invoice_payload();
        payload["TotalAmt"] = json!(MAX_EXACT_F64_INTEGER);

        match parse(&entity(EntityType::Invoice, "418", payload)) {
            Projection::Quarantine(_) => {}
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn a_purchase_takes_its_side_from_the_payload_not_the_document_type() {
        // EntityRef on a Purchase may be a vendor, a customer or an employee.
        let base = json!({
            "Id": "77", "TxnDate": "2026-03-09", "TotalAmt": 220.00,
            "EntityRef": { "value": "51", "type": "Vendor" }
        });
        let document = document_of(&entity(EntityType::Purchase, "77", base.clone()));
        assert_eq!(document.contact_type, Some(ContactType::Vendor));
        assert_eq!(document.contact_id.as_deref(), Some("51"));

        let mut employee = base;
        employee["EntityRef"]["type"] = json!("Employee");
        let document = document_of(&entity(EntityType::Purchase, "77", employee));
        assert_eq!(
            document.contact_type, None,
            "an employee is neither a customer nor a vendor"
        );
        assert_eq!(document.contact_id.as_deref(), Some("51"));
    }

    #[test]
    fn a_customer_projects_to_a_contact() {
        let payload = json!({
            "Id": "31",
            "DisplayName": "Blue Harbor Swim Club",
            "CompanyName": "Blue Harbor Swim Club LLC",
            "PrimaryEmailAddr": { "Address": "ap@blueharborswim.example" },
            "PrimaryPhone": { "FreeFormNumber": "(555) 010-4417" },
            "Balance": 234.56,
            "Active": true
        });
        match parse(&entity(EntityType::Customer, "31", payload)) {
            Projection::Parsed(parsed) => match *parsed {
                ParsedEntity::Contact(contact) => {
                    assert_eq!(contact.contact_type, ContactType::Customer);
                    assert_eq!(contact.display_name, "Blue Harbor Swim Club");
                    assert_eq!(contact.balance, Some(Money::from_minor(23_456)));
                    assert!(contact.is_active);
                }
                other => panic!("expected a contact, got {other:?}"),
            },
            other => panic!("expected a projection, got {other:?}"),
        }
    }

    #[test]
    fn an_item_keeps_price_precision_that_money_would_lose() {
        let payload = json!({
            "Id": "12", "Name": "XLPE sheet, 2in blue", "Sku": "FOAM-2-BLU",
            "Type": "Inventory",
            "UnitPrice": 49.25,
            "PurchaseCost": 18.4375,
            "QtyOnHand": 132.5,
            "IncomeAccountRef": { "value": "79" },
            "Active": true
        });
        match parse(&entity(EntityType::Item, "12", payload)) {
            Projection::Parsed(parsed) => match *parsed {
                ParsedEntity::Item(item) => {
                    assert_eq!(item.sku.as_deref(), Some("FOAM-2-BLU"));
                    assert_eq!(
                        item.purchase_cost,
                        Some(Decimal::from_str("18.4375").unwrap()),
                        "a four-decimal cost is a price, not an amount"
                    );
                    assert_eq!(item.qty_on_hand, Some(Decimal::from_str("132.5").unwrap()));
                }
                other => panic!("expected an item, got {other:?}"),
            },
            other => panic!("expected a projection, got {other:?}"),
        }
    }

    #[test]
    fn entities_with_no_projected_form_say_so() {
        for entity_type in [EntityType::CompanyInfo, EntityType::Preferences, EntityType::TaxCode] {
            assert_eq!(
                parse(&entity(entity_type, "1", json!({ "Id": "1" }))),
                Projection::NotProjected,
                "{entity_type:?} has no projected form"
            );
        }
    }

    #[test]
    fn an_empty_string_is_absent_rather_than_present_and_blank() {
        let mut payload = invoice_payload();
        payload["DocNumber"] = json!("");
        payload["PrivateNote"] = json!("   ");
        let document = document_of(&entity(EntityType::Invoice, "418", payload));
        assert_eq!(document.doc_number, None);
        assert_eq!(document.private_note, None);
    }
}
