//! The chaos test's child process. `DESIGN.md` §6.1, §10.
//!
//! `tests/chaos.rs` cannot exercise the `in_flight` recovery path from inside
//! its own process — a killed thread does not leave anything for the test to
//! restart into. This binary is what gets killed instead: it opens a real
//! file [`Store`], seeds a dependency chain of outbox records on its first
//! run, drains them against a [`JournalledMock`] that persists to the same
//! directory across invocations, and — given `--kill-after` — aborts partway
//! through, at exactly the window `DESIGN.md` §6.1 describes: after a remote
//! call has succeeded and before the local `mark_applied` commit. A second
//! invocation without `--kill-after` reopens the same store and journal and
//! must finish the job exactly once.
//!
//! No `clap`, matching `main.rs`: this crate's dependency list does not grow
//! for a test harness.

use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use uuid::Uuid;

use qbo_local::client::journal::JournalledMock;
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::outbox::{NewOutboxRecord, Operation, OutboxRecord, OutboxState};
use qbo_local::store::Store;
use qbo_local::worker::Drainer;

struct Args {
    db: PathBuf,
    journal_dir: PathBuf,
    kill_after: Option<usize>,
    independents: usize,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut db = None;
    let mut journal_dir = None;
    let mut kill_after = None;
    let mut independents = 3;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(next(&mut iter, "--db")?)),
            "--journal-dir" => journal_dir = Some(PathBuf::from(next(&mut iter, "--journal-dir")?)),
            "--kill-after" => {
                kill_after = Some(
                    next(&mut iter, "--kill-after")?
                        .parse::<usize>()
                        .map_err(|e| format!("--kill-after: {e}"))?,
                )
            }
            "--independents" => {
                independents = next(&mut iter, "--independents")?
                    .parse::<usize>()
                    .map_err(|e| format!("--independents: {e}"))?
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }

    Ok(Args {
        db: db.ok_or("--db is required")?,
        journal_dir: journal_dir.ok_or("--journal-dir is required")?,
        kill_after,
        independents,
    })
}

fn next<'a>(iter: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
    iter.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

/// The records a fresh store starts with: a dependency chain — customer,
/// invoice, payment — plus `independents` unrelated invoices that never wait
/// on anything, so the drain has both the hard case (§6.4) and the ordinary
/// one in the same run.
fn seed_records(now: chrono::DateTime<Utc>, independents: usize) -> Vec<OutboxRecord> {
    let mut records = Vec::new();

    let customer = OutboxRecord::new(
        NewOutboxRecord {
            id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            realm_id: realm(),
            entity_type: EntityType::Customer,
            operation: Operation::Create,
            payload_json: serde_json::json!({ "DisplayName": "Blue Harbor Swim Club" }),
            local_entity_id: "local:cust-1".to_string(),
            base_sync_token: None,
        },
        now,
    );

    let invoice = OutboxRecord::new(
        NewOutboxRecord {
            id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            realm_id: realm(),
            entity_type: EntityType::Invoice,
            operation: Operation::Create,
            payload_json: serde_json::json!({
                "CustomerRef": { "value": "local:cust-1" },
                "DocNumber": "9001",
                "TotalAmt": 100.0,
            }),
            local_entity_id: "local:inv-1".to_string(),
            base_sync_token: None,
        },
        now,
    )
    .depending_on(customer.id);

    let payment = OutboxRecord::new(
        NewOutboxRecord {
            id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            realm_id: realm(),
            entity_type: EntityType::Payment,
            operation: Operation::Create,
            payload_json: serde_json::json!({
                "CustomerRef": { "value": "local:cust-1" },
                "TotalAmt": 100.0,
                "Line": [ { "Amount": 100.0,
                            "LinkedTxn": [ { "TxnId": "local:inv-1", "TxnType": "Invoice" } ] } ]
            }),
            local_entity_id: "local:pay-1".to_string(),
            base_sync_token: None,
        },
        now,
    )
    .depending_on(invoice.id);

    records.push(customer);
    records.push(invoice);
    records.push(payment);

    for index in 0..independents {
        records.push(OutboxRecord::new(
            NewOutboxRecord {
                id: Uuid::now_v7(),
                request_id: Uuid::now_v7(),
                realm_id: realm(),
                entity_type: EntityType::Invoice,
                operation: Operation::Create,
                payload_json: serde_json::json!({
                    "CustomerRef": { "value": "999" },
                    "DocNumber": format!("9{index:03}"),
                    "TotalAmt": 50.0,
                }),
                local_entity_id: format!("local:inv-indep-{index}"),
                base_sync_token: None,
            },
            now,
        ));
    }

    records
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&raw) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("chaos-child: {message}");
            return ExitCode::from(2);
        }
    };

    let store = std::rc::Rc::new(Store::open(&args.db).expect("open file store"));
    let now = Utc::now();

    if store
        .list_outbox_records(&realm())
        .expect("list outbox")
        .is_empty()
    {
        store
            .register_realm(&realm(), "Chaos Test Co.", now)
            .expect("register realm");
        for record in seed_records(now, args.independents) {
            store
                .insert_outbox_record(&record)
                .expect("seed outbox record");
        }
    }

    let mut records = store.list_outbox_records(&realm()).expect("list outbox");

    // DESIGN.md §6.1: a record found `in_flight` belonged to a process that no
    // longer exists. Recovery returns it to the queue; the guards that run on
    // every attempt regardless — RequestId reuse, and query-before-create for
    // Customer/Item — are what make resending it safe rather than a duplicate.
    for record in &mut records {
        if record.state == OutboxState::InFlight {
            record
                .recover_in_flight(now)
                .expect("recover in_flight record");
            store
                .save_outbox_record(record)
                .expect("persist recovered record");
        }
    }

    let mut drainer = Drainer::new();
    for (local_entity_id, qbo_id) in store.load_local_id_map(&realm()).expect("load id map") {
        drainer.seed_mapping(&realm(), &local_entity_id, &qbo_id);
    }

    // Persistence seam: every transition is written to the store immediately,
    // in particular `in_flight` before the client call it guards — the
    // durability `DESIGN.md` §6.1 requires and this whole binary exists to
    // test.
    let store_for_hook = store.clone();
    drainer.set_on_transition(move |record, applied_qbo_id| {
        store_for_hook
            .save_outbox_record(record)
            .expect("persist outbox transition");
        if let Some(qbo_id) = applied_qbo_id {
            store_for_hook
                .record_local_id_mapping(
                    &realm(),
                    &record.local_entity_id,
                    record.entity_type,
                    qbo_id,
                    Utc::now(),
                )
                .expect("persist resolved id mapping");
        }
    });

    if let Some(kill_after) = args.kill_after {
        drainer.set_crash_hook(move |count| {
            if count == kill_after {
                eprintln!("chaos-child: aborting after successful call #{count} (--kill-after {kill_after})");
                std::process::abort();
            }
        });
    }

    let mut client = JournalledMock::open(&args.journal_dir, now).expect("open journal");

    let converged = |records: &[OutboxRecord]| {
        records
            .iter()
            .all(|r| r.state == OutboxState::Applied || r.state.needs_attention())
    };

    let max_passes = qbo_local::outbox::MAX_ATTEMPTS as usize + records.len() + 4;
    for _ in 0..max_passes {
        if converged(&records) {
            break;
        }
        drainer.drain(&mut records, &mut client, Utc::now());
    }

    if converged(&records) {
        println!(
            "chaos-child: converged — {} records, {} applied",
            records.len(),
            records
                .iter()
                .filter(|r| r.state == OutboxState::Applied)
                .count()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "chaos-child: did not converge: {:?}",
            records
                .iter()
                .map(|r| (r.local_entity_id.clone(), r.state))
                .collect::<Vec<_>>()
        );
        ExitCode::FAILURE
    }
}
