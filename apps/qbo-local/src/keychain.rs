//! macOS keychain backend for [`TokenStore`]. `DESIGN.md` §5.1, `HANDOFF.md` §2.1.
//!
//! Driven through `/usr/bin/security` rather than a framework binding, for two
//! reasons: it builds and tests on any platform, and the argument handling —
//! the only part that can leak a secret — is plain code a test can read.
//!
//! The leak to avoid is the token on a command line, where `ps` shows it to
//! every user on the machine. Writes therefore go through `security -i`, which
//! reads its commands from stdin, and the value is passed hex-encoded with
//! `-X` so no quoting rule can split or truncate it. Reads print the value on
//! stdout, which is a pipe to this process only.
//!
//! One keychain item per realm holds the whole [`TokenGenerations`] value, so
//! a rotation is one item update — the keychain's equivalent of the file
//! backend's single atomic rename.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::auth::{AuthError, TokenGenerations, TokenStore};
use crate::domain::RealmId;

pub const SECURITY: &str = "/usr/bin/security";
pub const DEFAULT_SERVICE: &str = "qbo-local";

/// `security find-generic-password` exit status for "no such item".
const ITEM_NOT_FOUND: i32 = 44;

/// What one `security` invocation returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `security`. A trait so tests can assert exactly what would be run —
/// arguments and stdin separately, since only stdin may carry the secret.
pub trait SecurityRunner {
    fn run(&self, args: &[String], stdin: Option<&str>) -> Result<Output, std::io::Error>;
}

pub struct SystemSecurity;

impl SecurityRunner for SystemSecurity {
    fn run(&self, args: &[String], stdin: Option<&str>) -> Result<Output, std::io::Error> {
        let mut child = Command::new(SECURITY)
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin was piped");
            pipe.write_all(input.as_bytes())?;
            // Dropping the pipe sends EOF, which ends `security -i`.
        }
        let output = child.wait_with_output()?;
        Ok(Output {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub struct KeychainTokenStore<R: SecurityRunner = SystemSecurity> {
    runner: R,
    service: String,
}

impl KeychainTokenStore<SystemSecurity> {
    pub fn new() -> Self {
        KeychainTokenStore::with_runner(SystemSecurity, DEFAULT_SERVICE)
    }
}

impl Default for KeychainTokenStore<SystemSecurity> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: SecurityRunner> KeychainTokenStore<R> {
    pub fn with_runner(runner: R, service: impl Into<String>) -> Self {
        KeychainTokenStore {
            runner,
            service: service.into(),
        }
    }

    fn account(realm: &RealmId) -> String {
        format!("tokens-{realm}")
    }

    fn failed(what: &str, output: &Output) -> AuthError {
        AuthError::Io(std::io::Error::other(format!(
            "security {what} exited {}: {}",
            output.status,
            output.stderr.trim()
        )))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(raw.get(i..i + 2)?, 16).ok())
        .collect()
}

impl<R: SecurityRunner> TokenStore for KeychainTokenStore<R> {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError> {
        let args: Vec<String> = [
            "find-generic-password",
            "-s",
            &self.service,
            "-a",
            &Self::account(realm),
            "-w",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let output = self.runner.run(&args, None)?;
        match output.status {
            0 => {}
            ITEM_NOT_FOUND => return Ok(None),
            _ => return Err(Self::failed("find-generic-password", &output)),
        }
        // `-w` prints printable data as-is and anything else as hex. The JSON
        // is printable, but reading both keeps a hand-edited item loadable.
        let value = output.stdout.trim_end_matches(['\n', '\r']);
        let bytes = if value.starts_with('{') {
            value.as_bytes().to_vec()
        } else {
            unhex(value).ok_or_else(|| {
                AuthError::Io(std::io::Error::other("keychain item is neither JSON nor hex"))
            })?
        };
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(generations)?;
        // Service and account are fixed-alphabet (a realm id is digits), and
        // the value is hex, so nothing on this line needs quoting.
        let command = format!(
            "add-generic-password -U -s {} -a {} -X {}\n",
            self.service,
            Self::account(&generations.realm_id),
            hex(&bytes)
        );
        let output = self.runner.run(&["-i".to_string()], Some(&command))?;
        // `security -i` exits 0 even when a command inside it fails, and
        // reports the failure on stderr.
        if output.status != 0 || !output.stderr.trim().is_empty() {
            return Err(Self::failed("add-generic-password", &output));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use chrono::DateTime;

    use super::*;
    use crate::auth::TokenSet;

    /// An in-memory keychain that records every invocation.
    #[derive(Default)]
    struct FakeSecurity {
        items: RefCell<HashMap<String, String>>,
        calls: RefCell<Vec<(Vec<String>, Option<String>)>>,
        fail_writes: bool,
    }

    impl SecurityRunner for FakeSecurity {
        fn run(&self, args: &[String], stdin: Option<&str>) -> Result<Output, std::io::Error> {
            self.calls.borrow_mut().push((args.to_vec(), stdin.map(str::to_string)));
            let ok = |stdout: String| Output { status: 0, stdout, stderr: String::new() };
            if args.first().map(String::as_str) == Some("find-generic-password") {
                let account = &args[4];
                return Ok(match self.items.borrow().get(account) {
                    Some(value) => ok(format!("{value}\n")),
                    None => Output { status: 44, stdout: String::new(), stderr: "not found".into() },
                });
            }
            let line = stdin.unwrap_or_default();
            if self.fail_writes {
                return Ok(Output { status: 0, stdout: String::new(), stderr: "User interaction is not allowed.".into() });
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            let account = words[words.iter().position(|w| *w == "-a").unwrap() + 1];
            let value = words[words.iter().position(|w| *w == "-X").unwrap() + 1];
            let json = String::from_utf8(unhex(value).unwrap()).unwrap();
            self.items.borrow_mut().insert(account.to_string(), json);
            Ok(ok(String::new()))
        }
    }

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn generations() -> TokenGenerations {
        TokenGenerations::new(
            realm(),
            TokenSet {
                access_token: "access-SECRET-a".into(),
                refresh_token: "refresh-SECRET-a".into(),
                obtained_at: DateTime::from_timestamp(1_755_300_000, 0).unwrap(),
                access_expires_in_secs: 3600,
                refresh_expires_in_secs: 8_726_400,
            },
        )
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(b"{\"a\":1}"), "7b2261223a317d");
        assert_eq!(unhex("7b2261223a317d").unwrap(), b"{\"a\":1}");
        assert!(unhex("7").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn a_saved_generation_loads_back() {
        let store = KeychainTokenStore::with_runner(FakeSecurity::default(), "qbo-local-test");
        assert!(store.load(&realm()).unwrap().is_none());
        store.save(&generations()).unwrap();
        assert_eq!(store.load(&realm()).unwrap(), Some(generations()));
    }

    #[test]
    fn the_secret_never_appears_in_arguments() {
        let store = KeychainTokenStore::with_runner(FakeSecurity::default(), "qbo-local-test");
        store.save(&generations()).unwrap();
        store.load(&realm()).unwrap();
        for (args, stdin) in store.runner.calls.borrow().iter() {
            assert!(!args.join(" ").contains("SECRET"), "argv is visible in ps: {args:?}");
            if let Some(stdin) = stdin {
                // Hex-encoded on stdin, so not even the pipe carries it raw.
                assert!(!stdin.contains("SECRET"));
            }
        }
    }

    #[test]
    fn a_rotation_updates_the_one_item_in_place() {
        let store = KeychainTokenStore::with_runner(FakeSecurity::default(), "qbo-local-test");
        let mut tokens = generations();
        store.save(&tokens).unwrap();
        let mut next = tokens.current.clone();
        next.refresh_token = "refresh-SECRET-b".into();
        tokens.rotate(next);
        store.save(&tokens).unwrap();

        assert_eq!(store.runner.items.borrow().len(), 1);
        let loaded = store.load(&realm()).unwrap().unwrap();
        assert_eq!(loaded.current.refresh_token, "refresh-SECRET-b");
        assert_eq!(loaded.previous.len(), 1);
        let (_, stdin) = &store.runner.calls.borrow()[0];
        assert!(stdin.as_deref().unwrap().starts_with("add-generic-password -U "));
    }

    #[test]
    fn a_failure_inside_interactive_mode_is_not_mistaken_for_success() {
        // `security -i` exits 0 regardless; stderr is the only signal. Taking
        // the exit code alone would report a rotation persisted that was not.
        let runner = FakeSecurity { fail_writes: true, ..FakeSecurity::default() };
        let store = KeychainTokenStore::with_runner(runner, "qbo-local-test");
        assert!(store.save(&generations()).is_err());
    }

    #[test]
    fn the_manager_persists_rotations_through_the_keychain() {
        use crate::http::ScriptedTransport;
        use crate::oauth::{ClientCredentials, TokenManager};

        let store = KeychainTokenStore::with_runner(FakeSecurity::default(), "qbo-local-test");
        let mut transport = ScriptedTransport::default();
        transport.respond(
            200,
            r#"{"access_token":"access-b","refresh_token":"refresh-b","expires_in":3600,"x_refresh_token_expires_in":8726400}"#,
        );
        let credentials = ClientCredentials { client_id: "id".into(), client_secret: "secret".into() };
        let mut manager = TokenManager::new(transport, store, credentials);
        manager.install(&realm(), generations().current).unwrap();
        manager.rotate(&realm(), DateTime::from_timestamp(1_755_303_000, 0).unwrap()).unwrap();

        let reloaded = manager.store().load(&realm()).unwrap().unwrap();
        assert_eq!(reloaded.current.refresh_token, "refresh-b");
    }
}
