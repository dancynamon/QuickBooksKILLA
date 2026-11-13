//! `qbo-local daemon --live` through the real binary, against the fake Intuit
//! server on the loopback interface (`HANDOFF.md` §2.5, §3.7).
//!
//! `tests/cli.rs` covers `daemon --mock` and the usage errors; `tests/daemon.rs`
//! and `tests/http_client.rs` cover the loop and the HTTP client as library
//! code. Nothing else proves the path the launchd job actually runs: a config
//! file becomes credentials, a token store and an `HttpQboClient`, and the
//! daemon ticks with it. `BUNZBOOKS_QBO_BASE_URL` points that client at the
//! fake server; it accepts only a loopback address, so this stays offline.
//!
//! Names and figures are invented (`HANDOFF.md` §2.6).

mod support;

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::json;

use qbo_local::auth::{FileTokenStore, TokenGenerations, TokenSet, TokenStore};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::Store;
use support::{entity, FakeRequest, FakeResponse};

const REALM: &str = "1234567890123456";

fn qbo_local(args: &[&str], base_url: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qbo-local"));
    command
        .args(args)
        .env("BUNZBOOKS_QBO_CLIENT_ID", "client-test")
        .env("BUNZBOOKS_QBO_CLIENT_SECRET", "secret-test")
        .env_remove("BUNZBOOKS_QBO_BASE_URL");
    if let Some(url) = base_url {
        command.env("BUNZBOOKS_QBO_BASE_URL", url);
    }
    command
        .output()
        .expect("failed to run the qbo-local binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A config with a file token store, rotation log and call-budget ledger all
/// inside `directory`, plus a fresh token generation already saved, so no
/// refresh (and so no token endpoint) is needed.
fn write_config(directory: &Path) -> std::path::PathBuf {
    let tokens_dir = directory.join("tokens");
    let realm = RealmId::parse(REALM).unwrap();
    FileTokenStore::new(&tokens_dir)
        .save(&TokenGenerations::new(
            realm,
            TokenSet {
                access_token: "access-fresh".to_string(),
                refresh_token: "refresh-0".to_string(),
                obtained_at: Utc::now(),
                access_expires_in_secs: 3600,
                refresh_expires_in_secs: 100 * 24 * 3600,
            },
        ))
        .unwrap();

    let config = directory.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
token_store = "file"
tokens_dir = {tokens:?}
rotation_log = {log:?}

[intuit]
environment = "production"

[[realms]]
id = "{REALM}"
name = "Test Company"

[budget]
daily_calls = 500
ledger = {ledger:?}
"#,
            tokens = tokens_dir.to_str().unwrap(),
            log = directory.join("rotation-log.jsonl").to_str().unwrap(),
            ledger = directory.join("call-budget.json").to_str().unwrap(),
        ),
    )
    .unwrap();
    config
}

fn init(db: &Path) {
    let output = qbo_local(
        &[
            "init",
            "--db",
            db.to_str().unwrap(),
            "--realm",
            REALM,
            "--name",
            "Test Company",
        ],
        None,
    );
    assert!(output.status.success(), "init failed: {}", stderr(&output));
}

#[test]
fn daemon_live_once_ticks_through_the_binary_against_the_fake_server() {
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let log = Arc::clone(&seen);
    let server = support::start(move |request: &FakeRequest| {
        log.lock().unwrap().push((
            request.method.clone(),
            request.path.clone(),
            request
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default(),
        ));
        let query = request.query.get("query").cloned().unwrap_or_default();
        if query.contains("FROM Customer") {
            let items = vec![entity(
                "31",
                "0",
                json!({ "DisplayName": "Blue Harbor Swim Club" }),
                "2026-08-01T00:00:00Z",
            )];
            FakeResponse::json(200, json!({ "QueryResponse": { "Customer": items } }))
        } else {
            FakeResponse::json(200, json!({ "QueryResponse": {} }))
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db);
    let config = write_config(directory.path());

    let output = qbo_local(
        &[
            "daemon",
            "--db",
            db.to_str().unwrap(),
            "--realm",
            REALM,
            "--live",
            "--config",
            config.to_str().unwrap(),
            "--once",
        ],
        Some(&server.base_url),
    );

    assert!(
        output.status.success(),
        "daemon --live --once failed:\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("next_delay="),
        "expected a tick line:\n{text}"
    );
    assert!(
        text.contains("next delay:"),
        "expected the next-delay line:\n{text}"
    );
    assert!(
        stderr(&output).contains("Intuit app client-test"),
        "expected the app banner on stderr:\n{}",
        stderr(&output)
    );

    let requests = seen.lock().unwrap();
    assert!(!requests.is_empty(), "the daemon never reached the server");
    for (method, path, authorization) in requests.iter() {
        // HANDOFF.md §3.1: the daemon only reads. Every call is a GET under
        // this realm, carrying the stored access token.
        assert_eq!(method, "GET", "the daemon sent a {method} to {path}");
        assert!(
            path.starts_with(&format!("/v3/company/{REALM}/")),
            "unexpected path {path}"
        );
        assert_eq!(authorization, "Bearer access-fresh");
    }

    // The customer the fake server returned is now in the replica, and
    // writes are still off for the realm.
    let store = Store::open(&db).unwrap();
    let realm = RealmId::parse(REALM).unwrap();
    assert!(store
        .get_entity(&realm, EntityType::Customer, "31")
        .unwrap()
        .is_some());
    assert!(!store.is_write_enabled(&realm).unwrap());
}

#[test]
fn a_non_loopback_base_url_override_is_refused_before_any_request() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db);
    let config = write_config(directory.path());

    let output = qbo_local(
        &[
            "daemon",
            "--db",
            db.to_str().unwrap(),
            "--realm",
            REALM,
            "--live",
            "--config",
            config.to_str().unwrap(),
            "--once",
        ],
        Some("https://example.com:443"),
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr:\n{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("BUNZBOOKS_QBO_BASE_URL"),
        "expected the refusal to name the variable:\n{}",
        stderr(&output)
    );
}
