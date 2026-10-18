//! The UI's command surface. Read-only in M1.
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
use crate::store::{DocumentRow, LineRow, Store, StoreError};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unknown command {0:?}")]
    UnknownCommand(String),
    #[error("bad arguments: {0}")]
    BadArguments(String),
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            ApiError::UnknownCommand(_) | ApiError::NotFound => 404,
            ApiError::BadArguments(_) => 400,
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
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HitDto {
    #[serde(rename_all = "camelCase")]
    Document { reason: String, document: DocumentDto },
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

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The commands, by name. Kept as a list so the dev server and the tests can
/// enumerate them; there is deliberately nothing here that writes.
pub const COMMANDS: &[&str] = &[
    "realms",
    "status",
    "search",
    "documents",
    "document",
    "contactDocuments",
    "classes",
];

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value, ApiError> {
    match command {
        "realms" => to_json(
            store
                .realms()?
                .into_iter()
                .map(|row| RealmDto {
                    realm_id: row.realm_id.to_string(),
                    display_name: row.display_name,
                    write_enabled: row.is_write_enabled,
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
                                document: (*row).into(),
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
            let rows = store.list_documents(&realm(&args.realm)?, args.doc_type, args.limit.clamp(1, MAX_ROWS))?;
            to_json(rows.into_iter().map(DocumentDto::from).collect::<Vec<_>>())
        }
        "document" => {
            let args: DocumentArgs = parse(args)?;
            let realm = realm(&args.realm)?;
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
                           "qboId": "1001", "contactType": "Customer", "contactId": "1005" });
        for command in COMMANDS {
            match dispatch(&store, command, args.clone()) {
                Ok(_) | Err(ApiError::NotFound) => {}
                Err(other) => panic!("{command}: {other}"),
            }
        }
    }
}
