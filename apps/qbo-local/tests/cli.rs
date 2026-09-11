//! The `qbo-local` binary end to end: subcommands run as a real subprocess
//! against a file database, `HANDOFF.md` §2.5, `ROADMAP.md` §A.
//!
//! Names and figures are invented (`HANDOFF.md` §2.6).

use std::path::Path;
use std::process::{Command, Output};

const REALM: &str = "1234567890123456";
const HTTP_CLIENT_MISSING: &str =
    "HttpQboClient is not built yet (HANDOFF.md §2.3); run with --mock to exercise the loop";

fn qbo_local(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qbo-local"))
        .args(args)
        .output()
        .expect("failed to run the qbo-local binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn init(db: &Path, realm: &str, name: &str) -> Output {
    let output = qbo_local(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--realm",
        realm,
        "--name",
        name,
    ]);
    assert!(output.status.success(), "init failed: {}", stderr(&output));
    output
}

#[test]
fn init_then_status_shows_the_registered_realm() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");

    init(&db, REALM, "Aquamentor, Inc.");

    let status = qbo_local(&["status", "--db", db.to_str().unwrap()]);

    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    let text = stdout(&status);
    assert!(
        text.contains(REALM),
        "expected the realm id in status output:\n{text}"
    );
    assert!(
        text.contains("Aquamentor, Inc."),
        "expected the display name in status output:\n{text}"
    );
    assert!(
        text.contains("realms registered : 1"),
        "expected a realm count:\n{text}"
    );
}

#[test]
fn init_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");

    init(&db, REALM, "Aquamentor, Inc.");
    // Registering the same realm again is not an error.
    let second = init(&db, REALM, "Aquamentor (renamed)");
    assert!(second.status.success());

    let status = qbo_local(&["status", "--db", db.to_str().unwrap()]);
    let text = stdout(&status);
    assert!(
        text.contains("realms registered : 1"),
        "re-registering must not duplicate the realm:\n{text}"
    );
    assert!(text.contains("Aquamentor (renamed)"));
}

#[test]
fn daemon_once_mock_exits_ok_and_prints_a_tick_line() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db, REALM, "Aquamentor, Inc.");

    let output = qbo_local(&[
        "daemon",
        "--db",
        db.to_str().unwrap(),
        "--realm",
        REALM,
        "--once",
        "--mock",
    ]);

    assert!(
        output.status.success(),
        "daemon --once --mock failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("next_delay="),
        "expected a tick line:\n{text}"
    );
    assert!(text.contains("snapshot="), "expected a tick line:\n{text}");
    // The per-entity SyncReport table.
    assert!(
        text.contains("entity"),
        "expected the sync report table:\n{text}"
    );
    assert!(
        text.contains("next delay:"),
        "expected the next-delay line:\n{text}"
    );
}

#[test]
fn sweep_mock_exits_ok_and_prints_a_clean_report() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db, REALM, "Aquamentor, Inc.");

    let output = qbo_local(&[
        "sweep",
        "--db",
        db.to_str().unwrap(),
        "--realm",
        REALM,
        "--mock",
    ]);

    assert!(
        output.status.success(),
        "sweep --mock failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("entity"),
        "expected the reconcile report table:\n{text}"
    );
    assert!(
        text.contains("clean                   : true"),
        "expected a clean report:\n{text}"
    );
}

#[test]
fn snapshot_creates_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db, REALM, "Aquamentor, Inc.");
    let snapshot_dir = directory.path().join("snapshots");

    let output = qbo_local(&[
        "snapshot",
        "--db",
        db.to_str().unwrap(),
        "--dir",
        snapshot_dir.to_str().unwrap(),
    ]);

    assert!(
        output.status.success(),
        "snapshot failed: {}",
        stderr(&output)
    );
    let entries: Vec<_> = std::fs::read_dir(&snapshot_dir)
        .expect("snapshot directory should have been created")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one snapshot file");
    assert!(entries[0].path().exists());
}

#[test]
fn daemon_without_mock_exits_2_with_the_exact_message() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db, REALM, "Aquamentor, Inc.");

    let output = qbo_local(&["daemon", "--db", db.to_str().unwrap(), "--realm", REALM]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output).trim_end(), HTTP_CLIENT_MISSING);
    assert!(
        stdout(&output).is_empty(),
        "no tick output should have been printed"
    );
}

#[test]
fn sweep_without_mock_exits_2_with_the_exact_message() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    init(&db, REALM, "Aquamentor, Inc.");

    let output = qbo_local(&["sweep", "--db", db.to_str().unwrap(), "--realm", REALM]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output).trim_end(), HTTP_CLIENT_MISSING);
}

#[test]
fn no_arguments_exits_2_with_usage() {
    let output = qbo_local(&[]);

    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(
        text.contains("USAGE"),
        "expected usage text on stderr:\n{text}"
    );
    assert!(
        text.contains("qbo-local status"),
        "expected usage text on stderr:\n{text}"
    );
}

#[test]
fn help_exits_2_and_lists_every_subcommand() {
    let output = qbo_local(&["--help"]);

    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    for subcommand in ["status", "init", "daemon", "sweep", "snapshot"] {
        assert!(
            text.contains(subcommand),
            "expected {subcommand:?} in --help output:\n{text}"
        );
    }
}

#[test]
fn status_without_db_runs_in_memory() {
    let output = qbo_local(&["status"]);

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(
        text.contains("in-memory"),
        "expected the in-memory note:\n{text}"
    );
}
