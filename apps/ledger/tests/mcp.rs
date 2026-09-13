//! The ledger MCP server, spawned as the real binary. `ROADMAP.md` §G, §D.
//!
//! `src/mcp.rs`'s own tests drive `handle_line` directly and cover the
//! protocol and tool logic; this test exists for the one thing that cannot —
//! that `bin/ledger-mcp.rs` actually reads `LEDGER_DB`/`LEDGER_COMPANY`,
//! opens the ledger read-write, and speaks newline-delimited JSON-RPC over
//! real stdio with nothing else landing on stdout (`apps/qbo-local/tests/mcp.rs`
//! is the sibling test this one is built to match).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use chrono::Utc;
use ledger::store::Ledger;
use serde_json::{json, Value};

const COMPANY: &str = "aquamentor";

#[test]
fn the_real_binary_speaks_jsonrpc_over_stdio_and_writes_nothing_else_to_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("ledger.db");

    {
        // Seed the company, then drop the writer before the child opens the
        // same file — WAL allows concurrent access regardless, but there is
        // no reason to hold this connection open past seeding.
        let ledger = Ledger::open(&db_path).unwrap();
        ledger
            .create_company(COMPANY, "Test Co", None, Utc::now())
            .unwrap();
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_ledger-mcp"))
        .env("LEDGER_DB", &db_path)
        .env("LEDGER_COMPANY", COMPANY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ledger-mcp should spawn");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let requests = [
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }),
        // A notification: no id, no response expected for this one.
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "locked_through", "arguments": {} }
        }),
    ];

    for request in &requests {
        writeln!(stdin, "{request}").unwrap();
    }
    stdin.flush().unwrap();
    drop(stdin); // EOF — the loop in main() exits once every line is read.

    let mut responses = Vec::new();
    for _ in 0..3 {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).unwrap();
        assert!(read > 0, "expected a response line, got EOF early");
        responses.push(serde_json::from_str::<Value>(line.trim_end()).unwrap());
    }

    assert_eq!(responses[0]["id"], json!(1));
    assert_eq!(
        responses[0]["result"]["protocolVersion"],
        json!("2025-06-18")
    );
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], json!("ledger"));

    assert_eq!(responses[1]["id"], json!(2));
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 17);
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == json!("save_and_post_document")));
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == json!("trial_balance")));

    assert_eq!(responses[2]["id"], json!(3));
    assert_ne!(responses[2]["result"]["isError"], json!(true));
    assert_eq!(
        responses[2]["result"]["structuredContent"]["locked_through"],
        Value::Null
    );

    // Nothing but the three JSON-RPC lines above ever reached stdout: reading
    // to EOF after those (the process exits once stdin closes) must be empty.
    let status = child.wait().expect("ledger-mcp should exit cleanly");
    assert!(status.success(), "process exited with {status:?}");

    let mut remainder = String::new();
    stdout.read_to_string(&mut remainder).unwrap();
    assert!(
        remainder.is_empty(),
        "unexpected trailing stdout: {remainder:?}"
    );
}

#[test]
fn missing_env_vars_fail_fast_rather_than_hanging_on_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ledger-mcp"))
        .env_remove("LEDGER_DB")
        .env_remove("LEDGER_COMPANY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ledger-mcp should spawn");

    let status = child.wait().expect("process should exit without any stdin");
    assert!(
        !status.success(),
        "should refuse to run without LEDGER_DB/LEDGER_COMPANY"
    );

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(
        stdout.is_empty(),
        "an env var failure must not write to stdout: {stdout:?}"
    );
}
