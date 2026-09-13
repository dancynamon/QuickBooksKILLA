//! Thin entry point for `ledger::mcp` — `ROADMAP.md` §G, §D.
//!
//! Opens the ledger named by `LEDGER_DB`, scoped to the company named by
//! `LEDGER_COMPANY`, read-write — this server posts — then loops
//! newline-delimited JSON-RPC over stdin/stdout via [`mcp_stdio::serve_stdio`].
//! All the protocol and tool logic lives in [`ledger::mcp`] and is tested
//! there directly; this is just the stdio plumbing around it, matching
//! `qbo-local-mcp`'s shape. Diagnostics go to stderr — stdout ever carries
//! only JSON-RPC response lines, since that is the wire a client reads.

use ledger::mcp;
use ledger::store::Ledger;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::var("LEDGER_DB")
        .map_err(|_| "LEDGER_DB must be set to the ledger's sqlite file path")?;
    let company = std::env::var("LEDGER_COMPANY")
        .map_err(|_| "LEDGER_COMPANY must be set to the company id (\"aquamentor\" or \"waterline\")")?;

    let ledger = Ledger::open(&db_path)
        .map_err(|err| format!("failed to open {db_path:?}: {err}"))?;
    let server = mcp::new_server(&ledger, company);

    mcp_stdio::serve_stdio(server);
}
