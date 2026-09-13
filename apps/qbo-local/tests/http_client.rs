//! `HttpQboClient` against a fake Intuit server (`tests/support/mod.rs`),
//! entirely offline — a std `TcpListener` on `127.0.0.1` never leaves the
//! loopback interface. `HANDOFF.md` §2.3.
//!
//! Covers the mapping table in `http.rs`'s module doc one behaviour at a
//! time, then proves [`SyncDriver`] itself against real HTTP rather than only
//! [`MockQbo`] — the point of building a fake server instead of only unit
//! tests on the parsing helpers.
//!
//! Names and figures are invented (`HANDOFF.md` §2.6).

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use qbo_local::auth::{FileTokenStore, RotationLogEntry, TokenGenerations, TokenSet, TokenStore};
use qbo_local::client::{QboClient, QboError, ReportName, ReportParams};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::driver::SyncDriver;
use qbo_local::http::HttpQboClient;
use qbo_local::oauth::OAuthConfig;
use qbo_local::reports;
use qbo_local::store::{ProjectedTable, Store};

use support::{entity, trial_balance_response, FakeRequest, FakeResponse, FakeServer};

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

/// Seed a token generation into a fresh [`FileTokenStore`] under `directory`,
/// then build an [`HttpQboClient`] pointed at `server` for both the API and
/// the OAuth token endpoint.
fn client_with_token(
    server: &FakeServer,
    directory: &std::path::Path,
    access_token: &str,
    obtained_at: DateTime<Utc>,
    access_expires_in_secs: i64,
) -> HttpQboClient {
    let token_set = TokenSet {
        access_token: access_token.to_string(),
        refresh_token: "refresh-0".to_string(),
        obtained_at,
        access_expires_in_secs,
        refresh_expires_in_secs: 100 * 24 * 3600,
    };
    let tokens_dir = directory.join("tokens");
    let store = FileTokenStore::new(&tokens_dir);
    store
        .save(&TokenGenerations::new(realm(), token_set))
        .unwrap();

    let oauth_config = OAuthConfig {
        client_id: "client-123".to_string(),
        client_secret: "secret-abc".to_string(),
        redirect_uri: "http://localhost:8765/callback".to_string(),
        scopes: "com.intuit.quickbooks.accounting".to_string(),
        authorize_url: format!("{}/connect/oauth2", server.base_url),
        token_url: format!("{}/oauth2/v1/tokens/bearer", server.base_url),
    };
    let token_store: Box<dyn TokenStore> = Box::new(FileTokenStore::new(&tokens_dir));
    let tokens = qbo_local::http::TokenSource::new(token_store, oauth_config, realm());
    HttpQboClient::new(
        server.base_url.clone(),
        tokens,
        directory.join("rotation-log.jsonl"),
    )
}

/// Fresh enough that `needs_refresh` is false — most tests don't want token
/// rotation in the picture at all.
fn fresh_client(server: &FakeServer, directory: &std::path::Path) -> HttpQboClient {
    client_with_token(server, directory, "token-fresh", Utc::now(), 3600)
}

fn extract_after(text: &str, marker: &str) -> Option<usize> {
    let index = text.find(marker)?;
    text[index + marker.len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn stale_sync_token_fault() -> Value {
    json!({ "Fault": { "Error": [ { "Message": "Stale Object Error", "Detail": "stale", "code": "5010" } ], "type": "ValidationFault" } })
}

fn validation_fault() -> Value {
    json!({ "Fault": { "Error": [ { "Message": "Invalid Reference Id", "Detail": "bad ref", "code": "6140" } ], "type": "ValidationFault" } })
}

// ---------------------------------------------------------------------------

#[test]
fn query_pages_and_converts_the_zero_based_start_to_intuits_one_based_startposition() {
    let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&queries);

    let server = support::start(move |request: &FakeRequest| {
        assert_eq!(request.method, "GET");
        let query = request.query.get("query").cloned().unwrap_or_default();
        captured.lock().unwrap().push(query.clone());
        let start = extract_after(&query, "STARTPOSITION").unwrap();
        let max = extract_after(&query, "MAXRESULTS").unwrap();
        let items: Vec<Value> = (start..start + max)
            .filter(|id| (1..=5).contains(id))
            .map(|id| {
                entity(
                    &id.to_string(),
                    "0",
                    json!({ "DocNumber": id.to_string() }),
                    "2026-01-01T00:00:00Z",
                )
            })
            .collect();
        FakeResponse::json(200, json!({ "QueryResponse": { "Invoice": items } }))
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let first_page = client
        .query(&realm(), EntityType::Invoice, None, 0, 2)
        .unwrap();
    assert_eq!(
        first_page
            .iter()
            .map(|p| p.qbo_id.as_str())
            .collect::<Vec<_>>(),
        ["1", "2"]
    );

    let second_page = client
        .query(&realm(), EntityType::Invoice, None, 2, 2)
        .unwrap();
    assert_eq!(
        second_page
            .iter()
            .map(|p| p.qbo_id.as_str())
            .collect::<Vec<_>>(),
        ["3", "4"]
    );

    let seen = queries.lock().unwrap();
    assert!(
        seen[0].contains("STARTPOSITION 1 MAXRESULTS 2"),
        "query was: {}",
        seen[0]
    );
    assert!(
        seen[1].contains("STARTPOSITION 3 MAXRESULTS 2"),
        "query was: {}",
        seen[1]
    );
}

#[test]
fn cdc_maps_a_deleted_status_to_is_deleted() {
    let server = support::start(|request: &FakeRequest| {
        assert_eq!(
            request.path,
            format!("/v3/company/{}/cdc", realm().as_str())
        );
        assert!(request.query.contains_key("changedSince"));
        let mut deleted = entity("20", "1", json!({}), "2026-01-02T00:00:00Z");
        deleted
            .as_object_mut()
            .unwrap()
            .insert("status".to_string(), Value::String("Deleted".to_string()));
        FakeResponse::json(
            200,
            json!({
                "CDCResponse": [ { "QueryResponse": [
                    { "Customer": [ entity("10", "0", json!({ "DisplayName": "Blue Harbor" }), "2026-01-02T00:00:00Z") ] },
                    { "Invoice": [ deleted ] },
                ] } ]
            }),
        )
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let changed = client
        .cdc(
            &realm(),
            &[EntityType::Customer, EntityType::Invoice],
            Utc::now() - Duration::days(1),
        )
        .unwrap();

    let by_id: HashMap<&str, bool> = changed
        .iter()
        .map(|p| (p.qbo_id.as_str(), p.is_deleted))
        .collect();
    assert_eq!(by_id.get("10"), Some(&false));
    assert_eq!(by_id.get("20"), Some(&true));
}

#[test]
fn create_replays_the_same_id_on_a_retry_with_the_same_request_id_after_a_500() {
    let created: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    let state = Arc::clone(&created);

    let server = support::start(move |request: &FakeRequest| {
        let request_id = request.query.get("requestid").cloned().unwrap_or_default();
        let mut store = state.lock().unwrap();
        if let Some(existing) = store.get(&request_id) {
            return FakeResponse::json(200, json!({ "Invoice": existing }));
        }
        // The request reaches the server and is persisted, but the response
        // never makes it back — exactly the case `RequestId` exists for.
        let created = entity("500", "0", request.body.clone(), "2026-01-03T00:00:00Z");
        store.insert(request_id, created);
        FakeResponse::json(
            500,
            json!({ "Fault": { "Error": [ { "Message": "internal error" } ] } }),
        )
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());
    let request_id = Uuid::now_v7();
    let payload = json!({ "DocNumber": "1099" });

    let first = client.create(&realm(), EntityType::Invoice, &payload, request_id);
    assert!(
        matches!(first, Err(QboError::Network(_))),
        "expected a 500 to map to Network, got {first:?}"
    );

    let second = client
        .create(&realm(), EntityType::Invoice, &payload, request_id)
        .unwrap();
    assert_eq!(
        second.qbo_id, "500",
        "retry with the same RequestId must return the same id"
    );
}

#[test]
fn update_sends_the_sync_token_and_a_5010_fault_maps_to_stale_sync_token() {
    let captured_body: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let state = Arc::clone(&captured_body);

    let server = support::start(move |request: &FakeRequest| {
        *state.lock().unwrap() = Some(request.body.clone());
        FakeResponse::json(400, stale_sync_token_fault())
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.update(
        &realm(),
        EntityType::Invoice,
        "418",
        &json!({ "TotalAmt": 50 }),
        "5",
        Uuid::now_v7(),
    );
    assert_eq!(result, Err(QboError::StaleSyncToken));

    let body = captured_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["Id"], "418");
    assert_eq!(body["SyncToken"], "5");
    assert_eq!(body["sparse"], false);
}

#[test]
fn a_429_response_is_rate_limited() {
    let server = support::start(|_: &FakeRequest| FakeResponse::json(429, json!({})));
    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.query(&realm(), EntityType::Invoice, None, 0, 10);
    assert_eq!(result, Err(QboError::RateLimited));
}

#[test]
fn a_plain_400_maps_to_validation_with_the_fault_detail() {
    let server = support::start(|_: &FakeRequest| FakeResponse::json(400, validation_fault()));
    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.create(&realm(), EntityType::Invoice, &json!({}), Uuid::now_v7());
    match result {
        Err(QboError::Validation(detail)) => assert!(
            detail.contains("Invalid Reference Id"),
            "detail was: {detail}"
        ),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn a_404_on_update_is_not_found() {
    let server = support::start(|_: &FakeRequest| FakeResponse::json(404, json!({})));
    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.update(
        &realm(),
        EntityType::Invoice,
        "999",
        &json!({}),
        "0",
        Uuid::now_v7(),
    );
    assert_eq!(result, Err(QboError::NotFound));
}

#[test]
fn fetch_of_a_missing_id_is_ok_none_rather_than_an_error() {
    let server = support::start(|_: &FakeRequest| FakeResponse::json(404, json!({})));
    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.fetch(&realm(), EntityType::Invoice, "999").unwrap();
    assert!(result.is_none());
}

#[test]
fn find_by_name_escapes_an_embedded_quote() {
    let queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&queries);

    let server = support::start(move |request: &FakeRequest| {
        captured
            .lock()
            .unwrap()
            .push(request.query.get("query").cloned().unwrap_or_default());
        let found = entity(
            "31",
            "0",
            json!({ "DisplayName": "O'Brien Aquatics" }),
            "2026-01-01T00:00:00Z",
        );
        FakeResponse::json(200, json!({ "QueryResponse": { "Customer": [ found ] } }))
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let found = client
        .find_by_name(&realm(), EntityType::Customer, "O'Brien Aquatics")
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().raw_json["DisplayName"], "O'Brien Aquatics");

    let sent = queries.lock().unwrap();
    assert!(
        sent[0].contains("DisplayName = 'O''Brien Aquatics'"),
        "query was: {}",
        sent[0]
    );
}

#[test]
fn reactive_refresh_on_401_retries_once_and_persists_the_new_generation() {
    let api_calls: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let seen_bearer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let api_state = Arc::clone(&api_calls);
    let bearer_state = Arc::clone(&seen_bearer);

    let server = support::start(move |request: &FakeRequest| {
        if request.path == "/oauth2/v1/tokens/bearer" {
            assert!(request
                .headers
                .get("authorization")
                .is_some_and(|h| h.starts_with("Basic ")));
            return FakeResponse::json(
                200,
                json!({
                    "access_token": "token-rotated",
                    "refresh_token": "refresh-rotated",
                    "expires_in": 3600,
                    "x_refresh_token_expires_in": 100 * 24 * 3600,
                }),
            );
        }

        bearer_state.lock().unwrap().push(
            request
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default(),
        );
        let mut calls = api_state.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FakeResponse::json(401, json!({}))
        } else {
            FakeResponse::json(200, json!({ "QueryResponse": { "Invoice": [] } }))
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client
        .query(&realm(), EntityType::Invoice, None, 0, 10)
        .unwrap();
    assert!(result.is_empty());

    let bearers = seen_bearer.lock().unwrap();
    assert_eq!(
        bearers.len(),
        2,
        "expected the failed attempt and the retry"
    );
    assert_eq!(
        bearers[1], "Bearer token-rotated",
        "the retry must use the refreshed token"
    );

    let store = FileTokenStore::new(directory.path().join("tokens"));
    let generations = store.load(&realm()).unwrap().unwrap();
    assert_eq!(generations.current.access_token, "token-rotated");
    assert_eq!(generations.previous[0].access_token, "token-fresh");

    let log_lines: Vec<RotationLogEntry> =
        std::fs::read_to_string(directory.path().join("rotation-log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(log_lines.len(), 1);
    assert_eq!(log_lines[0].outcome, "reactive");
}

#[test]
fn proactive_refresh_fires_when_the_token_is_within_the_refresh_window() {
    let api_calls: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let seen_bearer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let api_state = Arc::clone(&api_calls);
    let bearer_state = Arc::clone(&seen_bearer);

    let server = support::start(move |request: &FakeRequest| {
        if request.path == "/oauth2/v1/tokens/bearer" {
            *api_state.lock().unwrap() += 1;
            return FakeResponse::json(
                200,
                json!({
                    "access_token": "token-proactive",
                    "refresh_token": "refresh-proactive",
                    "expires_in": 3600,
                    "x_refresh_token_expires_in": 100 * 24 * 3600,
                }),
            );
        }
        bearer_state.lock().unwrap().push(
            request
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default(),
        );
        FakeResponse::json(200, json!({ "QueryResponse": { "Invoice": [] } }))
    });

    let directory = tempfile::tempdir().unwrap();
    // 55 minutes into a 60-minute token: inside the 10-minute refresh margin
    // (`auth::REFRESH_MARGIN_MINUTES`), so this must refresh before the call
    // ever reaches the API, not after a 401.
    let mut client = client_with_token(
        &server,
        directory.path(),
        "token-about-to-expire",
        Utc::now() - Duration::minutes(55),
        3600,
    );

    client
        .query(&realm(), EntityType::Invoice, None, 0, 10)
        .unwrap();

    assert_eq!(
        *api_calls.lock().unwrap(),
        1,
        "expected exactly one refresh call"
    );
    let bearers = seen_bearer.lock().unwrap();
    assert_eq!(
        *bearers,
        ["Bearer token-proactive".to_string()],
        "the only API call should already carry the refreshed token"
    );

    let log_lines: Vec<RotationLogEntry> =
        std::fs::read_to_string(directory.path().join("rotation-log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert_eq!(log_lines.len(), 1);
    assert_eq!(log_lines[0].outcome, "proactive");
}

// ---------------------------------------------------------------------------
// The sync driver, proven against HTTP rather than only MockQbo.
// ---------------------------------------------------------------------------

#[test]
fn the_sync_driver_runs_end_to_end_against_the_fake_server() {
    let server = support::start(|request: &FakeRequest| {
        let query = request.query.get("query").cloned().unwrap_or_default();
        if query.contains("FROM Customer") {
            let items = vec![
                entity(
                    "31",
                    "0",
                    json!({ "DisplayName": "Blue Harbor Swim Club" }),
                    "2026-08-01T00:00:00Z",
                ),
                entity(
                    "32",
                    "0",
                    json!({ "DisplayName": "Fairview Parks & Rec" }),
                    "2026-08-01T00:00:00Z",
                ),
            ];
            FakeResponse::json(200, json!({ "QueryResponse": { "Customer": items } }))
        } else if query.contains("FROM Invoice") {
            let items = vec![
                entity(
                    "418",
                    "0",
                    invoice_fields("1088", 1234.56),
                    "2026-08-01T01:00:00Z",
                ),
                entity(
                    "419",
                    "0",
                    invoice_fields("1089", 88.0),
                    "2026-08-01T01:00:00Z",
                ),
                entity(
                    "420",
                    "0",
                    invoice_fields("1090", 250.0),
                    "2026-08-01T01:00:00Z",
                ),
            ];
            FakeResponse::json(200, json!({ "QueryResponse": { "Invoice": items } }))
        } else {
            FakeResponse::json(200, json!({ "QueryResponse": {} }))
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let http_client = fresh_client(&server, directory.path());

    let store = Store::open_in_memory().unwrap();
    store
        .register_realm(&realm(), "Test Company", Utc::now())
        .unwrap();

    let mut driver = SyncDriver::new(
        http_client,
        qbo_local::driver::SyncOptions::default(),
        Utc::now(),
    );
    let report = driver
        .sync_realm(
            &store,
            &realm(),
            &[EntityType::Invoice, EntityType::Customer],
            Utc::now(),
        )
        .unwrap();

    assert_eq!(report.mirrored(), 5, "2 customers + 3 invoices");
    assert_eq!(
        store
            .count_projected(&realm(), ProjectedTable::Contacts)
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_projected(&realm(), ProjectedTable::Documents)
            .unwrap(),
        3
    );

    let invoice = store.get_document(&realm(), "418").unwrap().unwrap();
    assert_eq!(invoice.doc_number.as_deref(), Some("1088"));
    assert_eq!(
        invoice.contact_name.as_deref(),
        Some("Blue Harbor Swim Club")
    );
}

// ---------------------------------------------------------------------------
// Reports API (LEDGER-DESIGN.md §6, §7)
// ---------------------------------------------------------------------------

type CapturedRequest = Option<(String, HashMap<String, String>)>;

#[test]
fn report_hits_the_reports_path_with_the_given_params_and_parses() {
    let captured: Arc<Mutex<CapturedRequest>> = Arc::new(Mutex::new(None));
    let state = Arc::clone(&captured);

    let server = support::start(move |request: &FakeRequest| {
        *state.lock().unwrap() = Some((request.path.clone(), request.query.clone()));
        FakeResponse::json(
            200,
            trial_balance_response(&[
                ("35", "Checking", "412884.19", ""),
                ("61", "Sales Tax Payable", "", "8412.06"),
            ]),
        )
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());
    let params = ReportParams {
        start_date: Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
        end_date: Some(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap()),
        accounting_method: Some("Accrual".to_string()),
        date_macro: None,
        aging_method: None,
    };

    let value = client
        .report(&realm(), ReportName::TrialBalance, &params)
        .unwrap();

    let (path, query) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        path,
        format!("/v3/company/{}/reports/TrialBalance", realm().as_str())
    );
    assert_eq!(query.get("start_date").map(String::as_str), Some("2026-01-01"));
    assert_eq!(query.get("end_date").map(String::as_str), Some("2026-09-10"));
    assert_eq!(
        query.get("accounting_method").map(String::as_str),
        Some("Accrual")
    );
    assert!(!query.contains_key("date_macro"));
    assert!(!query.contains_key("aging_method"));

    let rows = reports::parse_trial_balance(&value).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "Checking");
    assert_eq!(rows[0].qbo_account_id.as_deref(), Some("35"));
}

#[test]
fn report_only_sends_the_params_that_are_given() {
    let captured: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
    let state = Arc::clone(&captured);

    let server = support::start(move |request: &FakeRequest| {
        *state.lock().unwrap() = Some(request.query.clone());
        FakeResponse::json(200, trial_balance_response(&[]))
    });

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    client
        .report(&realm(), ReportName::AgedReceivables, &ReportParams::default())
        .unwrap();

    let query = captured.lock().unwrap().clone().unwrap();
    for absent in ["start_date", "end_date", "accounting_method", "date_macro", "aging_method"] {
        assert!(!query.contains_key(absent), "unexpected {absent} in {query:?}");
    }
}

#[test]
fn a_report_fault_maps_the_same_way_as_every_other_call() {
    let server = support::start(|_: &FakeRequest| FakeResponse::json(500, json!({})));

    let directory = tempfile::tempdir().unwrap();
    let mut client = fresh_client(&server, directory.path());

    let result = client.report(&realm(), ReportName::TrialBalance, &ReportParams::default());
    assert!(matches!(result, Err(QboError::Network(_))), "{result:?}");
}

fn invoice_fields(doc_number: &str, total: f64) -> Value {
    json!({
        "DocNumber": doc_number,
        "TxnDate": "2026-07-14",
        "CustomerRef": { "value": "31" },
        "TotalAmt": total,
        "Line": [ { "LineNum": 1, "Description": "Rescue tube, 50 inch", "Amount": total,
                     "DetailType": "SalesItemLineDetail",
                     "SalesItemLineDetail": { "ItemRef": { "value": "12" } } } ]
    })
}
