//! MCP server over the ledger itself. `ROADMAP.md` §G ("Channel intake") and
//! §D: the write UI's skills — and every skill that already reads QBO through
//! `qbo-local-mcp` — need one write path into this book once M3 lands, and
//! this is it: every report `crate::report` and `crate::accountant` already
//! expose, plus the gated writes `crate::store`, `crate::post` and
//! `crate::bank` support. `docs/MCP.md` documents registration, every tool,
//! and the write semantics below.
//!
//! Unlike `qbo_local::mcp` (one server, every realm, `realm_id` on each
//! call), this server is bound to one company for its whole process —
//! `LEDGER_COMPANY` at startup, matching `Ledger` itself being opened
//! against one book. There is no per-call company argument.
//!
//! The JSON-RPC 2.0 / MCP stdio transport — line framing, error codes,
//! `initialize`/`initialized`/`ping`/`tools/list`/`tools/call` — lives in
//! [`mcp_stdio`], shared with `qbo_local::mcp`. This module is
//! [`LedgerToolSet`], the [`mcp_stdio::ToolSet`] impl that turns a
//! `tools/call` into a read or a gated write against [`crate::store::Ledger`];
//! [`Server`] is simply `mcp_stdio::Server<LedgerToolSet>` (there is no
//! backward-compatible wrapper to preserve here, unlike `qbo_local::mcp`),
//! built by [`new_server`] and driven by [`mcp_stdio::serve_stdio`] exactly
//! as `bin/ledger-mcp.rs` does.
//!
//! **The write gate.** `LEDGER-DESIGN.md` §4/§5: a replay in progress (the
//! importer walking the `qbo-local` replica, `Ledger::set_replaying`) must
//! never interleave with a live write, so every write tool below refuses
//! immediately when [`crate::store::Ledger::is_replaying`] is true, before it
//! touches anything. And because the period gate can reject a write for a
//! reason a caller only discovers from the error, every write tool's result
//! — success or refusal — carries the company's current
//! [`crate::store::Ledger::locked_through`] alongside it, so a caller always
//! sees the gate it is or is not up against. A domain failure (period
//! closed, a missing class, an unbalanced entry, and the rest) comes back as
//! a normal `tools/call` result with `isError: true` and the reason in
//! `content`, never as a JSON-RPC error — the same rule `qbo_local::mcp`
//! documents: a JSON-RPC error means the *transport* failed, not the
//! command.
//!
//! **Money on the wire.** Tool *output* always renders `Money` as a
//! two-place decimal string (`"1234.56"`, a credit as `"-12.00"`), same as
//! `qbo_local::mcp`. Tool *input* is more forgiving: a `Money` field may be
//! given either as that same decimal string or as a bare minor-unit integer
//! (`123456`) — [`normalize_money_fields`] rewrites every recognised
//! money-shaped field in an argument tree to the integer form before any
//! typed deserialiser (`LedgerDocument`'s own, or this module's) sees it, so
//! an LLM caller can write whichever is more natural for a given field
//! without the two forms ever needing separate schemas.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use mcp_stdio::{ServerInfo, ToolSet};
pub use mcp_stdio::{ToolError, ToolSpec};

use crate::accountant::{self, AccountantError};
use crate::authnet::link::{self, HostedPaymentClient, LinkError};
use crate::books::{self, Book, BooksError, Contact, Item};
use crate::payroll::{self, PayrollError};
use crate::post::{self, PostError, Posting};
use crate::report;
use crate::sales_orders::{self, ConvertLine, ConvertRequest, SalesOrderError};
use crate::store::{AdjustmentState, CommandMeta, Ledger, LedgerError};
use crate::types::{
    AccountId, ClassId, ContactKind, ContactRef, ItemAccounts, JournalEntry, JournalLine,
    LedgerDocument, PostingConfig, PostingContext,
};
use ledger_core::Money;

// ---------------------------------------------------------------------------
// The tool set and the server
// ---------------------------------------------------------------------------

/// The [`mcp_stdio::ToolSet`] over one open, read-write ledger, scoped to one
/// company for the life of the process. `pub` only so [`Server`] (a type
/// alias over a foreign generic type) can name it; its fields stay private.
///
/// Borrows the [`Ledger`] rather than owning it (`'a`) — every [`Ledger`]
/// method takes `&self` (interior mutability all the way down to the
/// `rusqlite::Connection`, same as `qbo_local::store::Store`), so nothing
/// here needs ownership, and a caller that already holds an open `Ledger` —
/// `apps/desktop/commands`' `ledger_query`, mirroring
/// `qbo_local::mcp::Server<'a>`'s own borrow — can hand this a reference
/// for the life of one call rather than moving the connection in and back
/// out again.
pub struct LedgerToolSet<'a> {
    ledger: &'a Ledger,
    company: String,
    /// The Authorize.net client `payment_link` asks for a hosted page.
    /// `None`: the tool says it is not configured.
    payments: Option<Box<dyn HostedPaymentClient + 'a>>,
}

impl<'a> ToolSet for LedgerToolSet<'a> {
    fn tools(&self) -> Vec<ToolSpec> {
        tools()
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        self.call_tool(name, args)
    }
}

/// The server: one open ledger, spoken to over JSON-RPC 2.0 via
/// [`mcp_stdio::Server`]. Build one with [`new_server`]; run it with
/// [`mcp_stdio::serve_stdio`].
pub type Server<'a> = mcp_stdio::Server<LedgerToolSet<'a>>;

pub fn new_server(ledger: &Ledger, company: String) -> Server<'_> {
    let info = ServerInfo {
        name: "ledger",
        version: env!("CARGO_PKG_VERSION"),
    };
    mcp_stdio::Server::new(
        LedgerToolSet {
            ledger,
            company,
            payments: None,
        },
        info,
    )
}

/// [`new_server`] with an Authorize.net client behind `payment_link`
/// (`crate::authnet::link`): `HttpAuthnetClient` in `ledger-mcp`,
/// `MockAuthnet` under test.
pub fn new_server_with_payments<'a>(
    ledger: &'a Ledger,
    company: String,
    payments: Box<dyn HostedPaymentClient + 'a>,
) -> Server<'a> {
    let info = ServerInfo {
        name: "ledger",
        version: env!("CARGO_PKG_VERSION"),
    };
    mcp_stdio::Server::new(
        LedgerToolSet {
            ledger,
            company,
            payments: Some(payments),
        },
        info,
    )
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl<'a> LedgerToolSet<'a> {
    fn call_tool(&mut self, name: &str, mut args: Value) -> Result<Value, ToolError> {
        match name {
            // --- reads --------------------------------------------------
            "trial_balance" => {
                let as_of = parse_date(&args, "as_of")?;
                let tb =
                    report::trial_balance(self.ledger, &self.company, as_of).map_err(ledger_err)?;
                Ok(trial_balance_json(&tb))
            }
            "profit_and_loss" => {
                let from = parse_date(&args, "from")?;
                let to = parse_date(&args, "to")?;
                let pnl = report::profit_and_loss(self.ledger, &self.company, from, to)
                    .map_err(ledger_err)?;
                Ok(pnl_json(&pnl))
            }
            "balance_sheet" => {
                let as_of = parse_date(&args, "as_of")?;
                let bs =
                    report::balance_sheet(self.ledger, &self.company, as_of).map_err(ledger_err)?;
                Ok(balance_sheet_json(&bs))
            }
            "sales_tax_lines" => self.sales_tax_lines_tool(&args),
            "general_ledger" => {
                let from = parse_date(&args, "from")?;
                let to = parse_date(&args, "to")?;
                let account = args
                    .get("account")
                    .and_then(Value::as_str)
                    .map(|s| AccountId(s.to_string()));
                let detail = accountant::general_ledger(
                    self.ledger,
                    &self.company,
                    from,
                    to,
                    account.as_ref(),
                )
                .map_err(accountant_err)?;
                Ok(gl_detail_json(&detail))
            }
            "audit_trail" => {
                let since = parse_optional_date(&args, "since")?;
                let rows = accountant::audit_trail(self.ledger, &self.company, since)
                    .map_err(accountant_err)?;
                Ok(json!(rows.iter().map(audit_row_json).collect::<Vec<_>>()))
            }
            "entries_for_document" => {
                let document_id = required_str(&args, "document_id")?;
                let entries = self
                    .ledger
                    .entries_for_document(&self.company, document_id)
                    .map_err(ledger_err)?;
                Ok(json!(entries
                    .iter()
                    .map(|(id, entry, posted)| json!({
                        "entry_id": id,
                        "entry": journal_entry_json(entry),
                        "is_posted": posted,
                    }))
                    .collect::<Vec<_>>()))
            }
            "list_adjustments" => {
                let state = match args.get("state").and_then(Value::as_str) {
                    Some(raw) => Some(
                        AdjustmentState::parse(raw)
                            .ok_or_else(|| ToolError::new(format!("unknown state {raw:?}")))?,
                    ),
                    None => None,
                };
                let list = accountant::list_adjustments(self.ledger, &self.company, state)
                    .map_err(accountant_err)?;
                Ok(json!(list
                    .iter()
                    .map(adjustment_request_json)
                    .collect::<Vec<_>>()))
            }
            "bank_status" => self.bank_status_tool(&args),
            "locked_through" => {
                let locked = self
                    .ledger
                    .locked_through(&self.company)
                    .map_err(ledger_err)?;
                Ok(json!({ "locked_through": locked.map(|d| d.to_string()) }))
            }

            "list_contacts" => {
                let kind = match args.get("kind").and_then(Value::as_str) {
                    None => None,
                    Some("customer") => Some(ContactKind::Customer),
                    Some("vendor") => Some(ContactKind::Vendor),
                    Some(other) => {
                        return Err(ToolError::new(format!(
                            "\"kind\" must be \"customer\" or \"vendor\", got {other:?}"
                        )))
                    }
                };
                let query = args.get("query").and_then(Value::as_str);
                let inactive = args
                    .get("include_inactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let contacts =
                    books::list_contacts(self.ledger, &self.company, kind, query, inactive)
                        .map_err(books_err)?;
                Ok(json!(contacts.iter().map(contact_json).collect::<Vec<_>>()))
            }
            "list_items" => {
                let query = args.get("query").and_then(Value::as_str);
                let inactive = args
                    .get("include_inactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let items = books::list_items(self.ledger, &self.company, query, inactive)
                    .map_err(books_err)?;
                Ok(json!(items.iter().map(item_json).collect::<Vec<_>>()))
            }
            "open_documents" => {
                let book = parse_book(&args)?;
                let as_of = parse_date(&args, "as_of")?;
                let open = books::open_documents(self.ledger, &self.company, book, as_of)
                    .map_err(books_err)?;
                Ok(json!(open
                    .iter()
                    .map(open_document_json)
                    .collect::<Vec<_>>()))
            }
            "get_document" => {
                let document_id = required_str(&args, "document_id")?;
                let doc = books::latest_document(self.ledger, &self.company, document_id)
                    .map_err(books_err)?;
                let version = self
                    .ledger
                    .document_version_count(&self.company, document_id)
                    .map_err(ledger_err)?;
                document_json(&doc, version)
            }
            "open_sales_orders" => {
                let as_of = parse_optional_date(&args, "as_of")?;
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                let orders = sales_orders::sales_orders(self.ledger, &self.company, as_of, !all)
                    .map_err(sales_order_err)?;
                Ok(json!(orders
                    .iter()
                    .map(sales_order_json)
                    .collect::<Vec<_>>()))
            }
            "aging" => {
                let book = parse_book(&args)?;
                let as_of = parse_date(&args, "as_of")?;
                let aging =
                    books::aging(self.ledger, &self.company, book, as_of).map_err(books_err)?;
                Ok(json!({
                    "book": book_str(aging.book),
                    "as_of": aging.as_of.to_string(),
                    "rows": aging.rows.iter().map(aging_row_json).collect::<Vec<_>>(),
                    "totals": aging_row_json(&aging.totals),
                }))
            }

            // --- writes ---------------------------------------------------
            "upsert_contact" => self.upsert_contact(&mut args),
            "upsert_item" => self.upsert_item(&mut args),
            "create_document" => self.create_document(&mut args),
            "convert_to_invoice" => self.convert_to_invoice(&args),
            "update_document" => self.update_document(&mut args),
            "void_document" => self.void_document(&args),
            "apply_credit" => self.apply_credit(&mut args),
            "save_and_post_document" => self.save_and_post_document(&mut args),
            "reverse_entry" => self.reverse_entry(&args),
            "propose_adjustment" => self.propose_adjustment(&mut args),
            "decide_adjustment" => self.decide_adjustment(&args),
            "bank_confirm_proposal" => self.bank_confirm_proposal(&args),
            "close_period" => self.close_period(&args),
            "payment_link" => self.payment_link(&args),
            "reopen_period" => self.reopen_period(&args),
            "import_payroll" => self.import_payroll(&mut args),

            other => Err(ToolError::new(format!("unknown tool: {other}"))),
        }
    }

    fn sales_tax_lines_tool(&self, args: &Value) -> Result<Value, ToolError> {
        let year =
            args.get("year")
                .and_then(Value::as_i64)
                .ok_or_else(|| ToolError::new("missing or invalid \"year\""))? as i32;
        let quarter = args
            .get("quarter")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::new("missing or invalid \"quarter\""))?
            as u32;
        if !(1..=4).contains(&quarter) {
            return Err(ToolError::new("\"quarter\" must be 1..=4"));
        }
        let rate = match args.get("rate") {
            None | Some(Value::Null) => PostingConfig::default().sales_tax_rate,
            Some(Value::String(raw)) => Decimal::from_str(raw)
                .map_err(|_| ToolError::new(format!("invalid \"rate\": {raw:?}")))?,
            Some(other) => {
                return Err(ToolError::new(format!(
                    "\"rate\" must be a decimal string, got {other}"
                )))
            }
        };
        let lines = report::sales_tax_lines(self.ledger, &self.company, (year, quarter), rate)
            .map_err(ledger_err)?;
        Ok(sales_tax_lines_json(&lines))
    }

    fn bank_status_tool(&self, args: &Value) -> Result<Value, ToolError> {
        let statement_id = required_str(args, "statement_id")?;
        let statement = self
            .ledger
            .bank_statement(&self.company, statement_id)
            .map_err(ledger_err)?;
        let lines = self
            .ledger
            .list_bank_lines(&self.company, statement_id)
            .map_err(ledger_err)?;
        let proposed = lines
            .iter()
            .filter(|line| line.match_kind.as_deref() == Some("proposed"))
            .count();
        let unmatched = lines
            .iter()
            .filter(|line| line.match_kind.is_none())
            .count();
        let matched = lines.len() - proposed - unmatched;
        Ok(json!({
            "statement": bank_statement_json(&statement),
            "lines": lines.iter().map(bank_line_json).collect::<Vec<_>>(),
            "summary": { "matched": matched, "proposed": proposed, "unmatched": unmatched },
        }))
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    fn refuse_if_replaying(&self) -> Result<(), ToolError> {
        // W24: an accountant handoff snapshot is opened read-only; say so
        // rather than surfacing SQLite's "attempt to write a readonly database".
        if self.ledger.is_read_only() {
            return Err(ToolError::new(
                "this ledger is a read-only accountant snapshot; writes are refused — send \
                 proposed adjustments to Dan to enter on the live book",
            ));
        }
        if self.ledger.is_replaying() {
            return Err(ToolError::new(
                "the ledger is replaying an import; writes are refused until it finishes",
            ));
        }
        Ok(())
    }

    /// D28: an open item the slim cutover import brought across is carried
    /// by the opening entry. Posting a new version of it would put its AR or
    /// AP on the books a second time.
    fn refuse_if_carried_by_opening(&self, document_id: &str) -> Result<(), ToolError> {
        if books::carried_by_opening(self.ledger, &self.company, document_id).map_err(books_err)? {
            return Err(ToolError::new(format!(
                "{document_id} came across in the cutover import and its balance sits in the \
                 opening entry; settle it with a payment or credit, or adjust it with a journal \
                 entry, rather than editing it"
            )));
        }
        Ok(())
    }

    /// Attaches the company's current `locked_through` to a write's result,
    /// so a caller always sees the gate it is or is not up against —
    /// required on every write tool by this module's own docs.
    fn with_locked_through(&self, mut value: Value) -> Result<Value, ToolError> {
        let locked = self
            .ledger
            .locked_through(&self.company)
            .map_err(ledger_err)?;
        if let Value::Object(map) = &mut value {
            map.insert(
                "locked_through".to_string(),
                json!(locked.map(|d| d.to_string())),
            );
        }
        Ok(value)
    }

    fn upsert_contact(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let raw = args
            .get_mut("contact")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"contact\""))?;
        let contact: Contact = serde_json::from_value(raw)
            .map_err(|err| ToolError::new(format!("invalid \"contact\": {err}")))?;
        books::upsert_contact(self.ledger, &self.company, &contact, Utc::now())
            .map_err(books_err)?;
        self.with_locked_through(json!({ "contact": contact_json(&contact) }))
    }

    fn upsert_item(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let mut raw = args
            .get_mut("item")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"item\""))?;
        normalize_money_fields(&mut raw);
        let item: Item = serde_json::from_value(raw)
            .map_err(|err| ToolError::new(format!("invalid \"item\": {err}")))?;
        books::upsert_item(self.ledger, &self.company, &item, Utc::now()).map_err(books_err)?;
        self.with_locked_through(json!({ "item": item_json(&item) }))
    }

    /// The standalone entry path (D28): the caller sends what a person would
    /// type, [`books::prepare_document`] checks it against the master data
    /// and fills the rest, then it is saved and posted exactly as
    /// `save_and_post_document` does.
    fn create_document(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let actor = required_str(args, "actor")?.to_string();
        let mut document_value = args
            .get_mut("document")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"document\""))?;
        fill_document_defaults(&mut document_value)?;
        normalize_money_fields(&mut document_value);
        let doc: LedgerDocument = serde_json::from_value(document_value)
            .map_err(|err| ToolError::new(format!("invalid \"document\": {err}")))?;

        self.refuse_if_carried_by_opening(&doc.document_id)?;
        let (doc, ctx) =
            books::prepare_document(self.ledger, &self.company, doc, PostingConfig::default())
                .map_err(books_err)?;
        self.save_and_post_prepared(doc, &ctx, actor, "mcp_create_document")
    }

    /// D42: the invoice for a sales order, linked back to it, through the
    /// same checks, numbering, tax and save-and-post as create_document.
    fn convert_to_invoice(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let actor = required_str(args, "actor")?.to_string();
        let sales_order_id = required_str(args, "sales_order_id")?.to_string();
        let txn_date = parse_date(args, "txn_date")?;
        let lines = match args.get("lines") {
            None | Some(Value::Null) => None,
            Some(Value::Array(lines)) => {
                let mut out = Vec::with_capacity(lines.len());
                for line in lines {
                    let line_no = line.get("line_no").and_then(Value::as_i64).ok_or_else(|| {
                        ToolError::new("each of \"lines\" needs an integer \"line_no\"")
                    })?;
                    let qty = match line.get("qty") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(raw)) => Some(
                            Decimal::from_str(raw)
                                .map_err(|_| ToolError::new(format!("invalid \"qty\": {raw:?}")))?,
                        ),
                        Some(Value::Number(n)) => Some(
                            Decimal::from_str(&n.to_string())
                                .map_err(|_| ToolError::new(format!("invalid \"qty\": {n}")))?,
                        ),
                        Some(other) => {
                            return Err(ToolError::new(format!("invalid \"qty\": {other}")))
                        }
                    };
                    out.push(ConvertLine { line_no, qty });
                }
                Some(out)
            }
            Some(other) => {
                return Err(ToolError::new(format!(
                    "\"lines\" must be an array of {{\"line_no\", \"qty\"}}, got {other}"
                )))
            }
        };
        let invoice = sales_orders::invoice_from_sales_order(
            self.ledger,
            &self.company,
            &sales_order_id,
            ConvertRequest {
                document_id: uuid::Uuid::now_v7().to_string(),
                txn_date,
                lines,
                number: args
                    .get("number")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                memo: args.get("memo").and_then(Value::as_str).map(str::to_string),
            },
        )
        .map_err(sales_order_err)?;
        let (invoice, ctx) = books::prepare_document(
            self.ledger,
            &self.company,
            invoice,
            PostingConfig::default(),
        )
        .map_err(books_err)?;
        let mut result =
            self.save_and_post_prepared(invoice, &ctx, actor, "mcp_convert_to_invoice")?;
        let remaining = sales_orders::sales_orders(self.ledger, &self.company, None, false)
            .map_err(sales_order_err)?
            .into_iter()
            .find(|so| so.document_id == sales_order_id);
        if let (Value::Object(map), Some(so)) = (&mut result, remaining) {
            map.insert("sales_order_id".into(), json!(sales_order_id));
            map.insert("sales_order_open".into(), json!(money_str(so.open)));
            map.insert("sales_order_closed".into(), json!(so.open.is_zero()));
        }
        Ok(result)
    }

    /// Saves a prepared document as its next version and posts it: the
    /// common tail of create_document and convert_to_invoice.
    fn save_and_post_prepared(
        &mut self,
        doc: LedgerDocument,
        ctx: &PostingContext,
        actor: String,
        command_kind: &str,
    ) -> Result<Value, ToolError> {
        // Post before saving, so a document the posting rules refuse (a
        // missing class, an unbalanced journal) leaves nothing behind. The
        // version it will be saved as is the next one.
        let next_version = self
            .ledger
            .document_version_count(&self.company, &doc.document_id)
            .map_err(ledger_err)?
            + 1;
        let posting = post::post(&doc, next_version, ctx).map_err(post_err)?;

        let now = Utc::now();
        let version = self
            .ledger
            .save_document(
                &self.company,
                &doc,
                CommandMeta {
                    actor_id: actor,
                    kind: command_kind.to_string(),
                    hlc: now.to_rfc3339(),
                },
                now,
            )
            .map_err(ledger_err)?;
        debug_assert_eq!(version.version, next_version);

        let (entry_id, entry_value) = match posting {
            Posting::NonPosting => (Value::Null, Value::Null),
            Posting::Entry(entry) => {
                let entry_id = self
                    .ledger
                    .post_entry(&self.company, &entry, now)
                    .map_err(ledger_err)?;
                (json!(entry_id), journal_entry_json(&entry))
            }
        };

        self.with_locked_through(json!({
            "document_id": doc.document_id,
            "version": version.version,
            "number": doc.number,
            "due_date": doc.due_date.map(|d| d.to_string()),
            "total": money_str(books::document_total(&doc).map_err(|e| ToolError::new(e.to_string()))?),
            "tax": doc.tax.as_ref().map(|t| money_str(t.total_tax)),
            "entry_id": entry_id,
            "entry": entry_value,
        }))
    }

    /// Edits a saved document (`LEDGER-DESIGN.md` §4/§6): the caller sends
    /// the document as typed, `document_id` included, and
    /// [`books::update_document`] saves it as the next version, reversing
    /// the old entry and posting the new one in one unit.
    fn update_document(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let actor = required_str(args, "actor")?.to_string();
        let mut document_value = args
            .get_mut("document")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"document\""))?;
        let Some(document_id) = document_value
            .get("document_id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Err(ToolError::new(
                "missing \"document.document_id\": update_document edits a saved document",
            ));
        };
        // An editor that only shows lines (a credit memo's form) must not
        // silently drop what the document already applies: no
        // `applications` key keeps the saved ones; `[]` clears them.
        if let Some(map) = document_value.as_object_mut() {
            if !map.contains_key("applications") {
                let saved = books::latest_document(self.ledger, &self.company, &document_id)
                    .map_err(books_err)?;
                map.insert(
                    "applications".to_string(),
                    serde_json::to_value(&saved.applications)
                        .map_err(|err| ToolError::new(err.to_string()))?,
                );
            }
        }
        fill_document_defaults(&mut document_value)?;
        normalize_money_fields(&mut document_value);
        let doc: LedgerDocument = serde_json::from_value(document_value)
            .map_err(|err| ToolError::new(format!("invalid \"document\": {err}")))?;
        let now = Utc::now();
        let revision = books::update_document(
            self.ledger,
            &self.company,
            doc,
            PostingConfig::default(),
            command("mcp_update_document", actor, now),
            now,
        )
        .map_err(books_err)?;
        self.with_locked_through(revision_json(&revision)?)
    }

    fn void_document(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let document_id = required_str(args, "document_id")?;
        let reason = required_str(args, "reason")?.trim();
        if reason.is_empty() {
            return Err(ToolError::new("\"reason\" may not be empty"));
        }
        let actor = required_str(args, "actor")?.to_string();
        let now = Utc::now();
        let revision = books::void_document(
            self.ledger,
            &self.company,
            document_id,
            reason,
            command("mcp_void_document", actor, now),
            now,
        )
        .map_err(books_err)?;
        self.with_locked_through(revision_json(&revision)?)
    }

    fn apply_credit(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let credit_id = required_str(args, "credit_id")?.to_string();
        let target_id = required_str(args, "target_document_id")?.to_string();
        let actor = required_str(args, "actor")?.to_string();
        normalize_money_fields(args);
        let amount = match args.get("amount") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(value_to_money(raw)?),
        };
        let now = Utc::now();
        let revision = books::apply_credit(
            self.ledger,
            &self.company,
            &credit_id,
            &target_id,
            amount,
            command("mcp_apply_credit", actor, now),
            now,
        )
        .map_err(books_err)?;
        let applied = revision
            .document
            .applications
            .iter()
            .find(|a| a.target_document_id == target_id)
            .map(|a| money_str(a.amount));
        let mut result = revision_json(&revision)?;
        if let Value::Object(map) = &mut result {
            map.insert("target_document_id".to_string(), json!(target_id));
            map.insert("applied".to_string(), json!(applied));
        }
        self.with_locked_through(result)
    }

    /// Asks Authorize.net for a hosted payment page for one open invoice
    /// (`crate::authnet::link`). Writes nothing to the ledger; refused during
    /// a replay all the same, since the open balance it charges would be
    /// half-imported.
    fn payment_link(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let document_id = required_str(args, "document_id")?.to_string();
        let client = self.payments.as_mut().ok_or_else(|| {
            ToolError::new(
                "Authorize.net is not configured for this server (docs/AUTHNET.md, \"Payment link\")",
            )
        })?;
        let minted = link::payment_link(
            self.ledger,
            &self.company,
            &document_id,
            client.as_mut(),
            Utc::now(),
        )
        .map_err(link_err)?;
        Ok(json!({
            "document_id": minted.document_id,
            "invoice_number": minted.invoice_number,
            "amount": money_str(minted.amount),
            "environment": minted.environment.as_str(),
            "url": minted.url,
            "token": minted.token,
            "expires_at": minted.expires_at.to_rfc3339(),
            "html": minted.html(),
        }))
    }

    fn save_and_post_document(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let actor = required_str(args, "actor")?.to_string();
        let mut document_value = args
            .get_mut("document")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"document\""))?;
        normalize_money_fields(&mut document_value);
        let doc: LedgerDocument = serde_json::from_value(document_value)
            .map_err(|err| ToolError::new(format!("invalid \"document\": {err}")))?;
        self.refuse_if_carried_by_opening(&doc.document_id)?;

        let now = Utc::now();
        let version = self
            .ledger
            .save_document(
                &self.company,
                &doc,
                CommandMeta {
                    actor_id: actor,
                    kind: "mcp_save_document".to_string(),
                    hlc: now.to_rfc3339(),
                },
                now,
            )
            .map_err(ledger_err)?;

        let ctx = self.posting_context(args)?;
        let posting = post::post(&doc, version.version, &ctx).map_err(post_err)?;

        let (entry_id, entry_value) = match posting {
            Posting::NonPosting => (Value::Null, Value::Null),
            Posting::Entry(entry) => {
                let entry_id = self
                    .ledger
                    .post_entry(&self.company, &entry, now)
                    .map_err(ledger_err)?;
                let entry_json = journal_entry_json(&entry);
                (json!(entry_id), entry_json)
            }
        };

        self.with_locked_through(json!({
            "document_id": doc.document_id,
            "version": version.version,
            "entry_id": entry_id,
            "entry": entry_value,
        }))
    }

    /// Builds the [`PostingContext`] `post::post` needs: the caller's own
    /// inline `items` (an MCP call has no replica to look items up in, unlike
    /// `crate::pipeline`'s import), no customer exemptions (a caller who
    /// needs one can post the document non-taxable directly), and the
    /// crate's default [`PostingConfig`].
    fn posting_context(&self, args: &Value) -> Result<PostingContext, ToolError> {
        let mut items = HashMap::new();
        if let Some(map) = args.get("items").and_then(Value::as_object) {
            for (item_id, raw) in map {
                let mut raw = raw.clone();
                normalize_money_fields(&mut raw);
                items.insert(item_id.clone(), parse_item_accounts(&raw)?);
            }
        }
        Ok(PostingContext {
            items,
            customers: HashMap::new(),
            config: PostingConfig::default(),
        })
    }

    fn reverse_entry(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let entry_id = required_str(args, "entry_id")?;
        let on = parse_date(args, "on")?;
        // `actor` is accepted for the same audit-facing symmetry every other
        // write tool has, but `Ledger::reverse_entry` records no actor of its
        // own (§4: a reversal is "the same correction path", not a new
        // command) — it is echoed back rather than silently dropped.
        let actor = required_str(args, "actor")?.to_string();
        let now = Utc::now();
        let reversal_id = self
            .ledger
            .reverse_entry(&self.company, entry_id, on, now)
            .map_err(ledger_err)?;
        let (entry, _) = self
            .ledger
            .entry(&self.company, &reversal_id)
            .map_err(ledger_err)?;
        self.with_locked_through(json!({
            "entry_id": reversal_id,
            "entry": journal_entry_json(&entry),
            "actor": actor,
        }))
    }

    fn propose_adjustment(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let requested_by = required_str(args, "requested_by")?.to_string();
        let description = required_str(args, "description")?.to_string();
        let mut lines_value = args
            .get_mut("lines")
            .map(Value::take)
            .ok_or_else(|| ToolError::new("missing \"lines\""))?;
        normalize_money_fields(&mut lines_value);
        let lines = parse_adjustment_lines(&lines_value)?;

        let now = Utc::now();
        let request = accountant::propose_adjustment(
            self.ledger,
            &self.company,
            &requested_by,
            &description,
            lines,
            now,
        )
        .map_err(accountant_err)?;
        self.with_locked_through(adjustment_request_json(&request))
    }

    fn decide_adjustment(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let request_id = required_str(args, "request_id")?;
        let decided_by = required_str(args, "decided_by")?;
        let approve = args
            .get("approve")
            .and_then(Value::as_bool)
            .ok_or_else(|| ToolError::new("missing or invalid \"approve\" (boolean)"))?;
        let note = args.get("note").and_then(Value::as_str).unwrap_or("");
        let now = Utc::now();
        let request = accountant::decide_adjustment(
            self.ledger,
            &self.company,
            request_id,
            decided_by,
            approve,
            note,
            now,
        )
        .map_err(accountant_err)?;
        self.with_locked_through(adjustment_request_json(&request))
    }

    fn bank_confirm_proposal(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let line_id = required_str(args, "line_id")?;
        let account = AccountId(required_str(args, "account")?.to_string());
        let class = args
            .get("class")
            .and_then(Value::as_str)
            .map(|s| ClassId(s.to_string()));
        // `Ledger::confirm_proposal` attributes the resulting document to a
        // fixed actor internally (`bank.rs`); `actor` is accepted here for
        // the same symmetry `reverse_entry` documents, and echoed back.
        let actor = required_str(args, "actor")?.to_string();
        let now = Utc::now();
        let entry_id = self
            .ledger
            .confirm_proposal(&self.company, line_id, &account, class, now)
            .map_err(ledger_err)?;
        self.with_locked_through(json!({ "entry_id": entry_id, "actor": actor }))
    }

    fn close_period(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let period_end = parse_date(args, "period_end")?;
        let actor = required_str(args, "actor")?;
        let note = args.get("note").and_then(Value::as_str).unwrap_or("");
        let now = Utc::now();
        self.ledger
            .close_period(&self.company, period_end, actor, note, now)
            .map_err(ledger_err)?;
        self.with_locked_through(json!({ "period_end": period_end.to_string() }))
    }

    /// Never quiet (D7, `crate::store::Ledger::reopen_period`): the store
    /// always writes an `is_reopen = 1` close-history row regardless of what
    /// `note` says, which is what makes this loud by design rather than by
    /// convention — the tool description says so up front rather than
    /// leaving a caller to discover it.
    fn reopen_period(&mut self, args: &Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let period_end = parse_date(args, "period_end")?;
        let actor = required_str(args, "actor")?;
        let note = args.get("note").and_then(Value::as_str).unwrap_or("");
        let now = Utc::now();
        self.ledger
            .reopen_period(&self.company, period_end, actor, note, now)
            .map_err(ledger_err)?;
        self.with_locked_through(json!({ "period_end": period_end.to_string(), "reopened": true }))
    }

    /// §12, D38: one SurePayroll run in, one flagged journal entry out
    /// (`crate::payroll`). Exactly one of `path`, `csv` or `runs` names the
    /// input; `config_path` overrides where `[payroll]` is read from
    /// (default `LEDGER_PAYROLL_CONFIG`, else `.local/config.toml`).
    /// `dry_run` builds and returns the entries without writing anything.
    fn import_payroll(&mut self, args: &mut Value) -> Result<Value, ToolError> {
        self.refuse_if_replaying()?;
        let actor = required_str(args, "actor")?.to_string();
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let config_path = args
            .get("config_path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(payroll::default_config_path);
        let config = payroll::PayrollConfig::load(&config_path).map_err(payroll_err)?;

        let runs_value = args.get_mut("runs").map(Value::take);
        let path = args.get("path").and_then(Value::as_str);
        let csv = args.get("csv").and_then(Value::as_str);
        let runs = match (path, csv, runs_value) {
            (Some(path), None, None) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|err| ToolError::new(format!("{path}: {err}")))?;
                payroll::parse_csv(&text).map_err(payroll_err)?
            }
            (None, Some(csv), None) => payroll::parse_csv(csv).map_err(payroll_err)?,
            (None, None, Some(mut runs)) => {
                normalize_money_fields(&mut runs);
                serde_json::from_value::<Vec<payroll::PayRun>>(runs)
                    .map_err(|err| ToolError::new(format!("invalid \"runs\": {err}")))?
            }
            _ => {
                return Err(ToolError::new(
                    "give exactly one of \"path\", \"csv\" or \"runs\"",
                ))
            }
        };

        if dry_run {
            let mut previews = Vec::with_capacity(runs.len());
            for run in &runs {
                let (doc, totals) = payroll::journal_document(run, &config).map_err(payroll_err)?;
                let entry =
                    match post::post(&doc, 1, &PostingContext::default()).map_err(post_err)? {
                        Posting::Entry(entry) => journal_entry_json(&entry),
                        Posting::NonPosting => Value::Null,
                    };
                previews.push(json!({
                    "run_id": run.run_id,
                    "pay_date": run.pay_date.to_string(),
                    "document_id": doc.document_id,
                    "status": "dry_run",
                    "totals": payroll_totals_json(&totals),
                    "entry": entry,
                }));
            }
            return self
                .with_locked_through(json!({ "dry_run": true, "runs": previews, "actor": actor }));
        }

        let outcomes = payroll::import_runs(
            self.ledger,
            &self.company,
            &runs,
            &config,
            &actor,
            Utc::now(),
        )
        .map_err(payroll_err)?;
        let mut results = Vec::with_capacity(outcomes.len());
        for outcome in &outcomes {
            let (entry, _) = self
                .ledger
                .entry(&self.company, &outcome.entry_id)
                .map_err(ledger_err)?;
            results.push(json!({
                "run_id": outcome.run_id,
                "pay_date": outcome.pay_date.to_string(),
                "document_id": outcome.document_id,
                "status": outcome.status.as_str(),
                "entry_id": outcome.entry_id,
                "totals": payroll_totals_json(&outcome.totals),
                "entry": journal_entry_json(&entry),
            }));
        }
        self.with_locked_through(json!({ "dry_run": false, "runs": results, "actor": actor }))
    }
}

// ---------------------------------------------------------------------------
// Tool schemas
// ---------------------------------------------------------------------------

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "trial_balance",
            description: "Every account's debit and credit total and signed balance as of a \
                date, plus which accounts sit on the side opposite their normal balance. \
                `LEDGER-DESIGN.md` §7.",
            input_schema: tool_schema(
                json!({ "as_of": date_prop("The date to balance as of, YYYY-MM-DD.") }),
                &["as_of"],
            ),
        },
        ToolSpec {
            name: "profit_and_loss",
            description: "Income, cost of goods sold and expense between two dates inclusive, \
                broken out by account and by class, with gross margin and net income.",
            input_schema: tool_schema(
                json!({
                    "from": date_prop("Start date, YYYY-MM-DD, inclusive."),
                    "to": date_prop("End date, YYYY-MM-DD, inclusive."),
                }),
                &["from", "to"],
            ),
        },
        ToolSpec {
            name: "balance_sheet",
            description: "Assets, liabilities and equity as of a date, with the current \
                calendar year's net income folded into equity as its own row so the sheet \
                balances without a formal closing entry.",
            input_schema: tool_schema(
                json!({ "as_of": date_prop("The date to report as of, YYYY-MM-DD.") }),
                &["as_of"],
            ),
        },
        ToolSpec {
            name: "sales_tax_lines",
            description: "The quarterly sales-tax filing lines (`LEDGER-DESIGN.md` §9): total \
                income, tax collected, taxable and non-taxable sales derived two ways (from the \
                tax account and from line-level taxability), the variance between them, and the \
                ST-50 lines. A non-zero variance is expected, not a bug — it is what Dan checks \
                before filing.",
            input_schema: tool_schema(
                json!({
                    "year": { "type": "integer", "description": "Calendar year, e.g. 2026." },
                    "quarter": {
                        "type": "integer", "minimum": 1, "maximum": 4,
                        "description": "1 through 4.",
                    },
                    "rate": {
                        "type": "string",
                        "description": "The sales tax rate as a decimal string, e.g. \
                            \"0.06625\". Defaults to the ledger's configured NJ rate.",
                    },
                }),
                &["year", "quarter"],
            ),
        },
        ToolSpec {
            name: "general_ledger",
            description: "Every posted line between two dates inclusive, grouped by account (or \
                just one account, when given), each with a running balance starting from the \
                balance carried in from before the range. `LEDGER-DESIGN.md` §8.",
            input_schema: tool_schema(
                json!({
                    "from": date_prop("Start date, YYYY-MM-DD, inclusive."),
                    "to": date_prop("End date, YYYY-MM-DD, inclusive."),
                    "account": {
                        "type": "string",
                        "description": "Restrict to one account number, e.g. \"1200\". Omit \
                            for every account.",
                    },
                }),
                &["from", "to"],
            ),
        },
        ToolSpec {
            name: "audit_trail",
            description: "Every posting since a date (or since the last period close, when \
                omitted), flagged (manual and imported) entries first, with who posted it and \
                when — the list a CPA actually reads. `LEDGER-DESIGN.md` §8.",
            input_schema: tool_schema(
                json!({
                    "since": date_prop(
                        "Earliest entry date to include, YYYY-MM-DD. Omit to use the company's \
                         current locked_through date."
                    ),
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "entries_for_document",
            description: "Every journal entry ever posted from one document — normally one, \
                plus a reversal if the document was corrected or voided.",
            input_schema: tool_schema(
                json!({
                    "document_id": {
                        "type": "string",
                        "description": "The document's id, as saved by save_and_post_document.",
                    },
                }),
                &["document_id"],
            ),
        },
        ToolSpec {
            name: "list_adjustments",
            description: "The adjusting-entry request queue (`LEDGER-DESIGN.md` §8): every \
                request an accountant has proposed, optionally filtered to one state, newest \
                first. Each row states whether its effective date now falls in a closed period, \
                since approving it would need a reopen first.",
            input_schema: tool_schema(
                json!({
                    "state": {
                        "type": "string",
                        "enum": ["proposed", "approved", "rejected", "posted"],
                        "description": "Restrict to one state. Omit for every request.",
                    },
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "bank_status",
            description: "One imported bank statement's header (period, opening/closing \
                balance, whether it is closed) plus every line on it and a match summary — how \
                many lines are cleared, still pending confirmation as a proposal, or entirely \
                unmatched. `LEDGER-DESIGN.md` §10.",
            input_schema: tool_schema(
                json!({
                    "statement_id": {
                        "type": "string",
                        "description": "The statement's id, as returned when it was imported.",
                    },
                }),
                &["statement_id"],
            ),
        },
        ToolSpec {
            name: "locked_through",
            description: "The date this company's period gate is locked through, or null if \
                no period has ever been closed. No entry may post on or before this date; every \
                write tool's result also carries this field so a caller always sees the gate.",
            input_schema: tool_schema(json!({}), &[]),
        },
        ToolSpec {
            name: "list_contacts",
            description: "Customers and vendors in this book (D28), by name. `kind` narrows to \
                \"customer\" or \"vendor\"; `query` matches anywhere in the display name, \
                company name or email, case-insensitive. Inactive contacts only with \
                `include_inactive`.",
            input_schema: tool_schema(
                json!({
                    "kind": { "type": "string", "enum": ["customer", "vendor"] },
                    "query": { "type": "string" },
                    "include_inactive": { "type": "boolean" },
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "list_items",
            description: "Products and services in this book (D28), by name, with their \
                accounts, default class, prices and unit cost. `query` matches anywhere in the \
                SKU, name or description, case-insensitive.",
            input_schema: tool_schema(
                json!({
                    "query": { "type": "string" },
                    "include_inactive": { "type": "boolean" },
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "open_documents",
            description: "Every invoice and credit memo (`book` \"receivable\") or bill and \
                vendor credit (`book` \"payable\") with a balance still open as of `as_of`, \
                oldest first: total, applied, open, due date. Credits are negative.",
            input_schema: tool_schema(
                json!({
                    "book": { "type": "string", "enum": ["receivable", "payable"] },
                    "as_of": date_prop("YYYY-MM-DD."),
                }),
                &["book", "as_of"],
            ),
        },
        ToolSpec {
            name: "open_sales_orders",
            description: "Sales orders (D42) with what is still to be invoiced, oldest first: \
                `document_id`, `number`, `contact_id`, `contact_name`, `txn_date`, `subtotal` \
                (lines less discounts, before tax), `invoiced` (what invoices made from it \
                took), `open`, `invoice_ids`, `memo`, `source_ref` (the QBO sales order id \
                when the cutover import carried it). Only orders with something open unless \
                `all`; voided orders never. `as_of` (YYYY-MM-DD) counts orders and invoices \
                dated on or before it; omitted, every one.",
            input_schema: tool_schema(
                json!({
                    "as_of": date_prop("YYYY-MM-DD. Omit for today and beyond."),
                    "all": { "type": "boolean", "description": "Include fully invoiced orders." },
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "aging",
            description: "AR (`book` \"receivable\") or AP (`book` \"payable\") aging as of \
                a date: one row per contact with current, 1-30, 31-60, 61-90 and over-90 days \
                past due, plus totals. Ties to the 1200 / 2000 balance on the trial balance.",
            input_schema: tool_schema(
                json!({
                    "book": { "type": "string", "enum": ["receivable", "payable"] },
                    "as_of": date_prop("YYYY-MM-DD."),
                }),
                &["book", "as_of"],
            ),
        },
        ToolSpec {
            name: "get_document",
            description: "The newest saved version of one document, as an editor needs it: \
                `kind`, `number`, `txn_date`, `due_date`, `contact`, `header_class`, `lines` \
                (each `line_no`, `kind`, `amount`, `class`, `item_id`, `account`, \
                `is_taxable`, `qty`, `description`), `tax`, `deposit_to`, `pay_from`, \
                `applications`, `unapplied`, `is_voided`, `memo`, `source_ref`, plus \
                `version` (how many versions are saved) and `total`.",
            input_schema: tool_schema(
                json!({ "document_id": { "type": "string" } }),
                &["document_id"],
            ),
        },
        ToolSpec {
            name: "upsert_contact",
            description: "Creates or replaces a customer or vendor. `contact`: `contact_id`, \
                `kind` (\"Customer\" or \"Vendor\"), `display_name` (unique per kind), and \
                optionally `company_name`, `email`, `phone`, `bill_address`, `ship_address`, \
                `terms_days` (net days; null is due on receipt), `is_tax_exempt`, \
                `default_class`, `is_active`, `source_ref` (the QBO id when imported).",
            input_schema: tool_schema(json!({ "contact": { "type": "object" } }), &["contact"]),
        },
        ToolSpec {
            name: "upsert_item",
            description: "Creates or replaces a product or service. `item`: `item_id`, `name`, \
                `item_type` (\"inventory\", \"non_inventory\", \"service\"), \
                `income_account`, `expense_account`, `asset_account` (inventory items only, and \
                required for them), and optionally `sku` (unique), `description`, \
                `default_class`, `sales_price`, `purchase_cost`, `unit_cost`, `is_taxable`, \
                `is_active`, `source_ref`. Every account and class must exist.",
            input_schema: tool_schema(json!({ "item": { "type": "object" } }), &["item"]),
        },
        ToolSpec {
            name: "create_document",
            description: "The standalone way to enter a document (D28): an invoice, estimate, \
                sales order (`kind` \"SalesOrder\", D42; never posts — see \
                convert_to_invoice), sales receipt, credit memo, purchase order, bill, payment, bill payment and the \
                rest. Send only what a person would type: `kind`, `txn_date`, `contact` \
                ({\"kind\":\"Customer\" or \"Vendor\",\"id\":...}), and `lines` (each \
                `kind`, `amount`, and `item_id` for Item, Shipping and Discount lines, \
                `account` for Account lines, optionally `qty`, `description`, `class`, \
                `is_taxable`); payments add `applications` \
                ([{\"target_document_id\",\"target_kind\",\"amount\"}]). Every other \
                LedgerDocument field may be left out. The contact must exist, be active and be \
                the right kind; every item must exist. Fills in: `document_id` (new), `number` \
                (next in sequence for invoices, estimates, sales orders, sales receipts, credit memos, refund \
                receipts and purchase orders; a number already used is refused), `due_date` \
                (from the contact's terms), `header_class` (the contact's default), and `tax` on \
                sales (taxable lines at the NJ rate, zero for an exempt customer). Then saves \
                and posts. Refused, with nothing saved, while an import replay is in progress, \
                on a closed period, a missing class or an unbalanced journal. Returns \
                `document_id`, `version`, `number`, `due_date`, `total`, `tax`, `entry_id` and \
                `entry`.",
            input_schema: tool_schema(
                json!({
                    "document": {
                        "type": "object",
                        "description": "The document as typed — see this tool's description.",
                    },
                    "actor": {
                        "type": "string",
                        "description": "Who is entering this document, recorded on the oplog.",
                    },
                }),
                &["document", "actor"],
            ),
        },
        ToolSpec {
            name: "convert_to_invoice",
            description: "Makes the invoice for a sales order (D42) and closes what it takes: \
                copies the order's customer, class, memo and lines (every line, or only \
                `lines` — [{\"line_no\", \"qty\"}], `qty` optional, a smaller quantity \
                scaling the amount), then numbers, dates due, taxes, saves and posts it \
                exactly as create_document does. The invoice carries an application to the \
                sales order for the pre-tax subtotal it took: a link, never money. Invoicing \
                the whole order closes it; to drop a remainder nobody will invoice, void the \
                sales order. Refused: not a sales order, voided, nothing left to invoice, \
                taking more than is open, no `lines` on an order already partly invoiced, a \
                closed period, an import replay in progress. Returns create_document's \
                fields plus `sales_order_id`, `sales_order_open` and `sales_order_closed`.",
            input_schema: tool_schema(
                json!({
                    "sales_order_id": { "type": "string" },
                    "txn_date": date_prop("The invoice date, YYYY-MM-DD."),
                    "lines": {
                        "type": "array",
                        "description": "Which order lines to invoice; omit for all.",
                        "items": { "type": "object" },
                    },
                    "number": { "type": "string", "description": "Omit for the next invoice number." },
                    "memo": { "type": "string", "description": "Omit to keep the order's memo." },
                    "actor": { "type": "string", "description": "Who is invoicing it." },
                }),
                &["sales_order_id", "txn_date", "actor"],
            ),
        },
        ToolSpec {
            name: "update_document",
            description: "Edits a saved document (`LEDGER-DESIGN.md` §4/§6: a correction is a \
                reversal, never an overwrite). `document` is what create_document takes, with \
                the saved `document_id`; it is checked and filled the same way, and saved as \
                the next version. When what it posts changed, the old entry is reversed (dated \
                as the entry it reverses) and the new one posted, in one all-or-nothing unit; \
                when it did not (a memo edit), the entry stands. The number is kept: leave it \
                out, or send the same one. Leave `applications` out to keep the saved ones \
                (`[]` clears them). Refused, with nothing changed: a voided document, a \
                change of `kind` or of contact type (customer to vendor), renumbering a \
                document the book numbers, a total below what is already applied against it, \
                an application beyond what is open on its target, either the old or the new \
                date in a closed period, and while an import replay is in progress. Returns \
                `document_id`, `version`, `number`, `due_date`, `total`, `tax`, `is_voided`, \
                `reversal_entry_ids`, `entry_id` and `entry`.",
            input_schema: tool_schema(
                json!({
                    "document": {
                        "type": "object",
                        "description": "The edited document, `document_id` included — see \
                            create_document for the shape.",
                    },
                    "actor": { "type": "string", "description": "Who is editing it." },
                }),
                &["document", "actor"],
            ),
        },
        ToolSpec {
            name: "void_document",
            description: "Voids a saved document (§6: it posts a reversal rather than \
                vanishing). Saves a new version with `is_voided` true and `Voided: <reason>` \
                on its memo, and reverses every live entry as of that entry's own date, \
                `VOID: ... (<reason>)` on the reversal. A voided invoice or bill drops off \
                open_documents and aging; a voided payment or credit memo releases what it \
                applied. Refused: already voided; an invoice or bill with a payment or credit \
                still applied against it (void or edit that first); a closed period; an import \
                replay in progress. Returns the same fields as update_document.",
            input_schema: tool_schema(
                json!({
                    "document_id": { "type": "string" },
                    "reason": { "type": "string", "description": "Why. Required, not empty." },
                    "actor": { "type": "string", "description": "Who is voiding it." },
                }),
                &["document_id", "reason", "actor"],
            ),
        },
        ToolSpec {
            name: "apply_credit",
            description: "Applies an open credit memo to an open invoice of the same customer \
                (or an open vendor credit to an open bill of the same vendor): a new version \
                of the credit with the application added, which open_documents and aging net \
                on both sides. The credit posts the same either way, so no entry moves. \
                `amount` defaults to the smaller of the two open balances and may not exceed \
                either. Refused on a voided document, a different contact, a credit dated in a \
                closed period, and while an import replay is in progress. Returns the same \
                fields as update_document plus `target_document_id` and `applied` (the \
                credit's total now applied to that target).",
            input_schema: tool_schema(
                json!({
                    "credit_id": {
                        "type": "string",
                        "description": "The CreditMemo or VendorCredit document.",
                    },
                    "target_document_id": {
                        "type": "string",
                        "description": "The Invoice or Bill to apply it to.",
                    },
                    "amount": {
                        "type": ["string", "integer"],
                        "description": "A two-place decimal string or minor units. Omit for \
                            as much as both have open.",
                    },
                    "actor": { "type": "string", "description": "Who is applying it." },
                }),
                &["credit_id", "target_document_id", "actor"],
            ),
        },
        ToolSpec {
            name: "payment_link",
            description: "An Authorize.net hosted payment page for one invoice (D15, D30, \
                docs/AUTHNET.md). Sends the invoice's own number as invoiceNumber and its open \
                balance as the amount, so the settlement import applies the customer's payment \
                back to this invoice. Refused for a document that is not an invoice, a voided \
                invoice, a fully paid one, or one with no number; refused while an import \
                replay is in progress. Writes nothing to the ledger and moves no money: the \
                customer pays on Authorize.net's page. The token lasts 15 minutes and must be \
                POSTed to `url` as form field `token` — `html` is a page that does that on \
                open. Returns `document_id`, `invoice_number`, `amount`, `environment` \
                (\"sandbox\" unless production is enabled), `url`, `token`, `expires_at` and \
                `html`.",
            input_schema: tool_schema(
                json!({
                    "document_id": {
                        "type": "string",
                        "description": "The invoice's document_id (create_document returns it).",
                    },
                }),
                &["document_id"],
            ),
        },
        ToolSpec {
            name: "save_and_post_document",
            description: "Saves a document (a new version if `document_id` already exists) and, \
                unless it is an Estimate, SalesOrder or PurchaseOrder (contracts, not transactions), posts \
                the balanced journal entry the posting-rules table (`LEDGER-DESIGN.md` §1) \
                derives from it. `document` is a full LedgerDocument: at minimum \
                `document_id`, `kind` (\"Invoice\", \"SalesReceipt\", \"Bill\", \"Payment\", \
                \"JournalEntry\", and the rest of DocKind's variants, exactly as spelled in \
                Rust), `number`, `txn_date` (YYYY-MM-DD), `contact` ({\"kind\":\"Customer\" or \
                \"Vendor\",\"id\":...} or null), `header_class`, `lines` (each a DocLine: \
                `line_no`, `kind` — \"Item\"/\"Account\"/\"Discount\"/\"Shipping\"/\
                \"Description\"/\"Journal\" — `amount`, `class`, `item_id`, `account`, \
                `is_taxable`, `qty`, `unit_cost`, `description`, `posting` — \"Debit\"/\"Credit\", \
                Journal lines only — `entity`), `tax` (or null), `deposit_to`, `pay_from`, \
                `applications`, `unapplied`, `is_voided`, `source_ref`, `memo`. A field the \
                document truly has none of is still required by name — pass `null` or `[]`, not \
                an absent key. Money fields anywhere in `document` (line `amount`/`unit_cost`, \
                `tax.total_tax`/`taxable_base`, `unapplied`) accept a two-place decimal string \
                or a minor-unit integer. Because an MCP call has no replica to look items up in, \
                any Item line's item must be described inline via `items`: a map of item id to \
                `{\"income\":account,\"expense\":account,\"asset\":account or null,\
                \"default_class\":class or null,\"unit_cost\":money or null}`. Refused while an \
                import replay is in progress, and on a closed period, a missing class, an \
                unknown item, or an unbalanced journal — each as isError with the reason, never \
                a JSON-RPC error. Returns `document_id`, `version`, `entry_id` (null for a \
                non-posting document), and `entry`.",
            input_schema: tool_schema(
                json!({
                    "document": {
                        "type": "object",
                        "description": "A full LedgerDocument JSON object — see this tool's \
                            description for the exact shape.",
                    },
                    "actor": {
                        "type": "string",
                        "description": "Who is saving this document, recorded on the oplog \
                            command.",
                    },
                    "items": {
                        "type": "object",
                        "description": "Item id to ItemAccounts JSON, for every item any Item \
                            line in `document` references. Omit when `document` has no Item \
                            lines.",
                    },
                }),
                &["document", "actor"],
            ),
        },
        ToolSpec {
            name: "reverse_entry",
            description: "The only correction path (`LEDGER-DESIGN.md` §4): posts a new entry \
                with the same lines, sides swapped, dated `on`, referencing the original. \
                Refused while an import replay is in progress, and if `on` falls in a closed \
                period.",
            input_schema: tool_schema(
                json!({
                    "entry_id": {
                        "type": "string",
                        "description": "The posted entry to reverse.",
                    },
                    "on": date_prop("The reversal's own entry date, YYYY-MM-DD."),
                    "actor": { "type": "string", "description": "Who is reversing this entry." },
                }),
                &["entry_id", "on", "actor"],
            ),
        },
        ToolSpec {
            name: "propose_adjustment",
            description: "Queues an adjusting entry for Dan to decide (`LEDGER-DESIGN.md` §8). \
                `lines` is an array of `{\"account\":number,\"class\":class id or omitted, \
                \"debit\":money,\"credit\":money,\"memo\":string or omitted,\
                \"entity\":{\"kind\":\"customer\" or \"vendor\",\"id\":...} or omitted}` — \
                exactly one of `debit`/`credit` per line, non-zero, and the whole set must \
                balance and follow the class rule (an income/COGS/expense account needs a \
                class, a balance-sheet account may not carry one) or the proposal is rejected \
                immediately rather than at approval time. Refused while an import replay is in \
                progress.",
            input_schema: tool_schema(
                json!({
                    "requested_by": {
                        "type": "string",
                        "description": "The accountant proposing this adjustment.",
                    },
                    "description": {
                        "type": "string",
                        "description": "Why this adjustment is needed.",
                    },
                    "lines": {
                        "type": "array",
                        "description": "See this tool's description for each line's shape.",
                        "items": { "type": "object" },
                    },
                }),
                &["requested_by", "description", "lines"],
            ),
        },
        ToolSpec {
            name: "decide_adjustment",
            description: "Decides one proposed adjustment. Rejecting only records the decision. \
                Approving saves the request's lines as a flagged JournalEntry document and \
                posts it through the same period gate and class rule every other document \
                goes through — approving a request whose date now falls in a closed period \
                fails with the ordinary period-closed error, exactly as it would for any other \
                document. Refused while an import replay is in progress.",
            input_schema: tool_schema(
                json!({
                    "request_id": { "type": "string", "description": "The proposal to decide." },
                    "decided_by": { "type": "string", "description": "Who is deciding this." },
                    "approve": {
                        "type": "boolean",
                        "description": "true to approve and post; false to reject.",
                    },
                    "note": {
                        "type": "string",
                        "description": "Why. Recorded either way; defaults to empty.",
                    },
                }),
                &["request_id", "decided_by", "approve"],
            ),
        },
        ToolSpec {
            name: "bank_confirm_proposal",
            description: "Turns a bank line's suggested proposal into a posted BankLine \
                document against the account (and class, if it needs one) the caller confirms, \
                then clears that line. Refused while an import replay is in progress, on a \
                closed statement, and on a line that is not currently a pending proposal.",
            input_schema: tool_schema(
                json!({
                    "line_id": {
                        "type": "string",
                        "description": "The bank line carrying the proposal to confirm.",
                    },
                    "account": {
                        "type": "string",
                        "description": "The account number to post the other side of this \
                            line to.",
                    },
                    "class": {
                        "type": "string",
                        "description": "Required when `account` is an income, COGS or expense \
                            account; omit for a balance-sheet account.",
                    },
                    "actor": {
                        "type": "string",
                        "description": "Who is confirming this proposal. Echoed back in the \
                            result; the ledger itself attributes the resulting document to its \
                            own fixed operator today.",
                    },
                }),
                &["line_id", "account", "actor"],
            ),
        },
        ToolSpec {
            name: "close_period",
            description: "Locks the book through `period_end`: no entry may post on or before \
                this date afterward without a reopen first. Snapshots the trial balance as of \
                that date before locking. Refused while an import replay is in progress.",
            input_schema: tool_schema(
                json!({
                    "period_end": date_prop("The date to lock through, YYYY-MM-DD."),
                    "actor": { "type": "string", "description": "Who is closing this period." },
                    "note": { "type": "string", "description": "Why. Defaults to empty." },
                }),
                &["period_end", "actor"],
            ),
        },
        ToolSpec {
            name: "reopen_period",
            description: "Reopening is loud by design (D7): it always writes a close-history \
                entry with is_reopen = 1, whatever `note` says, so a reopen is never silent or \
                accidental — there is no quiet way to relock a period back to where it was. \
                Refused while an import replay is in progress; fails if `period_end` was never \
                closed.",
            input_schema: tool_schema(
                json!({
                    "period_end": date_prop("The closed date to reopen, YYYY-MM-DD."),
                    "actor": { "type": "string", "description": "Who is reopening this period." },
                    "note": { "type": "string", "description": "Why. Defaults to empty." },
                }),
                &["period_end", "actor"],
            ),
        },
        ToolSpec {
            name: "import_payroll",
            description: "Journal one or more SurePayroll pay runs (LEDGER-DESIGN.md §12, \
                D38, docs/PAYROLL.md). Each run becomes one JournalEntry document \
                `payroll:<pay_date>:<run_id>` and one flagged entry on the check date: Dr wages \
                by class (5400 unless the mapping says otherwise), Dr employer taxes and \
                employer deductions, Cr 2400 for every withholding, deduction and employer tax, \
                Cr 1100 (or the configured bank) for net pay. Employee → class comes from \
                [payroll] in the config; any unmapped employee or department refuses the whole \
                import, as does a stated net pay that does not match to the cent, or a check \
                date in a closed period. All or nothing across runs. Idempotent: a run already \
                posted with the same figures comes back `already_imported`; with different \
                figures it is refused until that entry is reversed. `dry_run` returns the \
                entries without writing. Refused while an import replay is in progress.",
            input_schema: tool_schema(
                json!({
                    "actor": { "type": "string", "description": "Who is importing." },
                    "path": {
                        "type": "string",
                        "description": "A pay-run lines CSV on disk (docs/PAYROLL.md).",
                    },
                    "csv": {
                        "type": "string",
                        "description": "The same CSV as text, instead of `path`.",
                    },
                    "runs": {
                        "type": "array",
                        "description": "Runs as JSON instead of a CSV: [{run_id, pay_date, \
                            period_start?, period_end?, lines: [{employee, department?, \
                            section, item, amount}]}]. Sections: earning, noncash_earning, \
                            employee_tax, employee_deduction, employer_tax, \
                            employer_deduction, net_pay.",
                        "items": { "type": "object" },
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Build and return the entries; write nothing.",
                    },
                    "config_path": {
                        "type": "string",
                        "description": "Config holding [payroll]; default \
                            $LEDGER_PAYROLL_CONFIG or .local/config.toml.",
                    },
                }),
                &["actor"],
            ),
        },
    ]
}

fn tool_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn date_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

// ---------------------------------------------------------------------------
// Rendering: crate types to MCP-shaped JSON. Money always as a two-place
// decimal string, matching `qbo_local::mcp`'s own convention.
// ---------------------------------------------------------------------------

/// Renders [`Money`] as a decimal string with two places — `"1234.56"`,
/// negative as `"-12.00"` — for MCP tool output, whose consumers are LLM
/// skills reading text and JSON, not the bare minor-units integer `Money`'s
/// own `Serialize` impl produces.
fn money_str(money: Money) -> String {
    let minor = money.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn class_id_json(class: &Option<ClassId>) -> Value {
    json!(class.as_ref().map(|class| class.0.clone()))
}

fn contact_ref_json(entity: &Option<ContactRef>) -> Value {
    match entity {
        Some(entity) => json!({
            "kind": match entity.kind {
                ContactKind::Customer => "customer",
                ContactKind::Vendor => "vendor",
            },
            "id": entity.id,
        }),
        None => Value::Null,
    }
}

fn tb_row_json(row: &report::TbRow) -> Value {
    json!({
        "account_id": row.account_id.0,
        "number": row.number,
        "name": row.name,
        "classification": row.classification.as_str(),
        "is_contra": row.is_contra,
        "needs_mapping": row.needs_mapping,
        "source_ref": row.source_ref,
        "debit": money_str(row.debit),
        "credit": money_str(row.credit),
        "balance": money_str(row.balance),
    })
}

fn trial_balance_json(tb: &report::TrialBalance) -> Value {
    json!({
        "rows": tb.rows.iter().map(tb_row_json).collect::<Vec<_>>(),
        "total_debits": money_str(tb.total_debits),
        "total_credits": money_str(tb.total_credits),
        "wrong_side": tb.wrong_side.iter().map(|a| json!(a.0)).collect::<Vec<_>>(),
    })
}

fn by_section_json(section: &report::BySection) -> Value {
    json!({
        "by_account": section.by_account.iter().map(|(account, amount)| json!({
            "account_id": account.0,
            "amount": money_str(*amount),
        })).collect::<Vec<_>>(),
        "by_class": section.by_class.iter().map(|(class, amount)| json!({
            "class_id": class.as_ref().map(|c| c.0.clone()),
            "amount": money_str(*amount),
        })).collect::<Vec<_>>(),
        "total": money_str(section.total),
    })
}

fn pnl_json(pnl: &report::Pnl) -> Value {
    json!({
        "income": by_section_json(&pnl.income),
        "cogs": by_section_json(&pnl.cogs),
        "expense": by_section_json(&pnl.expense),
        "gross_margin": money_str(pnl.gross_margin),
        "net_income": money_str(pnl.net_income),
    })
}

fn bs_row_json(row: &report::BsRow) -> Value {
    json!({
        "account_id": row.account_id.as_ref().map(|a| a.0.clone()),
        "name": row.name,
        "balance": money_str(row.balance),
    })
}

fn bs_section_json(section: &report::BsSection) -> Value {
    json!({
        "rows": section.rows.iter().map(bs_row_json).collect::<Vec<_>>(),
        "total": money_str(section.total),
    })
}

fn balance_sheet_json(bs: &report::BalanceSheet) -> Value {
    json!({
        "assets": bs_section_json(&bs.assets),
        "liabilities": bs_section_json(&bs.liabilities),
        "equity": bs_section_json(&bs.equity),
        "total_liabilities_and_equity": money_str(bs.total_liabilities_and_equity),
    })
}

fn sales_tax_lines_json(lines: &report::SalesTaxLines) -> Value {
    json!({
        "a_total_income": money_str(lines.a_total_income),
        "b_tax_collected": money_str(lines.b_tax_collected),
        "c_taxable_sales": money_str(lines.c_taxable_sales),
        "d_nontaxable_sales": money_str(lines.d_nontaxable_sales),
        "e_line_level_taxable": lines.e_line_level_taxable.map(money_str),
        "variance": lines.variance.map(money_str),
        "st50": lines.st50.iter().map(|(line, amount)| json!({
            "line": line,
            "amount": money_str(*amount),
        })).collect::<Vec<_>>(),
    })
}

fn gl_line_json(line: &accountant::GlLine) -> Value {
    json!({
        "entry_id": line.entry_id,
        "entry_date": line.entry_date.to_string(),
        "source_type": line.source_type.as_str(),
        "source_id": line.source_id,
        "memo": line.memo,
        "class": class_id_json(&line.class),
        "entity": contact_ref_json(&line.entity),
        "is_flagged": line.is_flagged,
        "reversal_of": line.reversal_of,
        "debit": money_str(line.debit),
        "credit": money_str(line.credit),
        "running_balance": money_str(line.running_balance),
    })
}

fn gl_section_json(section: &accountant::GlAccountSection) -> Value {
    json!({
        "account_id": section.account_id.0,
        "number": section.number,
        "name": section.name,
        "opening_balance": money_str(section.opening_balance),
        "lines": section.lines.iter().map(gl_line_json).collect::<Vec<_>>(),
        "closing_balance": money_str(section.closing_balance),
    })
}

fn gl_detail_json(detail: &accountant::GlDetail) -> Value {
    json!({ "sections": detail.sections.iter().map(gl_section_json).collect::<Vec<_>>() })
}

fn audit_row_json(row: &accountant::AuditRow) -> Value {
    json!({
        "entry_id": row.entry_id,
        "entry_date": row.entry_date.to_string(),
        "source_type": row.source_type.as_str(),
        "source_id": row.source_id,
        "source_version": row.source_version,
        "memo": row.memo,
        "is_flagged": row.is_flagged,
        "reversal_of": row.reversal_of,
        "actor": row.actor,
        "command_kind": row.command_kind,
        "at": row.at.to_rfc3339(),
    })
}

fn journal_line_json(line: &JournalLine) -> Value {
    json!({
        "line_no": line.line_no,
        "account": line.account.0,
        "class": class_id_json(&line.class),
        "debit": money_str(line.debit),
        "credit": money_str(line.credit),
        "memo": line.memo,
        "entity": contact_ref_json(&line.entity),
        "is_taxable": line.is_taxable,
        "tax_amount": line.tax_amount.map(money_str),
        "tax_rate": line.tax_rate.map(|rate| rate.to_string()),
    })
}

fn journal_entry_json(entry: &JournalEntry) -> Value {
    json!({
        "entry_date": entry.entry_date.to_string(),
        "memo": entry.memo,
        "source_type": entry.source_type.as_str(),
        "source_id": entry.source_id,
        "source_version": entry.source_version,
        "reversal_of": entry.reversal_of,
        "is_flagged": entry.is_flagged,
        "lines": entry.lines.iter().map(journal_line_json).collect::<Vec<_>>(),
    })
}

fn adjustment_request_json(request: &accountant::AdjustmentRequest) -> Value {
    json!({
        "request_id": request.request_id,
        "requested_by": request.requested_by,
        "requested_at": request.requested_at.to_rfc3339(),
        "description": request.description,
        "lines": request.lines.iter().map(journal_line_json).collect::<Vec<_>>(),
        "state": request.state.as_str(),
        "decided_by": request.decided_by,
        "decided_at": request.decided_at.map(|at| at.to_rfc3339()),
        "decision_note": request.decision_note,
        "posted_entry_id": request.posted_entry_id,
        "period_closed": request.period_closed,
    })
}

fn bank_statement_json(statement: &crate::store::BankStatementRow) -> Value {
    json!({
        "statement_id": statement.statement_id,
        "account_id": statement.account_id.0,
        "period_start": statement.period_start.to_string(),
        "period_end": statement.period_end.to_string(),
        "opening_balance": money_str(statement.opening_balance),
        "closing_balance": money_str(statement.closing_balance),
        "imported_at": statement.imported_at,
        "closed_at": statement.closed_at,
    })
}

fn bank_line_json(line: &crate::store::BankLineRow) -> Value {
    json!({
        "line_id": line.line_id,
        "statement_id": line.statement_id,
        "account_id": line.account_id.0,
        "posted_on": line.posted_on.to_string(),
        "amount": money_str(line.amount),
        "description": line.description,
        "external_id": line.external_id,
        "matched_entry_id": line.matched_entry_id,
        "matched_line_no": line.matched_line_no,
        "match_kind": line.match_kind,
        "matched_at": line.matched_at,
    })
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Field names that hold a [`Money`] value somewhere in a tool argument's
/// JSON tree. [`normalize_money_fields`] walks for exactly these — chosen
/// because every one of them is a `Money`-typed field in `crate::types` or
/// `crate::store` and none of them collides with an unrelated field sharing
/// the name elsewhere in this crate's document shapes.
const MONEY_FIELDS: &[&str] = &[
    "amount",
    "unit_cost",
    "sales_price",
    "purchase_cost",
    "total_tax",
    "taxable_base",
    "unapplied",
    "debit",
    "credit",
    "tax_amount",
    "opening_balance",
    "closing_balance",
];

/// Rewrites every [`MONEY_FIELDS`] value in `value`'s JSON tree that is
/// spelled as a two-place decimal string into the bare minor-unit integer
/// [`Money`]'s own wire format uses, in place. A value already numeric, or a
/// field this function does not recognise, is left untouched — this is a
/// convenience for callers who find a decimal string more natural, not a
/// general schema coercion.
fn normalize_money_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if MONEY_FIELDS.contains(&key.as_str()) {
                    if let Value::String(raw) = entry {
                        if let Ok(minor) = decimal_str_to_minor(raw) {
                            *entry = json!(minor);
                        }
                    }
                }
                normalize_money_fields(entry);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_money_fields(item);
            }
        }
        _ => {}
    }
}

/// A two-place (or fewer) decimal string to minor units — `"1234.56"` to
/// `123456`, `"-12"` to `-1200`. More than two decimal places is refused
/// (D9's over-precision rule) rather than rounded silently, matching
/// `crate::bank::ParseError::Precision`'s stance on the same question.
fn decimal_str_to_minor(raw: &str) -> Result<i64, ()> {
    let decimal = Decimal::from_str(raw).map_err(|_| ())?;
    if decimal.scale() > 2 {
        return Err(());
    }
    (decimal * Decimal::new(100, 0)).try_into().map_err(|_| ())
}

fn value_to_money(value: &Value) -> Result<Money, ToolError> {
    value
        .as_i64()
        .map(Money::from_minor)
        .ok_or_else(|| ToolError::new(format!("expected a money amount, got {value}")))
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::new(format!("missing or invalid \"{key}\"")))
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

/// `ItemAccounts` derives neither `Serialize` nor `Deserialize` (it is
/// posting-context plumbing, `LEDGER-DESIGN.md` §1, not a wire type), so
/// `save_and_post_document`'s inline `items` map is parsed by hand rather
/// than through `serde_json::from_value`.
fn parse_item_accounts(raw: &Value) -> Result<ItemAccounts, ToolError> {
    let income = AccountId(required_str(raw, "income")?.to_string());
    let expense = AccountId(required_str(raw, "expense")?.to_string());
    let asset = raw
        .get("asset")
        .and_then(Value::as_str)
        .map(|s| AccountId(s.to_string()));
    let default_class = raw
        .get("default_class")
        .and_then(Value::as_str)
        .map(|s| ClassId(s.to_string()));
    let unit_cost = match raw.get("unit_cost") {
        None | Some(Value::Null) => None,
        Some(other) => Some(value_to_money(other)?),
    };
    Ok(ItemAccounts {
        income,
        expense,
        asset,
        default_class,
        unit_cost,
    })
}

fn parse_contact_ref(raw: &Value) -> Result<ContactRef, ToolError> {
    let kind = required_str(raw, "kind")?;
    let kind = match kind.to_ascii_lowercase().as_str() {
        "customer" => ContactKind::Customer,
        "vendor" => ContactKind::Vendor,
        _ => {
            return Err(ToolError::new(format!(
                "entity.kind must be \"customer\" or \"vendor\", got {kind:?}"
            )))
        }
    };
    let id = required_str(raw, "id")?.to_string();
    Ok(ContactRef { kind, id })
}

/// A friendlier shape than `JournalLine`'s own field set (which also carries
/// tax bookkeeping that never applies to a manual adjustment): `account`,
/// exactly one of `debit`/`credit`, and optional `class`/`memo`/`entity`.
fn parse_adjustment_lines(value: &Value) -> Result<Vec<JournalLine>, ToolError> {
    let array = value
        .as_array()
        .ok_or_else(|| ToolError::new("\"lines\" must be an array"))?;
    array
        .iter()
        .enumerate()
        .map(|(index, raw)| parse_adjustment_line(raw, (index + 1) as i64))
        .collect()
}

fn parse_adjustment_line(raw: &Value, line_no: i64) -> Result<JournalLine, ToolError> {
    let account = AccountId(required_str(raw, "account")?.to_string());
    let debit = raw.get("debit").filter(|v| !v.is_null());
    let credit = raw.get("credit").filter(|v| !v.is_null());
    let mut line = match (debit, credit) {
        (Some(amount), None) => JournalLine::debit(line_no, account, value_to_money(amount)?),
        (None, Some(amount)) => JournalLine::credit(line_no, account, value_to_money(amount)?),
        _ => {
            return Err(ToolError::new(format!(
                "line {line_no}: exactly one of \"debit\" or \"credit\" is required"
            )))
        }
    };
    if let Some(class) = raw.get("class").and_then(Value::as_str) {
        line = line.with_class(Some(ClassId(class.to_string())));
    }
    if let Some(memo) = raw.get("memo").and_then(Value::as_str) {
        line.memo = Some(memo.to_string());
    }
    if let Some(entity) = raw.get("entity").filter(|v| !v.is_null()) {
        line = line.with_entity(Some(parse_contact_ref(entity)?));
    }
    Ok(line)
}

// ---------------------------------------------------------------------------
// Error mapping — a domain failure always becomes an `isError` tool result,
// never a JSON-RPC error (`qbo_local::mcp`'s rule, restated in this module's
// docs).
// ---------------------------------------------------------------------------

/// [`LedgerError::PeriodClosed`]'s own message ("period locked through
/// ...") does not literally contain the word "closed"; every write tool's
/// documented refusal on a closed period does, so this rewords just that one
/// variant rather than leaving a caller to grep for "locked" instead.
fn books_err(err: BooksError) -> ToolError {
    match err {
        BooksError::Ledger(inner) => ledger_err(inner),
        other => ToolError::new(other.to_string()),
    }
}

fn parse_book(args: &Value) -> Result<Book, ToolError> {
    match args.get("book").and_then(Value::as_str) {
        Some("receivable") => Ok(Book::Receivable),
        Some("payable") => Ok(Book::Payable),
        other => Err(ToolError::new(format!(
            "\"book\" must be \"receivable\" or \"payable\", got {other:?}"
        ))),
    }
}

const fn book_str(book: Book) -> &'static str {
    match book {
        Book::Receivable => "receivable",
        Book::Payable => "payable",
    }
}

/// `create_document` takes only what a person would type: every
/// LedgerDocument field it leaves out gets its empty value here, and a
/// missing `document_id` gets a fresh one. `save_and_post_document` stays
/// strict.
fn fill_document_defaults(value: &mut Value) -> Result<(), ToolError> {
    let map = value
        .as_object_mut()
        .ok_or_else(|| ToolError::new("\"document\" must be an object"))?;
    map.entry("document_id")
        .or_insert_with(|| json!(uuid::Uuid::now_v7().to_string()));
    for key in [
        "number",
        "due_date",
        "contact",
        "header_class",
        "tax",
        "deposit_to",
        "pay_from",
        "source_ref",
        "memo",
    ] {
        map.entry(key).or_insert(Value::Null);
    }
    map.entry("lines").or_insert_with(|| json!([]));
    map.entry("applications").or_insert_with(|| json!([]));
    map.entry("unapplied").or_insert(json!(0));
    map.entry("is_voided").or_insert(json!(false));
    if let Some(Value::Array(lines)) = map.get_mut("lines") {
        for (index, line) in lines.iter_mut().enumerate() {
            let Some(line) = line.as_object_mut() else {
                continue;
            };
            line.entry("line_no").or_insert(json!(index as i64 + 1));
            line.entry("is_taxable").or_insert(json!(false));
            for key in [
                "class",
                "item_id",
                "account",
                "qty",
                "unit_cost",
                "description",
                "posting",
                "entity",
            ] {
                line.entry(key).or_insert(Value::Null);
            }
        }
    }
    Ok(())
}

fn contact_json(contact: &Contact) -> Value {
    json!({
        "contact_id": contact.contact_id,
        "kind": match contact.kind {
            ContactKind::Customer => "customer",
            ContactKind::Vendor => "vendor",
        },
        "display_name": contact.display_name,
        "company_name": contact.company_name,
        "email": contact.email,
        "phone": contact.phone,
        "bill_address": contact.bill_address,
        "ship_address": contact.ship_address,
        "terms_days": contact.terms_days,
        "is_tax_exempt": contact.is_tax_exempt,
        "default_class": class_id_json(&contact.default_class),
        "is_active": contact.is_active,
        "source_ref": contact.source_ref,
    })
}

fn item_json(item: &Item) -> Value {
    json!({
        "item_id": item.item_id,
        "sku": item.sku,
        "name": item.name,
        "description": item.description,
        "item_type": serde_json::to_value(item.item_type).unwrap_or(Value::Null),
        "income_account": item.income_account.0,
        "expense_account": item.expense_account.0,
        "asset_account": item.asset_account.as_ref().map(|a| a.0.clone()),
        "default_class": class_id_json(&item.default_class),
        "sales_price": item.sales_price.map(money_str),
        "purchase_cost": item.purchase_cost.map(money_str),
        "unit_cost": item.unit_cost.map(money_str),
        "is_taxable": item.is_taxable,
        "is_active": item.is_active,
        "source_ref": item.source_ref,
    })
}

fn command(kind: &str, actor: String, now: chrono::DateTime<Utc>) -> CommandMeta {
    CommandMeta {
        actor_id: actor,
        kind: kind.to_string(),
        hlc: now.to_rfc3339(),
    }
}

/// A saved document as an editor needs it back: every field a person typed
/// or the book filled, money as two-place strings, plus its `version` and
/// `total`.
fn document_json(doc: &LedgerDocument, version: i64) -> Result<Value, ToolError> {
    let total = books::document_total(doc).map_err(|e| ToolError::new(e.to_string()))?;
    Ok(json!({
        "document_id": doc.document_id,
        "version": version,
        "kind": doc.kind.as_str(),
        "number": doc.number,
        "txn_date": doc.txn_date.to_string(),
        "due_date": doc.due_date.map(|d| d.to_string()),
        "contact": contact_ref_json(&doc.contact),
        "header_class": class_id_json(&doc.header_class),
        "lines": doc.lines.iter().map(|line| json!({
            "line_no": line.line_no,
            "kind": line.kind,
            "amount": money_str(line.amount),
            "class": class_id_json(&line.class),
            "item_id": line.item_id,
            "account": line.account.as_ref().map(|a| a.0.clone()),
            "is_taxable": line.is_taxable,
            "qty": line.qty.map(|q| q.to_string()),
            "description": line.description,
        })).collect::<Vec<_>>(),
        "tax": doc.tax.as_ref().map(|t| json!({
            "total_tax": money_str(t.total_tax),
            "taxable_base": money_str(t.taxable_base),
            "rate": t.rate.to_string(),
        })),
        "deposit_to": doc.deposit_to.as_ref().map(|a| a.0.clone()),
        "pay_from": doc.pay_from.as_ref().map(|a| a.0.clone()),
        "applications": doc.applications.iter().map(|a| json!({
            "target_document_id": a.target_document_id,
            "target_kind": a.target_kind.as_str(),
            "amount": money_str(a.amount),
        })).collect::<Vec<_>>(),
        "unapplied": money_str(doc.unapplied),
        "is_voided": doc.is_voided,
        "memo": doc.memo,
        "source_ref": doc.source_ref,
        "total": money_str(total),
    }))
}

/// What `update_document`, `void_document` and `apply_credit` return.
fn revision_json(revision: &books::Revision) -> Result<Value, ToolError> {
    let doc = &revision.document;
    let total = books::document_total(doc).map_err(|e| ToolError::new(e.to_string()))?;
    let (entry_id, entry) = match &revision.entry {
        Some((id, entry)) => (json!(id), journal_entry_json(entry)),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "document_id": doc.document_id,
        "version": revision.version,
        "number": doc.number,
        "due_date": doc.due_date.map(|d| d.to_string()),
        "total": money_str(total),
        "tax": doc.tax.as_ref().map(|t| money_str(t.total_tax)),
        "is_voided": doc.is_voided,
        "reversal_entry_ids": revision.reversals,
        "entry_id": entry_id,
        "entry": entry,
    }))
}

fn sales_order_json(so: &sales_orders::OpenSalesOrder) -> Value {
    json!({
        "document_id": so.document_id,
        "number": so.number,
        "contact_id": so.contact_id,
        "contact_name": so.contact_name,
        "txn_date": so.txn_date.to_string(),
        "subtotal": money_str(so.subtotal),
        "invoiced": money_str(so.invoiced),
        "open": money_str(so.open),
        "invoice_ids": so.invoice_ids,
        "memo": so.memo,
        "source_ref": so.source_ref,
    })
}

fn sales_order_err(err: SalesOrderError) -> ToolError {
    match err {
        SalesOrderError::Books(inner) => books_err(inner),
        other => ToolError::new(other.to_string()),
    }
}

fn open_document_json(doc: &books::OpenDocument) -> Value {
    json!({
        "document_id": doc.document_id,
        "kind": doc.kind.as_str(),
        "number": doc.number,
        "contact_id": doc.contact_id,
        "contact_name": doc.contact_name,
        "txn_date": doc.txn_date.to_string(),
        "due_date": doc.due_date.to_string(),
        "total": money_str(doc.total),
        "applied": money_str(doc.applied),
        "open": money_str(doc.open),
    })
}

fn aging_row_json(row: &books::AgingRow) -> Value {
    json!({
        "contact_id": row.contact_id,
        "contact_name": row.contact_name,
        "current": money_str(row.current),
        "days_1_30": money_str(row.days_1_30),
        "days_31_60": money_str(row.days_31_60),
        "days_61_90": money_str(row.days_61_90),
        "over_90": money_str(row.over_90),
        "total": money_str(row.total),
    })
}

fn ledger_err(err: LedgerError) -> ToolError {
    if let LedgerError::PeriodClosed { period_end } = &err {
        return ToolError::new(format!(
            "period closed through {period_end}: cannot post on or before this date ({err})"
        ));
    }
    ToolError::new(err.to_string())
}

fn accountant_err(err: AccountantError) -> ToolError {
    if let AccountantError::Ledger(LedgerError::PeriodClosed { period_end }) = &err {
        return ToolError::new(format!(
            "period closed through {period_end}: cannot post on or before this date ({err})"
        ));
    }
    ToolError::new(err.to_string())
}

fn link_err(err: LinkError) -> ToolError {
    ToolError::new(err.to_string())
}

fn payroll_err(err: PayrollError) -> ToolError {
    ToolError::new(err.to_string())
}

fn payroll_totals_json(totals: &payroll::RunTotals) -> Value {
    json!({
        "gross": money_str(totals.gross),
        "employer_costs": money_str(totals.employer_costs),
        "liabilities": money_str(totals.liabilities),
        "net_pay": money_str(totals.net_pay),
        "noncash_not_posted": money_str(totals.noncash),
    })
}

fn post_err(err: PostError) -> ToolError {
    ToolError::new(err.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use serde_json::json;

    use super::*;
    use crate::chart;
    use crate::store::Ledger;

    const COMPANY: &str = "aquamentor";

    /// `new_server` borrows (`LedgerToolSet<'a>`), so the `Ledger` a test
    /// wants to seed has to outlive the `Server` built over it — this
    /// returns the `Ledger` too, rather than a `seeded_server()` that
    /// returns just the `Server` the way it did before that borrow, since a
    /// function cannot return a struct borrowing a sibling it also moves
    /// out.
    fn seed_ledger() -> Ledger {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .create_company(COMPANY, "Test Co", None, Utc::now())
            .unwrap();
        ledger
    }

    fn call(server: &mut Server, id: i64, method: &str, params: Value) -> Value {
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let response = server
            .handle_line(&line)
            .expect("a request always gets a response");
        serde_json::from_str(&response).unwrap()
    }

    fn call_tool(server: &mut Server, id: i64, name: &str, arguments: Value) -> Value {
        call(
            server,
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn invoice_document(document_id: &str, class: &str) -> Value {
        json!({
            "document_id": document_id,
            "kind": "Invoice",
            "number": null,
            "txn_date": "2026-09-01",
            "due_date": null,
            "contact": { "kind": "Customer", "id": "cust-1" },
            "header_class": class,
            "lines": [
                {
                    "line_no": 1,
                    "kind": "Item",
                    "amount": "100.00",
                    "class": null,
                    "item_id": null,
                    "account": null,
                    "is_taxable": true,
                    "qty": null,
                    "unit_cost": null,
                    "description": "widgets",
                    "posting": null,
                    "entity": null
                },
                {
                    "line_no": 2,
                    "kind": "Item",
                    "amount": "50.00",
                    "class": null,
                    "item_id": null,
                    "account": null,
                    "is_taxable": true,
                    "qty": null,
                    "unit_cost": null,
                    "description": "more widgets",
                    "posting": null,
                    "entity": null
                }
            ],
            "tax": { "total_tax": "9.94", "taxable_base": "150.00", "rate": "0.06625" },
            "deposit_to": null,
            "pay_from": null,
            "applications": [],
            "unapplied": 0,
            "is_voided": false,
            "source_ref": null,
            "memo": "test invoice"
        })
    }

    #[test]
    fn initialize_names_the_ledger_server() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = call(
            &mut server,
            1,
            "initialize",
            json!({ "protocolVersion": mcp_stdio::PROTOCOL_VERSION }),
        );
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(mcp_stdio::PROTOCOL_VERSION)
        );
        assert_eq!(response["result"]["serverInfo"]["name"], json!("ledger"));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = server.handle_line("{ not json").unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], json!(-32700));
    }

    #[test]
    fn tools_list_carries_every_tool() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = call(&mut server, 1, "tools/list", json!({}));
        let listed = response["result"]["tools"].as_array().unwrap();
        let expected = [
            "trial_balance",
            "profit_and_loss",
            "balance_sheet",
            "sales_tax_lines",
            "general_ledger",
            "audit_trail",
            "entries_for_document",
            "list_adjustments",
            "bank_status",
            "locked_through",
            "list_contacts",
            "list_items",
            "open_documents",
            "open_sales_orders",
            "aging",
            "get_document",
            "upsert_contact",
            "upsert_item",
            "create_document",
            "convert_to_invoice",
            "update_document",
            "void_document",
            "apply_credit",
            "save_and_post_document",
            "reverse_entry",
            "propose_adjustment",
            "decide_adjustment",
            "bank_confirm_proposal",
            "close_period",
            "reopen_period",
            "payment_link",
            "import_payroll",
        ];
        assert_eq!(listed.len(), expected.len());
        for name in expected {
            let tool = listed
                .iter()
                .find(|tool| tool["name"] == json!(name))
                .unwrap_or_else(|| panic!("tools/list is missing {name:?}"));
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
        }
    }

    /// Invented run, invented ids: one mapped employee, one not.
    #[test]
    fn import_payroll_dry_runs_posts_once_and_refuses_the_unmapped() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[payroll.employees]\n\"E-1\" = { class = \"cnc\" }\n",
        )
        .unwrap();
        let config = config.to_str().unwrap();
        let line = |employee: &str, section: &str, item: &str, amount: &str| json!({ "employee": employee, "section": section, "item": item, "amount": amount });
        let runs = json!([{
            "run_id": "MCP-1",
            "pay_date": "2027-05-07",
            "lines": [
                line("E-1", "earning", "REGULAR", "600.00"),
                line("E-1", "employee_tax", "FED WTH", "50.00"),
                line("E-1", "employer_tax", "CO FICA", "37.20"),
                line("E-1", "net_pay", "NET PAY", "550.00"),
            ],
        }]);

        let dry = call_tool(
            &mut server,
            1,
            "import_payroll",
            json!({ "actor": "test", "runs": runs, "dry_run": true, "config_path": config }),
        );
        assert_ne!(dry["result"]["isError"], json!(true), "{dry}");
        let preview = &dry["result"]["structuredContent"]["runs"][0];
        assert_eq!(preview["status"], json!("dry_run"));
        assert_eq!(preview["entry"]["is_flagged"], json!(true));
        assert_eq!(ledger.oplog_len(COMPANY).unwrap(), 0);

        let posted = call_tool(
            &mut server,
            2,
            "import_payroll",
            json!({ "actor": "test", "runs": runs, "config_path": config }),
        );
        assert_ne!(posted["result"]["isError"], json!(true), "{posted}");
        let run = &posted["result"]["structuredContent"]["runs"][0];
        assert_eq!(run["status"], json!("posted"));
        assert_eq!(run["document_id"], json!("payroll:2027-05-07:MCP-1"));
        assert_eq!(run["totals"]["net_pay"], json!("550.00"));
        assert_eq!(run["totals"]["liabilities"], json!("87.20"));
        assert!(posted["result"]["structuredContent"]
            .as_object()
            .unwrap()
            .contains_key("locked_through"));

        let again = call_tool(
            &mut server,
            3,
            "import_payroll",
            json!({ "actor": "test", "runs": runs, "config_path": config }),
        );
        assert_eq!(
            again["result"]["structuredContent"]["runs"][0]["status"],
            json!("already_imported")
        );

        let csv = "run_id,pay_date,employee,section,item,amount\nMCP-2,2027-05-21,E-2,earning,REGULAR,10.00\n";
        let refused = call_tool(
            &mut server,
            4,
            "import_payroll",
            json!({ "actor": "test", "csv": csv, "config_path": config }),
        );
        assert_eq!(refused["result"]["isError"], json!(true));
        let text = refused["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("E-2"), "{text}");

        let both = call_tool(
            &mut server,
            5,
            "import_payroll",
            json!({ "actor": "test", "csv": csv, "runs": [], "config_path": config }),
        );
        assert_eq!(both["result"]["isError"], json!(true));
    }

    #[test]
    fn save_and_post_a_two_line_taxable_invoice_balances_and_moves_the_trial_balance() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = call_tool(
            &mut server,
            1,
            "save_and_post_document",
            json!({
                "document": invoice_document("inv-1", "foam"),
                "actor": "dan",
            }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["document_id"], json!("inv-1"));
        assert_eq!(result["version"], json!(1));
        assert!(!result["entry_id"].is_null());
        assert_eq!(result["locked_through"], Value::Null);
        let lines = result["entry"]["lines"].as_array().unwrap();
        let ar_line = lines
            .iter()
            .find(|line| line["account"] == json!(chart::ACCOUNTS_RECEIVABLE))
            .expect("AR line present");
        assert_eq!(ar_line["debit"], json!("159.94"));

        let response = call_tool(
            &mut server,
            2,
            "trial_balance",
            json!({ "as_of": "2026-09-01" }),
        );
        let rows = response["result"]["structuredContent"]["rows"]
            .as_array()
            .unwrap();
        let ar_row = rows
            .iter()
            .find(|row| row["account_id"] == json!(chart::ACCOUNTS_RECEIVABLE))
            .expect("AR row present");
        assert_eq!(ar_row["balance"], json!("159.94"));
    }

    #[test]
    fn save_and_post_into_a_closed_period_is_an_is_error_containing_closed() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        {
            let response = call_tool(
                &mut server,
                1,
                "close_period",
                json!({
                    "period_end": "2026-12-31",
                    "actor": "dan",
                    "note": "year end",
                }),
            );
            assert_ne!(response["result"]["isError"], json!(true), "{response}");
        }

        let response = call_tool(
            &mut server,
            2,
            "save_and_post_document",
            json!({
                "document": invoice_document("inv-2", "foam"),
                "actor": "dan",
            }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("closed"), "{text:?}");
        assert_eq!(
            response["result"]["structuredContent"],
            Value::Null,
            "an isError result carries no structuredContent"
        );
    }

    #[test]
    fn refuses_to_write_while_replaying() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        // Flip the same flag a real import replay would, then confirm every
        // write refuses. `set_replaying` takes `&self` (an `AtomicBool`
        // underneath), so the shared reference `tool_set()` hands back is
        // enough — no need to reach in mutably.
        server.tool_set().ledger.set_replaying(true);

        let response = call_tool(
            &mut server,
            1,
            "close_period",
            json!({ "period_end": "2026-12-31", "actor": "dan" }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("replaying"), "{text:?}");
    }

    #[test]
    fn propose_then_approve_an_adjustment_moves_the_trial_balance() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = call_tool(
            &mut server,
            1,
            "propose_adjustment",
            json!({
                "requested_by": "joel",
                "description": "correct a misclassified expense",
                "lines": [
                    { "account": chart::SHOP_SUPPLIES, "class": "foam", "debit": "25.00" },
                    { "account": chart::CHECKING, "credit": "25.00" },
                ],
            }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let request_id = response["result"]["structuredContent"]["request_id"]
            .as_str()
            .unwrap()
            .to_string();

        let response = call_tool(
            &mut server,
            2,
            "decide_adjustment",
            json!({
                "request_id": request_id,
                "decided_by": "dan",
                "approve": true,
                "note": "looks right",
            }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["state"],
            json!("posted")
        );

        let response = call_tool(
            &mut server,
            3,
            "trial_balance",
            json!({ "as_of": Utc::now().format("%Y-%m-%d").to_string() }),
        );
        let rows = response["result"]["structuredContent"]["rows"]
            .as_array()
            .unwrap();
        let supplies = rows
            .iter()
            .find(|row| row["account_id"] == json!(chart::SHOP_SUPPLIES))
            .expect("shop supplies row present");
        assert_eq!(supplies["balance"], json!("25.00"));
    }

    #[test]
    fn reverse_entry_mirrors_the_original() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let posted = call_tool(
            &mut server,
            1,
            "save_and_post_document",
            json!({
                "document": invoice_document("inv-3", "foam"),
                "actor": "dan",
            }),
        );
        let entry_id = posted["result"]["structuredContent"]["entry_id"]
            .as_str()
            .unwrap()
            .to_string();

        let response = call_tool(
            &mut server,
            2,
            "reverse_entry",
            json!({ "entry_id": entry_id, "on": "2026-09-02", "actor": "dan" }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let lines = response["result"]["structuredContent"]["entry"]["lines"]
            .as_array()
            .unwrap();
        let ar_line = lines
            .iter()
            .find(|line| line["account"] == json!(chart::ACCOUNTS_RECEIVABLE))
            .expect("AR line present");
        assert_eq!(ar_line["credit"], json!("159.94"));
        assert_eq!(ar_line["debit"], json!("0.00"));
    }

    #[test]
    fn bad_json_is_minus_32700() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = server.handle_line("not json at all").unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], json!(-32700));
    }

    #[test]
    fn locked_through_reports_none_before_any_close() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let response = call_tool(&mut server, 1, "locked_through", json!({}));
        assert_eq!(
            response["result"]["structuredContent"]["locked_through"],
            Value::Null
        );
    }

    #[test]
    fn missing_class_on_an_income_line_is_an_is_error() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        let mut doc = invoice_document("inv-4", "foam");
        doc["header_class"] = Value::Null;
        doc["lines"][0]["class"] = Value::Null;
        doc["lines"][1]["class"] = Value::Null;
        let response = call_tool(
            &mut server,
            1,
            "save_and_post_document",
            json!({ "document": doc, "actor": "dan" }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("class"), "{text:?}");
    }

    /// A customer on net 30, a vendor, and one taxable item, entered through
    /// the tools themselves.
    fn seed_master_data(server: &mut Server) {
        for (id, contact) in [
            (
                1,
                json!({ "contact_id": "c-pool", "kind": "Customer", "display_name": "Pool Co",
                        "terms_days": 30, "default_class": "foam" }),
            ),
            (
                2,
                json!({ "contact_id": "v-foam", "kind": "Vendor", "display_name": "Foam Supplier" }),
            ),
        ] {
            let response = call_tool(server, id, "upsert_contact", json!({ "contact": contact }));
            assert_ne!(response["result"]["isError"], json!(true), "{response}");
        }
        let response = call_tool(
            server,
            3,
            "upsert_item",
            json!({ "item": {
                "item_id": "mat", "sku": "MAT-1", "name": "Swim mat",
                "item_type": "non_inventory", "income_account": "4100",
                "expense_account": "6100", "default_class": "foam", "sales_price": "25.00"
            }}),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
    }

    #[test]
    fn create_document_fills_the_blanks_and_aging_follows_a_payment() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        seed_master_data(&mut server);

        let items = call_tool(&mut server, 4, "list_items", json!({ "query": "mat-1" }));
        assert_eq!(
            items["result"]["structuredContent"][0]["sales_price"],
            json!("25.00")
        );

        let response = call_tool(
            &mut server,
            5,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Invoice", "txn_date": "2026-09-01",
                "contact": { "kind": "Customer", "id": "c-pool" },
                "lines": [{ "kind": "Item", "item_id": "mat", "amount": "100.00",
                            "is_taxable": true }]
            }}),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let created = &response["result"]["structuredContent"];
        assert_eq!(created["number"], json!("1001"));
        assert_eq!(created["due_date"], json!("2026-10-01"));
        assert_eq!(created["tax"], json!("6.62"));
        assert_eq!(created["total"], json!("106.62"));
        let invoice_id = created["document_id"].as_str().unwrap().to_string();

        let response = call_tool(
            &mut server,
            6,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Payment", "txn_date": "2026-10-05",
                "contact": { "kind": "Customer", "id": "c-pool" },
                "applications": [{ "target_document_id": invoice_id,
                                   "target_kind": "Invoice", "amount": "50.00" }]
            }}),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");

        let aging = call_tool(
            &mut server,
            7,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        );
        let totals = &aging["result"]["structuredContent"]["totals"];
        assert_eq!(totals["days_1_30"], json!("56.62"));
        assert_eq!(totals["total"], json!("56.62"));
        assert_eq!(
            aging["result"]["structuredContent"]["rows"][0]["contact_name"],
            json!("Pool Co")
        );

        let open = call_tool(
            &mut server,
            8,
            "open_documents",
            json!({ "book": "receivable", "as_of": "2026-10-15" }),
        );
        assert_eq!(
            open["result"]["structuredContent"][0]["number"],
            json!("1001")
        );
        assert_eq!(
            open["result"]["structuredContent"][0]["open"],
            json!("56.62")
        );
    }

    /// D42: a sales order is numbered and never posts; convert_to_invoice
    /// makes the invoice from it, part then the rest, and the order closes.
    #[test]
    fn a_sales_order_converts_to_invoices_and_closes() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        seed_master_data(&mut server);

        let response = call_tool(
            &mut server,
            4,
            "create_document",
            json!({ "actor": "pipeline", "document": {
                "kind": "SalesOrder", "txn_date": "2026-09-01",
                "contact": { "kind": "Customer", "id": "c-pool" },
                "memo": "PO 77",
                "lines": [
                    { "kind": "Item", "item_id": "mat", "qty": "4", "amount": "100.00",
                      "is_taxable": true, "description": "Swim mat" },
                    { "kind": "Description", "amount": "0.00", "description": "Ship blind" }
                ]
            }}),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let created = &response["result"]["structuredContent"];
        assert_eq!(created["number"], json!("1001"));
        assert_eq!(created["entry_id"], Value::Null);
        assert_eq!(created["tax"], json!("6.62"));
        let so_id = created["document_id"].as_str().unwrap().to_string();

        let open = call_tool(&mut server, 5, "open_sales_orders", json!({}));
        let rows = &open["result"]["structuredContent"];
        assert_eq!(rows.as_array().unwrap().len(), 1, "{open}");
        assert_eq!(rows[0]["open"], json!("100.00"));
        assert_eq!(rows[0]["contact_name"], json!("Pool Co"));

        // A part: one of the four mats.
        let response = call_tool(
            &mut server,
            6,
            "convert_to_invoice",
            json!({ "actor": "dan", "sales_order_id": so_id, "txn_date": "2026-09-10",
                    "lines": [{ "line_no": 1, "qty": "1" }] }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let first = &response["result"]["structuredContent"];
        assert_eq!(first["total"], json!("26.66"));
        assert_eq!(first["sales_order_open"], json!("75.00"));
        assert_eq!(first["sales_order_closed"], json!(false));
        assert!(first["entry_id"].is_string());
        let first_id = first["document_id"].as_str().unwrap().to_string();
        let invoice = call_tool(
            &mut server,
            7,
            "get_document",
            json!({ "document_id": first_id }),
        );
        let invoice = &invoice["result"]["structuredContent"];
        assert_eq!(invoice["memo"], json!("PO 77"));
        assert_eq!(
            invoice["applications"][0]["target_kind"],
            json!("sales_order")
        );
        assert_eq!(invoice["applications"][0]["amount"], json!("25.00"));

        // With part invoiced, "everything" is refused rather than doubled.
        let response = call_tool(
            &mut server,
            8,
            "convert_to_invoice",
            json!({ "actor": "dan", "sales_order_id": so_id, "txn_date": "2026-09-12" }),
        );
        assert_eq!(response["result"]["isError"], json!(true), "{response}");
        // More than is left is refused too.
        let response = call_tool(
            &mut server,
            9,
            "convert_to_invoice",
            json!({ "actor": "dan", "sales_order_id": so_id, "txn_date": "2026-09-12",
                    "lines": [{ "line_no": 1 }] }),
        );
        assert_eq!(response["result"]["isError"], json!(true), "{response}");

        let response = call_tool(
            &mut server,
            10,
            "convert_to_invoice",
            json!({ "actor": "dan", "sales_order_id": so_id, "txn_date": "2026-09-12",
                    "lines": [{ "line_no": 1, "qty": 3 }, { "line_no": 2 }] }),
        );
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        let second = &response["result"]["structuredContent"];
        assert_eq!(second["number"], json!("1002"));
        assert_eq!(second["sales_order_closed"], json!(true));

        let open = call_tool(&mut server, 11, "open_sales_orders", json!({}));
        assert_eq!(open["result"]["structuredContent"], json!([]));
        let all = call_tool(&mut server, 12, "open_sales_orders", json!({ "all": true }));
        let row = &all["result"]["structuredContent"][0];
        assert_eq!(row["invoiced"], json!("100.00"));
        assert_eq!(row["invoice_ids"].as_array().unwrap().len(), 2);

        // The link moves no money: AR is the two invoices, nothing more
        // (26.66 + 79.97; each invoice rounds its own tax).
        let aging = call_tool(
            &mut server,
            13,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-09-30" }),
        );
        assert_eq!(
            aging["result"]["structuredContent"]["totals"]["total"],
            json!("106.63")
        );
        let response = call_tool(
            &mut server,
            14,
            "convert_to_invoice",
            json!({ "actor": "dan", "sales_order_id": so_id, "txn_date": "2026-09-12",
                    "lines": [{ "line_no": 1, "qty": 1 }] }),
        );
        assert_eq!(response["result"]["isError"], json!(true), "{response}");
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("nothing left to invoice"), "{text}");

        // An invoice made from an order stays editable after the order is voided.
        let voided = call_tool(
            &mut server,
            15,
            "void_document",
            json!({ "actor": "dan", "document_id": so_id, "reason": "test" }),
        );
        assert_ne!(voided["result"]["isError"], json!(true), "{voided}");
        let edited = call_tool(
            &mut server,
            16,
            "update_document",
            json!({ "actor": "dan", "document": {
                "document_id": first_id, "kind": "Invoice", "txn_date": "2026-09-10",
                "contact": { "kind": "Customer", "id": "c-pool" }, "memo": "edited",
                "lines": [{ "kind": "Item", "item_id": "mat", "qty": "1", "amount": "25.00",
                            "is_taxable": true }]
            }}),
        );
        assert_ne!(edited["result"]["isError"], json!(true), "{edited}");
    }

    #[test]
    fn create_document_refuses_an_unknown_customer_and_saves_nothing() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        seed_master_data(&mut server);
        let before = ledger.oplog_len(COMPANY).unwrap();
        let response = call_tool(
            &mut server,
            4,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Invoice", "txn_date": "2026-09-01",
                "contact": { "kind": "Customer", "id": "nobody" },
                "lines": [{ "kind": "Item", "item_id": "mat", "amount": "100.00" }]
            }}),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("unknown customer nobody"), "{text:?}");
        assert_eq!(ledger.oplog_len(COMPANY).unwrap(), before);
    }

    #[test]
    fn create_document_that_cannot_post_saves_nothing() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        seed_master_data(&mut server);
        // A vendor with no default class and an account line with no class:
        // the posting rules refuse it, and nothing may be left behind.
        let response = call_tool(
            &mut server,
            4,
            "create_document",
            json!({ "actor": "dan", "document": {
                "kind": "Bill", "txn_date": "2026-09-01", "number": "FS-1",
                "contact": { "kind": "Vendor", "id": "v-foam" },
                "lines": [{ "kind": "Account", "account": "6100", "amount": "40.00" }]
            }}),
        );
        assert_eq!(response["result"]["isError"], json!(true), "{response}");
        assert_eq!(ledger.oplog_len(COMPANY).unwrap(), 0);
    }

    #[test]
    fn decimal_str_to_minor_matches_the_documented_examples() {
        assert_eq!(decimal_str_to_minor("1234.56"), Ok(123_456));
        assert_eq!(decimal_str_to_minor("-12"), Ok(-1200));
        assert_eq!(
            decimal_str_to_minor(dec!(1.005).to_string().as_str()),
            Err(())
        );
    }

    #[test]
    fn edit_void_and_apply_credit_through_the_tools() {
        let ledger = seed_ledger();
        let mut server = new_server(&ledger, COMPANY.to_string());
        seed_master_data(&mut server);
        let create = |server: &mut Server, id, document: Value| {
            let response = call_tool(
                server,
                id,
                "create_document",
                json!({ "actor": "dan", "document": document }),
            );
            assert_ne!(response["result"]["isError"], json!(true), "{response}");
            response["result"]["structuredContent"]["document_id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let invoice_id = create(
            &mut server,
            4,
            json!({ "kind": "Invoice", "txn_date": "2026-09-01",
                    "contact": { "kind": "Customer", "id": "c-pool" },
                    "lines": [{ "kind": "Item", "item_id": "mat", "amount": "100.00" }] }),
        );

        // Read it back as an editor would, change the amount, send it back.
        let got = call_tool(
            &mut server,
            5,
            "get_document",
            json!({ "document_id": invoice_id }),
        );
        let mut document = got["result"]["structuredContent"].clone();
        assert_eq!(document["number"], json!("1001"));
        assert_eq!(document["version"], json!(1));
        assert_eq!(document["lines"][0]["amount"], json!("100.00"));
        assert_eq!(document["contact"]["kind"], json!("customer"));
        // The editor sends what a person typed, as create_document takes it.
        let edited = json!({
            "document_id": invoice_id, "kind": "Invoice", "txn_date": "2026-09-01",
            "number": document["number"].take(),
            "contact": { "kind": "Customer", "id": "c-pool" },
            "lines": [{ "kind": "Item", "item_id": "mat", "amount": "150.00" }],
        });
        let updated = call_tool(
            &mut server,
            6,
            "update_document",
            json!({ "actor": "dan", "document": edited }),
        );
        assert_ne!(updated["result"]["isError"], json!(true), "{updated}");
        let result = &updated["result"]["structuredContent"];
        assert_eq!(result["version"], json!(2));
        assert_eq!(result["number"], json!("1001"));
        assert_eq!(result["total"], json!("150.00"));
        assert_eq!(result["reversal_entry_ids"].as_array().unwrap().len(), 1);
        assert!(result["entry_id"].is_string());
        assert!(result.get("locked_through").is_some());

        // A kind change is refused, as a tool error.
        let refused = call_tool(
            &mut server,
            7,
            "update_document",
            json!({ "actor": "dan", "document": {
                "document_id": invoice_id, "kind": "SalesReceipt", "txn_date": "2026-09-01",
                "contact": { "kind": "Customer", "id": "c-pool" },
                "lines": [{ "kind": "Item", "item_id": "mat", "amount": "150.00" }] }}),
        );
        assert_eq!(refused["result"]["isError"], json!(true));

        let credit_id = create(
            &mut server,
            8,
            json!({ "kind": "CreditMemo", "txn_date": "2026-09-05",
                    "contact": { "kind": "Customer", "id": "c-pool" },
                    "lines": [{ "kind": "Item", "item_id": "mat", "amount": "40.00" }] }),
        );
        let applied = call_tool(
            &mut server,
            9,
            "apply_credit",
            json!({ "actor": "dan", "credit_id": credit_id,
                    "target_document_id": invoice_id, "amount": "25.00" }),
        );
        assert_ne!(applied["result"]["isError"], json!(true), "{applied}");
        assert_eq!(
            applied["result"]["structuredContent"]["applied"],
            json!("25.00")
        );
        assert_eq!(
            applied["result"]["structuredContent"]["entry_id"],
            Value::Null
        );
        let open = call_tool(
            &mut server,
            10,
            "open_documents",
            json!({ "book": "receivable", "as_of": "2026-12-31" }),
        );
        let open = open["result"]["structuredContent"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(open[0]["open"], json!("125.00"));
        assert_eq!(open[1]["open"], json!("-15.00"));

        // Editing the credit's lines without sending `applications` keeps
        // what it already applies.
        let edited = call_tool(
            &mut server,
            90,
            "update_document",
            json!({ "actor": "dan", "document": {
                "document_id": credit_id, "kind": "CreditMemo", "txn_date": "2026-09-05",
                "contact": { "kind": "Customer", "id": "c-pool" },
                "lines": [{ "kind": "Item", "item_id": "mat", "amount": "45.00" }] }}),
        );
        assert_ne!(edited["result"]["isError"], json!(true), "{edited}");
        let open = call_tool(
            &mut server,
            91,
            "open_documents",
            json!({ "book": "receivable", "as_of": "2026-12-31" }),
        );
        let open = open["result"]["structuredContent"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(open[0]["open"], json!("125.00"));
        assert_eq!(open[1]["open"], json!("-20.00"));

        // The invoice has a credit applied: void the credit first.
        let refused = call_tool(
            &mut server,
            11,
            "void_document",
            json!({ "actor": "dan", "document_id": invoice_id, "reason": "duplicate" }),
        );
        assert_eq!(refused["result"]["isError"], json!(true));
        let missing_reason = call_tool(
            &mut server,
            12,
            "void_document",
            json!({ "actor": "dan", "document_id": credit_id, "reason": "  " }),
        );
        assert_eq!(missing_reason["result"]["isError"], json!(true));
        for (id, document_id) in [(13, &credit_id), (14, &invoice_id)] {
            let voided = call_tool(
                &mut server,
                id,
                "void_document",
                json!({ "actor": "dan", "document_id": document_id, "reason": "duplicate" }),
            );
            assert_ne!(voided["result"]["isError"], json!(true), "{voided}");
            assert_eq!(
                voided["result"]["structuredContent"]["is_voided"],
                json!(true)
            );
        }
        let aging = call_tool(
            &mut server,
            15,
            "aging",
            json!({ "book": "receivable", "as_of": "2026-12-31" }),
        );
        assert_eq!(
            aging["result"]["structuredContent"]["totals"]["total"],
            json!("0.00")
        );
        let got = call_tool(
            &mut server,
            16,
            "get_document",
            json!({ "document_id": invoice_id }),
        );
        assert_eq!(got["result"]["structuredContent"]["is_voided"], json!(true));
        assert_eq!(
            got["result"]["structuredContent"]["memo"],
            json!("Voided: duplicate")
        );
    }
}
