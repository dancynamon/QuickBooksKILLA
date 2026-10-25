//! `.local/config.toml` — what `--live` reads to build an [`crate::http::HttpQboClient`].
//! `HANDOFF.md` §0, §2.3.
//!
//! `.local/` is gitignored in full (`HANDOFF.md` §0: realm ids and secrets
//! never belong in the repository). `config.example.toml` at the repository
//! root is the tracked template — copy it to `.local/config.toml` and fill in
//! real values, which then never leave this machine.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::domain::{DomainError, RealmId};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Toml {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("realm: {0}")]
    Realm(#[from] DomainError),
    /// The config file still names an Intuit app. The one it named was the
    /// app the QuickBooks MCPs use, and qbo-local's polling spent that app's
    /// monthly quota for all of them (23 Oct 2026). Refused outright rather
    /// than ignored, so an old config cannot quietly put it back.
    #[error(
        "{path}: [intuit].client_id / client_secret are no longer read — remove both lines. \
         BunzBooks uses its own Intuit app, never one shared with the QuickBooks MCPs: \
         store it in the login keychain (service {service}, accounts client_id and \
         client_secret) or set {ENV_CLIENT_ID} and {ENV_CLIENT_SECRET}"
    )]
    AppInConfigFile { path: PathBuf, service: String },
    #[error(
        "no BunzBooks Intuit app credentials: store them in the login keychain (service \
         {service}, accounts client_id and client_secret), or set {ENV_CLIENT_ID} and \
         {ENV_CLIENT_SECRET}. Never the QuickBooks MCPs' app: its quota is theirs"
    )]
    MissingCredentials { service: String },
    #[error("{ENV_CLIENT_ID} and {ENV_CLIENT_SECRET} must be set together")]
    PartialEnvironment,
    #[error("reading the keychain (service {service}): {detail}")]
    Keychain { service: String, detail: String },
    #[error("realm {0:?} is not listed under [[realms]] in the config file")]
    UnknownRealm(String),
}

fn default_environment() -> String {
    "production".to_string()
}

fn default_redirect_port() -> u16 {
    8765
}

fn default_tokens_dir() -> PathBuf {
    PathBuf::from(".local/tokens")
}

fn default_rotation_log() -> PathBuf {
    PathBuf::from(".local/rotation-log.jsonl")
}

/// Keychain service holding BunzBooks' own Intuit app: accounts `client_id`
/// and `client_secret`. No other tool on the Mac reads it.
pub const APP_KEYCHAIN_SERVICE: &str = "bunzbooks-intuit-app";

/// The environment override for [`APP_KEYCHAIN_SERVICE`]. Deliberately not
/// `QBO_CLIENT_ID` / `QBO_CLIENT_SECRET`, which other tools set for theirs.
pub const ENV_CLIENT_ID: &str = "BUNZBOOKS_QBO_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "BUNZBOOKS_QBO_CLIENT_SECRET";

fn default_app_keychain_service() -> String {
    APP_KEYCHAIN_SERVICE.to_string()
}

fn default_token_keychain_service() -> String {
    crate::keychain::DEFAULT_SERVICE.to_string()
}

fn default_daily_calls() -> u32 {
    crate::budget::DEFAULT_DAILY_CALLS
}

fn default_budget_ledger() -> PathBuf {
    PathBuf::from(".local/call-budget.json")
}

/// `[budget]` — the daily ceiling on Intuit calls ([`crate::budget`]).
#[derive(Debug, Clone, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_daily_calls")]
    pub daily_calls: u32,
    #[serde(default = "default_budget_ledger")]
    pub ledger: PathBuf,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            daily_calls: default_daily_calls(),
            ledger: default_budget_ledger(),
        }
    }
}

/// BunzBooks' Intuit app. The secret never reaches `Debug`.
#[derive(Clone)]
pub struct AppCredentials {
    pub client_id: String,
    pub client_secret: String,
    /// Where they came from, for the line a live command prints at start.
    pub source: String,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// Which [`crate::auth::TokenStore`] backend `--live` and `auth` use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenStoreKind {
    /// `keychain::KeychainTokenStore` — macOS only, and the default there
    /// (`HANDOFF.md` §3.5: secrets keychain-only).
    Keychain,
    /// [`crate::auth::FileTokenStore`] under `tokens_dir`.
    File,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntuitConfig {
    /// Read only to be refused ([`ConfigError::AppInConfigFile`]); the app
    /// comes from [`LocalConfig::credentials`].
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Where [`LocalConfig::credentials`] looks for the app.
    #[serde(default = "default_app_keychain_service")]
    pub app_keychain_service: String,
    /// Where the keychain token store keeps tokens. Separate from every
    /// other tool's, and from qbo-local's own pre-October service, whose
    /// tokens belong to the shared app.
    #[serde(default = "default_token_keychain_service")]
    pub token_keychain_service: String,
    #[serde(default = "default_environment")]
    pub environment: String,
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RealmConfig {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalConfig {
    pub intuit: IntuitConfig,
    /// Where tokens live. Omitted: the keychain on macOS, the file store
    /// everywhere else ([`LocalConfig::token_store_kind`]).
    #[serde(default)]
    pub token_store: Option<TokenStoreKind>,
    #[serde(default, rename = "realms")]
    pub realms: Vec<RealmConfig>,
    #[serde(default = "default_tokens_dir")]
    pub tokens_dir: PathBuf,
    #[serde(default = "default_rotation_log")]
    pub rotation_log: PathBuf,
    #[serde(default)]
    pub budget: BudgetConfig,
    /// The file this was loaded from, for messages.
    #[serde(skip)]
    pub path: PathBuf,
}

impl LocalConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: LocalConfig =
            toml::from_str(&text).map_err(|source| ConfigError::Toml {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;
        config.path = path.to_path_buf();
        Ok(config)
    }

    /// BunzBooks' own Intuit app: [`ENV_CLIENT_ID`] / [`ENV_CLIENT_SECRET`]
    /// when both are set, otherwise the login keychain under
    /// `[intuit].app_keychain_service`. Never the config file, and never the
    /// `QBO_CLIENT_*` names other tools use.
    pub fn credentials(&self) -> Result<AppCredentials, ConfigError> {
        let service = &self.intuit.app_keychain_service;
        resolve_credentials(
            self,
            std::env::var(ENV_CLIENT_ID).ok(),
            std::env::var(ENV_CLIENT_SECRET).ok(),
            |account| read_keychain(service, account),
        )
    }

    pub fn call_budget(&self) -> crate::budget::CallBudget {
        crate::budget::CallBudget::persistent(&self.budget.ledger, self.budget.daily_calls)
    }

    pub fn token_store_kind(&self) -> TokenStoreKind {
        self.token_store.unwrap_or(if cfg!(target_os = "macos") {
            TokenStoreKind::Keychain
        } else {
            TokenStoreKind::File
        })
    }

    pub fn base_url(&self) -> &'static str {
        match self.intuit.environment.as_str() {
            "sandbox" => crate::http::SANDBOX_BASE_URL,
            _ => crate::http::PRODUCTION_BASE_URL,
        }
    }

    /// Look up a configured realm by its raw id and validate it as a
    /// [`RealmId`] in the same step, so a typo in the config file is caught
    /// here rather than surfacing later as a confusing empty sync.
    pub fn realm(&self, id: &str) -> Result<RealmId, ConfigError> {
        if !self.realms.iter().any(|realm| realm.id == id) {
            return Err(ConfigError::UnknownRealm(id.to_string()));
        }
        Ok(RealmId::parse(id)?)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// [`LocalConfig::credentials`] with the environment and keychain passed in,
/// so the precedence is testable without either.
fn resolve_credentials(
    config: &LocalConfig,
    env_id: Option<String>,
    env_secret: Option<String>,
    keychain: impl Fn(&str) -> Result<Option<String>, String>,
) -> Result<AppCredentials, ConfigError> {
    let service = config.intuit.app_keychain_service.clone();
    if non_empty(config.intuit.client_id.clone()).is_some()
        || non_empty(config.intuit.client_secret.clone()).is_some()
    {
        return Err(ConfigError::AppInConfigFile {
            path: config.path.clone(),
            service,
        });
    }
    match (non_empty(env_id), non_empty(env_secret)) {
        (Some(client_id), Some(client_secret)) => {
            return Ok(AppCredentials {
                client_id,
                client_secret,
                source: format!("environment ({ENV_CLIENT_ID})"),
            })
        }
        (None, None) => {}
        _ => return Err(ConfigError::PartialEnvironment),
    }
    let read = |account: &str| {
        keychain(account).map_err(|detail| ConfigError::Keychain {
            service: service.clone(),
            detail,
        })
    };
    match (
        non_empty(read("client_id")?),
        non_empty(read("client_secret")?),
    ) {
        (Some(client_id), Some(client_secret)) => Ok(AppCredentials {
            client_id,
            client_secret,
            source: format!("login keychain ({service})"),
        }),
        _ => Err(ConfigError::MissingCredentials { service }),
    }
}

#[cfg(target_os = "macos")]
fn read_keychain(service: &str, account: &str) -> Result<Option<String>, String> {
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    match security_framework::passwords::get_generic_password(service, account) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(|value| Some(value.trim().to_string()))
            .map_err(|_| format!("item {account} is not UTF-8")),
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(format!("{error} (OSStatus {})", error.code())),
    }
}

#[cfg(not(target_os = "macos"))]
fn read_keychain(_service: &str, _account: &str) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(directory: &Path, contents: &str) -> PathBuf {
        let path = directory.join("config.toml");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn loads_a_minimal_config_with_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            r#"
                [intuit]
                client_id = "abc123"

                [[realms]]
                id = "1234567890123456"
                name = "Aquamentor, Inc."
            "#,
        );

        let config = LocalConfig::load(&path).unwrap();
        assert_eq!(config.intuit.environment, "production");
        assert_eq!(config.intuit.redirect_port, 8765);
        assert_eq!(config.tokens_dir, PathBuf::from(".local/tokens"));
        assert_eq!(config.base_url(), crate::http::PRODUCTION_BASE_URL);
        assert_eq!(
            config.realm("1234567890123456").unwrap().as_str(),
            "1234567890123456"
        );
    }

    #[test]
    fn sandbox_environment_selects_the_sandbox_host() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            r#"
                [intuit]
                client_id = "abc123"
                environment = "sandbox"
            "#,
        );
        let config = LocalConfig::load(&path).unwrap();
        assert_eq!(config.base_url(), crate::http::SANDBOX_BASE_URL);
    }

    #[test]
    fn an_unlisted_realm_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            r#"
                [intuit]
                client_id = "abc123"
            "#,
        );
        let config = LocalConfig::load(&path).unwrap();
        assert!(matches!(
            config.realm("1234567890123456"),
            Err(ConfigError::UnknownRealm(_))
        ));
    }

    fn bare(directory: &Path) -> LocalConfig {
        let path = write(
            directory,
            "[intuit]\n[[realms]]\nid = \"1\"\nname = \"A\"\n",
        );
        LocalConfig::load(&path).unwrap()
    }

    fn keychain(
        id: Option<&'static str>,
        secret: Option<&'static str>,
    ) -> impl Fn(&str) -> Result<Option<String>, String> {
        move |account| {
            Ok(match account {
                "client_id" => id.map(str::to_string),
                "client_secret" => secret.map(str::to_string),
                other => panic!("unexpected account {other}"),
            })
        }
    }

    #[test]
    fn an_app_in_the_config_file_is_refused_even_with_the_keychain_set() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(directory.path(), "[intuit]\nclient_id = \"shared\"\n");
        let config = LocalConfig::load(&path).unwrap();
        let outcome = resolve_credentials(&config, None, None, keychain(Some("own"), Some("s")));
        assert!(matches!(outcome, Err(ConfigError::AppInConfigFile { .. })));

        let path = write(directory.path(), "[intuit]\nclient_secret = \"shared\"\n");
        let config = LocalConfig::load(&path).unwrap();
        let outcome = resolve_credentials(&config, None, None, keychain(Some("own"), Some("s")));
        assert!(matches!(outcome, Err(ConfigError::AppInConfigFile { .. })));
    }

    #[test]
    fn credentials_come_from_the_environment_then_the_keychain() {
        let directory = tempfile::tempdir().unwrap();
        let config = bare(directory.path());

        let from_env = resolve_credentials(
            &config,
            Some("env-id".into()),
            Some("env-secret".into()),
            keychain(Some("kc-id"), Some("kc-secret")),
        )
        .unwrap();
        assert_eq!(from_env.client_id, "env-id");
        assert_eq!(from_env.client_secret, "env-secret");

        let from_keychain = resolve_credentials(
            &config,
            None,
            None,
            keychain(Some("kc-id"), Some("kc-secret")),
        )
        .unwrap();
        assert_eq!(from_keychain.client_id, "kc-id");
        assert!(from_keychain.source.contains(APP_KEYCHAIN_SERVICE));
        assert!(!format!("{from_keychain:?}").contains("kc-secret"));
    }

    #[test]
    fn half_a_set_of_credentials_is_an_error_not_a_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let config = bare(directory.path());
        assert!(matches!(
            resolve_credentials(
                &config,
                Some("id".into()),
                None,
                keychain(Some("a"), Some("b"))
            ),
            Err(ConfigError::PartialEnvironment)
        ));
        assert!(matches!(
            resolve_credentials(&config, None, None, keychain(Some("a"), None)),
            Err(ConfigError::MissingCredentials { .. })
        ));
    }

    #[test]
    fn the_tracked_example_names_no_intuit_app() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            include_str!("../../../.local/config.example.toml"),
        );
        let config = LocalConfig::load(&path).unwrap();
        assert!(config.intuit.client_id.is_none());
        assert!(config.intuit.client_secret.is_none());
        assert_eq!(config.intuit.app_keychain_service, APP_KEYCHAIN_SERVICE);
    }

    #[test]
    fn the_budget_defaults_to_fifteen_thousand_a_day() {
        let directory = tempfile::tempdir().unwrap();
        let config = bare(directory.path());
        assert_eq!(config.budget.daily_calls, 15_000);
        assert_eq!(
            config.budget.ledger,
            PathBuf::from(".local/call-budget.json")
        );
        assert_eq!(
            config.intuit.token_keychain_service,
            crate::keychain::DEFAULT_SERVICE
        );
    }

    #[test]
    fn the_token_store_defaults_per_platform_and_can_be_forced() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            "[intuit]\nclient_id = \"x\"\n[[realms]]\nid = \"1\"\nname = \"A\"\n",
        );
        let config = LocalConfig::load(&path).unwrap();
        let expected = if cfg!(target_os = "macos") {
            TokenStoreKind::Keychain
        } else {
            TokenStoreKind::File
        };
        assert_eq!(config.token_store_kind(), expected);

        let path = write(
            directory.path(),
            "token_store = \"file\"\n[intuit]\nclient_id = \"x\"\n",
        );
        assert_eq!(
            LocalConfig::load(&path).unwrap().token_store_kind(),
            TokenStoreKind::File
        );
    }
}
