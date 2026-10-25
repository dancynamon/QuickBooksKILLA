//! The reconciliation sweep. `DESIGN.md` §7.
//!
//! CDC is a latency optimisation; this is the correctness check behind it. One
//! entity type at a time: read every live record from QBO, diff against the
//! replica, heal what is missing or stale, and quarantine — never delete — what
//! the replica holds that QBO no longer does.
//!
//! The remote read uses the same paged query as a full sweep, so it returns
//! full payloads and healing costs no second request. An id-and-timestamp-only
//! read would be cheaper on the wire and is the obvious refinement once the
//! query language's field projection is confirmed against a live realm.
//!
//! Not here: §7.5's trial-balance diff. A replica-computed trial balance is
//! posting logic, which belongs to the ledger project (`DECISIONS.md` D1).

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::{EntitySyncReport, SyncDriver, SyncError, SyncPath};
use crate::client::{EntityPayload, QboClient};
use crate::domain::{EntityType, RealmId};
use crate::store::Store;
use crate::sync::SweepReason;

/// One disagreement between QBO and the replica.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Discrepancy {
    /// In QBO, absent locally — or present locally only as a tombstone.
    Missing { qbo_id: String },
    /// Local copy older than QBO's.
    Stale {
        qbo_id: String,
        local: DateTime<Utc>,
        remote: DateTime<Utc>,
    },
    /// Live locally, absent from QBO. A delete CDC never delivered, or a row
    /// skipped by paging across a tie. Quarantined, not removed.
    Extra { qbo_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    pub entity_type: EntityType,
    pub remote: usize,
    pub local: usize,
    pub discrepancies: Vec<Discrepancy>,
    /// Missing and stale records re-mirrored from the payloads just read.
    pub healed: usize,
    pub quarantined_extra: usize,
    /// Extras left alone because they changed after the sweep began — the
    /// remote read may simply have paged past them.
    pub skipped_recent: usize,
    pub requests: usize,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.discrepancies.is_empty()
    }
}

impl<C: QboClient> SyncDriver<C> {
    /// Reconcile one entity type. Cursors are not touched: this checks the
    /// replica, it does not redefine where incremental sync resumes.
    pub fn reconcile_entity_type(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_type: EntityType,
        now: DateTime<Utc>,
    ) -> Result<ReconcileReport, SyncError> {
        // `fetch_page` and `apply` report into a sync report; this one is a
        // scratch accumulator whose request count is carried over below.
        let mut scratch = EntitySyncReport {
            entity_type,
            path: SyncPath::FullSweep {
                reason: SweepReason::NoCursor,
                pages: 0,
            },
            mirrored: 0,
            projected: 0,
            quarantined: 0,
            requests: 0,
        };

        let mut remote: HashMap<String, EntityPayload> = HashMap::new();
        let mut start = 0usize;
        loop {
            let page = self.fetch_page(realm, entity_type, None, start, now, &mut scratch)?;
            let returned = page.len();
            // The query endpoint omits deleted records; a double that returns
            // tombstones is treated the same way.
            remote.extend(
                page.into_iter()
                    .filter(|p| !p.is_deleted)
                    .map(|p| (p.qbo_id.clone(), p)),
            );
            if returned < self.options.page_size {
                break;
            }
            start += returned;
        }

        let local = store.indexed_entities(realm, entity_type)?;
        let local_by_id: HashMap<&str, _> =
            local.iter().map(|row| (row.qbo_id.as_str(), row)).collect();

        let mut discrepancies = Vec::new();
        let mut heal: Vec<EntityPayload> = Vec::new();

        let mut remote_ids: Vec<&String> = remote.keys().collect();
        remote_ids.sort();
        for qbo_id in remote_ids {
            let payload = &remote[qbo_id];
            match local_by_id.get(qbo_id.as_str()) {
                None => {
                    discrepancies.push(Discrepancy::Missing { qbo_id: qbo_id.clone() });
                    heal.push(payload.clone());
                }
                Some(row) if row.is_deleted => {
                    discrepancies.push(Discrepancy::Missing { qbo_id: qbo_id.clone() });
                    heal.push(payload.clone());
                }
                Some(row) if row.last_updated_utc < payload.last_updated_utc => {
                    discrepancies.push(Discrepancy::Stale {
                        qbo_id: qbo_id.clone(),
                        local: row.last_updated_utc,
                        remote: payload.last_updated_utc,
                    });
                    heal.push(payload.clone());
                }
                // Equal, or local ahead because a poll landed mid-sweep.
                Some(_) => {}
            }
        }

        if !heal.is_empty() {
            self.apply(store, realm, &heal, None, now, &mut scratch)?;
        }

        let mut quarantined_extra = 0;
        let mut skipped_recent = 0;
        let mut extras: Vec<_> = local
            .iter()
            .filter(|row| !row.is_deleted && !remote.contains_key(&row.qbo_id))
            .collect();
        extras.sort_by(|a, b| a.qbo_id.cmp(&b.qbo_id));
        for row in extras {
            if row.last_updated_utc >= now {
                skipped_recent += 1;
                continue;
            }
            discrepancies.push(Discrepancy::Extra { qbo_id: row.qbo_id.clone() });
            if let Some(entity) = store.get_entity(realm, entity_type, &row.qbo_id)? {
                store.quarantine(
                    realm,
                    &entity,
                    &format!("reconcile {}: live locally, absent from QBO", now.to_rfc3339()),
                    now,
                )?;
                quarantined_extra += 1;
            }
        }

        Ok(ReconcileReport {
            entity_type,
            remote: remote.len(),
            local: local.iter().filter(|row| !row.is_deleted).count(),
            discrepancies,
            healed: heal.len(),
            quarantined_extra,
            skipped_recent,
            requests: scratch.requests,
        })
    }

    /// Reconcile several types, masters first, and check line integrity.
    pub fn reconcile_realm(
        &mut self,
        store: &Store,
        realm: &RealmId,
        entity_types: &[EntityType],
        now: DateTime<Utc>,
    ) -> Result<(Vec<ReconcileReport>, i64), SyncError> {
        let mut ordered = entity_types.to_vec();
        ordered.sort_by_key(|entity_type| entity_type.tier());
        let mut reports = Vec::new();
        for entity_type in ordered {
            reports.push(self.reconcile_entity_type(store, realm, entity_type, now)?);
        }
        Ok((reports, store.orphaned_line_count(realm)?))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::client::MockQbo;
    use crate::driver::SyncOptions;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(hours: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap() + Duration::hours(hours)
    }

    fn customer(id: &str, updated: DateTime<Utc>) -> EntityPayload {
        EntityPayload {
            entity_type: EntityType::Customer,
            qbo_id: id.to_string(),
            sync_token: "0".to_string(),
            last_updated_utc: updated,
            is_deleted: false,
            raw_json: json!({ "Id": id, "SyncToken": "0", "DisplayName": format!("CUSTOMER {id}") }),
        }
    }

    fn setup() -> (Store, SyncDriver<MockQbo>) {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test Company", at(0)).unwrap();
        let mut qbo = MockQbo::new(at(0));
        for id in ["1", "2", "3"] {
            qbo.seed(&realm(), customer(id, at(1)));
        }
        let options = SyncOptions {
            page_size: 2,
            ..SyncOptions::default()
        };
        let mut driver = SyncDriver::new(qbo, options, at(0));
        driver
            .sync_entity_type(&store, &realm(), EntityType::Customer, at(2))
            .unwrap();
        (store, driver)
    }

    #[test]
    fn a_replica_in_step_with_qbo_is_clean() {
        let (store, mut driver) = setup();
        let report = driver
            .reconcile_entity_type(&store, &realm(), EntityType::Customer, at(3))
            .unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!((report.remote, report.local), (3, 3));
        assert_eq!(report.requests, 2, "three records at page size two");
    }

    #[test]
    fn missing_and_stale_records_are_healed_from_the_payloads_read() {
        let (store, mut driver) = setup();
        driver.client_mut().seed(&realm(), customer("4", at(1)));
        let mut edited = customer("2", at(4));
        edited.raw_json["DisplayName"] = json!("CUSTOMER 2 RENAMED");
        driver.client_mut().seed(&realm(), edited);

        let report = driver
            .reconcile_entity_type(&store, &realm(), EntityType::Customer, at(5))
            .unwrap();
        assert_eq!(
            report.discrepancies,
            vec![
                Discrepancy::Stale { qbo_id: "2".into(), local: at(1), remote: at(4) },
                Discrepancy::Missing { qbo_id: "4".into() },
            ]
        );
        assert_eq!(report.healed, 2);
        let healed = store.get_entity(&realm(), EntityType::Customer, "2").unwrap().unwrap();
        assert_eq!(healed.raw_json["DisplayName"], "CUSTOMER 2 RENAMED");

        let again = driver
            .reconcile_entity_type(&store, &realm(), EntityType::Customer, at(6))
            .unwrap();
        assert!(again.is_clean(), "healing converges: {again:?}");
    }

    #[test]
    fn an_extra_is_quarantined_and_never_deleted() {
        let (store, mut driver) = setup();
        // Deleted in QBO, and the delete never arrived through CDC.
        let mut qbo = MockQbo::new(at(0));
        for id in ["1", "2"] {
            qbo.seed(&realm(), customer(id, at(1)));
        }
        *driver.client_mut() = qbo;

        let report = driver
            .reconcile_entity_type(&store, &realm(), EntityType::Customer, at(5))
            .unwrap();
        assert_eq!(report.discrepancies, vec![Discrepancy::Extra { qbo_id: "3".into() }]);
        assert_eq!(report.quarantined_extra, 1);
        assert!(store.get_entity(&realm(), EntityType::Customer, "3").unwrap().is_some());
        let quarantined = store.quarantined(&realm()).unwrap();
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].reason.contains("absent from QBO"));
    }

    #[test]
    fn a_local_tombstone_qbo_still_has_is_missing() {
        let (store, mut driver) = setup();
        let mut tombstone = customer("1", at(1));
        tombstone.is_deleted = true;
        store
            .apply_batch(&realm(), &[super::super::mirrored(tombstone)], None, at(2))
            .unwrap();
        let report = driver
            .reconcile_entity_type(&store, &realm(), EntityType::Customer, at(3))
            .unwrap();
        assert_eq!(report.discrepancies, vec![Discrepancy::Missing { qbo_id: "1".into() }]);
        assert!(!store.get_entity(&realm(), EntityType::Customer, "1").unwrap().unwrap().is_deleted);
    }

    #[test]
    fn a_line_integrity_check_runs_with_the_realm() {
        let (store, mut driver) = setup();
        let (reports, orphans) = driver
            .reconcile_realm(&store, &realm(), &[EntityType::Customer], at(3))
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(orphans, 0);
    }
}
