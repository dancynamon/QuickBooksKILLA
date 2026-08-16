//! The outbox write queue. `DESIGN.md` §6.
//!
//! Every local mutation becomes a durable record here before the UI is told it
//! succeeded. This is the component where the project succeeds or fails: a bug
//! that loses a record loses a write QBO never saw, and a bug that replays one
//! double-bills a customer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{EntityType, RealmId};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OutboxError {
    #[error("illegal outbox transition: {from:?} -> {to:?}")]
    IllegalTransition { from: OutboxState, to: OutboxState },
    #[error("record {0} is blocked by an unapplied dependency")]
    DependencyNotSatisfied(Uuid),
}

/// ```text
///                     ┌──────────────────────────────────────┐
///                     │                                      │
///                     v                                      │
///   [ pending ] ──> [ in_flight ] ──> [ applied ]            │ retry
///        ^                │                                  │
///        │                ├──> [ conflicted ] ── resolve ────┤
///        │                ├──> [ rejected ]   ── resolve ────┘
///        │                │
///        └────────────────┘  transient failure
/// ```
///
/// Only `Applied` is terminal without human involvement. `Conflicted` and
/// `Rejected` are terminal *until resolved* — they surface in the failure inbox
/// and are never silently dropped or auto-merged.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OutboxState {
    Pending,
    InFlight,
    Applied,
    Conflicted,
    Rejected,
}

/// Why a drain attempt did not reach `Applied`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DrainFailure {
    /// Network error, 429, or 5xx. Retryable without human involvement.
    Transient(String),
    /// QBO rejected the write: stale `SyncToken`. Someone edited the record in
    /// the QBO web UI. Always surfaced, never resolved by last-writer-wins.
    StaleSyncToken,
    /// QBO refused the write outright — validation, permission, business rule.
    Rejected(String),
}

impl OutboxState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, OutboxState::Applied)
    }

    /// Whether this record belongs in the failure inbox (`DESIGN.md` §6.6).
    pub const fn needs_attention(self) -> bool {
        matches!(self, OutboxState::Conflicted | OutboxState::Rejected)
    }

    /// Eligible to be picked up by the drain worker.
    pub const fn is_drainable(self) -> bool {
        matches!(self, OutboxState::Pending)
    }

    pub const fn can_transition_to(self, next: OutboxState) -> bool {
        use OutboxState::*;
        matches!(
            (self, next),
            (Pending, InFlight)
                // Transient failure returns the record to the queue.
                | (InFlight, Pending)
                | (InFlight, Applied)
                | (InFlight, Conflicted)
                | (InFlight, Rejected)
                // Human resolution re-queues a failed record.
                | (Conflicted, Pending)
                | (Rejected, Pending)
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            OutboxState::Pending => "pending",
            OutboxState::InFlight => "in_flight",
            OutboxState::Applied => "applied",
            OutboxState::Conflicted => "conflicted",
            OutboxState::Rejected => "rejected",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Operation {
    Create,
    Update,
    // No delete or void in v1 — destructive changes happen in QBO's web UI
    // where the confirmation dialogs live (DESIGN.md §8).
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxRecord {
    /// UUIDv7 — sorts chronologically, which is what gives per-entity ordering
    /// without a separate sequence column.
    pub id: Uuid,
    pub realm_id: RealmId,
    pub entity_type: EntityType,
    pub operation: Operation,
    pub payload_json: serde_json::Value,
    /// Local identifier, `local:`-prefixed until QBO assigns a real one.
    pub local_entity_id: String,
    /// The `SyncToken` this update was based on. `None` for creates.
    pub base_sync_token: Option<String>,
    /// Generated once at creation and **reused on every retry**. A `RequestId`
    /// regenerated per attempt provides no idempotency at all, which is how a
    /// network timeout becomes a duplicate invoice.
    pub request_id: Uuid,
    pub state: OutboxState,
    pub attempts: u32,
    /// Outbox id of the record that must reach `Applied` first (§6.4).
    pub depends_on: Option<Uuid>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Attempts beyond this are treated as permanently rejected rather than retried
/// forever. A record stuck in a retry loop is invisible; one in the failure
/// inbox is not.
pub const MAX_ATTEMPTS: u32 = 8;

/// The caller-supplied fields of a new outbox record.
///
/// A struct rather than a nine-argument constructor: several of these are
/// `Uuid` or `Option<String>`, and positional arguments of the same type are
/// exactly how a `request_id` ends up in the `id` field.
pub struct NewOutboxRecord {
    pub id: Uuid,
    pub request_id: Uuid,
    pub realm_id: RealmId,
    pub entity_type: EntityType,
    pub operation: Operation,
    pub payload_json: serde_json::Value,
    pub local_entity_id: String,
    pub base_sync_token: Option<String>,
}

impl OutboxRecord {
    pub fn new(fields: NewOutboxRecord, now: DateTime<Utc>) -> Self {
        OutboxRecord {
            id: fields.id,
            realm_id: fields.realm_id,
            entity_type: fields.entity_type,
            operation: fields.operation,
            payload_json: fields.payload_json,
            local_entity_id: fields.local_entity_id,
            base_sync_token: fields.base_sync_token,
            request_id: fields.request_id,
            state: OutboxState::Pending,
            attempts: 0,
            depends_on: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn depending_on(mut self, parent: Uuid) -> Self {
        self.depends_on = Some(parent);
        self
    }

    /// Whether the drain worker may pick this record up.
    ///
    /// `dependency_applied` is whether [`Self::depends_on`] has reached
    /// `Applied`; a record with an unsatisfied dependency stays put and is shown
    /// in the failure inbox as blocked, not as an independent failure.
    pub fn is_eligible(&self, dependency_applied: bool) -> bool {
        self.state.is_drainable() && (self.depends_on.is_none() || dependency_applied)
    }

    fn transition(&mut self, next: OutboxState, now: DateTime<Utc>) -> Result<(), OutboxError> {
        if !self.state.can_transition_to(next) {
            return Err(OutboxError::IllegalTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.updated_at = now;
        Ok(())
    }

    /// Move to `in_flight`. Persisted **before** the HTTP request is issued, so
    /// a process killed mid-request restarts to find the record here and knows
    /// to query QBO for the `RequestId` rather than blindly resending.
    pub fn begin_attempt(
        &mut self,
        dependency_applied: bool,
        now: DateTime<Utc>,
    ) -> Result<(), OutboxError> {
        if self.depends_on.is_some() && !dependency_applied {
            return Err(OutboxError::DependencyNotSatisfied(self.id));
        }
        self.transition(OutboxState::InFlight, now)?;
        self.attempts += 1;
        Ok(())
    }

    pub fn mark_applied(&mut self, now: DateTime<Utc>) -> Result<(), OutboxError> {
        self.last_error = None;
        self.transition(OutboxState::Applied, now)
    }

    /// Resolve a failed attempt into the right next state.
    ///
    /// A transient failure returns to `pending` for retry until [`MAX_ATTEMPTS`],
    /// after which it is rejected so that it becomes visible rather than looping.
    /// A stale `SyncToken` always goes to `conflicted` and is never resolved by
    /// last-writer-wins — it means someone edited that record in QBO's web UI,
    /// and silently overwriting discards a real edit.
    pub fn mark_failed(
        &mut self,
        failure: DrainFailure,
        now: DateTime<Utc>,
    ) -> Result<(), OutboxError> {
        let next = match &failure {
            DrainFailure::Transient(_) if self.attempts < MAX_ATTEMPTS => OutboxState::Pending,
            DrainFailure::Transient(_) => OutboxState::Rejected,
            DrainFailure::StaleSyncToken => OutboxState::Conflicted,
            DrainFailure::Rejected(_) => OutboxState::Rejected,
        };
        self.last_error = Some(match failure {
            DrainFailure::Transient(message) => message,
            DrainFailure::StaleSyncToken => "stale SyncToken: record changed in QBO".to_string(),
            DrainFailure::Rejected(message) => message,
        });
        self.transition(next, now)
    }

    /// Re-queue a record after human resolution in the failure inbox.
    pub fn resolve(&mut self, now: DateTime<Utc>) -> Result<(), OutboxError> {
        self.attempts = 0;
        self.last_error = None;
        self.transition(OutboxState::Pending, now)
    }

    /// Whether a create of this record must query QBO for an existing match
    /// before issuing, because `RequestId` does not deduplicate it (§6.5).
    pub fn needs_query_before_create(&self) -> bool {
        self.operation == Operation::Create && self.entity_type.requires_query_before_create()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EntityType;
    use proptest::prelude::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000, 0).unwrap()
    }

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn record(entity_type: EntityType, operation: Operation) -> OutboxRecord {
        OutboxRecord::new(
            NewOutboxRecord {
                id: Uuid::now_v7(),
                request_id: Uuid::now_v7(),
                realm_id: realm(),
                entity_type,
                operation,
                payload_json: serde_json::json!({}),
                local_entity_id: "local:test".to_string(),
                base_sync_token: None,
            },
            now(),
        )
    }

    const ALL_STATES: [OutboxState; 5] = [
        OutboxState::Pending,
        OutboxState::InFlight,
        OutboxState::Applied,
        OutboxState::Conflicted,
        OutboxState::Rejected,
    ];

    #[test]
    fn a_new_record_is_pending_and_drainable() {
        let record = record(EntityType::Invoice, Operation::Create);
        assert_eq!(record.state, OutboxState::Pending);
        assert!(record.is_eligible(false));
        assert_eq!(record.attempts, 0);
    }

    #[test]
    fn the_happy_path_reaches_applied() {
        let mut record = record(EntityType::Invoice, Operation::Create);
        record.begin_attempt(true, now()).unwrap();
        assert_eq!(record.state, OutboxState::InFlight);
        assert_eq!(record.attempts, 1);
        record.mark_applied(now()).unwrap();
        assert_eq!(record.state, OutboxState::Applied);
        assert!(record.state.is_terminal());
    }

    #[test]
    fn applied_is_terminal_from_every_direction() {
        // The invariant that matters most: nothing re-queues an applied write,
        // because re-queuing one is a duplicate document in the book of record.
        for state in ALL_STATES {
            assert!(
                !OutboxState::Applied.can_transition_to(state),
                "applied should not transition to {state:?}"
            );
        }
    }

    #[test]
    fn a_transient_failure_returns_to_the_queue() {
        let mut record = record(EntityType::Invoice, Operation::Create);
        record.begin_attempt(true, now()).unwrap();
        record
            .mark_failed(DrainFailure::Transient("connection reset".into()), now())
            .unwrap();
        assert_eq!(record.state, OutboxState::Pending);
        assert!(record.is_eligible(true));
        assert_eq!(record.attempts, 1, "attempts survive a retry");
        assert!(record.last_error.is_some());
    }

    #[test]
    fn transient_failures_stop_retrying_and_become_visible() {
        let mut record = record(EntityType::Invoice, Operation::Create);
        for _ in 0..MAX_ATTEMPTS {
            record.begin_attempt(true, now()).unwrap();
            record
                .mark_failed(DrainFailure::Transient("timeout".into()), now())
                .unwrap();
        }
        // The last failure crossed MAX_ATTEMPTS, so it is in the failure inbox
        // rather than looping invisibly.
        assert_eq!(record.state, OutboxState::Rejected);
        assert!(record.state.needs_attention());
        assert!(!record.is_eligible(true));
    }

    #[test]
    fn a_stale_sync_token_always_conflicts_and_never_overwrites() {
        let mut record = record(EntityType::Invoice, Operation::Update);
        record.begin_attempt(true, now()).unwrap();
        record.mark_failed(DrainFailure::StaleSyncToken, now()).unwrap();
        assert_eq!(record.state, OutboxState::Conflicted);
        assert!(record.state.needs_attention());
        assert!(!record.is_eligible(true), "must not silently retry over the edit");
    }

    #[test]
    fn a_stale_token_conflicts_on_the_first_attempt_not_after_retries() {
        // Retrying a stale-token failure would be last-writer-wins by another
        // name, so it must not be subject to the transient retry budget.
        let mut record = record(EntityType::Invoice, Operation::Update);
        record.begin_attempt(true, now()).unwrap();
        record.mark_failed(DrainFailure::StaleSyncToken, now()).unwrap();
        assert_eq!(record.attempts, 1);
        assert_eq!(record.state, OutboxState::Conflicted);
    }

    #[test]
    fn resolution_requeues_and_clears_the_retry_budget() {
        let mut record = record(EntityType::Invoice, Operation::Update);
        record.begin_attempt(true, now()).unwrap();
        record.mark_failed(DrainFailure::StaleSyncToken, now()).unwrap();
        record.resolve(now()).unwrap();
        assert_eq!(record.state, OutboxState::Pending);
        assert_eq!(record.attempts, 0);
        assert!(record.last_error.is_none());
        assert!(record.is_eligible(true));
    }

    #[test]
    fn a_dependent_record_waits_for_its_parent() {
        // The canonical case: create a customer, immediately invoice them.
        let customer = record(EntityType::Customer, Operation::Create);
        let mut invoice =
            record(EntityType::Invoice, Operation::Create).depending_on(customer.id);

        assert!(!invoice.is_eligible(false), "must not drain before the customer");
        assert_eq!(
            invoice.begin_attempt(false, now()),
            Err(OutboxError::DependencyNotSatisfied(invoice.id))
        );
        assert_eq!(invoice.state, OutboxState::Pending);
        assert_eq!(invoice.attempts, 0, "a blocked record burns no retry budget");

        assert!(invoice.is_eligible(true));
        invoice.begin_attempt(true, now()).unwrap();
        assert_eq!(invoice.state, OutboxState::InFlight);
    }

    #[test]
    fn customer_and_item_creates_need_a_query_first() {
        // The RequestId idempotency gap, DESIGN.md §6.5.
        assert!(record(EntityType::Customer, Operation::Create).needs_query_before_create());
        assert!(record(EntityType::Item, Operation::Create).needs_query_before_create());
        assert!(!record(EntityType::Invoice, Operation::Create).needs_query_before_create());
        // An update carries a real QBO id already, so there is nothing to
        // duplicate.
        assert!(!record(EntityType::Customer, Operation::Update).needs_query_before_create());
    }

    #[test]
    fn illegal_transitions_are_refused() {
        let mut record = record(EntityType::Invoice, Operation::Create);
        // Cannot apply a record that was never in flight.
        assert!(matches!(
            record.mark_applied(now()),
            Err(OutboxError::IllegalTransition { .. })
        ));
        assert_eq!(record.state, OutboxState::Pending, "state unchanged on refusal");

        // Cannot begin an attempt on a record already in flight.
        record.begin_attempt(true, now()).unwrap();
        assert!(matches!(
            record.begin_attempt(true, now()),
            Err(OutboxError::IllegalTransition { .. })
        ));
        assert_eq!(record.attempts, 1, "a refused transition burns no attempt");
    }

    #[test]
    fn request_id_is_stable_across_retries() {
        // The whole point of RequestId: a retry after a timeout must present the
        // same id, or Intuit sees a fresh request and creates a second document.
        let mut record = record(EntityType::Invoice, Operation::Create);
        let request_id = record.request_id;
        for _ in 0..3 {
            record.begin_attempt(true, now()).unwrap();
            record
                .mark_failed(DrainFailure::Transient("timeout".into()), now())
                .unwrap();
            assert_eq!(record.request_id, request_id);
        }
    }

    #[test]
    fn uuid_v7_ids_order_records_chronologically() {
        // Per-entity ordering depends on this: an update must not overtake its
        // own create.
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        assert!(first < second);
    }

    proptest! {
        /// No sequence of legal transitions escapes `Applied`.
        #[test]
        fn applied_is_absorbing(steps: Vec<u8>) {
            let mut state = OutboxState::Applied;
            for step in steps {
                let next = ALL_STATES[step as usize % ALL_STATES.len()];
                if state.can_transition_to(next) {
                    state = next;
                }
            }
            prop_assert_eq!(state, OutboxState::Applied);
        }

        /// From any state, following only legal transitions never lands
        /// anywhere outside the declared state set, and a record that needs
        /// attention is never simultaneously drainable — otherwise a conflicted
        /// write could quietly retry over someone's edit.
        #[test]
        fn attention_and_drainability_are_exclusive(start: u8, steps: Vec<u8>) {
            let mut state = ALL_STATES[start as usize % ALL_STATES.len()];
            for step in steps {
                let next = ALL_STATES[step as usize % ALL_STATES.len()];
                if state.can_transition_to(next) {
                    state = next;
                }
                prop_assert!(!(state.needs_attention() && state.is_drainable()));
            }
        }
    }
}
