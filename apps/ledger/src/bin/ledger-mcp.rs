//! Thin entry point for `ledger::mcp` — `ROADMAP.md` §G, §D.
//!
//! Opens the ledger named by `LEDGER_DB`, scoped to the company named by
//! `LEDGER_COMPANY`, read-write — this server posts — unless the file is
//! write-protected (an accountant handoff snapshot, `ledger::handoff`), which
//! opens read-only so every write tool refuses — then loops
//! newline-delimited JSON-RPC over stdin/stdout via [`mcp_stdio::serve_stdio`].
//! All the protocol and tool logic lives in [`ledger::mcp`] and is tested
//! there directly; this is just the stdio plumbing around it, matching
//! `qbo-local-mcp`'s shape. `LEDGER_AUTHNET_CONFIG` (default
//! `.local/config.toml`) names the `[authnet]` config `payment_link` reads. Diagnostics go to stderr — stdout ever carries
//! only JSON-RPC response lines, since that is the wire a client reads.

use ledger::mcp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::var("LEDGER_DB")
        .map_err(|_| "LEDGER_DB must be set to the ledger's sqlite file path")?;
    let company = std::env::var("LEDGER_COMPANY").map_err(|_| {
        "LEDGER_COMPANY must be set to the company id (\"aquamentor\" or \"waterline\")"
    })?;

    // A write-protected file (an accountant handoff snapshot, W24) opens
    // read-only, and every write tool then refuses.
    let ledger = ledger::handoff::open_book(std::path::Path::new(&db_path))
        .map_err(|err| format!("failed to open {db_path:?}: {err}"))?;
    // Authorize.net for `payment_link`: sandbox unless the config enables
    // production; credentials are read per call, from the environment or the
    // keychain, never from a file.
    let payments =
        ledger::authnet::link::live_client(&ledger::authnet::link::default_config_path());
    let server = mcp::new_server_with_payments(&ledger, company, payments);

    mcp_stdio::serve_stdio(server);
}
