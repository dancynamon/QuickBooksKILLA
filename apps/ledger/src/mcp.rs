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
use crate::post::{self, Posting, PostError};
use crate::report;
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
    mcp_stdio::Server::new(LedgerToolSet { ledger, company }, info)
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
                let tb = report::trial_balance(self.ledger, &self.company, as_of)
                    .map_err(ledger_err)?;
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
                let bs = report::balance_sheet(self.ledger, &self.company, as_of)
                    .map_err(ledger_err)?;
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
                let detail =
                    accountant::general_ledger(self.ledger, &self.company, from, to, account.as_ref())
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
                let locked = self.ledger.locked_through(&self.company).map_err(ledger_err)?;
                Ok(json!({ "locked_through": locked.map(|d| d.to_string()) }))
            }

            // --- writes ---------------------------------------------------
            "save_and_post_document" => self.save_and_post_document(&mut args),
            "reverse_entry" => self.reverse_entry(&args),
            "propose_adjustment" => self.propose_adjustment(&mut args),
            "decide_adjustment" => self.decide_adjustment(&args),
            "bank_confirm_proposal" => self.bank_confirm_proposal(&args),
            "close_period" => self.close_period(&args),
            "reopen_period" => self.reopen_period(&args),

            other => Err(ToolError::new(format!("unknown tool: {other}"))),
        }
    }

    fn sales_tax_lines_tool(&self, args: &Value) -> Result<Value, ToolError> {
        let year = args
            .get("year")
            .and_then(Value::as_i64)
            .ok_or_else(|| ToolError::new("missing or invalid \"year\""))? as i32;
        let quarter = args
            .get("quarter")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::new("missing or invalid \"quarter\""))? as u32;
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
        let unmatched = lines.iter().filter(|line| line.match_kind.is_none()).count();
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
        if self.ledger.is_replaying() {
            return Err(ToolError::new(
                "the ledger is replaying an import; writes are refused until it finishes",
            ));
        }
        Ok(())
    }

    /// Attaches the company's current `locked_through` to a write's result,
    /// so a caller always sees the gate it is or is not up against —
    /// required on every write tool by this module's own docs.
    fn with_locked_through(&self, mut value: Value) -> Result<Value, ToolError> {
        let locked = self.ledger.locked_through(&self.company).map_err(ledger_err)?;
        if let Value::Object(map) = &mut value {
            map.insert("locked_through".to_string(), json!(locked.map(|d| d.to_string())));
        }
        Ok(value)
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
            name: "save_and_post_document",
            description: "Saves a document (a new version if `document_id` already exists) and, \
                unless it is an Estimate or PurchaseOrder (contracts, not transactions), posts \
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
            "save_and_post_document",
            "reverse_entry",
            "propose_adjustment",
            "decide_adjustment",
            "bank_confirm_proposal",
            "close_period",
            "reopen_period",
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

    #[test]
    fn decimal_str_to_minor_matches_the_documented_examples() {
        assert_eq!(decimal_str_to_minor("1234.56"), Ok(123_456));
        assert_eq!(decimal_str_to_minor("-12"), Ok(-1200));
        assert_eq!(decimal_str_to_minor(dec!(1.005).to_string().as_str()), Err(()));
    }
}
