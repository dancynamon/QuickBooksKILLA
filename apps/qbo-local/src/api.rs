//! The UI's command surface: reads, plus (M2) the commands that queue writes
//! in [`write_commands`], each refused while the company's writes are off.
//!
//! One entry point, [`dispatch`], taking a command name and JSON arguments and
//! returning JSON. Both front doors call it: the Tauri shell's single `api`
//! command, and the loopback dev server (`qbo-local serve`). The UI therefore
//! cannot tell which one it is talking to, and every command is tested here
//! without either.
//!
//! The wire types are separate from the store's on purpose: they are a
//! contract with TypeScript (`ui/src/api.ts`), and a field renamed in the store
//! should be a compile error here rather than a blank column there. Money goes
//! over the wire as integer minor units — never a float — so the UI formats an
//! exact value.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::domain::{ContactType, DocumentType, RealmId};
use crate::store::search::{Hit, MatchReason};
use crate::store::reports::AgingSide;
use crate::store::{DocumentRow, LineRow, Store, StoreError};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unknown command {0:?}")]
    UnknownCommand(String),
    #[error("bad arguments: {0}")]
    BadArguments(String),
    #[error("not found")]
    NotFound,
    /// Refused by policy, not by a mistake in the request: writes are off.
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            ApiError::UnknownCommand(_) | ApiError::NotFound => 404,
            ApiError::BadArguments(_) => 400,
            ApiError::Forbidden(_) => 403,
            ApiError::Store(_) => 500,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RealmDto {
    pub realm_id: String,
    pub display_name: String,
    pub write_enabled: bool,
    pub environment: String,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDto {
    pub qbo_id: String,
    pub doc_type: DocumentType,
    pub doc_number: Option<String>,
    pub txn_date: String,
    pub due_date: Option<String>,
    pub contact_id: Option<String>,
    pub contact_type: Option<ContactType>,
    pub contact_name: Option<String>,
    pub class_id: Option<String>,
    pub total_minor: i64,
    pub balance_minor: Option<i64>,
    pub status: Option<String>,
    pub po_number: Option<String>,
    pub private_note: Option<String>,
    pub customer_memo: Option<String>,
    /// Set on a document that exists only in the outbox: its write state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_write: Option<String>,
}

impl From<DocumentRow> for DocumentDto {
    fn from(row: DocumentRow) -> Self {
        DocumentDto {
            qbo_id: row.qbo_id,
            doc_type: row.doc_type,
            doc_number: row.doc_number,
            txn_date: row.txn_date,
            due_date: row.due_date,
            contact_id: row.contact_id,
            contact_type: row.contact_type,
            contact_name: row.contact_name,
            class_id: row.class_id,
            total_minor: row.total.minor(),
            balance_minor: row.balance.map(|m| m.minor()),
            status: row.doc_status,
            po_number: row.po_number,
            private_note: row.private_note,
            customer_memo: row.customer_memo,
            pending_write: None,
        }
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LineDto {
    pub line_no: i64,
    pub item_id: Option<String>,
    pub description: Option<String>,
    pub qty: Option<String>,
    pub unit_price: Option<String>,
    pub amount_minor: i64,
    pub class_id: Option<String>,
    pub taxable: bool,
}

impl From<LineRow> for LineDto {
    fn from(row: LineRow) -> Self {
        LineDto {
            line_no: row.line_no,
            item_id: row.item_id,
            description: row.description,
            qty: row.qty,
            unit_price: row.unit_price,
            amount_minor: row.amount.minor(),
            class_id: row.class_id,
            taxable: row.is_taxable,
        }
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkDto {
    pub from_qbo_id: String,
    pub from_type: String,
    pub to_qbo_id: String,
    pub to_type: String,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDetailDto {
    pub document: DocumentDto,
    pub lines: Vec<LineDto>,
    /// Every document in this one's chain, itself included.
    pub related: Vec<DocumentDto>,
    pub links: Vec<LinkDto>,
    /// Linked ids the replica does not hold — shown as a gap, not hidden.
    pub unresolved: Vec<String>,
    /// Queued or failed writes about this document (§6.7).
    pub open_writes: Vec<write_commands::WriteDto>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HitDto {
    #[serde(rename_all = "camelCase")]
    Document { reason: String, document: Box<DocumentDto> },
    #[serde(rename_all = "camelCase")]
    Contact {
        reason: String,
        contact_type: ContactType,
        qbo_id: String,
        display_name: String,
        company_name: Option<String>,
        balance_minor: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    Item {
        reason: String,
        qbo_id: String,
        name: String,
        sku: Option<String>,
        unit_price: Option<String>,
    },
}

/// Labels the UI shows next to a hit, so a number that matched as a PO says so.
fn reason_label(reason: MatchReason) -> &'static str {
    match reason {
        MatchReason::DocumentNumber => "number",
        MatchReason::PurchaseOrderNumber => "customer PO",
        MatchReason::Sku => "SKU",
        MatchReason::Amount => "amount",
        MatchReason::DocumentNumberPrefix => "number prefix",
        MatchReason::Text => "text",
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchDto {
    pub hits: Vec<HitDto>,
    /// Measured, and shown in the palette — the speed is the product.
    pub elapsed_micros: u128,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EntityStatusDto {
    pub entity_type: String,
    pub count: i64,
    pub last_cdc_cursor: Option<String>,
    pub last_full_sweep: Option<String>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub environment: String,
    pub entity_types: Vec<EntityStatusDto>,
    /// The newest cursor across types: "last synced" in the chrome.
    pub last_synced: Option<String>,
    pub outbox_pending: i64,
    pub outbox_needs_attention: i64,
    pub quarantined: i64,
    pub write_enabled: bool,
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RealmArgs {
    realm: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchArgs {
    realm: String,
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    25
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListArgs {
    realm: String,
    doc_type: DocumentType,
    #[serde(default = "default_list_limit")]
    limit: i64,
}

fn default_list_limit() -> i64 {
    MAX_ROWS
}

/// A whole register in one response. The UI renders only the visible window,
/// so the cost of a large register is the query and the JSON, both measured
/// in tens of milliseconds at the book's size.
const MAX_ROWS: i64 = 50_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentArgs {
    realm: String,
    qbo_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactArgs {
    realm: String,
    contact_type: ContactType,
    contact_id: String,
    #[serde(default = "default_list_limit")]
    limit: i64,
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ApiError> {
    serde_json::from_value(args).map_err(|e| ApiError::BadArguments(e.to_string()))
}

fn realm(raw: &str) -> Result<RealmId, ApiError> {
    RealmId::parse(raw).map_err(|e| ApiError::BadArguments(e.to_string()))
}

fn to_json(value: impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::BadArguments(e.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgingArgs {
    realm: String,
    side: String,
    /// `YYYY-MM-DD`; today when omitted.
    as_of: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeArgs {
    realm: String,
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemArgs {
    realm: String,
    item_id: String,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgingRowDto {
    pub contact_id: String,
    pub contact_name: Option<String>,
    pub current_minor: i64,
    pub days1to30_minor: i64,
    pub days31to60_minor: i64,
    pub days61to90_minor: i64,
    pub over90_minor: i64,
    pub total_minor: i64,
    pub documents: i64,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgingDto {
    pub as_of: String,
    pub rows: Vec<AgingRowDto>,
}

/// A date argument must be `YYYY-MM-DD`: it is compared as text in SQL, and
/// anything else would compare wrongly rather than fail.
fn date_arg(raw: &str) -> Result<String, ApiError> {
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(|d| d.to_string())
        .map_err(|_| ApiError::BadArguments(format!("not a YYYY-MM-DD date: {raw:?}")))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The read commands, by name. Kept as a list so the tests can enumerate
/// them; nothing in this list writes. Writes are [`WRITE_COMMANDS`].
pub const COMMANDS: &[&str] = &[
    "realms",
    "status",
    "search",
    "documents",
    "document",
    "contactDocuments",
    "classes",
    "aging",
    "salesByCustomer",
    "items",
    "itemDocuments",
];

mod write_commands;
pub use write_commands::WRITE_COMMANDS;

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value, ApiError> {
    dispatch_at(store, command, args, chrono::Utc::now())
}

/// [`dispatch`] at a given time, so write commands are testable with a fixed clock.
pub fn dispatch_at(store: &Store, command: &str, args: Value, now: chrono::DateTime<chrono::Utc>) -> Result<Value, ApiError> {
    if let Some(result) = write_commands::dispatch(store, command, args.clone(), now) {
        return result;
    }
    match command {
        "realms" => to_json(
            store
                .realms()?
                .into_iter()
                .map(|row| RealmDto {
                    display_name: row.display_name,
                    write_enabled: row.is_write_enabled,
                    environment: store
                        .realm_environment(&row.realm_id)
                        .map(|e| e.as_str().to_string())
                        .unwrap_or_else(|_| "production".into()),
                    realm_id: row.realm_id.to_string(),
                })
                .collect::<Vec<_>>(),
        ),
        "status" => {
            let args: RealmArgs = parse(args)?;
            let status = store.sync_status(&realm(&args.realm)?)?;
            let last_synced = status
                .entity_types
                .iter()
                .filter_map(|t| t.last_cdc_cursor.clone())
                .max();
            to_json(StatusDto {
                environment: store.realm_environment(&realm(&args.realm)?)?.as_str().to_string(),
                entity_types: status
                    .entity_types
                    .into_iter()
                    .map(|t| EntityStatusDto {
                        entity_type: t.entity_type,
                        count: t.count,
                        last_cdc_cursor: t.last_cdc_cursor,
                        last_full_sweep: t.last_full_sweep,
                    })
                    .collect(),
                last_synced,
                outbox_pending: status.outbox_pending,
                outbox_needs_attention: status.outbox_needs_attention,
                quarantined: status.quarantined,
                write_enabled: status.is_write_enabled,
            })
        }
        "search" => {
            let args: SearchArgs = parse(args)?;
            let started = Instant::now();
            let hits = store.search(&realm(&args.realm)?, &args.query, args.limit.min(200))?;
            let elapsed_micros = started.elapsed().as_micros();
            to_json(SearchDto {
                hits: hits
                    .into_iter()
                    .map(|hit| {
                        let reason = reason_label(hit.reason).to_string();
                        match hit.hit {
                            Hit::Document(row) => HitDto::Document {
                                reason,
                                document: Box::new((*row).into()),
                            },
                            Hit::Contact(contact) => HitDto::Contact {
                                reason,
                                contact_type: contact.contact_type,
                                qbo_id: contact.qbo_id,
                                display_name: contact.display_name,
                                company_name: contact.company_name,
                                balance_minor: contact.balance.map(|m| m.minor()),
                            },
                            Hit::Item(item) => HitDto::Item {
                                reason,
                                qbo_id: item.qbo_id,
                                name: item.name,
                                sku: item.sku,
                                unit_price: item.unit_price,
                            },
                        }
                    })
                    .collect(),
                elapsed_micros,
            })
        }
        "documents" => {
            let args: ListArgs = parse(args)?;
            let realm = realm(&args.realm)?;
            // Queued creates first: they are the newest, and a person who just
            // saved one expects to see it at the top.
            let mut out = write_commands::pending_documents(store, &realm, args.doc_type)?;
            let rows = store.list_documents(&realm, args.doc_type, args.limit.clamp(1, MAX_ROWS))?;
            out.extend(rows.into_iter().map(DocumentDto::from));
            to_json(out)
        }
        "document" => {
            let args: DocumentArgs = parse(args)?;
            let realm = realm(&args.realm)?;
            let open_writes = write_commands::open_writes(store, &realm, &args.qbo_id)?;
            if args.qbo_id.starts_with(crate::worker::LOCAL_ID_PREFIX) {
                // Not in QBO yet: shown from its queued payload, honestly marked.
                let record = store
                    .outbox_records(&realm, &[crate::outbox::OutboxState::Pending, crate::outbox::OutboxState::InFlight,
                        crate::outbox::OutboxState::Conflicted, crate::outbox::OutboxState::Rejected])?
                    .into_iter()
                    .find(|r| r.local_entity_id == args.qbo_id)
                    .ok_or(ApiError::NotFound)?;
                let (document, lines) = write_commands::pending_document(store, &realm, &record)?.ok_or(ApiError::NotFound)?;
                return to_json(DocumentDetailDto { related: vec![], links: vec![], unresolved: vec![], document, lines, open_writes });
            }
            let document = store
                .get_document(&realm, &args.qbo_id)?
                .ok_or(ApiError::NotFound)?;
            let lines = store.document_lines(&realm, &args.qbo_id)?;
            let lineage = store.lineage(&realm, &args.qbo_id, 6)?;
            to_json(DocumentDetailDto {
                document: document.into(),
                lines: lines.into_iter().map(LineDto::from).collect(),
                related: lineage.documents.into_iter().map(DocumentDto::from).collect(),
                links: lineage
                    .edges
                    .into_iter()
                    .map(|edge| LinkDto {
                        from_qbo_id: edge.from_qbo_id,
                        from_type: edge.from_type,
                        to_qbo_id: edge.to_qbo_id,
                        to_type: edge.to_type,
                    })
                    .collect(),
                unresolved: lineage.unresolved,
                open_writes,
            })
        }
        "contactDocuments" => {
            let args: ContactArgs = parse(args)?;
            let rows = store.documents_for_contact(
                &realm(&args.realm)?,
                args.contact_type,
                &args.contact_id,
                args.limit.clamp(1, MAX_ROWS),
            )?;
            to_json(rows.into_iter().map(DocumentDto::from).collect::<Vec<_>>())
        }
        "classes" => {
            let args: RealmArgs = parse(args)?;
            let map: serde_json::Map<String, Value> = store
                .class_names(&realm(&args.realm)?)?
                .into_iter()
                .map(|(id, name)| (id, Value::String(name)))
                .collect();
            Ok(Value::Object(map))
        }
        "aging" => {
            let args: AgingArgs = parse(args)?;
            let side = match args.side.as_str() {
                "receivable" => AgingSide::Receivable,
                "payable" => AgingSide::Payable,
                other => return Err(ApiError::BadArguments(format!("side must be receivable or payable, not {other:?}"))),
            };
            let as_of = match args.as_of {
                Some(raw) => date_arg(&raw)?,
                None => chrono::Local::now().date_naive().to_string(),
            };
            let rows = store.aging(&realm(&args.realm)?, side, &as_of)?;
            to_json(AgingDto {
                as_of,
                rows: rows
                    .into_iter()
                    .map(|r| AgingRowDto {
                        contact_id: r.contact_id,
                        contact_name: r.contact_name,
                        current_minor: r.current.minor(),
                        days1to30_minor: r.days_1_30.minor(),
                        days31to60_minor: r.days_31_60.minor(),
                        days61to90_minor: r.days_61_90.minor(),
                        over90_minor: r.days_over_90.minor(),
                        total_minor: r.total.minor(),
                        documents: r.documents,
                    })
                    .collect(),
            })
        }
        "salesByCustomer" => {
            let args: RangeArgs = parse(args)?;
            let rows = store.sales_by_customer(&realm(&args.realm)?, &date_arg(&args.from)?, &date_arg(&args.to)?)?;
            to_json(
                rows.into_iter()
                    .map(|r| serde_json::json!({
                        "contactId": r.contact_id,
                        "contactName": r.contact_name,
                        "netSalesMinor": r.net_sales.minor(),
                        "documents": r.documents,
                    }))
                    .collect::<Vec<_>>(),
            )
        }
        "items" => {
            let args: RealmArgs = parse(args)?;
            to_json(
                store
                    .items(&realm(&args.realm)?)?
                    .into_iter()
                    .map(|i| serde_json::json!({
                        "qboId": i.qbo_id, "name": i.name, "sku": i.sku, "itemType": i.item_type,
                        "unitPrice": i.unit_price, "purchaseCost": i.purchase_cost,
                        "qtyOnHand": i.qty_on_hand, "active": i.is_active, "usedOn": i.used_on,
                    }))
                    .collect::<Vec<_>>(),
            )
        }
        "itemDocuments" => {
            let args: ItemArgs = parse(args)?;
            let rows = store.documents_for_item(&realm(&args.realm)?, &args.item_id, MAX_ROWS)?;
            to_json(rows.into_iter().map(DocumentDto::from).collect::<Vec<_>>())
        }
        other => Err(ApiError::UnknownCommand(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::demo::{self, DemoSize, DEMO_REALM};

    fn book() -> Store {
        let store = Store::open_in_memory().unwrap();
        let size = DemoSize { customers: 15, vendors: 4, items: 20, invoices: 80, purchase_orders: 20 };
        demo::seed(&store, size, 3, Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()).unwrap();
        store
    }

    #[test]
    fn realms_are_listed_and_read_only() {
        let value = dispatch(&book(), "realms", Value::Null).unwrap();
        assert_eq!(value[0]["realmId"], DEMO_REALM);
        assert_eq!(value[0]["writeEnabled"], false);
    }

    #[test]
    fn a_register_lists_invoices_with_exact_money() {
        let value = dispatch(&book(), "documents", json!({ "realm": DEMO_REALM, "docType": "Invoice" })).unwrap();
        let rows = value.as_array().unwrap();
        assert_eq!(rows.len(), 80);
        assert!(rows[0]["totalMinor"].is_i64(), "money crosses the wire as integers");
        assert!(rows[0]["contactName"].is_string(), "the customer is joined, not looked up per row");
    }

    #[test]
    fn a_document_carries_its_lines_and_chain() {
        let store = book();
        let invoices = dispatch(&store, "documents", json!({ "realm": DEMO_REALM, "docType": "Invoice" })).unwrap();
        // A paid invoice links to its payment, so its chain has at least two documents.
        let paid = invoices
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["balanceMinor"] == 0)
            .expect("the demo book has paid invoices");
        let detail = dispatch(&store, "document", json!({ "realm": DEMO_REALM, "qboId": paid["qboId"] })).unwrap();
        assert!(!detail["lines"].as_array().unwrap().is_empty());
        assert!(detail["related"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn search_labels_why_a_number_matched() {
        let value = dispatch(&book(), "search", json!({ "realm": DEMO_REALM, "query": "9005" })).unwrap();
        assert_eq!(value["hits"][0]["kind"], "document");
        assert_eq!(value["hits"][0]["reason"], "number");
        assert!(value["elapsedMicros"].is_u64());
    }

    #[test]
    fn status_reports_counts_and_the_write_gate() {
        let value = dispatch(&book(), "status", json!({ "realm": DEMO_REALM })).unwrap();
        assert_eq!(value["writeEnabled"], false);
        assert_eq!(value["outboxPending"], 0);
        assert!(value["entityTypes"].as_array().unwrap().iter().any(|t| t["entityType"] == "Invoice"));
    }

    #[test]
    fn classes_map_ids_to_names() {
        let value = dispatch(&book(), "classes", json!({ "realm": DEMO_REALM })).unwrap();
        assert_eq!(value["2"], "Foam Fabrication");
    }

    #[test]
    fn aging_buckets_sum_to_the_total() {
        let value = dispatch(&book(), "aging", json!({ "realm": DEMO_REALM, "side": "receivable", "asOf": "2026-10-01" })).unwrap();
        assert_eq!(value["asOf"], "2026-10-01");
        let rows = value["rows"].as_array().unwrap();
        assert!(!rows.is_empty(), "the demo book has open invoices");
        for row in rows {
            let parts: i64 = ["currentMinor", "days1to30Minor", "days31to60Minor", "days61to90Minor", "over90Minor"]
                .iter()
                .map(|k| row[*k].as_i64().unwrap())
                .sum();
            assert_eq!(parts, row["totalMinor"].as_i64().unwrap());
        }
        assert_eq!(
            dispatch(&book(), "aging", json!({ "realm": DEMO_REALM, "side": "sideways" })).unwrap_err().status(),
            400
        );
        assert_eq!(
            dispatch(&book(), "aging", json!({ "realm": DEMO_REALM, "side": "payable", "asOf": "10/01/2026" })).unwrap_err().status(),
            400
        );
    }

    #[test]
    fn items_and_their_documents_are_listed() {
        let store = book();
        let items = dispatch(&store, "items", json!({ "realm": DEMO_REALM })).unwrap();
        let used = items.as_array().unwrap().iter().find(|i| i["usedOn"].as_i64().unwrap() > 0).unwrap();
        let docs = dispatch(&store, "itemDocuments", json!({ "realm": DEMO_REALM, "itemId": used["qboId"] })).unwrap();
        assert_eq!(docs.as_array().unwrap().len() as i64, used["usedOn"].as_i64().unwrap());
    }

    #[test]
    fn errors_are_typed_for_the_transport() {
        let store = book();
        let missing = dispatch(&store, "document", json!({ "realm": DEMO_REALM, "qboId": "nope" })).unwrap_err();
        assert_eq!(missing.status(), 404);
        let bad = dispatch(&store, "documents", json!({ "realm": "x" })).unwrap_err();
        assert_eq!(bad.status(), 400);
        assert_eq!(dispatch(&store, "deleteEverything", Value::Null).unwrap_err().status(), 404);
    }

    #[test]
    fn every_listed_command_dispatches() {
        let store = book();
        let args = json!({ "realm": DEMO_REALM, "query": "harbor", "docType": "Bill",
                           "qboId": "1001", "contactType": "Customer", "contactId": "1005",
                           "side": "payable", "from": "2026-01-01", "to": "2026-12-31", "itemId": "1030" });
        for command in COMMANDS {
            match dispatch(&store, command, args.clone()) {
                Ok(_) | Err(ApiError::NotFound) => {}
                Err(other) => panic!("{command}: {other}"),
            }
        }
    }
}
