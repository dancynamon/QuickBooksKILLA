//! The sync driver: what actually keeps the replica current. `DESIGN.md` §4.
//!
//! Everything this needs already existed in pieces — a client trait, a cursor
//! strategy, a store, a projection. This is the loop that runs them, and the
//! place where §4's failure modes stop being described and start being handled.
//!
//! It is generic over [`QboClient`], so the whole of it is exercised against the
//! in-memory double. The only part that needs Intuit credentials is the HTTP
//! implementation of that trait.

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::client::{EntityPayload, QboClient, QboError, UpdatedRange};
use crate::domain::{EntityType, RealmId};
use crate::ratelimit::{BucketClass, RealmLimiter, RealmLimits};
use crate::store::{MirroredEntity, Store, StoreError};
use crate::sync::{
    cdc_coverage_confirmed, classify_response_with_cap, plan_sync, CdcOutcome, SweepReason,
    SyncCursor, SyncStrategy, CDC_RESPONSE_CAP, DEFAULT_CDC_MAX_AGE_DAYS,
};

/// QBO's own paging maximum for a query.
pub const MAX_PAGE_SIZE: usize = 1000;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("qbo: {0}")]
    Qbo(#[from] QboError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The limiter refused for longer than the caller was willing to wait. Not
    /// a failure of sync — a signal to come back later.
    #[error("rate budget exhausted for realm {realm}")]
    RateBudgetExhausted { realm: String },
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub page_size: usize,
    /// How stale a CDC cursor may get before a sweep replaces it.
    pub cdc_max_age: Duration,
    /// The response size at which a CDC reply is assumed truncated. Lowered by
    /// tests; otherwise the real cap.
    pub cdc_response_cap: usize,
    pub limits: RealmLimits,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            page_size: MAX_PAGE_SIZE,
            cdc_max_age: Duration::days(DEFAULT_CDC_MAX_AGE_DAYS),
            cdc_response_cap: CDC_RESPONSE_CAP,
            limits: RealmLimits::default(),
        }
    }
}

/// What one entity type's sync actually did.
///
/// Reported rather than logged, because "how did the replica get into this
/// state" is a question worth being able to answer without a log file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitySyncReport {
    pub entity_type: EntityType,
    pub path: SyncPath,
    pub mirrored: usize,
    pub projected: usize,
    pub quarantined: usize,
    /// Requests spent. The figure that matters against the rate budget.
    pub requests: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncPath {
    /// A full paged read of every record of the type.
    FullSweep { reason: SweepReason, pages: usize },
    /// An incremental poll that covered its window.
    Cdc { changed_since: DateTime<Utc> },
    /// A poll came back at the cap, so the window was re-read through the
    /// bounded query endpoint instead — which pages, where CDC does not.
    CdcThenBackfill {
        changed_since: DateTime<Utc>,
        pages: usize,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub entities: Vec<EntitySyncReport>,
}

impl SyncReport {
    pub fn mirrored(&self) -> usize {
        self.entities.iter().map(|report| report.mirrored).sum()
    }

    pub fn quarantined(&self) -> usize {
        self.entities.iter().map(|report| report.quarantined).sum()
    }

    pub fn requests(&self) -> usize {
        self.entities.iter().map(|report| report.requests).sum()
    }
}

pub struct SyncDriver<C: QboClient> {
    client: C,
    options: SyncOptions,
    limiter: RealmLimiter,
}

impl<C: QboClient> SyncDriver<C> {
    pub fn new(client: C, options: SyncOptions, now: DateTime<Utc>) -> Self {
        let limiter = RealmLimiter::new(options.limits, now);
        SyncDriver {
            client,
            options,
            limiter,
        }
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    /// Sync several entity types, masters before documents.
    ///
    /// Order matters: a document's projection joins to its contact for display,
    /// and mirroring documents first would leave a register briefly showing
    /// rows with no customer name. Nothing breaks, but the first thing the user
    /// sees would be wrong, and the fix costs nothing but an ordering.
    pub fn sync_realm(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_types: &[EntityType],
        now: DateTime<Utc>,
    ) -> Result<SyncReport, SyncError> {
        let mut ordered = entity_types.to_vec();
        ordered.sort_by_key(|entity_type| entity_type.tier());

        let mut report = SyncReport::default();
        for entity_type in ordered {
            report
                .entities
                .push(self.sync_entity_type(store, realm, entity_type, now)?);
        }
        Ok(report)
    }

    pub fn sync_entity_type(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        now: DateTime<Utc>,
    ) -> Result<EntitySyncReport, SyncError> {
        let cursor = store.load_cursor(realm, entity_type)?;
        let strategy = plan_sync(
            &cursor,
            now,
            self.options.cdc_max_age,
            cdc_coverage_confirmed(entity_type),
        );

        match strategy {
            SyncStrategy::FullSweep { reason } => {
                self.full_sweep(store, realm, entity_type, reason, now)
            }
            SyncStrategy::Cdc { changed_since } => {
                self.poll(store, realm, entity_type, cursor, changed_since, now)
            }
        }
    }

    // -----------------------------------------------------------------------

    fn full_sweep(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        reason: SweepReason,
        now: DateTime<Utc>,
    ) -> Result<EntitySyncReport, SyncError> {
        let mut report = EntitySyncReport {
            entity_type,
            path: SyncPath::FullSweep {
                reason: reason.clone(),
                pages: 0,
            },
            mirrored: 0,
            projected: 0,
            quarantined: 0,
            requests: 0,
        };

        let mut start = 0usize;
        let mut pages = 0usize;

        loop {
            let page = self.fetch_page(realm, entity_type, None, start, now, &mut report)?;
            pages += 1;
            let returned = page.len();

            // The sweep's own cursor is the moment the sweep started. Using the
            // newest record's timestamp instead would leave a gap for anything
            // changed mid-sweep, which is the same silent loss §4.3 guards CDC
            // against.
            let final_page = returned < self.options.page_size;
            let cursor = final_page.then(|| SyncCursor {
                realm_id: realm.clone(),
                entity_type,
                last_cdc_cursor: Some(now),
                last_full_sweep: Some(now),
            });

            self.apply(store, realm, &page, cursor.as_ref(), now, &mut report)?;

            if final_page {
                break;
            }
            start += returned;
        }

        report.path = SyncPath::FullSweep { reason, pages };
        Ok(report)
    }

    fn poll(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        cursor: SyncCursor,
        changed_since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<EntitySyncReport, SyncError> {
        let mut report = EntitySyncReport {
            entity_type,
            path: SyncPath::Cdc { changed_since },
            mirrored: 0,
            projected: 0,
            quarantined: 0,
            requests: 0,
        };

        self.spend(realm, BucketClass::General, now)?;
        report.requests += 1;
        let changed = self.client.cdc(realm, &[entity_type], changed_since)?;

        match classify_response_with_cap(changed.len(), self.options.cdc_response_cap) {
            CdcOutcome::Complete => {
                let mut advanced = cursor;
                advanced.advance(&CdcOutcome::Complete, now);
                self.apply(store, realm, &changed, Some(&advanced), now, &mut report)?;
                Ok(report)
            }
            CdcOutcome::PossiblyTruncated => {
                // The response may be missing records, so none of it can be
                // trusted to define a new cursor. Discard it and re-read the
                // window through the bounded endpoint, where the size of each
                // request is under our control rather than Intuit's.
                self.backfill(store, realm, entity_type, changed_since, now, &mut report)
            }
        }
    }

    /// Re-read `[changed_since, now)` through the query endpoint, paged.
    ///
    /// This is why the bounded window matters. CDC takes only a `changedSince`
    /// and has no upper bound, so a truncated CDC response cannot be narrowed:
    /// moving the cursor forward to the newest record received would silently
    /// drop any record sharing that timestamp that the response did not
    /// include. The query endpoint takes both bounds *and* pages, so the same
    /// window can be re-read completely, at a page size we choose rather than
    /// one Intuit imposes.
    ///
    /// The cursor advances to the moment the sync started, never to the newest
    /// record seen. Anything modified while the backfill was running has a later
    /// timestamp and is picked up by the next poll rather than skipped by this
    /// one.
    fn backfill(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        changed_since: DateTime<Utc>,
        now: DateTime<Utc>,
        report: &mut EntitySyncReport,
    ) -> Result<EntitySyncReport, SyncError> {
        let window = UpdatedRange {
            from: changed_since,
            to: now,
        };
        let last_full_sweep = store.load_cursor(realm, entity_type)?.last_full_sweep;

        let mut start = 0usize;
        let mut pages = 0usize;

        loop {
            let page = self.fetch_page(realm, entity_type, Some(window), start, now, report)?;
            pages += 1;
            let returned = page.len();
            let final_page = returned < self.options.page_size;

            let cursor = final_page.then(|| SyncCursor {
                realm_id: realm.clone(),
                entity_type,
                last_cdc_cursor: Some(now),
                last_full_sweep,
            });

            self.apply(store, realm, &page, cursor.as_ref(), now, report)?;

            if final_page {
                break;
            }
            start += returned;
        }

        report.path = SyncPath::CdcThenBackfill {
            changed_since,
            pages,
        };
        Ok(report.clone())
    }

    // -----------------------------------------------------------------------

    fn fetch_page(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        now: DateTime<Utc>,
        report: &mut EntitySyncReport,
    ) -> Result<Vec<EntityPayload>, SyncError> {
        self.spend(realm, BucketClass::General, now)?;
        report.requests += 1;
        Ok(self.client.query(
            realm,
            entity_type,
            updated,
            start_position,
            self.options.page_size,
        )?)
    }

    fn apply(
        &mut self,
        store: &Store,
        realm: &RealmId,
        payloads: &[EntityPayload],
        cursor: Option<&SyncCursor>,
        now: DateTime<Utc>,
        report: &mut EntitySyncReport,
    ) -> Result<(), SyncError> {
        let entities: Vec<MirroredEntity> = payloads.iter().cloned().map(mirrored).collect();
        let applied = store.apply_batch(realm, &entities, cursor, now)?;
        report.mirrored += applied.mirrored;
        report.projected += applied.projected;
        report.quarantined += applied.quarantined;
        Ok(())
    }

    /// Take a token or refuse. The driver does not sleep — a caller that wants
    /// to wait is better placed to decide how long than a loop inside here.
    fn spend(
        &mut self,
        realm: &RealmId,
        class: BucketClass,
        now: DateTime<Utc>,
    ) -> Result<(), SyncError> {
        if self.limiter.try_acquire(class, now) {
            // Sync requests are sequential, so the slot is done with as soon as
            // it is taken; the concurrency guard exists for the write path.
            self.limiter.release();
            Ok(())
        } else {
            Err(SyncError::RateBudgetExhausted {
                realm: realm.as_str().to_string(),
            })
        }
    }
}

fn mirrored(payload: EntityPayload) -> MirroredEntity {
    MirroredEntity {
        entity_type: payload.entity_type,
        qbo_id: payload.qbo_id,
        sync_token: payload.sync_token,
        last_updated_utc: payload.last_updated_utc,
        is_deleted: payload.is_deleted,
        raw_json: payload.raw_json,
    }
}
