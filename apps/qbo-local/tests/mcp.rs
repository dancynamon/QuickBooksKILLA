//! The MCP server, spawned as the real binary. `ROADMAP.md` §G.
//!
//! `src/mcp.rs`'s own tests drive `Server::handle_line` directly and cover
//! the protocol and tool logic; this test exists for the one thing that
//! cannot — that `bin/qbo-local-mcp.rs` actually reads `QBO_LOCAL_DB`, opens
//! it read-only, and speaks newline-delimited JSON-RPC over real stdio with
//! nothing else landing on stdout. Every name and figure below is invented
//! (HANDOFF.md §2.6).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use chrono::{TimeZone, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap()
}

#[test]
fn the_real_binary_speaks_jsonrpc_over_stdio_and_writes_nothing_else_to_stdout() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("replica.db");

    {
        // Seed the replica, then drop the writer before the child opens it
        // read-only — WAL allows concurrent readers regardless, but there is
        // no reason to hold the write connection open past seeding.
        let store = Store::open(&db_path).unwrap();
        store.register_realm(&realm(), "Test Co", now()).unwrap();
        let customer = MirroredEntity {
            entity_type: EntityType::Customer,
            qbo_id: "31".to_string(),
            sync_token: "1".to_string(),
            last_updated_utc: now(),
            is_deleted: false,
            raw_json: json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true }),
        };
        store.upsert_entity(&realm(), &customer, now()).unwrap();
        store.project_entity(&realm(), &customer, now()).unwrap();
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_qbo-local-mcp"))
        .env("QBO_LOCAL_DB", &db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("qbo-local-mcp should spawn");

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
            "params": {
                "name": "contact_detail",
                "arguments": {
                    "realm_id": realm().as_str(), "contact_type": "customer", "qbo_id": "31"
                }
            }
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
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        json!("qbo-local")
    );

    assert_eq!(responses[1]["id"], json!(2));
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 11);
    assert!(tools.iter().any(|tool| tool["name"] == json!("search")));
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == json!("contact_detail")));

    assert_eq!(responses[2]["id"], json!(3));
    assert_ne!(responses[2]["result"]["isError"], json!(true));
    let text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("Blue Harbor Swim Club"));

    // Nothing but the three JSON-RPC lines above ever reached stdout: reading
    // to EOF after those (the process exits once stdin closes) must be empty.
    let status = child.wait().expect("qbo-local-mcp should exit cleanly");
    assert!(status.success(), "process exited with {status:?}");

    let mut remainder = String::new();
    stdout.read_to_string(&mut remainder).unwrap();
    assert!(
        remainder.is_empty(),
        "unexpected trailing stdout: {remainder:?}"
    );
}

#[test]
fn a_missing_qbo_local_db_env_var_fails_fast_rather_than_hanging_on_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_qbo-local-mcp"))
        .env_remove("QBO_LOCAL_DB")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("qbo-local-mcp should spawn");

    let status = child.wait().expect("process should exit without any stdin");
    assert!(
        !status.success(),
        "should refuse to run without QBO_LOCAL_DB"
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
