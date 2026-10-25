//! The reconciliation sweep: verifying the replica against QBO's own index
//! rather than trusting CDC. `DESIGN.md` §7, steps 1-4.
//!
//! CDC drift is real — a truncated response missed before this build existed,
//! an entity type CDC turns out not to cover, a cursor nobody polled for
//! months. The sweep is the recovery path for all of it: it does not read a
//! single payload it does not have to. It pulls QBO's id + `LastUpdatedTime`
//! index (step 1), diffs it against the replica's own index (step 2), heals
//! anything missing or stale by fetching just those records (step 3), and
//! quarantines — **never deletes** — anything local that QBO's index no
//! longer names (step 4). Orphaned `document_lines` rows are reported
//! alongside, once per realm rather than once per entity type, since they are
//! not scoped to any one type.
//!
//! **Step 5 — diffing a QBO Trial Balance against one computed from the
//! replica — is out of scope for this module.** There is no ledger yet
//! (`DESIGN.md` §9) to compute a replica-side trial balance from, so there is
//! nothing here to diff against QBO's.
//!
//! A sweep is not a sync: it never touches `sync_cursors`. [`Store::apply_batch`]
//! is called with `cursor: None` throughout, on purpose — advancing a CDC
//! cursor here would tell the next poll it can skip a window the sweep never
//! actually proved complete for that window's own reasons (§4.3 is about CDC
//! truncation, not about this).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::client::{EntityPayload, IndexEntry, QboClient, QboError};
use crate::domain::{EntityType, RealmId};
use crate::driver::MAX_PAGE_SIZE;
use crate::ratelimit::{BucketClass, RealmLimiter, RealmLimits};
use crate::store::index::{LocalIndexEntry, OrphanedLine};
use crate::store::{MirroredEntity, Store, StoreError};

/// Batches of healed entities are applied at this size, same ceiling the sync
/// driver's own paging uses — large enough that a real book heals in a
/// handful of commits, small enough that one transaction never holds the
/// whole sweep.
const HEAL_BATCH_SIZE: usize = 500;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("qbo: {0}")]
    Qbo(#[from] QboError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The limiter refused for longer than the caller was willing to wait. Not
    /// a failure of the sweep — a signal to come back later.
    #[error("rate budget exhausted for realm {realm}")]
    RateBudgetExhausted { realm: String },
}

#[derive(Clone, Debug)]
pub struct ReconcileOptions {
    pub page_size: usize,
    pub limits: RealmLimits,
}

impl Default for ReconcileOptions {
    fn default() -> Self {
        ReconcileOptions {
            page_size: MAX_PAGE_SIZE,
            limits: RealmLimits::default(),
        }
    }
}

/// What the sweep found and did for one entity type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityReconcileReport {
    pub entity_type: EntityType,
    pub checked_remote: usize,
    pub checked_local: usize,
    /// In QBO's index, not local.
    pub missing: Vec<String>,
    /// In both, but QBO's `last_updated_utc` is newer or its `is_deleted`
    /// disagrees with the local copy.
    pub stale: Vec<String>,
    /// Local and not locally deleted, but absent from QBO's index.
    pub extra: Vec<String>,
    pub healed: usize,
    pub quarantined: usize,
    /// Requests spent against QBO. The figure that matters against the rate
    /// budget.
    pub requests: usize,
}

impl EntityReconcileReport {
    /// No drift found for this type: nothing missing, stale, or extra.
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty() && self.extra.is_empty()
    }
}

/// What one realm's sweep found and did, across every entity type it covered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub entities: Vec<EntityReconcileReport>,
    pub orphaned_lines: Vec<OrphanedLine>,
}

impl ReconcileReport {
    /// No drift anywhere: every entity type is clean and no line is orphaned.
    pub fn is_clean(&self) -> bool {
        self.entities.iter().all(EntityReconcileReport::is_clean) && self.orphaned_lines.is_empty()
    }
}

pub struct Reconciler<C: QboClient> {
    client: C,
    options: ReconcileOptions,
    limiter: RealmLimiter,
}

impl<C: QboClient> Reconciler<C> {
    pub fn new(client: C, options: ReconcileOptions, now: DateTime<Utc>) -> Self {
        let limiter = RealmLimiter::new(options.limits, now);
        Reconciler {
            client,
            options,
            limiter,
        }
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    /// Sweep several entity types, then read orphaned lines once for the
    /// realm — they are not scoped to any one entity type, so reading them
    /// once per entity type would just repeat the same query.
    pub fn sweep_realm(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_types: impl IntoIterator<Item = EntityType>,
        now: DateTime<Utc>,
    ) -> Result<ReconcileReport, ReconcileError> {
        let mut report = ReconcileReport::default();
        for entity_type in entity_types {
            report
                .entities
                .push(self.sweep_entity_type(store, realm, entity_type, now)?);
        }
        report.orphaned_lines = store.orphaned_lines(realm)?;
        Ok(report)
    }

    /// Steps 1-4 of `DESIGN.md` §7, for one entity type.
    pub fn sweep_entity_type(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        now: DateTime<Utc>,
    ) -> Result<EntityReconcileReport, ReconcileError> {
        let mut report = EntityReconcileReport {
            entity_type,
            checked_remote: 0,
            checked_local: 0,
            missing: Vec::new(),
            stale: Vec::new(),
            extra: Vec::new(),
            healed: 0,
            quarantined: 0,
            requests: 0,
        };

        let remote = self.pull_index(realm, entity_type, now, &mut report)?;
        report.checked_remote = remote.len();

        let local: HashMap<String, LocalIndexEntry> = store
            .entity_index(realm, entity_type)?
            .into_iter()
            .map(|entry| (entry.qbo_id.clone(), entry))
            .collect();
        report.checked_local = local.len();

        diff_indexes(&remote, &local, &mut report);

        // Missing and stale ids are healed the same way — fetch, then apply —
        // so they are handed to `heal` as one list rather than as two
        // parameters that would only be zipped back together inside it.
        let mut to_heal: Vec<String> = report
            .missing
            .iter()
            .chain(&report.stale)
            .cloned()
            .collect();
        to_heal.sort();
        let extra = report.extra.clone();

        self.heal(store, realm, entity_type, &to_heal, now, &mut report)?;
        self.quarantine_extra(store, realm, entity_type, &extra, now, &mut report)?;

        Ok(report)
    }

    // -----------------------------------------------------------------------

    /// Step 1: page `index` until a short page proves there is no more.
    fn pull_index(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        now: DateTime<Utc>,
        report: &mut EntityReconcileReport,
    ) -> Result<HashMap<String, IndexEntry>, ReconcileError> {
        let mut remote = HashMap::new();
        let mut start = 0usize;

        loop {
            self.spend(realm, now)?;
            report.requests += 1;
            let page = self
                .client
                .index(realm, entity_type, start, self.options.page_size)?;
            let returned = page.len();
            for entry in page {
                remote.insert(entry.qbo_id.clone(), entry);
            }

            let final_page = returned < self.options.page_size;
            if final_page {
                break;
            }
            start += returned;
        }

        Ok(remote)
    }

    /// Step 3: fetch each missing and stale id and apply it, batched. A
    /// `fetch` that comes back `None` for a stale id means QBO deleted it
    /// between the index read and now — heal that as a local delete, keeping
    /// the raw JSON the replica already has rather than losing it.
    fn heal(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        to_heal: &[String],
        now: DateTime<Utc>,
        report: &mut EntityReconcileReport,
    ) -> Result<(), ReconcileError> {
        let mut batch: Vec<MirroredEntity> = Vec::new();

        for qbo_id in to_heal {
            self.spend(realm, now)?;
            report.requests += 1;

            match self.client.fetch(realm, entity_type, qbo_id)? {
                Some(payload) => batch.push(mirrored(payload)),
                None => {
                    if let Some(existing) = store.get_entity(realm, entity_type, qbo_id)? {
                        batch.push(MirroredEntity {
                            entity_type,
                            qbo_id: qbo_id.clone(),
                            sync_token: existing.sync_token,
                            last_updated_utc: now,
                            is_deleted: true,
                            raw_json: existing.raw_json,
                        });
                    }
                }
            }

            if batch.len() >= HEAL_BATCH_SIZE {
                report.healed += flush(store, realm, &mut batch, now)?;
            }
        }
        report.healed += flush(store, realm, &mut batch, now)?;

        Ok(())
    }

    /// Step 4: quarantine every extra id. Never delete.
    fn quarantine_extra(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        extra: &[String],
        now: DateTime<Utc>,
        report: &mut EntityReconcileReport,
    ) -> Result<(), ReconcileError> {
        let reason = format!("not in QBO index at {}", now.to_rfc3339());
        for qbo_id in extra {
            if store.quarantine_existing(realm, entity_type, qbo_id, &reason, now)? {
                report.quarantined += 1;
            }
        }
        Ok(())
    }

    /// Take a token or refuse. The sweep does not sleep — a caller that wants
    /// to wait is better placed to decide how long than a loop in here.
    fn spend(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<(), ReconcileError> {
        if self.limiter.try_acquire(BucketClass::General, now) {
            // The sweep issues requests sequentially, so the slot is done with
            // as soon as it is taken; the concurrency guard exists for the
            // write path.
            self.limiter.release();
            Ok(())
        } else {
            Err(ReconcileError::RateBudgetExhausted {
                realm: realm.as_str().to_string(),
            })
        }
    }
}

/// Step 2: classify every id either side knows about.
///
/// **Missing** entries from QBO's index that are already flagged deleted
/// there are skipped — a record local never mirrored and QBO says is gone is
/// not drift, it is agreement. A local row already flagged deleted is likewise
/// never counted **extra**, for the same reason in the other direction.
fn diff_indexes(
    remote: &HashMap<String, IndexEntry>,
    local: &HashMap<String, LocalIndexEntry>,
    report: &mut EntityReconcileReport,
) {
    for (qbo_id, remote_entry) in remote {
        match local.get(qbo_id) {
            None => {
                if !remote_entry.is_deleted {
                    report.missing.push(qbo_id.clone());
                }
            }
            Some(local_entry) => {
                if remote_entry.last_updated_utc > local_entry.last_updated_utc
                    || remote_entry.is_deleted != local_entry.is_deleted
                {
                    report.stale.push(qbo_id.clone());
                }
            }
        }
    }

    for (qbo_id, local_entry) in local {
        if !local_entry.is_deleted && !remote.contains_key(qbo_id) {
            report.extra.push(qbo_id.clone());
        }
    }

    report.missing.sort();
    report.stale.sort();
    report.extra.sort();
}

/// Apply and clear a heal batch, returning how many entities were applied.
///
/// `cursor: None` throughout — a sweep is not a sync and must not move the CDC
/// cursor (module doc comment above).
fn flush(
    store: &Store,
    realm: &RealmId,
    batch: &mut Vec<MirroredEntity>,
    now: DateTime<Utc>,
) -> Result<usize, ReconcileError> {
    if batch.is_empty() {
        return Ok(0);
    }
    let applied = store.apply_batch(realm, batch, None, now)?;
    batch.clear();
    Ok(applied.mirrored)
}

/// As `driver::mirrored` — a fetched payload becomes the shape `apply_batch`
/// takes. Copied rather than shared because that conversion is not part of the
/// sync driver's public surface.
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
