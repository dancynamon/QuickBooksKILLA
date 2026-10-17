//! The HTTP implementation of [`QboClient`]. `HANDOFF.md` §2.3.
//!
//! Split in two so the part that can be wrong is the part that is tested:
//!
//! - **Mapping** — building each request (URL, query language, headers) and
//!   turning each response into [`EntityPayload`]s or a [`QboError`]. Pure
//!   functions over [`HttpRequest`] / [`HttpResponse`], exercised offline
//!   against synthetic fixtures shaped like Intuit's responses.
//! - **Transport** — moving bytes. A trait, so tests substitute a scripted one
//!   and the real one ([`live::UreqTransport`], behind the `live` feature) is a
//!   few lines with nothing in it to get wrong.
//!
//! Rate limiting is **not** done here. The sync driver already spends from a
//! [`crate::ratelimit::RealmLimiter`] before every call; a second limiter in
//! the client would double-count every request against the same budget.

use chrono::{DateTime, SecondsFormat, Utc};
use thiserror::Error;
use uuid::Uuid;

use crate::client::{unique_name_field, EntityPayload, QboClient, QboError, UpdatedRange};
use crate::domain::{EntityType, RealmId};

/// The oldest minor version Intuit still honours; requests below it are
/// silently upgraded, so pinning it explicitly keeps response shapes stable.
pub const DEFAULT_MINOR_VERSION: u32 = 75;

pub const PRODUCTION_BASE: &str = "https://quickbooks.api.intuit.com";
pub const SANDBOX_BASE: &str = "https://sandbox-quickbooks.api.intuit.com";

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The request never produced a response: DNS, TLS, reset, timeout.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("transport: {0}")]
pub struct TransportError(pub String);

pub trait Transport {
    /// Send one request. Any HTTP status is a response, not an error — a 400
    /// carries the fault body the mapping needs to read.
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// Where the bearer token comes from. Implemented by
/// [`crate::oauth::TokenManager`]; a trait so the client is testable without it.
pub trait AccessTokens {
    /// A token valid now, refreshing first if it is near expiry.
    fn bearer(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<String, QboError>;

    /// Intuit refused the token just used. Rotate regardless of its expiry —
    /// the reactive fallback behind proactive refresh (`DESIGN.md` §5.4).
    fn refused(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<(), QboError>;
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// RFC 3986 percent-encoding of everything outside the unreserved set.
pub fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A string literal in QBO's query language. Apostrophes are backslash-escaped,
/// and so is the backslash itself, or a name ending in one would swallow the
/// closing quote.
pub fn quote_literal(raw: &str) -> String {
    let escaped = raw.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// The query-language statement for a paged, optionally windowed read.
///
/// `start_position` is zero-based, as the trait defines it; QBO's
/// `STARTPOSITION` is one-based. That off-by-one is exactly the kind of thing a
/// mapping layer exists to own in one place.
pub fn select_statement(
    entity_type: EntityType,
    updated: Option<UpdatedRange>,
    start_position: usize,
    max_results: usize,
) -> String {
    let mut statement = format!("SELECT * FROM {}", entity_type.as_str());
    if let Some(window) = updated {
        statement.push_str(&format!(
            " WHERE MetaData.LastUpdatedTime >= {} AND MetaData.LastUpdatedTime < {}",
            quote_literal(&timestamp(window.from)),
            quote_literal(&timestamp(window.to)),
        ));
    }
    // Ordered so STARTPOSITION paging is stable. Ties on LastUpdatedTime are
    // ordered however Intuit orders them; that is unverified (DESIGN.md §0) and
    // the reconciliation sweep is what catches a row skipped across a tie.
    statement.push_str(&format!(
        " ORDERBY MetaData.LastUpdatedTime STARTPOSITION {} MAXRESULTS {}",
        start_position + 1,
        max_results
    ));
    statement
}

pub fn name_statement(entity_type: EntityType, field: &str, name: &str) -> String {
    format!(
        "SELECT * FROM {} WHERE {} = {}",
        entity_type.as_str(),
        field,
        quote_literal(name)
    )
}

/// The REST resource name for writes: the entity name lower-cased.
fn resource(entity_type: EntityType) -> String {
    entity_type.as_str().to_ascii_lowercase()
}

#[derive(Clone, Debug)]
pub struct HttpQboConfig {
    /// [`PRODUCTION_BASE`] or [`SANDBOX_BASE`]. Writes point at the sandbox
    /// until explicitly repointed (`DESIGN.md` §8).
    pub base_url: String,
    pub minor_version: u32,
}

impl HttpQboConfig {
    pub fn production() -> Self {
        HttpQboConfig {
            base_url: PRODUCTION_BASE.to_string(),
            minor_version: DEFAULT_MINOR_VERSION,
        }
    }

    pub fn sandbox() -> Self {
        HttpQboConfig {
            base_url: SANDBOX_BASE.to_string(),
            minor_version: DEFAULT_MINOR_VERSION,
        }
    }

    fn company(&self, realm: &RealmId) -> String {
        format!("{}/v3/company/{}", self.base_url.trim_end_matches('/'), realm)
    }

    pub fn query_url(&self, realm: &RealmId, statement: &str) -> String {
        format!(
            "{}/query?query={}&minorversion={}",
            self.company(realm),
            percent_encode(statement),
            self.minor_version
        )
    }

    pub fn cdc_url(
        &self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> String {
        let names: Vec<&str> = entity_types.iter().map(|e| e.as_str()).collect();
        format!(
            "{}/cdc?entities={}&changedSince={}&minorversion={}",
            self.company(realm),
            percent_encode(&names.join(",")),
            percent_encode(&timestamp(changed_since)),
            self.minor_version
        )
    }

    /// Create and update share one endpoint; `requestid` is what makes a
    /// retried write replay instead of repeating. It is passed in, never minted
    /// here — regenerating it per attempt gives no idempotency at all.
    pub fn write_url(&self, realm: &RealmId, entity_type: EntityType, request_id: Uuid) -> String {
        format!(
            "{}/{}?requestid={}&minorversion={}",
            self.company(realm),
            resource(entity_type),
            request_id,
            self.minor_version
        )
    }
}

/// An update body: the outbox payload with `Id` and the base `SyncToken`
/// written over whatever it carried. The token is the conflict check, so it
/// must be the one the edit was based on, not one the payload happens to hold.
pub fn update_body(
    payload: &serde_json::Value,
    qbo_id: &str,
    base_sync_token: &str,
) -> Result<serde_json::Value, QboError> {
    let mut body = payload.clone();
    let object = body
        .as_object_mut()
        .ok_or_else(|| QboError::Validation("update payload is not a JSON object".into()))?;
    object.insert("Id".into(), qbo_id.into());
    object.insert("SyncToken".into(), base_sync_token.into());
    Ok(body)
}

// ---------------------------------------------------------------------------
// Response mapping
// ---------------------------------------------------------------------------

/// Intuit fault codes the mapping distinguishes. Everything else QBO refuses is
/// [`QboError::Validation`].
const FAULT_STALE_OBJECT: &str = "5010";
const FAULT_OBJECT_NOT_FOUND: &str = "610";
const FAULT_AUTHENTICATION: &str = "3200";
const FAULT_THROTTLED: &str = "003001";

/// Map a non-success response to the error the worker routes on.
///
/// Status decides first where it is unambiguous; the fault code decides the
/// rest. A stale `SyncToken` must come out as [`QboError::StaleSyncToken`] and
/// nothing else, because that is the variant that bypasses the retry budget —
/// retrying a stale token is last-writer-wins by another name.
pub fn map_failure(response: &HttpResponse) -> QboError {
    let fault = parse_fault(&response.body);
    let code = fault.as_ref().map(|f| f.code.as_str());

    match (response.status, code) {
        (429, _) | (_, Some(FAULT_THROTTLED)) => QboError::RateLimited,
        (401, _) | (_, Some(FAULT_AUTHENTICATION)) => {
            QboError::Unauthorized(describe(response, fault.as_ref()))
        }
        (_, Some(FAULT_STALE_OBJECT)) => QboError::StaleSyncToken,
        (_, Some(FAULT_OBJECT_NOT_FOUND)) | (404, None) => QboError::NotFound,
        (500..=599, _) => QboError::Network(describe(response, fault.as_ref())),
        _ => QboError::Validation(describe(response, fault.as_ref())),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Fault {
    code: String,
    message: String,
    detail: String,
}

/// Intuit wraps errors as `{"Fault":{"Error":[{...}],"type":...}}`, and some
/// gateway errors come back as `{"fault":{"error":[...]}}` in lower case. Both
/// are read; the first error is the one reported.
fn parse_fault(body: &str) -> Option<Fault> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let fault = value.get("Fault").or_else(|| value.get("fault"))?;
    let errors = fault.get("Error").or_else(|| fault.get("error"))?;
    let first = errors.as_array()?.first()?;
    let field = |name: &str| {
        first
            .get(name)
            .or_else(|| first.get(name.to_ascii_lowercase()))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    Some(Fault {
        code: field("code"),
        message: field("Message"),
        detail: field("Detail"),
    })
}

fn describe(response: &HttpResponse, fault: Option<&Fault>) -> String {
    match fault {
        Some(f) if f.detail.is_empty() => format!("HTTP {} [{}] {}", response.status, f.code, f.message),
        Some(f) => format!("HTTP {} [{}] {}: {}", response.status, f.code, f.message, f.detail),
        None => {
            let excerpt: String = response.body.chars().take(200).collect();
            format!("HTTP {} {}", response.status, excerpt)
        }
    }
}

/// One entity object as QBO serialises it.
///
/// Deleted entities in a CDC response carry `"status": "Deleted"` and little
/// else. They have no `SyncToken`, so an empty one is recorded: a tombstone is
/// never the base of an update.
pub fn parse_entity(
    entity_type: EntityType,
    object: &serde_json::Value,
) -> Result<EntityPayload, QboError> {
    let protocol = |what: &str| QboError::Protocol(format!("{} {what}", entity_type.as_str()));

    let qbo_id = object
        .get("Id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| protocol("has no Id"))?
        .to_string();
    let is_deleted = object.get("status").and_then(|v| v.as_str()) == Some("Deleted");
    let sync_token = object
        .get("SyncToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if sync_token.is_empty() && !is_deleted {
        return Err(protocol(&format!("{qbo_id} has no SyncToken")));
    }
    let updated_raw = object
        .pointer("/MetaData/LastUpdatedTime")
        .and_then(|v| v.as_str())
        .ok_or_else(|| protocol(&format!("{qbo_id} has no MetaData.LastUpdatedTime")))?;
    let last_updated_utc = DateTime::parse_from_rfc3339(updated_raw)
        .map_err(|_| protocol(&format!("{qbo_id} has unparseable LastUpdatedTime {updated_raw:?}")))?
        .with_timezone(&Utc);

    Ok(EntityPayload {
        entity_type,
        qbo_id,
        sync_token,
        last_updated_utc,
        is_deleted,
        raw_json: object.clone(),
    })
}

fn parse_json(response: &HttpResponse) -> Result<serde_json::Value, QboError> {
    serde_json::from_str(&response.body)
        .map_err(|e| QboError::Protocol(format!("response is not JSON: {e}")))
}

/// `{"QueryResponse": {"Invoice": [...], "startPosition": 1, ...}}`. An empty
/// result is `{"QueryResponse": {}}` — no key at all, not an empty array.
pub fn parse_query(entity_type: EntityType, response: &HttpResponse) -> Result<Vec<EntityPayload>, QboError> {
    let value = parse_json(response)?;
    let query = value
        .get("QueryResponse")
        .ok_or_else(|| QboError::Protocol("query response has no QueryResponse".into()))?;
    match query.get(entity_type.as_str()) {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(objects)) => {
            objects.iter().map(|o| parse_entity(entity_type, o)).collect()
        }
        // CompanyInfo and Preferences are singletons in some responses.
        Some(object @ serde_json::Value::Object(_)) => Ok(vec![parse_entity(entity_type, object)?]),
        Some(_) => Err(QboError::Protocol(format!(
            "QueryResponse.{} is neither array nor object",
            entity_type.as_str()
        ))),
    }
}

/// `{"CDCResponse": [{"QueryResponse": [{"Invoice": [...]}, {"Customer": [...]}]}]}`.
///
/// Only the requested types are read; anything else Intuit volunteers is
/// ignored rather than guessed at.
pub fn parse_cdc(entity_types: &[EntityType], response: &HttpResponse) -> Result<Vec<EntityPayload>, QboError> {
    let value = parse_json(response)?;
    let blocks = value
        .get("CDCResponse")
        .and_then(|v| v.as_array())
        .ok_or_else(|| QboError::Protocol("cdc response has no CDCResponse array".into()))?;

    let mut out = Vec::new();
    for block in blocks {
        let Some(queries) = block.get("QueryResponse").and_then(|v| v.as_array()) else {
            continue;
        };
        for query in queries {
            for &entity_type in entity_types {
                if let Some(objects) = query.get(entity_type.as_str()).and_then(|v| v.as_array()) {
                    for object in objects {
                        out.push(parse_entity(entity_type, object)?);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `{"Invoice": {...}, "time": "..."}` — the written entity under its own name.
pub fn parse_write(entity_type: EntityType, response: &HttpResponse) -> Result<EntityPayload, QboError> {
    let value = parse_json(response)?;
    let object = value.get(entity_type.as_str()).ok_or_else(|| {
        QboError::Protocol(format!("write response has no {} object", entity_type.as_str()))
    })?;
    parse_entity(entity_type, object)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct HttpQboClient<T: Transport, A: AccessTokens> {
    transport: T,
    tokens: A,
    config: HttpQboConfig,
    clock: Box<dyn FnMut() -> DateTime<Utc>>,
    /// Requests sent, retries after a 401 included. Reported, not limited.
    pub requests_sent: usize,
}

impl<T: Transport, A: AccessTokens> HttpQboClient<T, A> {
    pub fn new(transport: T, tokens: A, config: HttpQboConfig) -> Self {
        Self::with_clock(transport, tokens, config, Box::new(Utc::now))
    }

    pub fn with_clock(
        transport: T,
        tokens: A,
        config: HttpQboConfig,
        clock: Box<dyn FnMut() -> DateTime<Utc>>,
    ) -> Self {
        HttpQboClient {
            transport,
            tokens,
            config,
            clock,
            requests_sent: 0,
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn tokens(&self) -> &A {
        &self.tokens
    }

    /// Send, and on a 401 rotate the token once and resend. One retry only: a
    /// second 401 straight after a successful refresh is not something another
    /// refresh fixes.
    fn execute(
        &mut self,
        realm: &RealmId,
        method: Method,
        url: String,
        body: Option<String>,
    ) -> Result<HttpResponse, QboError> {
        let mut refreshed = false;
        loop {
            let now = (self.clock)();
            let bearer = self.tokens.bearer(realm, now)?;
            let mut headers = vec![
                ("Authorization".to_string(), format!("Bearer {bearer}")),
                ("Accept".to_string(), "application/json".to_string()),
            ];
            if body.is_some() {
                headers.push(("Content-Type".to_string(), "application/json".to_string()));
            }
            let request = HttpRequest {
                method,
                url: url.clone(),
                headers,
                body: body.clone(),
            };
            self.requests_sent += 1;
            let response = self
                .transport
                .send(&request)
                .map_err(|e| QboError::Network(e.0))?;

            if (200..300).contains(&response.status) {
                return Ok(response);
            }
            let error = map_failure(&response);
            if matches!(error, QboError::Unauthorized(_)) && !refreshed {
                refreshed = true;
                self.tokens.refused(realm, (self.clock)())?;
                continue;
            }
            return Err(error);
        }
    }
}

impl<T: Transport, A: AccessTokens> QboClient for HttpQboClient<T, A> {
    fn query(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        updated: Option<UpdatedRange>,
        start_position: usize,
        max_results: usize,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let statement = select_statement(entity_type, updated, start_position, max_results);
        let url = self.config.query_url(realm, &statement);
        let response = self.execute(realm, Method::Get, url, None)?;
        parse_query(entity_type, &response)
    }

    fn cdc(
        &mut self,
        realm: &RealmId,
        entity_types: &[EntityType],
        changed_since: DateTime<Utc>,
    ) -> Result<Vec<EntityPayload>, QboError> {
        let url = self.config.cdc_url(realm, entity_types, changed_since);
        let response = self.execute(realm, Method::Get, url, None)?;
        parse_cdc(entity_types, &response)
    }

    fn create(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        payload: &serde_json::Value,
        request_id: Uuid,
    ) -> Result<EntityPayload, QboError> {
        let url = self.config.write_url(realm, entity_type, request_id);
        let response = self.execute(realm, Method::Post, url, Some(payload.to_string()))?;
        parse_write(entity_type, &response)
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
        let body = update_body(payload, qbo_id, base_sync_token)?;
        let url = self.config.write_url(realm, entity_type, request_id);
        let response = self.execute(realm, Method::Post, url, Some(body.to_string()))?;
        parse_write(entity_type, &response)
    }

    fn find_by_name(
        &mut self,
        realm: &RealmId,
        entity_type: EntityType,
        name: &str,
    ) -> Result<Option<EntityPayload>, QboError> {
        let field = unique_name_field(entity_type).ok_or_else(|| {
            QboError::Validation(format!("{} has no unique name field", entity_type.as_str()))
        })?;
        let url = self
            .config
            .query_url(realm, &name_statement(entity_type, field, name));
        let response = self.execute(realm, Method::Get, url, None)?;
        // QBO's equality is case-insensitive; the duplicate rule it enforces is
        // too, so a case-different match is the record a create would clash with.
        Ok(parse_query(entity_type, &response)?.into_iter().next())
    }
}

// ---------------------------------------------------------------------------
// Live transport
// ---------------------------------------------------------------------------

/// The real transport. Behind the `live` feature so `cargo test` neither
/// compiles a TLS stack nor has any way to reach Intuit.
#[cfg(feature = "live")]
pub mod live {
    use std::time::Duration;

    use super::{HttpRequest, HttpResponse, Method, Transport, TransportError};

    pub struct UreqTransport {
        agent: ureq::Agent,
    }

    impl UreqTransport {
        pub fn new(timeout: Duration) -> Self {
            UreqTransport {
                agent: ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .try_proxy_from_env(true)
                    .build(),
            }
        }
    }

    impl Default for UreqTransport {
        fn default() -> Self {
            UreqTransport::new(Duration::from_secs(60))
        }
    }

    impl Transport for UreqTransport {
        fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let method = match request.method {
                Method::Get => "GET",
                Method::Post => "POST",
            };
            let mut builder = self.agent.request(method, &request.url);
            for (name, value) in &request.headers {
                builder = builder.set(name, value);
            }
            let result = match &request.body {
                Some(body) => builder.send_string(body),
                None => builder.call(),
            };
            let response = match result {
                Ok(response) => response,
                Err(ureq::Error::Status(_, response)) => response,
                Err(ureq::Error::Transport(error)) => return Err(TransportError(error.to_string())),
            };
            let status = response.status();
            let body = response
                .into_string()
                .map_err(|e| TransportError(format!("reading body: {e}")))?;
            Ok(HttpResponse { status, body })
        }
    }
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

/// A transport that returns queued responses and records what it was sent.
#[derive(Default)]
pub struct ScriptedTransport {
    responses: std::collections::VecDeque<Result<HttpResponse, TransportError>>,
    pub sent: Vec<HttpRequest>,
}

impl ScriptedTransport {
    pub fn respond(&mut self, status: u16, body: impl Into<String>) -> &mut Self {
        self.responses.push_back(Ok(HttpResponse {
            status,
            body: body.into(),
        }));
        self
    }

    pub fn fail(&mut self, message: &str) -> &mut Self {
        self.responses.push_back(Err(TransportError(message.to_string())));
        self
    }
}

impl Transport for ScriptedTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.push(request.clone());
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err(TransportError("script exhausted".into())))
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

    /// Counts refreshes; hands out a new token after each.
    #[derive(Default)]
    struct CountingTokens {
        generation: usize,
        refusals: usize,
    }

    impl AccessTokens for CountingTokens {
        fn bearer(&mut self, _: &RealmId, _: DateTime<Utc>) -> Result<String, QboError> {
            Ok(format!("token-{}", self.generation))
        }
        fn refused(&mut self, _: &RealmId, _: DateTime<Utc>) -> Result<(), QboError> {
            self.refusals += 1;
            self.generation += 1;
            Ok(())
        }
    }

    fn client(transport: ScriptedTransport) -> HttpQboClient<ScriptedTransport, CountingTokens> {
        HttpQboClient::with_clock(
            transport,
            CountingTokens::default(),
            HttpQboConfig::sandbox(),
            Box::new(|| at(0)),
        )
    }

    fn invoice(id: &str, token: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "Id": id,
            "SyncToken": token,
            "DocNumber": format!("T-{id}"),
            "TotalAmt": 100.0,
            "MetaData": { "CreateTime": updated, "LastUpdatedTime": updated }
        })
    }

    fn fault(code: &str, message: &str) -> String {
        serde_json::json!({
            "Fault": {
                "Error": [{ "Message": message, "Detail": "synthetic", "code": code }],
                "type": "ValidationFault"
            },
            "time": "2026-08-23T10:00:00.000-07:00"
        })
        .to_string()
    }

    // --- encoding ---------------------------------------------------------

    #[test]
    fn percent_encoding_leaves_only_unreserved_characters() {
        assert_eq!(percent_encode("a-Z_0.~"), "a-Z_0.~");
        assert_eq!(percent_encode("SELECT * FROM x"), "SELECT%20%2A%20FROM%20x");
        assert_eq!(percent_encode("Bob's & Co"), "Bob%27s%20%26%20Co");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn literals_escape_apostrophes_and_backslashes() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal("Bob's Pools"), r"'Bob\'s Pools'");
        // A trailing backslash must not escape the closing quote.
        assert_eq!(quote_literal(r"ends\"), r"'ends\\'");
    }

    // --- request building -------------------------------------------------

    #[test]
    fn start_position_is_one_based_on_the_wire() {
        let statement = select_statement(EntityType::Invoice, None, 0, 1000);
        assert_eq!(
            statement,
            "SELECT * FROM Invoice ORDERBY MetaData.LastUpdatedTime STARTPOSITION 1 MAXRESULTS 1000"
        );
        assert!(select_statement(EntityType::Invoice, None, 1000, 1000).contains("STARTPOSITION 1001"));
    }

    #[test]
    fn a_window_is_half_open() {
        let statement = select_statement(
            EntityType::Bill,
            Some(UpdatedRange { from: at(0), to: at(60) }),
            0,
            500,
        );
        assert!(statement.contains("MetaData.LastUpdatedTime >= '2025-08-15T23:20:00Z'"));
        assert!(statement.contains("AND MetaData.LastUpdatedTime < '2025-08-15T23:21:00Z'"));
    }

    #[test]
    fn urls_are_realm_scoped_and_pin_the_minor_version() {
        let config = HttpQboConfig::production();
        let url = config.cdc_url(&realm(), &[EntityType::Invoice, EntityType::Customer], at(0));
        assert_eq!(
            url,
            "https://quickbooks.api.intuit.com/v3/company/1234567890123456/cdc\
             ?entities=Invoice%2CCustomer&changedSince=2025-08-15T23%3A20%3A00Z&minorversion=75"
        );
        let request_id = Uuid::nil();
        assert_eq!(
            config.write_url(&realm(), EntityType::PurchaseOrder, request_id),
            "https://quickbooks.api.intuit.com/v3/company/1234567890123456/purchaseorder\
             ?requestid=00000000-0000-0000-0000-000000000000&minorversion=75"
        );
    }

    #[test]
    fn an_update_body_carries_the_base_token_not_the_payloads() {
        let payload = serde_json::json!({ "SyncToken": "9", "TotalAmt": 5 });
        let body = update_body(&payload, "42", "3").unwrap();
        assert_eq!(body["Id"], "42");
        assert_eq!(body["SyncToken"], "3");
        assert_eq!(body["TotalAmt"], 5);
        assert!(update_body(&serde_json::json!([1]), "42", "3").is_err());
    }

    // --- response mapping -------------------------------------------------

    #[test]
    fn query_responses_parse_including_the_empty_shape() {
        let body = serde_json::json!({
            "QueryResponse": {
                "Invoice": [invoice("1", "0", "2026-08-01T09:00:00-07:00"),
                            invoice("2", "4", "2026-08-02T09:00:00-07:00")],
                "startPosition": 1,
                "maxResults": 2
            }
        });
        let response = HttpResponse { status: 200, body: body.to_string() };
        let parsed = parse_query(EntityType::Invoice, &response).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].sync_token, "4");
        // Offsets are normalised to UTC.
        assert_eq!(parsed[0].last_updated_utc.to_rfc3339(), "2026-08-01T16:00:00+00:00");

        let empty = HttpResponse { status: 200, body: r#"{"QueryResponse":{}}"#.into() };
        assert!(parse_query(EntityType::Invoice, &empty).unwrap().is_empty());
    }

    #[test]
    fn cdc_responses_carry_tombstones_and_ignore_unrequested_types() {
        let body = serde_json::json!({
            "CDCResponse": [{
                "QueryResponse": [
                    { "Invoice": [
                        invoice("7", "2", "2026-08-03T09:00:00Z"),
                        { "Id": "8", "status": "Deleted",
                          "MetaData": { "LastUpdatedTime": "2026-08-03T10:00:00Z" } }
                    ] },
                    { "Customer": [ { "Id": "c1", "SyncToken": "0",
                                      "MetaData": { "LastUpdatedTime": "2026-08-03T11:00:00Z" } } ] }
                ]
            }]
        });
        let response = HttpResponse { status: 200, body: body.to_string() };
        let parsed = parse_cdc(&[EntityType::Invoice], &response).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(!parsed[0].is_deleted);
        assert!(parsed[1].is_deleted);
        assert_eq!(parsed[1].sync_token, "");
    }

    #[test]
    fn a_live_entity_without_a_sync_token_is_a_protocol_error() {
        let object = serde_json::json!({ "Id": "1", "MetaData": { "LastUpdatedTime": "2026-08-03T09:00:00Z" } });
        assert!(matches!(parse_entity(EntityType::Invoice, &object), Err(QboError::Protocol(_))));
    }

    #[test]
    fn failures_map_to_the_variants_the_worker_routes_on() {
        let response = |status, body: String| HttpResponse { status, body };
        assert_eq!(
            map_failure(&response(400, fault("5010", "Stale Object Error"))),
            QboError::StaleSyncToken
        );
        assert_eq!(map_failure(&response(400, fault("610", "Object Not Found"))), QboError::NotFound);
        assert_eq!(map_failure(&response(429, String::new())), QboError::RateLimited);
        assert_eq!(map_failure(&response(400, fault("003001", "ThrottleExceeded"))), QboError::RateLimited);
        assert!(matches!(map_failure(&response(401, fault("3200", "AuthenticationFailed"))), QboError::Unauthorized(_)));
        assert!(matches!(map_failure(&response(503, "<html>down</html>".into())), QboError::Network(_)));
        assert!(matches!(
            map_failure(&response(400, fault("6000", "A business validation error has occurred"))),
            QboError::Validation(_)
        ));
    }

    #[test]
    fn transient_classification_survives_the_mapping() {
        // The worker's retry-versus-inbox decision rides on this.
        assert!(map_failure(&HttpResponse { status: 502, body: String::new() }).is_transient());
        assert!(map_failure(&HttpResponse { status: 429, body: String::new() }).is_transient());
        assert!(!map_failure(&HttpResponse { status: 400, body: fault("5010", "Stale") }).is_transient());
        assert!(!map_failure(&HttpResponse { status: 400, body: fault("6000", "bad") }).is_transient());
    }

    // --- client -----------------------------------------------------------

    #[test]
    fn a_query_sends_a_bearer_and_parses_the_page() {
        let mut transport = ScriptedTransport::default();
        transport.respond(
            200,
            serde_json::json!({ "QueryResponse": { "Invoice": [invoice("1", "0", "2026-08-01T00:00:00Z")] } })
                .to_string(),
        );
        let mut qbo = client(transport);
        let page = qbo.query(&realm(), EntityType::Invoice, None, 0, 1000).unwrap();
        assert_eq!(page.len(), 1);

        let sent = &qbo.transport().sent[0];
        assert_eq!(sent.method, Method::Get);
        assert_eq!(sent.header("authorization"), Some("Bearer token-0"));
        assert!(sent.url.starts_with("https://sandbox-quickbooks.api.intuit.com/v3/company/1234567890123456/query?query=SELECT"));
    }

    #[test]
    fn a_401_rotates_the_token_once_and_resends() {
        let mut transport = ScriptedTransport::default();
        transport
            .respond(401, fault("3200", "AuthenticationFailed"))
            .respond(200, r#"{"QueryResponse":{}}"#);
        let mut qbo = client(transport);
        assert!(qbo.query(&realm(), EntityType::Invoice, None, 0, 10).unwrap().is_empty());
        assert_eq!(qbo.tokens().refusals, 1);
        assert_eq!(qbo.transport().sent[1].header("Authorization"), Some("Bearer token-1"));
    }

    #[test]
    fn a_second_401_is_returned_not_looped_on() {
        let mut transport = ScriptedTransport::default();
        transport
            .respond(401, fault("3200", "AuthenticationFailed"))
            .respond(401, fault("3200", "AuthenticationFailed"));
        let mut qbo = client(transport);
        let result = qbo.query(&realm(), EntityType::Invoice, None, 0, 10);
        assert!(matches!(result, Err(QboError::Unauthorized(_))));
        assert_eq!(qbo.requests_sent, 2);
    }

    #[test]
    fn a_transport_failure_is_a_transient_network_error() {
        let mut transport = ScriptedTransport::default();
        transport.fail("connection reset by peer");
        let mut qbo = client(transport);
        let error = qbo.cdc(&realm(), &[EntityType::Invoice], at(0)).unwrap_err();
        assert!(error.is_transient());
    }

    #[test]
    fn create_reuses_the_callers_request_id() {
        let mut transport = ScriptedTransport::default();
        transport.fail("timeout").respond(
            200,
            serde_json::json!({ "Invoice": invoice("55", "0", "2026-08-01T00:00:00Z") }).to_string(),
        );
        let mut qbo = client(transport);
        let request_id = Uuid::now_v7();
        let payload = serde_json::json!({ "DocNumber": "T-1" });

        assert!(qbo.create(&realm(), EntityType::Invoice, &payload, request_id).is_err());
        let created = qbo.create(&realm(), EntityType::Invoice, &payload, request_id).unwrap();
        assert_eq!(created.qbo_id, "55");

        let sent = &qbo.transport().sent;
        assert_eq!(sent[0].url, sent[1].url, "a retry must carry the same requestid");
        assert!(sent[0].url.contains(&request_id.to_string()));
        assert_eq!(sent[0].header("Content-Type"), Some("application/json"));
    }

    #[test]
    fn a_stale_update_comes_back_as_stale_sync_token() {
        let mut transport = ScriptedTransport::default();
        transport.respond(400, fault("5010", "Stale Object Error"));
        let mut qbo = client(transport);
        let result = qbo.update(
            &realm(),
            EntityType::Invoice,
            "55",
            &serde_json::json!({ "TotalAmt": 1 }),
            "0",
            Uuid::now_v7(),
        );
        assert_eq!(result, Err(QboError::StaleSyncToken));
        let body: serde_json::Value =
            serde_json::from_str(qbo.transport().sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["SyncToken"], "0");
    }

    #[test]
    fn find_by_name_queries_the_unique_field_with_escaping() {
        let mut transport = ScriptedTransport::default();
        transport.respond(200, r#"{"QueryResponse":{}}"#);
        let mut qbo = client(transport);
        assert!(qbo
            .find_by_name(&realm(), EntityType::Customer, "Bob's Pools")
            .unwrap()
            .is_none());
        let url = &qbo.transport().sent[0].url;
        assert!(url.contains(&percent_encode(r"WHERE DisplayName = 'Bob\'s Pools'")));

        assert!(matches!(
            qbo.find_by_name(&realm(), EntityType::Invoice, "x"),
            Err(QboError::Validation(_))
        ));
    }

    #[test]
    fn the_sync_driver_runs_over_the_http_client() {
        // The seam: nothing above the trait changes. A full sweep of one type
        // through the real mapping, against a store.
        use crate::driver::{SyncDriver, SyncOptions};
        use crate::store::Store;

        let mut transport = ScriptedTransport::default();
        transport.respond(
            200,
            serde_json::json!({ "QueryResponse": { "Customer": [
                { "Id": "1", "SyncToken": "0", "DisplayName": "HARBOR SWIM CLUB",
                  "MetaData": { "LastUpdatedTime": "2026-08-01T00:00:00Z" } }
            ] } })
            .to_string(),
        );
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "test", at(0)).unwrap();
        let mut driver = SyncDriver::new(client(transport), SyncOptions::default(), at(0));
        let report = driver
            .sync_entity_type(&store, &realm(), EntityType::Customer, at(0))
            .unwrap();
        assert_eq!(report.mirrored, 1);
    }
}
