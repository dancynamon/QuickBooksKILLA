//! Load QBO entities from a JSON file into a **scratch copy** of a replica:
//! `[{"entity_type": "Transfer", "raw": { ...QBO JSON... }}, ...]`, each
//! upserted and projected the way a sync would. For a practice run whose
//! replica predates a new entity type (docs/PRACTICE-RUN.md: D37's three were
//! read from QBO with read-only queries and loaded this way, because a
//! rebuilt `qbo-local` needs a person at the keychain prompt, D26).
//!
//! ```text
//! cargo run -p ledger --example load_replica_entities -- \
//!     --replica SCRATCH/replica.db --realm ID --file entities.json
//! ```
//!
//! Refuses the live replica's file name under `.local/` as a guard; it writes.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (mut replica, mut realm, mut file) = (None, None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--replica" => replica = args.next().map(PathBuf::from),
            "--realm" => realm = args.next(),
            "--file" => file = args.next().map(PathBuf::from),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let replica = replica.ok_or("missing --replica")?;
    if replica.components().any(|c| c.as_os_str() == ".local") {
        return Err("refusing a replica under .local/: load into a scratch copy".into());
    }
    let realm = RealmId::parse(realm.ok_or("missing --realm")?)?;
    let file = file.ok_or("missing --file")?;
    let store = Store::open(&replica)?;
    let rows: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
    let now = Utc::now();
    let mut loaded = 0usize;
    for row in rows {
        let entity_type = EntityType::parse(row["entity_type"].as_str().ok_or("entity_type")?)?;
        let raw = row["raw"].clone();
        let last_updated = raw["MetaData"]["LastUpdatedTime"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(now);
        let entity = MirroredEntity {
            entity_type,
            qbo_id: raw["Id"].as_str().ok_or("Id")?.to_string(),
            sync_token: raw["SyncToken"].as_str().unwrap_or("0").to_string(),
            last_updated_utc: last_updated,
            is_deleted: false,
            raw_json: raw,
        };
        store.upsert_entity(&realm, &entity, now)?;
        store.project_entity(&realm, &entity, now)?;
        loaded += 1;
    }
    println!("loaded {loaded} entities into {}", replica.display());
    Ok(())
}
