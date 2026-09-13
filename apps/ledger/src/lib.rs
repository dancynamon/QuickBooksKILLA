//! `ledger` — the double-entry book that replaces QuickBooks. `LEDGER-DESIGN.md`.
//!
//! Documents describe what happened; the ledger is what happened. One function
//! turns a document into a balanced entry ([`post`]), one store holds the
//! entries behind a period gate ([`store`]), and one importer replays the
//! `qbo-local` replica through that same function ([`import`]) so history in
//! this book is derived rather than copied.
//!
//! Module layout follows the design document's sections:
//!
//! - [`types`]  — the document and entry shapes every module shares
//! - [`chart`]  — §2: account numbers and the seed chart
//! - [`post`]   — §1: the posting-rules table as code
//! - [`store`]  — §4, §5: schema, invariants, period close, oplog
//! - [`report`] — §7, §9: trial balance, P&L, balance sheet, the tax lines
//! - [`import`]   — §6: replica to documents, chart mapping, opening balances
//! - [`pipeline`] — §6: the importer wired to [`post`] and [`store`] —
//!   replica in, posted entries out
//! - [`accountant`] — §8: the accountant's read-only role, GL detail and
//!   exports, and the adjusting-entry request queue
//! - [`bank`]     — §10: statement import, matching, proposals, the
//!   per-statement close
//! - [`mcp`]      — `ROADMAP.md` §G, §D: the MCP server over this ledger —
//!   every report above as a read tool, plus gated writes through
//!   [`post`] and [`store`]
//! - [`verify`]   — §4, §5: an invariants sweep recomputed independently of
//!   the schema's own triggers and foreign keys
//! - [`nightly`]  — ROADMAP.md §E: the parallel-run command — import, diff
//!   against a QBO trial balance CSV, write the §7 report, and the launchd
//!   plist that schedules it

pub mod accountant;
pub mod bank;
pub mod chart;
pub mod import;
pub mod mcp;
pub mod nightly;
pub mod pipeline;
pub mod post;
pub mod report;
pub mod store;
pub mod types;
pub mod verify;
