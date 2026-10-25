//! The fixture pipeline end to end, offline: an invented replica is sliced
//! (`qbo_local::slice`), the slice is recorded, `tools/scrub-fixtures.py`
//! scrubs it, and the scrubbed set replays into a fresh replica that is
//! still closed, still consistent and holds nothing of the original's
//! names, contacts, realm id or amounts. `HANDOFF.md` §2.6, `DECISIONS.md`
//! D31.
//!
//! Also runs the scrubber's own Python suites (`tools/test_scrub_*.py`), so
//! `cargo test --workspace` covers the property tests over generated books.
//! Needs `python3` on PATH, as `synthetic_fixtures.rs` already does.
//!
//! Every name, address and amount here is invented.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncOptions};
use qbo_local::fixture::FixtureQbo;
use qbo_local::slice::{self, SliceOptions, DENY_NAMES_FILE};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

const REALM: &str = "9130350000000001";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("apps/qbo-local should be two levels under the repo root")
        .to_path_buf()
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn python(args: &[&str]) -> std::process::Output {
    Command::new("python3")
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("failed to run python3 — is it on PATH?")
}

fn assert_ok(output: &std::process::Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn the_python_scrubber_suites_pass() {
    let output = python(&[
        "-m",
        "unittest",
        "discover",
        "-s",
        "tools",
        "-p",
        "test_scrub_*.py",
    ]);
    assert_ok(
        &output,
        "python3 -m unittest discover -s tools -p 'test_scrub_*.py'",
    );
}

/// Two customers sharing one deposit, a third the slice leaves out, a
/// vendor with a bill and a bill payment, a journal entry, and the masters
/// they reference. Every invariant `check_invariants` knows holds.
fn invented_book() -> Vec<(EntityType, &'static str, Value)> {
    use EntityType as E;
    vec![
        (
            E::Account,
            "1",
            json!({"Id": "1", "Name": "Checking", "AcctNum": "4417", "CurrentBalance": 5120.5}),
        ),
        (
            E::Account,
            "2",
            json!({"Id": "2", "Name": "Sales", "CurrentBalance": 18300.0}),
        ),
        (
            E::Account,
            "3",
            json!({"Id": "3", "Name": "Supplies", "CurrentBalance": 940.25}),
        ),
        (E::Term, "4", json!({"Id": "4", "Name": "Net 30"})),
        (E::Class, "5", json!({"Id": "5", "Name": "Wholesale"})),
        (
            E::Item,
            "10",
            json!({"Id": "10", "Name": "36in Rescue Tube", "UnitPrice": 41.5,
            "IncomeAccountRef": {"value": "2"}, "PrefVendorRef": {"value": "300"}}),
        ),
        (
            E::Customer,
            "100",
            json!({"Id": "100", "DisplayName": "Gannetfield Swim League",
            "PrimaryEmailAddr": {"Address": "office@gannetfield-swim.test"},
            "PrimaryPhone": {"FreeFormNumber": "(518) 555-7741"},
            "BillAddr": {"Line1": "Gannetfield Swim League", "Line2": "88 Puffin Wharf Rd", "City": "Troy",
                         "CountrySubDivisionCode": "NY", "PostalCode": "12180"},
            "SalesTermRef": {"value": "4"}, "Balance": 83.0, "BalanceWithJobs": 83.0}),
        ),
        (
            E::Customer,
            "200",
            json!({"Id": "200", "DisplayName": "Ob", "Balance": 0.0}),
        ),
        (
            E::Customer,
            "999",
            json!({"Id": "999", "DisplayName": "Marrowby Lido Trust", "Balance": 0.0}),
        ),
        (
            E::Vendor,
            "300",
            json!({"Id": "300", "DisplayName": "Corvidae Foam Supply",
            "PrimaryEmailAddr": {"Address": "billing@corvidae-foam.test"}, "Balance": 120.0}),
        ),
        (
            E::Invoice,
            "1000",
            json!({"Id": "1000", "DocNumber": "5001", "TxnDate": "2026-04-02",
            "CustomerRef": {"value": "100", "name": "Gannetfield Swim League"}, "ClassRef": {"value": "5"},
            "Line": [{"Amount": 166.0, "DetailType": "SalesItemLineDetail", "Description": "Same order as Marrowby Lido Trust",
                      "SalesItemLineDetail": {"ItemRef": {"value": "10"}, "Qty": 4, "UnitPrice": 41.5, "TaxCodeRef": {"value": "NON"}}}],
            "TotalAmt": 166.0, "Balance": 83.0, "BillEmail": {"Address": "office@gannetfield-swim.test"},
            "LinkedTxn": [{"TxnId": "1001", "TxnType": "Payment"}]}),
        ),
        (
            E::Payment,
            "1001",
            json!({"Id": "1001", "TxnDate": "2026-04-20", "CustomerRef": {"value": "100"},
            "Line": [{"Amount": 83.0, "LinkedTxn": [{"TxnId": "1000", "TxnType": "Invoice"}]}],
            "TotalAmt": 83.0, "UnappliedAmt": 0.0, "DepositToAccountRef": {"value": "1"},
            "LinkedTxn": [{"TxnId": "5000", "TxnType": "Deposit"}]}),
        ),
        (
            E::Invoice,
            "2000",
            json!({"Id": "2000", "TxnDate": "2026-04-03", "CustomerRef": {"value": "200"},
            "Line": [{"Amount": 249.0, "DetailType": "SalesItemLineDetail",
                      "SalesItemLineDetail": {"ItemRef": {"value": "10"}, "Qty": 6, "UnitPrice": 41.5}}],
            "TotalAmt": 249.0, "Balance": 0.0}),
        ),
        (
            E::Payment,
            "2001",
            json!({"Id": "2001", "TxnDate": "2026-04-21", "CustomerRef": {"value": "200"},
            "Line": [{"Amount": 249.0, "LinkedTxn": [{"TxnId": "2000", "TxnType": "Invoice"}]}],
            "TotalAmt": 249.0, "UnappliedAmt": 0.0}),
        ),
        (
            E::Deposit,
            "5000",
            json!({"Id": "5000", "DepositToAccountRef": {"value": "1"},
            "Line": [{"Amount": 83.0, "LinkedTxn": [{"TxnId": "1001", "TxnType": "Payment"}]},
                     {"Amount": 249.0, "LinkedTxn": [{"TxnId": "2001", "TxnType": "Payment"}]}],
            "TotalAmt": 332.0}),
        ),
        (
            E::Invoice,
            "9000",
            json!({"Id": "9000", "TxnDate": "2026-04-04", "CustomerRef": {"value": "999"},
            "Line": [{"Amount": 41.5, "DetailType": "SalesItemLineDetail", "SalesItemLineDetail": {"ItemRef": {"value": "10"}}}],
            "TotalAmt": 41.5, "Balance": 0.0}),
        ),
        (
            E::Bill,
            "3000",
            json!({"Id": "3000", "TxnDate": "2026-03-01", "VendorRef": {"value": "300"},
            "Line": [{"Amount": 360.0, "DetailType": "AccountBasedExpenseLineDetail",
                      "AccountBasedExpenseLineDetail": {"AccountRef": {"value": "3"}}}],
            "TotalAmt": 360.0, "Balance": 120.0}),
        ),
        (
            E::BillPayment,
            "3001",
            json!({"Id": "3001", "TxnDate": "2026-03-15", "VendorRef": {"value": "300"},
            "Line": [{"Amount": 240.0, "LinkedTxn": [{"TxnId": "3000", "TxnType": "Bill"}]}],
            "TotalAmt": 240.0, "CheckPayment": {"BankAccountRef": {"value": "1"}}}),
        ),
        (
            E::JournalEntry,
            "4000",
            json!({"Id": "4000", "TxnDate": "2026-03-31", "PrivateNote": "Reclass 1,250.00", "TotalAmt": 1250.0,
            "Line": [
                {"Amount": 1250.0, "DetailType": "JournalEntryLineDetail", "JournalEntryLineDetail": {
                    "PostingType": "Debit", "AccountRef": {"value": "3"},
                    "Entity": {"Type": "Vendor", "EntityRef": {"value": "300"}}}},
                {"Amount": 1250.0, "DetailType": "JournalEntryLineDetail", "JournalEntryLineDetail": {
                    "PostingType": "Credit", "AccountRef": {"value": "1"}}}]}),
        ),
    ]
}

fn build_replica(path: &Path, realm: &RealmId) {
    let store = Store::open(path).unwrap();
    store.register_realm(realm, "invented", at()).unwrap();
    for (entity_type, id, raw) in invented_book() {
        let entity = MirroredEntity {
            entity_type,
            qbo_id: id.to_string(),
            sync_token: "0".to_string(),
            last_updated_utc: at(),
            is_deleted: false,
            raw_json: raw,
        };
        store.upsert_entity(realm, &entity, at()).unwrap();
    }
}

#[test]
fn a_slice_scrubs_and_replays_closed_consistent_and_clean() {
    let realm = RealmId::parse(REALM).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let db = scratch.path().join("replica.db");
    build_replica(&db, &realm);

    // -- slice ----------------------------------------------------------------
    let store = Store::open_read_only(&db).unwrap();
    let options = SliceOptions {
        customers: 0,
        vendors: 0,
        customer_ids: vec!["100".into()],
        vendor_ids: vec!["300".into()],
        ..SliceOptions::default()
    };
    let report = slice::slice_realm(&store, &realm, &options).unwrap();
    let held: HashSet<(EntityType, String)> = report
        .entities
        .iter()
        .map(|e| (e.entity_type, e.qbo_id.clone()))
        .collect();
    let has = |t: EntityType, id: &str| held.contains(&(t, id.to_string()));
    assert!(has(EntityType::Invoice, "1000") && has(EntityType::Payment, "1001"));
    assert!(has(EntityType::Bill, "3000") && has(EntityType::BillPayment, "3001"));
    assert!(has(EntityType::JournalEntry, "4000") && has(EntityType::Item, "10"));
    assert!(
        has(EntityType::Term, "4") && has(EntityType::Class, "5") && has(EntityType::Account, "1")
    );
    assert!(
        !has(EntityType::Deposit, "5000"),
        "a deposit batching an outside payment joined"
    );
    assert!(!has(EntityType::Customer, "999") && !has(EntityType::Invoice, "9000"));
    assert_eq!(
        report.pruned_links, 1,
        "payment 1001's link to the left-out deposit"
    );
    assert!(report.deny_names.contains("Marrowby Lido Trust"));

    let raw_dir = scratch.path().join("slice-raw");
    slice::record_slice(&report, &realm, &raw_dir, at()).unwrap();
    assert!(raw_dir.join(DENY_NAMES_FILE).exists());
    assert!(
        slice::record_slice(&report, &realm, &raw_dir, at()).is_err(),
        "recording into a non-empty directory must be refused"
    );

    // -- scrub ----------------------------------------------------------------
    let seed_file = scratch.path().join("seed");
    fs::write(&seed_file, "integration-test-seed\n").unwrap();
    let out_dir = scratch.path().join("slice-scrubbed");
    let script = repo_root().join("tools").join("scrub-fixtures.py");
    let output = python(&[
        script.to_str().unwrap(),
        "--seed-file",
        seed_file.to_str().unwrap(),
        raw_dir.to_str().unwrap(),
        out_dir.to_str().unwrap(),
    ]);
    assert_ok(&output, "scrub-fixtures.py");

    // -- nothing original survives ---------------------------------------------
    let mut text = String::new();
    for entry in walk(&out_dir) {
        text.push_str(&fs::read_to_string(entry).unwrap());
    }
    for needle in [
        REALM,
        "Gannetfield",
        "Corvidae",
        "Marrowby",
        "gannetfield-swim.test",
        "corvidae-foam.test",
        "555-7741",
        "Puffin Wharf",
        "4417",
        "1,250.00",
    ] {
        assert!(!text.contains(needle), "{needle:?} survived scrubbing");
    }

    // -- replay: closed, complete, consistent ----------------------------------
    let replica = Store::open_in_memory().unwrap();
    let placeholder = RealmId::parse("1234567890123456").unwrap();
    replica
        .register_realm(&placeholder, "replayed", at())
        .unwrap();
    let mut driver = SyncDriver::new(FixtureQbo::new(&out_dir), SyncOptions::default(), at());
    let entity_types: Vec<EntityType> = EntityType::m0_scope().collect();
    let sync = driver
        .sync_realm(&replica, &placeholder, &entity_types, at())
        .expect("replaying the scrubbed slice failed");
    assert_eq!(
        sync.quarantined(),
        0,
        "{:?}",
        replica.quarantined(&placeholder).unwrap()
    );
    assert_eq!(sync.mirrored(), report.entities.len());

    let mut replayed = Vec::new();
    replica
        .for_each_entity(&placeholder, |entity| {
            replayed.push(entity);
            Ok(())
        })
        .unwrap();
    let known: HashSet<(EntityType, String)> = invented_book()
        .into_iter()
        .map(|(t, id, _)| (t, id.to_string()))
        .collect();
    assert_eq!(
        slice::dangling(&replayed, &known),
        vec![],
        "the replayed slice is not closed"
    );

    let invoice = replayed
        .iter()
        .find(|e| e.entity_type == EntityType::Invoice && e.qbo_id == "1000")
        .unwrap();
    assert_ne!(invoice.raw_json["TotalAmt"], json!(166.0));
    let total = invoice.raw_json["TotalAmt"].as_f64().unwrap();
    let balance = invoice.raw_json["Balance"].as_f64().unwrap();
    assert!(
        0.0 < balance && balance < total,
        "a part-paid invoice stays part-paid"
    );

    let check = python(&[
        "-I",
        "-c",
        "import importlib.util, json, pathlib, sys\n\
         spec = importlib.util.spec_from_file_location('sf', sys.argv[1])\n\
         sf = importlib.util.module_from_spec(spec); sys.modules['sf'] = sf; spec.loader.exec_module(sf)\n\
         entities = {}\n\
         for path in sorted(pathlib.Path(sys.argv[2]).rglob('query-*.json')):\n\
         \x20   for p in json.loads(path.read_text()):\n\
         \x20       entities[(p['entity_type'], p['qbo_id'])] = p['raw_json']\n\
         bad = sf.check_invariants(entities)\n\
         assert not bad, sorted(bad)\n\
         print(len(entities), 'entities consistent')\n",
        script.to_str().unwrap(),
        out_dir.to_str().unwrap(),
    ]);
    assert_ok(&check, "invariant check over the scrubbed slice");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}
