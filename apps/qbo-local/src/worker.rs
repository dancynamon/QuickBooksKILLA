//! The outbox drain worker. `DESIGN.md` §6.3–6.5.
//!
//! Takes eligible outbox records and pushes them to QBO, in order per entity and
//! in parallel across entities, rewriting local ids to real ones as parents are
//! applied. Every failure lands in a state the UI can show; nothing is dropped
//! and nothing is auto-merged.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::client::{unique_name_of, QboClient, QboError};
use crate::domain::{EntityType, RealmId};
use crate::outbox::{DrainFailure, Operation, OutboxRecord, OutboxState};

/// Prefix marking an id that exists only locally until QBO assigns a real one.
pub const LOCAL_ID_PREFIX: &str = "local:";

/// See [`Drainer::set_on_transition`].
type TransitionHook = Box<dyn FnMut(&OutboxRecord, Option<&str>)>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub applied: usize,
    /// Applied by adopting an existing QBO record rather than creating one —
    /// the duplicate that did not happen.
    pub adopted: usize,
    pub conflicted: usize,
    pub rejected: usize,
    /// Returned to the queue after a transient failure.
    pub retried: usize,
    /// Held back: an unapplied dependency, an unresolved local reference, or an
    /// earlier record for the same entity that has not landed.
    pub blocked: usize,
}

/// Rewrite every `local:` reference in a payload using the id map.
///
/// Returns the rewritten payload and whether every reference resolved. An
/// unresolved reference means the parent has not been applied, so the record is
/// not safe to send — sending it would create a document pointing at a customer
/// QBO has never heard of.
fn rewrite_local_ids(
    value: &serde_json::Value,
    realm: &RealmId,
    id_map: &HashMap<(String, String), String>,
) -> (serde_json::Value, bool) {
    match value {
        serde_json::Value::String(text) if text.starts_with(LOCAL_ID_PREFIX) => {
            match id_map.get(&(realm.as_str().to_string(), text.clone())) {
                Some(qbo_id) => (serde_json::Value::String(qbo_id.clone()), true),
                None => (value.clone(), false),
            }
        }
        serde_json::Value::Array(items) => {
            let mut resolved = true;
            let rewritten = items
                .iter()
                .map(|item| {
                    let (value, ok) = rewrite_local_ids(item, realm, id_map);
                    resolved &= ok;
                    value
                })
                .collect();
            (serde_json::Value::Array(rewritten), resolved)
        }
        serde_json::Value::Object(fields) => {
            let mut resolved = true;
            let rewritten = fields
                .iter()
                .map(|(key, item)| {
                    let (value, ok) = rewrite_local_ids(item, realm, id_map);
                    resolved &= ok;
                    (key.clone(), value)
                })
                .collect();
            (serde_json::Value::Object(rewritten), resolved)
        }
        _ => (value.clone(), true),
    }
}

/// Drains outbox records and remembers which local ids QBO has assigned.
#[derive(Default)]
pub struct Drainer {
    /// `(realm, local_entity_id) -> qbo_id`
    id_map: HashMap<(String, String), String>,
    /// Client calls that reached `Applied` in this `Drainer`'s lifetime — the
    /// counter `set_crash_hook` compares against `--kill-after`.
    applied_calls: usize,
    /// Fired after every persisted-state change during a drain pass —
    /// `begin_attempt` (before the client call, so a caller backed by a
    /// [`crate::store::Store`] can commit `in_flight` to disk before the
    /// request is issued, per `DESIGN.md` §6.1), then `mark_applied` or
    /// `mark_failed` (after it). `worker` itself has no `Store` dependency;
    /// this hook is how a durable caller gets one without `worker` growing
    /// one.
    on_transition: Option<TransitionHook>,
    /// Test-only. See [`Self::set_crash_hook`].
    crash_hook: Option<Box<dyn FnMut(usize)>>,
}

impl Drainer {
    pub fn new() -> Self {
        Drainer::default()
    }

    pub fn resolved_id(&self, realm: &RealmId, local_entity_id: &str) -> Option<&String> {
        self.id_map
            .get(&(realm.as_str().to_string(), local_entity_id.to_string()))
    }

    fn record_mapping(&mut self, realm: &RealmId, local_entity_id: &str, qbo_id: &str) {
        self.id_map.insert(
            (realm.as_str().to_string(), local_entity_id.to_string()),
            qbo_id.to_string(),
        );
    }

    /// Seed a mapping this `Drainer` did not itself resolve — reloaded, for
    /// instance, from `store::Store`'s `local_id_map` table after a process
    /// restart. Without this, a fresh `Drainer` has no memory of ids QBO
    /// assigned in an earlier run, and cannot rewrite a still-pending
    /// dependant's `local:` reference to one.
    pub fn seed_mapping(&mut self, realm: &RealmId, local_entity_id: &str, qbo_id: &str) {
        self.record_mapping(realm, local_entity_id, qbo_id);
    }

    /// Persistence seam: called with the record's state right after it
    /// changes, twice per attempt — once for `begin_attempt`, before the
    /// client call, and once for the `mark_applied`/`mark_failed` that
    /// follows it. The second argument is the id QBO assigned, when this call
    /// is the one that just applied the record; `None` otherwise.
    pub fn set_on_transition(&mut self, hook: impl FnMut(&OutboxRecord, Option<&str>) + 'static) {
        self.on_transition = Some(Box::new(hook));
    }

    /// Test-only: the crash window `DESIGN.md` §6.1 requires the `in_flight`
    /// recovery path to survive — after a remote call has succeeded and
    /// before the resulting `mark_applied` commit. `hook` receives the count
    /// of calls that have reached that point so far in this `Drainer`'s
    /// lifetime; `tests/chaos.rs` uses it to call `std::process::abort()` on
    /// the Kth one.
    #[doc(hidden)]
    pub fn set_crash_hook(&mut self, hook: impl FnMut(usize) + 'static) {
        self.crash_hook = Some(Box::new(hook));
    }

    /// One drain pass over the queue.
    ///
    /// Records are processed in UUIDv7 order, which is creation order, so an
    /// update can never overtake its own create. A record that does not reach
    /// `Applied` blocks every later record for the same entity in this pass —
    /// letting the next one through would reorder writes against that entity.
    pub fn drain(
        &mut self,
        records: &mut [OutboxRecord],
        client: &mut dyn QboClient,
        now: DateTime<Utc>,
    ) -> DrainReport {
        let mut report = DrainReport::default();

        let mut order: Vec<usize> = (0..records.len()).collect();
        order.sort_by_key(|&index| records[index].id);

        // Live view of every record's state, so a dependency applied earlier in
        // this same pass unblocks its dependants immediately.
        let mut states: HashMap<uuid::Uuid, OutboxState> =
            records.iter().map(|r| (r.id, r.state)).collect();

        // Entities with an unlanded write earlier in this pass.
        let mut blocked_entities: HashSet<(String, EntityType, String)> = HashSet::new();

        for index in order {
            let (id, realm, entity_type, local_entity_id, depends_on, state) = {
                let record = &records[index];
                (
                    record.id,
                    record.realm_id.clone(),
                    record.entity_type,
                    record.local_entity_id.clone(),
                    record.depends_on,
                    record.state,
                )
            };
            let entity_key = (
                realm.as_str().to_string(),
                entity_type,
                local_entity_id.clone(),
            );

            if !state.is_drainable() {
                if !state.is_terminal() {
                    blocked_entities.insert(entity_key);
                }
                continue;
            }

            let dependency_applied = match depends_on {
                None => true,
                Some(parent) => states.get(&parent) == Some(&OutboxState::Applied),
            };
            if !dependency_applied || blocked_entities.contains(&entity_key) {
                report.blocked += 1;
                blocked_entities.insert(entity_key);
                continue;
            }

            let (payload, references_resolved) =
                rewrite_local_ids(&records[index].payload_json, &realm, &self.id_map);
            if !references_resolved {
                report.blocked += 1;
                blocked_entities.insert(entity_key);
                continue;
            }

            // Persisted before the request is issued, so a crash mid-flight
            // restarts to find the record here rather than in `pending`.
            if records[index].begin_attempt(true, now).is_err() {
                report.blocked += 1;
                blocked_entities.insert(entity_key);
                continue;
            }
            if let Some(hook) = self.on_transition.as_mut() {
                hook(&records[index], None);
            }

            let outcome = self.issue(client, &records[index], &payload, &realm, now);

            match outcome {
                Ok(Applied { qbo_id, adopted }) => {
                    self.applied_calls += 1;
                    if let Some(hook) = self.crash_hook.as_mut() {
                        hook(self.applied_calls);
                    }
                    self.record_mapping(&realm, &local_entity_id, &qbo_id);
                    let _ = records[index].mark_applied(now);
                    if let Some(hook) = self.on_transition.as_mut() {
                        hook(&records[index], Some(qbo_id.as_str()));
                    }
                    states.insert(id, OutboxState::Applied);
                    report.applied += 1;
                    if adopted {
                        report.adopted += 1;
                    }
                }
                Err(error) => {
                    let failure = match &error {
                        QboError::StaleSyncToken => DrainFailure::StaleSyncToken,
                        other if other.is_transient() => DrainFailure::Transient(other.to_string()),
                        other => DrainFailure::Rejected(other.to_string()),
                    };
                    let _ = records[index].mark_failed(failure, now);
                    if let Some(hook) = self.on_transition.as_mut() {
                        hook(&records[index], None);
                    }
                    let new_state = records[index].state;
                    states.insert(id, new_state);
                    match new_state {
                        OutboxState::Conflicted => report.conflicted += 1,
                        OutboxState::Rejected => report.rejected += 1,
                        _ => report.retried += 1,
                    }
                    blocked_entities.insert((
                        realm.as_str().to_string(),
                        entity_type,
                        local_entity_id,
                    ));
                }
            }
        }

        report
    }

    fn issue(
        &self,
        client: &mut dyn QboClient,
        record: &OutboxRecord,
        payload: &serde_json::Value,
        realm: &RealmId,
        _now: DateTime<Utc>,
    ) -> Result<Applied, QboError> {
        match record.operation {
            Operation::Create => {
                // Customer and Item creates are not deduplicated by RequestId
                // (DESIGN.md §6.5), so look for an existing record first and
                // adopt it rather than risking a duplicate master.
                if record.needs_query_before_create() {
                    if let Some(name) = unique_name_of(record.entity_type, payload) {
                        if let Some(existing) =
                            client.find_by_name(realm, record.entity_type, &name)?
                        {
                            return Ok(Applied {
                                qbo_id: existing.qbo_id,
                                adopted: true,
                            });
                        }
                    }
                }
                let created =
                    client.create(realm, record.entity_type, payload, record.request_id)?;
                Ok(Applied {
                    qbo_id: created.qbo_id,
                    adopted: false,
                })
            }
            Operation::Update => {
                let qbo_id = self
                    .resolved_id(realm, &record.local_entity_id)
                    .cloned()
                    .unwrap_or_else(|| record.local_entity_id.clone());
                let updated = client.update(
                    realm,
                    record.entity_type,
                    &qbo_id,
                    payload,
                    record.base_sync_token.as_deref().unwrap_or(""),
                    record.request_id,
                )?;
                Ok(Applied {
                    qbo_id: updated.qbo_id,
                    adopted: false,
                })
            }
        }
    }
}

struct Applied {
    qbo_id: String,
    adopted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockQbo;
    use crate::outbox::{NewOutboxRecord, MAX_ATTEMPTS};
    use proptest::prelude::*;
    use uuid::Uuid;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + secs, 0).unwrap()
    }

    fn record(
        entity_type: EntityType,
        operation: Operation,
        local_entity_id: &str,
        payload: serde_json::Value,
    ) -> OutboxRecord {
        OutboxRecord::new(
            NewOutboxRecord {
                id: Uuid::now_v7(),
                request_id: Uuid::now_v7(),
                realm_id: realm(),
                entity_type,
                operation,
                payload_json: payload,
                local_entity_id: local_entity_id.to_string(),
                base_sync_token: if operation == Operation::Update {
                    Some("0".to_string())
                } else {
                    None
                },
            },
            at(0),
        )
    }

    #[test]
    fn a_create_applies_and_records_its_assigned_id() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "21234" }),
        )];

        let report = drainer.drain(&mut records, &mut qbo, at(0));
        assert_eq!(report.applied, 1);
        assert_eq!(records[0].state, OutboxState::Applied);
        assert!(drainer.resolved_id(&realm(), "local:inv-1").is_some());
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 1);
    }

    #[test]
    fn a_dependent_invoice_waits_then_gets_its_reference_rewritten() {
        // The canonical case: create a customer, immediately invoice them.
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();

        let customer = record(
            EntityType::Customer,
            Operation::Create,
            "local:cust-1",
            serde_json::json!({ "DisplayName": "BLUE HARBOR SWIM" }),
        );
        let invoice = record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({
                "CustomerRef": { "value": "local:cust-1" },
                "DocNumber": "21234"
            }),
        )
        .depending_on(customer.id);

        let mut records = vec![customer, invoice];
        let report = drainer.drain(&mut records, &mut qbo, at(0));

        // Both land in one pass: the customer applies first, which unblocks the
        // invoice immediately rather than waiting for the next tick.
        assert_eq!(report.applied, 2);
        assert_eq!(report.blocked, 0);

        let customer_id = drainer
            .resolved_id(&realm(), "local:cust-1")
            .unwrap()
            .clone();
        let invoice_id = drainer.resolved_id(&realm(), "local:inv-1").unwrap();
        let stored = qbo.get(&realm(), EntityType::Invoice, invoice_id).unwrap();
        assert_eq!(stored.raw_json["CustomerRef"]["value"], customer_id);
        assert!(!stored.raw_json["CustomerRef"]["value"]
            .as_str()
            .unwrap()
            .starts_with(LOCAL_ID_PREFIX));
    }

    #[test]
    fn a_dependant_is_held_when_its_parent_fails() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();

        let customer = record(
            EntityType::Customer,
            Operation::Create,
            "local:cust-1",
            serde_json::json!({ "DisplayName": "ACME" }),
        );
        let invoice = record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "CustomerRef": { "value": "local:cust-1" } }),
        )
        .depending_on(customer.id);

        // The customer's find_by_name probe fails, so the customer never lands.
        qbo.fail_next(QboError::Network("connection reset".into()));

        let mut records = vec![customer, invoice];
        let report = drainer.drain(&mut records, &mut qbo, at(0));

        assert_eq!(report.retried, 1);
        assert_eq!(report.blocked, 1);
        assert_eq!(
            records[1].state,
            OutboxState::Pending,
            "dependant must not fail on its own"
        );
        assert_eq!(
            records[1].attempts, 0,
            "a blocked dependant burns no retry budget"
        );
        assert_eq!(
            qbo.count(&realm(), EntityType::Invoice),
            0,
            "no orphaned invoice"
        );
    }

    #[test]
    fn a_retried_customer_create_adopts_rather_than_duplicating() {
        // The failure this design exists to prevent. The create reaches QBO and
        // succeeds, but the response is lost, so the record retries. RequestId
        // does not deduplicate customers, so without query-before-create this
        // produces a second customer master.
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Customer,
            Operation::Create,
            "local:cust-1",
            serde_json::json!({ "DisplayName": "FAIRVIEW COUNTY PARKS & REC." }),
        )];

        // First pass: the create lands in QBO.
        drainer.drain(&mut records, &mut qbo, at(0));
        assert_eq!(records[0].state, OutboxState::Applied);

        // Now model the response being lost on the way back: QBO holds the
        // customer, but the outbox never learned that, so the record re-queues.
        records[0].state = OutboxState::Pending;
        records[0].attempts = 0;

        // Second pass: the same record drains again.
        let report = drainer.drain(&mut records, &mut qbo, at(1));

        assert_eq!(report.applied, 1);
        assert_eq!(
            report.adopted, 1,
            "should have adopted the existing customer"
        );
        assert_eq!(
            qbo.count(&realm(), EntityType::Customer),
            1,
            "a duplicate customer master was created"
        );
    }

    #[test]
    fn a_retried_invoice_create_relies_on_request_id() {
        // Invoices are covered by RequestId, so a replay returns the original
        // rather than creating a second document.
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "21234" }),
        )];

        drainer.drain(&mut records, &mut qbo, at(0));
        records[0].state = OutboxState::Pending;
        records[0].attempts = 0;
        drainer.drain(&mut records, &mut qbo, at(1));

        assert_eq!(
            qbo.count(&realm(), EntityType::Invoice),
            1,
            "duplicate invoice"
        );
        assert_eq!(records[0].state, OutboxState::Applied);
    }

    #[test]
    fn a_stale_token_conflicts_and_stays_out_of_the_queue() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();

        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "1" }),
        )];
        drainer.drain(&mut records, &mut qbo, at(0));
        let qbo_id = drainer
            .resolved_id(&realm(), "local:inv-1")
            .unwrap()
            .clone();

        // Someone edits the invoice in QBO's web UI.
        qbo.bump_sync_token(&realm(), EntityType::Invoice, &qbo_id);

        let mut updates = vec![record(
            EntityType::Invoice,
            Operation::Update,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "1", "TotalAmt": 99.0 }),
        )];
        let report = drainer.drain(&mut updates, &mut qbo, at(1));

        assert_eq!(report.conflicted, 1);
        assert_eq!(updates[0].state, OutboxState::Conflicted);
        assert!(updates[0].state.needs_attention());
        // The edit made in QBO survived.
        assert_ne!(
            qbo.get(&realm(), EntityType::Invoice, &qbo_id)
                .unwrap()
                .raw_json["TotalAmt"],
            99.0
        );
    }

    #[test]
    fn an_update_never_overtakes_its_own_create() {
        // Ordering per entity. The update is queued second and references the
        // same local id, so it must not be attempted before the create lands.
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();

        let create = record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "1" }),
        );
        let update = record(
            EntityType::Invoice,
            Operation::Update,
            "local:inv-1",
            serde_json::json!({ "DocNumber": "1", "TotalAmt": 74.24 }),
        );
        // Queue them out of order in the vector; drain order comes from the id.
        let mut records = vec![update, create];
        let report = drainer.drain(&mut records, &mut qbo, at(0));

        assert_eq!(report.applied, 2);
        let qbo_id = drainer.resolved_id(&realm(), "local:inv-1").unwrap();
        assert_eq!(
            qbo.get(&realm(), EntityType::Invoice, qbo_id)
                .unwrap()
                .raw_json["TotalAmt"],
            74.24
        );
    }

    #[test]
    fn a_transient_failure_is_retried_and_eventually_succeeds() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({}),
        )];

        qbo.fail_next(QboError::RateLimited);
        let first = drainer.drain(&mut records, &mut qbo, at(0));
        assert_eq!(first.retried, 1);
        assert_eq!(records[0].state, OutboxState::Pending);

        let second = drainer.drain(&mut records, &mut qbo, at(1));
        assert_eq!(second.applied, 1);
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 1);
    }

    #[test]
    fn persistent_transient_failures_end_in_the_failure_inbox() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({}),
        )];

        for pass in 0..MAX_ATTEMPTS {
            qbo.fail_next(QboError::Network("reset".into()));
            drainer.drain(&mut records, &mut qbo, at(pass as i64));
        }
        assert_eq!(records[0].state, OutboxState::Rejected);
        assert!(records[0].state.needs_attention());
    }

    #[test]
    fn a_validation_failure_is_rejected_not_retried() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({}),
        )];

        qbo.fail_next(QboError::Validation("account required".into()));
        let report = drainer.drain(&mut records, &mut qbo, at(0));

        assert_eq!(report.rejected, 1);
        assert_eq!(report.retried, 0);
        assert_eq!(records[0].state, OutboxState::Rejected);
    }

    #[test]
    fn an_unresolved_local_reference_blocks_rather_than_sending() {
        // No dependency recorded, but the payload still points at a local id.
        // Sending would create an invoice referencing a customer QBO has never
        // heard of.
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "CustomerRef": { "value": "local:missing" } }),
        )];

        let report = drainer.drain(&mut records, &mut qbo, at(0));
        assert_eq!(report.blocked, 1);
        assert_eq!(report.applied, 0);
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 0);
        assert_eq!(records[0].attempts, 0);
    }

    #[test]
    fn rewriting_leaves_ordinary_values_alone() {
        let mut id_map = HashMap::new();
        id_map.insert(
            ("1234567890123456".to_string(), "local:cust-1".to_string()),
            "42".to_string(),
        );
        let payload = serde_json::json!({
            "CustomerRef": { "value": "local:cust-1" },
            "DocNumber": "21234",
            "TotalAmt": 74.24,
            "Line": [{ "Description": "not a local: reference in prose" }],
            "Nested": [{ "Ref": "local:cust-1" }]
        });
        let (rewritten, resolved) = rewrite_local_ids(&payload, &realm(), &id_map);

        assert!(resolved);
        assert_eq!(rewritten["CustomerRef"]["value"], "42");
        assert_eq!(rewritten["Nested"][0]["Ref"], "42");
        assert_eq!(rewritten["DocNumber"], "21234");
        assert_eq!(rewritten["TotalAmt"], 74.24);
        // A string merely containing "local:" mid-sentence is not a reference.
        assert_eq!(
            rewritten["Line"][0]["Description"],
            "not a local: reference in prose"
        );
    }

    proptest! {
        /// Convergence: any sequence of local creates, drained repeatedly until
        /// the queue settles, produces exactly one QBO record per local entity —
        /// no duplicates, nothing lost — even with transient failures injected
        /// throughout, and even if a fresh `Drainer` — the in-process stand-in
        /// for a restarted process, `DESIGN.md` §6.1 — takes over partway
        /// through with no memory but what a `Store` would have persisted:
        /// resolved id mappings, and any `in_flight` record recovered rather
        /// than resent blind (`tests/chaos.rs` exercises the same recovery
        /// path across a real process boundary).
        #[test]
        fn draining_converges_without_duplicates(
            count in 1usize..12,
            failure_points in prop::collection::vec(any::<bool>(), 0..24),
            restart_at in 0usize..8,
        ) {
            let mut qbo = MockQbo::new(at(0));
            let mut drainer = Drainer::new();

            let mut records: Vec<OutboxRecord> = (0..count)
                .map(|index| {
                    record(
                        EntityType::Invoice,
                        Operation::Create,
                        &format!("local:inv-{index}"),
                        serde_json::json!({ "DocNumber": index.to_string() }),
                    )
                })
                .collect();

            let mut failures = failure_points.into_iter();
            let mut restarted = false;
            for pass in 0..(MAX_ATTEMPTS as usize + count) {
                if records.iter().all(|r| r.state.is_terminal() || r.state.needs_attention()) {
                    break;
                }
                if failures.next().unwrap_or(false) {
                    qbo.fail_next(QboError::Network("induced".into()));
                }

                // Once, at a proptest-chosen pass, simulate a process restart:
                // a brand-new `Drainer` takes over with none of the in-memory
                // state the old one built up, reseeded only with what a
                // `Store`-backed caller would actually have persisted — every
                // mapping already resolved (`local_id_map`, §6.4) — same as
                // `tests/chaos.rs` reloading a fresh `Drainer` after a real
                // process kill. A record `drain` had carried to `in_flight`
                // and no further within a single pass is impossible — a pass
                // is synchronous and always resolves what it starts — so the
                // property this adds is that draining tolerates losing and
                // rebuilding the `Drainer` itself mid-sequence, not merely
                // running start to finish inside one long-lived instance.
                if !restarted && pass == restart_at {
                    restarted = true;
                    let mut fresh = Drainer::new();
                    for outbox_record in &records {
                        if outbox_record.state == OutboxState::Applied {
                            if let Some(qbo_id) = drainer.resolved_id(&realm(), &outbox_record.local_entity_id) {
                                fresh.seed_mapping(&realm(), &outbox_record.local_entity_id, qbo_id);
                            }
                        }
                    }
                    drainer = fresh;
                }

                drainer.drain(&mut records, &mut qbo, at(pass as i64));
            }

            // Every record that reached Applied has exactly one QBO record, and
            // nothing was created twice.
            let applied = records.iter().filter(|r| r.state == OutboxState::Applied).count();
            prop_assert_eq!(qbo.count(&realm(), EntityType::Invoice), applied);

            // Nothing is silently stuck: every record ended terminal or visible.
            for outbox_record in &records {
                prop_assert!(
                    outbox_record.state == OutboxState::Applied
                        || outbox_record.state.needs_attention(),
                    "record left in {:?}", outbox_record.state
                );
            }
        }
    }

    #[test]
    fn the_crash_hook_fires_after_the_kth_successful_call_and_before_its_commit() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![
            record(
                EntityType::Invoice,
                Operation::Create,
                "local:inv-1",
                serde_json::json!({}),
            ),
            record(
                EntityType::Invoice,
                Operation::Create,
                "local:inv-2",
                serde_json::json!({}),
            ),
        ];

        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen_in_hook = seen.clone();
        drainer.set_crash_hook(move |count| seen_in_hook.borrow_mut().push(count));

        drainer.drain(&mut records, &mut qbo, at(0));

        // Fired once per successful call, in order, before either record's
        // `mark_applied` had a chance to run — both still ended up Applied,
        // which only proves the hook does not itself interfere.
        assert_eq!(*seen.borrow(), vec![1, 2]);
        assert!(records.iter().all(|r| r.state == OutboxState::Applied));
    }

    #[test]
    fn the_transition_hook_sees_in_flight_before_the_call_and_the_qbo_id_after() {
        let mut qbo = MockQbo::new(at(0));
        let mut drainer = Drainer::new();
        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({}),
        )];

        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen_in_hook = seen.clone();
        drainer.set_on_transition(move |outbox_record, qbo_id| {
            seen_in_hook
                .borrow_mut()
                .push((outbox_record.state, qbo_id.map(str::to_string)));
        });

        drainer.drain(&mut records, &mut qbo, at(0));

        assert_eq!(
            *seen.borrow(),
            vec![
                (OutboxState::InFlight, None),
                (OutboxState::Applied, Some("1".to_string())),
            ]
        );
    }

    #[test]
    fn seeded_mappings_rewrite_a_dependants_reference_without_ever_drawing_the_parent() {
        // What a restarted process relies on: the parent already applied in an
        // earlier run, so this `Drainer` never drains it — only a `Store`
        // reload of `local_id_map` told it the mapping.
        let mut qbo = MockQbo::new(at(0));
        qbo.create(
            &realm(),
            EntityType::Customer,
            &serde_json::json!({ "DisplayName": "BLUE HARBOR SWIM" }),
            Uuid::now_v7(),
        )
        .unwrap();
        let qbo_id = qbo
            .find_by_name(&realm(), EntityType::Customer, "BLUE HARBOR SWIM")
            .unwrap()
            .unwrap()
            .qbo_id;

        let mut drainer = Drainer::new();
        drainer.seed_mapping(&realm(), "local:cust-1", &qbo_id);

        let mut records = vec![record(
            EntityType::Invoice,
            Operation::Create,
            "local:inv-1",
            serde_json::json!({ "CustomerRef": { "value": "local:cust-1" } }),
        )];
        let report = drainer.drain(&mut records, &mut qbo, at(0));

        assert_eq!(report.applied, 1);
        assert_eq!(records[0].state, OutboxState::Applied);
        let stored = qbo
            .get(
                &realm(),
                EntityType::Invoice,
                drainer.resolved_id(&realm(), "local:inv-1").unwrap(),
            )
            .unwrap();
        assert_eq!(stored.raw_json["CustomerRef"]["value"], qbo_id);
    }
}
