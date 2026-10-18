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
use crate::compose::{self, DocumentDraft, DraftLine};
use crate::domain::{ContactType, DocumentType, EntityType, RealmId};
use crate::outbox::{Operation, OutboxRecord, OutboxState};
use crate::store::Store;
use crate::writes::{self, Resolution, WriteError};

/// Commands that queue writes. Each refuses with 403 while the company's
/// writes are off.
pub const WRITE_COMMANDS: &[&str] = &[
    "customers",
    "documentDraft",
    "saveDocument",
    "createCustomer",
    "outbox",
    "resolveWrite",
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
    let lines: Vec<LineDto> = payload
        .get("Line")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, line)| LineDto {
            line_no: i as i64 + 1,
            item_id: text(line, "/SalesItemLineDetail/ItemRef/value"),
            description: text(line, "/Description"),
            qty: line.pointer("/SalesItemLineDetail/Qty").map(|v| v.to_string()),
            unit_price: line.pointer("/SalesItemLineDetail/UnitPrice").map(|v| v.to_string()),
            amount_minor: line.get("Amount").map(minor_of).unwrap_or(0),
            class_id: text(line, "/SalesItemLineDetail/ClassRef/value"),
            taxable: text(line, "/SalesItemLineDetail/TaxCodeRef/value").as_deref() == Some("TAX"),
        })
        .collect();
    let contact_id = text(payload, "/CustomerRef/value");
    let contact_name = match &contact_id {
        Some(id) => customer_name(store, realm, id)?,
        None => None,
    };
    let document = DocumentDto {
        qbo_id: record.local_entity_id.clone(),
        doc_type,
        doc_number: text(payload, "/DocNumber"),
        txn_date: text(payload, "/TxnDate").unwrap_or_default(),
        due_date: text(payload, "/DueDate"),
        contact_id,
        contact_type: Some(ContactType::Customer),
        contact_name,
        class_id: text(payload, "/ClassRef/value"),
        // Before tax: QuickBooks adds tax when it accepts the document.
        total_minor: lines.iter().map(|l| l.amount_minor).sum(),
        balance_minor: None,
        status: Some(write_status(record.state).to_string()),
        po_number: text(payload, "/CustomField/0/StringValue"),
        private_note: text(payload, "/PrivateNote"),
        customer_memo: text(payload, "/CustomerMemo/value"),
        pending_write: Some(record.state.as_str().to_string()),
    };
    Ok(Some((document, lines)))
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
struct ResolveArgs {
    realm: String,
    id: String,
    action: String,
}

pub(super) fn dispatch(store: &Store, command: &str, args: Value, now: DateTime<Utc>) -> Option<Result<Value, ApiError>> {
    let result = match command {
        "customers" => customers(store, args),
        "documentDraft" => document_draft(store, args),
        "saveDocument" => save_document(store, args, now),
        "createCustomer" => create_customer(store, args, now),
        "outbox" => outbox(store, args),
        "resolveWrite" => resolve_write(store, args, now),
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
        "customerId": doc.contact_id,
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
            "docType": "Invoice", "customerId": customer, "txnDate": "2026-11-02", "dueDate": "2026-12-02",
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
}
