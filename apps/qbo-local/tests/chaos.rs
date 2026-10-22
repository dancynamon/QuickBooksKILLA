//! Exactly-once across a real process kill. `DESIGN.md` §6.1, §10.
//!
//! Every other test in this crate proves the outbox state machine and the
//! drain worker are correct *within one process*. That leaves exactly the
//! failure `DESIGN.md` §6.1 is about untested: a process killed between
//! persisting `in_flight` and committing `mark_applied`, where the only thing
//! a second process has to go on is whatever reached disk. `bin/chaos-child`
//! is a real, separate process built for this: it opens a file [`Store`] and
//! a [`JournalledMock`] — the only [`QboClient`] double in this crate that
//! survives past its own process — seeds a dependency chain plus independent
//! records, drains them, and (`--kill-after K`) `abort()`s partway through.
//! This test kills it, restarts it without `--kill-after`, and checks that
//! what actually happened to "QBO" — read back from the journal a completely
//! separate process wrote — has no duplicates and no gaps.

use std::path::Path;
use std::process::Command;

use chrono::Utc;
use qbo_local::client::journal::JournalledMock;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::outbox::OutboxState;
use qbo_local::store::Store;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn chaos_child() -> &'static str {
    env!("CARGO_BIN_EXE_chaos-child")
}

/// Spawn the child. Returns whether it exited successfully — `false` covers
/// both an ordinary nonzero exit and the `SIGABRT` `--kill-after` produces.
fn run_child(
    db: &Path,
    journal_dir: &Path,
    independents: usize,
    kill_after: Option<usize>,
) -> bool {
    let mut command = Command::new(chaos_child());
    command
        .arg("--db")
        .arg(db)
        .arg("--journal-dir")
        .arg(journal_dir)
        .arg("--independents")
        .arg(independents.to_string());
    if let Some(k) = kill_after {
        command.arg("--kill-after").arg(k.to_string());
    }
    let output = command.output().expect("spawn chaos-child");
    if !output.status.success() {
        // Expected on the killed run; printed on an unexpected failure of the
        // "clean" respawn so a broken scenario is diagnosable from test output
        // rather than just "assertion failed".
        eprintln!(
            "chaos-child exited {:?}\n  stdout: {}\n  stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.status.success()
}

/// One (`--independents`, `--kill-after`) scenario: kill the child at exactly
/// that many successful client calls in, respawn until it converges, and
/// check the result for every exactly-once property `DESIGN.md` §6 promises.
fn run_scenario(independents: usize, kill_after: usize) {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    let journal_dir = directory.path().join("journal");

    let survived = run_child(&db, &journal_dir, independents, Some(kill_after));
    assert!(
        !survived,
        "independents={independents} kill_after={kill_after}: expected the child to abort"
    );

    // Respawn without --kill-after until it converges. Nothing in this
    // harness injects an ongoing failure, so recovery should succeed on the
    // very next run; the retry loop only guards against flakiness in the
    // harness itself, not a real property being tested.
    let mut converged = false;
    for _ in 0..3 {
        if run_child(&db, &journal_dir, independents, None) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "independents={independents} kill_after={kill_after}: never converged after the kill"
    );

    verify(&db, &journal_dir, independents, kill_after);
}

fn verify(db: &Path, journal_dir: &Path, independents: usize, kill_after: usize) {
    let label = format!("independents={independents} kill_after={kill_after}");
    let store = Store::open(db).unwrap();
    let records = store.list_outbox_records(&realm()).unwrap();

    let expected_total = 3 + independents; // customer, invoice, payment + independents
    assert_eq!(
        records.len(),
        expected_total,
        "{label}: record count changed"
    );

    // Every record applied exactly once, and nothing is left in a state
    // nothing drains — the specific bug an unrecovered `in_flight` row is.
    for record in &records {
        assert_eq!(
            record.state,
            OutboxState::Applied,
            "{label}: {} not applied",
            record.local_entity_id
        );
        assert_ne!(
            record.state,
            OutboxState::InFlight,
            "{label}: stuck in_flight"
        );
    }

    // The mock holds exactly one entity per local id — the duplicate-customer
    // failure DESIGN.md §6.5 exists to prevent did not happen, and neither did
    // a duplicate invoice or payment via a resent RequestId.
    let mock = JournalledMock::open(journal_dir, Utc::now()).unwrap();
    assert_eq!(
        mock.count(&realm(), EntityType::Customer),
        1,
        "{label}: duplicate customer"
    );
    assert_eq!(
        mock.count(&realm(), EntityType::Payment),
        1,
        "{label}: duplicate payment"
    );
    assert_eq!(
        mock.count(&realm(), EntityType::Invoice),
        1 + independents,
        "{label}: invoice count drifted from what was seeded"
    );

    // Convergence: every local id this run resolved names an entity that
    // really exists in the mock, under the id the outbox recorded for it — a
    // fresh pull from "QBO" would reconstruct exactly this replica, not
    // something adjacent to it.
    let id_map = store.load_local_id_map(&realm()).unwrap();
    assert_eq!(
        id_map.len(),
        expected_total,
        "{label}: id map missing an entry"
    );
    for record in &records {
        let qbo_id = id_map
            .iter()
            .find(|(local, _)| local == &record.local_entity_id)
            .map(|(_, qbo_id)| qbo_id.clone())
            .unwrap_or_else(|| panic!("{label}: no id_map entry for {}", record.local_entity_id));
        assert!(
            mock.get(&realm(), record.entity_type, &qbo_id).is_some(),
            "{label}: {} resolved to {qbo_id}, which the mock does not hold",
            record.local_entity_id
        );
    }

    // Every RequestId in calls.jsonl was sent at most twice — once, plus at
    // most one recovery replay after the kill — and never with two different
    // payloads, which is what would turn a safe replay into a corrupted
    // write.
    let calls = JournalledMock::read_calls(journal_dir).unwrap();
    assert!(!calls.is_empty(), "{label}: no calls were logged at all");
    let mut by_request: std::collections::HashMap<uuid::Uuid, Vec<&serde_json::Value>> =
        std::collections::HashMap::new();
    for call in &calls {
        by_request
            .entry(call.request_id)
            .or_default()
            .push(&call.payload);
    }
    for (request_id, payloads) in &by_request {
        assert!(
            payloads.len() <= 2,
            "{label}: request_id {request_id} was sent {} times",
            payloads.len()
        );
        assert!(
            payloads.iter().all(|payload| **payload == *payloads[0]),
            "{label}: request_id {request_id} was sent with different payloads across attempts"
        );
    }
}

#[test]
fn exactly_once_across_a_process_kill() {
    let mut scenarios = 0usize;
    // Several "seeds" (independent-record counts, which also change the total
    // number of successful calls and therefore the chain's position relative
    // to the rest) and, for each, a kill point after every successful call —
    // hitting the customer, the invoice, the payment, and every independent.
    for independents in 1..=3usize {
        let total = 3 + independents;
        for kill_after in 1..=total {
            run_scenario(independents, kill_after);
            scenarios += 1;
        }
    }
    // Not a real assertion on behaviour — just proof the loop above actually
    // ran the matrix it claims to, so a refactor that empties it fails loudly.
    assert_eq!(scenarios, (1..=3).map(|i| 3 + i).sum::<usize>());
}

#[test]
fn a_run_with_no_kill_converges_in_one_pass() {
    // The baseline the kill scenarios are a deviation from: nothing wrong
    // happens when nothing is killed.
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("replica.db");
    let journal_dir = directory.path().join("journal");

    assert!(run_child(&db, &journal_dir, 2, None));
    verify(&db, &journal_dir, 2, 0);
}
