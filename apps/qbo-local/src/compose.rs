//! Turning what a person typed into the payload QBO accepts. `DESIGN.md` §1, §6.
//!
//! Line amounts are computed here, once, with [`RoundingPolicy::LineExtension`]
//! — the one rounding decision this project makes on the write path. Sales
//! tax is not: QuickBooks calculates it on save from each line's tax code, and
//! the replica mirrors what it computed. Showing a locally computed tax would
//! be a number QBO might not agree with.

use std::str::FromStr;

use ledger_core::{round_money, Money, RoundingPolicy};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::domain::DocumentType;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DraftLine {
    pub item_id: Option<String>,
    pub description: Option<String>,
    /// Decimal as text, exactly as typed, so `0.1` stays `0.1`.
    pub qty: String,
    pub rate: String,
    pub class_id: Option<String>,
    #[serde(default)]
    pub taxable: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDraft {
    pub doc_type: DocumentType,
    /// The customer on a sales document, the vendor on a purchase document: a
    /// QBO id, or a `local:` id for a customer created moments ago.
    #[serde(alias = "customerId")]
    pub contact_id: String,
    pub txn_date: String,
    pub due_date: Option<String>,
    pub doc_number: Option<String>,
    pub po_number: Option<String>,
    pub private_note: Option<String>,
    pub customer_memo: Option<String>,
    pub class_id: Option<String>,
    pub lines: Vec<DraftLine>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ComposeError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> ComposeError {
    ComposeError::Invalid(message.into())
}

/// The document types the editor writes. Payments, expenses, refunds and
/// journal entries are entered in QuickBooks until their editors exist: each
/// needs a bank or expense account chosen, which these do not.
pub const EDITABLE: &[DocumentType] = &[
    DocumentType::Estimate,
    DocumentType::Invoice,
    DocumentType::SalesReceipt,
    DocumentType::CreditMemo,
    DocumentType::PurchaseOrder,
    DocumentType::Bill,
];

/// Vendor-side documents: a `VendorRef`, item-based expense lines, no sales
/// tax code.
pub const fn is_purchase(doc_type: DocumentType) -> bool {
    matches!(doc_type, DocumentType::PurchaseOrder | DocumentType::Bill)
}

fn decimal(raw: &str, what: &str) -> Result<Decimal, ComposeError> {
    let trimmed = raw.trim().replace(',', "");
    Decimal::from_str(&trimmed).map_err(|_| invalid(format!("{what} {raw:?} is not a number")))
}

fn date(raw: &str, what: &str) -> Result<String, ComposeError> {
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(|d| d.to_string())
        .map_err(|_| invalid(format!("{what} {raw:?} is not a YYYY-MM-DD date")))
}

/// Exact decimal text as a JSON number. QBO reads the digits; no float is
/// involved in producing them.
fn number(value: Decimal) -> Value {
    Value::Number(serde_json::Number::from_str(&value.normalize().to_string()).expect("a Decimal prints as a JSON number"))
}

fn money(value: Money) -> Value {
    number(Decimal::new(value.minor(), 2))
}

/// Quantity times rate, rounded half away from zero to the cent.
pub fn line_amount(qty: &str, rate: &str) -> Result<Money, ComposeError> {
    let product = decimal(qty, "quantity")?
        .checked_mul(decimal(rate, "rate")?)
        .ok_or_else(|| invalid("quantity × rate overflows"))?;
    round_money(product, RoundingPolicy::LineExtension).map_err(|e| invalid(e.to_string()))
}

/// The subtotal the editor shows: the sum of rounded line amounts, which is
/// what QBO will hold before tax.
pub fn subtotal(lines: &[DraftLine]) -> Result<Money, ComposeError> {
    let amounts = lines.iter().map(|l| line_amount(&l.qty, &l.rate)).collect::<Result<Vec<_>, _>>()?;
    Money::checked_sum(amounts).map_err(|e| invalid(e.to_string()))
}

/// Build the QBO body for a draft.
///
/// `po_definition_id` is the `DefinitionId` of the book's "P.O. Number"
/// custom field, found from existing documents; a draft with a PO number
/// and no known field is refused rather than silently dropping the PO.
/// For an update the body is sparse: QBO changes only the fields sent, so a
/// field this editor does not know about is left as it was.
pub fn to_payload(draft: &DocumentDraft, po_definition_id: Option<&str>, update: bool) -> Result<Value, ComposeError> {
    if !EDITABLE.contains(&draft.doc_type) {
        return Err(invalid(format!("{} is not editable here yet", draft.doc_type.as_str())));
    }
    let purchase = is_purchase(draft.doc_type);
    if draft.contact_id.trim().is_empty() {
        return Err(invalid(if purchase { "choose a vendor" } else { "choose a customer" }));
    }
    if draft.lines.is_empty() {
        return Err(invalid("add at least one line"));
    }

    let mut lines = Vec::new();
    for (index, line) in draft.lines.iter().enumerate() {
        if line.item_id.is_none() && line.description.as_deref().unwrap_or("").trim().is_empty() {
            return Err(invalid(format!("line {} needs a product or a description", index + 1)));
        }
        let amount = line_amount(&line.qty, &line.rate)?;
        let mut detail = Map::new();
        if let Some(item) = &line.item_id {
            detail.insert("ItemRef".into(), json!({ "value": item }));
        }
        detail.insert("Qty".into(), number(decimal(&line.qty, "quantity")?));
        detail.insert("UnitPrice".into(), number(decimal(&line.rate, "rate")?));
        if let Some(class) = line.class_id.as_ref().or(draft.class_id.as_ref()) {
            detail.insert("ClassRef".into(), json!({ "value": class }));
        }
        if !purchase {
            detail.insert("TaxCodeRef".into(), json!({ "value": if line.taxable { "TAX" } else { "NON" } }));
        }
        let detail_type = if purchase { "ItemBasedExpenseLineDetail" } else { "SalesItemLineDetail" };
        if purchase && line.item_id.is_none() {
            return Err(invalid(format!("line {}: a purchase line needs a product, which carries its expense account", index + 1)));
        }
        let mut out = Map::new();
        out.insert("LineNum".into(), json!(index + 1));
        out.insert("DetailType".into(), json!(detail_type));
        out.insert("Amount".into(), money(amount));
        if let Some(description) = line.description.as_deref().filter(|d| !d.trim().is_empty()) {
            out.insert("Description".into(), json!(description));
        }
        out.insert(detail_type.into(), Value::Object(detail));
        lines.push(Value::Object(out));
    }

    let mut body = Map::new();
    if update {
        body.insert("sparse".into(), json!(true));
    }
    body.insert(
        if purchase { "VendorRef" } else { "CustomerRef" }.into(),
        json!({ "value": draft.contact_id }),
    );
    body.insert("TxnDate".into(), json!(date(&draft.txn_date, "date")?));
    if let Some(due) = draft.due_date.as_deref().filter(|d| !d.is_empty()) {
        body.insert("DueDate".into(), json!(date(due, "due date")?));
    }
    if let Some(number) = draft.doc_number.as_deref().filter(|n| !n.trim().is_empty()) {
        body.insert("DocNumber".into(), json!(number.trim()));
    }
    if let Some(class) = &draft.class_id {
        body.insert("ClassRef".into(), json!({ "value": class }));
    }
    if let Some(note) = &draft.private_note {
        body.insert("PrivateNote".into(), json!(note));
    }
    if let Some(memo) = &draft.customer_memo {
        // A purchase order's message to the vendor; sales documents' to the customer.
        if purchase {
            body.insert("Memo".into(), json!(memo));
        } else {
            body.insert("CustomerMemo".into(), json!({ "value": memo }));
        }
    }
    if purchase && draft.po_number.as_deref().is_some_and(|p| !p.trim().is_empty()) {
        return Err(invalid("a customer PO belongs on the sales document, not the purchase"));
    }
    if let Some(po) = draft.po_number.as_deref().filter(|p| !p.trim().is_empty()) {
        let definition = po_definition_id
            .ok_or_else(|| invalid("this book has no P.O. Number custom field; leave the PO blank or add the field in QuickBooks"))?;
        body.insert(
            "CustomField".into(),
            json!([{ "DefinitionId": definition, "Name": "P.O. Number", "Type": "StringType", "StringValue": po.trim() }]),
        );
    }
    body.insert("Line".into(), Value::Array(lines));
    Ok(Value::Object(body))
}

/// A new customer. `DisplayName` is the unique name QBO enforces and the one
/// the drain's duplicate guard queries by (§6.5).
pub fn customer_payload(display_name: &str, company_name: Option<&str>, email: Option<&str>) -> Result<Value, ComposeError> {
    let name = display_name.trim();
    if name.is_empty() {
        return Err(invalid("a customer needs a name"));
    }
    if name.contains(':') {
        return Err(invalid("QuickBooks does not allow ':' in a customer name"));
    }
    let mut body = Map::new();
    body.insert("DisplayName".into(), json!(name));
    if let Some(company) = company_name.map(str::trim).filter(|c| !c.is_empty()) {
        body.insert("CompanyName".into(), json!(company));
    }
    if let Some(email) = email.map(str::trim).filter(|e| !e.is_empty()) {
        if !email.contains('@') {
            return Err(invalid(format!("{email:?} is not an email address")));
        }
        body.insert("PrimaryEmailAddr".into(), json!({ "Address": email }));
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(qty: &str, rate: &str) -> DraftLine {
        DraftLine {
            item_id: Some("12".into()),
            description: Some("Rescue tube".into()),
            qty: qty.into(),
            rate: rate.into(),
            class_id: None,
            taxable: false,
        }
    }

    fn draft(lines: Vec<DraftLine>) -> DocumentDraft {
        DocumentDraft {
            doc_type: DocumentType::Invoice,
            contact_id: "31".into(),
            txn_date: "2026-11-02".into(),
            due_date: Some("2026-12-02".into()),
            doc_number: None,
            po_number: None,
            private_note: None,
            customer_memo: None,
            class_id: Some("1".into()),
            lines,
        }
    }

    #[test]
    fn line_amounts_round_half_away_from_zero() {
        assert_eq!(line_amount("3", "0.335").unwrap(), Money::from_minor(101)); // 1.005 -> 1.01
        assert_eq!(line_amount("20", "49.25").unwrap(), Money::from_minor(98_500));
        assert_eq!(line_amount("1,000", "0.1").unwrap(), Money::from_minor(10_000));
        assert!(line_amount("two", "1").is_err());
    }

    #[test]
    fn the_subtotal_is_the_sum_of_rounded_lines() {
        // Each line rounds up a half cent; summing first would lose both.
        let lines = vec![line("1", "0.005"), line("1", "0.005")];
        assert_eq!(subtotal(&lines).unwrap(), Money::from_minor(2));
    }

    #[test]
    fn a_draft_becomes_a_qbo_body_with_exact_numbers() {
        let body = to_payload(&draft(vec![line("20", "49.25")]), None, false).unwrap();
        assert_eq!(body["CustomerRef"]["value"], "31");
        assert_eq!(body["Line"][0]["Amount"].to_string(), "985");
        assert_eq!(body["Line"][0]["SalesItemLineDetail"]["UnitPrice"].to_string(), "49.25");
        assert_eq!(body["Line"][0]["SalesItemLineDetail"]["ClassRef"]["value"], "1", "header class flows to lines");
        assert_eq!(body["Line"][0]["SalesItemLineDetail"]["TaxCodeRef"]["value"], "NON");
        assert!(body.get("sparse").is_none());
        assert_eq!(to_payload(&draft(vec![line("1", "1")]), None, true).unwrap()["sparse"], true);
    }

    #[test]
    fn a_po_number_needs_the_books_custom_field() {
        let mut d = draft(vec![line("1", "1")]);
        d.po_number = Some("BH-99214".into());
        assert!(to_payload(&d, None, false).is_err(), "never drop a PO silently");
        let body = to_payload(&d, Some("3"), false).unwrap();
        assert_eq!(body["CustomField"][0]["DefinitionId"], "3");
        assert_eq!(body["CustomField"][0]["StringValue"], "BH-99214");
    }

    #[test]
    fn incomplete_drafts_are_refused_with_a_reason() {
        assert!(to_payload(&draft(vec![]), None, false).unwrap_err().to_string().contains("at least one line"));
        let mut blank = line("1", "1");
        blank.item_id = None;
        blank.description = None;
        assert!(to_payload(&draft(vec![blank]), None, false).unwrap_err().to_string().contains("line 1"));
        let mut deposit = draft(vec![line("1", "1")]);
        deposit.doc_type = DocumentType::Deposit;
        assert!(to_payload(&deposit, None, false).is_err());
        let mut bad_date = draft(vec![line("1", "1")]);
        bad_date.txn_date = "11/02/2026".into();
        assert!(to_payload(&bad_date, None, false).is_err());
    }

    #[test]
    fn a_purchase_order_goes_to_a_vendor_with_expense_lines() {
        let mut po = draft(vec![line("40", "18.4375")]);
        po.doc_type = DocumentType::PurchaseOrder;
        po.customer_memo = Some("Deliver to dock 2".into());
        let body = to_payload(&po, None, false).unwrap();
        assert_eq!(body["VendorRef"]["value"], "31");
        assert!(body.get("CustomerRef").is_none());
        assert_eq!(body["Line"][0]["DetailType"], "ItemBasedExpenseLineDetail");
        assert_eq!(body["Line"][0]["Amount"].to_string(), "737.5");
        assert!(body["Line"][0]["ItemBasedExpenseLineDetail"].get("TaxCodeRef").is_none());
        assert_eq!(body["Memo"], "Deliver to dock 2");

        let mut no_item = line("1", "5");
        no_item.item_id = None;
        po.lines = vec![no_item];
        assert!(to_payload(&po, None, false).unwrap_err().to_string().contains("needs a product"));
    }

    #[test]
    fn customers_are_checked_before_queueing() {
        assert_eq!(customer_payload(" Harbor Swim ", None, Some("ap@x.invalid")).unwrap()["DisplayName"], "Harbor Swim");
        assert!(customer_payload("", None, None).is_err());
        assert!(customer_payload("A:B", None, None).is_err());
        assert!(customer_payload("A", None, Some("nope")).is_err());
    }
}
