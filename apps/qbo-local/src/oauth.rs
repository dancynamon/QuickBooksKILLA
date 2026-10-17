//! OAuth 2.0 against Intuit: the one-time authorisation, and every refresh
//! after it. `DESIGN.md` §5, `HANDOFF.md` §2.2.
//!
//! [`crate::auth`] owns what a token *is* and how it reaches disk. This module
//! owns the conversation with Intuit's token endpoint, and the one ordering
//! rule that matters: **a rotated token is persisted before it is used.** If
//! the process dies after Intuit issued a new refresh token and before it
//! reached disk, the retained generations are the way back; if it dies after it
//! reached disk, nothing was lost at all.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::auth::{AuthError, RotationLogEntry, TokenGenerations, TokenSet, TokenStore};
use crate::client::QboError;
use crate::domain::RealmId;
use crate::http::{percent_encode, AccessTokens, HttpRequest, Method, Transport};

pub const AUTHORIZE_ENDPOINT: &str = "https://appcenter.intuit.com/connect/oauth2";
pub const TOKEN_ENDPOINT: &str = "https://oauth.platform.intuit.com/oauth2/v1/tokens/bearer";
pub const SCOPE_ACCOUNTING: &str = "com.intuit.quickbooks.accounting";

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("token endpoint unreachable: {0}")]
    Transport(String),
    /// The refresh token or authorisation code is no longer valid. For a
    /// refresh this is the signal to fall back a generation, not to retry.
    #[error("invalid_grant: {0}")]
    InvalidGrant(String),
    #[error("token endpoint refused (HTTP {status}): {detail}")]
    Rejected { status: u16, detail: String },
    #[error("token endpoint response unreadable: {0}")]
    Protocol(String),
    #[error("authorisation callback: {0}")]
    Callback(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

/// The Intuit developer app's credentials. Debug output redacts the secret, so
/// a stray `{:?}` in a log line cannot leak it.
#[derive(Clone)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl ClientCredentials {
    fn basic_auth(&self) -> String {
        format!(
            "Basic {}",
            base64(format!("{}:{}", self.client_id, self.client_secret).as_bytes())
        )
    }
}

/// Standard base64 with padding. Twenty lines here beat a dependency used once.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn percent_decode(raw: &str) -> Result<String, OAuthError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = raw
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| OAuthError::Callback(format!("bad escape in {raw:?}")))?;
                out.push(hex);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| OAuthError::Callback("callback is not UTF-8".into()))
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

/// Where to send the user to approve access. `state` is echoed back on the
/// callback and must be checked there — it is the CSRF guard.
pub fn authorize_url(credentials: &ClientCredentials, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_ENDPOINT}?{}",
        form(&[
            ("client_id", &credentials.client_id),
            ("response_type", "code"),
            ("scope", SCOPE_ACCOUNTING),
            ("redirect_uri", redirect_uri),
            ("state", state),
        ])
    )
}

fn token_request(credentials: &ClientCredentials, body: String) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url: TOKEN_ENDPOINT.to_string(),
        headers: vec![
            ("Authorization".into(), credentials.basic_auth()),
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/x-www-form-urlencoded".into()),
        ],
        body: Some(body),
    }
}

fn call_token_endpoint(
    transport: &mut impl Transport,
    request: &HttpRequest,
    now: DateTime<Utc>,
) -> Result<TokenSet, OAuthError> {
    let response = transport
        .send(request)
        .map_err(|e| OAuthError::Transport(e.0))?;
    let value: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();

    if !(200..300).contains(&response.status) {
        let error = value.get("error").and_then(|v| v.as_str()).unwrap_or_default();
        let detail = value
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or(error)
            .to_string();
        return Err(if error == "invalid_grant" {
            OAuthError::InvalidGrant(detail)
        } else {
            OAuthError::Rejected {
                status: response.status,
                detail,
            }
        });
    }

    let text = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| OAuthError::Protocol(format!("missing {name}")))
    };
    let seconds = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| OAuthError::Protocol(format!("missing {name}")))
    };
    Ok(TokenSet {
        access_token: text("access_token")?,
        refresh_token: text("refresh_token")?,
        obtained_at: now,
        access_expires_in_secs: seconds("expires_in")?,
        refresh_expires_in_secs: seconds("x_refresh_token_expires_in")?,
    })
}

pub fn exchange_code(
    transport: &mut impl Transport,
    credentials: &ClientCredentials,
    code: &str,
    redirect_uri: &str,
    now: DateTime<Utc>,
) -> Result<TokenSet, OAuthError> {
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
    ]);
    call_token_endpoint(transport, &token_request(credentials, body), now)
}

pub fn refresh(
    transport: &mut impl Transport,
    credentials: &ClientCredentials,
    refresh_token: &str,
    now: DateTime<Utc>,
) -> Result<TokenSet, OAuthError> {
    let body = form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]);
    call_token_endpoint(transport, &token_request(credentials, body), now)
}

// ---------------------------------------------------------------------------
// Loopback callback
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub realm_id: RealmId,
}

/// Read the redirect's request line: `GET /callback?code=…&state=…&realmId=… HTTP/1.1`.
///
/// The realm comes from the callback, not from config: it is the company the
/// user actually picked on Intuit's consent screen, which is the one the
/// tokens are good for.
pub fn parse_callback(request_line: &str, expected_state: &str) -> Result<Callback, OAuthError> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| OAuthError::Callback("malformed request line".into()))?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();

    let mut params = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key)?, percent_decode(value)?);
    }

    if let Some(error) = params.get("error") {
        return Err(OAuthError::Callback(format!("Intuit returned {error}")));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(OAuthError::Callback("state mismatch; refusing the code".into()));
    }
    let code = params
        .remove("code")
        .ok_or_else(|| OAuthError::Callback("no code".into()))?;
    let realm = params
        .remove("realmId")
        .ok_or_else(|| OAuthError::Callback("no realmId".into()))?;
    let realm_id = RealmId::parse(realm).map_err(|e| OAuthError::Callback(e.to_string()))?;
    Ok(Callback { code, realm_id })
}

/// Accept one connection on the loopback listener, answer it, and return the
/// parsed callback. Requests that are not the callback (a browser's favicon
/// fetch) are answered 404 and skipped.
pub fn await_callback(listener: &TcpListener, expected_state: &str) -> Result<Callback, OAuthError> {
    loop {
        let (stream, _) = listener
            .accept()
            .map_err(|e| OAuthError::Callback(e.to_string()))?;
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .map_err(|e| OAuthError::Callback(e.to_string()))?;
        let mut stream = reader.into_inner();

        if !request_line.contains("/callback") {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let result = parse_callback(&request_line, expected_state);
        let message = match &result {
            Ok(_) => "qbo-local is authorised. You can close this tab.",
            Err(_) => "qbo-local authorisation failed. Check the terminal.",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
            message.len()
        );
        return result;
    }
}

// ---------------------------------------------------------------------------
// Token manager
// ---------------------------------------------------------------------------

/// Supplies bearer tokens to the HTTP client, rotating them against Intuit and
/// persisting every rotation through a [`TokenStore`] before handing it out.
pub struct TokenManager<T: Transport, S: TokenStore> {
    transport: T,
    store: S,
    credentials: ClientCredentials,
    cache: HashMap<RealmId, TokenGenerations>,
    /// Append-only JSONL of every rotation attempt (§5.3). Fingerprints only.
    rotation_log: Option<PathBuf>,
}

impl<T: Transport, S: TokenStore> TokenManager<T, S> {
    pub fn new(transport: T, store: S, credentials: ClientCredentials) -> Self {
        TokenManager {
            transport,
            store,
            credentials,
            cache: HashMap::new(),
            rotation_log: None,
        }
    }

    pub fn with_rotation_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.rotation_log = Some(path.into());
        self
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Record the outcome of a first authorisation. Overwrites whatever the
    /// store held for that realm — re-authorising is a deliberate reset.
    pub fn install(&mut self, realm: &RealmId, tokens: TokenSet) -> Result<(), OAuthError> {
        let generations = TokenGenerations::new(realm.clone(), tokens);
        self.store.save(&generations)?;
        self.cache.insert(realm.clone(), generations);
        Ok(())
    }

    fn generations(&mut self, realm: &RealmId) -> Result<&mut TokenGenerations, OAuthError> {
        if !self.cache.contains_key(realm) {
            let loaded = self
                .store
                .load(realm)?
                .ok_or_else(|| AuthError::Lockout(realm.clone()))?;
            self.cache.insert(realm.clone(), loaded);
        }
        Ok(self.cache.get_mut(realm).expect("inserted above"))
    }

    /// Rotate: try each generation's refresh token, newest first, until Intuit
    /// accepts one.
    ///
    /// Only `invalid_grant` moves on to an older generation. A network failure
    /// stops immediately — the newest token is probably still good, and
    /// spending older ones on an outage would burn the fallbacks for nothing.
    pub fn rotate(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<(), OAuthError> {
        let generations = self.generations(realm)?.clone();
        let outgoing = generations.current.fingerprint();

        for candidate in generations.candidates() {
            if candidate.refresh_is_expired(now) {
                continue;
            }
            match refresh(&mut self.transport, &self.credentials, &candidate.refresh_token, now) {
                Ok(next) => {
                    let incoming = next.fingerprint();
                    let mut updated = generations.clone();
                    updated.rotate(next);
                    // Cache first so this process keeps working even if the
                    // save fails; then persist, and only then report success.
                    self.cache.insert(realm.clone(), updated.clone());
                    let saved = self.store.save(&updated);
                    self.log(realm, now, &outgoing, &incoming, match &saved {
                        Ok(()) => "rotated".to_string(),
                        Err(e) => format!("rotated but NOT persisted: {e}"),
                    });
                    saved?;
                    return Ok(());
                }
                Err(OAuthError::InvalidGrant(detail)) => {
                    self.log(realm, now, &candidate.fingerprint(), "-", format!("invalid_grant: {detail}"));
                }
                Err(other) => {
                    self.log(realm, now, &outgoing, "-", format!("failed: {other}"));
                    return Err(other);
                }
            }
        }
        self.log(realm, now, &outgoing, "-", "lockout: no generation accepted".into());
        Err(AuthError::Lockout(realm.clone()).into())
    }

    fn log(&self, realm: &RealmId, at: DateTime<Utc>, outgoing: &str, incoming: &str, outcome: String) {
        let Some(path) = &self.rotation_log else {
            return;
        };
        let entry = RotationLogEntry {
            at,
            realm_id: realm.clone(),
            outgoing: outgoing.to_string(),
            incoming: incoming.to_string(),
            outcome,
        };
        // A rotation log that cannot be written must not fail the rotation it
        // describes; the token on disk is what matters.
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

fn to_qbo(error: OAuthError) -> QboError {
    match error {
        OAuthError::Transport(detail) => QboError::Network(detail),
        other => QboError::Unauthorized(other.to_string()),
    }
}

impl<T: Transport, S: TokenStore> AccessTokens for TokenManager<T, S> {
    fn bearer(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<String, QboError> {
        if self.generations(realm).map_err(to_qbo)?.current.needs_refresh(now) {
            self.rotate(realm, now).map_err(to_qbo)?;
        }
        Ok(self.generations(realm).map_err(to_qbo)?.current.access_token.clone())
    }

    fn refused(&mut self, realm: &RealmId, now: DateTime<Utc>) -> Result<(), QboError> {
        self.rotate(realm, now).map_err(to_qbo)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;
    use std::thread;

    use chrono::Duration;

    use super::*;
    use crate::auth::FileTokenStore;
    use crate::http::ScriptedTransport;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + secs, 0).unwrap()
    }

    fn credentials() -> ClientCredentials {
        ClientCredentials {
            client_id: "client-abc".into(),
            client_secret: "secret-xyz".into(),
        }
    }

    fn token_body(label: &str) -> String {
        serde_json::json!({
            "token_type": "bearer",
            "access_token": format!("access-{label}"),
            "refresh_token": format!("refresh-{label}"),
            "expires_in": 3600,
            "x_refresh_token_expires_in": 8_726_400
        })
        .to_string()
    }

    fn set(label: &str, obtained_at: DateTime<Utc>) -> TokenSet {
        TokenSet {
            access_token: format!("access-{label}"),
            refresh_token: format!("refresh-{label}"),
            obtained_at,
            access_expires_in_secs: 3600,
            refresh_expires_in_secs: 100 * 24 * 3600,
        }
    }

    const INVALID_GRANT: &str = r#"{"error":"invalid_grant","error_description":"Token invalid"}"#;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (raw, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(raw.as_bytes()), encoded);
        }
    }

    #[test]
    fn debug_output_never_shows_the_secret() {
        let shown = format!("{:?}", credentials());
        assert!(!shown.contains("secret-xyz"));
        assert!(shown.contains("client-abc"));
    }

    #[test]
    fn the_authorize_url_requests_accounting_scope() {
        let url = authorize_url(&credentials(), "http://localhost:8765/callback", "s1");
        assert!(url.starts_with(AUTHORIZE_ENDPOINT));
        assert!(url.contains("scope=com.intuit.quickbooks.accounting"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8765%2Fcallback"));
        assert!(url.contains("state=s1"));
    }

    #[test]
    fn a_refresh_posts_a_form_with_basic_auth() {
        let mut transport = ScriptedTransport::default();
        transport.respond(200, token_body("b"));
        let tokens = refresh(&mut transport, &credentials(), "refresh-a", at(0)).unwrap();
        assert_eq!(tokens.refresh_token, "refresh-b");
        assert_eq!(tokens.access_expires_at(), at(3600));

        let sent = &transport.sent[0];
        assert_eq!(sent.url, TOKEN_ENDPOINT);
        assert_eq!(sent.header("Authorization"), Some("Basic Y2xpZW50LWFiYzpzZWNyZXQteHl6"));
        assert_eq!(sent.body.as_deref(), Some("grant_type=refresh_token&refresh_token=refresh-a"));
    }

    #[test]
    fn invalid_grant_is_distinguished_from_other_refusals() {
        let mut transport = ScriptedTransport::default();
        transport
            .respond(400, INVALID_GRANT)
            .respond(401, r#"{"error":"invalid_client"}"#);
        assert!(matches!(
            refresh(&mut transport, &credentials(), "r", at(0)),
            Err(OAuthError::InvalidGrant(_))
        ));
        assert!(matches!(
            refresh(&mut transport, &credentials(), "r", at(0)),
            Err(OAuthError::Rejected { status: 401, .. })
        ));
    }

    #[test]
    fn callbacks_are_parsed_and_state_is_enforced() {
        let line = "GET /callback?code=AB%2B1&state=s1&realmId=1234567890123456 HTTP/1.1\r\n";
        let callback = parse_callback(line, "s1").unwrap();
        assert_eq!(callback.code, "AB+1");
        assert_eq!(callback.realm_id, realm());

        assert!(parse_callback(line, "other").is_err());
        assert!(parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1", "s1").is_err());
    }

    #[test]
    fn the_loopback_listener_skips_strays_and_answers_the_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = thread::spawn(move || {
            let send = |path: &str| {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut reply = String::new();
                std::io::Read::read_to_string(&mut stream, &mut reply).unwrap();
                reply
            };
            let stray = send("/favicon.ico");
            let real = send("/callback?code=c1&state=s1&realmId=1234567890123456");
            (stray, real)
        });
        let callback = await_callback(&listener, "s1").unwrap();
        let (stray, real) = browser.join().unwrap();
        assert!(stray.starts_with("HTTP/1.1 404"));
        assert!(real.contains("authorised"));
        assert_eq!(callback.code, "c1");
    }

    fn manager(
        transport: ScriptedTransport,
        dir: &std::path::Path,
    ) -> TokenManager<ScriptedTransport, FileTokenStore> {
        TokenManager::new(transport, FileTokenStore::new(dir), credentials())
            .with_rotation_log(dir.join("rotation-test.jsonl"))
    }

    #[test]
    fn a_fresh_token_is_used_without_calling_intuit() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = manager(ScriptedTransport::default(), dir.path());
        tokens.install(&realm(), set("a", at(0))).unwrap();
        assert_eq!(tokens.bearer(&realm(), at(60)).unwrap(), "access-a");
        assert!(tokens.transport().sent.is_empty());
    }

    #[test]
    fn a_near_expiry_token_rotates_and_is_on_disk_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut transport = ScriptedTransport::default();
        transport.respond(200, token_body("b"));
        let mut tokens = manager(transport, dir.path());
        tokens.install(&realm(), set("a", at(0))).unwrap();

        let bearer = tokens.bearer(&realm(), at(0) + Duration::minutes(55)).unwrap();
        assert_eq!(bearer, "access-b");

        // A restart reads the rotated generation, with the old one retained.
        let reloaded = FileTokenStore::new(dir.path()).load(&realm()).unwrap().unwrap();
        assert_eq!(reloaded.current.refresh_token, "refresh-b");
        assert_eq!(reloaded.previous[0].refresh_token, "refresh-a");

        let log = std::fs::read_to_string(dir.path().join("rotation-test.jsonl")).unwrap();
        assert!(log.contains("rotated"));
        assert!(!log.contains("refresh-a") && !log.contains("access-b"), "log holds fingerprints only");
    }

    #[test]
    fn invalid_grant_falls_back_to_a_retained_generation() {
        // The crash case: Intuit invalidated `b` but the process died with it
        // half-persisted, so the store's current token is dead and `a` is not.
        let dir = tempfile::tempdir().unwrap();
        let mut transport = ScriptedTransport::default();
        transport.respond(400, INVALID_GRANT).respond(200, token_body("c"));
        let mut tokens = manager(transport, dir.path());
        let mut generations = TokenGenerations::new(realm(), set("a", at(0)));
        generations.rotate(set("b", at(10)));
        tokens.store.save(&generations).unwrap();

        tokens.refused(&realm(), at(20)).unwrap();
        let sent = &tokens.transport().sent;
        assert!(sent[0].body.as_deref().unwrap().contains("refresh-b"));
        assert!(sent[1].body.as_deref().unwrap().contains("refresh-a"));
        assert_eq!(tokens.bearer(&realm(), at(30)).unwrap(), "access-c");
    }

    #[test]
    fn a_network_failure_does_not_spend_older_generations() {
        let dir = tempfile::tempdir().unwrap();
        let mut transport = ScriptedTransport::default();
        transport.fail("connection reset");
        let mut tokens = manager(transport, dir.path());
        let mut generations = TokenGenerations::new(realm(), set("a", at(0)));
        generations.rotate(set("b", at(10)));
        tokens.store.save(&generations).unwrap();

        let error = tokens.refused(&realm(), at(20)).unwrap_err();
        assert!(error.is_transient());
        assert_eq!(tokens.transport().sent.len(), 1);
    }

    #[test]
    fn every_generation_refused_is_a_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let mut transport = ScriptedTransport::default();
        transport.respond(400, INVALID_GRANT).respond(400, INVALID_GRANT);
        let mut tokens = manager(transport, dir.path());
        let mut generations = TokenGenerations::new(realm(), set("a", at(0)));
        generations.rotate(set("b", at(10)));
        tokens.store.save(&generations).unwrap();

        let error = tokens.rotate(&realm(), at(20)).unwrap_err();
        assert!(matches!(error, OAuthError::Auth(AuthError::Lockout(_))));
        let log = std::fs::read_to_string(dir.path().join("rotation-test.jsonl")).unwrap();
        assert!(log.contains("lockout"));
    }

    #[test]
    fn an_unauthorised_realm_is_reported_not_panicked_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut tokens = manager(ScriptedTransport::default(), dir.path());
        assert!(matches!(tokens.bearer(&realm(), at(0)), Err(QboError::Unauthorized(_))));
    }

    #[test]
    fn the_http_client_and_token_manager_compose() {
        // Proactive refresh inside a real client call: the token endpoint is
        // hit first, then the API with the rotated bearer.
        use crate::client::QboClient;
        use crate::domain::EntityType;
        use crate::http::{HttpQboClient, HttpQboConfig};

        let dir = tempfile::tempdir().unwrap();
        let mut token_transport = ScriptedTransport::default();
        token_transport.respond(200, token_body("b"));
        let mut tokens = manager(token_transport, dir.path());
        tokens.install(&realm(), set("a", at(0))).unwrap();

        let mut api = ScriptedTransport::default();
        api.respond(200, r#"{"QueryResponse":{}}"#);
        let mut qbo = HttpQboClient::with_clock(
            api,
            tokens,
            HttpQboConfig::sandbox(),
            Box::new(|| at(0) + Duration::minutes(55)),
        );
        qbo.query(&realm(), EntityType::Invoice, None, 0, 10).unwrap();
        assert_eq!(qbo.transport().sent[0].header("Authorization"), Some("Bearer access-b"));
    }
}
