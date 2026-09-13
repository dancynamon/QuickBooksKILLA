//! Record/replay HTTP fixtures for the QBO API, so the test suite — and
//! `qbo-local replay` — run fully offline. `DESIGN.md` §10, `HANDOFF.md` §2.6.
//!
//! [`RecordingQbo`] wraps any [`QboClient`] and writes every response it sees
//! to a directory, one file per call plus a `manifest.json` listing every call
//! in order. [`FixtureQbo`] is the other half: it implements [`QboClient`] by
//! reading those same files back, keyed exactly the way [`RecordingQbo`] wrote
//! them so the two always agree on where a given call's answer lives.
//!
//! **Fixtures are read-only.** `create` and `update` are writes; recording
//! passes them straight through to the wrapped client without capturing
//! anything (M0 never issues one during a recording session — see
//! `DESIGN.md` §8), and replaying one is refused outright rather than
//! silently doing nothing, so a write mistakenly issued against a fixture
//! fails loudly instead of vanishing.
//!
//! A fixture directory recorded here is not the one that gets committed.
//! `HANDOFF.md` §2.6 is explicit: only a *scrubbed* copy — produced by
//! `tools/scrub-fixtures.py`, never a hand edit of a recording — ever reaches
//! `git add`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::client::{
    EntityPayload, IndexEntry, QboClient, QboError, ReportName, ReportParams, UpdatedRange,
};
use crate::domain::{EntityType, RealmId};

// ---------------------------------------------------------------------------
// Fixture keys — shared by RecordingQbo (writer) and FixtureQbo (reader) so
// the two can never disagree about where a call's answer lives.
// ---------------------------------------------------------------------------

/// A timestamp turned into something safe to put in a filename. Not required
/// to be parseable back — both sides only ever compare it for equality.
fn ts_key(at: DateTime<Utc>) -> String {
    at.to_rfc3339().replace([':', '+'], "-")
}

fn range_bounds(updated: Option<UpdatedRange>) -> (String, String) {
    match updated {
        None => ("none".to_string(), "none".to_string()),
        Some(range) => (ts_key(range.from), ts_key(range.to)),
    }
}

fn query_path(
    entity_type: EntityType,
    updated: Option<UpdatedRange>,
    start_position: usize,
) -> PathBuf {
    let (from, to) = range_bounds(updated);
    PathBuf::from(entity_type.as_str()).join(format!("query-{from}-{to}-{start_position}.json"))
}

fn cdc_path(changed_since: DateTime<Utc>, entity_types: &[EntityType]) -> PathBuf {
    let names: Vec<&str> = entity_types.iter().map(|e| e.as_str()).collect();
    PathBuf::from("cdc").join(format!(
        "{}-{}.json",
        ts_key(changed_since),
        names.join("+")
    ))
}

fn fetch_path(entity_type: EntityType, qbo_id: &str) -> PathBuf {
    PathBuf::from(entity_type.as_str()).join(format!("fetch-{qbo_id}.json"))
}

fn index_path(entity_type: EntityType, start_position: usize) -> PathBuf {
    PathBuf::from(entity_type.as_str()).join(format!("index-{start_position}.json"))
}

fn find_path(entity_type: EntityType, name: &str) -> PathBuf {
    PathBuf::from(entity_type.as_str()).join(format!("find-{}.json", fnv1a_hex(name)))
}

/// `reports/<Name>-<start>-<end>-<accounting_method>-<date_macro>-<aging_method>.json`.
/// Human-readable rather than hashed — unlike [`find_path`]'s free-text
/// customer name, every field of [`ReportParams`] is already a short date or
/// enum-shaped string, so there is no reason to obscure it behind a digest.
/// That also means a committed synthetic fixture's filename can be written by
/// hand rather than only ever produced by a recording run.
fn report_path(name: ReportName, params: &ReportParams) -> PathBuf {
    fn field(value: &Option<impl ToString>) -> String {
        value
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "none".to_string())
    }
    PathBuf::from("reports").join(format!(
        "{}-{}-{}-{}-{}-{}.json",
        name.as_str(),
        field(&params.start_date),
        field(&params.end_date),
        field(&params.accounting_method),
        field(&params.date_macro),
        field(&params.aging_method),
    ))
}

/// FNV-1a, chosen over `DefaultHasher` because it is a fixed, documented
/// algorithm rather than "whatever the standard library's SipHash happens to
/// do this compiler version" — a fixture filename must mean the same thing
/// however this crate was built.
fn fnv1a_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Payload <-> JSON, independent of serde on the client types themselves —
// EntityPayload and IndexEntry stay free of a wire format opinion; this
// module owns fixture serialization end to end.
// ---------------------------------------------------------------------------

fn corrupt(rel: &Path, detail: &str) -> QboError {
    QboError::Network(format!("corrupt fixture {}: {detail}", rel.display()))
}

fn missing_fixture(rel: &Path) -> QboError {
    QboError::Network(format!("no fixture for {}", rel.display()))
}

fn payload_to_json(payload: &EntityPayload) -> Value {
    json!({
        "entity_type": payload.entity_type.as_str(),
        "qbo_id": payload.qbo_id,
        "sync_token": payload.sync_token,
        "last_updated_utc": payload.last_updated_utc.to_rfc3339(),
        "is_deleted": payload.is_deleted,
        "raw_json": payload.raw_json,
    })
}

fn payload_from_json(rel: &Path, value: &Value) -> Result<EntityPayload, QboError> {
    let obj = value
        .as_object()
        .ok_or_else(|| corrupt(rel, "expected an object"))?;
    let entity_type = obj
        .get("entity_type")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing entity_type"))?;
    let entity_type =
        EntityType::parse(entity_type).map_err(|e| corrupt(rel, &format!("entity_type: {e}")))?;
    let qbo_id = obj
        .get("qbo_id")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing qbo_id"))?
        .to_string();
    let sync_token = obj
        .get("sync_token")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing sync_token"))?
        .to_string();
    let raw_timestamp = obj
        .get("last_updated_utc")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing last_updated_utc"))?;
    let last_updated_utc = DateTime::parse_from_rfc3339(raw_timestamp)
        .map_err(|e| corrupt(rel, &format!("bad last_updated_utc: {e}")))?
        .with_timezone(&Utc);
    let is_deleted = obj
        .get("is_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw_json = obj.get("raw_json").cloned().unwrap_or(Value::Null);
    Ok(EntityPayload {
        entity_type,
        qbo_id,
        sync_token,
        last_updated_utc,
        is_deleted,
        raw_json,
    })
}

fn index_entry_to_json(entry: &IndexEntry) -> Value {
    json!({
        "qbo_id": entry.qbo_id,
        "last_updated_utc": entry.last_updated_utc.to_rfc3339(),
        "is_deleted": entry.is_deleted,
    })
}

fn index_entry_from_json(rel: &Path, value: &Value) -> Result<IndexEntry, QboError> {
    let obj = value
        .as_object()
        .ok_or_else(|| corrupt(rel, "expected an object"))?;
    let qbo_id = obj
        .get("qbo_id")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing qbo_id"))?
        .to_string();
    let raw_timestamp = obj
        .get("last_updated_utc")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(rel, "missing last_updated_utc"))?;
    let last_updated_utc = DateTime::parse_from_rfc3339(raw_timestamp)
        .map_err(|e| corrupt(rel, &format!("bad last_updated_utc: {e}")))?
        .with_timezone(&Utc);
    let is_deleted = obj
        .get("is_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(IndexEntry {
        qbo_id,
        last_updated_utc,
        is_deleted,
    })
}

/// `{"error": "..."}` — a call that reached the wrapped client and failed.
/// The message round-trips through [`QboError`]'s own variants where it can,
/// so a replayed fixture raises the same *kind* of error the recording saw,
/// not just a generic network failure standing in for it.
fn error_to_json(error: &QboError) -> Value {
    let message = match error {
        QboError::StaleSyncToken => "StaleSyncToken".to_string(),
        QboError::RateLimited => "RateLimited".to_string(),
        QboError::NotFound => "NotFound".to_string(),
        QboError::Validation(detail) => format!("Validation: {detail}"),
        QboError::Network(detail) => format!("Network: {detail}"),
    };
    json!({ "error": message })
}

fn error_from_message(message: &str) -> QboError {
    if message == "StaleSyncToken" {
        QboError::StaleSyncToken
    } else if message == "RateLimited" {
        QboError::RateLimited
    } else if message == "NotFound" {
        QboError::NotFound
    } else if let Some(detail) = message.strip_prefix("Validation: ") {
        QboError::Validation(detail.to_string())
    } else if let Some(detail) = message.strip_prefix("Network: ") {
        QboError::Network(detail.to_string())
    } else {
        QboError::Network(format!("recorded error not recognised: {message}"))
    }
}

/// `Some` only for an object shaped exactly like [`error_to_json`]'s output —
/// a `null` (a fetch/find that legitimately found nothing) or any other shape
/// is a successful response, not a recorded error.
fn as_recorded_error(value: &Value) -> Option<QboError> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    match obj.get("error") {
        Some(Value::String(message)) => Some(error_from_message(message)),
        _ => None,
    }
}

fn read_fixture(dir: &Path, rel: &Path) -> Result<Value, QboError> {
    let full = dir.join(rel);
    let text = fs::read_to_string(&full).map_err(|_| missing_fixture(rel))?;
    serde_json::from_str(&text).map_err(|e| corrupt(rel, &e.to_string()))
}

fn io_error(rel: &Path, error: io::Error) -> QboError {
    QboError::Network(format!(
        "fixture io error writing {}: {error}",
        rel.display()
    ))
}

fn rel_to_string(rel: &Path) -> String {
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// RecordingQbo
// ---------------------------------------------------------------------------

/// One row of `manifest.json` — every call a recording session made, in the
/// order it made them, with the parameters and the response file that holds
/// the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub call: String,
    pub entity_type: Option<String>,
    pub params: Value,
    pub response_file: String,
}

/// Wraps any [`QboClient`] and writes every response it returns to `dir`,
/// keyed so [`FixtureQbo`] can find it again by the same call.
///
/// Errors are recorded too — as `{"error": "..."}` — and always re-raised: a
/// recording session must behave exactly like the client it wraps, never
/// silently swallow a failure to keep writing fixtures.
pub struct RecordingQbo<C: QboClient> {
    inner: C,
    dir: PathBuf,
    manifest: Vec<ManifestEntry>,
}

impl<C: QboClient> RecordingQbo<C> {
    pub fn new(inner: C, dir: impl Into<PathBuf>) -> Self {
        RecordingQbo {
            inner,
            dir: dir.into(),
            manifest: Vec::new(),
        }
    }

    /// Every call recorded so far, in order.
    pub fn manifest(&self) -> &[ManifestEntry] {
        &self.manifest
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn into_inner(self) -> C {
        self.inner
    }

    fn record(
        &mut self,
        call: &str,
        entity_type: Option<EntityType>,
        params: Value,
        rel: PathBuf,
        body: Value,
    ) -> Result<(), QboError> {
        let full = self.dir.join(&rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(&rel, e))?;
        }
        let text = serde_json::to_string_pretty(&body)
            .map_err(|e| QboError::Network(format!("encoding fixture {}: {e}", rel.display())))?;
        fs::write(&full, text).map_err(|e| io_error(&rel, e))?;

        self.manifest.push(ManifestEntry {
            call: call.to_string(),
            entity_type: entity_type.map(|e| e.as_str().to_string()),
            params,
            response_file: rel_to_string(&rel),
        });
        self.write_manifest()
    }

    fn write_manifest(&self) -> Result<(), QboError> {
        fs::create_dir_all(&self.dir).map_err(|e| io_error(Path::new("manifest.json"), e))?;
        let text = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| QboError::Network(format!("encoding manifest.json: {e}")))?;
        fs::write(self.dir.join("manifest.json"), text)
            .map_err(|e| io_error(Path::new("manifest.json"), e))
    }
}

impl<C: QboClient> QboClient for RecordingQbo<C> {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let result = self
            .inner
            .query(realm, entity_type, updated, start_position, max_results);
        let rel = query_path(entity_type, updated, start_position);
        let params = json!({
            "realm": realm.as_str(),
            "entity_type": entity_type.as_str(),
            "updated": updated.map(|r| json!({"from": r.from.to_rfc3339(), "to": r.to.to_rfc3339()})),
            "start_position": start_position,
            "max_results": max_results,
        });
        let body = match &result {
            Ok(payloads) => Value::Array(payloads.iter().map(payload_to_json).collect()),
            Err(error) => error_to_json(error),
        };
        self.record("query", Some(entity_type), params, rel, body)?;
        result
    }

    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let result = self.inner.cdc(realm, entity_types, changed_since);
        let rel = cdc_path(changed_since, entity_types);
        let params = json!({
            "realm": realm.as_str(),
            "entity_types": entity_types.iter().map(|e| e.as_str()).collect::<Vec<_>>(),
            "changed_since": changed_since.to_rfc3339(),
        });
        let body = match &result {
            Ok(payloads) => Value::Array(payloads.iter().map(payload_to_json).collect()),
            Err(error) => error_to_json(error),
        };
        self.record("cdc", None, params, rel, body)?;
        result
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        // Writes are never captured: fixtures are a read-only recording of
        // what a sync saw (`FixtureQbo` below refuses to replay one), and M0
        // never issues a create through a client under recording.
        self.inner.create(realm, entity_type, payload, request_id)
    }

    fn update(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
        payload: &Value,
        base_sync_token: &str,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        self.inner.update(
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
        let result = self.inner.find_by_name(realm, entity_type, name);
        let rel = find_path(entity_type, name);
        let params = json!({
            "realm": realm.as_str(),
            "entity_type": entity_type.as_str(),
            "name": name,
        });
        let body = match &result {
            Ok(Some(payload)) => payload_to_json(payload),
            Ok(None) => Value::Null,
            Err(error) => error_to_json(error),
        };
        self.record("find_by_name", Some(entity_type), params, rel, body)?;
        result
    }

    fn index(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<IndexEntry>, QboError> {
        // Overridden rather than left to the trait default: the default
        // would call `self.query`, which on `Self` is the recording override
        // above — double-recording an index read as a query fixture instead
        // of the `index-<start>.json` key it belongs under. Forwarding to
        // `self.inner.index` keeps whatever the wrapped client actually does
        // (a real index projection, or the default's delegation to its own
        // `query`) invisible to the recording layer.
        let result = self
            .inner
            .index(realm, entity_type, start_position, max_results);
        let rel = index_path(entity_type, start_position);
        let params = json!({
            "realm": realm.as_str(),
            "entity_type": entity_type.as_str(),
            "start_position": start_position,
            "max_results": max_results,
        });
        let body = match &result {
            Ok(entries) => Value::Array(entries.iter().map(index_entry_to_json).collect()),
            Err(error) => error_to_json(error),
        };
        self.record("index", Some(entity_type), params, rel, body)?;
        result
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let result = self.inner.fetch(realm, entity_type, qbo_id);
        let rel = fetch_path(entity_type, qbo_id);
        let params = json!({
            "realm": realm.as_str(),
            "entity_type": entity_type.as_str(),
            "qbo_id": qbo_id,
        });
        let body = match &result {
            Ok(Some(payload)) => payload_to_json(payload),
            Ok(None) => Value::Null,
            Err(error) => error_to_json(error),
        };
        self.record("fetch", Some(entity_type), params, rel, body)?;
        result
    }

    /// Overridden, not left to the trait default: the default refuses
    /// outright, and a recording session needs the real Reports API call
    /// this wraps, captured the same way every other read is.
    fn report(
        &mut self,
        realm: &RealmId,
        name: ReportName,
        params: &ReportParams,
    ) -> Result<Value, QboError> {
        let result = self.inner.report(realm, name, params);
        let rel = report_path(name, params);
        let call_params = json!({
            "realm": realm.as_str(),
            "name": name.as_str(),
            "start_date": params.start_date.map(|d| d.to_string()),
            "end_date": params.end_date.map(|d| d.to_string()),
            "accounting_method": params.accounting_method,
            "date_macro": params.date_macro,
            "aging_method": params.aging_method,
        });
        let body = match &result {
            Ok(value) => value.clone(),
            Err(error) => error_to_json(error),
        };
        self.record("report", None, call_params, rel, body)?;
        result
    }
}

// ---------------------------------------------------------------------------
// FixtureQbo
// ---------------------------------------------------------------------------

/// Implements [`QboClient`] by replaying a directory [`RecordingQbo`] wrote.
///
/// A call with no matching fixture is [`QboError::Network`] naming the exact
/// key that was missing, so a test that needed a fixture nobody recorded
/// fails loudly with the file to go add, rather than hanging or returning an
/// empty page that looks like "nothing changed".
pub struct FixtureQbo {
    dir: PathBuf,
}

impl FixtureQbo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FixtureQbo { dir: dir.into() }
    }

    fn load(&self, rel: &Path) -> Result<Value, QboError> {
        read_fixture(&self.dir, rel)
    }
}

const READ_ONLY: &str = "fixtures are read-only";

impl QboClient for FixtureQbo {
    fn query(
        &mut self,
        _realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let rel = query_path(entity_type, updated, start_position);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        let array = value
            .as_array()
            .ok_or_else(|| corrupt(&rel, "expected an array"))?;
        let payloads = array
            .iter()
            .map(|v| payload_from_json(&rel, v))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(payloads.into_iter().take(max_results).collect())
    }

    fn cdc(
        &mut self,
        _realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let rel = cdc_path(changed_since, entity_types);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        let array = value
            .as_array()
            .ok_or_else(|| corrupt(&rel, "expected an array"))?;
        array.iter().map(|v| payload_from_json(&rel, v)).collect()
    }

    fn create(
        &mut self,
        _realm: &RealmId,
        _entity_type: EntityType,
        _payload: &Value,
        _request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        Err(QboError::Validation(READ_ONLY.to_string()))
    }

    fn update(
        &mut self,
        _realm: &RealmId,
        _entity_type: EntityType,
        _qbo_id: &str,
        _payload: &Value,
        _base_sync_token: &str,
        _request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        Err(QboError::Validation(READ_ONLY.to_string()))
    }

    fn find_by_name(
        &mut self,
        _realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let rel = find_path(entity_type, name);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(payload_from_json(&rel, &value)?))
    }

    fn index(
        &mut self,
        _realm: &RealmId,
        entity_type: EntityType,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<IndexEntry>, QboError> {
        let rel = index_path(entity_type, start_position);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        let array = value
            .as_array()
            .ok_or_else(|| corrupt(&rel, "expected an array"))?;
        let entries = array
            .iter()
            .map(|v| index_entry_from_json(&rel, v))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries.into_iter().take(max_results).collect())
    }

    fn fetch(
        &mut self,
        _realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let rel = fetch_path(entity_type, qbo_id);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        if value.is_null() {
            return Ok(None);
        }
        Ok(Some(payload_from_json(&rel, &value)?))
    }

    /// Overridden, not left to the trait default: replaying a report fixture
    /// is exactly what `qbo-local report --replay` and the nightly parallel
    /// run's synthetic tests need, and the default refuses outright.
    fn report(
        &mut self,
        _realm: &RealmId,
        name: ReportName,
        params: &ReportParams,
    ) -> Result<Value, QboError> {
        let rel = report_path(name, params);
        let value = self.load(&rel)?;
        if let Some(error) = as_recorded_error(&value) {
            return Err(error);
        }
        Ok(value)
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockQbo;
    use crate::driver::{SyncDriver, SyncOptions};
    use crate::store::Store;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn payload(
        entity_type: EntityType,
        qbo_id: &str,
        raw_json: Value,
        at: DateTime<Utc>,
    ) -> EntityPayload {
        EntityPayload {
            entity_type,
            qbo_id: qbo_id.to_string(),
            sync_token: "0".to_string(),
            last_updated_utc: at,
            is_deleted: false,
            raw_json,
        }
    }

    /// Invented data, shaped after `HANDOFF.md` §2.6's rule that nothing
    /// committed may carry a real name — these never leave this process.
    fn seeded_mock(at: DateTime<Utc>) -> MockQbo {
        let mut mock = MockQbo::new(at);
        let realm = realm();
        mock.seed(
            &realm,
            payload(
                EntityType::Customer,
                "1",
                json!({ "Id": "1", "DisplayName": "BLUE HARBOR SWIM" }),
                at,
            ),
        );
        mock.seed(
            &realm,
            payload(
                EntityType::Customer,
                "2",
                json!({ "Id": "2", "DisplayName": "FAIRVIEW COUNTY PARKS & REC." }),
                at,
            ),
        );
        mock.seed(
            &realm,
            payload(
                EntityType::Item,
                "1",
                json!({ "Id": "1", "Name": "50-INCH RESCUE TUBE" }),
                at,
            ),
        );
        mock.seed(
            &realm,
            payload(
                EntityType::Invoice,
                "101",
                json!({
                    "Id": "101",
                    "DocNumber": "1001",
                    "CustomerRef": { "value": "1", "name": "BLUE HARBOR SWIM" },
                    "TotalAmt": 249.0,
                    "Line": [{ "Amount": 249.0, "DetailType": "SalesItemLineDetail" }],
                }),
                at,
            ),
        );
        mock.seed(
            &realm,
            payload(
                EntityType::Invoice,
                "102",
                json!({
                    "Id": "102",
                    "DocNumber": "1002",
                    "CustomerRef": { "value": "2", "name": "FAIRVIEW COUNTY PARKS & REC." },
                    "TotalAmt": 830.0,
                }),
                at,
            ),
        );
        mock
    }

    // -- path helpers ---------------------------------------------------

    #[test]
    fn query_path_is_stable_for_the_same_call() {
        let a = query_path(EntityType::Customer, None, 0);
        let b = query_path(EntityType::Customer, None, 0);
        assert_eq!(a, b);
        assert_eq!(a, PathBuf::from("Customer/query-none-none-0.json"));
    }

    #[test]
    fn query_path_differs_by_window_and_start() {
        let bounded = query_path(
            EntityType::Invoice,
            Some(UpdatedRange {
                from: now(),
                to: now(),
            }),
            0,
        );
        let unbounded = query_path(EntityType::Invoice, None, 0);
        let next_page = query_path(EntityType::Invoice, None, 1000);
        assert_ne!(bounded, unbounded);
        assert_ne!(unbounded, next_page);
    }

    #[test]
    fn find_path_hashes_the_name_deterministically() {
        let a = find_path(EntityType::Customer, "BLUE HARBOR SWIM");
        let b = find_path(EntityType::Customer, "BLUE HARBOR SWIM");
        let other = find_path(EntityType::Customer, "SOMEONE ELSE");
        assert_eq!(a, b);
        assert_ne!(a, other);
    }

    // -- report path --------------------------------------------------------

    #[test]
    fn report_path_is_human_readable_and_stable() {
        let params = ReportParams {
            start_date: None,
            end_date: Some(now().date_naive()),
            accounting_method: Some("Accrual".to_string()),
            date_macro: None,
            aging_method: None,
        };
        let path = report_path(ReportName::TrialBalance, &params);
        assert_eq!(
            path,
            PathBuf::from("reports/TrialBalance-none-2026-01-01-Accrual-none-none.json")
        );
        assert_eq!(path, report_path(ReportName::TrialBalance, &params));
    }

    #[test]
    fn report_path_differs_by_report_name_and_params() {
        let end_only = ReportParams {
            end_date: Some(now().date_naive()),
            ..ReportParams::default()
        };
        let tb = report_path(ReportName::TrialBalance, &end_only);
        let ar = report_path(ReportName::AgedReceivables, &end_only);
        let with_method = ReportParams {
            accounting_method: Some("Cash".to_string()),
            ..end_only.clone()
        };
        assert_ne!(tb, ar);
        assert_ne!(tb, report_path(ReportName::TrialBalance, &with_method));
    }

    // -- report record/replay ------------------------------------------------

    #[test]
    fn recording_a_report_writes_a_readable_fixture_and_replays_it() {
        let directory = tempfile::tempdir().unwrap();
        let realm = realm();
        let params = ReportParams {
            start_date: None,
            end_date: Some(now().date_naive()),
            accounting_method: Some("Accrual".to_string()),
            date_macro: None,
            aging_method: None,
        };
        let canned = seeded_mock(now());
        let mut recorder = RecordingQbo::new(canned, directory.path());

        // The wrapped MockQbo has no report support of its own (the trait's
        // default), so recording captures the refusal — proving errors round
        // trip through a report fixture exactly as they do for every other
        // call.
        let recorded = recorder.report(&realm, ReportName::TrialBalance, &params);
        assert!(matches!(recorded, Err(QboError::Validation(_))));

        let rel = report_path(ReportName::TrialBalance, &params);
        assert!(directory.path().join(&rel).exists());

        let mut fixture = FixtureQbo::new(directory.path());
        let replayed = fixture.report(&realm, ReportName::TrialBalance, &params);
        assert_eq!(recorded, replayed);
    }

    #[test]
    fn replaying_a_report_fixture_returns_the_recorded_json_whole() {
        let directory = tempfile::tempdir().unwrap();
        let realm = realm();
        let params = ReportParams {
            end_date: Some(now().date_naive()),
            accounting_method: Some("Accrual".to_string()),
            ..ReportParams::default()
        };
        let rel = report_path(ReportName::TrialBalance, &params);
        let full = directory.path().join(&rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        let body = json!({
            "Header": { "ReportName": "TrialBalance" },
            "Rows": { "Row": [] }
        });
        fs::write(&full, serde_json::to_string(&body).unwrap()).unwrap();

        let mut fixture = FixtureQbo::new(directory.path());
        let replayed = fixture
            .report(&realm, ReportName::TrialBalance, &params)
            .unwrap();
        assert_eq!(replayed, body);
    }

    // -- FixtureQbo -------------------------------------------------------

    #[test]
    fn a_call_with_no_fixture_names_the_missing_key() {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = FixtureQbo::new(directory.path());
        let error = fixture
            .query(&realm(), EntityType::Customer, None, 0, 1000)
            .unwrap_err();
        match error {
            QboError::Network(message) => {
                assert!(message.contains("Customer"), "{message}");
                assert!(message.contains("query-none-none-0.json"), "{message}");
            }
            other => panic!("expected QboError::Network, got {other:?}"),
        }
    }

    #[test]
    fn writes_are_refused_rather_than_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = FixtureQbo::new(directory.path());
        let create = fixture.create(&realm(), EntityType::Customer, &json!({}), Uuid::now_v7());
        let update = fixture.update(
            &realm(),
            EntityType::Invoice,
            "1",
            &json!({}),
            "0",
            Uuid::now_v7(),
        );
        assert_eq!(create, Err(QboError::Validation(READ_ONLY.to_string())));
        assert_eq!(update, Err(QboError::Validation(READ_ONLY.to_string())));
    }

    // -- RecordingQbo -----------------------------------------------------

    #[test]
    fn recording_writes_one_file_per_call_and_a_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let mut recorder = RecordingQbo::new(seeded_mock(now()), directory.path());

        recorder
            .query(&realm(), EntityType::Customer, None, 0, 1000)
            .unwrap();
        recorder
            .find_by_name(&realm(), EntityType::Customer, "BLUE HARBOR SWIM")
            .unwrap();
        recorder.fetch(&realm(), EntityType::Item, "1").unwrap();
        recorder
            .index(&realm(), EntityType::Invoice, 0, 1000)
            .unwrap();
        recorder
            .cdc(&realm(), &[EntityType::Invoice], now())
            .unwrap();

        assert_eq!(recorder.manifest().len(), 5);
        for entry in recorder.manifest() {
            assert!(
                directory.path().join(&entry.response_file).exists(),
                "missing recorded file {}",
                entry.response_file
            );
        }
        assert!(directory.path().join("manifest.json").exists());

        // The query fixture round-trips the two customers seeded above.
        let fixture = FixtureQbo::new(directory.path());
        let mut fixture = fixture;
        let replayed = fixture
            .query(&realm(), EntityType::Customer, None, 0, 1000)
            .unwrap();
        assert_eq!(replayed.len(), 2);
    }

    #[test]
    fn recording_captures_and_still_raises_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut mock = seeded_mock(now());
        mock.fail_next(QboError::RateLimited);
        let mut recorder = RecordingQbo::new(mock, directory.path());

        let result = recorder.query(&realm(), EntityType::Customer, None, 0, 1000);
        assert_eq!(result, Err(QboError::RateLimited));

        let rel = query_path(EntityType::Customer, None, 0);
        let raw = fs::read_to_string(directory.path().join(&rel)).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["error"], "RateLimited");

        // And replaying the same fixture raises the same error.
        let mut fixture = FixtureQbo::new(directory.path());
        let replayed = fixture.query(&realm(), EntityType::Customer, None, 0, 1000);
        assert_eq!(replayed, Err(QboError::RateLimited));
    }

    // -- the property: record, then replay, matches a direct sync ---------

    #[test]
    fn replaying_a_recording_reproduces_a_direct_sync_exactly() {
        let now = now();
        let realm = realm();
        let entity_types = [EntityType::Customer, EntityType::Item, EntityType::Invoice];
        let directory = tempfile::tempdir().unwrap();

        // Record.
        let recorder = RecordingQbo::new(seeded_mock(now), directory.path());
        let mut record_driver = SyncDriver::new(recorder, SyncOptions::default(), now);
        let record_store = Store::open_in_memory().unwrap();
        record_store
            .register_realm(&realm, "recording", now)
            .unwrap();
        record_driver
            .sync_realm(&record_store, &realm, &entity_types, now)
            .unwrap();

        // A direct sync from the same seeded data, never touching a fixture.
        let mut direct_driver = SyncDriver::new(seeded_mock(now), SyncOptions::default(), now);
        let direct_store = Store::open_in_memory().unwrap();
        direct_store.register_realm(&realm, "direct", now).unwrap();
        direct_driver
            .sync_realm(&direct_store, &realm, &entity_types, now)
            .unwrap();

        // Replay, from nothing but what recording wrote to disk.
        let fixture = FixtureQbo::new(directory.path());
        let mut replay_driver = SyncDriver::new(fixture, SyncOptions::default(), now);
        let replay_store = Store::open_in_memory().unwrap();
        replay_store.register_realm(&realm, "replay", now).unwrap();
        replay_driver
            .sync_realm(&replay_store, &realm, &entity_types, now)
            .unwrap();

        for &entity_type in &entity_types {
            assert_eq!(
                direct_store.count_entities(&realm, entity_type).unwrap(),
                replay_store.count_entities(&realm, entity_type).unwrap(),
                "count mismatch for {entity_type:?}"
            );
        }

        for (entity_type, qbo_id) in [
            (EntityType::Customer, "1"),
            (EntityType::Customer, "2"),
            (EntityType::Item, "1"),
            (EntityType::Invoice, "101"),
            (EntityType::Invoice, "102"),
        ] {
            let direct = direct_store
                .get_entity(&realm, entity_type, qbo_id)
                .unwrap()
                .unwrap_or_else(|| panic!("direct sync missing {entity_type:?}/{qbo_id}"));
            let replayed = replay_store
                .get_entity(&realm, entity_type, qbo_id)
                .unwrap()
                .unwrap_or_else(|| panic!("replay missing {entity_type:?}/{qbo_id}"));
            assert_eq!(
                direct.raw_json, replayed.raw_json,
                "raw JSON mismatch for {entity_type:?}/{qbo_id}"
            );
            assert_eq!(direct.sync_token, replayed.sync_token);
            assert_eq!(direct.is_deleted, replayed.is_deleted);
        }
    }
}
