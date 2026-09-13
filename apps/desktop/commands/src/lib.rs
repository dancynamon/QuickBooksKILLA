//! The whole body of the Mac session's `#[tauri::command]`. `ROADMAP.md` §B1,
//! `apps/desktop/README.md`.
//!
//! Tauri commands are ordinary functions the frontend reaches with
//! `invoke("query", { tool, args })` (`js/data/tauri.js`), and the read-only
//! query API this app needs already exists twice over: as typed methods on
//! [`qbo_local::store::Store`], and as an MCP `tools/call` surface over
//! exactly those methods
//! ([`qbo_local::mcp`], `docs/MCP.md`). [`query`] does not re-open that
//! surface — it drives it, the same way any other MCP client would, so this
//! crate carries no tool dispatch, no argument parsing and no JSON-shape
//! duplication of its own to drift from `mcp.rs`. The Tauri command the Mac
//! session writes is then a signature and one call into this function; the
//! logic already exists and is already tested.
//!
//! Depends on `qbo-local` only (`ROADMAP.md` §B1) — no `tauri` dependency
//! here, so this crate builds and tests on any machine, including this one,
//! which has neither the Tauri toolchain nor the system libraries it needs.

use qbo_local::mcp::Server;
use qbo_local::store::Store;
use serde_json::Value;

/// Everything that can go wrong turning one `(tool, args)` pair into a
/// result: the tool call itself was refused (`isError: true` on the MCP
/// response — a bad `realm_id`, an id that does not exist, an unparseable
/// date), or the response shape coming back from [`Server::handle_line`]
/// was not what a `tools/call` result looks like, which would mean the two
/// crates have drifted apart rather than that the query was bad.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The tool ran and said no. `message` is exactly the text MCP would
    /// have shown a caller reading `content[0].text` — nothing is
    /// reformatted, because that text is already written for a reader, not
    /// for a machine.
    #[error("{message}")]
    Tool { message: String },

    /// [`Server::handle_line`] returned nothing, or something that is not a
    /// well-formed JSON-RPC response to the request this function sent. A
    /// request built here is always a request with an `id`, so a `None`
    /// reply is already a contract violation, not a notification.
    #[error("qbo-local-mcp gave no response to a tools/call request")]
    NoResponse,

    /// The JSON-RPC response carried a top-level `error` — a transport
    /// fault (`ROADMAP.md`/`docs/MCP.md`: malformed request, unknown
    /// method), never a query outcome. Reaching this means `query` built a
    /// request `handle_line` could not parse as one of its own methods,
    /// which is this crate's bug, not the caller's.
    #[error("qbo-local-mcp protocol error {code}: {message}")]
    Protocol { code: i64, message: String },

    /// A `tools/call` response with neither `isError` nor `structuredContent`
    /// — not a shape `mcp.rs` produces today. Surfaced rather than panicking,
    /// because a future MCP server change should fail one query, not the
    /// whole desktop app.
    #[error("qbo-local-mcp returned a tools/call result with no structuredContent")]
    MalformedResult,
}

/// Run one read tool against `store` and return its `structuredContent` —
/// exactly the JSON shape `docs/MCP.md` documents for that tool, the same
/// value `apps/desktop/ui/js/data/tauri.js` expects back from `invoke`.
///
/// `tool` is one of the eleven names in [`qbo_local::mcp::tools`] —
/// `"search"`, `"document_detail"`, `"list_documents"`, `"open_documents"`,
/// `"contact_detail"`, `"item_detail"`, `"ar_aging"`, `"ap_aging"`,
/// `"sync_status"`, `"class_tree"`, `"chart_of_accounts"` — and `args` is
/// that tool's argument object, `realm_id` included; both are passed through
/// verbatim to `tools/call`; an unknown name comes back as
/// [`CommandError::Tool`], the same as it would over MCP.
pub fn query(store: &Store, tool: &str, args: Value) -> Result<Value, CommandError> {
    let mut server = Server::new(store);

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args },
    });
    let line = request.to_string();

    let response_line = server.handle_line(&line).ok_or(CommandError::NoResponse)?;
    let response: Value =
        serde_json::from_str(&response_line).map_err(|err| CommandError::Protocol {
            code: -32700,
            message: format!("could not parse qbo-local-mcp's own response: {err}"),
        })?;

    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown protocol error")
            .to_string();
        return Err(CommandError::Protocol { code, message });
    }

    let result = response.get("result").ok_or(CommandError::MalformedResult)?;

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("the tool call failed")
            .to_string();
        return Err(CommandError::Tool { message });
    }

    result
        .get("structuredContent")
        .cloned()
        .ok_or(CommandError::MalformedResult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use qbo_local::domain::{EntityType, RealmId};
    use qbo_local::store::MirroredEntity;
    use serde_json::json;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap()
    }

    fn mirrored(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
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

    /// A store seeded like `apps/qbo-local/tests/query.rs`: one customer and
    /// one invoice against them, enough for `document_detail`,
    /// `contact_detail` and `sync_status` to each have something real to
    /// answer with.
    fn seeded_store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test Co", now()).unwrap();
        seed(
            &store,
            mirrored(
                EntityType::Customer,
                "31",
                json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
            ),
        );
        seed(
            &store,
            mirrored(
                EntityType::Invoice,
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
        store
    }

    #[test]
    fn document_detail_round_trips_through_the_json_rpc_boundary() {
        let store = seeded_store();
        let result = query(
            &store,
            "document_detail",
            json!({ "realm_id": realm().as_str(), "qbo_id": "418" }),
        )
        .unwrap();

        assert_eq!(result["document"]["qbo_id"], json!("418"));
        assert_eq!(result["document"]["total"], json!("1234.56"));
        assert_eq!(result["lines"][0]["item_id"], json!("12"));
    }

    #[test]
    fn contact_detail_round_trips_through_the_json_rpc_boundary() {
        let store = seeded_store();
        let result = query(
            &store,
            "contact_detail",
            json!({ "realm_id": realm().as_str(), "contact_type": "customer", "qbo_id": "31" }),
        )
        .unwrap();

        assert_eq!(
            result["contact"]["display_name"],
            json!("Blue Harbor Swim Club")
        );
        assert_eq!(result["open_documents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sync_status_round_trips_through_the_json_rpc_boundary() {
        let store = seeded_store();
        let result = query(
            &store,
            "sync_status",
            json!({ "realm_id": realm().as_str() }),
        )
        .unwrap();

        assert_eq!(result["write_enabled"], json!(false));
        let invoice = result["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["entity_type"] == json!("Invoice"))
            .unwrap();
        assert_eq!(invoice["mirrored"], json!(1));
    }

    #[test]
    fn a_tool_level_failure_becomes_a_command_error_not_a_panic() {
        let store = seeded_store();
        let err = query(
            &store,
            "document_detail",
            json!({ "realm_id": realm().as_str(), "qbo_id": "does-not-exist" }),
        )
        .unwrap_err();

        match err {
            CommandError::Tool { message } => assert!(message.contains("does-not-exist")),
            other => panic!("expected CommandError::Tool, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error() {
        let store = seeded_store();
        let err = query(
            &store,
            "not_a_real_tool",
            json!({ "realm_id": realm().as_str() }),
        )
        .unwrap_err();

        match err {
            CommandError::Tool { message } => assert!(message.contains("not_a_real_tool")),
            other => panic!("expected CommandError::Tool, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_realm_id_is_a_tool_error_not_a_protocol_error() {
        let store = seeded_store();
        let err = query(&store, "sync_status", json!({})).unwrap_err();
        assert!(matches!(err, CommandError::Tool { .. }));
    }
}
