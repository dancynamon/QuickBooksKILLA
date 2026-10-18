//! The write path, end to end: queue a change locally, drain it to QBO.
//! `DESIGN.md` §6 and §8.
//!
//! Queueing is the local commit the UI waits for: one row in the outbox, in
//! milliseconds, with no network. Draining is what the background loop does
//! later. Both refuse outright for a realm whose writes are off, so the gate
//! in §8 is enforced here and not only by a disabled button.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::client::QboClient;
use crate::domain::{EntityType, RealmId};
use crate::outbox::{NewOutboxRecord, Operation, OutboxError, OutboxRecord, OutboxState};
use crate::store::{Store, StoreError};
use crate::worker::{DrainReport, Drainer, LOCAL_ID_PREFIX};

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("writes are off for this company; turn them on with `qbo-local writes --enable` once the read-only period is over")]
    WritesOff,
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("outbox: {0}")]
    Outbox(#[from] OutboxError),
    #[error("no outbox record {0}")]
    NoSuchRecord(Uuid),
    #[error("{0}")]
    Invalid(String),
}

fn require_writes(store: &Store, realm: &RealmId) -> Result<(), WriteError> {
    if store.is_write_enabled(realm)? {
        Ok(())
    } else {
        Err(WriteError::WritesOff)
    }
}

/// Every `local:` id a payload references.
fn local_references(value: &serde_json::Value, out: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) if s.starts_with(LOCAL_ID_PREFIX) => {
            out.insert(s.clone());
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| local_references(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| local_references(v, out)),
        _ => {}
    }
}

/// The create record behind a local id that QBO has not yet assigned, if
/// any. A local id with no create in the outbox is a bug in the caller.
fn parent_of(store: &Store, realm: &RealmId, local_id: &str) -> Result<Option<Uuid>, WriteError> {
    if store.resolve_local_id(realm, local_id)?.is_some() {
        return Ok(None);
    }
    let parent = store
        .outbox_records(
            realm,
            &[OutboxState::Pending, OutboxState::InFlight, OutboxState::Conflicted, OutboxState::Rejected],
        )?
        .into_iter()
        .find(|r| r.operation == Operation::Create && r.local_entity_id == local_id);
    match parent {
        Some(record) => Ok(Some(record.id)),
        None => Err(WriteError::Invalid(format!("{local_id} refers to nothing queued or synced"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn queue(
    store: &Store,
    realm: &RealmId,
    entity_type: EntityType,
    operation: Operation,
    local_entity_id: String,
    payload: serde_json::Value,
    base_sync_token: Option<String>,
    now: DateTime<Utc>,
) -> Result<OutboxRecord, WriteError> {
    require_writes(store, realm)?;
    let mut references = HashSet::new();
    local_references(&payload, &mut references);
    references.remove(&local_entity_id);
    // One parent is enough to order on: the drain also refuses to send any
    // payload with an unresolved local reference, so a second parent still
    // holds the record back until it lands.
    let mut depends_on = None;
    let mut sorted: Vec<_> = references.into_iter().collect();
    sorted.sort();
    for reference in sorted {
        if let Some(parent) = parent_of(store, realm, &reference)? {
            depends_on = Some(parent);
        }
    }
    let mut record = OutboxRecord::new(
        NewOutboxRecord {
            id: Uuid::now_v7(),
            // Minted once, here, and stored. Every retry reuses it.
            request_id: Uuid::now_v7(),
            realm_id: realm.clone(),
            entity_type,
            operation,
            payload_json: payload,
            local_entity_id,
            base_sync_token,
        },
        now,
    );
    record.depends_on = depends_on;
    store.enqueue(&record)?;
    Ok(record)
}

/// Queue a create. Returns the record; its `local_entity_id` is usable in
/// other payloads straight away, before QBO has seen it.
pub fn queue_create(
    store: &Store,
    realm: &RealmId,
    entity_type: EntityType,
    payload: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<OutboxRecord, WriteError> {
    let local_id = format!("{LOCAL_ID_PREFIX}{}", Uuid::now_v7());
    queue(store, realm, entity_type, Operation::Create, local_id, payload, None, now)
}

/// Queue an update to a record QBO already holds, based on the `SyncToken`
/// the replica holds now. If QBO has moved on since, the drain reports a
/// conflict rather than overwriting the other edit.
pub fn queue_update(
    store: &Store,
    realm: &RealmId,
    entity_type: EntityType,
    qbo_id: &str,
    payload: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<OutboxRecord, WriteError> {
    let entity = store
        .get_entity(realm, entity_type, qbo_id)?
        .ok_or_else(|| WriteError::Invalid(format!("{} {qbo_id} is not in the replica", entity_type.as_str())))?;
    if entity.is_deleted {
        return Err(WriteError::Invalid(format!("{} {qbo_id} was deleted in QuickBooks", entity_type.as_str())));
    }
    queue(store, realm, entity_type, Operation::Update, qbo_id.to_string(), payload, Some(entity.sync_token), now)
}

/// What a person chose in the failure inbox.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Send it again. For a conflict this re-bases the edit on the version
    /// the replica holds now — which the person has just looked at — rather
    /// than the one it was originally based on.
    Retry,
    /// Do not send it. The record stays in the outbox as discarded.
    Discard,
}

pub fn resolve(
    store: &Store,
    realm: &RealmId,
    id: Uuid,
    resolution: Resolution,
    now: DateTime<Utc>,
) -> Result<OutboxRecord, WriteError> {
    let mut record = store.outbox_record(id)?.ok_or(WriteError::NoSuchRecord(id))?;
    if &record.realm_id != realm {
        return Err(WriteError::NoSuchRecord(id));
    }
    match resolution {
        Resolution::Discard => record.discard(now)?,
        Resolution::Retry => {
            require_writes(store, realm)?;
            if record.state == OutboxState::Conflicted && record.operation == Operation::Update {
                let current = store
                    .get_entity(realm, record.entity_type, &record.local_entity_id)?
                    .ok_or_else(|| WriteError::Invalid("the record is no longer in the replica".into()))?;
                if Some(&current.sync_token) == record.base_sync_token.as_ref() {
                    return Err(WriteError::Invalid(
                        "the replica has not picked up the other edit yet; sync, look at it, then retry".into(),
                    ));
                }
                record.base_sync_token = Some(current.sync_token);
            }
            record.resolve(now)?;
        }
    }
    store.save_outbox_state(&record)?;
    Ok(record)
}

/// The minimum read-only period before a production company may take writes
/// (`DESIGN.md` §8). Counted from the first complete sweep.
pub const SOAK_DAYS: i64 = 14;

/// Whether writes may be switched on for `realm` now, and if not, why.
///
/// - A sandbox company: always.
/// - A production company: only after [`SOAK_DAYS`] of read-only use since its
///   first complete sweep, only with `confirmed` (the explicit go), and only
///   when no other production company already has writes on.
pub fn check_enable(store: &Store, realm: &RealmId, confirmed: bool, now: DateTime<Utc>) -> Result<(), String> {
    use crate::store::outbox_store::Environment;
    let environment = store.realm_environment(realm).map_err(|e| e.to_string())?;
    if environment == Environment::Sandbox {
        return Ok(());
    }
    let first = store
        .first_full_sweep(realm)
        .map_err(|e| e.to_string())?
        .ok_or("this company has never completed a full sync; the read-only period has not started")?;
    let elapsed = (now - first).num_days();
    if elapsed < SOAK_DAYS {
        return Err(format!(
            "read-only period: {elapsed} of {SOAK_DAYS} days done since {}; {} to go",
            first.date_naive(),
            SOAK_DAYS - elapsed
        ));
    }
    if !confirmed {
        return Err("production writes need the explicit go: pass --confirm".into());
    }
    for other in store.realms().map_err(|e| e.to_string())? {
        if &other.realm_id != realm
            && other.is_write_enabled
            && store.realm_environment(&other.realm_id).map_err(|e| e.to_string())? == Environment::Production
        {
            return Err(format!("{} already has writes on; one production company at a time", other.display_name));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// Records a crash had left `in_flight`, returned to the queue.
    pub recovered: usize,
    pub drain: DrainReport,
}

/// One pass: recover, drain, persist every state change and id mapping.
pub fn drain_once(
    store: &Store,
    realm: &RealmId,
    client: &mut dyn QboClient,
    now: DateTime<Utc>,
) -> Result<WriteReport, WriteError> {
    require_writes(store, realm)?;
    let mut report = WriteReport::default();

    let mut records = store.outbox_records(
        realm,
        &[OutboxState::Pending, OutboxState::InFlight, OutboxState::Conflicted, OutboxState::Rejected],
    )?;
    for record in records.iter_mut().filter(|r| r.state == OutboxState::InFlight) {
        record.recover_in_flight(now)?;
        store.save_outbox_state(record)?;
        report.recovered += 1;
    }

    // Parents that already landed in an earlier pass are not in the open set;
    // load them so their dependants see `Applied` rather than "missing".
    let open: HashSet<Uuid> = records.iter().map(|r| r.id).collect();
    let parents: Vec<Uuid> = records
        .iter()
        .filter_map(|r| r.depends_on)
        .filter(|p| !open.contains(p))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for parent in parents {
        if let Some(record) = store.outbox_record(parent)? {
            records.push(record);
        }
    }

    let mut drainer = Drainer::with_mappings(store.id_map(realm)?);
    let mut persist = |record: &OutboxRecord| store.save_outbox_state(record).map_err(|e| e.to_string());
    report.drain = drainer.drain_with(&mut records, client, now, &mut persist);

    for record in records.iter().filter(|r| r.state == OutboxState::Applied && r.operation == Operation::Create) {
        if let Some(qbo_id) = drainer.resolved_id(realm, &record.local_entity_id) {
            store.record_id_mapping(realm, &record.local_entity_id, record.entity_type, qbo_id, now)?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::client::{MockQbo, QboError};
    use crate::store::MirroredEntity;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap()
    }

    fn store(writes: bool) -> Store {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test", now()).unwrap();
        store.set_write_enabled(&realm(), writes).unwrap();
        store
    }

    #[test]
    fn nothing_queues_or_drains_while_writes_are_off() {
        let store = store(false);
        assert!(matches!(
            queue_create(&store, &realm(), EntityType::Invoice, json!({}), now()),
            Err(WriteError::WritesOff)
        ));
        assert!(matches!(drain_once(&store, &realm(), &mut MockQbo::new(now()), now()), Err(WriteError::WritesOff)));
    }

    #[test]
    fn a_new_customer_and_its_invoice_land_in_order_across_a_restart() {
        let store = store(true);
        let customer = queue_create(&store, &realm(), EntityType::Customer, json!({ "DisplayName": "HARBOR SWIM CLUB" }), now()).unwrap();
        let invoice = queue_create(
            &store,
            &realm(),
            EntityType::Invoice,
            json!({ "CustomerRef": { "value": customer.local_entity_id }, "Line": [] }),
            now(),
        )
        .unwrap();
        assert_eq!(invoice.depends_on, Some(customer.id), "the invoice waits for its customer");

        // The customer fails once; the invoice must not go out first.
        let mut qbo = MockQbo::new(now());
        qbo.fail_next(QboError::Network("reset".into()));
        let first = drain_once(&store, &realm(), &mut qbo, now()).unwrap();
        assert_eq!(first.drain.applied, 0);
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 0);

        // A new process: fresh drainer, mappings and states from the store.
        let second = drain_once(&store, &realm(), &mut qbo, now()).unwrap();
        assert_eq!(second.drain.applied, 2);
        let customer_id = store.resolve_local_id(&realm(), &customer.local_entity_id).unwrap().unwrap();
        let invoices: Vec<_> = qbo.query(&realm(), EntityType::Invoice, None, 0, 10).unwrap();
        assert_eq!(invoices[0].raw_json["CustomerRef"]["value"], customer_id.as_str(), "local id rewritten");
        assert!(store.outbox_records(&realm(), &[OutboxState::Pending]).unwrap().is_empty());
    }

    #[test]
    fn a_crash_left_in_flight_resends_with_the_same_request_id() {
        let store = store(true);
        let mut record = queue_create(&store, &realm(), EntityType::Invoice, json!({ "DocNumber": "T-9" }), now()).unwrap();
        let mut qbo = MockQbo::new(now());
        // The request reached Intuit; the process died before recording it.
        qbo.create(&realm(), EntityType::Invoice, &record.payload_json, record.request_id).unwrap();
        record.begin_attempt(true, now()).unwrap();
        store.save_outbox_state(&record).unwrap();

        let report = drain_once(&store, &realm(), &mut qbo, now()).unwrap();
        assert_eq!(report.recovered, 1);
        assert_eq!(report.drain.applied, 1);
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 1, "replayed, not duplicated");
    }

    #[test]
    fn a_conflict_waits_for_a_person_and_retries_on_the_new_version() {
        let store = store(true);
        let entity = MirroredEntity {
            entity_type: EntityType::Invoice,
            qbo_id: "55".into(),
            sync_token: "0".into(),
            last_updated_utc: now(),
            is_deleted: false,
            raw_json: json!({ "Id": "55", "SyncToken": "0" }),
        };
        store.apply_batch(&realm(), std::slice::from_ref(&entity), None, now()).unwrap();
        let mut qbo = MockQbo::new(now());
        qbo.seed(&realm(), crate::client::EntityPayload {
            entity_type: EntityType::Invoice, qbo_id: "55".into(), sync_token: "0".into(),
            last_updated_utc: now(), is_deleted: false, raw_json: json!({ "Id": "55" }),
        });
        let record = queue_update(&store, &realm(), EntityType::Invoice, "55", json!({ "PrivateNote": "rush", "sparse": true }), now()).unwrap();
        qbo.bump_sync_token(&realm(), EntityType::Invoice, "55");

        assert_eq!(drain_once(&store, &realm(), &mut qbo, now()).unwrap().drain.conflicted, 1);
        // Retrying before the replica has the other edit is refused.
        assert!(matches!(resolve(&store, &realm(), record.id, Resolution::Retry, now()), Err(WriteError::Invalid(_))));

        let mut synced = entity.clone();
        synced.sync_token = "1".into();
        store.apply_batch(&realm(), &[synced], None, now()).unwrap();
        let retried = resolve(&store, &realm(), record.id, Resolution::Retry, now()).unwrap();
        assert_eq!(retried.base_sync_token.as_deref(), Some("1"));
        assert_eq!(drain_once(&store, &realm(), &mut qbo, now()).unwrap().drain.applied, 1);
    }

    #[test]
    fn discarding_keeps_the_record() {
        let store = store(true);
        let record = queue_create(&store, &realm(), EntityType::Invoice, json!({}), now()).unwrap();
        let mut qbo = MockQbo::new(now());
        qbo.fail_next(QboError::Validation("customer required".into()));
        drain_once(&store, &realm(), &mut qbo, now()).unwrap();
        // Pending cannot be discarded; only what failed.
        let discarded = resolve(&store, &realm(), record.id, Resolution::Discard, now()).unwrap();
        assert_eq!(discarded.state, OutboxState::Discarded);
        assert!(store.outbox_record(record.id).unwrap().is_some());
        assert_eq!(drain_once(&store, &realm(), &mut qbo, now()).unwrap().drain, DrainReport::default());
    }

    #[test]
    fn production_writes_wait_out_the_soak_and_need_the_go() {
        use crate::store::outbox_store::Environment;
        use crate::sync::SyncCursor;
        let store = store(false);
        assert!(check_enable(&store, &realm(), true, now()).unwrap_err().contains("never completed"));

        let mut cursor = SyncCursor::new(realm(), EntityType::Invoice);
        cursor.record_full_sweep(now());
        store.save_cursor(&cursor).unwrap();
        let day_ten = now() + chrono::Duration::days(10);
        assert!(check_enable(&store, &realm(), true, day_ten).unwrap_err().contains("4 to go"));
        let day_fifteen = now() + chrono::Duration::days(15);
        assert!(check_enable(&store, &realm(), false, day_fifteen).unwrap_err().contains("--confirm"));
        assert!(check_enable(&store, &realm(), true, day_fifteen).is_ok());

        // A second production company waits for the first to be switched off.
        let other = RealmId::parse("1234567890123457").unwrap();
        store.register_realm(&other, "Second", now()).unwrap();
        let mut other_cursor = SyncCursor::new(other.clone(), EntityType::Invoice);
        other_cursor.record_full_sweep(now());
        store.save_cursor(&other_cursor).unwrap();
        store.set_write_enabled(&realm(), true).unwrap();
        assert!(check_enable(&store, &other, true, day_fifteen).unwrap_err().contains("one production company"));

        // A sandbox company needs none of it.
        store.set_realm_environment(&other, Environment::Sandbox).unwrap();
        assert!(check_enable(&store, &other, false, now()).is_ok());
    }

    #[test]
    fn a_reference_to_nothing_is_refused_at_queue_time() {
        let store = store(true);
        let result = queue_create(&store, &realm(), EntityType::Invoice, json!({ "CustomerRef": { "value": "local:ghost" } }), now());
        assert!(matches!(result, Err(WriteError::Invalid(_))));
    }
}
