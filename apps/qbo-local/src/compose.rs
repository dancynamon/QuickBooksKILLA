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

/// A customer payment as typed: the amount received and, optionally, how it
/// is spread. With no applications it is spread oldest-first.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentDraft {
    pub customer_id: String,
    pub txn_date: String,
    /// Decimal as text, two places at most.
    pub amount: String,
    /// The cheque or transfer number.
    pub reference: Option<String>,
    pub private_note: Option<String>,
    pub applications: Option<Vec<PaymentApplication>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentApplication {
    pub invoice_id: String,
    pub amount: String,
}

/// A payment to a vendor against open bills, by cheque from a bank account
/// or on a credit card — which one follows from the account chosen.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BillPaymentDraft {
    pub vendor_id: String,
    pub txn_date: String,
    pub amount: String,
    pub account_id: String,
    /// The cheque number; QuickBooks numbers it if left blank.
    pub reference: Option<String>,
    pub private_note: Option<String>,
    pub applications: Option<Vec<PaymentApplication>>,
}

/// How a bill payment leaves the business.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PayFrom {
    Check,
    CreditCard,
}

impl PayFrom {
    /// From the QBO `AccountType` of the account chosen; anything else is not
    /// an account a bill can be paid from.
    pub fn for_account_type(account_type: &str) -> Option<Self> {
        match account_type {
            "Bank" => Some(PayFrom::Check),
            "Credit Card" => Some(PayFrom::CreditCard),
            _ => None,
        }
    }
}

/// An invoice a payment can go against, with what is still owed on it after
/// any payment already queued.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenInvoice {
    pub qbo_id: String,
    pub doc_number: Option<String>,
    pub txn_date: String,
    pub due_date: Option<String>,
    pub balance: Money,
}

/// How a payment lands: what goes on each invoice, and what is left over as
/// a credit on the customer.
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentPlan {
    pub total: Money,
    pub applied: Vec<(String, Money)>,
    pub unapplied: Money,
}

fn show(value: Money) -> String {
    Decimal::new(value.minor(), 2).to_string()
}

/// Money as typed, refused rather than rounded if it has more than cents.
pub fn cents(raw: &str, what: &str) -> Result<Money, ComposeError> {
    let value = decimal(raw, what)?;
    if value.normalize().scale() > 2 {
        return Err(invalid(format!("{what} {raw:?} has more than two decimal places")));
    }
    let minor = (value * Decimal::from(100)).trunc();
    i64::try_from(minor.mantissa())
        .ok()
        .filter(|_| minor.scale() == 0)
        .map(Money::from_minor)
        .ok_or_else(|| invalid(format!("{what} {raw:?} is too large")))
}

/// Spread `amount` across `open` in the order given — the caller sorts them
/// oldest due first — paying each in full until the money runs out.
pub fn apply_oldest_first(open: &[OpenInvoice], amount: Money) -> Vec<(String, Money)> {
    let mut left = amount.minor();
    let mut out = Vec::new();
    for invoice in open {
        if left <= 0 {
            break;
        }
        let take = left.min(invoice.balance.minor());
        if take > 0 {
            out.push((invoice.qbo_id.clone(), Money::from_minor(take)));
            left -= take;
        }
    }
    out
}

/// Check a draft against the customer's open invoices and decide where the
/// money goes. Nothing is applied past an invoice's balance, to an invoice
/// that is not this customer's open one, or beyond the amount received.
pub fn plan_payment(draft: &PaymentDraft, open: &[OpenInvoice]) -> Result<PaymentPlan, ComposeError> {
    if draft.customer_id.trim().is_empty() {
        return Err(invalid("choose a customer"));
    }
    plan(&draft.txn_date, &draft.amount, draft.applications.as_deref(), open, "invoice", "received")
}

/// The same rules on the vendor side, plus one: the whole amount must go on
/// bills. Money sent to a vendor against nothing is a prepayment, which is
/// entered in QuickBooks.
pub fn plan_bill_payment(draft: &BillPaymentDraft, open: &[OpenInvoice]) -> Result<PaymentPlan, ComposeError> {
    if draft.vendor_id.trim().is_empty() {
        return Err(invalid("choose a vendor"));
    }
    if draft.account_id.trim().is_empty() {
        return Err(invalid("choose the account it is paid from"));
    }
    let plan = plan(&draft.txn_date, &draft.amount, draft.applications.as_deref(), open, "bill", "paid")?;
    if plan.unapplied.minor() != 0 {
        return Err(invalid(format!(
            "{} is not on any bill; a payment to a vendor must go on bills in full",
            show(plan.unapplied)
        )));
    }
    Ok(plan)
}

fn plan(
    txn_date: &str,
    amount: &str,
    applications: Option<&[PaymentApplication]>,
    open: &[OpenInvoice],
    noun: &str,
    verb: &str,
) -> Result<PaymentPlan, ComposeError> {
    date(txn_date, "date")?;
    let total = cents(amount, "amount")?;
    if total.minor() <= 0 {
        return Err(invalid("a payment needs an amount above zero"));
    }
    let applied = match applications {
        None => apply_oldest_first(open, total),
        Some(applications) => {
            let mut out: Vec<(String, Money)> = Vec::new();
            for application in applications {
                let amount = cents(&application.amount, "applied amount")?;
                if amount.minor() == 0 {
                    continue;
                }
                if amount.minor() < 0 {
                    return Err(invalid("an applied amount cannot be negative"));
                }
                let open_doc = open
                    .iter()
                    .find(|i| i.qbo_id == application.invoice_id)
                    .ok_or_else(|| invalid(format!("{noun} {} is not open", application.invoice_id)))?;
                let name = open_doc.doc_number.as_deref().unwrap_or(&open_doc.qbo_id);
                if out.iter().any(|(id, _)| id == &open_doc.qbo_id) {
                    return Err(invalid(format!("{noun} {name} is listed twice")));
                }
                if amount.minor() > open_doc.balance.minor() {
                    return Err(invalid(format!(
                        "{noun} {name} has {} open; {} is more than that",
                        show(open_doc.balance),
                        show(amount)
                    )));
                }
                out.push((open_doc.qbo_id.clone(), amount));
            }
            out
        }
    };
    let applied_total: i64 = applied.iter().map(|(_, m)| m.minor()).sum();
    if applied_total > total.minor() {
        return Err(invalid(format!("{} is applied but only {} was {verb}", show(Money::from_minor(applied_total)), show(total))));
    }
    Ok(PaymentPlan { total, applied, unapplied: Money::from_minor(total.minor() - applied_total) })
}

/// The QBO body for a planned payment. No deposit account is named, so
/// QuickBooks puts it in Undeposited Funds, as its own screen does by
/// default; whatever is not applied stays on the customer as a credit.
pub fn payment_payload(draft: &PaymentDraft, plan: &PaymentPlan) -> Result<Value, ComposeError> {
    let mut body = Map::new();
    body.insert("CustomerRef".into(), json!({ "value": draft.customer_id }));
    body.insert("TxnDate".into(), json!(date(&draft.txn_date, "date")?));
    body.insert("TotalAmt".into(), money(plan.total));
    if let Some(reference) = draft.reference.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        body.insert("PaymentRefNum".into(), json!(reference));
    }
    if let Some(note) = draft.private_note.as_deref().filter(|n| !n.trim().is_empty()) {
        body.insert("PrivateNote".into(), json!(note));
    }
    let lines: Vec<Value> = plan
        .applied
        .iter()
        .map(|(id, amount)| json!({ "Amount": money(*amount), "LinkedTxn": [{ "TxnId": id, "TxnType": "Invoice" }] }))
        .collect();
    body.insert("Line".into(), Value::Array(lines));
    Ok(Value::Object(body))
}

/// The QBO body for a planned bill payment.
pub fn bill_payment_payload(draft: &BillPaymentDraft, plan: &PaymentPlan, pay_from: PayFrom) -> Result<Value, ComposeError> {
    let mut body = Map::new();
    body.insert("VendorRef".into(), json!({ "value": draft.vendor_id }));
    body.insert("TxnDate".into(), json!(date(&draft.txn_date, "date")?));
    body.insert("TotalAmt".into(), money(plan.total));
    match pay_from {
        PayFrom::Check => {
            body.insert("PayType".into(), json!("Check"));
            body.insert("CheckPayment".into(), json!({ "BankAccountRef": { "value": draft.account_id } }));
        }
        PayFrom::CreditCard => {
            body.insert("PayType".into(), json!("CreditCard"));
            body.insert("CreditCardPayment".into(), json!({ "CCAccountRef": { "value": draft.account_id } }));
        }
    }
    if let Some(reference) = draft.reference.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        body.insert("DocNumber".into(), json!(reference));
    }
    if let Some(note) = draft.private_note.as_deref().filter(|n| !n.trim().is_empty()) {
        body.insert("PrivateNote".into(), json!(note));
    }
    let lines: Vec<Value> = plan
        .applied
        .iter()
        .map(|(id, amount)| json!({ "Amount": money(*amount), "LinkedTxn": [{ "TxnId": id, "TxnType": "Bill" }] }))
        .collect();
    body.insert("Line".into(), Value::Array(lines));
    Ok(Value::Object(body))
}

/// Money spent straight from a bank account or card — a receipt, not a bill —
/// charged to expense accounts line by line.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExpenseDraft {
    /// The vendor paid, when there is one on file.
    pub payee_id: Option<String>,
    pub txn_date: String,
    /// The bank or credit card account it came out of.
    pub account_id: String,
    /// A cheque number makes it a cheque; without one a bank expense is cash.
    pub reference: Option<String>,
    pub private_note: Option<String>,
    pub lines: Vec<ExpenseLine>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExpenseLine {
    pub account_id: String,
    pub description: Option<String>,
    pub amount: String,
    pub class_id: Option<String>,
}

/// The QBO `Purchase` body for an expense, and its total.
pub fn expense_payload(draft: &ExpenseDraft, pay_from: PayFrom) -> Result<(Value, Money), ComposeError> {
    if draft.lines.is_empty() {
        return Err(invalid("add at least one line"));
    }
    let reference = draft.reference.as_deref().map(str::trim).filter(|r| !r.is_empty());
    let payment_type = match (pay_from, reference) {
        (PayFrom::CreditCard, _) => "CreditCard",
        (PayFrom::Check, Some(_)) => "Check",
        (PayFrom::Check, None) => "Cash",
    };
    let mut lines = Vec::new();
    let mut amounts = Vec::new();
    for (index, line) in draft.lines.iter().enumerate() {
        if line.account_id.trim().is_empty() {
            return Err(invalid(format!("line {} needs an expense account", index + 1)));
        }
        let amount = cents(&line.amount, &format!("line {} amount", index + 1))?;
        if amount.minor() <= 0 {
            return Err(invalid(format!("line {} needs an amount above zero", index + 1)));
        }
        amounts.push(amount);
        let mut detail = Map::new();
        detail.insert("AccountRef".into(), json!({ "value": line.account_id }));
        if let Some(class) = &line.class_id {
            detail.insert("ClassRef".into(), json!({ "value": class }));
        }
        detail.insert("BillableStatus".into(), json!("NotBillable"));
        let mut out = Map::new();
        out.insert("LineNum".into(), json!(index + 1));
        out.insert("DetailType".into(), json!("AccountBasedExpenseLineDetail"));
        out.insert("Amount".into(), money(amount));
        if let Some(description) = line.description.as_deref().filter(|d| !d.trim().is_empty()) {
            out.insert("Description".into(), json!(description));
        }
        out.insert("AccountBasedExpenseLineDetail".into(), Value::Object(detail));
        lines.push(Value::Object(out));
    }
    let total = Money::checked_sum(amounts).map_err(|e| invalid(e.to_string()))?;
    let mut body = Map::new();
    body.insert("PaymentType".into(), json!(payment_type));
    body.insert("AccountRef".into(), json!({ "value": draft.account_id }));
    body.insert("TxnDate".into(), json!(date(&draft.txn_date, "date")?));
    if let Some(payee) = draft.payee_id.as_deref().filter(|p| !p.trim().is_empty()) {
        body.insert("EntityRef".into(), json!({ "value": payee, "type": "Vendor" }));
    }
    if let Some(reference) = reference {
        body.insert("DocNumber".into(), json!(reference));
    }
    if let Some(note) = draft.private_note.as_deref().filter(|n| !n.trim().is_empty()) {
        body.insert("PrivateNote".into(), json!(note));
    }
    body.insert("Line".into(), Value::Array(lines));
    Ok((Value::Object(body), total))
}

/// A bank deposit of payments waiting in Undeposited Funds.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DepositDraft {
    pub account_id: String,
    pub txn_date: String,
    /// Payments and sales receipts to put on the slip, by QBO id.
    pub payment_ids: Vec<String>,
    pub private_note: Option<String>,
}

/// One payment as it goes on a deposit: its id, QBO type and amount.
#[derive(Clone, Debug, PartialEq)]
pub struct DepositItem {
    pub qbo_id: String,
    pub txn_type: String,
    pub amount: Money,
}

/// The QBO body for a deposit. Each line links the payment it banks, so
/// QuickBooks moves it out of Undeposited Funds rather than counting it twice.
pub fn deposit_payload(draft: &DepositDraft, items: &[DepositItem]) -> Result<Value, ComposeError> {
    if draft.account_id.trim().is_empty() {
        return Err(invalid("choose the bank account"));
    }
    if items.is_empty() {
        return Err(invalid("tick at least one payment"));
    }
    let mut body = Map::new();
    body.insert("DepositToAccountRef".into(), json!({ "value": draft.account_id }));
    body.insert("TxnDate".into(), json!(date(&draft.txn_date, "date")?));
    if let Some(note) = draft.private_note.as_deref().filter(|n| !n.trim().is_empty()) {
        body.insert("PrivateNote".into(), json!(note));
    }
    let total = Money::checked_sum(items.iter().map(|i| i.amount)).map_err(|e| invalid(e.to_string()))?;
    body.insert("TotalAmt".into(), money(total));
    let lines: Vec<Value> = items
        .iter()
        .map(|i| json!({ "Amount": money(i.amount), "LinkedTxn": [{ "TxnId": i.qbo_id, "TxnType": i.txn_type, "TxnLineId": "0" }] }))
        .collect();
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

    fn open(id: &str, balance: i64) -> OpenInvoice {
        OpenInvoice { qbo_id: id.into(), doc_number: Some(format!("10{id}")), txn_date: "2026-09-01".into(), due_date: None, balance: Money::from_minor(balance) }
    }

    fn payment(amount: &str, applications: Option<Vec<(&str, &str)>>) -> PaymentDraft {
        PaymentDraft {
            customer_id: "58".into(),
            txn_date: "2026-11-02".into(),
            amount: amount.into(),
            reference: Some(" 4417 ".into()),
            private_note: None,
            applications: applications
                .map(|a| a.into_iter().map(|(id, amt)| PaymentApplication { invoice_id: id.into(), amount: amt.into() }).collect()),
        }
    }

    #[test]
    fn a_cheque_pays_the_oldest_invoices_first_and_leaves_the_rest_as_credit() {
        let invoices = [open("1", 50_000), open("2", 30_000), open("3", 20_000)];
        let plan = plan_payment(&payment("650.00", None), &invoices).unwrap();
        assert_eq!(plan.applied, vec![("1".into(), Money::from_minor(50_000)), ("2".into(), Money::from_minor(15_000))]);
        assert_eq!(plan.unapplied, Money::from_minor(0));

        let over = plan_payment(&payment("1,100", None), &invoices).unwrap();
        assert_eq!(over.applied.len(), 3);
        assert_eq!(over.unapplied, Money::from_minor(10_000), "an overpayment stays on the customer");

        let body = payment_payload(&payment("650.00", None), &plan).unwrap();
        assert_eq!(body["TotalAmt"], json!(650));
        assert_eq!(body["PaymentRefNum"], "4417");
        assert_eq!(body["Line"][1]["Amount"], json!(150));
        assert_eq!(body["Line"][1]["LinkedTxn"][0], json!({ "TxnId": "2", "TxnType": "Invoice" }));
        assert!(body.get("DepositToAccountRef").is_none(), "QuickBooks defaults it to Undeposited Funds");
    }

    #[test]
    fn a_chosen_spread_is_held_to_each_balance_and_to_the_amount_received() {
        let invoices = [open("1", 50_000), open("2", 30_000)];
        let plan = plan_payment(&payment("300", Some(vec![("2", "300.00"), ("1", "0")])), &invoices).unwrap();
        assert_eq!(plan.applied, vec![("2".into(), Money::from_minor(30_000))]);

        let refuse = |amount: &str, apps: Vec<(&str, &str)>| plan_payment(&payment(amount, Some(apps)), &invoices).unwrap_err().to_string();
        assert!(refuse("400", vec![("2", "300.01")]).contains("has 300.00 open"));
        assert!(refuse("100", vec![("1", "60"), ("2", "60")]).contains("only 100.00 was received"));
        assert!(refuse("100", vec![("9", "10")]).contains("is not open"));
        assert!(refuse("100", vec![("1", "10"), ("1", "10")]).contains("twice"));
        assert!(refuse("100", vec![("1", "10.005")]).contains("two decimal places"));
        assert!(plan_payment(&payment("0", None), &invoices).unwrap_err().to_string().contains("above zero"));
    }

    #[test]
    fn a_bill_payment_goes_on_bills_in_full_from_the_account_chosen() {
        let bills = [open("7", 40_000), open("8", 10_000)];
        let draft = BillPaymentDraft {
            vendor_id: "61".into(),
            txn_date: "2026-11-02".into(),
            amount: "450".into(),
            account_id: "90".into(),
            reference: Some("1207".into()),
            private_note: None,
            applications: None,
        };
        let plan = plan_bill_payment(&draft, &bills).unwrap();
        assert_eq!(plan.applied, vec![("7".into(), Money::from_minor(40_000)), ("8".into(), Money::from_minor(5_000))]);
        let body = bill_payment_payload(&draft, &plan, PayFrom::Check).unwrap();
        assert_eq!(body["PayType"], "Check");
        assert_eq!(body["CheckPayment"]["BankAccountRef"]["value"], "90");
        assert_eq!(body["DocNumber"], "1207");
        assert_eq!(body["Line"][0]["LinkedTxn"][0]["TxnType"], "Bill");
        let card = bill_payment_payload(&draft, &plan, PayFrom::CreditCard).unwrap();
        assert_eq!(card["CreditCardPayment"]["CCAccountRef"]["value"], "90");

        let over = BillPaymentDraft { amount: "600".into(), ..draft.clone() };
        assert!(plan_bill_payment(&over, &bills).unwrap_err().to_string().contains("100.00 is not on any bill"));
        assert_eq!(PayFrom::for_account_type("Bank"), Some(PayFrom::Check));
        assert_eq!(PayFrom::for_account_type("Accounts Payable"), None);
    }

    #[test]
    fn an_expense_is_cash_cheque_or_card_by_how_it_was_paid() {
        let draft = ExpenseDraft {
            payee_id: Some("61".into()),
            txn_date: "2026-11-02".into(),
            account_id: "90".into(),
            reference: None,
            private_note: None,
            lines: vec![
                ExpenseLine { account_id: "95".into(), description: Some("LTL to Garwood".into()), amount: "412.50".into(), class_id: Some("2".into()) },
                ExpenseLine { account_id: "96".into(), description: None, amount: "38".into(), class_id: None },
            ],
        };
        let (body, total) = expense_payload(&draft, PayFrom::Check).unwrap();
        assert_eq!(total, Money::from_minor(45_050));
        assert_eq!(body["PaymentType"], "Cash");
        assert_eq!(body["EntityRef"], json!({ "value": "61", "type": "Vendor" }));
        assert_eq!(body["Line"][0]["AccountBasedExpenseLineDetail"]["AccountRef"]["value"], "95");
        assert_eq!(body["Line"][0]["AccountBasedExpenseLineDetail"]["ClassRef"]["value"], "2");
        let cheque = ExpenseDraft { reference: Some("1208".into()), ..draft.clone() };
        assert_eq!(expense_payload(&cheque, PayFrom::Check).unwrap().0["PaymentType"], "Check");
        assert_eq!(expense_payload(&draft, PayFrom::CreditCard).unwrap().0["PaymentType"], "CreditCard");

        let mut bad = draft.clone();
        bad.lines[1].account_id = String::new();
        assert!(expense_payload(&bad, PayFrom::Check).unwrap_err().to_string().contains("line 2 needs an expense account"));
    }
}
