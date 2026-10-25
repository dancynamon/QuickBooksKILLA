//! The whole body of the Mac session's two `#[tauri::command]`s.
//! `ROADMAP.md` §B1, `apps/desktop/README.md`.
//!
//! Tauri commands are ordinary functions the frontend reaches with
//! `invoke("query", { tool, args })` (`js/data/tauri.js`) and
//! `invoke("ledger_query", { tool, args })` (`js/data/ledger/tauri.js`), and
//! both APIs this app needs already exist as an MCP `tools/call` surface —
//! the read-only replica's ([`qbo_local::mcp`], `docs/MCP.md`) and the
//! ledger's own reads and gated writes ([`ledger::mcp`], `docs/MCP.md`'s
//! ledger section). Neither [`query`] nor [`ledger_query`] re-opens that
//! surface — each drives it, the same way any other MCP client would, so
//! this crate carries no tool dispatch, no argument parsing and no
//! JSON-shape duplication of its own to drift from either `mcp.rs`. The
//! Tauri command the Mac session writes for each is then a signature and one
//! call into this crate; the logic already exists and is already tested.
//!
//! Depends on `qbo-local` and `ledger` only (`ROADMAP.md` §B1) — no `tauri`
//! dependency here, so this crate builds and tests on any machine, including
//! this one, which has neither the Tauri toolchain nor the system libraries
//! it needs.

use qbo_local::mcp::Server;
use qbo_local::store::Store;
use serde_json::Value;

use ledger::store::Ledger;

/// Everything that can go wrong turning one `(tool, args)` pair into a
/// result, for either [`query`] or [`ledger_query`]: the tool call itself
/// was refused (`isError: true` on the MCP response — a bad `realm_id`, an
/// id that does not exist, an unparseable date, a closed period), or the
/// response shape coming back from `Server::handle_line` was not what a
/// `tools/call` result looks like, which would mean this crate and the MCP
/// server it drives have drifted apart rather than that the query was bad.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The tool ran and said no. `message` is exactly the text MCP would
    /// have shown a caller reading `content[0].text` — nothing is
    /// reformatted, because that text is already written for a reader, not
    /// for a machine. This is also how a write tool's domain failures reach
    /// a screen: a closed period, a missing class, an unbalanced journal —
    /// `docs/MCP.md`'s ledger section's write gate never raises a JSON-RPC
    /// error for one of these, only this.
    #[error("{message}")]
    Tool { message: String },

    /// `Server::handle_line` returned nothing, or something that is not a
    /// well-formed JSON-RPC response to the request this function sent. A
    /// request built here is always a request with an `id`, so a `None`
    /// reply is already a contract violation, not a notification.
    #[error("the MCP server gave no response to a tools/call request")]
    NoResponse,

    /// The JSON-RPC response carried a top-level `error` — a transport
    /// fault (`ROADMAP.md`/`docs/MCP.md`: malformed request, unknown
    /// method), never a query outcome. Reaching this means this crate built
    /// a request `handle_line` could not parse as one of its own methods,
    /// which is this crate's bug, not the caller's.
    #[error("MCP protocol error {code}: {message}")]
    Protocol { code: i64, message: String },

    /// A `tools/call` response with neither `isError` nor `structuredContent`
    /// — not a shape either `mcp.rs` produces today. Surfaced rather than
    /// panicking, because a future MCP server change should fail one query,
    /// not the whole desktop app.
    #[error("the MCP server returned a tools/call result with no structuredContent")]
    MalformedResult,
}

/// Sends one `tools/call` line to `server` and unwraps its response into
/// [`CommandError`]'s cases — the JSON-RPC/`tools/call` unwrapping [`query`]
/// and [`ledger_query`] would otherwise each repeat verbatim, since both
/// drive an `mcp_stdio`-shaped server the same way.
fn call_tool(
    mut handle_line: impl FnMut(&str) -> Option<String>,
    tool: &str,
    args: Value,
) -> Result<Value, CommandError> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args },
    });
    let line = request.to_string();

    let response_line = handle_line(&line).ok_or(CommandError::NoResponse)?;
    let response: Value =
        serde_json::from_str(&response_line).map_err(|err| CommandError::Protocol {
            code: -32700,
            message: format!("could not parse the MCP server's own response: {err}"),
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

    let result = response
        .get("result")
        .ok_or(CommandError::MalformedResult)?;

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
    call_tool(|line| server.handle_line(line), tool, args)
}

/// Run one ledger tool — read or gated write — against `ledger`, scoped to
/// `company`, and return its `structuredContent`: exactly the JSON shape
/// `docs/MCP.md`'s ledger section documents for that tool, the same value
/// `apps/desktop/ui/js/data/ledger/tauri.js` expects back from `invoke`.
///
/// `tool` is one of the twenty-eight names [`ledger::mcp::tools`] lists —
/// fifteen reads (`"trial_balance"`, `"profit_and_loss"`, `"balance_sheet"`,
/// `"sales_tax_lines"`, `"general_ledger"`, `"audit_trail"`,
/// `"entries_for_document"`, `"list_adjustments"`, `"bank_status"`,
/// `"locked_through"`, `"list_contacts"`, `"list_items"`,
/// `"open_documents"`, `"aging"`, `"get_document"`) and thirteen writes
/// (`"upsert_contact"`, `"upsert_item"`, `"create_document"`,
/// `"update_document"`, `"void_document"`, `"apply_credit"`,
/// `"save_and_post_document"`,
/// `"reverse_entry"`, `"propose_adjustment"`, `"decide_adjustment"`,
/// `"bank_confirm_proposal"`, `"close_period"`, `"reopen_period"`) — and
/// `args` is that tool's argument object exactly as `docs/MCP.md`
/// documents it; unlike [`query`] there is no `realm_id`/company key inside
/// `args` itself, since `ledger-mcp` (and this function, mirroring it) is
/// scoped to one company for the call rather than taking one per argument
/// object.
///
/// Opens no gate of its own: a write tool's domain failure (a closed
/// period, a missing class, an unbalanced journal, a replay in progress)
/// comes back as [`CommandError::Tool`] exactly as `docs/MCP.md`'s write
/// gate documents — `ledger::mcp`'s own `LedgerToolSet` is what refuses it,
/// this function only carries the refusal through.
pub fn ledger_query(
    ledger: &Ledger,
    company: &str,
    tool: &str,
    args: Value,
) -> Result<Value, CommandError> {
    let mut server = ledger::mcp::new_server(ledger, company.to_string());
    call_tool(|line| server.handle_line(line), tool, args)
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

    // -----------------------------------------------------------------------
    // ledger_query — the ledger's own reads and gated writes
    // -----------------------------------------------------------------------

    const COMPANY: &str = "aquamentor";

    fn seeded_ledger() -> Ledger {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .create_company(COMPANY, "Test Co", None, now())
            .unwrap();
        ledger
    }

    fn invoice_document(document_id: &str) -> Value {
        invoice_document_dated(document_id, "2026-09-01")
    }

    fn invoice_document_dated(document_id: &str, txn_date: &str) -> Value {
        json!({
            "document_id": document_id,
            "kind": "Invoice",
            "number": null,
            "txn_date": txn_date,
            "due_date": null,
            "contact": { "kind": "Customer", "id": "cust-1" },
            "header_class": "foam",
            "lines": [
                {
                    "line_no": 1, "kind": "Item", "amount": "100.00", "class": "foam",
                    "item_id": null, "account": "4100", "is_taxable": false, "qty": null,
                    "unit_cost": null, "description": "widgets", "posting": null, "entity": null
                }
            ],
            "tax": null,
            "deposit_to": null,
            "pay_from": null,
            "applications": [],
            "unapplied": 0,
            "is_voided": false,
            "source_ref": null,
            "memo": "test invoice"
        })
    }

    /// Exactly the `document`s `js/screens/ledger/entry.js::buildDocument`
    /// sends (pinned by `test/ledger-entry.test.js`), through the same
    /// `ledger_query` the Tauri window calls: the entry screen's wire shape
    /// is the real `create_document`'s, not only the fixture's.
    #[test]
    fn ledger_query_create_document_takes_what_the_entry_screen_sends() {
        let ledger = seeded_ledger();
        for (tool, args) in [
            (
                "upsert_contact",
                json!({ "contact": { "contact_id": "cust-blue-harbor", "kind": "Customer",
                "display_name": "Blue Harbor Swim Club", "terms_days": 30, "default_class": "foam" } }),
            ),
            (
                "upsert_contact",
                json!({ "contact": { "contact_id": "vend-spectra", "kind": "Vendor",
                "display_name": "Spectra EVA", "terms_days": 30 } }),
            ),
            (
                "upsert_item",
                json!({ "item": { "item_id": "rt-50-red", "sku": "RT-50-RED",
                "name": "Rescue tube", "item_type": "non_inventory", "income_account": "4100",
                "expense_account": "5000", "default_class": "foam", "sales_price": "19.95",
                "is_taxable": true } }),
            ),
        ] {
            ledger_query(&ledger, COMPANY, tool, args).unwrap();
        }

        let invoice = ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Invoice", "txn_date": "2026-10-01",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "lines": [{ "kind": "Item", "amount": "199.50", "is_taxable": true,
                            "item_id": "rt-50-red", "qty": "10", "description": "Rescue tube" }]
            }}),
        )
        .unwrap();
        assert_eq!(invoice["tax"], json!("13.22"));
        assert_eq!(invoice["total"], json!("212.72"));
        assert_eq!(invoice["due_date"], json!("2026-10-31"));
        assert!(!invoice["entry_id"].is_null());

        let bill = ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Bill", "txn_date": "2026-10-01", "number": "SPX-2001",
                "contact": { "kind": "Vendor", "id": "vend-spectra" },
                "lines": [{ "kind": "Account", "amount": "84.10", "is_taxable": false,
                            "account": "6100", "class": "foam" }]
            }}),
        )
        .unwrap();
        assert_eq!(bill["number"], json!("SPX-2001"));
        assert_eq!(bill["total"], json!("84.10"));

        ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Payment", "txn_date": "2026-10-10",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "applications": [{ "target_document_id": invoice["document_id"],
                                   "target_kind": "Invoice", "amount": "212.72" }],
                "deposit_to": "1100"
            }}),
        )
        .unwrap();

        let ar = ledger_query(
            &ledger,
            COMPANY,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(ar["totals"]["total"], json!("0.00"));
        let ap = ledger_query(
            &ledger,
            COMPANY,
            "aging",
            json!({ "book": "payable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(ap["totals"]["current"], json!("84.10"));

        ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "BillPayment", "txn_date": "2026-10-12",
                "contact": { "kind": "Vendor", "id": "vend-spectra" },
                "applications": [{ "target_document_id": bill["document_id"],
                                   "target_kind": "Bill", "amount": "84.10" }],
                "pay_from": "1100"
            }}),
        )
        .unwrap();
        let ap = ledger_query(
            &ledger,
            COMPANY,
            "aging",
            json!({ "book": "payable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(ap["totals"]["total"], json!("0.00"));

        let receipt = ledger_query(&ledger, COMPANY, "create_document", json!({ "actor": "dan", "document": {
            "kind": "SalesReceipt", "txn_date": "2026-10-12",
            "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
            "lines": [{ "kind": "Item", "amount": "19.95", "is_taxable": true, "item_id": "rt-50-red", "qty": "1" }],
            "deposit_to": "1100"
        }}))
        .unwrap();
        assert_eq!(receipt["total"], json!("21.27"));

        let credit = ledger_query(&ledger, COMPANY, "create_document", json!({ "actor": "dan", "document": {
            "kind": "CreditMemo", "txn_date": "2026-10-13",
            "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
            "lines": [{ "kind": "Item", "amount": "19.95", "is_taxable": true, "item_id": "rt-50-red", "qty": "1" }]
        }}))
        .unwrap();
        assert_eq!(credit["number"], json!("1001"));
        let open = ledger_query(
            &ledger,
            COMPANY,
            "open_documents",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(
            open[0]["open"],
            json!("-21.27"),
            "an unapplied credit memo sits open as a negative"
        );
    }

    /// Editing, voiding and applying a credit exactly as the entry and
    /// open-items screens send them (pinned by `test/ledger-entry.test.js`
    /// and `test/ledger-fixture.test.js`): `get_document` to open a saved
    /// invoice, `buildDocument` plus its `document_id` back through
    /// `update_document`, `apply_credit` from a customer's open credit memo,
    /// and `void_document` with a reason.
    #[test]
    fn ledger_query_edit_void_and_apply_credit_take_what_the_screens_send() {
        let ledger = seeded_ledger();
        for (tool, args) in [
            (
                "upsert_contact",
                json!({ "contact": { "contact_id": "cust-blue-harbor", "kind": "Customer",
                "display_name": "Blue Harbor Swim Club", "terms_days": 30, "default_class": "foam" } }),
            ),
            (
                "upsert_item",
                json!({ "item": { "item_id": "rt-50-red", "sku": "RT-50-RED",
                "name": "Rescue tube", "item_type": "non_inventory", "income_account": "4100",
                "expense_account": "5000", "default_class": "foam", "sales_price": "19.95",
                "is_taxable": true } }),
            ),
        ] {
            ledger_query(&ledger, COMPANY, tool, args).unwrap();
        }
        let line = |qty: &str, amount: &str| {
            json!({ "kind": "Item", "amount": amount,
            "is_taxable": true, "item_id": "rt-50-red", "qty": qty, "description": "Rescue tube" })
        };
        let invoice = ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Invoice", "txn_date": "2026-10-01",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "lines": [line("10", "199.50")]
            }}),
        )
        .unwrap();
        let invoice_id = invoice["document_id"].as_str().unwrap().to_string();

        let saved = ledger_query(
            &ledger,
            COMPANY,
            "get_document",
            json!({ "document_id": invoice_id }),
        )
        .unwrap();
        assert_eq!(saved["number"], json!("1001"));
        assert_eq!(saved["lines"][0]["qty"], json!("10"));
        assert_eq!(saved["lines"][0]["item_id"], json!("rt-50-red"));
        assert_eq!(saved["total"], json!("212.72"));

        let edited = ledger_query(
            &ledger,
            COMPANY,
            "update_document",
            json!({ "actor": "dan", "document": {
                "document_id": invoice_id, "kind": "Invoice", "txn_date": "2026-10-01",
                "number": "1001", "memo": "two more",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "lines": [line("12", "239.40")]
            }}),
        )
        .unwrap();
        assert_eq!(edited["version"], json!(2));
        assert_eq!(edited["number"], json!("1001"));
        assert_eq!(edited["total"], json!("255.26"));
        assert_eq!(edited["reversal_entry_ids"].as_array().unwrap().len(), 1);

        let credit = ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "CreditMemo", "txn_date": "2026-10-05",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "lines": [line("1", "19.95")]
            }}),
        )
        .unwrap();
        let applied = ledger_query(
            &ledger,
            COMPANY,
            "apply_credit",
            json!({
                "credit_id": credit["document_id"], "target_document_id": invoice_id, "actor": "dan"
            }),
        )
        .unwrap();
        assert_eq!(applied["applied"], json!("21.27"));
        let ar = ledger_query(
            &ledger,
            COMPANY,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(ar["totals"]["total"], json!("233.99"));
        let tb = ledger_query(
            &ledger,
            COMPANY,
            "trial_balance",
            json!({ "as_of": "2026-10-15" }),
        )
        .unwrap();
        let ar_row = tb["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["account_id"] == json!("1200"))
            .unwrap();
        assert_eq!(ar_row["balance"], json!("233.99"), "the aging ties to 1200");

        // The invoice has a credit on it: refused, as a tool error the screen shows.
        let refused = ledger_query(
            &ledger,
            COMPANY,
            "void_document",
            json!({ "document_id": invoice_id, "reason": "entered twice", "actor": "dan" }),
        );
        assert!(
            matches!(refused, Err(CommandError::Tool { .. })),
            "{refused:?}"
        );
        for id in [credit["document_id"].as_str().unwrap(), invoice_id.as_str()] {
            let voided = ledger_query(
                &ledger,
                COMPANY,
                "void_document",
                json!({ "document_id": id, "reason": "entered twice", "actor": "dan" }),
            )
            .unwrap();
            assert_eq!(voided["is_voided"], json!(true));
        }
        let ar = ledger_query(
            &ledger,
            COMPANY,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(ar["totals"]["total"], json!("0.00"));
    }

    /// A sales order as the entry screen sends it, listed and converted the
    /// way Receivables does it (D42), through the real ledger.
    #[test]
    fn ledger_query_sales_order_lists_and_converts_like_the_receivables_screen() {
        let ledger = seeded_ledger();
        for (tool, args) in [
            (
                "upsert_contact",
                json!({ "contact": { "contact_id": "cust-blue-harbor", "kind": "Customer",
                "display_name": "Blue Harbor Swim Club", "terms_days": 30, "default_class": "foam" } }),
            ),
            (
                "upsert_item",
                json!({ "item": { "item_id": "rt-50-red", "name": "Rescue tube",
                "item_type": "non_inventory", "income_account": "4100", "expense_account": "5000",
                "default_class": "foam", "is_taxable": true } }),
            ),
        ] {
            ledger_query(&ledger, COMPANY, tool, args).unwrap();
        }
        let so = ledger_query(
            &ledger,
            COMPANY,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "SalesOrder", "txn_date": "2026-10-01",
                "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
                "lines": [{ "kind": "Item", "amount": "199.50", "is_taxable": true,
                            "item_id": "rt-50-red", "qty": "10" }]
            }}),
        )
        .unwrap();
        assert!(so["entry_id"].is_null(), "a sales order posts nothing");

        let open = ledger_query(
            &ledger,
            COMPANY,
            "open_sales_orders",
            json!({ "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(open[0]["open"], json!("199.50"));

        let invoice = ledger_query(
            &ledger,
            COMPANY,
            "convert_to_invoice",
            json!({
                "sales_order_id": so["document_id"], "txn_date": "2026-10-10", "actor": "dan"
            }),
        )
        .unwrap();
        assert_eq!(invoice["total"], json!("212.72"));
        assert!(!invoice["entry_id"].is_null());
        let open = ledger_query(
            &ledger,
            COMPANY,
            "open_sales_orders",
            json!({ "as_of": "2026-10-15" }),
        )
        .unwrap();
        assert_eq!(open, json!([]));
    }

    /// The editors' `contact` and `item` exactly as
    /// `js/screens/ledger/masterData.js` builds them: every optional field
    /// present, blanks as `null`.
    #[test]
    fn ledger_query_upserts_take_what_the_editors_send() {
        let ledger = seeded_ledger();
        let contact = ledger_query(
            &ledger,
            COMPANY,
            "upsert_contact",
            json!({ "contact": {
                "contact_id": "cust-lakeside-y", "kind": "Customer", "display_name": "Lakeside Y",
                "company_name": null, "email": null, "phone": null, "bill_address": null,
                "ship_address": null, "terms_days": 15, "is_tax_exempt": false,
                "default_class": null, "is_active": true, "source_ref": null
            }}),
        )
        .unwrap();
        assert_eq!(contact["contact"]["kind"], json!("customer"));
        assert_eq!(contact["contact"]["terms_days"], json!(15));

        let item = ledger_query(
            &ledger,
            COMPANY,
            "upsert_item",
            json!({ "item": {
                "item_id": "item-eva-2", "sku": "EVA-2", "name": "EVA sheet", "description": null,
                "item_type": "inventory", "income_account": "4100", "expense_account": "5000",
                "asset_account": "1300", "default_class": null, "sales_price": "80.00",
                "purchase_cost": null, "unit_cost": null, "is_taxable": true, "is_active": true,
                "source_ref": null
            }}),
        )
        .unwrap();
        assert_eq!(item["item"]["sales_price"], json!("80.00"));
        assert_eq!(item["item"]["asset_account"], json!("1300"));
    }

    #[test]
    fn ledger_query_locked_through_round_trips_through_the_json_rpc_boundary() {
        let ledger = seeded_ledger();
        let result = ledger_query(&ledger, COMPANY, "locked_through", json!({})).unwrap();
        assert_eq!(result["locked_through"], Value::Null);
    }

    #[test]
    fn ledger_query_save_and_post_document_posts_a_balanced_entry() {
        let ledger = seeded_ledger();
        let result = ledger_query(
            &ledger,
            COMPANY,
            "save_and_post_document",
            json!({ "document": invoice_document("inv-1"), "actor": "dan" }),
        )
        .unwrap();

        assert_eq!(result["document_id"], json!("inv-1"));
        assert!(!result["entry_id"].is_null());
        assert_eq!(result["locked_through"], Value::Null);
    }

    #[test]
    fn ledger_query_trial_balance_reflects_a_posted_document() {
        let ledger = seeded_ledger();
        ledger_query(
            &ledger,
            COMPANY,
            "save_and_post_document",
            json!({ "document": invoice_document("inv-2"), "actor": "dan" }),
        )
        .unwrap();

        let tb = ledger_query(
            &ledger,
            COMPANY,
            "trial_balance",
            json!({ "as_of": "2026-09-01" }),
        )
        .unwrap();
        let rows = tb["rows"].as_array().unwrap();
        let ar_row = rows
            .iter()
            .find(|row| row["account_id"] == json!(ledger::chart::ACCOUNTS_RECEIVABLE))
            .expect("AR row present");
        assert_eq!(ar_row["balance"], json!("100.00"));
    }

    /// A write dated on or before a closed period comes back as an ordinary
    /// [`CommandError::Tool`] naming the closed period — never a panic —
    /// exactly as `docs/MCP.md`'s write gate documents.
    #[test]
    fn ledger_query_a_write_into_a_closed_period_is_a_tool_error_not_a_panic() {
        let ledger = seeded_ledger();
        let closed = ledger_query(
            &ledger,
            COMPANY,
            "close_period",
            json!({ "period_end": "2026-08-31", "actor": "dan", "note": "August close" }),
        )
        .unwrap();
        assert_eq!(closed["locked_through"], json!("2026-08-31"));

        let err = ledger_query(
            &ledger,
            COMPANY,
            "save_and_post_document",
            json!({ "document": invoice_document_dated("inv-3", "2026-08-15"), "actor": "dan" }),
        )
        .unwrap_err();

        match err {
            CommandError::Tool { message } => assert!(message.contains("closed"), "{message:?}"),
            other => panic!("expected CommandError::Tool, got {other:?}"),
        }
    }

    #[test]
    fn ledger_query_an_unknown_ledger_tool_is_a_tool_error() {
        let ledger = seeded_ledger();
        let err = ledger_query(&ledger, COMPANY, "not_a_real_tool", json!({})).unwrap_err();
        match err {
            CommandError::Tool { message } => assert!(message.contains("not_a_real_tool")),
            other => panic!("expected CommandError::Tool, got {other:?}"),
        }
    }
}
