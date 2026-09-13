//! MCP server over the read-only query API. `ROADMAP.md` §G ("Channel
//! intake"): Dan's existing Claude skills talk to QuickBooks today through
//! Intuit's own MCP server; this is the same protocol over this replica
//! instead, so those skills repoint here without being rewritten, and every
//! other skill that only reads QBO keeps working after cutover. Document
//! writes are Phase D — everything here is read-only, over
//! [`crate::store::query`], [`crate::store::search`] and
//! [`crate::store::lineage`].
//!
//! The JSON-RPC 2.0 / MCP stdio transport itself — line framing, error
//! codes, `initialize`/`initialized`/`ping`/`tools/list`/`tools/call` — lives
//! in [`mcp_stdio`] and is shared with `ledger-mcp` (`docs/MCP.md`). This
//! module is [`QboToolSet`]: the [`mcp_stdio::ToolSet`] impl that turns a
//! `tools/call` into a query against [`crate::store`], plus [`Server`], a
//! thin wrapper so `bin/qbo-local-mcp.rs` and every existing caller of
//! `qbo_local::mcp::Server` keep the same two methods they always had.
//!
//! Read-only by construction, not by convention:
//! [`crate::store::Store::open_read_only`] opens the replica with SQLite's own
//! `SQLITE_OPEN_READ_ONLY` flag, so a bug here cannot become a write path.

use chrono::NaiveDate;
use serde_json::{json, Value};

use mcp_stdio::{ServerInfo, ToolSet};
pub use mcp_stdio::{ToolError, ToolSpec, PROTOCOL_VERSION};

use crate::domain::{ContactType, DocumentType, EntityType, RealmId};
use crate::store::query::{Page, MAX_PAGE_LIMIT};
use crate::store::Store;

/// How many hits [`Store::search`] returns when a call omits `limit`. Small
/// enough that an LLM caller reading the text content is not skimming past
/// noise, generous enough that the ranked stages (`DESIGN.md` §3.3) rarely
/// need a second call.
const DEFAULT_SEARCH_LIMIT: usize = 20;

/// The [`mcp_stdio::ToolSet`] over one open replica.
struct QboToolSet {
    store: Store,
}

impl ToolSet for QboToolSet {
    fn tools(&self) -> Vec<ToolSpec> {
        tools()
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        self.call_tool(name, args)
    }
}

/// The server: one open replica, spoken to over JSON-RPC 2.0 via
/// [`mcp_stdio::Server`]. A thin wrapper — `new` and `handle_line` are the
/// entire public surface, unchanged from before this module was built on the
/// shared transport crate.
pub struct Server {
    inner: mcp_stdio::Server<QboToolSet>,
}

impl Server {
    pub fn new(store: Store) -> Self {
        let info = ServerInfo {
            name: "qbo-local",
            version: env!("CARGO_PKG_VERSION"),
        };
        Server {
            inner: mcp_stdio::Server::new(QboToolSet { store }, info),
        }
    }

    /// Handle one line of input and produce at most one line of output. See
    /// [`mcp_stdio::Server::handle_line`] for the framing and notification
    /// rules this delegates to.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        self.inner.handle_line(line)
    }
}

impl QboToolSet {
    /// Dispatch one `tools/call` to the query API and serialise its result.
    ///
    /// Every tool takes `realm_id` — checked first, uniformly, so a bad or
    /// missing realm never reaches the store call it would otherwise fail
    /// inside of with a less specific message.
    fn call_tool(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let realm = parse_realm(&args)?;

        let result = match name {
            "search" => {
                let query = required_str(&args, "query")?;
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|limit| limit as usize)
                    .unwrap_or(DEFAULT_SEARCH_LIMIT)
                    .min(MAX_PAGE_LIMIT);
                to_value(self.store.search(&realm, query, limit))?
            }
            "document_detail" => {
                let qbo_id = required_str(&args, "qbo_id")?;
                let detail = self
                    .store
                    .document_detail(&realm, qbo_id)
                    .map_err(store_err)?
                    .ok_or_else(|| not_found("document", qbo_id))?;
                to_value(Ok(detail))?
            }
            "list_documents" => {
                let doc_type = parse_doc_type(&args)?;
                let from = parse_optional_date(&args, "from")?.unwrap_or_else(earliest_date);
                let to = parse_optional_date(&args, "to")?.unwrap_or_else(latest_date);
                let page = parse_page(&args);
                to_value(
                    self.store
                        .documents_in_range(&realm, Some(doc_type), from, to, page),
                )?
            }
            "open_documents" => {
                let doc_type = parse_doc_type(&args)?;
                let page = parse_page(&args);
                to_value(self.store.open_documents(&realm, doc_type, page))?
            }
            "contact_detail" => {
                let contact_type = parse_contact_type(&args)?;
                let qbo_id = required_str(&args, "qbo_id")?;
                let detail = self
                    .store
                    .contact_detail(&realm, contact_type, qbo_id)
                    .map_err(store_err)?
                    .ok_or_else(|| not_found("contact", qbo_id))?;
                to_value(Ok(detail))?
            }
            "item_detail" => {
                let qbo_id = required_str(&args, "qbo_id")?;
                let detail = self
                    .store
                    .item_detail(&realm, qbo_id)
                    .map_err(store_err)?
                    .ok_or_else(|| not_found("item", qbo_id))?;
                to_value(Ok(detail))?
            }
            "ar_aging" => {
                let as_of = parse_date(&args, "as_of")?;
                to_value(self.store.ar_aging(&realm, as_of))?
            }
            "ap_aging" => {
                let as_of = parse_date(&args, "as_of")?;
                to_value(self.store.ap_aging(&realm, as_of))?
            }
            "sync_status" => to_value(self.store.sync_status(&realm))?,
            "class_tree" => to_value(self.store.class_tree(&realm))?,
            "chart_of_accounts" => to_value(self.store.chart_of_accounts(&realm))?,
            other => return Err(ToolError::new(format!("unknown tool: {other}"))),
        };

        Ok(result)
    }
}

/// Every tool this server exposes. A free function, not a method — the list
/// does not depend on an open store, and `tools/list` needs no realm.
pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "search",
            description: "Search this realm's mirrored QuickBooks data. Tries exact identifiers \
                first — a document number, the customer's PO number, an item SKU, a dollar \
                amount — before falling back to full text across contacts, items, memos and line \
                descriptions, so typing a known number lands on that record rather than a ranked \
                guess. Results come back best-match-first, each one tagged with which of those \
                reasons it matched on.",
            input_schema: tool_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "What to search for: a document number, PO number, SKU, \
                            dollar amount, or free text.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_PAGE_LIMIT,
                        "description": "Maximum hits to return. Defaults to 20.",
                    },
                }),
                &["query"],
            ),
        },
        ToolSpec {
            name: "document_detail",
            description: "Fetch one document — invoice, bill, estimate, sales receipt, purchase \
                order, credit memo, payment, and so on — by its QuickBooks id. Returns the \
                document header, every line item, and its lineage: the other documents it links \
                to or was built from, such as the estimate an invoice came from or the bills a \
                bill payment settles.",
            input_schema: tool_schema(
                json!({
                    "qbo_id": {
                        "type": "string",
                        "description": "The document's QuickBooks id (not its doc number).",
                    },
                }),
                &["qbo_id"],
            ),
        },
        ToolSpec {
            name: "list_documents",
            description: "List documents of one type — invoices, bills, estimates, and so on — \
                whose transaction date falls within an optional range, newest first, paged with \
                offset/limit. Use this for a register or an activity feed; omit `from`/`to` for \
                the full mirrored history.",
            input_schema: tool_schema(
                json!({
                    "doc_type": doc_type_schema(),
                    "from": {
                        "type": "string",
                        "description": "Earliest transaction date to include, YYYY-MM-DD. \
                            Omit for no lower bound.",
                    },
                    "to": {
                        "type": "string",
                        "description": "Latest transaction date to include, YYYY-MM-DD. Omit \
                            for no upper bound.",
                    },
                    "offset": offset_schema(),
                    "limit": limit_schema(),
                }),
                &["doc_type"],
            ),
        },
        ToolSpec {
            name: "open_documents",
            description: "List documents of one type that are still open, newest first, paged \
                with offset/limit — the AR/AP work queue. Open means carrying a balance, or, for \
                purchase orders and estimates, holding an open QBO status (`Open`, `Pending` or \
                `Accepted`) even at a zero balance: unpaid invoices, unbilled purchase orders, \
                estimates still pending or awaiting a final invoice.",
            input_schema: tool_schema(
                json!({
                    "doc_type": doc_type_schema(),
                    "offset": offset_schema(),
                    "limit": limit_schema(),
                }),
                &["doc_type"],
            ),
        },
        ToolSpec {
            name: "contact_detail",
            description: "Fetch one customer or vendor by its QuickBooks id: their contact info \
                and balance, every document still carrying a balance against them, and their 50 \
                most recent documents regardless of balance.",
            input_schema: tool_schema(
                json!({
                    "contact_type": {
                        "type": "string",
                        "enum": ["customer", "vendor"],
                        "description": "Which side of the book the id belongs to. Customer and \
                            vendor ids are separate spaces in QuickBooks, so this is required.",
                    },
                    "qbo_id": {
                        "type": "string",
                        "description": "The customer's or vendor's QuickBooks id.",
                    },
                }),
                &["contact_type", "qbo_id"],
            ),
        },
        ToolSpec {
            name: "item_detail",
            description: "Fetch one item — a product or service on the price list — by its \
                QuickBooks id: its price, cost, quantity on hand, every document that has used \
                it, and the total quantity sold across invoices and sales receipts (purchases \
                and other non-sales lines do not count toward units sold).",
            input_schema: tool_schema(
                json!({
                    "qbo_id": {
                        "type": "string",
                        "description": "The item's QuickBooks id.",
                    },
                }),
                &["qbo_id"],
            ),
        },
        ToolSpec {
            name: "ar_aging",
            description: "Accounts-receivable aging as of a given date: every customer with an \
                open invoice balance, bucketed into current, 1-30, 31-60, 61-90 and over-90 days \
                past due, plus realm-wide totals per bucket. An invoice with no due date falls \
                back to its transaction date rather than being excluded.",
            input_schema: tool_schema(
                json!({
                    "as_of": {
                        "type": "string",
                        "description": "The date to age against, YYYY-MM-DD.",
                    },
                }),
                &["as_of"],
            ),
        },
        ToolSpec {
            name: "ap_aging",
            description: "Accounts-payable aging as of a given date — the vendor-facing mirror \
                of ar_aging, bucketing open bill balances by days past due instead of open \
                invoice balances.",
            input_schema: tool_schema(
                json!({
                    "as_of": {
                        "type": "string",
                        "description": "The date to age against, YYYY-MM-DD.",
                    },
                }),
                &["as_of"],
            ),
        },
        ToolSpec {
            name: "sync_status",
            description: "Report this realm's sync health: whether writes are enabled, and per \
                entity type how many records are mirrored, the last CDC cursor and full-sweep \
                timestamps, and how many records are quarantined for failing to parse.",
            input_schema: tool_schema(json!({}), &[]),
        },
        ToolSpec {
            name: "class_tree",
            description: "List every QBO class in this realm — the per-product-line dimension \
                (foam, signs, chairs, CNC work) — with its parent id, for building the \
                hierarchy.",
            input_schema: tool_schema(json!({}), &[]),
        },
        ToolSpec {
            name: "chart_of_accounts",
            description: "List every account in this realm's chart of accounts, with its \
                number, type, subtype, classification and current balance.",
            input_schema: tool_schema(json!({}), &[]),
        },
    ]
}

/// Build a tool's JSON Schema, adding the `realm_id` property and requirement
/// every tool shares so no tool spec has to repeat it.
fn tool_schema(properties: Value, required: &[&str]) -> Value {
    let mut properties = properties
        .as_object()
        .cloned()
        .expect("tool schema properties must be a JSON object");
    properties.insert(
        "realm_id".to_string(),
        json!({
            "type": "string",
            "description": "The QBO realm (company) id — a numeric string. Aquamentor and \
                WaterLine CNC are separate realms sharing nothing; every call is scoped to \
                exactly one.",
        }),
    );

    let mut required: Vec<Value> = required.iter().map(|name| json!(*name)).collect();
    required.push(json!("realm_id"));

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false,
    })
}

fn doc_type_schema() -> Value {
    json!({
        "type": "string",
        "enum": doc_type_names(),
        "description": "The QuickBooks document type, e.g. \"Invoice\" or \"Bill\".",
    })
}

fn offset_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "description": "How many matching rows to skip. Defaults to 0.",
    })
}

fn limit_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_PAGE_LIMIT,
        "description": format!(
            "Maximum rows to return. Defaults to 200, capped at {MAX_PAGE_LIMIT} regardless of \
             what is asked for."
        ),
    })
}

/// Every [`DocumentType`] name, in [`EntityType::ALL`]'s order — reusing the
/// canonical list rather than a second hand-kept copy of the thirteen names.
fn doc_type_names() -> Vec<&'static str> {
    EntityType::ALL
        .iter()
        .filter_map(|entity| entity.as_document())
        .map(DocumentType::as_str)
        .collect()
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::new(format!("missing or invalid \"{key}\"")))
}

fn parse_realm(args: &Value) -> Result<RealmId, ToolError> {
    let raw = required_str(args, "realm_id")?;
    RealmId::parse(raw).map_err(|err| ToolError::new(format!("invalid realm_id: {err}")))
}

fn parse_doc_type(args: &Value) -> Result<DocumentType, ToolError> {
    let raw = required_str(args, "doc_type")?;
    EntityType::parse(raw)
        .ok()
        .and_then(EntityType::as_document)
        .ok_or_else(|| ToolError::new(format!("unknown doc_type: {raw:?}")))
}

/// `contact_type` is spelled lower-case on the wire (`"customer"` /
/// `"vendor"`) — the friendlier form for a tool schema an LLM fills in —
/// rather than [`ContactType::parse`]'s exact-case QBO spelling, which stays
/// reserved for the string actually stored in the `contacts` table.
fn parse_contact_type(args: &Value) -> Result<ContactType, ToolError> {
    let raw = required_str(args, "contact_type")?;
    match raw.to_ascii_lowercase().as_str() {
        "customer" => Ok(ContactType::Customer),
        "vendor" => Ok(ContactType::Vendor),
        _ => Err(ToolError::new(format!(
            "contact_type must be \"customer\" or \"vendor\", got {raw:?}"
        ))),
    }
}

fn parse_date(args: &Value, key: &str) -> Result<NaiveDate, ToolError> {
    let raw = required_str(args, key)?;
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| ToolError::new(format!("invalid {key}: expected YYYY-MM-DD, got {raw:?}")))
}

fn parse_optional_date(args: &Value, key: &str) -> Result<Option<NaiveDate>, ToolError> {
    match args.get(key).and_then(Value::as_str) {
        Some(raw) => NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| {
                ToolError::new(format!("invalid {key}: expected YYYY-MM-DD, got {raw:?}"))
            }),
        None => Ok(None),
    }
}

fn parse_page(args: &Value) -> Page {
    let default = Page::default();
    Page {
        offset: args
            .get("offset")
            .and_then(Value::as_u64)
            .map(|offset| offset as usize)
            .unwrap_or(default.offset),
        limit: args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|limit| limit as usize)
            .unwrap_or(default.limit),
    }
}

/// Stand-in bounds for "no lower/upper bound was given" on `list_documents`'s
/// date range — `documents_in_range` takes two closed [`NaiveDate`]s, not
/// `Option`s, so an omitted `from`/`to` widens to these instead of every
/// caller re-deriving "the whole history" its own way.
fn earliest_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1900, 1, 1).expect("1900-01-01 is a valid date")
}

fn latest_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).expect("9999-12-31 is a valid date")
}

fn not_found(kind: &str, qbo_id: &str) -> ToolError {
    ToolError::new(format!("no {kind} {qbo_id:?} in this realm"))
}

fn store_err(err: crate::store::StoreError) -> ToolError {
    ToolError::new(format!("store error: {err}"))
}

fn to_value<T: serde::Serialize>(
    result: Result<T, crate::store::StoreError>,
) -> Result<Value, ToolError> {
    let value = result.map_err(store_err)?;
    Ok(serde_json::to_value(value).expect("a typed store result always serialises"))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::EntityType as ET;
    use crate::store::{MirroredEntity, Store};

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap()
    }

    fn mirrored(entity_type: ET, id: &str, raw: Value) -> MirroredEntity {
        MirroredEntity {
            entity_type,
            qbo_id: id.to_string(),
            sync_token: "1".into(),
            last_updated_utc: now(),
            is_deleted: false,
            raw_json: raw,
        }
    }

    fn seed(store: &Store, entity: MirroredEntity) {
        store.upsert_entity(&realm(), &entity, now()).unwrap();
        store.project_entity(&realm(), &entity, now()).unwrap();
    }

    /// A server over a store that already carries one customer and one open
    /// invoice against them — enough for every tool that takes a `qbo_id` to
    /// have something real to find.
    fn seeded_server() -> Server {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test Co", now()).unwrap();
        seed(
            &store,
            mirrored(
                ET::Customer,
                "31",
                json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
            ),
        );
        seed(
            &store,
            mirrored(
                ET::Invoice,
                "418",
                json!({
                    "Id": "418", "DocNumber": "1088", "TxnDate": "2026-07-14",
                    "CustomerRef": { "value": "31" }, "TotalAmt": 1234.56, "Balance": 1234.56,
                    "Line": [ {
                        "LineNum": 1, "Description": "Rescue tube, 50 inch, red",
                        "Amount": 1234.56, "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": { "ItemRef": { "value": "12" }, "Qty": 20 }
                    } ]
                }),
            ),
        );
        seed(
            &store,
            mirrored(
                ET::Item,
                "12",
                json!({ "Id": "12", "Name": "Rescue tube, 50 inch", "Sku": "RT-50-RED", "Active": true }),
            ),
        );
        Server::new(store)
    }

    fn call(server: &mut Server, id: i64, method: &str, params: Value) -> Value {
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let response = server
            .handle_line(&line)
            .expect("a request always gets a response");
        serde_json::from_str(&response).unwrap()
    }

    // -----------------------------------------------------------------------
    // Protocol shape
    // -----------------------------------------------------------------------

    #[test]
    fn initialize_echoes_a_supported_protocol_version_and_names_the_server() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "x", "version": "0" } }),
        );
        assert_eq!(response["id"], json!(1));
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(PROTOCOL_VERSION)
        );
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(response["result"]["serverInfo"]["name"], json!("qbo-local"));
        assert_eq!(
            response["result"]["serverInfo"]["version"],
            json!(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn initialize_states_its_own_version_when_the_client_asks_for_one_it_does_not_know() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        );
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(PROTOCOL_VERSION)
        );
    }

    #[test]
    fn ping_returns_an_empty_result() {
        let mut server = seeded_server();
        let response = call(&mut server, 1, "ping", json!({}));
        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn a_notification_gets_no_response() {
        let mut server = seeded_server();
        let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert_eq!(server.handle_line(&line), None);

        // Even a notification naming a real method gets nothing back — the
        // "no id, no reply" rule has no carve-out for a method that exists.
        let line = json!({ "jsonrpc": "2.0", "method": "ping" }).to_string();
        assert_eq!(server.handle_line(&line), None);
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_null_id() {
        let mut server = seeded_server();
        let response = server.handle_line("{ not json").unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], json!(-32700));
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let mut server = seeded_server();
        let response = call(&mut server, 7, "not/a/real/method", json!({}));
        assert_eq!(response["error"]["code"], json!(-32601));
        assert_eq!(response["id"], json!(7));
    }

    #[test]
    fn an_empty_line_is_silently_ignored() {
        let mut server = seeded_server();
        assert_eq!(server.handle_line(""), None);
        assert_eq!(server.handle_line("   "), None);
    }

    // -----------------------------------------------------------------------
    // tools/list
    // -----------------------------------------------------------------------

    #[test]
    fn tools_list_carries_every_tool_with_a_schema() {
        let mut server = seeded_server();
        let response = call(&mut server, 1, "tools/list", json!({}));
        let listed = response["result"]["tools"].as_array().unwrap();

        let expected = [
            "search",
            "document_detail",
            "list_documents",
            "open_documents",
            "contact_detail",
            "item_detail",
            "ar_aging",
            "ap_aging",
            "sync_status",
            "class_tree",
            "chart_of_accounts",
        ];
        assert_eq!(listed.len(), expected.len());
        for name in expected {
            let tool = listed
                .iter()
                .find(|tool| tool["name"] == json!(name))
                .unwrap_or_else(|| panic!("tools/list is missing {name:?}"));
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
            // Every tool takes realm_id, and requires it.
            assert!(tool["inputSchema"]["properties"]["realm_id"].is_object());
            let required = tool["inputSchema"]["required"].as_array().unwrap();
            assert!(required.contains(&json!("realm_id")));
        }
    }

    // -----------------------------------------------------------------------
    // tools/call — one happy path per tool, plus the error shapes
    // -----------------------------------------------------------------------

    #[test]
    fn an_unknown_tool_is_a_tool_error_not_a_json_rpc_error() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "not_a_tool", "arguments": { "realm_id": realm().as_str() } }),
        );
        assert!(
            response.get("error").is_none(),
            "must not be a JSON-RPC error"
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not_a_tool"));
    }

    #[test]
    fn a_missing_realm_id_is_a_tool_error() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "sync_status", "arguments": {} }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
    }

    #[test]
    fn search_finds_the_seeded_invoice_by_document_number() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "search",
                "arguments": { "realm_id": realm().as_str(), "query": "1088" },
            }),
        );
        assert_ne!(response["result"]["isError"], json!(true));
        let hits = response["result"]["structuredContent"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["reason"], json!("DocumentNumber"));
        assert_eq!(hits[0]["hit"]["kind"], json!("Document"));
        assert_eq!(hits[0]["hit"]["qbo_id"], json!("418"));
    }

    #[test]
    fn document_detail_returns_lines_and_renders_money_as_a_decimal_string() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "document_detail",
                "arguments": { "realm_id": realm().as_str(), "qbo_id": "418" },
            }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["document"]["qbo_id"], json!("418"));
        assert_eq!(result["document"]["total"], json!("1234.56"));
        assert_eq!(result["document"]["balance"], json!("1234.56"));
        assert_eq!(result["lines"][0]["item_id"], json!("12"));
    }

    #[test]
    fn document_detail_on_a_missing_id_is_a_tool_error() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "document_detail",
                "arguments": { "realm_id": realm().as_str(), "qbo_id": "does-not-exist" },
            }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("does-not-exist"));
    }

    #[test]
    fn list_documents_and_open_documents_return_the_seeded_invoice() {
        let mut server = seeded_server();

        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "list_documents",
                "arguments": { "realm_id": realm().as_str(), "doc_type": "Invoice" },
            }),
        );
        let rows = response["result"]["structuredContent"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["qbo_id"], json!("418"));

        let response = call(
            &mut server,
            2,
            "tools/call",
            json!({
                "name": "open_documents",
                "arguments": { "realm_id": realm().as_str(), "doc_type": "Invoice" },
            }),
        );
        let rows = response["result"]["structuredContent"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["qbo_id"], json!("418"));
    }

    #[test]
    fn contact_detail_splits_open_from_recent_and_accepts_lower_case_contact_type() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "contact_detail",
                "arguments": {
                    "realm_id": realm().as_str(), "contact_type": "customer", "qbo_id": "31"
                },
            }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(
            result["contact"]["display_name"],
            json!("Blue Harbor Swim Club")
        );
        assert_eq!(result["open_documents"].as_array().unwrap().len(), 1);

        let response = call(
            &mut server,
            2,
            "tools/call",
            json!({
                "name": "contact_detail",
                "arguments": {
                    "realm_id": realm().as_str(), "contact_type": "vendor", "qbo_id": "31"
                },
            }),
        );
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "31 is a customer, not a vendor"
        );
    }

    #[test]
    fn item_detail_reports_units_sold() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "item_detail",
                "arguments": { "realm_id": realm().as_str(), "qbo_id": "12" },
            }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["item"]["sku"], json!("RT-50-RED"));
        assert_eq!(result["units_sold"], json!("20"));
    }

    #[test]
    fn ar_aging_buckets_the_seeded_invoice_and_totals_match_the_rows() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "ar_aging",
                "arguments": { "realm_id": realm().as_str(), "as_of": "2026-09-11" },
            }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["rows"][0]["contact_id"], json!("31"));
        assert_eq!(result["rows"][0]["total"], json!("1234.56"));
        // No DueDate on the seeded invoice: falls back to its 2026-07-14
        // txn_date, 59 days before as_of — the 31-60 bucket.
        assert_eq!(result["totals"]["d31_60"], json!("1234.56"));
    }

    #[test]
    fn ap_aging_answers_with_no_bills_seeded() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "ap_aging",
                "arguments": { "realm_id": realm().as_str(), "as_of": "2026-09-11" },
            }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["rows"].as_array().unwrap().len(), 0);
        assert_eq!(result["totals"]["current"], json!("0.00"));
    }

    #[test]
    fn ar_aging_rejects_an_unparseable_date() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({
                "name": "ar_aging",
                "arguments": { "realm_id": realm().as_str(), "as_of": "not-a-date" },
            }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
    }

    #[test]
    fn sync_status_reports_write_disabled_and_the_seeded_mirror_counts() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "sync_status", "arguments": { "realm_id": realm().as_str() } }),
        );
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["write_enabled"], json!(false));
        assert_eq!(result["quarantined_total"], json!(0));
        let invoice = result["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entity| entity["entity_type"] == json!("Invoice"))
            .unwrap();
        assert_eq!(invoice["mirrored"], json!(1));
    }

    #[test]
    fn class_tree_and_chart_of_accounts_return_empty_lists_when_nothing_is_mirrored() {
        let mut server = seeded_server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "class_tree", "arguments": { "realm_id": realm().as_str() } }),
        );
        assert_eq!(response["result"]["structuredContent"], json!([]));

        let response = call(
            &mut server,
            2,
            "tools/call",
            json!({ "name": "chart_of_accounts", "arguments": { "realm_id": realm().as_str() } }),
        );
        assert_eq!(response["result"]["structuredContent"], json!([]));
    }

    // -----------------------------------------------------------------------
    // Money rendering (also covered end to end above, this is the direct case)
    // -----------------------------------------------------------------------

    #[test]
    fn money_renders_as_a_two_place_decimal_string_positive_and_negative() {
        use crate::domain::DocumentType;
        use crate::store::DocumentRow;
        use ledger_core::Money;

        let row = DocumentRow {
            qbo_id: "1".into(),
            doc_type: DocumentType::Invoice,
            doc_number: None,
            txn_date: "2026-01-01".into(),
            due_date: None,
            contact_id: None,
            contact_type: None,
            contact_name: None,
            class_id: None,
            total: Money::from_minor(123_456),
            balance: Some(Money::from_minor(-1_200)),
            doc_status: None,
            po_number: None,
            private_note: None,
            customer_memo: None,
            is_deleted: false,
        };

        let value = serde_json::to_value(&row).unwrap();
        assert_eq!(value["total"], json!("1234.56"));
        assert_eq!(value["balance"], json!("-12.00"));

        // Money's own Serialize impl is untouched: bare minor units.
        assert_eq!(
            serde_json::to_string(&Money::from_minor(123_456)).unwrap(),
            "123456"
        );
    }
}
