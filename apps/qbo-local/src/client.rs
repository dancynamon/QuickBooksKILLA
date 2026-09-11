//! The QBO API boundary, and an in-memory double for testing against it.
//!
//! The trait is the only place the rest of the app can reach Intuit, which is
//! what lets the whole suite run offline. [`MockQbo`] is not a stub that returns
//! canned success — it reproduces the three behaviours the outbox has to survive:
//! `SyncToken` conflicts, `RequestId` deduplication, and the fact that
//! deduplication reportedly does **not** cover Customer or Item.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{EntityType, RealmId};

/// One entity as QBO returns it.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityPayload {
    pub entity_type: EntityType,
    pub qbo_id: String,
    pub sync_token: String,
    pub last_updated_utc: DateTime<Utc>,
    pub is_deleted: bool,
    pub raw_json: serde_json::Value,
}

/// One row of the id + `MetaData.LastUpdatedTime` index the reconciliation
/// sweep reads (`DESIGN.md` §7 step 1) — everything [`EntityPayload`] carries
/// except the payload itself, which the sweep does not need until it decides
/// something must be healed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub qbo_id: String,
    pub last_updated_utc: DateTime<Utc>,
    pub is_deleted: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum QboError {
    /// The `SyncToken` sent does not match the current one: someone changed
    /// this record in the QBO web UI. The canonical conflict signal.
    #[error("stale SyncToken")]
    StaleSyncToken,
    #[error("validation failure: {0}")]
    Validation(String),
    #[error("rate limited")]
    RateLimited,
    #[error("network failure: {0}")]
    Network(String),
    #[error("not found")]
    NotFound,
}

impl QboError {
    /// Whether retrying unchanged could plausibly succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, QboError::RateLimited | QboError::Network(_))
    }
}

/// A half-open `[from, to)` window on `MetaData.LastUpdatedTime`.
///
/// CDC takes only a `changedSince` and has no upper bound, so a truncated CDC
/// response cannot be narrowed from the top. The query endpoint can: QBO's
/// query language accepts `WHERE MetaData.LastUpdatedTime >= x AND < y`. That
/// is what makes recovery from truncation proportional — a bounded backfill of
/// the window that overflowed, rather than re-downloading the whole book.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UpdatedRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

pub trait QboClient {
    /// Paged read of records of a type, optionally bounded to an update window.
    ///
    /// Unbounded, this is the full-sweep and initial-pull path. Bounded, it is
    /// the backfill path CDC truncation falls back to (`DESIGN.md` §4.3).
    /// Results come back ordered by update time so that paging is stable.
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError>;

    /// Everything changed since a timestamp, deletes included.
    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError>;

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError>;

    fn update(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
        payload: &serde_json::Value,
        base_sync_token: &str,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError>;

    /// Look a master up by its unique name — `DisplayName` for Customer, `Name`
    /// for Item. This is the duplicate guard standing in for `RequestId` on the
    /// two types it reportedly does not cover (`DESIGN.md` §6.5).
    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError>;

    /// Paged id + `MetaData.LastUpdatedTime` read, for the reconciliation
    /// sweep's first step (`DESIGN.md` §7 step 1): who exists, and when they
    /// last changed, without pulling every payload.
    ///
    /// The default projects a full [`QboClient::query`] page down to the index
    /// fields, which is correct but wasteful. A real HTTP client should
    /// override this with a projected `SELECT Id, MetaData FROM <Entity>` so
    /// the sweep pulls ids and timestamps only rather than whole payloads.
    fn index(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<IndexEntry>, QboError> {
        Ok(self
            .query(realm, entity_type, None, start_position, max_results)?
            .into_iter()
            .map(|payload| IndexEntry {
                qbo_id: payload.qbo_id,
                last_updated_utc: payload.last_updated_utc,
                is_deleted: payload.is_deleted,
            })
            .collect())
    }

    /// One entity by id, or `None` if QBO no longer serves it at all — the
    /// reconciliation sweep's signal that a record it still holds locally was
    /// deleted there (`DESIGN.md` §7 step 3).
    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError>;
}

/// The field carrying a master's unique name, per entity type.
pub fn unique_name_field(entity_type: EntityType) -> Option<&'static str> {
    match entity_type {
        EntityType::Customer => Some("DisplayName"),
        EntityType::Item => Some("Name"),
        _ => None,
    }
}

/// Extract the unique name from a create payload, if this type has one.
pub fn unique_name_of(entity_type: EntityType, payload: &serde_json::Value) -> Option<String> {
    let field = unique_name_field(entity_type)?;
    payload.get(field)?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------------------
// In-memory double
// ---------------------------------------------------------------------------

type EntityKey = (String, EntityType, String);

/// An in-memory QBO that behaves like the real one where it matters.
#[derive(Default)]
pub struct MockQbo {
    entities: HashMap<EntityKey, EntityPayload>,
    /// Responses already returned for a `RequestId`, so a retry replays rather
    /// than duplicating — the behaviour `RequestId` exists to provide.
    request_log: HashMap<(String, Uuid), EntityPayload>,
    next_id: u64,
    clock: DateTime<Utc>,
    /// Errors to return on the next calls, oldest first. Lets a test induce a
    /// timeout at an exact point.
    scripted_failures: Vec<QboError>,
    pub create_calls: usize,
    pub query_calls: usize,
    /// How many objects one CDC response may carry. Set to the real
    /// [`crate::sync::CDC_RESPONSE_CAP`] by `new`; tests lower it so truncation
    /// can be exercised without seeding a thousand entities.
    cdc_cap: usize,
}

impl MockQbo {
    pub fn new(clock: DateTime<Utc>) -> Self {
        MockQbo {
            next_id: 1,
            clock,
            cdc_cap: crate::sync::CDC_RESPONSE_CAP,
            ..Default::default()
        }
    }

    /// Lower the CDC response cap, so a test can hit truncation with a handful
    /// of entities instead of a thousand.
    pub fn set_cdc_cap(&mut self, cap: usize) {
        self.cdc_cap = cap;
    }

    /// Queue an error to be returned by the next call, before any state change.
    /// Models a request that reached Intuit and then failed on the way back.
    pub fn fail_next(&mut self, error: QboError) {
        self.scripted_failures.push(error);
    }

    pub fn advance(&mut self, to: DateTime<Utc>) {
        self.clock = to;
    }

    fn take_failure(&mut self) -> Option<QboError> {
        if self.scripted_failures.is_empty() {
            None
        } else {
            Some(self.scripted_failures.remove(0))
        }
    }

    fn allocate_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    pub fn count(&self, realm: &RealmId, entity_type: EntityType) -> usize {
        self.entities
            .keys()
            .filter(|(r, t, _)| r == realm.as_str() && *t == entity_type)
            .count()
    }

    pub fn get(
        &self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Option<&EntityPayload> {
        self.entities
            .get(&(realm.as_str().to_string(), entity_type, qbo_id.to_string()))
    }

    /// Seed a record as though it were already in QBO.
    pub fn seed(&mut self, realm: &RealmId, payload: EntityPayload) {
        self.entities.insert(
            (
                realm.as_str().to_string(),
                payload.entity_type,
                payload.qbo_id.clone(),
            ),
            payload,
        );
    }

    /// Simulate someone editing a record in the QBO web UI: the `SyncToken`
    /// moves on, so any queued update built on the old one now conflicts.
    pub fn bump_sync_token(&mut self, realm: &RealmId, entity_type: EntityType, qbo_id: &str) {
        if let Some(entity) = self.entities.get_mut(&(
            realm.as_str().to_string(),
            entity_type,
            qbo_id.to_string(),
        )) {
            let next: u64 = entity.sync_token.parse::<u64>().unwrap_or(0) + 1;
            entity.sync_token = next.to_string();
        }
    }
}

impl QboClient for MockQbo {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        self.query_calls += 1;
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let mut matches: Vec<EntityPayload> = self
            .entities
            .iter()
            .filter(|((r, t, _), payload)| {
                r == realm.as_str()
                    && *t == entity_type
                    && updated.is_none_or(|window| {
                        payload.last_updated_utc >= window.from
                            && payload.last_updated_utc < window.to
                    })
            })
            .map(|(_, payload)| payload.clone())
            .collect();
        // Ordered by update time, then id, so a paged read is stable — which is
        // what STARTPOSITION paging assumes and what the real endpoint gives
        // with an explicit ORDERBY.
        matches.sort_by(|a, b| {
            a.last_updated_utc
                .cmp(&b.last_updated_utc)
                .then(a.qbo_id.cmp(&b.qbo_id))
        });
        Ok(matches.into_iter().skip(start_position).take(max_results).collect())
    }

    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let mut matches: Vec<EntityPayload> = self
            .entities
            .iter()
            .filter(|((r, t, _), payload)| {
                r == realm.as_str()
                    && entity_types.contains(t)
                    && payload.last_updated_utc >= changed_since
            })
            .map(|(_, payload)| payload.clone())
            .collect();
        matches.sort_by(|a, b| {
            a.last_updated_utc
                .cmp(&b.last_updated_utc)
                .then(a.qbo_id.cmp(&b.qbo_id))
        });
        // The real endpoint truncates rather than erroring, which is the whole
        // reason §4.3 exists. A double that returned everything would let a
        // driver look correct while being untested against the failure that
        // matters.
        matches.truncate(self.cdc_cap);
        Ok(matches)
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        self.create_calls += 1;

        // RequestId replay — but NOT for Customer or Item, which is the whole
        // reason those two need a query-before-create guard.
        let deduplicates = !entity_type.requires_query_before_create();
        let log_key = (realm.as_str().to_string(), request_id);
        if deduplicates {
            if let Some(existing) = self.request_log.get(&log_key) {
                return Ok(existing.clone());
            }
        }

        if let Some(error) = self.take_failure() {
            return Err(error);
        }

        // QBO enforces DisplayName uniqueness for customers independently of
        // RequestId, so a duplicate create fails loudly rather than silently
        // creating a second master.
        if let Some(name) = unique_name_of(entity_type, payload) {
            let clash = self.entities.iter().any(|((r, t, _), existing)| {
                r == realm.as_str()
                    && *t == entity_type
                    && unique_name_of(entity_type, &existing.raw_json).as_deref() == Some(&name)
            });
            if clash {
                return Err(QboError::Validation(format!("duplicate name {name:?}")));
            }
        }

        let created = EntityPayload {
            entity_type,
            qbo_id: self.allocate_id(),
            sync_token: "0".to_string(),
            last_updated_utc: self.clock,
            is_deleted: false,
            raw_json: payload.clone(),
        };
        self.seed(realm, created.clone());
        if deduplicates {
            self.request_log.insert(log_key, created.clone());
        }
        Ok(created)
    }

    fn update(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
        payload: &serde_json::Value,
        base_sync_token: &str,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        let log_key = (realm.as_str().to_string(), request_id);
        if let Some(existing) = self.request_log.get(&log_key) {
            return Ok(existing.clone());
        }
        if let Some(error) = self.take_failure() {
            return Err(error);
        }

        let key = (realm.as_str().to_string(), entity_type, qbo_id.to_string());
        let entity = self.entities.get_mut(&key).ok_or(QboError::NotFound)?;
        if entity.sync_token != base_sync_token {
            return Err(QboError::StaleSyncToken);
        }

        let next: u64 = entity.sync_token.parse::<u64>().unwrap_or(0) + 1;
        entity.sync_token = next.to_string();
        entity.raw_json = payload.clone();
        entity.last_updated_utc = self.clock;
        let updated = entity.clone();
        self.request_log.insert(log_key, updated.clone());
        Ok(updated)
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.query_calls += 1;
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        Ok(self
            .entities
            .iter()
            .find(|((r, t, _), existing)| {
                r == realm.as_str()
                    && *t == entity_type
                    && unique_name_of(entity_type, &existing.raw_json).as_deref() == Some(name)
            })
            .map(|(_, payload)| payload.clone()))
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.query_calls += 1;
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        // A record QBO has soft-deleted no longer comes back from a direct
        // read, even though `query`/`cdc` still surface its id with
        // `is_deleted` set — that gap is what tells the sweep a stale id was
        // deleted rather than merely edited.
        Ok(self
            .get(realm, entity_type, qbo_id)
            .filter(|e| !e.is_deleted)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + secs, 0).unwrap()
    }

    #[test]
    fn request_id_replays_rather_than_duplicating_an_invoice() {
        let mut qbo = MockQbo::new(at(0));
        let request_id = Uuid::now_v7();
        let payload = serde_json::json!({ "DocNumber": "21234" });

        let first = qbo.create(&realm(), EntityType::Invoice, &payload, request_id).unwrap();
        let second = qbo.create(&realm(), EntityType::Invoice, &payload, request_id).unwrap();

        assert_eq!(first.qbo_id, second.qbo_id);
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 1);
    }

    #[test]
    fn request_id_does_not_deduplicate_a_customer() {
        // The gap this whole guard exists for: replaying the same RequestId
        // against Customer does not replay the response.
        let mut qbo = MockQbo::new(at(0));
        let request_id = Uuid::now_v7();

        qbo.create(
            &realm(),
            EntityType::Customer,
            &serde_json::json!({ "DisplayName": "FAIRVIEW COUNTY PARKS & REC." }),
            request_id,
        )
        .unwrap();

        // Only the independent DisplayName uniqueness rule stops the duplicate.
        let second = qbo.create(
            &realm(),
            EntityType::Customer,
            &serde_json::json!({ "DisplayName": "FAIRVIEW COUNTY PARKS & REC." }),
            request_id,
        );
        assert!(matches!(second, Err(QboError::Validation(_))));
        assert_eq!(qbo.count(&realm(), EntityType::Customer), 1);
    }

    #[test]
    fn a_stale_sync_token_is_refused() {
        let mut qbo = MockQbo::new(at(0));
        let created = qbo
            .create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7())
            .unwrap();

        // Someone edits it in the QBO web UI.
        qbo.bump_sync_token(&realm(), EntityType::Invoice, &created.qbo_id);

        let result = qbo.update(
            &realm(),
            EntityType::Invoice,
            &created.qbo_id,
            &serde_json::json!({ "TotalAmt": 1 }),
            &created.sync_token,
            Uuid::now_v7(),
        );
        assert_eq!(result, Err(QboError::StaleSyncToken));
    }

    #[test]
    fn an_update_on_the_current_token_succeeds_and_advances_it() {
        let mut qbo = MockQbo::new(at(0));
        let created = qbo
            .create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7())
            .unwrap();
        let updated = qbo
            .update(
                &realm(),
                EntityType::Invoice,
                &created.qbo_id,
                &serde_json::json!({ "TotalAmt": 74.24 }),
                &created.sync_token,
                Uuid::now_v7(),
            )
            .unwrap();
        assert_ne!(updated.sync_token, created.sync_token);
        assert_eq!(updated.raw_json["TotalAmt"], 74.24);
    }

    #[test]
    fn find_by_name_locates_a_customer() {
        let mut qbo = MockQbo::new(at(0));
        qbo.create(
            &realm(),
            EntityType::Customer,
            &serde_json::json!({ "DisplayName": "BLUE HARBOR SWIM" }),
            Uuid::now_v7(),
        )
        .unwrap();

        let found = qbo
            .find_by_name(&realm(), EntityType::Customer, "BLUE HARBOR SWIM")
            .unwrap();
        assert!(found.is_some());
        assert!(qbo
            .find_by_name(&realm(), EntityType::Customer, "NOBODY")
            .unwrap()
            .is_none());
    }

    #[test]
    fn realms_are_isolated_in_the_double_too() {
        let mut qbo = MockQbo::new(at(0));
        let waterline = RealmId::parse("1234567890123457").unwrap();
        qbo.create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7())
            .unwrap();
        assert_eq!(qbo.count(&realm(), EntityType::Invoice), 1);
        assert_eq!(qbo.count(&waterline, EntityType::Invoice), 0);
    }

    #[test]
    fn query_pages() {
        let mut qbo = MockQbo::new(at(0));
        for index in 0..5 {
            qbo.create(
                &realm(),
                EntityType::Invoice,
                &serde_json::json!({ "DocNumber": index.to_string() }),
                Uuid::now_v7(),
            )
            .unwrap();
        }
        assert_eq!(qbo.query(&realm(), EntityType::Invoice, None, 0, 2).unwrap().len(), 2);
        assert_eq!(qbo.query(&realm(), EntityType::Invoice, None, 4, 2).unwrap().len(), 1);
        assert_eq!(qbo.query(&realm(), EntityType::Invoice, None, 9, 2).unwrap().len(), 0);
    }

    #[test]
    fn cdc_returns_only_what_changed_since_the_cursor() {
        let mut qbo = MockQbo::new(at(0));
        qbo.create(&realm(), EntityType::Invoice, &serde_json::json!({ "n": 1 }), Uuid::now_v7())
            .unwrap();
        qbo.advance(at(100));
        qbo.create(&realm(), EntityType::Invoice, &serde_json::json!({ "n": 2 }), Uuid::now_v7())
            .unwrap();

        let changed = qbo.cdc(&realm(), &[EntityType::Invoice], at(50)).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].raw_json["n"], 2);
    }

    #[test]
    fn scripted_failures_fire_once_and_in_order() {
        let mut qbo = MockQbo::new(at(0));
        qbo.fail_next(QboError::Network("connection reset".into()));
        qbo.fail_next(QboError::RateLimited);

        assert!(matches!(
            qbo.create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7()),
            Err(QboError::Network(_))
        ));
        assert_eq!(
            qbo.create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7()),
            Err(QboError::RateLimited)
        );
        assert!(qbo
            .create(&realm(), EntityType::Invoice, &serde_json::json!({}), Uuid::now_v7())
            .is_ok());
    }

    #[test]
    fn transient_and_permanent_errors_are_distinguished() {
        assert!(QboError::RateLimited.is_transient());
        assert!(QboError::Network("reset".into()).is_transient());
        assert!(!QboError::StaleSyncToken.is_transient());
        assert!(!QboError::Validation("bad".into()).is_transient());
        assert!(!QboError::NotFound.is_transient());
    }
}
