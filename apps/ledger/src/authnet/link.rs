//! The hosted payment link on the own invoice. D15 (customer payments move
//! to Authorize.net at cutover), D30 (this design), `docs/AUTHNET.md`.
//!
//! Authorize.net's Accept Hosted: the ledger asks the gateway for a form
//! token (`getHostedPaymentPageRequest`) carrying the invoice's own number as
//! `order.invoiceNumber` and its open balance as `amount`; the customer's
//! browser POSTs that token to the hosted form, pays there, and the card data
//! never touches this machine. When the batch settles, the Merchant
//! Interface export carries the same number in its `Invoice Number` column,
//! and [`super::import_settlement`] applies the payment to this invoice — the
//! loop this module closes is: number out on the request, number back on the
//! settlement row.
//!
//! **The token is short-lived and must be POSTed** (D30): Authorize.net
//! documents the form token as valid for 15 minutes and delivered by an HTML
//! form POST, never a GET. So [`PaymentLink`] is not a URL that can be
//! printed on a PDF and clicked a week later. It is the form URL, the token,
//! its expiry, and a self-submitting HTML page ([`PaymentLink::html`]) that
//! does the POST — open it within 15 minutes, or hand it to the customer
//! straight away. A durable "Pay now" link on a sent invoice needs a small
//! endpoint that mints a fresh token at click time; that is a follow-up, and
//! [`payment_link`] is the piece it would call.
//!
//! [`HostedPaymentClient`] is the seam, the same shape as
//! `qbo_local::client::QboClient`: [`HttpAuthnetClient`] speaks to the real
//! gateway (sandbox unless production is explicitly enabled), [`MockAuthnet`]
//! records requests and hands back tokens so the whole flow runs offline.
//!
//! **Credentials** (API Login ID and Transaction Key) never live in the
//! repository or in any config file: [`EnvCredentials`] reads
//! `AUTHNET_API_LOGIN_ID` / `AUTHNET_TRANSACTION_KEY`, and on macOS
//! `KeychainCredentials` reads one login-keychain item per environment.
//! [`Credentials`]' `Debug` redacts both, and no error this module builds
//! ever includes them.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ledger_core::Money;

use crate::books::{self, Book, BooksError};
use crate::store::{Ledger, LedgerError};
use crate::types::{ContactKind, DocKind, LedgerDocument};

/// How long a form token is good for. Authorize.net: "The form token is
/// valid for 15 minutes."
pub const TOKEN_LIFETIME_MINUTES: i64 = 15;

/// `order.invoiceNumber`, `refId` and `customer.id` all cap at 20 characters.
pub const MAX_INVOICE_NUMBER_LEN: usize = 20;
const MAX_CUSTOMER_ID_LEN: usize = 20;
const MAX_DESCRIPTION_LEN: usize = 255;

/// The login-keychain service credentials are filed under, one item per
/// environment (account `sandbox` or `production`), data the JSON
/// `{"api_login_id": "...", "transaction_key": "..."}`.
pub const KEYCHAIN_SERVICE: &str = "com.aquamentor.ledger.authnet";

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("no document {0}")]
    UnknownDocument(String),
    #[error(
        "document {document_id} is a {kind}, not an invoice; only an invoice gets a payment link"
    )]
    NotAnInvoice {
        document_id: String,
        kind: &'static str,
    },
    #[error("invoice {0} is voided")]
    Voided(String),
    #[error("invoice {0} is fully paid; nothing is open to collect")]
    FullyPaid(String),
    #[error("document {0} has no invoice number; the settlement import could not match the payment back")]
    MissingNumber(String),
    #[error(
        "invoice number {0:?} is longer than Authorize.net's 20-character invoiceNumber; \
         the settlement import could not match it back"
    )]
    NumberTooLong(String),
    #[error(
        "production is not enabled: set environment = \"production\" and \
         allow_production = true under [authnet] in the config file"
    )]
    ProductionDisabled,
    #[error("Authorize.net credentials for {environment} not found: {hint}")]
    MissingCredentials {
        environment: &'static str,
        hint: String,
    },
    #[error("Authorize.net refused the request: {code} {text}")]
    Gateway { code: String, text: String },
    #[error("Authorize.net returned HTTP {0}")]
    Http(u16),
    #[error("could not reach Authorize.net: {0}")]
    Network(String),
    #[error("unexpected response from Authorize.net: {0}")]
    BadResponse(String),
    #[error("config {path}: {reason}")]
    Config { path: String, reason: String },
    #[error("keychain: {0}")]
    Keychain(String),
    #[error(transparent)]
    Books(#[from] BooksError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Environment and configuration
// ---------------------------------------------------------------------------

/// Sandbox or production. Each names its own API endpoint and hosted form.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Sandbox,
    Production,
}

impl Environment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Environment::Sandbox => "sandbox",
            Environment::Production => "production",
        }
    }

    /// The JSON/XML API endpoint every request is POSTed to.
    pub const fn api_url(self) -> &'static str {
        match self {
            Environment::Sandbox => "https://apitest.authorize.net/xml/v1/request.api",
            Environment::Production => "https://api.authorize.net/xml/v1/request.api",
        }
    }

    /// The hosted payment form the customer's browser POSTs the token to.
    pub const fn form_url(self) -> &'static str {
        match self {
            Environment::Sandbox => "https://test.authorize.net/payment/payment",
            Environment::Production => "https://accept.authorize.net/payment/payment",
        }
    }
}

/// The `[authnet]` table of `.local/config.toml` (gitignored). Every field
/// has a default, so a missing file or table means sandbox with Aquamentor's
/// name on the receipt. Unknown keys are refused, so a `transaction_key`
/// written here by mistake is an error rather than silently read.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkConfig {
    #[serde(default)]
    pub environment: Environment,
    /// Production also needs this, so a one-word edit can't start taking
    /// real cards.
    #[serde(default)]
    pub allow_production: bool,
    #[serde(default = "default_merchant_name")]
    pub merchant_name: String,
    /// Where the hosted receipt's Continue button goes. Omitted, the hosted
    /// receipt is the last page.
    #[serde(default)]
    pub return_url: Option<String>,
    #[serde(default)]
    pub cancel_url: Option<String>,
}

fn default_merchant_name() -> String {
    "Aquamentor".to_string()
}

impl Default for LinkConfig {
    fn default() -> Self {
        LinkConfig {
            environment: Environment::Sandbox,
            allow_production: false,
            merchant_name: default_merchant_name(),
            return_url: None,
            cancel_url: None,
        }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    authnet: Option<toml::Value>,
}

impl LinkConfig {
    /// Reads `[authnet]` from `path`. A missing file is the default config;
    /// a present but unreadable one is an error. The rest of the file
    /// (`qbo-local`'s `[intuit]`, `[[realms]]`) is ignored here.
    pub fn load(path: &Path) -> Result<LinkConfig, LinkError> {
        let config_err = |reason: String| LinkError::Config {
            path: path.display().to_string(),
            reason,
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LinkConfig::default())
            }
            Err(err) => return Err(config_err(err.to_string())),
        };
        LinkConfig::parse(&text).map_err(config_err)
    }

    pub fn parse(text: &str) -> Result<LinkConfig, String> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| e.to_string())?;
        match file.authnet {
            None => Ok(LinkConfig::default()),
            Some(table) => {
                if table.get("transaction_key").is_some() || table.get("api_login_id").is_some() {
                    return Err("credentials are never read from a file: use the keychain \
                         (`ledger authnet credentials`) or AUTHNET_API_LOGIN_ID / \
                         AUTHNET_TRANSACTION_KEY"
                        .to_string());
                }
                table.try_into().map_err(|e: toml::de::Error| e.to_string())
            }
        }
    }

    /// The environment requests go to, refusing production unless both
    /// switches are set.
    pub fn checked_environment(&self) -> Result<Environment, LinkError> {
        match self.environment {
            Environment::Production if !self.allow_production => Err(LinkError::ProductionDisabled),
            env => Ok(env),
        }
    }
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// API Login ID and Transaction Key. `Debug` never prints either.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Credentials {
    pub api_login_id: String,
    pub transaction_key: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { api_login_id: <redacted>, transaction_key: <redacted> }")
    }
}

/// Where [`HttpAuthnetClient`] gets its credentials, read at request time so
/// a server can start without them and a rotated key takes effect without a
/// restart.
pub trait CredentialSource {
    fn load(&self, environment: Environment) -> Result<Credentials, LinkError>;
}

/// `AUTHNET_API_LOGIN_ID` and `AUTHNET_TRANSACTION_KEY`.
pub struct EnvCredentials;

impl CredentialSource for EnvCredentials {
    fn load(&self, environment: Environment) -> Result<Credentials, LinkError> {
        match (
            std::env::var("AUTHNET_API_LOGIN_ID"),
            std::env::var("AUTHNET_TRANSACTION_KEY"),
        ) {
            (Ok(api_login_id), Ok(transaction_key))
                if !api_login_id.is_empty() && !transaction_key.is_empty() =>
            {
                Ok(Credentials {
                    api_login_id,
                    transaction_key,
                })
            }
            _ => Err(LinkError::MissingCredentials {
                environment: environment.as_str(),
                hint: "AUTHNET_API_LOGIN_ID / AUTHNET_TRANSACTION_KEY are not set".to_string(),
            }),
        }
    }
}

/// Fixed credentials, for tests and for a caller that already holds them.
pub struct StaticCredentials(pub Credentials);

impl CredentialSource for StaticCredentials {
    fn load(&self, _environment: Environment) -> Result<Credentials, LinkError> {
        Ok(self.0.clone())
    }
}

/// The first source that has credentials wins; the error names every miss.
pub struct ChainCredentials(pub Vec<Box<dyn CredentialSource>>);

impl CredentialSource for ChainCredentials {
    fn load(&self, environment: Environment) -> Result<Credentials, LinkError> {
        let mut misses = Vec::new();
        for source in &self.0 {
            match source.load(environment) {
                Ok(credentials) => return Ok(credentials),
                Err(LinkError::MissingCredentials { hint, .. }) => misses.push(hint),
                Err(other) => return Err(other),
            }
        }
        Err(LinkError::MissingCredentials {
            environment: environment.as_str(),
            hint: misses.join("; "),
        })
    }
}

/// The environment variables, then (on macOS) the login keychain.
pub fn default_credentials() -> Box<dyn CredentialSource> {
    #[allow(unused_mut)]
    let mut sources: Vec<Box<dyn CredentialSource>> = vec![Box::new(EnvCredentials)];
    #[cfg(target_os = "macos")]
    sources.push(Box::new(KeychainCredentials::new()));
    Box::new(ChainCredentials(sources))
}

#[cfg(target_os = "macos")]
pub use keychain::KeychainCredentials;

#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::passwords::{get_generic_password, set_generic_password};

    use super::{CredentialSource, Credentials, Environment, LinkError, KEYCHAIN_SERVICE};

    /// `errSecItemNotFound`.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    /// One generic-password item per environment under [`KEYCHAIN_SERVICE`],
    /// the same layout as `qbo_local::keychain::KeychainTokenStore`.
    pub struct KeychainCredentials {
        service: String,
    }

    impl KeychainCredentials {
        pub fn new() -> Self {
            Self::with_service(KEYCHAIN_SERVICE)
        }

        pub fn with_service(service: impl Into<String>) -> Self {
            KeychainCredentials {
                service: service.into(),
            }
        }

        pub fn save(
            &self,
            environment: Environment,
            credentials: &Credentials,
        ) -> Result<(), LinkError> {
            let bytes = serde_json::to_vec(credentials)?;
            set_generic_password(&self.service, environment.as_str(), &bytes)
                .map_err(|e| LinkError::Keychain(format!("{e} (OSStatus {})", e.code())))
        }
    }

    impl Default for KeychainCredentials {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CredentialSource for KeychainCredentials {
        fn load(&self, environment: Environment) -> Result<Credentials, LinkError> {
            match get_generic_password(&self.service, environment.as_str()) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
                    // Never echo the item's bytes: they are the secret.
                    LinkError::Keychain(format!(
                        "item {}/{} is not {{\"api_login_id\",\"transaction_key\"}} JSON",
                        self.service,
                        environment.as_str()
                    ))
                }),
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {
                    Err(LinkError::MissingCredentials {
                        environment: environment.as_str(),
                        hint: format!(
                            "no keychain item {}/{} (run `ledger authnet credentials --environment {}`)",
                            self.service,
                            environment.as_str(),
                            environment.as_str()
                        ),
                    })
                }
                Err(e) => Err(LinkError::Keychain(format!("{e} (OSStatus {})", e.code()))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// What the ledger asks the gateway for: everything comes from the invoice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRequest {
    pub document_id: String,
    /// The invoice's own number, sent as `order.invoiceNumber` — the key
    /// [`super::import_settlement`] matches the settled sale back on.
    pub invoice_number: String,
    /// The invoice's open balance.
    pub amount: Money,
    pub description: String,
    /// The ledger contact id, sent as `customer.id` when it fits the
    /// gateway's 20 alphanumeric characters (the settlement export's
    /// `Customer ID` column then names the same contact).
    pub customer_id: Option<String>,
    pub customer_email: Option<String>,
}

/// What the gateway hands back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedPage {
    pub token: String,
}

/// The seam: [`HttpAuthnetClient`] live, [`MockAuthnet`] offline.
pub trait HostedPaymentClient {
    fn environment(&self) -> Environment;
    fn hosted_payment_page(&mut self, request: &LinkRequest) -> Result<HostedPage, LinkError>;
}

impl<T: HostedPaymentClient + ?Sized> HostedPaymentClient for Box<T> {
    fn environment(&self) -> Environment {
        (**self).environment()
    }
    fn hosted_payment_page(&mut self, request: &LinkRequest) -> Result<HostedPage, LinkError> {
        (**self).hosted_payment_page(request)
    }
}

// The wire shapes. Authorize.net's JSON is translated from its XML schema
// and is order-sensitive, so these are structs (serde keeps field order),
// never a `serde_json::Map` (which sorts keys).

#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(rename = "getHostedPaymentPageRequest")]
    request: PageRequest<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageRequest<'a> {
    merchant_authentication: MerchantAuthentication<'a>,
    ref_id: &'a str,
    transaction_request: TransactionRequest<'a>,
    hosted_payment_settings: Settings,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MerchantAuthentication<'a> {
    name: &'a str,
    transaction_key: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionRequest<'a> {
    transaction_type: &'static str,
    amount: String,
    order: Order<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    customer: Option<Customer<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Order<'a> {
    invoice_number: &'a str,
    description: &'a str,
}

#[derive(Serialize)]
struct Customer<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
}

#[derive(Serialize)]
struct Settings {
    setting: Vec<Setting>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Setting {
    setting_name: &'static str,
    /// A JSON object serialised to a string, as the API expects.
    setting_value: String,
}

/// `amount` on the wire: a plain two-place decimal, no currency symbol.
pub fn amount_str(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// The `getHostedPaymentPageRequest` body, exactly as it goes on the wire.
/// Holds the credentials — never log it.
pub fn request_body(
    credentials: &Credentials,
    request: &LinkRequest,
    config: &LinkConfig,
) -> Result<String, LinkError> {
    let mut return_options = serde_json::Map::new();
    return_options.insert("showReceipt".into(), Value::Bool(true));
    if let Some(url) = &config.return_url {
        return_options.insert("url".into(), Value::String(url.clone()));
        return_options.insert("urlText".into(), Value::String("Continue".into()));
    }
    if let Some(url) = &config.cancel_url {
        return_options.insert("cancelUrl".into(), Value::String(url.clone()));
        return_options.insert("cancelUrlText".into(), Value::String("Cancel".into()));
    }
    let settings = vec![
        Setting {
            setting_name: "hostedPaymentReturnOptions",
            setting_value: Value::Object(return_options).to_string(),
        },
        Setting {
            setting_name: "hostedPaymentButtonOptions",
            setting_value: serde_json::json!({ "text": "Pay" }).to_string(),
        },
        Setting {
            setting_name: "hostedPaymentOrderOptions",
            setting_value: serde_json::json!({
                "show": true,
                "merchantName": config.merchant_name,
            })
            .to_string(),
        },
    ];
    let customer =
        (request.customer_id.is_some() || request.customer_email.is_some()).then_some(Customer {
            id: request.customer_id.as_deref(),
            email: request.customer_email.as_deref(),
        });
    let envelope = Envelope {
        request: PageRequest {
            merchant_authentication: MerchantAuthentication {
                name: &credentials.api_login_id,
                transaction_key: &credentials.transaction_key,
            },
            ref_id: &request.invoice_number,
            transaction_request: TransactionRequest {
                transaction_type: "authCaptureTransaction",
                amount: amount_str(request.amount),
                order: Order {
                    invoice_number: &request.invoice_number,
                    description: &request.description,
                },
                customer,
            },
            hosted_payment_settings: Settings { setting: settings },
        },
    };
    Ok(serde_json::to_string(&envelope)?)
}

/// Reads a `getHostedPaymentPageResponse`. Authorize.net's JSON responses
/// can start with a UTF-8 byte-order mark, which is stripped first.
pub fn parse_response(body: &str) -> Result<HostedPage, LinkError> {
    let body = body.trim_start_matches('\u{feff}');
    let value: Value =
        serde_json::from_str(body).map_err(|e| LinkError::BadResponse(e.to_string()))?;
    let messages = &value["messages"];
    let first = &messages["message"][0];
    let code = first["code"].as_str().unwrap_or("").to_string();
    let text = first["text"].as_str().unwrap_or("").to_string();
    match messages["resultCode"].as_str() {
        Some("Ok") => match value["token"].as_str() {
            Some(token) if !token.is_empty() => Ok(HostedPage {
                token: token.to_string(),
            }),
            _ => Err(LinkError::BadResponse(
                "resultCode Ok but no token".to_string(),
            )),
        },
        Some(_) => Err(LinkError::Gateway { code, text }),
        None => Err(LinkError::BadResponse("no messages.resultCode".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// The live client. Blocking `ureq`, as `qbo_local::http` is.
pub struct HttpAuthnetClient {
    environment: Environment,
    api_url: String,
    config: LinkConfig,
    credentials: Box<dyn CredentialSource>,
    agent: ureq::Agent,
}

impl HttpAuthnetClient {
    /// Refuses production unless the config allows it.
    pub fn new(
        config: LinkConfig,
        credentials: Box<dyn CredentialSource>,
    ) -> Result<Self, LinkError> {
        let environment = config.checked_environment()?;
        Ok(HttpAuthnetClient {
            environment,
            api_url: environment.api_url().to_string(),
            config,
            credentials,
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
        })
    }

    /// Points the client at another endpoint — a fake server under test.
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }
}

impl HostedPaymentClient for HttpAuthnetClient {
    fn environment(&self) -> Environment {
        self.environment
    }

    fn hosted_payment_page(&mut self, request: &LinkRequest) -> Result<HostedPage, LinkError> {
        let credentials = self.credentials.load(self.environment)?;
        let body = request_body(&credentials, request, &self.config)?;
        let response = self
            .agent
            .post(&self.api_url)
            .set("Content-Type", "application/json")
            .send_string(&body);
        match response {
            Ok(response) => {
                let text = response
                    .into_string()
                    .map_err(|e| LinkError::Network(e.to_string()))?;
                parse_response(&text)
            }
            Err(ureq::Error::Status(status, _)) => Err(LinkError::Http(status)),
            // A transport error's Display names the URL and the I/O cause,
            // never the request body.
            Err(ureq::Error::Transport(transport)) => {
                Err(LinkError::Network(transport.to_string()))
            }
        }
    }
}

/// Stands in when the config could not be loaded, so the server still
/// starts and `payment_link` says why it can't run instead of vanishing.
pub struct Unavailable(pub String);

impl HostedPaymentClient for Unavailable {
    fn environment(&self) -> Environment {
        Environment::Sandbox
    }

    fn hosted_payment_page(&mut self, _request: &LinkRequest) -> Result<HostedPage, LinkError> {
        Err(LinkError::Config {
            path: "[authnet]".to_string(),
            reason: self.0.clone(),
        })
    }
}

/// The live client `ledger` and `ledger-mcp` use: the config at `path`
/// (default `.local/config.toml`), credentials from the environment, then the
/// keychain. A config that can't be used becomes [`Unavailable`].
pub fn live_client(path: &Path) -> Box<dyn HostedPaymentClient> {
    match LinkConfig::load(path)
        .and_then(|config| HttpAuthnetClient::new(config, default_credentials()))
    {
        Ok(client) => Box::new(client),
        Err(err) => Box::new(Unavailable(err.to_string())),
    }
}

/// The config file `live_client` reads when no path is given:
/// `LEDGER_AUTHNET_CONFIG`, else `.local/config.toml`.
pub fn default_config_path() -> std::path::PathBuf {
    std::env::var_os("LEDGER_AUTHNET_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(".local/config.toml"))
}

/// The offline double: records every request (shared, so a test keeps a
/// handle after moving the mock into a server) and answers with a numbered
/// token, or with the failure queued by [`MockAuthnet::fail_next`].
#[derive(Clone, Default)]
pub struct MockAuthnet {
    environment: Environment,
    requests: Arc<Mutex<Vec<LinkRequest>>>,
    next_failure: Arc<Mutex<Option<LinkError>>>,
}

impl MockAuthnet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn requests(&self) -> Vec<LinkRequest> {
        self.requests.lock().expect("mock lock").clone()
    }

    pub fn fail_next(&self, error: LinkError) {
        *self.next_failure.lock().expect("mock lock") = Some(error);
    }
}

impl HostedPaymentClient for MockAuthnet {
    fn environment(&self) -> Environment {
        self.environment
    }

    fn hosted_payment_page(&mut self, request: &LinkRequest) -> Result<HostedPage, LinkError> {
        if let Some(error) = self.next_failure.lock().expect("mock lock").take() {
            return Err(error);
        }
        let mut requests = self.requests.lock().expect("mock lock");
        requests.push(request.clone());
        Ok(HostedPage {
            token: format!("mock-token-{}", requests.len()),
        })
    }
}

// ---------------------------------------------------------------------------
// From invoice to link
// ---------------------------------------------------------------------------

/// Late enough to count every payment ever applied (dates are stored as
/// ISO text, so this must stay a four-digit year).
fn all_time() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date")
}

fn is_gateway_customer_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CUSTOMER_ID_LEN
        && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Builds the request for `document_id`, refusing anything that is not an
/// open, numbered, non-voided invoice. The amount is the open balance after
/// every payment and credit applied to it so far.
pub fn link_request(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
) -> Result<LinkRequest, LinkError> {
    let payload = ledger
        .latest_document_payload(company, document_id)?
        .ok_or_else(|| LinkError::UnknownDocument(document_id.to_string()))?;
    let doc: LedgerDocument = serde_json::from_str(&payload)?;
    if doc.kind != DocKind::Invoice {
        return Err(LinkError::NotAnInvoice {
            document_id: document_id.to_string(),
            kind: doc.kind.as_str(),
        });
    }
    let label = doc
        .number
        .clone()
        .unwrap_or_else(|| document_id.to_string());
    if doc.is_voided {
        return Err(LinkError::Voided(label));
    }
    let number = doc
        .number
        .clone()
        .ok_or_else(|| LinkError::MissingNumber(document_id.to_string()))?;
    if number.len() > MAX_INVOICE_NUMBER_LEN {
        return Err(LinkError::NumberTooLong(number));
    }

    let open = books::open_documents(ledger, company, Book::Receivable, all_time())?
        .into_iter()
        .find(|open| open.document_id == document_id)
        .map(|open| open.open)
        .filter(|amount| amount.minor() > 0)
        .ok_or_else(|| LinkError::FullyPaid(label.clone()))?;

    let contact = match &doc.contact {
        Some(contact) if contact.kind == ContactKind::Customer => {
            books::contact(ledger, company, &contact.id)?
        }
        _ => None,
    };
    let customer_id = doc
        .contact
        .as_ref()
        .map(|c| c.id.clone())
        .filter(|id| is_gateway_customer_id(id));
    let customer_email = contact.as_ref().and_then(|c| c.email.clone());
    let mut description = match &contact {
        Some(c) => format!("Invoice {number} - {}", c.display_name),
        None => format!("Invoice {number}"),
    };
    if description.len() > MAX_DESCRIPTION_LEN {
        let mut cut = MAX_DESCRIPTION_LEN;
        while !description.is_char_boundary(cut) {
            cut -= 1;
        }
        description.truncate(cut);
    }

    Ok(LinkRequest {
        document_id: document_id.to_string(),
        invoice_number: number,
        amount: open,
        description,
        customer_id,
        customer_email,
    })
}

/// A minted payment link: everything a caller needs to send the customer to
/// the hosted form before the token expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentLink {
    pub document_id: String,
    pub invoice_number: String,
    pub amount: Money,
    pub environment: Environment,
    /// The hosted form. The token must be POSTed here as field `token`.
    pub url: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl PaymentLink {
    /// A page that POSTs the token to the hosted form as soon as it opens,
    /// with a button for a browser that blocks the script.
    pub fn html(&self) -> String {
        let number = html_escape(&self.invoice_number);
        format!(
            "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
<title>Pay invoice {number}</title></head>\n<body onload=\"document.forms[0].submit()\">\n\
<form method=\"post\" action=\"{url}\">\n\
<input type=\"hidden\" name=\"token\" value=\"{token}\">\n\
<p>Invoice {number}: {amount} due. Taking you to the secure Authorize.net payment page.</p>\n\
<button type=\"submit\">Pay {amount}</button>\n</form>\n</body></html>\n",
            url = html_escape(&self.url),
            token = html_escape(&self.token),
            amount = amount_str(self.amount),
        )
    }
}

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The whole step: refuse what can't be paid, build the request from the
/// invoice, ask the gateway for a token.
pub fn payment_link(
    ledger: &Ledger,
    company: &str,
    document_id: &str,
    client: &mut dyn HostedPaymentClient,
    now: DateTime<Utc>,
) -> Result<PaymentLink, LinkError> {
    let request = link_request(ledger, company, document_id)?;
    let page = client.hosted_payment_page(&request)?;
    let environment = client.environment();
    Ok(PaymentLink {
        document_id: request.document_id,
        invoice_number: request.invoice_number,
        amount: request.amount,
        environment,
        url: environment.form_url().to_string(),
        token: page.token,
        expires_at: now + Duration::minutes(TOKEN_LIFETIME_MINUTES),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            api_login_id: "login-123".to_string(),
            transaction_key: "key-456".to_string(),
        }
    }

    fn request() -> LinkRequest {
        LinkRequest {
            document_id: "inv-1".to_string(),
            invoice_number: "1001".to_string(),
            amount: Money::from_minor(10_662),
            description: "Invoice 1001 - Pool Co".to_string(),
            customer_id: Some("cpool".to_string()),
            customer_email: Some("ap@pool.example".to_string()),
        }
    }

    #[test]
    fn the_body_carries_number_and_amount_in_schema_order() {
        let body = request_body(&credentials(), &request(), &LinkConfig::default()).unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        let tr = &value["getHostedPaymentPageRequest"]["transactionRequest"];
        assert_eq!(tr["transactionType"], "authCaptureTransaction");
        assert_eq!(tr["amount"], "106.62");
        assert_eq!(tr["order"]["invoiceNumber"], "1001");
        assert_eq!(tr["customer"]["id"], "cpool");
        // Order matters to the gateway: merchantAuthentication, refId,
        // transactionRequest, hostedPaymentSettings; and inside the
        // transaction, type, amount, order, customer.
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(at("merchantAuthentication") < at("refId"));
        assert!(at("refId") < at("transactionRequest"));
        assert!(at("transactionRequest") < at("hostedPaymentSettings"));
        assert!(at("transactionType") < at("\"amount\""));
        assert!(at("\"amount\"") < at("\"order\""));
        assert!(at("\"order\"") < at("\"customer\""));
        let settings = value["getHostedPaymentPageRequest"]["hostedPaymentSettings"]["setting"]
            .as_array()
            .unwrap();
        let order = settings
            .iter()
            .find(|s| s["settingName"] == "hostedPaymentOrderOptions")
            .unwrap();
        let inner: Value = serde_json::from_str(order["settingValue"].as_str().unwrap()).unwrap();
        assert_eq!(inner["merchantName"], "Aquamentor");
    }

    #[test]
    fn credentials_never_show_in_debug() {
        let text = format!("{:?}", credentials());
        assert!(
            !text.contains("login-123") && !text.contains("key-456"),
            "{text}"
        );
    }

    #[test]
    fn responses_parse_with_and_without_a_bom() {
        let ok = "\u{feff}{\"token\":\"abc\",\"messages\":{\"resultCode\":\"Ok\",\"message\":[{\"code\":\"I00001\",\"text\":\"Successful.\"}]}}";
        assert_eq!(parse_response(ok).unwrap().token, "abc");
        let err = "{\"messages\":{\"resultCode\":\"Error\",\"message\":[{\"code\":\"E00007\",\"text\":\"User authentication failed due to invalid authentication values.\"}]}}";
        match parse_response(err) {
            Err(LinkError::Gateway { code, .. }) => assert_eq!(code, "E00007"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_response("<html>"),
            Err(LinkError::BadResponse(_))
        ));
    }

    #[test]
    fn production_needs_both_switches() {
        let config = LinkConfig::parse("[authnet]\nenvironment = \"production\"\n").unwrap();
        assert!(matches!(
            config.checked_environment(),
            Err(LinkError::ProductionDisabled)
        ));
        assert!(
            HttpAuthnetClient::new(config, Box::new(StaticCredentials(credentials()))).is_err()
        );
        let config =
            LinkConfig::parse("[authnet]\nenvironment = \"production\"\nallow_production = true\n")
                .unwrap();
        assert_eq!(
            config.checked_environment().unwrap(),
            Environment::Production
        );
        assert_eq!(
            LinkConfig::parse("").unwrap().environment,
            Environment::Sandbox
        );
    }

    #[test]
    fn a_config_file_holding_a_key_is_refused() {
        let err = LinkConfig::parse("[authnet]\ntransaction_key = \"x\"\n").unwrap_err();
        assert!(err.contains("never read from a file"), "{err}");
        assert!(!err.contains("\"x\""));
        assert!(LinkConfig::parse("[authnet]\nsurprise = 1\n").is_err());
        // The rest of qbo-local's file is not this module's business.
        let config = LinkConfig::parse(
            "[intuit]\nclient_id = \"abc\"\n\n[authnet]\nmerchant_name = \"WaterLine CNC\"\n",
        )
        .unwrap();
        assert_eq!(config.merchant_name, "WaterLine CNC");
    }

    #[test]
    fn the_html_posts_the_token_to_the_form() {
        let link = PaymentLink {
            document_id: "inv-1".into(),
            invoice_number: "1001".into(),
            amount: Money::from_minor(10_662),
            environment: Environment::Sandbox,
            url: Environment::Sandbox.form_url().into(),
            token: "t\"<k".into(),
            expires_at: Utc::now(),
        };
        let html = link.html();
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"https://test.authorize.net/payment/payment\""));
        assert!(html.contains("name=\"token\" value=\"t&quot;&lt;k\""));
        assert!(html.contains("106.62"));
    }

    #[test]
    fn customer_ids_the_gateway_would_reject_are_left_off() {
        assert!(is_gateway_customer_id("cpool"));
        assert!(!is_gateway_customer_id("c-pool"));
        assert!(!is_gateway_customer_id(
            "0192d4c8-7a1b-7c3d-9e8f-0a1b2c3d4e5f"
        ));
    }
}
