//! What one commit per entity actually costs on disk.
//!
//! An in-memory database has no fsync, so it cannot answer this — and measuring
//! it there would have made a claim the measurement did not support.

use chrono::{TimeZone, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::json;

#[test]
#[ignore = "measurement; run explicitly with --ignored"]
fn per_entity_commits_versus_batched_on_a_file_database() {
    use std::time::Instant;

    const COUNT: usize = 2_000;
    const PAGE: usize = 500;

    let realm = RealmId::parse("1234567890123456").unwrap();
    let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();

    let entities: Vec<MirroredEntity> = (0..COUNT)
        .map(|index| MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: format!("i{index}"),
            sync_token: "0".into(),
            last_updated_utc: now,
            is_deleted: false,
            raw_json: json!({
                "Id": format!("i{index}"),
                "DocNumber": format!("{}", 20_000 + index),
                "TxnDate": "2026-01-15",
                "CustomerRef": { "value": "31" },
                "TotalAmt": 100.00,
                "Line": [ { "LineNum": 1, "Description": "Foam blank, blue",
                            "Amount": 100.00, "DetailType": "SalesItemLineDetail",
                            "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
            }),
        })
        .collect();

    let directory = tempfile::tempdir().unwrap();

    let one_at_a_time = {
        let store = Store::open(directory.path().join("per-entity.db")).unwrap();
        store.register_realm(&realm, "Measurement", now).unwrap();
        let started = Instant::now();
        for entity in &entities {
            store.upsert_entity(&realm, entity, now).unwrap();
            store.project_entity(&realm, entity, now).unwrap();
        }
        started.elapsed()
    };

    let batched = {
        let store = Store::open(directory.path().join("batched.db")).unwrap();
        store.register_realm(&realm, "Measurement", now).unwrap();
        let started = Instant::now();
        for page in entities.chunks(PAGE) {
            store.apply_batch(&realm, page, None, now).unwrap();
        }
        started.elapsed()
    };

    println!(
        "  {COUNT} invoices, WAL + synchronous=FULL\n    \
         one commit per entity : {one_at_a_time:?}\n    \
         {PAGE} per commit      : {batched:?}"
    );

    assert!(
        batched < one_at_a_time,
        "batching should not be slower: {batched:?} vs {one_at_a_time:?}"
    );
}
