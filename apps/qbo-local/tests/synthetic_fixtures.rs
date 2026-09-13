//! `apps/qbo-local/tests/fixtures/synthetic/` — the committed fixture set
//! that gives the offline test suite real *response shapes* to work against
//! ahead of scrubbed ones from the real book replacing them (`HANDOFF.md`
//! §2.6).
//!
//! Two things live here:
//!
//! - `generate_synthetic_fixtures`, `#[ignore]`d because it writes to the
//!   repository rather than only reading it. It records from a seeded
//!   [`MockQbo`] using invented names already used elsewhere in this crate's
//!   tests, runs them through `tools/scrub-fixtures.py` exactly as a real
//!   recording would be, and replaces the committed directory with the
//!   result. Regenerate with:
//!
//!       cargo test -p qbo-local --test synthetic_fixtures -- --ignored generate_synthetic_fixtures
//!
//! - `replaying_the_committed_synthetic_fixtures_matches_expected_counts`,
//!   which always runs: it syncs a fresh in-memory replica from the committed
//!   directory through [`FixtureQbo`] and checks the counts that generation
//!   above produced.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use qbo_local::client::{EntityPayload, MockQbo};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncOptions};
use qbo_local::fixture::{FixtureQbo, RecordingQbo};
use qbo_local::store::Store;
use serde_json::json;

const REALM: &str = "1234567890123456";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn entity_types() -> [EntityType; 3] {
    [EntityType::Customer, EntityType::Item, EntityType::Invoice]
}

fn synthetic_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("synthetic")
}

fn payload(
    entity_type: EntityType,
    qbo_id: &str,
    raw_json: serde_json::Value,
    at: DateTime<Utc>,
) -> EntityPayload {
    EntityPayload {
        entity_type,
        qbo_id: qbo_id.to_string(),
        sync_token: "0".to_string(),
        last_updated_utc: at,
        is_deleted: false,
        raw_json,
    }
}

/// The invented book this fixture set describes. Two customers, one item,
/// two invoices — enough to exercise `CustomerRef`/`ItemRef` shapes without
/// being a copy of the real book. `HANDOFF.md` §2.6: invented up front, never
/// scrubbed after the fact.
fn seeded_mock(at: DateTime<Utc>, realm: &RealmId) -> MockQbo {
    let mut mock = MockQbo::new(at);
    mock.seed(
        realm,
        payload(
            EntityType::Customer,
            "1",
            json!({
                "Id": "1",
                "DisplayName": "BLUE HARBOR SWIM",
                "PrimaryEmailAddr": { "Address": "ap@bluesharborswim.example" },
                "PrimaryPhone": { "FreeFormNumber": "555-201-4488" },
                "BillAddr": {
                    "Line1": "BLUE HARBOR SWIM",
                    "Line2": "44 Tideway Ave",
                    "City": "Harborview",
                    "CountrySubDivisionCode": "NY",
                    "PostalCode": "10999"
                }
            }),
            at,
        ),
    );
    mock.seed(
        realm,
        payload(
            EntityType::Customer,
            "2",
            json!({ "Id": "2", "DisplayName": "FAIRVIEW COUNTY PARKS & REC." }),
            at,
        ),
    );
    mock.seed(
        realm,
        payload(
            EntityType::Item,
            "1",
            json!({ "Id": "1", "Name": "50-INCH RESCUE TUBE", "UnitPrice": 83.0 }),
            at,
        ),
    );
    mock.seed(
        realm,
        payload(
            EntityType::Invoice,
            "101",
            json!({
                "Id": "101",
                "DocNumber": "1001",
                "TxnDate": "2026-01-02",
                "CustomerRef": { "value": "1", "name": "BLUE HARBOR SWIM" },
                "Line": [{
                    "Id": "1",
                    "LineNum": 1,
                    "Amount": 249.0,
                    "DetailType": "SalesItemLineDetail",
                    "SalesItemLineDetail": {
                        "ItemRef": { "value": "1", "name": "50-INCH RESCUE TUBE" },
                        "Qty": 3,
                        "UnitPrice": 83.0
                    },
                    "Description": "50\" rescue tubes, red"
                }],
                "TotalAmt": 249.0,
                "Balance": 0.0,
                "PrivateNote": "Ship with next Friday's route"
            }),
            at,
        ),
    );
    mock.seed(
        realm,
        payload(
            EntityType::Invoice,
            "102",
            json!({
                "Id": "102",
                "DocNumber": "1002",
                "TxnDate": "2026-01-05",
                "CustomerRef": { "value": "2", "name": "FAIRVIEW COUNTY PARKS & REC." },
                "TotalAmt": 830.0,
                "Balance": 830.0
            }),
            at,
        ),
    );
    mock
}

#[test]
#[ignore = "regenerates tests/fixtures/synthetic; run explicitly with \
            `cargo test -p qbo-local --test synthetic_fixtures -- --ignored generate_synthetic_fixtures`"]
fn generate_synthetic_fixtures() {
    let realm = RealmId::parse(REALM).unwrap();
    let at = now();

    let scratch = tempfile::tempdir().unwrap();
    let raw_dir = scratch.path().join("raw");

    let recorder = RecordingQbo::new(seeded_mock(at, &realm), &raw_dir);
    let mut driver = SyncDriver::new(recorder, SyncOptions::default(), at);
    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm, "synthetic", at).unwrap();
    driver
        .sync_realm(&store, &realm, &entity_types(), at)
        .expect("recording sync failed");

    // Scrub exactly as a real recording would be — HANDOFF.md §2.6: only a
    // scrubbed directory is ever committed, never a hand-edited recording.
    let manifest_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_root
        .parent()
        .and_then(Path::parent)
        .expect("apps/qbo-local should be two levels under the repo root");
    let script = repo_root.join("tools").join("scrub-fixtures.py");
    let dest = synthetic_dir();

    if dest.exists() {
        std::fs::remove_dir_all(&dest).expect("failed to clear the previous synthetic fixtures");
    }

    let output = Command::new("python3")
        .arg(&script)
        .arg(&raw_dir)
        .arg(&dest)
        .output()
        .expect("failed to run tools/scrub-fixtures.py — is python3 on PATH?");
    assert!(
        output.status.success(),
        "scrub-fixtures.py failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn replaying_the_committed_synthetic_fixtures_matches_expected_counts() {
    let dir = synthetic_dir();
    assert!(
        dir.join("manifest.json").exists(),
        "expected a committed synthetic fixture set at {} — regenerate with \
         `cargo test -p qbo-local --test synthetic_fixtures -- --ignored generate_synthetic_fixtures`",
        dir.display()
    );

    let realm = RealmId::parse(REALM).unwrap();
    let at = now();

    let store = Store::open_in_memory().unwrap();
    store.register_realm(&realm, "synthetic-replay", at).unwrap();
    let client = FixtureQbo::new(&dir);
    let mut driver = SyncDriver::new(client, SyncOptions::default(), at);
    let report = driver
        .sync_realm(&store, &realm, &entity_types(), at)
        .expect("replay sync failed");

    assert_eq!(store.count_entities(&realm, EntityType::Customer).unwrap(), 2);
    assert_eq!(store.count_entities(&realm, EntityType::Item).unwrap(), 1);
    assert_eq!(store.count_entities(&realm, EntityType::Invoice).unwrap(), 2);
    assert_eq!(report.mirrored(), 5);
    assert_eq!(report.quarantined(), 0);
}
