//! The live HTTP transport: [`HttpQboClient`] implements [`crate::client::QboClient`]
//! against Intuit's REST API. `HANDOFF.md` §2.3.
//!
//! Everything above the [`crate::client::QboClient`] trait — [`crate::driver::SyncDriver`],
//! [`crate::reconcile::Reconciler`], [`crate::worker`] — is exercised against
//! [`crate::client::MockQbo`] and never changes when this module does; the trait is
//! the seam. This module's own tests run against a hand-rolled fake HTTP
//! server (`tests/http_client.rs`) rather than the real Intuit sandbox, so the
//! whole suite stays offline (`HANDOFF.md` §6, `DESIGN.md` §0).
//!
//! No async runtime: `ureq` is a blocking client, which is enough for a sync
//! loop that issues one request at a time and never needs to overlap I/O with
//! other work. Pulling in an async runtime for that would be a second
//! concurrency model layered under one this crate does not otherwise have.
//!
//! ## Mapping HTTP onto [`QboClient`]
//!
//! - `query` builds `SELECT * FROM <Entity>` with an optional
//!   `WHERE MetaData.LastUpdatedTime >= x AND < y` clause, followed by
//!   `ORDERBY MetaData.LastUpdatedTime STARTPOSITION n MAXRESULTS m`, against
//!   `GET /v3/company/<realm>/query`. QBO's `STARTPOSITION` is 1-based; the
//!   trait's `start_position` is 0-based (`crate::client::QboClient::query`
//!   documents zero as "the first page"), so every call adds one before it
//!   goes on the wire.
//! - `cdc` calls `GET /v3/company/<realm>/cdc?entities=A,B&changedSince=...`.
//!   An entry whose `status` is `"Deleted"` becomes `is_deleted = true`.
//! - `create`/`update` `POST /v3/company/<realm>/<entity>`, entity name
//!   lowercased for the URL. `update` folds `Id`, the caller's
//!   `base_sync_token` as `SyncToken`, and `sparse: false` into the body —
//!   QBO applies a sparse update only to the fields present, and this client
//!   always sends the full record, so `sparse` is always `false`.
//! - `find_by_name` queries `WHERE <field> = '<escaped>'`, escaping a
//!   embedded quote by doubling it, the same rule QBO's own query language
//!   uses for string literals.
//! - `index` projects `SELECT Id, MetaData FROM <Entity>`, paged, rather than
//!   pulling whole payloads — the reconciliation sweep's first step only
//!   needs ids and timestamps (`crate::reconcile`).
//! - `fetch` is a plain `GET /v3/company/<realm>/<entity>/<id>`.
//!
//! ## Retries and errors
//!
//! Every call spends one [`RealmLimiter`] token from the `General` bucket
//! before it does anything else; a refusal becomes [`QboError::RateLimited`]
//! without an HTTP request ever going out — a local exhaustion behaves like a
//! 429 in every way that matters to a caller (`DESIGN.md` §4.4).
//!
//! HTTP status is mapped exactly as `HANDOFF.md` §2.3 specifies: `429` →
//! [`QboError::RateLimited`]; `5xx` and a transport failure (no response at
//! all) → [`QboError::Network`]; `401` refreshes the token once, reactively,
//! and retries the same request once — a second `401` becomes
//! [`QboError::Network`], never a silent loop; `404` → [`QboError::NotFound`];
//! a `4xx` whose body carries `Fault.Error[].code == "5010"` (QBO's Stale
//! Object Error) → [`QboError::StaleSyncToken`]; every other `4xx` →
//! [`QboError::Validation`]. The `RequestId` a caller passes in is sent
//! unchanged on every attempt and is never regenerated inside this module —
//! retrying a `create` with a fresh id is exactly the duplicate-master bug
//! `HANDOFF.md` §6 describes.
//!
//! ## Token rotation
//!
//! Before every call, [`TokenSource::current`] checks
//! [`crate::auth::TokenSet::needs_refresh`]; if it is due, this module
//! refreshes proactively, rotates [`crate::auth::TokenGenerations`], saves
//! through the configured [`crate::auth::TokenStore`], and appends a
//! fingerprint-only [`crate::auth::RotationLogEntry`] to the JSONL rotation
//! log — the same sequence a reactive 401 refresh runs, just triggered by the
//! clock instead of a response.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::auth::{AuthError, RotationLogEntry, TokenGenerations, TokenSet, TokenStore};
use crate::client::{
    unique_name_field, EntityPayload, IndexEntry, QboClient, QboError, UpdatedRange,
};
use crate::domain::{EntityType, RealmId};
use crate::oauth::{self, OAuthConfig};
use crate::ratelimit::{BucketClass, RealmLimiter, RealmLimits};

/// Intuit's production API host.
pub const PRODUCTION_BASE_URL: &str = "https://quickbooks.api.intuit.com";
/// Intuit's sandbox API host.
pub const SANDBOX_BASE_URL: &str = "https://sandbox-quickbooks.api.intuit.com";

/// The `minorversion` query parameter sent on every request. QBO ties
/// behavioural changes to this number rather than to the URL path; 75 is
/// current as of this writing (`HANDOFF.md` §4 — verify against Intuit's
/// release notes if entity shapes drift).
pub const DEFAULT_MINOR_VERSION: u32 = 75;

// ---------------------------------------------------------------------------
// Token source
// ---------------------------------------------------------------------------

/// Loads, refreshes and persists one realm's [`TokenGenerations`].
///
/// Kept separate from [`HttpQboClient`] so the refresh-and-save sequence has
/// one owner: the client decides *when* to call it (proactively on
/// [`TokenSet::needs_refresh`], reactively on a 401) and appends the rotation
/// log entry, but never touches [`TokenStore`] directly.
pub struct TokenSource {
    store: Box<dyn TokenStore>,
    config: OAuthConfig,
    realm: RealmId,
    /// Loaded lazily, from `store`, on first use — a `TokenSource` built for
    /// a realm the store has never seen must not fail until it is actually
    /// asked for a token.
    generations: Option<TokenGenerations>,
}

impl TokenSource {
    pub fn new(store: Box<dyn TokenStore>, config: OAuthConfig, realm: RealmId) -> Self {
        TokenSource {
            store,
            config,
            realm,
            generations: None,
        }
    }

    fn loaded(&mut self) -> Result<&mut TokenGenerations, QboError> {
        if self.generations.is_none() {
            let found = self
                .store
                .load(&self.realm)
                .map_err(auth_err)?
                .ok_or_else(|| {
                    QboError::Network(format!(
                        "no stored tokens for realm {} — run `qbo-local auth --realm {}` first",
                        self.realm, self.realm
                    ))
                })?;
            self.generations = Some(found);
        }
        Ok(self.generations.as_mut().expect("just populated"))
    }

    /// The newest usable generation, without refreshing it.
    pub fn current(&mut self, now: DateTime<Utc>) -> Result<TokenSet, QboError> {
        let generations = self.loaded()?;
        generations.newest_usable(now).cloned().map_err(auth_err)
    }

    /// Refresh against Intuit, rotate the generation, persist it, and return
    /// the fingerprints of the outgoing and incoming refresh tokens (for the
    /// rotation log) plus the new access token.
    pub fn refresh(
        &mut self,
        agent: &ureq::Agent,
        now: DateTime<Utc>,
    ) -> Result<RotationOutcome, QboError> {
        let current = self.current(now)?;
        let outgoing_fingerprint = current.fingerprint();
        let next = oauth::refresh(&self.config, &current.refresh_token, agent)
            .map_err(|error| QboError::Network(format!("token refresh: {error}")))?;
        let incoming_fingerprint = next.fingerprint();
        let access_token = next.access_token.clone();

        let generations = self.loaded()?;
        generations.rotate(next);
        let snapshot = generations.clone();
        self.store.save(&snapshot).map_err(auth_err)?;

        Ok(RotationOutcome {
            outgoing_fingerprint,
            incoming_fingerprint,
            access_token,
        })
    }
}

/// What a refresh produced, for the caller to both use and log.
pub struct RotationOutcome {
    pub outgoing_fingerprint: String,
    pub incoming_fingerprint: String,
    pub access_token: String,
}

fn auth_err(error: AuthError) -> QboError {
    QboError::Network(format!("token store: {error}"))
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// [`QboClient`] implemented over real HTTP.
pub struct HttpQboClient {
    base: String,
    tokens: TokenSource,
    limiter: RealmLimiter,
    agent: ureq::Agent,
    minor_version: u32,
    rotation_log: PathBuf,
}

impl HttpQboClient {
    /// `base` is one of [`PRODUCTION_BASE_URL`] / [`SANDBOX_BASE_URL`] in
    /// production, or a fake server's `http://127.0.0.1:PORT` under test.
    pub fn new(
        base: impl Into<String>,
        tokens: TokenSource,
        rotation_log: impl Into<PathBuf>,
    ) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        HttpQboClient {
            base: base.into(),
            tokens,
            limiter: RealmLimiter::new(RealmLimits::default(), Utc::now()),
            agent,
            minor_version: DEFAULT_MINOR_VERSION,
            rotation_log: rotation_log.into(),
        }
    }

    pub fn with_minor_version(mut self, minor_version: u32) -> Self {
        self.minor_version = minor_version;
        self
    }

    fn minorversion_param(&self) -> (String, String) {
        ("minorversion".to_string(), self.minor_version.to_string())
    }

    fn entity_path(&self, realm: &RealmId, entity_type: EntityType) -> String {
        format!(
            "/v3/company/{}/{}",
            realm.as_str(),
            entity_type.as_str().to_ascii_lowercase()
        )
    }

    // -- token handling -----------------------------------------------------

    fn ensure_token(&mut self, now: DateTime<Utc>) -> Result<String, QboError> {
        let current = self.tokens.current(now)?;
        if current.needs_refresh(now) {
            let outcome = self.tokens.refresh(&self.agent, now)?;
            self.log_rotation(now, &outcome, "proactive");
            Ok(outcome.access_token)
        } else {
            Ok(current.access_token)
        }
    }

    fn reactive_refresh(&mut self, now: DateTime<Utc>) -> Result<String, QboError> {
        let outcome = self.tokens.refresh(&self.agent, now)?;
        self.log_rotation(now, &outcome, "reactive");
        Ok(outcome.access_token)
    }

    /// Best-effort: a rotation log write failing must not fail the request it
    /// rode in on. The log exists so a 3am breakage has a first thing to
    /// read (`DESIGN.md` §5.3), not as a transactional record the request
    /// depends on.
    fn log_rotation(&self, at: DateTime<Utc>, outcome: &RotationOutcome, kind: &str) {
        let entry = RotationLogEntry {
            at,
            realm_id: self.tokens.realm.clone(),
            outgoing: outcome.outgoing_fingerprint.clone(),
            incoming: outcome.incoming_fingerprint.clone(),
            outcome: kind.to_string(),
        };
        if let Err(error) = append_jsonl(&self.rotation_log, &entry) {
            eprintln!("qbo-local: failed to append rotation log entry: {error}");
        }
    }

    // -- request plumbing -----------------------------------------------------

    /// Spend a rate-limit token, run `body` (which may itself hit the
    /// network more than once, for the 401-retry case), then release the
    /// concurrency slot regardless of outcome.
    fn request_json(
        &mut self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, QboError> {
        let now = Utc::now();
        if !self.limiter.try_acquire(BucketClass::General, now) {
            return Err(QboError::RateLimited);
        }
        let outcome = self.request_json_inner(method, path, query, body);
        self.limiter.release();
        outcome
    }

    fn request_json_inner(
        &mut self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, QboError> {
        let now = Utc::now();
        let token = self.ensure_token(now)?;
        match self.attempt(method, path, query, body, &token) {
            Err(AttemptError::Unauthorized) => {
                let token = self.reactive_refresh(Utc::now())?;
                self.attempt(method, path, query, body, &token)
                    .map_err(AttemptError::into_qbo_error)
            }
            other => other.map_err(AttemptError::into_qbo_error),
        }
    }

    fn attempt(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        token: &str,
    ) -> Result<Value, AttemptError> {
        let url = format!("{}{}", self.base, path);
        let mut request = match method {
            Method::Get => self.agent.get(&url),
            Method::Post => self.agent.post(&url),
        };
        for (key, value) in query {
            request = request.query(key, value);
        }
        request = request
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json");

        let result = match body {
            Some(body) => request.send_json(body.clone()),
            None => request.call(),
        };

        match result {
            Ok(response) => response
                .into_json()
                .map_err(|error| AttemptError::Network(format!("decoding response: {error}"))),
            Err(ureq::Error::Status(code, response)) => {
                let fault: Value = response.into_json().unwrap_or(Value::Null);
                Err(classify_status(code, &fault))
            }
            Err(ureq::Error::Transport(transport)) => {
                Err(AttemptError::Network(transport.to_string()))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
}

/// What one HTTP attempt can come back as, before the 401 retry decides
/// whether it is final.
enum AttemptError {
    Unauthorized,
    RateLimited,
    NotFound,
    StaleSyncToken,
    Validation(String),
    Network(String),
}

impl AttemptError {
    fn into_qbo_error(self) -> QboError {
        match self {
            // A second 401 after the reactive refresh means the new token
            // still doesn't work — that is a network-shaped failure from the
            // caller's point of view, not something retrying again would fix.
            AttemptError::Unauthorized => {
                QboError::Network("401 Unauthorized after token refresh".to_string())
            }
            AttemptError::RateLimited => QboError::RateLimited,
            AttemptError::NotFound => QboError::NotFound,
            AttemptError::StaleSyncToken => QboError::StaleSyncToken,
            AttemptError::Validation(detail) => QboError::Validation(detail),
            AttemptError::Network(detail) => QboError::Network(detail),
        }
    }
}

fn classify_status(code: u16, fault: &Value) -> AttemptError {
    match code {
        401 => AttemptError::Unauthorized,
        404 => AttemptError::NotFound,
        429 => AttemptError::RateLimited,
        500..=599 => AttemptError::Network(format!("http {code}: {}", fault_detail(fault))),
        _ if is_stale_sync_token(fault) => AttemptError::StaleSyncToken,
        _ => AttemptError::Validation(fault_detail(fault)),
    }
}

/// QBO's Stale Object Error, `Fault.Error[].code == "5010"` — the signal a
/// `SyncToken` sent with an update no longer matches the current one.
fn is_stale_sync_token(fault: &Value) -> bool {
    fault
        .pointer("/Fault/Error")
        .and_then(Value::as_array)
        .is_some_and(|errors| {
            errors
                .iter()
                .any(|error| error.get("code").and_then(Value::as_str) == Some("5010"))
        })
}

fn fault_detail(fault: &Value) -> String {
    let messages: Vec<String> = fault
        .pointer("/Fault/Error")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .map(|error| {
                    let message = error.get("Message").and_then(Value::as_str).unwrap_or("");
                    let detail = error.get("Detail").and_then(Value::as_str).unwrap_or("");
                    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
                    format!("{message} ({code}): {detail}")
                })
                .collect()
        })
        .unwrap_or_default();
    if messages.is_empty() {
        fault.to_string()
    } else {
        messages.join("; ")
    }
}

// ---------------------------------------------------------------------------
// QboClient
// ---------------------------------------------------------------------------

impl QboClient for HttpQboClient {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let query_string = build_select(entity_type, updated, start_position, max_results);
        let path = format!("/v3/company/{}/query", realm.as_str());
        let params = [
            ("query".to_string(), query_string),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Get, &path, &params, None)?;
        query_response_items(&value, entity_type)
            .iter()
            .map(|item| to_entity_payload(entity_type, item))
            .collect()
    }

    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let entities = entity_types
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let path = format!("/v3/company/{}/cdc", realm.as_str());
        let params = [
            ("entities".to_string(), entities),
            ("changedSince".to_string(), changed_since.to_rfc3339()),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Get, &path, &params, None)?;
        parse_cdc_response(&value)
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        let path = self.entity_path(realm, entity_type);
        let params = [
            ("requestid".to_string(), request_id.to_string()),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Post, &path, &params, Some(payload))?;
        parse_single_entity(&value, entity_type)
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
        let mut body = payload.clone();
        let object = body.as_object_mut().ok_or_else(|| {
            QboError::Validation("update payload must be a JSON object".to_string())
        })?;
        object.insert("Id".to_string(), Value::String(qbo_id.to_string()));
        object.insert(
            "SyncToken".to_string(),
            Value::String(base_sync_token.to_string()),
        );
        object.insert("sparse".to_string(), Value::Bool(false));

        let path = self.entity_path(realm, entity_type);
        let params = [
            ("requestid".to_string(), request_id.to_string()),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Post, &path, &params, Some(&body))?;
        parse_single_entity(&value, entity_type)
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let field = unique_name_field(entity_type).ok_or_else(|| {
            QboError::Validation(format!("{entity_type:?} has no unique name field"))
        })?;
        let escaped = name.replace('\'', "''");
        let query_string = format!(
            "SELECT * FROM {} WHERE {field} = '{escaped}'",
            entity_type.as_str()
        );
        let path = format!("/v3/company/{}/query", realm.as_str());
        let params = [
            ("query".to_string(), query_string),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Get, &path, &params, None)?;
        let mut items = query_response_items(&value, entity_type)
            .iter()
            .map(|item| to_entity_payload(entity_type, item))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items.pop())
    }

    fn index(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<IndexEntry>, QboError> {
        // A projected query — ids and timestamps only — rather than the
        // default `QboClient::index` (a full `query` with the payload
        // thrown away), so the reconciliation sweep's first step doesn't
        // pull whole payloads over the wire just to discard them.
        let query_string = format!(
            "SELECT Id, MetaData FROM {} ORDERBY MetaData.LastUpdatedTime STARTPOSITION {} MAXRESULTS {max_results}",
            entity_type.as_str(),
            start_position + 1,
        );
        let path = format!("/v3/company/{}/query", realm.as_str());
        let params = [
            ("query".to_string(), query_string),
            self.minorversion_param(),
        ];
        let value = self.request_json(Method::Get, &path, &params, None)?;
        query_response_items(&value, entity_type)
            .iter()
            .map(to_index_entry)
            .collect()
    }

    fn fetch(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        qbo_id: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let path = format!("{}/{qbo_id}", self.entity_path(realm, entity_type));
        let params = [self.minorversion_param()];
        match self.request_json(Method::Get, &path, &params, None) {
            Ok(value) => Ok(Some(parse_single_entity(&value, entity_type)?)),
            Err(QboError::NotFound) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

/// `SELECT * FROM <Entity> [WHERE MetaData.LastUpdatedTime >= x AND < y]
/// ORDERBY MetaData.LastUpdatedTime STARTPOSITION n MAXRESULTS m`.
///
/// `start_position` is the trait's 0-based page offset; QBO's `STARTPOSITION`
/// counts from 1, so it goes on the wire as `start_position + 1`.
fn build_select(
    entity_type: EntityType,
    updated: Option<UpdatedRange>,
    start_position: usize,
    max_results: usize,
) -> String {
    let mut query = format!("SELECT * FROM {}", entity_type.as_str());
    if let Some(range) = updated {
        query.push_str(&format!(
            " WHERE MetaData.LastUpdatedTime >= '{}' AND MetaData.LastUpdatedTime < '{}'",
            range.from.to_rfc3339(),
            range.to.to_rfc3339(),
        ));
    }
    query.push_str(&format!(
        " ORDERBY MetaData.LastUpdatedTime STARTPOSITION {} MAXRESULTS {max_results}",
        start_position + 1,
    ));
    query
}

/// `value["QueryResponse"][entity_type.as_str()]`, or empty when QBO omits
/// the key entirely — its convention for "no rows matched" rather than an
/// empty array.
fn query_response_items(value: &Value, entity_type: EntityType) -> Vec<Value> {
    value
        .pointer(&format!("/QueryResponse/{}", entity_type.as_str()))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn parse_single_entity(value: &Value, entity_type: EntityType) -> Result<EntityPayload, QboError> {
    let object = value.get(entity_type.as_str()).ok_or_else(|| {
        QboError::Validation(format!(
            "response missing {:?} object: {value}",
            entity_type.as_str()
        ))
    })?;
    to_entity_payload(entity_type, object)
}

/// `CDCResponse[].QueryResponse[]` is an array of objects, each keyed by
/// entity type name (plus paging metadata keys this walk skips) — unlike a
/// plain query response, one CDC call can carry several entity types at once.
fn parse_cdc_response(value: &Value) -> Result<Vec<EntityPayload>, QboError> {
    const METADATA_KEYS: &[&str] = &["startPosition", "maxResults", "totalCount"];

    let mut out = Vec::new();
    let responses = value
        .pointer("/CDCResponse")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for response in responses {
        let query_responses = response
            .get("QueryResponse")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for query_response in query_responses {
            let Value::Object(fields) = query_response else {
                continue;
            };
            for (key, items) in fields {
                if METADATA_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let Ok(entity_type) = EntityType::parse(&key) else {
                    continue;
                };
                let Value::Array(items) = items else { continue };
                for item in items {
                    out.push(to_entity_payload(entity_type, &item)?);
                }
            }
        }
    }
    Ok(out)
}

/// `Id`, `SyncToken`, `MetaData.LastUpdatedTime`, and (CDC only) `status ==
/// "Deleted"` — the raw object is kept whole as `raw_json` regardless.
fn to_entity_payload(entity_type: EntityType, object: &Value) -> Result<EntityPayload, QboError> {
    let qbo_id = object
        .get("Id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            QboError::Validation(format!("{entity_type:?} response missing Id: {object}"))
        })?
        .to_string();
    let sync_token = object
        .get("SyncToken")
        .and_then(Value::as_str)
        .unwrap_or("0")
        .to_string();
    let last_updated_raw = object
        .pointer("/MetaData/LastUpdatedTime")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            QboError::Validation(format!(
                "{entity_type:?} {qbo_id} missing MetaData.LastUpdatedTime"
            ))
        })?;
    let last_updated_utc = DateTime::parse_from_rfc3339(last_updated_raw)
        .map_err(|error| {
            QboError::Validation(format!(
                "{entity_type:?} {qbo_id}: bad LastUpdatedTime {last_updated_raw:?}: {error}"
            ))
        })?
        .with_timezone(&Utc);
    let is_deleted = object.get("status").and_then(Value::as_str) == Some("Deleted");

    Ok(EntityPayload {
        entity_type,
        qbo_id,
        sync_token,
        last_updated_utc,
        is_deleted,
        raw_json: object.clone(),
    })
}

fn to_index_entry(object: &Value) -> Result<IndexEntry, QboError> {
    let qbo_id = object
        .get("Id")
        .and_then(Value::as_str)
        .ok_or_else(|| QboError::Validation(format!("index response missing Id: {object}")))?
        .to_string();
    let last_updated_raw = object
        .pointer("/MetaData/LastUpdatedTime")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            QboError::Validation(format!(
                "index entry {qbo_id} missing MetaData.LastUpdatedTime"
            ))
        })?;
    let last_updated_utc = DateTime::parse_from_rfc3339(last_updated_raw)
        .map_err(|error| {
            QboError::Validation(format!(
                "index entry {qbo_id}: bad LastUpdatedTime {last_updated_raw:?}: {error}"
            ))
        })?
        .with_timezone(&Utc);
    // The query endpoint never returns a soft-deleted object at all — only
    // CDC surfaces the deletion itself — so an indexed id is, by
    // construction, not deleted.
    Ok(IndexEntry {
        qbo_id,
        last_updated_utc,
        is_deleted: false,
    })
}

fn append_jsonl<T: serde::Serialize>(path: &Path, entry: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line =
        serde_json::to_string(entry).map_err(|error| std::io::Error::other(error.to_string()))?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_select_has_no_where_clause_when_unbounded() {
        let query = build_select(EntityType::Invoice, None, 0, 100);
        assert_eq!(
            query,
            "SELECT * FROM Invoice ORDERBY MetaData.LastUpdatedTime STARTPOSITION 1 MAXRESULTS 100"
        );
    }

    #[test]
    fn build_select_converts_zero_based_start_to_one_based_startposition() {
        let query = build_select(EntityType::Customer, None, 40, 20);
        assert!(
            query.contains("STARTPOSITION 41 MAXRESULTS 20"),
            "query was: {query}"
        );
    }

    #[test]
    fn build_select_adds_a_bounded_where_clause() {
        let from = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let to = DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let query = build_select(EntityType::Invoice, Some(UpdatedRange { from, to }), 0, 10);
        assert!(
            query.contains("WHERE MetaData.LastUpdatedTime >= '2026-01-01T00:00:00+00:00'"),
            "query was: {query}"
        );
        assert!(
            query.contains("AND MetaData.LastUpdatedTime < '2026-01-02T00:00:00+00:00'"),
            "query was: {query}"
        );
    }

    #[test]
    fn query_response_items_is_empty_when_qbo_omits_the_key() {
        let value = json!({ "QueryResponse": { "maxResults": 0 } });
        assert!(query_response_items(&value, EntityType::Invoice).is_empty());
    }

    #[test]
    fn to_entity_payload_reads_id_sync_token_and_timestamp() {
        let object = json!({
            "Id": "418",
            "SyncToken": "3",
            "MetaData": { "LastUpdatedTime": "2026-08-01T12:00:00-07:00" },
        });
        let payload = to_entity_payload(EntityType::Invoice, &object).unwrap();
        assert_eq!(payload.qbo_id, "418");
        assert_eq!(payload.sync_token, "3");
        assert!(!payload.is_deleted);
        assert_eq!(
            payload.last_updated_utc.to_rfc3339(),
            "2026-08-01T19:00:00+00:00"
        );
    }

    #[test]
    fn to_entity_payload_reads_a_missing_sync_token_as_zero() {
        let object =
            json!({ "Id": "1", "MetaData": { "LastUpdatedTime": "2026-01-01T00:00:00Z" } });
        let payload = to_entity_payload(EntityType::Invoice, &object).unwrap();
        assert_eq!(payload.sync_token, "0");
    }

    #[test]
    fn to_entity_payload_maps_a_deleted_status() {
        let object = json!({
            "Id": "20", "SyncToken": "1",
            "MetaData": { "LastUpdatedTime": "2026-01-01T00:00:00Z" },
            "status": "Deleted",
        });
        assert!(
            to_entity_payload(EntityType::Invoice, &object)
                .unwrap()
                .is_deleted
        );
    }

    #[test]
    fn to_entity_payload_without_an_id_is_a_validation_error() {
        let object = json!({ "MetaData": { "LastUpdatedTime": "2026-01-01T00:00:00Z" } });
        assert!(matches!(
            to_entity_payload(EntityType::Invoice, &object),
            Err(QboError::Validation(_))
        ));
    }

    #[test]
    fn parse_cdc_response_walks_every_entity_type_and_skips_paging_metadata() {
        let value = json!({
            "CDCResponse": [ { "QueryResponse": [
                { "startPosition": 1, "maxResults": 1, "Customer": [
                    { "Id": "10", "SyncToken": "0", "MetaData": { "LastUpdatedTime": "2026-01-01T00:00:00Z" } }
                ] },
                { "Invoice": [
                    { "Id": "20", "SyncToken": "0", "MetaData": { "LastUpdatedTime": "2026-01-01T00:00:00Z" }, "status": "Deleted" }
                ] },
            ] } ]
        });
        let payloads = parse_cdc_response(&value).unwrap();
        assert_eq!(payloads.len(), 2);
        assert!(payloads
            .iter()
            .any(|p| p.entity_type == EntityType::Customer && p.qbo_id == "10" && !p.is_deleted));
        assert!(payloads
            .iter()
            .any(|p| p.entity_type == EntityType::Invoice && p.qbo_id == "20" && p.is_deleted));
    }

    #[test]
    fn is_stale_sync_token_matches_only_fault_code_5010() {
        let stale = json!({ "Fault": { "Error": [ { "code": "5010" } ] } });
        let other = json!({ "Fault": { "Error": [ { "code": "6140" } ] } });
        assert!(is_stale_sync_token(&stale));
        assert!(!is_stale_sync_token(&other));
        assert!(!is_stale_sync_token(&Value::Null));
    }

    #[test]
    fn fault_detail_joins_message_and_detail_per_error() {
        let fault = json!({ "Fault": { "Error": [
            { "Message": "Invalid Reference Id", "Detail": "CustomerRef", "code": "6140" }
        ] } });
        let detail = fault_detail(&fault);
        assert!(detail.contains("Invalid Reference Id"));
        assert!(detail.contains("CustomerRef"));
        assert!(detail.contains("6140"));
    }

    #[test]
    fn fault_detail_falls_back_to_the_raw_body_when_there_is_no_fault_shape() {
        let body = json!({ "something": "unexpected" });
        assert_eq!(fault_detail(&body), body.to_string());
    }

    #[test]
    fn classify_status_maps_every_code_per_handoff_2_3() {
        assert!(matches!(
            classify_status(401, &Value::Null),
            AttemptError::Unauthorized
        ));
        assert!(matches!(
            classify_status(404, &Value::Null),
            AttemptError::NotFound
        ));
        assert!(matches!(
            classify_status(429, &Value::Null),
            AttemptError::RateLimited
        ));
        assert!(matches!(
            classify_status(500, &Value::Null),
            AttemptError::Network(_)
        ));
        assert!(matches!(
            classify_status(
                400,
                &json!({ "Fault": { "Error": [ { "code": "5010" } ] } })
            ),
            AttemptError::StaleSyncToken
        ));
        assert!(matches!(
            classify_status(
                400,
                &json!({ "Fault": { "Error": [ { "code": "6140" } ] } })
            ),
            AttemptError::Validation(_)
        ));
    }
}
