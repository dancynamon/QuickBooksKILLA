//! OAuth 2.0 authorization-code flow against Intuit's endpoints. `HANDOFF.md`
//! §2.2, `DESIGN.md` §5.
//!
//! This is the one-time, per-realm interactive step: send the user to
//! Intuit's consent page, catch the redirect it sends back to a loopback
//! address, and exchange the code it carries for a [`TokenSet`]
//! (`crate::auth`) — everything after that is [`crate::http::TokenSource`]
//! refreshing the same tokens without a browser involved.
//!
//! Kept as a sibling module to [`crate::auth`] rather than nested under it —
//! `auth.rs` is a single file, not a directory, and this module's job (an
//! HTTP round trip and a TCP listener) is different enough from that one's
//! (durable local persistence) that folding them together would blur which
//! is which.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Deserialize;
use thiserror::Error;

use crate::auth::TokenSet;

/// Intuit's production authorization endpoint.
pub const INTUIT_AUTHORIZE_URL: &str = "https://appcenter.intuit.com/connect/oauth2";
/// Intuit's production token endpoint — used for both the initial exchange
/// and every later refresh.
pub const INTUIT_TOKEN_URL: &str = "https://oauth.platform.intuit.com/oauth2/v1/tokens/bearer";
/// The one scope this app needs: read/write access to the accounting API.
pub const ACCOUNTING_SCOPE: &str = "com.intuit.quickbooks.accounting";

#[derive(Debug, Error)]
pub enum AuthFlowError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for the OAuth redirect")]
    Timeout,
    #[error("the redirect carried no `code` parameter")]
    MissingCode,
    #[error("token endpoint returned http {status}: {body}")]
    TokenEndpoint { status: u16, body: String },
    #[error("token endpoint: {0}")]
    Transport(String),
    #[error("token endpoint response: {0}")]
    Decode(String),
}

/// Everything needed to talk to Intuit's OAuth endpoints for one app
/// registration. `intuit` fills in Intuit's own URLs and the accounting
/// scope; the caller still supplies `client_id`, `client_secret` and
/// `redirect_uri` from `crate::config::LocalConfig`.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: String,
    pub authorize_url: String,
    pub token_url: String,
}

impl OAuthConfig {
    pub fn intuit(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        OAuthConfig {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            redirect_uri: redirect_uri.into(),
            scopes: ACCOUNTING_SCOPE.to_string(),
            authorize_url: INTUIT_AUTHORIZE_URL.to_string(),
            token_url: INTUIT_TOKEN_URL.to_string(),
        }
    }

    /// The URL to send the user's browser to. `state` should be a
    /// per-attempt random value the caller checks against what the redirect
    /// returns, as a CSRF guard.
    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "{}?client_id={}&response_type=code&scope={}&redirect_uri={}&state={}",
            self.authorize_url,
            percent_encode(&self.client_id),
            percent_encode(&self.scopes),
            percent_encode(&self.redirect_uri),
            percent_encode(state),
        )
    }
}

/// What Intuit's redirect to the loopback listener carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthCode {
    pub code: String,
    pub state: String,
    /// Empty if the redirect omitted it — some Intuit flows don't include a
    /// realm on the query string, in which case the caller already knows
    /// which realm it asked to authorize.
    pub realm_id: String,
}

/// Bind `127.0.0.1:port`, accept exactly one GET request, and return the
/// `code`/`state`/`realmId` query parameters it carried.
///
/// One request only — this is a single interactive consent round trip, not a
/// server, so it closes right after rather than lingering and accepting a
/// stray retry from the browser.
pub fn run_loopback(port: u16, timeout: Duration) -> Result<AuthCode, AuthFlowError> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                return handle_callback(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(AuthFlowError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn handle_callback(stream: TcpStream) -> Result<AuthCode, AuthFlowError> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Drain headers up to the blank line; their content doesn't matter here.
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or(AuthFlowError::MissingCode)?;
    let query = target.split_once('?').map_or("", |(_, query)| query);
    let params = parse_query_string(query);

    let code = params
        .get("code")
        .cloned()
        .ok_or(AuthFlowError::MissingCode)?;
    let state = params.get("state").cloned().unwrap_or_default();
    let realm_id = params.get("realmId").cloned().unwrap_or_default();

    let body =
        "<html><body>Authorized. You can close this window and return to qbo-local.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok(AuthCode {
        code,
        state,
        realm_id,
    })
}

fn parse_query_string(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or("");
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Enough of RFC 3986 for the ASCII query parameters this flow actually
/// carries (authorization codes, UUID state values, digit-string realm ids)
/// — not a general-purpose URL codec.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => out.push(' '),
            b'%' => {
                let hi = bytes.next();
                let lo = bytes.next();
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        let hex = [hi, lo];
                        if let Ok(hex_str) = std::str::from_utf8(&hex) {
                            if let Ok(value) = u8::from_str_radix(hex_str, 16) {
                                out.push(value as char);
                                continue;
                            }
                        }
                        out.push('%');
                    }
                    _ => out.push('%'),
                }
            }
            other => out.push(other as char),
        }
    }
    out
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    x_refresh_token_expires_in: i64,
}

/// Exchange a freshly-issued authorization code for the first [`TokenSet`] —
/// generation zero, before any rotation has happened.
pub fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    agent: &ureq::Agent,
) -> Result<TokenSet, AuthFlowError> {
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}",
        percent_encode(code),
        percent_encode(&config.redirect_uri),
    );
    token_request(config, agent, &form)
}

/// Trade a refresh token for a new [`TokenSet`]. Intuit invalidates
/// `refresh_token` the moment this succeeds, which is the whole reason
/// `crate::auth`'s generation retention and atomic persistence exist.
pub fn refresh(
    config: &OAuthConfig,
    refresh_token: &str,
    agent: &ureq::Agent,
) -> Result<TokenSet, AuthFlowError> {
    let form = format!(
        "grant_type=refresh_token&refresh_token={}",
        percent_encode(refresh_token)
    );
    token_request(config, agent, &form)
}

fn token_request(
    config: &OAuthConfig,
    agent: &ureq::Agent,
    form: &str,
) -> Result<TokenSet, AuthFlowError> {
    let result = agent
        .post(&config.token_url)
        .set(
            "Authorization",
            &basic_auth_header(&config.client_id, &config.client_secret),
        )
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(form);

    let response = match result {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(AuthFlowError::TokenEndpoint { status, body });
        }
        Err(ureq::Error::Transport(transport)) => {
            return Err(AuthFlowError::Transport(transport.to_string()))
        }
    };

    let parsed: TokenResponse = response
        .into_json()
        .map_err(|error| AuthFlowError::Decode(error.to_string()))?;

    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        obtained_at: Utc::now(),
        access_expires_in_secs: parsed.expires_in,
        refresh_expires_in_secs: parsed.x_refresh_token_expires_in,
    })
}

fn basic_auth_header(client_id: &str, client_secret: &str) -> String {
    format!(
        "Basic {}",
        base64_encode(format!("{client_id}:{client_secret}").as_bytes())
    )
}

/// A small hand-rolled base64 encoder (standard alphabet, `=` padding) so
/// this module doesn't need a dependency for one header. HTTP Basic auth is
/// the only place this crate needs base64 at all.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_every_parameter() {
        let config =
            OAuthConfig::intuit("client-123", "secret-abc", "http://localhost:8765/callback");
        let url = config.authorize_url("state-xyz");
        assert!(url.starts_with(INTUIT_AUTHORIZE_URL));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope=com.intuit.quickbooks.accounting"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8765%2Fcallback"));
        assert!(url.contains("state=state-xyz"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64_encode(b"client-123:secret-abc"),
            "Y2xpZW50LTEyMzpzZWNyZXQtYWJj"
        );
    }

    #[test]
    fn percent_round_trips_reserved_characters() {
        let raw = "a b+c/d?e=f&g";
        assert_eq!(percent_decode(&percent_encode(raw)), raw);
    }

    #[test]
    fn query_string_parsing_decodes_each_value() {
        let params = parse_query_string("code=abc%20def&state=xyz&realmId=123");
        assert_eq!(params.get("code").map(String::as_str), Some("abc def"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(params.get("realmId").map(String::as_str), Some("123"));
    }

    #[test]
    fn loopback_times_out_when_nothing_connects() {
        // Port 0 asks the OS for any free port; the point here is only that
        // a listener with nothing arriving returns a timeout rather than
        // hanging.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let result = run_loopback(port, Duration::from_millis(80));
        assert!(matches!(result, Err(AuthFlowError::Timeout)));
    }
}
