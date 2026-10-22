//! The UI's write commands, and the "not yet in QuickBooks" view of queued
//! creates. `DESIGN.md` §6.6, §6.7.
//!
//! Every command here ends in the outbox: none talks to QBO. Saving is the
//! local commit, measured in milliseconds; the background drain does the rest
//! and the UI shows where each write stands until it lands.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{parse, realm, to_json, ApiError, DocumentDto, LineDto};
use crate::compose::{self, BillPaymentDraft, DepositDraft, DepositItem, ExpenseDraft, DocumentDraft, DraftLine, OpenInvoice, PayFrom, PaymentDraft, PaymentPlan};
use crate::domain::{ContactType, DocumentType, EntityType, RealmId};
use crate::outbox::{Operation, OutboxRecord, OutboxState};
use crate::store::Store;
use crate::writes::{self, Resolution, WriteError};

/// Commands that queue writes. Each refuses with 403 while the company's
/// writes are off.
pub const WRITE_COMMANDS: &[&str] = &[
    "customers",
    "vendors",
    "documentDraft",
    "saveDocument",
    "createCustomer",
    "outbox",
    "resolveWrite",
    "openInvoices",
    "paymentPreview",
    "receivePayment",
    "paymentAccounts",
    "openBills",
    "billPaymentPreview",
    "payBills",
    "undeposited",
    "makeDeposit",
    "expenseAccounts",
    "saveExpense",
];

impl From<WriteError> for ApiError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::WritesOff => ApiError::Forbidden(error.to_string()),
            WriteError::NoSuchRecord(_) => ApiError::NotFound,
            WriteError::Invalid(message) => ApiError::BadArguments(message),
            WriteError::Outbox(e) => ApiError::BadArguments(e.to_string()),
            WriteError::Store(e) => ApiError::Store(e),
        }
    }
}

const OPEN: &[OutboxState] = &[OutboxState::Pending, OutboxState::InFlight, OutboxState::Conflicted, OutboxState::Rejected];

/// How a queued write is described to a person.
fn write_status(state: OutboxState) -> &'static str {
    match state {
        OutboxState::Pending | OutboxState::InFlight => "Not yet in QuickBooks",
        OutboxState::Conflicted => "Changed in QuickBooks; needs a look",
        OutboxState::Rejected => "Refused by QuickBooks",
        OutboxState::Applied => "In QuickBooks",
        OutboxState::Discarded => "Discarded",
    }
}

/// An amount QBO will read from the payload, as minor units. The payload's
/// amounts were written by `compose` from `Money`, so they have two places.
fn minor_of(value: &Value) -> i64 {
    Decimal::from_str(&value.to_string())
        .ok()
        .and_then(|d| (d * Decimal::from(100)).round().to_string().parse().ok())
        .unwrap_or(0)
}

fn text(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(|v| v.as_str()).map(str::to_string)
}

/// Name for a customer reference that may be a local id still in the outbox.
fn customer_name(store: &Store, realm: &RealmId, id: &str) -> Result<Option<String>, ApiError> {
    if let Some(name) = store.contact_name(realm, ContactType::Customer, id)? {
        return Ok(Some(name));
    }
    Ok(store
        .outbox_records(realm, OPEN)?
        .into_iter()
        .find(|r| r.entity_type == EntityType::Customer && r.local_entity_id == id)
        .and_then(|r| text(&r.payload_json, "/DisplayName")))
}

/// A queued create, shaped like a mirrored document so a register can list it
/// next to the real ones, marked as what it is.
pub(super) fn pending_document(store: &Store, realm: &RealmId, record: &OutboxRecord) -> Result<Option<(DocumentDto, Vec<LineDto>)>, ApiError> {
    let Some(doc_type) = record.entity_type.as_document() else {
        return Ok(None);
    };
    let payload = &record.payload_json;
    let purchase = crate::compose::is_purchase(doc_type);
    let contact_type = if purchase { ContactType::Vendor } else { ContactType::Customer };
    let contact_id = text(payload, if purchase { "/VendorRef/value" } else { "/CustomerRef/value" });
    let contact_name = match &contact_id {
        Some(id) if purchase => store.contact_name(realm, ContactType::Vendor, id)?,
        Some(id) => customer_name(store, realm, id)?,
        None => None,
    };
    if doc_type == DocumentType::Purchase {
        return Ok(Some(pending_expense(store, realm, record)?));
    }
    if doc_type == DocumentType::Deposit {
        return Ok(Some(pending_deposit(store, realm, record)?));
    }
    if matches!(doc_type, DocumentType::Payment | DocumentType::BillPayment) {
        return Ok(Some(pending_payment(store, realm, record, contact_id, contact_name)?));
    }
    let detail_type = if purchase { "ItemBasedExpenseLineDetail" } else { "SalesItemLineDetail" };
    let lines: Vec<LineDto> = payload
        .get("Line")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, line)| LineDto {
            line_no: i as i64 + 1,
            item_id: text(line, &format!("/{detail_type}/ItemRef/value")),
            description: text(line, "/Description"),
            qty: line.pointer(&format!("/{detail_type}/Qty")).map(|v| v.to_string()),
            unit_price: line.pointer(&format!("/{detail_type}/UnitPrice")).map(|v| v.to_string()),
            amount_minor: line.get("Amount").map(minor_of).unwrap_or(0),
            class_id: text(line, &format!("/{detail_type}/ClassRef/value")),
            taxable: text(line, &format!("/{detail_type}/TaxCodeRef/value")).as_deref() == Some("TAX"),
        })
        .collect();
    let document = DocumentDto {
        qbo_id: record.local_entity_id.clone(),
        doc_type,
        doc_number: text(payload, "/DocNumber"),
        txn_date: text(payload, "/TxnDate").unwrap_or_default(),
        due_date: text(payload, "/DueDate"),
        contact_id,
        contact_type: Some(contact_type),
        contact_name,
        class_id: text(payload, "/ClassRef/value"),
        // Before tax: QuickBooks adds tax when it accepts the document.
        total_minor: lines.iter().map(|l| l.amount_minor).sum(),
        balance_minor: None,
        status: Some(write_status(record.state).to_string()),
        po_number: text(payload, "/CustomField/0/StringValue"),
        private_note: text(payload, "/PrivateNote"),
        customer_memo: text(payload, "/CustomerMemo/value").or_else(|| text(payload, "/Memo")),
        pending_write: Some(record.state.as_str().to_string()),
    };
    Ok(Some((document, lines)))
}

/// A queued payment, received or made: one line per invoice or bill it pays,
/// and its full amount as the total, unapplied credit included.
fn pending_payment(
    store: &Store,
    realm: &RealmId,
    record: &OutboxRecord,
    contact_id: Option<String>,
    contact_name: Option<String>,
) -> Result<(DocumentDto, Vec<LineDto>), ApiError> {
    let payload = &record.payload_json;
    let paying_bills = record.entity_type == EntityType::BillPayment;
    let noun = if paying_bills { "bill" } else { "invoice" };
    let mut lines = Vec::new();
    for (i, line) in payload.get("Line").and_then(|l| l.as_array()).into_iter().flatten().enumerate() {
        let invoice = text(line, "/LinkedTxn/0/TxnId");
        let number = match &invoice {
            Some(id) => store.get_document(realm, id)?.and_then(|d| d.doc_number).unwrap_or_else(|| id.clone()),
            None => String::new(),
        };
        lines.push(LineDto {
            line_no: i as i64 + 1,
            item_id: None,
            description: Some(format!("Applied to {noun} {number}")),
            qty: None,
            unit_price: None,
            amount_minor: line.get("Amount").map(minor_of).unwrap_or(0),
            class_id: None,
            taxable: false,
        });
    }
    let total = payload.get("TotalAmt").map(minor_of).unwrap_or(0);
    let applied: i64 = lines.iter().map(|l| l.amount_minor).sum();
    let document = DocumentDto {
        qbo_id: record.local_entity_id.clone(),
        doc_type: if paying_bills { DocumentType::BillPayment } else { DocumentType::Payment },
        doc_number: text(payload, "/PaymentRefNum").or_else(|| text(payload, "/DocNumber")),
        txn_date: text(payload, "/TxnDate").unwrap_or_default(),
        due_date: None,
        contact_id,
        contact_type: Some(if paying_bills { ContactType::Vendor } else { ContactType::Customer }),
        contact_name,
        class_id: None,
        total_minor: total,
        // What is left as a credit on the customer.
        balance_minor: Some(total - applied).filter(|b| *b != 0),
        status: Some(write_status(record.state).to_string()),
        po_number: None,
        private_note: text(payload, "/PrivateNote"),
        customer_memo: None,
        pending_write: Some(record.state.as_str().to_string()),
    };
    Ok((document, lines))
}

/// A queued expense: its lines named by the account they are charged to.
fn pending_expense(store: &Store, realm: &RealmId, record: &OutboxRecord) -> Result<(DocumentDto, Vec<LineDto>), ApiError> {
    let payload = &record.payload_json;
    let accounts = store.expense_accounts(realm)?;
    let lines: Vec<LineDto> = payload
        .get("Line")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, line)| {
            let account = text(line, "/AccountBasedExpenseLineDetail/AccountRef/value").unwrap_or_default();
            let account_name = accounts.iter().find(|a| a.0 == account).map(|a| a.1.clone()).unwrap_or(account);
            LineDto {
                line_no: i as i64 + 1,
                item_id: None,
                description: Some(match text(line, "/Description") {
                    Some(d) => format!("{account_name}: {d}"),
                    None => account_name,
                }),
                qty: None,
                unit_price: None,
                amount_minor: line.get("Amount").map(minor_of).unwrap_or(0),
                class_id: text(line, "/AccountBasedExpenseLineDetail/ClassRef/value"),
                taxable: false,
            }
        })
        .collect();
    let contact_id = text(payload, "/EntityRef/value");
    let contact_name = match &contact_id {
        Some(id) => store.contact_name(realm, ContactType::Vendor, id)?,
        None => None,
    };
    let document = DocumentDto {
        qbo_id: record.local_entity_id.clone(),
        doc_type: DocumentType::Purchase,
        doc_number: text(payload, "/DocNumber"),
        txn_date: text(payload, "/TxnDate").unwrap_or_default(),
        due_date: None,
        contact_type: contact_id.as_ref().map(|_| ContactType::Vendor),
        contact_id,
        contact_name,
        class_id: None,
        total_minor: lines.iter().map(|l| l.amount_minor).sum(),
        balance_minor: None,
        status: Some(write_status(record.state).to_string()),
        po_number: None,
        private_note: text(payload, "/PrivateNote"),
        customer_memo: None,
        pending_write: Some(record.state.as_str().to_string()),
    };
    Ok((document, lines))
}

/// A queued deposit: one line per payment it banks.
fn pending_deposit(store: &Store, realm: &RealmId, record: &OutboxRecord) -> Result<(DocumentDto, Vec<LineDto>), ApiError> {
    let payload = &record.payload_json;
    let mut lines = Vec::new();
    for (i, line) in payload.get("Line").and_then(|l| l.as_array()).into_iter().flatten().enumerate() {
        let from = match text(line, "/LinkedTxn/0/TxnId") {
            Some(id) => store.get_document(realm, &id)?.and_then(|d| d.contact_name).unwrap_or(id),
            None => String::new(),
        };
        lines.push(LineDto {
            line_no: i as i64 + 1,
            item_id: None,
            description: Some(format!("Payment from {from}")),
            qty: None,
            unit_price: None,
            amount_minor: line.get("Amount").map(minor_of).unwrap_or(0),
            class_id: None,
            taxable: false,
        });
    }
    let document = DocumentDto {
        qbo_id: record.local_entity_id.clone(),
        doc_type: DocumentType::Deposit,
        doc_number: None,
        txn_date: text(payload, "/TxnDate").unwrap_or_default(),
        due_date: None,
        contact_id: None,
        contact_type: None,
        contact_name: None,
        class_id: None,
        total_minor: payload.get("TotalAmt").map(minor_of).unwrap_or(0),
        balance_minor: None,
        status: Some(write_status(record.state).to_string()),
        po_number: None,
        private_note: text(payload, "/PrivateNote"),
        customer_memo: None,
        pending_write: Some(record.state.as_str().to_string()),
    };
    Ok((document, lines))
}

/// Payments already on a deposit that has not landed yet.
fn queued_deposits(store: &Store, realm: &RealmId) -> Result<std::collections::HashSet<String>, ApiError> {
    Ok(store
        .outbox_records(realm, OPEN)?
        .into_iter()
        .filter(|r| r.entity_type == EntityType::Deposit && r.operation == Operation::Create)
        .flat_map(|r| {
            r.payload_json
                .get("Line")
                .and_then(|l| l.as_array())
                .into_iter()
                .flatten()
                .filter_map(|line| text(line, "/LinkedTxn/0/TxnId"))
                .collect::<Vec<_>>()
        })
        .collect())
}

/// Amounts queued payments will take off each invoice once they land.
fn queued_applications(store: &Store, realm: &RealmId, payment: EntityType) -> Result<std::collections::HashMap<String, i64>, ApiError> {
    let mut out = std::collections::HashMap::new();
    for record in store.outbox_records(realm, OPEN)? {
        if record.entity_type != payment || record.operation != Operation::Create {
            continue;
        }
        for line in record.payload_json.get("Line").and_then(|l| l.as_array()).into_iter().flatten() {
            if let Some(id) = text(line, "/LinkedTxn/0/TxnId") {
                *out.entry(id).or_insert(0) += line.get("Amount").map(minor_of).unwrap_or(0);
            }
        }
    }
    Ok(out)
}

/// A customer's invoices with money still owed, oldest due first, net of
/// payments already queued against them — so the same cheque cannot be
/// applied twice while the first is on its way.
fn open_invoices(store: &Store, realm: &RealmId, customer_id: &str) -> Result<(Vec<OpenInvoice>, Vec<i64>), ApiError> {
    open_documents(store, realm, ContactType::Customer, customer_id)
}

/// Invoices for a customer, bills for a vendor: open, oldest due first, net
/// of queued payments.
fn open_documents(
    store: &Store,
    realm: &RealmId,
    contact_type: ContactType,
    contact_id: &str,
) -> Result<(Vec<OpenInvoice>, Vec<i64>), ApiError> {
    let (doc_type, payment) = match contact_type {
        ContactType::Vendor => (DocumentType::Bill, EntityType::BillPayment),
        _ => (DocumentType::Invoice, EntityType::Payment),
    };
    let queued = queued_applications(store, realm, payment)?;
    let mut rows: Vec<_> = store
        .documents_for_contact(realm, contact_type, contact_id, 100_000)?
        .into_iter()
        .filter(|d| d.doc_type == doc_type && d.balance.is_some_and(|b| b.minor() > 0))
        .collect();
    rows.sort_by(|a, b| {
        let due = |d: &crate::store::DocumentRow| d.due_date.clone().unwrap_or_else(|| d.txn_date.clone());
        due(a).cmp(&due(b)).then(a.txn_date.cmp(&b.txn_date)).then(a.doc_number.cmp(&b.doc_number))
    });
    let mut open = Vec::new();
    let mut held = Vec::new();
    for row in rows {
        let on_the_way = queued.get(&row.qbo_id).copied().unwrap_or(0);
        let balance = row.balance.map(|b| b.minor()).unwrap_or(0) - on_the_way;
        if balance <= 0 {
            continue;
        }
        held.push(on_the_way);
        open.push(OpenInvoice {
            qbo_id: row.qbo_id,
            doc_number: row.doc_number,
            txn_date: row.txn_date,
            due_date: row.due_date,
            balance: ledger_core::Money::from_minor(balance),
        });
    }
    Ok((open, held))
}

fn plan_json(plan: &PaymentPlan, open: &[OpenInvoice]) -> Value {
    serde_json::json!({
        "totalMinor": plan.total.minor(),
        "unappliedMinor": plan.unapplied.minor(),
        "applied": plan.applied.iter().map(|(id, amount)| serde_json::json!({
            "invoiceId": id,
            "docNumber": open.iter().find(|i| &i.qbo_id == id).and_then(|i| i.doc_number.clone()),
            "amountMinor": amount.minor(),
        })).collect::<Vec<_>>(),
    })
}

/// Queued creates of one document type, newest first, for a register.
pub(super) fn pending_documents(store: &Store, realm: &RealmId, doc_type: DocumentType) -> Result<Vec<DocumentDto>, ApiError> {
    let mut out = Vec::new();
    for record in store.outbox_records(realm, OPEN)?.iter().rev() {
        if record.operation == Operation::Create && record.entity_type == doc_type.as_entity() {
            if let Some((doc, _)) = pending_document(store, realm, record)? {
                out.push(doc);
            }
        }
    }
    Ok(out)
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriteDto {
    pub id: String,
    pub entity_type: String,
    pub operation: String,
    /// The record the write is about: a QBO id, or a local id until QBO assigns one.
    pub entity_id: String,
    /// What a person would call it: a document number, a customer name.
    pub label: String,
    pub state: String,
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// The write this one waits for, when that one has not landed.
    pub blocked_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn write_dto(record: &OutboxRecord, blocked_by: Option<String>) -> WriteDto {
    let p = &record.payload_json;
    let label = text(p, "/DisplayName")
        .or_else(|| text(p, "/DocNumber").map(|n| format!("{} {n}", record.entity_type.as_str())))
        .unwrap_or_else(|| format!("{} {}", record.entity_type.as_str(), if record.local_entity_id.starts_with("local:") { "(new)" } else { &record.local_entity_id }));
    WriteDto {
        id: record.id.to_string(),
        entity_type: record.entity_type.as_str().to_string(),
        operation: record.operation.as_str().to_string(),
        entity_id: record.local_entity_id.clone(),
        label,
        state: record.state.as_str().to_string(),
        status: write_status(record.state).to_string(),
        attempts: record.attempts,
        last_error: record.last_error.clone(),
        blocked_by,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

/// Open writes about one document, for its banner.
pub(super) fn open_writes(store: &Store, realm: &RealmId, entity_id: &str) -> Result<Vec<WriteDto>, ApiError> {
    Ok(store.open_writes_for(realm, entity_id)?.iter().map(|r| write_dto(r, None)).collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RealmOnly {
    realm: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftArgs {
    realm: String,
    qbo_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveArgs {
    realm: String,
    /// Present to edit an existing document; absent to create one.
    qbo_id: Option<String>,
    draft: DocumentDraft,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerArgs {
    realm: String,
    display_name: String,
    company_name: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInvoicesArgs {
    realm: String,
    customer_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaymentArgs {
    realm: String,
    draft: PaymentDraft,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenBillsArgs {
    realm: String,
    vendor_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillPaymentArgs {
    realm: String,
    draft: BillPaymentDraft,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveArgs {
    realm: String,
    id: String,
    action: String,
}

pub(super) fn dispatch(store: &Store, command: &str, args: Value, now: DateTime<Utc>) -> Option<Result<Value, ApiError>> {
    let result = match command {
        "customers" => customers(store, args),
        "vendors" => vendors(store, args),
        "documentDraft" => document_draft(store, args),
        "saveDocument" => save_document(store, args, now),
        "createCustomer" => create_customer(store, args, now),
        "outbox" => outbox(store, args),
        "resolveWrite" => resolve_write(store, args, now),
        "openInvoices" => open_invoices_command(store, args),
        "paymentPreview" => payment_preview(store, args),
        "receivePayment" => receive_payment(store, args, now),
        "paymentAccounts" => payment_accounts(store, args),
        "openBills" => open_bills_command(store, args),
        "billPaymentPreview" => bill_payment_preview(store, args),
        "payBills" => pay_bills(store, args, now),
        "undeposited" => undeposited(store, args),
        "makeDeposit" => make_deposit(store, args, now),
        "expenseAccounts" => expense_accounts(store, args),
        "saveExpense" => save_expense(store, args, now),
        _ => return None,
    };
    Some(result)
}

fn customers(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    let realm = realm(&args.realm)?;
    let mut out: Vec<Value> = store
        .contacts(&realm, ContactType::Customer)?
        .into_iter()
        .map(|(id, name, company)| serde_json::json!({ "qboId": id, "displayName": name, "companyName": company, "pending": false }))
        .collect();
    // Customers created moments ago are usable before QBO has them (§6.4).
    for record in store.outbox_records(&realm, OPEN)? {
        if record.entity_type == EntityType::Customer && record.operation == Operation::Create {
            out.push(serde_json::json!({
                "qboId": record.local_entity_id,
                "displayName": text(&record.payload_json, "/DisplayName"),
                "companyName": text(&record.payload_json, "/CompanyName"),
                "pending": true,
            }));
        }
    }
    to_json(out)
}

fn vendors(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    to_json(
        store
            .contacts(&realm(&args.realm)?, ContactType::Vendor)?
            .into_iter()
            .map(|(id, name, company)| serde_json::json!({ "qboId": id, "displayName": name, "companyName": company, "pending": false }))
            .collect::<Vec<_>>(),
    )
}

/// An existing document as an editable draft.
fn document_draft(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: DraftArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let doc = store.get_document(&realm, &args.qbo_id)?.ok_or(ApiError::NotFound)?;
    if !compose::EDITABLE.contains(&doc.doc_type) {
        return Err(ApiError::BadArguments(format!("{} is not editable here yet", doc.doc_type.as_str())));
    }
    let lines: Vec<DraftLine> = store
        .document_lines(&realm, &args.qbo_id)?
        .into_iter()
        .map(|l| DraftLine {
            item_id: l.item_id,
            description: l.description,
            qty: l.qty.unwrap_or_else(|| "1".into()),
            // A line with no rate was entered as an amount; keep the amount.
            rate: l.unit_price.unwrap_or_else(|| Decimal::new(l.amount.minor(), 2).to_string()),
            class_id: l.class_id,
            taxable: l.is_taxable,
        })
        .collect();
    let draft = serde_json::json!({
        "docType": doc.doc_type,
        "contactId": doc.contact_id,
        "txnDate": doc.txn_date,
        "dueDate": doc.due_date,
        "docNumber": doc.doc_number,
        "poNumber": doc.po_number,
        "privateNote": doc.private_note,
        "customerMemo": doc.customer_memo,
        "classId": doc.class_id,
        "lines": lines.iter().map(|l| serde_json::json!({
            "itemId": l.item_id, "description": l.description, "qty": l.qty, "rate": l.rate,
            "classId": l.class_id, "taxable": l.taxable,
        })).collect::<Vec<_>>(),
    });
    to_json(serde_json::json!({ "draft": draft, "contactName": doc.contact_name, "openWrites": store.open_writes_for(&realm, &args.qbo_id)?.len() }))
}

fn save_document(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: SaveArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let po = store.po_field_definition(&realm)?;
    let entity_type = args.draft.doc_type.as_entity();
    let record = match &args.qbo_id {
        None => {
            let payload = compose::to_payload(&args.draft, po.as_deref(), false).map_err(|e| ApiError::BadArguments(e.to_string()))?;
            writes::queue_create(store, &realm, entity_type, payload, now)?
        }
        Some(id) if id.starts_with(crate::worker::LOCAL_ID_PREFIX) => {
            return Err(ApiError::BadArguments(
                "this document is still on its way to QuickBooks; edit it once it has landed".into(),
            ))
        }
        Some(id) => {
            let existing = store.get_document(&realm, id)?.ok_or(ApiError::NotFound)?;
            if existing.doc_type != args.draft.doc_type {
                return Err(ApiError::BadArguments("a document cannot change type".into()));
            }
            let payload = compose::to_payload(&args.draft, po.as_deref(), true).map_err(|e| ApiError::BadArguments(e.to_string()))?;
            writes::queue_update(store, &realm, entity_type, id, payload, now)?
        }
    };
    to_json(serde_json::json!({ "outboxId": record.id.to_string(), "entityId": record.local_entity_id, "state": record.state.as_str() }))
}

fn create_customer(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: CustomerArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let payload = compose::customer_payload(&args.display_name, args.company_name.as_deref(), args.email.as_deref())
        .map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let name = text(&payload, "/DisplayName").unwrap_or_default();
    // QBO rejects a duplicate DisplayName; say so now rather than after a drain.
    let pending = store
        .outbox_records(&realm, OPEN)?
        .into_iter()
        .filter(|r| r.entity_type == EntityType::Customer)
        .filter_map(|r| text(&r.payload_json, "/DisplayName"));
    let taken = store
        .contacts(&realm, ContactType::Customer)?
        .into_iter()
        .map(|(_, existing, _)| existing)
        .chain(pending)
        .any(|existing| existing.eq_ignore_ascii_case(&name));
    if taken {
        return Err(ApiError::BadArguments(format!("a customer named {name:?} already exists")));
    }
    let record = writes::queue_create(store, &realm, EntityType::Customer, payload, now)?;
    to_json(serde_json::json!({ "outboxId": record.id.to_string(), "entityId": record.local_entity_id, "displayName": name }))
}

fn outbox(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    let realm = realm(&args.realm)?;
    let records = store.outbox_records(&realm, OPEN)?;
    let out: Vec<WriteDto> = records
        .iter()
        .map(|r| {
            let blocked_by = r
                .depends_on
                .and_then(|parent| records.iter().find(|p| p.id == parent))
                .map(|p| write_dto(p, None).label);
            write_dto(r, blocked_by)
        })
        .collect();
    to_json(out)
}

fn open_invoices_command(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: OpenInvoicesArgs = parse(args)?;
    let (open, held) = open_invoices(store, &realm(&args.realm)?, &args.customer_id)?;
    open_json(open, held)
}

fn open_bills_command(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: OpenBillsArgs = parse(args)?;
    let (open, held) = open_documents(store, &realm(&args.realm)?, ContactType::Vendor, &args.vendor_id)?;
    open_json(open, held)
}

/// Bank and credit card accounts: what a bill can be paid from.
fn payment_accounts(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    to_json(
        store
            .payment_accounts(&realm(&args.realm)?)?
            .into_iter()
            .map(|(id, name, account_type)| serde_json::json!({ "qboId": id, "name": name, "accountType": account_type }))
            .collect::<Vec<_>>(),
    )
}

fn pay_from(store: &Store, realm: &RealmId, account_id: &str) -> Result<PayFrom, ApiError> {
    store
        .payment_accounts(realm)?
        .into_iter()
        .find(|(id, _, _)| id == account_id)
        .and_then(|(_, _, account_type)| PayFrom::for_account_type(&account_type))
        .ok_or_else(|| ApiError::BadArguments("pay from a bank or credit card account".into()))
}

fn bill_payment_preview(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: BillPaymentArgs = parse(args)?;
    let (open, _) = open_documents(store, &realm(&args.realm)?, ContactType::Vendor, &args.draft.vendor_id)?;
    let plan = compose::plan_bill_payment(&args.draft, &open).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    to_json(plan_json(&plan, &open))
}

fn pay_bills(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: BillPaymentArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let (open, _) = open_documents(store, &realm, ContactType::Vendor, &args.draft.vendor_id)?;
    let plan = compose::plan_bill_payment(&args.draft, &open).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let from = pay_from(store, &realm, &args.draft.account_id)?;
    let payload = compose::bill_payment_payload(&args.draft, &plan, from).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let record = writes::queue_create(store, &realm, EntityType::BillPayment, payload, now)?;
    let mut out = plan_json(&plan, &open);
    out["outboxId"] = serde_json::json!(record.id.to_string());
    out["entityId"] = serde_json::json!(record.local_entity_id);
    to_json(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DepositArgs {
    realm: String,
    draft: DepositDraft,
}

/// What is waiting to be banked, less anything already on a queued deposit.
fn undeposited_rows(store: &Store, realm: &RealmId) -> Result<Vec<crate::store::outbox_store::UndepositedRow>, ApiError> {
    let queued = queued_deposits(store, realm)?;
    Ok(store.undeposited(realm)?.into_iter().filter(|r| !queued.contains(&r.0)).collect())
}

fn undeposited(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    to_json(
        undeposited_rows(store, &realm(&args.realm)?)?
            .into_iter()
            .map(|(id, doc_type, reference, date, customer, amount)| {
                serde_json::json!({ "qboId": id, "docType": doc_type, "reference": reference, "txnDate": date, "customerName": customer, "amountMinor": amount })
            })
            .collect::<Vec<_>>(),
    )
}

fn make_deposit(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: DepositArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let bank = store.payment_accounts(&realm)?.into_iter().any(|(id, _, t)| id == args.draft.account_id && t == "Bank");
    if !bank {
        return Err(ApiError::BadArguments("deposit to a bank account".into()));
    }
    let waiting = undeposited_rows(store, &realm)?;
    let mut items = Vec::new();
    for id in &args.draft.payment_ids {
        let row = waiting
            .iter()
            .find(|r| &r.0 == id)
            .ok_or_else(|| ApiError::BadArguments(format!("payment {id} is not waiting to be deposited")))?;
        if items.iter().any(|i: &DepositItem| &i.qbo_id == id) {
            return Err(ApiError::BadArguments(format!("payment {id} is ticked twice")));
        }
        items.push(DepositItem { qbo_id: row.0.clone(), txn_type: row.1.clone(), amount: ledger_core::Money::from_minor(row.5) });
    }
    let payload = compose::deposit_payload(&args.draft, &items).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let total = payload.get("TotalAmt").map(minor_of).unwrap_or(0);
    let record = writes::queue_create(store, &realm, EntityType::Deposit, payload, now)?;
    to_json(serde_json::json!({ "outboxId": record.id.to_string(), "entityId": record.local_entity_id, "totalMinor": total, "count": items.len() }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpenseArgs {
    realm: String,
    draft: ExpenseDraft,
}

fn expense_accounts(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: RealmOnly = parse(args)?;
    to_json(
        store
            .expense_accounts(&realm(&args.realm)?)?
            .into_iter()
            .map(|(id, name, account_type)| serde_json::json!({ "qboId": id, "name": name, "accountType": account_type }))
            .collect::<Vec<_>>(),
    )
}

fn save_expense(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: ExpenseArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let from = pay_from(store, &realm, &args.draft.account_id)?;
    let known: Vec<String> = store.expense_accounts(&realm)?.into_iter().map(|a| a.0).collect();
    if let Some((index, _)) = args.draft.lines.iter().enumerate().find(|(_, l)| !l.account_id.is_empty() && !known.contains(&l.account_id)) {
        return Err(ApiError::BadArguments(format!("line {}: charge it to an expense account", index + 1)));
    }
    if let Some(payee) = args.draft.payee_id.as_deref().filter(|p| !p.is_empty()) {
        if store.contact_name(&realm, ContactType::Vendor, payee)?.is_none() {
            return Err(ApiError::BadArguments("choose the payee from the vendor list".into()));
        }
    }
    let (payload, total) = compose::expense_payload(&args.draft, from).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let record = writes::queue_create(store, &realm, EntityType::Purchase, payload, now)?;
    to_json(serde_json::json!({ "outboxId": record.id.to_string(), "entityId": record.local_entity_id, "totalMinor": total.minor() }))
}

fn open_json(open: Vec<OpenInvoice>, held: Vec<i64>) -> Result<Value, ApiError> {
    to_json(
        open.iter()
            .zip(held)
            .map(|(i, queued)| {
                serde_json::json!({
                    "qboId": i.qbo_id, "docNumber": i.doc_number, "txnDate": i.txn_date, "dueDate": i.due_date,
                    "balanceMinor": i.balance.minor(), "queuedMinor": queued,
                })
            })
            .collect::<Vec<_>>(),
    )
}

/// Where a payment would go, without queueing it.
fn payment_preview(store: &Store, args: Value) -> Result<Value, ApiError> {
    let args: PaymentArgs = parse(args)?;
    let (open, _) = open_invoices(store, &realm(&args.realm)?, &args.draft.customer_id)?;
    let plan = compose::plan_payment(&args.draft, &open).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    to_json(plan_json(&plan, &open))
}

fn receive_payment(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: PaymentArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    if args.draft.customer_id.starts_with(crate::worker::LOCAL_ID_PREFIX) {
        return Err(ApiError::BadArguments("this customer is still on its way to QuickBooks and has no invoices to pay yet".into()));
    }
    let (open, _) = open_invoices(store, &realm, &args.draft.customer_id)?;
    let plan = compose::plan_payment(&args.draft, &open).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let payload = compose::payment_payload(&args.draft, &plan).map_err(|e| ApiError::BadArguments(e.to_string()))?;
    let record = writes::queue_create(store, &realm, EntityType::Payment, payload, now)?;
    let mut out = plan_json(&plan, &open);
    out["outboxId"] = serde_json::json!(record.id.to_string());
    out["entityId"] = serde_json::json!(record.local_entity_id);
    to_json(out)
}

fn resolve_write(store: &Store, args: Value, now: DateTime<Utc>) -> Result<Value, ApiError> {
    let args: ResolveArgs = parse(args)?;
    let realm = realm(&args.realm)?;
    let id = Uuid::parse_str(&args.id).map_err(|_| ApiError::BadArguments("not an outbox id".into()))?;
    let resolution = match args.action.as_str() {
        "retry" => Resolution::Retry,
        "discard" => Resolution::Discard,
        other => return Err(ApiError::BadArguments(format!("action must be retry or discard, not {other:?}"))),
    };
    let record = writes::resolve(store, &realm, id, resolution, now)?;
    to_json(write_dto(&record, None))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::super::dispatch_at;
    use crate::client::MockQbo;
    use crate::demo::{self, DemoSize, DEMO_REALM};
    use crate::store::Store;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap()
    }

    fn book(writes: bool) -> Store {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 6, vendors: 2, items: 8, invoices: 20, purchase_orders: 2 };
        demo::seed(&store, size, 5, now()).unwrap();
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        store.set_write_enabled(&realm, writes).unwrap();
        store
    }

    fn draft(customer: &str) -> serde_json::Value {
        json!({
            "docType": "Invoice", "contactId": customer, "txnDate": "2026-11-02", "dueDate": "2026-12-02",
            "lines": [ { "itemId": "1035", "description": "Rescue tube, red", "qty": "20", "rate": "49.25", "taxable": false } ]
        })
    }

    #[test]
    fn saving_while_writes_are_off_is_refused_plainly() {
        let store = book(false);
        let error = dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": draft("1005") }), now()).unwrap_err();
        assert_eq!(error.status(), 403);
        assert!(error.to_string().contains("writes are off"));
    }

    #[test]
    fn a_new_invoice_shows_in_its_register_as_not_yet_in_quickbooks() {
        let store = book(true);
        let saved = dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": draft("1005") }), now()).unwrap();
        let local = saved["entityId"].as_str().unwrap().to_string();
        assert!(local.starts_with("local:"));

        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Invoice" }), now()).unwrap();
        let first = &register[0];
        assert_eq!(first["qboId"], local.as_str(), "queued documents list first");
        assert_eq!(first["totalMinor"], 98_500);
        assert_eq!(first["status"], "Not yet in QuickBooks");
        assert_eq!(first["pendingWrite"], "pending");

        let detail = dispatch_at(&store, "document", json!({ "realm": DEMO_REALM, "qboId": local }), now()).unwrap();
        assert_eq!(detail["lines"][0]["amountMinor"], 98_500);
        let status = dispatch_at(&store, "status", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert_eq!(status["outboxPending"], 1);
    }

    #[test]
    fn a_new_customer_is_pickable_at_once_and_its_invoice_waits_for_it() {
        let store = book(true);
        let customer = dispatch_at(&store, "createCustomer", json!({ "realm": DEMO_REALM, "displayName": "Gull Pond Swim Team" }), now()).unwrap();
        let local = customer["entityId"].as_str().unwrap().to_string();
        let picker = dispatch_at(&store, "customers", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert!(picker.as_array().unwrap().iter().any(|c| c["qboId"] == local.as_str() && c["pending"] == true));

        dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": draft(&local) }), now()).unwrap();
        let writes = dispatch_at(&store, "outbox", json!({ "realm": DEMO_REALM }), now()).unwrap();
        let invoice = writes.as_array().unwrap().iter().find(|w| w["entityType"] == "Invoice").unwrap();
        assert_eq!(invoice["blockedBy"], "Gull Pond Swim Team");

        let duplicate = dispatch_at(&store, "createCustomer", json!({ "realm": DEMO_REALM, "displayName": "gull pond swim team" }), now());
        assert_eq!(duplicate.unwrap_err().status(), 400, "QBO would refuse the duplicate name");
    }

    #[test]
    fn an_existing_invoice_round_trips_through_the_editor_as_a_sparse_update() {
        let store = book(true);
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        let invoice = store.list_documents(&realm, crate::domain::DocumentType::Invoice, 1).unwrap().remove(0);
        let loaded = dispatch_at(&store, "documentDraft", json!({ "realm": DEMO_REALM, "qboId": invoice.qbo_id }), now()).unwrap();
        let mut draft = loaded["draft"].clone();
        draft["privateNote"] = json!("Ship with the next pallet");
        dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "qboId": invoice.qbo_id, "draft": draft }), now()).unwrap();

        let record = store.open_writes_for(&realm, &invoice.qbo_id).unwrap().remove(0);
        assert_eq!(record.payload_json["sparse"], true);
        assert_eq!(record.base_sync_token.as_deref(), Some("0"));
        assert_eq!(record.payload_json["PrivateNote"], "Ship with the next pallet");
        // The total the editor would send equals what the replica holds.
        let sent: i64 = record.payload_json["Line"].as_array().unwrap().iter().map(|l| super::minor_of(&l["Amount"])).sum();
        assert_eq!(sent, invoice.total.minor());
    }

    #[test]
    fn a_purchase_order_queues_for_a_vendor_and_lists_under_purchase_orders() {
        let store = book(true);
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        let vendor = store.contacts(&realm, crate::domain::ContactType::Vendor).unwrap().remove(0);
        let item = store.items(&realm).unwrap().remove(0).qbo_id;
        let po = json!({ "docType": "PurchaseOrder", "contactId": vendor.0, "txnDate": "2026-11-02",
                         "lines": [ { "itemId": item, "description": "XLPE sheet", "qty": "40", "rate": "18.4375" } ] });
        dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": po }), now()).unwrap();
        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "PurchaseOrder" }), now()).unwrap();
        assert_eq!(register[0]["contactName"], vendor.1.as_str());
        assert_eq!(register[0]["contactType"], "Vendor");
        assert_eq!(register[0]["totalMinor"], 73_750);
    }

    #[test]
    fn a_refused_write_is_listed_and_can_be_discarded() {
        let store = book(true);
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        dispatch_at(&store, "saveDocument", json!({ "realm": DEMO_REALM, "draft": draft("1005") }), now()).unwrap();
        let mut qbo = MockQbo::new(now());
        qbo.fail_next(crate::client::QboError::Validation("Invalid account".into()));
        crate::writes::drain_once(&store, &realm, &mut qbo, now()).unwrap();

        let writes = dispatch_at(&store, "outbox", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert_eq!(writes[0]["status"], "Refused by QuickBooks");
        assert!(writes[0]["lastError"].as_str().unwrap().contains("Invalid account"));
        let id = writes[0]["id"].as_str().unwrap();
        dispatch_at(&store, "resolveWrite", json!({ "realm": DEMO_REALM, "id": id, "action": "discard" }), now()).unwrap();
        assert!(dispatch_at(&store, "outbox", json!({ "realm": DEMO_REALM }), now()).unwrap().as_array().unwrap().is_empty());
    }

    fn open_for(store: &Store) -> (String, Vec<serde_json::Value>) {
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        let customer = store
            .contacts(&realm, crate::domain::ContactType::Customer)
            .unwrap()
            .into_iter()
            .map(|c| c.0)
            .find(|id| {
                dispatch_at(store, "openInvoices", json!({ "realm": DEMO_REALM, "customerId": id }), now()).unwrap().as_array().unwrap().len() >= 2
            })
            .expect("a demo customer with two open invoices");
        let open = dispatch_at(store, "openInvoices", json!({ "realm": DEMO_REALM, "customerId": customer }), now()).unwrap();
        (customer, open.as_array().unwrap().clone())
    }

    #[test]
    fn a_received_payment_goes_oldest_first_and_cannot_be_applied_twice() {
        let store = book(true);
        let (customer, open) = open_for(&store);
        let first = open[0]["balanceMinor"].as_i64().unwrap();
        let amount = format!("{}.{:02}", (first + 100) / 100, (first + 100) % 100);
        let draft = json!({ "customerId": customer, "txnDate": "2026-11-02", "amount": amount, "reference": "4417" });

        let preview = dispatch_at(&store, "paymentPreview", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        assert_eq!(preview["applied"][0]["invoiceId"], open[0]["qboId"]);
        assert_eq!(preview["applied"][0]["amountMinor"], first);
        assert_eq!(preview["applied"][1]["amountMinor"], 100);
        assert!(store.outbox_records(&crate::domain::RealmId::parse(DEMO_REALM).unwrap(), super::OPEN).unwrap().is_empty(), "a preview queues nothing");

        let saved = dispatch_at(&store, "receivePayment", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        let local = saved["entityId"].as_str().unwrap().to_string();

        let after = dispatch_at(&store, "openInvoices", json!({ "realm": DEMO_REALM, "customerId": customer }), now()).unwrap();
        assert!(after.as_array().unwrap().iter().all(|i| i["qboId"] != open[0]["qboId"]), "the paid invoice is no longer open");
        assert_eq!(after[0]["queuedMinor"], 100, "the part-paid one shows what is on its way");

        let doubled = json!({ "customerId": customer, "txnDate": "2026-11-02", "amount": "1.00",
                              "applications": [ { "invoiceId": open[0]["qboId"], "amount": "1.00" } ] });
        let refused = dispatch_at(&store, "receivePayment", json!({ "realm": DEMO_REALM, "draft": doubled }), now()).unwrap_err();
        assert!(refused.to_string().contains("not open"), "{refused}");

        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Payment" }), now()).unwrap();
        assert_eq!(register[0]["qboId"], local.as_str());
        assert_eq!(register[0]["docNumber"], "4417");
        assert_eq!(register[0]["totalMinor"], first + 100);
        let detail = dispatch_at(&store, "document", json!({ "realm": DEMO_REALM, "qboId": local }), now()).unwrap();
        assert!(detail["lines"][0]["description"].as_str().unwrap().starts_with("Applied to invoice"));
    }

    #[test]
    fn a_payment_lands_through_the_drain_as_a_linked_payment() {
        let store = book(true);
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        let (customer, open) = open_for(&store);
        let draft = json!({ "customerId": customer, "txnDate": "2026-11-02", "amount": "25.00" });
        dispatch_at(&store, "receivePayment", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        let mut qbo = MockQbo::new(now());
        crate::writes::drain_once(&store, &realm, &mut qbo, now()).unwrap();
        assert_eq!(qbo.count(&realm, crate::domain::EntityType::Payment), 1);
        let writes = dispatch_at(&store, "outbox", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert!(writes.as_array().unwrap().is_empty(), "applied writes leave the open list");
        let _ = open;
    }

    #[test]
    fn bills_are_paid_from_a_bank_or_card_account_and_land_through_the_drain() {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 4, vendors: 3, items: 8, invoices: 4, purchase_orders: 60 };
        demo::seed(&store, size, 5, now()).unwrap();
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        store.set_write_enabled(&realm, true).unwrap();
        let accounts = dispatch_at(&store, "paymentAccounts", json!({ "realm": DEMO_REALM }), now()).unwrap();
        let types: Vec<_> = accounts.as_array().unwrap().iter().map(|a| a["accountType"].as_str().unwrap().to_string()).collect();
        assert!(types.iter().all(|t| t == "Bank" || t == "Credit Card"), "{types:?}");
        let checking = accounts.as_array().unwrap().iter().find(|a| a["accountType"] == "Bank").unwrap()["qboId"].clone();
        let receivable = json!("92");

        let vendor = store
            .contacts(&realm, crate::domain::ContactType::Vendor)
            .unwrap()
            .into_iter()
            .map(|c| c.0)
            .find(|id| !dispatch_at(&store, "openBills", json!({ "realm": DEMO_REALM, "vendorId": id }), now()).unwrap().as_array().unwrap().is_empty())
            .expect("a demo vendor with an open bill");
        let bills = dispatch_at(&store, "openBills", json!({ "realm": DEMO_REALM, "vendorId": vendor }), now()).unwrap();
        let owed = bills[0]["balanceMinor"].as_i64().unwrap();
        let amount = format!("{}.{:02}", owed / 100, owed % 100);
        let draft = json!({ "vendorId": vendor, "txnDate": "2026-11-02", "amount": amount, "accountId": checking, "reference": "1207" });

        let wrong = json!({ "vendorId": vendor, "txnDate": "2026-11-02", "amount": amount, "accountId": receivable });
        assert_eq!(dispatch_at(&store, "payBills", json!({ "realm": DEMO_REALM, "draft": wrong }), now()).unwrap_err().status(), 400);

        let saved = dispatch_at(&store, "payBills", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        assert_eq!(saved["applied"][0]["amountMinor"], owed);
        let after = dispatch_at(&store, "openBills", json!({ "realm": DEMO_REALM, "vendorId": vendor }), now()).unwrap();
        assert!(after.as_array().unwrap().iter().all(|b| b["qboId"] != bills[0]["qboId"]));
        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "BillPayment" }), now()).unwrap();
        assert_eq!(register[0]["docNumber"], "1207");
        assert_eq!(register[0]["contactType"], "Vendor");

        let mut qbo = MockQbo::new(now());
        crate::writes::drain_once(&store, &realm, &mut qbo, now()).unwrap();
        assert_eq!(qbo.count(&realm, crate::domain::EntityType::BillPayment), 1);
    }

    #[test]
    fn a_deposit_banks_waiting_payments_once() {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 6, vendors: 2, items: 8, invoices: 400, purchase_orders: 2 };
        demo::seed(&store, size, 5, now()).unwrap();
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        store.set_write_enabled(&realm, true).unwrap();
        let waiting = dispatch_at(&store, "undeposited", json!({ "realm": DEMO_REALM }), now()).unwrap();
        let waiting = waiting.as_array().unwrap().clone();
        assert!(!waiting.is_empty(), "the demo leaves recent cheques in Undeposited Funds");
        let ids: Vec<_> = waiting.iter().take(2).map(|w| w["qboId"].clone()).collect();
        let expected: i64 = waiting.iter().take(2).map(|w| w["amountMinor"].as_i64().unwrap()).sum();

        let wrong = json!({ "accountId": "92", "txnDate": "2026-11-02", "paymentIds": ids });
        assert_eq!(dispatch_at(&store, "makeDeposit", json!({ "realm": DEMO_REALM, "draft": wrong }), now()).unwrap_err().status(), 400);

        let draft = json!({ "accountId": "90", "txnDate": "2026-11-02", "paymentIds": ids });
        let saved = dispatch_at(&store, "makeDeposit", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        assert_eq!(saved["totalMinor"], expected);
        let after = dispatch_at(&store, "undeposited", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert_eq!(after.as_array().unwrap().len(), waiting.len() - 2, "banked payments leave the list");
        let again = dispatch_at(&store, "makeDeposit", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap_err();
        assert!(again.to_string().contains("not waiting"), "{again}");

        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Deposit" }), now()).unwrap();
        assert_eq!(register[0]["totalMinor"], expected);
        let mut qbo = MockQbo::new(now());
        crate::writes::drain_once(&store, &realm, &mut qbo, now()).unwrap();
        assert_eq!(qbo.count(&realm, crate::domain::EntityType::Deposit), 1);
    }

    #[test]
    fn an_expense_on_the_card_is_charged_to_expense_accounts() {
        let store = book(true);
        let realm = crate::domain::RealmId::parse(DEMO_REALM).unwrap();
        let accounts = dispatch_at(&store, "expenseAccounts", json!({ "realm": DEMO_REALM }), now()).unwrap();
        assert!(accounts.as_array().unwrap().iter().any(|a| a["name"] == "Shipping and Freight"));
        assert!(accounts.as_array().unwrap().iter().all(|a| a["accountType"] != "Bank"));
        let vendor = store.contacts(&realm, crate::domain::ContactType::Vendor).unwrap().remove(0).0;

        let draft = json!({ "payeeId": vendor, "txnDate": "2026-11-02", "accountId": "93",
                            "lines": [ { "accountId": "95", "description": "LTL to Garwood", "amount": "412.50" } ] });
        let saved = dispatch_at(&store, "saveExpense", json!({ "realm": DEMO_REALM, "draft": draft }), now()).unwrap();
        assert_eq!(saved["totalMinor"], 41_250);
        let register = dispatch_at(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Purchase" }), now()).unwrap();
        assert_eq!(register[0]["qboId"], saved["entityId"]);
        assert_eq!(register[0]["contactType"], "Vendor");
        let detail = dispatch_at(&store, "document", json!({ "realm": DEMO_REALM, "qboId": saved["entityId"] }), now()).unwrap();
        assert_eq!(detail["lines"][0]["description"], "Shipping and Freight: LTL to Garwood");

        let to_a_bank = json!({ "txnDate": "2026-11-02", "accountId": "93", "lines": [ { "accountId": "90", "amount": "5" } ] });
        let refused = dispatch_at(&store, "saveExpense", json!({ "realm": DEMO_REALM, "draft": to_a_bank }), now()).unwrap_err();
        assert!(refused.to_string().contains("expense account"), "{refused}");

        let mut qbo = MockQbo::new(now());
        crate::writes::drain_once(&store, &realm, &mut qbo, now()).unwrap();
        assert_eq!(qbo.count(&realm, crate::domain::EntityType::Purchase), 1);
    }
}
