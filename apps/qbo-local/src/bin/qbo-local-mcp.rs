//! Thin entry point for `qbo_local::mcp` — `ROADMAP.md` §G.
//!
//! Opens the replica named by `QBO_LOCAL_DB` strictly read-only, then loops
//! newline-delimited JSON-RPC over stdin/stdout. All the protocol and tool
//! logic lives in [`qbo_local::mcp`] and is tested there directly; this is
//! just the stdio plumbing around it. Diagnostics go to stderr — stdout ever
//! carries only JSON-RPC response lines, since that is the wire a client
//! reads.

use std::io::{self, BufRead, Write};

use qbo_local::mcp::Server;
use qbo_local::store::Store;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::var("QBO_LOCAL_DB")
        .map_err(|_| "QBO_LOCAL_DB must be set to the replica's sqlite file path")?;

    let store = Store::open_read_only(&db_path)
        .map_err(|err| format!("failed to open {db_path:?} read-only: {err}"))?;
    let mut server = Server::new(&store);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if let Some(response) = server.handle_line(&line) {
            out.write_all(response.as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }

    Ok(())
}
