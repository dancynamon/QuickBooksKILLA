//! `qbo-local` — status entry point.
//!
//! The Tauri shell and React UI are not built yet. This binary reports what the
//! M0 foundations currently hold, so the build is inspectable before there is
//! anything to look at.

use chrono::Utc;
use qbo_local::domain::{EntityType, RealmId, SyncTier};
use qbo_local::ratelimit::RealmLimits;
use qbo_local::store::{latest_version, ProjectedTable, Store};
use qbo_local::sync::{CDC_LOOKBACK_DAYS, DEFAULT_CDC_MAX_AGE_DAYS};

const PROJECTED_TABLES: &[ProjectedTable] = &[
    ProjectedTable::Contacts,
    ProjectedTable::Items,
    ProjectedTable::Accounts,
    ProjectedTable::Classes,
    ProjectedTable::Documents,
    ProjectedTable::DocumentLines,
    ProjectedTable::DocumentLinks,
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open_in_memory()?;
    // Nothing is mirrored yet; this exists so the counts below have a realm to
    // be zero for, rather than being printed as an unexplained blank.
    let realm = RealmId::parse("0000000000000000")?;
    store.register_realm(&realm, "empty replica", Utc::now())?;
    let limits = RealmLimits::default();

    println!("qbo-local — M0 foundations");
    println!();
    println!("  replica schema version : {}", store.schema_version()?);
    println!("  latest migration       : {}", latest_version());
    println!();

    let masters = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Masters).count();
    let documents = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Documents).count();
    let peripheral = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Peripheral).count();

    println!("  entity types           : {} total", EntityType::ALL.len());
    println!("    masters (M0)         : {masters}");
    println!("    documents (M0)       : {documents}");
    println!("    peripheral (later)   : {peripheral}");
    println!();

    println!("  rate budgets per realm");
    println!("    general              : {}/min", limits.general.refill_per_minute);
    println!("    batch                : {}/min", limits.batch.refill_per_minute);
    println!("    reports              : {}/min", limits.reports.refill_per_minute);
    println!("    max concurrent       : {}", limits.max_concurrent);
    println!();

    println!("  cdc lookback (documented) : {CDC_LOOKBACK_DAYS} days");
    println!("  cdc cursor max age (ours) : {DEFAULT_CDC_MAX_AGE_DAYS} days");
    println!();
    // An empty replica: the interesting figure is that these are all derived,
    // so every one of them can be thrown away and rebuilt from `entities`.
    let projected: i64 = PROJECTED_TABLES
        .iter()
        .map(|table| store.count_projected(&realm, *table).unwrap_or(0))
        .sum();

    println!("  projection (§3.2)");
    println!("    tables       : {}", PROJECTED_TABLES.len());
    println!("    rows         : {projected}");
    println!("    rebuilt from : entities.raw_json, no Intuit round-trip");
    println!();

    println!("  writes: disabled for every realm until explicitly enabled");
    println!("  generated at {}", Utc::now().to_rfc3339());

    Ok(())
}
