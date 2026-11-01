//! Sync cursors against a Mac clock that disagrees with Intuit's. `DESIGN.md`
//! §4.3: cursors are Intuit's timestamps, never the local clock.
//!
//! Throughout, [`MockQbo`]'s clock is Intuit's, and the `now` handed to the
//! driver is the Mac's. Before this file the driver cursored on the second,
//! so a Mac running ahead wrote a cursor past changes nobody had fetched, and
//! the next poll silently stepped over them.
//!
//! Names and figures are invented (HANDOFF.md §2.6).

use chrono::{DateTime, Duration, TimeZone, Utc};
use proptest::prelude::*;
use qbo_local::client::{EntityPayload, MockQbo, QboClient, QboError, ReadPage, UpdatedRange};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::driver::{SyncDriver, SyncOptions, SyncPath};
use qbo_local::ratelimit::{BucketBudget, RealmLimits};
use qbo_local::store::{MirroredEntity, Store};
use qbo_local::sync::{SweepReason, SyncCursor};
use serde_json::json;
use uuid::Uuid;

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

/// Intuit's clock, in minutes from a fixed start.
fn intuit(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap() + Duration::minutes(minutes)
}

fn store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", intuit(0))
        .unwrap();
    store
}

fn invoice(id: &str, version: u32, updated: DateTime<Utc>) -> EntityPayload {
    EntityPayload {
        entity_type: EntityType::Invoice,
        qbo_id: id.to_string(),
        sync_token: version.to_string(),
        last_updated_utc: updated,
        is_deleted: false,
        raw_json: json!({
            "Id": id, "DocNumber": format!("88{id}"), "TxnDate": "2026-10-01",
            "CustomerRef": { "value": "31" }, "TotalAmt": 100.0 + f64::from(version),
            "SyncToken": version.to_string(),
        }),
    }
}

fn options() -> SyncOptions {
    SyncOptions {
        limits: RealmLimits {
            general: BucketBudget::per_minute(100_000.0),
            ..RealmLimits::default()
        },
        ..SyncOptions::default()
    }
}

/// Sweep once at Intuit's `intuit(0)`, with the Mac reading `mac_now`.
fn swept(qbo: MockQbo, store: &Store, mac_now: DateTime<Utc>) -> SyncDriver<MockQbo> {
    let mut driver = SyncDriver::new(qbo, options(), mac_now);
    let report = driver
        .sync_entity_type(store, &realm(), EntityType::Invoice, mac_now)
        .unwrap();
    assert!(matches!(report.path, SyncPath::FullSweep { .. }));
    driver
}

fn held_version(store: &Store, id: &str) -> Option<String> {
    store
        .get_entity(&realm(), EntityType::Invoice, id)
        .unwrap()
        .map(|entity| entity.sync_token)
}

fn cdc_cursor(store: &Store) -> DateTime<Utc> {
    store
        .load_cursor(&realm(), EntityType::Invoice)
        .unwrap()
        .last_cdc_cursor
        .unwrap()
}

// ---------------------------------------------------------------------------
// Skew
// ---------------------------------------------------------------------------

#[test]
fn a_mac_clock_running_ahead_does_not_skip_what_changed_in_the_gap() {
    // The bug as recorded in DESIGN.md §12 item 5: the Mac reads two hours
    // ahead. Cursoring on the Mac's clock wrote 11:00 while Intuit read 9:00,
    // and an edit at 9:05 was older than the cursor, so no poll ever asked
    // for it.
    let ahead = Duration::hours(2);
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0) + ahead);
    assert_eq!(
        cdc_cursor(&store),
        intuit(0),
        "the cursor is Intuit's clock"
    );

    driver.client_mut().advance(intuit(10));
    driver
        .client_mut()
        .seed(&realm(), invoice("1", 0, intuit(5)));

    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(15) + ahead)
        .unwrap();
    assert_eq!(held_version(&store, "1").as_deref(), Some("0"));
    assert_eq!(cdc_cursor(&store), intuit(10));
}

#[test]
fn a_mac_clock_running_behind_neither_skips_nor_counts_a_record_twice() {
    let behind = -Duration::hours(2);
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0) + behind);

    driver.client_mut().advance(intuit(10));
    driver
        .client_mut()
        .seed(&realm(), invoice("1", 0, intuit(9)));
    let first = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(10) + behind)
        .unwrap();
    assert_eq!(first.mirrored, 1);
    assert_eq!(cdc_cursor(&store), intuit(10));

    // The next poll reaches back past the cursor and sees the 9:09 edit
    // again. It is the version already held, so it is not a change.
    driver.client_mut().advance(intuit(11));
    let second = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(11) + behind)
        .unwrap();
    assert!(matches!(second.path, SyncPath::Cdc { .. }));
    assert_eq!(second.mirrored, 0, "a re-delivery is not a change");
    assert_eq!(held_version(&store, "1").as_deref(), Some("0"));
}

#[test]
fn a_record_stamped_just_before_the_cursor_is_caught_by_the_overlap() {
    // Intuit answered at 9:10, so the cursor is 9:10. A write that was still
    // committing then surfaces afterwards stamped 9:08. Without the overlap
    // the next poll, from 9:10, would never see it.
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0));
    driver.client_mut().advance(intuit(10));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(10))
        .unwrap();
    assert_eq!(cdc_cursor(&store), intuit(10));

    driver.client_mut().advance(intuit(11));
    driver
        .client_mut()
        .seed(&realm(), invoice("1", 0, intuit(8)));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(11))
        .unwrap();
    assert_eq!(report.mirrored, 1);
    assert_eq!(held_version(&store, "1").as_deref(), Some("0"));
}

#[test]
fn a_cursor_ahead_of_intuits_clock_is_replaced_by_a_sweep() {
    // A cursor written before this fix, by a Mac running an hour ahead: it
    // says 10:00 while Intuit says 9:30. The edit at 9:20 is behind it and no
    // poll can find it, so the driver sweeps rather than trusting it.
    let mut qbo = MockQbo::new(intuit(30));
    qbo.seed(&realm(), invoice("1", 0, intuit(20)));
    let store = store();
    store
        .save_cursor(&SyncCursor {
            realm_id: realm(),
            entity_type: EntityType::Invoice,
            last_cdc_cursor: Some(intuit(60)),
            last_full_sweep: Some(intuit(60)),
        })
        .unwrap();

    let mut driver = SyncDriver::new(qbo, options(), intuit(90));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(90))
        .unwrap();
    assert!(
        matches!(
            report.path,
            SyncPath::FullSweep {
                reason: SweepReason::CursorAheadOfServer,
                ..
            }
        ),
        "{:?}",
        report.path
    );
    assert_eq!(report.requests, 2, "the poll that found it, and the sweep");
    assert_eq!(held_version(&store, "1").as_deref(), Some("0"));
    assert_eq!(cdc_cursor(&store), intuit(30), "re-based on Intuit's clock");
}

// ---------------------------------------------------------------------------
// Sleep and wake
// ---------------------------------------------------------------------------

#[test]
fn a_machine_that_slept_picks_up_everything_changed_while_it_slept() {
    // The lid closed on Friday evening and opened on Monday morning. Edits
    // landed all weekend; the first poll after waking has all of them.
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0));

    let weekend = 60 * 24 * 3;
    for (index, minute) in [30, 60 * 20, 60 * 40, weekend - 5].into_iter().enumerate() {
        driver
            .client_mut()
            .seed(&realm(), invoice(&index.to_string(), 0, intuit(minute)));
    }
    driver.client_mut().advance(intuit(weekend));

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(weekend))
        .unwrap();
    assert!(matches!(report.path, SyncPath::Cdc { .. }));
    assert_eq!(report.mirrored, 4);
    assert_eq!(cdc_cursor(&store), intuit(weekend));
}

#[test]
fn a_clock_that_stood_still_through_a_sleep_still_cursors_on_intuits() {
    // A suspended VM or a dead clock battery: the Mac wakes still reading the
    // moment it slept, while Intuit's clock moved on a day. The cursor follows
    // Intuit, and an edit made during the day is not lost to the stale clock.
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0));

    driver
        .client_mut()
        .seed(&realm(), invoice("1", 0, intuit(60 * 12)));
    driver.client_mut().advance(intuit(60 * 24));
    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(1))
        .unwrap();
    assert_eq!(report.mirrored, 1);
    assert_eq!(cdc_cursor(&store), intuit(60 * 24));
}

// ---------------------------------------------------------------------------
// Empty pages
// ---------------------------------------------------------------------------

#[test]
fn an_empty_cdc_page_advances_the_cursor_to_intuits_clock() {
    // Nothing changed, but Intuit's answer still proves nothing changed up
    // to its clock, so the cursor moves rather than ageing toward a sweep.
    let store = store();
    let mut driver = swept(MockQbo::new(intuit(0)), &store, intuit(0));
    driver.client_mut().advance(intuit(60));

    let report = driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(60))
        .unwrap();
    assert!(matches!(report.path, SyncPath::Cdc { .. }));
    assert_eq!(report.mirrored, 0);
    assert_eq!(cdc_cursor(&store), intuit(60));
}

/// A client that cannot say what time Intuit answered, like a replayed
/// fixture.
struct NoServerClock(MockQbo);

impl QboClient for NoServerClock {
    fn query_page(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<ReadPage, QboError> {
        let page = self
            .0
            .query_page(realm, entity_type, updated, start_position, max_results)?;
        Ok(ReadPage {
            server_time: None,
            ..page
        })
    }

    fn cdc_page(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<ReadPage, QboError> {
        let page = self.0.cdc_page(realm, entity_types, changed_since)?;
        Ok(ReadPage {
            server_time: None,
            ..page
        })
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        self.0.create(realm, entity_type, payload, request_id)
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
        self.0.update(
            realm,
            entity_type,
            qbo_id,
            payload,
            base_sync_token,
            request_id,
        )
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.0.find_by_name(realm, entity_type, name)
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        self.0.fetch(realm, entity_type, qbo_id)
    }
}

#[test]
fn without_intuits_clock_an_empty_page_never_moves_the_cursor() {
    // No server time and no records: nothing proves any moment was covered,
    // and the Mac's clock is not evidence. The cursor stays put.
    let store = store();
    store
        .save_cursor(&SyncCursor {
            realm_id: realm(),
            entity_type: EntityType::Invoice,
            last_cdc_cursor: Some(intuit(0)),
            last_full_sweep: Some(intuit(0)),
        })
        .unwrap();
    let mut driver = SyncDriver::new(
        NoServerClock(MockQbo::new(intuit(60))),
        options(),
        intuit(0),
    );

    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(240))
        .unwrap();
    assert_eq!(cdc_cursor(&store), intuit(0));

    // A page with records cursors on the newest of them instead.
    driver
        .client_mut()
        .0
        .seed(&realm(), invoice("1", 0, intuit(45)));
    driver
        .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(300))
        .unwrap();
    assert_eq!(cdc_cursor(&store), intuit(45));
    assert_eq!(held_version(&store, "1").as_deref(), Some("0"));
}

// ---------------------------------------------------------------------------
// Idempotent upserts
// ---------------------------------------------------------------------------

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

#[test]
fn a_late_re_delivery_of_an_older_version_never_rolls_a_record_back() {
    let store = store();
    let newer = mirrored(invoice("1", 3, intuit(20)));
    let older = mirrored(invoice("1", 2, intuit(10)));

    let first = store
        .apply_changes(&realm(), std::slice::from_ref(&newer), None, intuit(20))
        .unwrap();
    assert_eq!(first.mirrored, 1);

    let again = store
        .apply_changes(&realm(), &[newer, older], None, intuit(21))
        .unwrap();
    assert_eq!(again.mirrored, 0);
    assert_eq!(again.already_held, 2);
    assert_eq!(held_version(&store, "1").as_deref(), Some("3"));
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

/// One step of a session: Intuit's clock moves, some invoices are edited,
/// and the Mac polls reading a clock off Intuit's by `skew_minutes`.
#[derive(Clone, Debug)]
struct Step {
    /// Up to three days, so a step can be a laptop asleep over a weekend.
    advance_minutes: i64,
    /// `(invoice, seconds before Intuit's now it was stamped)`. The lag is
    /// a write still committing when the last poll was answered, and stays
    /// inside the overlap, as §4.3 assumes.
    edits: Vec<(u8, i64)>,
    skew_minutes: i64,
}

fn step() -> impl Strategy<Value = Step> {
    (
        prop_oneof![4 => 0i64..30, 1 => 30i64..(60 * 24 * 3)],
        prop::collection::vec((0u8..6, 0i64..240), 0..5),
        -180i64..=180,
    )
        .prop_map(|(advance_minutes, edits, skew_minutes)| Step {
            advance_minutes,
            edits,
            skew_minutes,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Whatever the Mac's clock says, however long it sleeps, and whether
    /// CDC truncates or not: after every poll the replica holds the latest
    /// version of every invoice Intuit stamped before it answered, and one
    /// more poll brings in the rest. Nothing is ever stepped over by a
    /// cursor advance.
    #[test]
    fn no_edit_is_ever_skipped_across_a_cursor_advance(
        steps in prop::collection::vec(step(), 1..25),
        initial_skew in -180i64..=180,
        cdc_cap in prop_oneof![Just(1000usize), 2usize..5],
    ) {
        let mut qbo = MockQbo::new(intuit(0));
        qbo.set_cdc_cap(cdc_cap);
        let store = store();
        let mut driver = SyncDriver::new(
            qbo,
            SyncOptions {
                cdc_response_cap: cdc_cap,
                page_size: 3,
                ..options()
            },
            intuit(0),
        );
        driver
            .sync_entity_type(&store, &realm(), EntityType::Invoice, intuit(initial_skew))
            .unwrap();

        // Per invoice: (version, stamp) as Intuit holds it.
        let mut latest: [Option<(u32, DateTime<Utc>)>; 6] = [None; 6];
        let mut now = intuit(0);
        for step in &steps {
            now += Duration::minutes(step.advance_minutes);
            driver.client_mut().advance(now);
            for &(id, lag) in &step.edits {
                let slot = &mut latest[usize::from(id)];
                let version = slot.map_or(0, |(version, _)| version + 1);
                // Never stamped before the version it replaces.
                let stamp = slot.map_or(now - Duration::seconds(lag), |(_, previous)| {
                    (now - Duration::seconds(lag)).max(previous)
                });
                *slot = Some((version, stamp));
                driver
                    .client_mut()
                    .seed(&realm(), invoice(&id.to_string(), version, stamp));
            }

            let mac_now = now + Duration::minutes(step.skew_minutes);
            driver
                .sync_entity_type(&store, &realm(), EntityType::Invoice, mac_now)
                .unwrap();

            for (id, held) in latest.iter().enumerate() {
                if let Some((version, stamp)) = held {
                    if *stamp < now {
                        prop_assert_eq!(
                            held_version(&store, &id.to_string()),
                            Some(version.to_string()),
                            "invoice {} after a poll at {}", id, now
                        );
                    }
                }
            }
        }

        // One more poll a second later, and nothing at all is outstanding.
        now += Duration::seconds(1);
        driver.client_mut().advance(now);
        driver
            .sync_entity_type(&store, &realm(), EntityType::Invoice, now)
            .unwrap();
        for (id, held) in latest.iter().enumerate() {
            if let Some((version, _)) = held {
                prop_assert_eq!(
                    held_version(&store, &id.to_string()),
                    Some(version.to_string())
                );
            }
        }
    }
}
