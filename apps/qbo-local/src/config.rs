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
    #[error(
        "no client secret configured: set the QBO_CLIENT_SECRET environment variable, or \
         [intuit].client_secret in the config file (HANDOFF.md §0: prefer the environment)"
    )]
    MissingClientSecret,
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

#[derive(Debug, Clone, Deserialize)]
pub struct IntuitConfig {
    pub client_id: String,
    /// Prefer the `QBO_CLIENT_SECRET` environment variable
    /// ([`LocalConfig::client_secret`]) over writing the secret into a file
    /// at all, even a gitignored one.
    #[serde(default)]
    pub client_secret: Option<String>,
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
    #[serde(default, rename = "realms")]
    pub realms: Vec<RealmConfig>,
    #[serde(default = "default_tokens_dir")]
    pub tokens_dir: PathBuf,
    #[serde(default = "default_rotation_log")]
    pub rotation_log: PathBuf,
}

impl LocalConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Toml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// `QBO_CLIENT_SECRET` wins when set, so the secret need never sit in a
    /// file at all — the config file only has to carry the client id.
    pub fn client_secret(&self) -> Result<String, ConfigError> {
        std::env::var("QBO_CLIENT_SECRET")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.intuit
                    .client_secret
                    .clone()
                    .filter(|value| !value.is_empty())
            })
            .ok_or(ConfigError::MissingClientSecret)
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
        assert_eq!(config.intuit.client_id, "abc123");
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

    /// `QBO_CLIENT_SECRET` is process-global state, and `cargo test` runs
    /// this file's tests in parallel threads of one process — so both halves
    /// of this behaviour (env wins; neither source means an error) live in
    /// one test rather than two that could race on the same variable.
    #[test]
    fn client_secret_prefers_the_environment_then_reports_when_neither_is_set() {
        let directory = tempfile::tempdir().unwrap();
        let with_file_secret = write(
            directory.path(),
            r#"
                [intuit]
                client_id = "abc123"
                client_secret = "from-file"
            "#,
        );
        let without_file_secret = {
            let path = directory.path().join("no-secret.toml");
            fs::write(&path, "[intuit]\nclient_id = \"abc123\"\n").unwrap();
            path
        };
        let with_secret = LocalConfig::load(&with_file_secret).unwrap();
        let without_secret = LocalConfig::load(&without_file_secret).unwrap();

        std::env::remove_var("QBO_CLIENT_SECRET");
        assert_eq!(with_secret.client_secret().unwrap(), "from-file");
        assert!(matches!(
            without_secret.client_secret(),
            Err(ConfigError::MissingClientSecret)
        ));

        std::env::set_var("QBO_CLIENT_SECRET", "from-env");
        assert_eq!(
            with_secret.client_secret().unwrap(),
            "from-env",
            "environment should win over the file"
        );
        assert_eq!(without_secret.client_secret().unwrap(), "from-env");
        std::env::remove_var("QBO_CLIENT_SECRET");
    }
}
