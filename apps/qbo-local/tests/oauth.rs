//! The OAuth loopback flow end to end, offline. `HANDOFF.md` §2.2.
//!
//! A client thread plays the browser's role — it connects to the loopback
//! listener the way Intuit's redirect would — while [`oauth::run_loopback`]
//! runs on the main thread; then the returned code is exchanged against the
//! fake Intuit server from `tests/support/mod.rs`, entirely on
//! `127.0.0.1`.
//!
//! Names and figures are invented (`HANDOFF.md` §2.6).

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::json;

use qbo_local::oauth::{self, AuthFlowError, OAuthConfig};

use support::{FakeRequest, FakeResponse};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[test]
fn the_loopback_listener_receives_a_code_from_a_client_thread() {
    let port = free_port();

    let client = std::thread::spawn(move || {
        // Give the listener a moment to bind before connecting — the same
        // race a real browser redirect would never hit, since the user only
        // clicks through after the listener is already up, but a test
        // thread can race ahead of it.
        std::thread::sleep(Duration::from_millis(50));
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = "GET /callback?code=auth-code-123&state=state-xyz&realmId=1234567890123456 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        stream.write_all(request.as_bytes()).unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).ok();
        response
    });

    let auth_code = oauth::run_loopback(port, Duration::from_secs(5)).unwrap();
    assert_eq!(auth_code.code, "auth-code-123");
    assert_eq!(auth_code.state, "state-xyz");
    assert_eq!(auth_code.realm_id, "1234567890123456");

    let response = client.join().unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "response was: {response}"
    );
    assert!(
        response.contains("close this window"),
        "response was: {response}"
    );
}

#[test]
fn a_missing_code_is_reported_rather_than_hanging() {
    let port = free_port();

    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?state=state-xyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
    });

    let result = oauth::run_loopback(port, Duration::from_secs(5));
    assert!(matches!(result, Err(AuthFlowError::MissingCode)));
    client.join().unwrap();
}

#[test]
fn exchanging_a_code_against_the_fake_server_yields_a_token_set() {
    let server = support::start(|request: &FakeRequest| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/oauth2/v1/tokens/bearer");
        assert!(
            request
                .headers
                .get("authorization")
                .is_some_and(|h| h.starts_with("Basic ")),
            "expected HTTP Basic auth on the token request"
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/x-www-form-urlencoded")
        );
        FakeResponse::json(
            200,
            json!({
                "access_token": "access-xyz",
                "refresh_token": "refresh-xyz",
                "expires_in": 3600,
                "x_refresh_token_expires_in": 100 * 24 * 3600,
            }),
        )
    });

    let config = OAuthConfig {
        client_id: "client-123".to_string(),
        client_secret: "secret-abc".to_string(),
        redirect_uri: "http://localhost:8765/callback".to_string(),
        scopes: "com.intuit.quickbooks.accounting".to_string(),
        authorize_url: format!("{}/connect/oauth2", server.base_url),
        token_url: format!("{}/oauth2/v1/tokens/bearer", server.base_url),
    };
    let agent = ureq::AgentBuilder::new().build();

    let token_set = oauth::exchange_code(&config, "auth-code-123", &agent).unwrap();
    assert_eq!(token_set.access_token, "access-xyz");
    assert_eq!(token_set.refresh_token, "refresh-xyz");
    assert_eq!(token_set.access_expires_in_secs, 3600);
    assert_eq!(token_set.refresh_expires_in_secs, 100 * 24 * 3600);
}

#[test]
fn refreshing_against_the_fake_server_yields_a_new_token_set() {
    let server = support::start(|request: &FakeRequest| {
        assert_eq!(request.path, "/oauth2/v1/tokens/bearer");
        FakeResponse::json(
            200,
            json!({
                "access_token": "access-rotated",
                "refresh_token": "refresh-rotated",
                "expires_in": 3600,
                "x_refresh_token_expires_in": 100 * 24 * 3600,
            }),
        )
    });

    let config = OAuthConfig {
        client_id: "client-123".to_string(),
        client_secret: "secret-abc".to_string(),
        redirect_uri: "http://localhost:8765/callback".to_string(),
        scopes: "com.intuit.quickbooks.accounting".to_string(),
        authorize_url: format!("{}/connect/oauth2", server.base_url),
        token_url: format!("{}/oauth2/v1/tokens/bearer", server.base_url),
    };
    let agent = ureq::AgentBuilder::new().build();

    let token_set = oauth::refresh(&config, "refresh-old", &agent).unwrap();
    assert_eq!(token_set.access_token, "access-rotated");
    assert_eq!(token_set.refresh_token, "refresh-rotated");
}

#[test]
fn a_token_endpoint_error_is_reported_with_its_body() {
    let server = support::start(|_: &FakeRequest| {
        FakeResponse::json(
            400,
            json!({ "error": "invalid_grant", "error_description": "code expired" }),
        )
    });

    let config = OAuthConfig {
        client_id: "client-123".to_string(),
        client_secret: "secret-abc".to_string(),
        redirect_uri: "http://localhost:8765/callback".to_string(),
        scopes: "com.intuit.quickbooks.accounting".to_string(),
        authorize_url: format!("{}/connect/oauth2", server.base_url),
        token_url: format!("{}/oauth2/v1/tokens/bearer", server.base_url),
    };
    let agent = ureq::AgentBuilder::new().build();

    let result = oauth::exchange_code(&config, "stale-code", &agent);
    match result {
        Err(AuthFlowError::TokenEndpoint { status, body }) => {
            assert_eq!(status, 400);
            assert!(body.contains("invalid_grant"), "body was: {body}");
        }
        other => panic!("expected TokenEndpoint, got {other:?}"),
    }
}
