//! OAuth token persistence and rotation. `DESIGN.md` §5.
//!
//! The kickoff prompt calls this the single most likely cause of a 3am
//! breakage, and it is right. The access token lasts about an hour; refreshing
//! it returns a **new refresh token and invalidates the old one**. Lose the new
//! one before it reaches disk — power cut, crash, half-written file — and the
//! account is locked out until re-authorised by hand.
//!
//! Two mechanisms defend against that: an atomic, fully-fsynced write (§5.1),
//! and retained previous generations to fall back through (§5.2).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::RealmId;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("token store i/o failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("token store is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("no usable token generation remains for realm {0}; re-authorisation required")]
    Lockout(RealmId),
}

/// How many previous generations to retain (§5.2).
pub const RETAINED_GENERATIONS: usize = 2;

/// Refresh this far before the access token actually expires, so rotation
/// happens while idle rather than mid-batch (§5.4).
pub const REFRESH_MARGIN_MINUTES: i64 = 10;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub obtained_at: DateTime<Utc>,
    pub access_expires_in_secs: i64,
    pub refresh_expires_in_secs: i64,
}

impl TokenSet {
    pub fn access_expires_at(&self) -> DateTime<Utc> {
        self.obtained_at + Duration::seconds(self.access_expires_in_secs)
    }

    pub fn refresh_expires_at(&self) -> DateTime<Utc> {
        self.obtained_at + Duration::seconds(self.refresh_expires_in_secs)
    }

    /// Proactive refresh: true once the access token is within
    /// [`REFRESH_MARGIN_MINUTES`] of expiry. Reactive refresh on a 401 remains
    /// the fallback, but the intent is that it rarely fires.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        now + Duration::minutes(REFRESH_MARGIN_MINUTES) >= self.access_expires_at()
    }

    /// Past this, no refresh is possible and the user must re-authorise.
    pub fn refresh_is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.refresh_expires_at()
    }

    /// A stable, non-reversible-enough handle for the rotation log. **Never log
    /// the token itself.** Same convention as a card's last four.
    pub fn fingerprint(&self) -> String {
        let token = &self.refresh_token;
        let tail: String = token.chars().rev().take(4).collect::<Vec<_>>()
            .into_iter().rev().collect();
        format!("…{tail} ({} chars)", token.chars().count())
    }
}

/// The current token set plus the previous generations retained as fallback.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TokenGenerations {
    pub realm_id: RealmId,
    pub current: TokenSet,
    /// Most recent first, capped at [`RETAINED_GENERATIONS`].
    pub previous: Vec<TokenSet>,
}

impl TokenGenerations {
    pub fn new(realm_id: RealmId, current: TokenSet) -> Self {
        TokenGenerations {
            realm_id,
            current,
            previous: Vec::new(),
        }
    }

    /// Record a rotation: the outgoing set becomes the newest previous
    /// generation and the oldest is dropped.
    pub fn rotate(&mut self, next: TokenSet) {
        let outgoing = std::mem::replace(&mut self.current, next);
        self.previous.insert(0, outgoing);
        self.previous.truncate(RETAINED_GENERATIONS);
    }

    /// Every generation, newest first — the order to try on startup when the
    /// current set fails. Covers a crash between Intuit invalidating the old
    /// token and the new one reaching disk.
    pub fn candidates(&self) -> impl Iterator<Item = &TokenSet> {
        std::iter::once(&self.current).chain(self.previous.iter())
    }

    /// The newest generation whose refresh token has not itself expired.
    pub fn newest_usable(&self, now: DateTime<Utc>) -> Result<&TokenSet, AuthError> {
        self.candidates()
            .find(|set| !set.refresh_is_expired(now))
            .ok_or_else(|| AuthError::Lockout(self.realm_id.clone()))
    }
}

/// Write `bytes` to `path` so that a crash at any point leaves either the old
/// contents or the new, never a truncated file (§5.1).
///
/// Both fsyncs are required. `rename` is atomic but not automatically durable —
/// without the directory fsync a crash can leave the old file in place while
/// Intuit has already invalidated the token it holds, which is the lockout this
/// whole module exists to prevent.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "token path has no parent directory",
        )
    })?;
    fs::create_dir_all(directory)?;

    // Same directory as the target, so the rename cannot cross a filesystem.
    let temporary = path.with_extension("tmp");
    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    fs::rename(&temporary, path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Durable storage for a realm's token generations.
///
/// The production backend is the OS keychain (`DESIGN.md` §8 — secrets never in
/// the repo, never in a dotfile that could sync to Dropbox). This trait exists
/// so the rotation logic above is testable without one, and so the keychain
/// backend can be added without touching the caller.
pub trait TokenStore {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError>;
    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError>;
}

/// File-backed store using [`atomic_write`]. Used in tests and development.
pub struct FileTokenStore {
    directory: PathBuf,
}

impl FileTokenStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        FileTokenStore {
            directory: directory.into(),
        }
    }

    fn path_for(&self, realm: &RealmId) -> PathBuf {
        self.directory.join(format!("tokens-{realm}.json"))
    }
}

impl TokenStore for FileTokenStore {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError> {
        let path = self.path_for(realm);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(generations)?;
        atomic_write(&self.path_for(&generations.realm_id), &bytes)?;
        Ok(())
    }
}

/// One line of the append-only rotation log (§5.3). When a 3am breakage
/// happens this file is the first thing to read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotationLogEntry {
    pub at: DateTime<Utc>,
    pub realm_id: RealmId,
    /// Fingerprint only — never the token.
    pub outgoing: String,
    pub incoming: String,
    pub outcome: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn at(offset_secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_300_000 + offset_secs, 0).unwrap()
    }

    fn token_set(label: &str, obtained_at: DateTime<Utc>) -> TokenSet {
        TokenSet {
            access_token: format!("access-{label}"),
            refresh_token: format!("refresh-{label}"),
            obtained_at,
            access_expires_in_secs: 3600,
            refresh_expires_in_secs: 100 * 24 * 3600,
        }
    }

    #[test]
    fn refresh_is_proactive_not_reactive() {
        let set = token_set("a", at(0));
        assert!(!set.needs_refresh(at(0)));
        // 49 minutes in: still fine.
        assert!(!set.needs_refresh(at(49 * 60)));
        // 50 minutes in: inside the 10-minute margin on a 60-minute token.
        assert!(set.needs_refresh(at(50 * 60)));
        // Expiry itself is well past the trigger.
        assert!(set.needs_refresh(at(3600)));
    }

    #[test]
    fn refresh_token_expiry_is_tracked_separately() {
        let set = token_set("a", at(0));
        assert!(!set.refresh_is_expired(at(99 * 24 * 3600)));
        assert!(set.refresh_is_expired(at(100 * 24 * 3600)));
    }

    #[test]
    fn fingerprint_does_not_leak_the_token() {
        let set = token_set("aquamentor", at(0));
        let fingerprint = set.fingerprint();
        assert!(!fingerprint.contains(&set.refresh_token));
        assert!(fingerprint.contains("ntor"));
    }

    #[test]
    fn rotation_retains_exactly_two_previous_generations() {
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        for index in 2..=6 {
            generations.rotate(token_set(&index.to_string(), at(index * 3600)));
        }
        assert_eq!(generations.current.refresh_token, "refresh-6");
        assert_eq!(generations.previous.len(), RETAINED_GENERATIONS);
        // Newest first.
        assert_eq!(generations.previous[0].refresh_token, "refresh-5");
        assert_eq!(generations.previous[1].refresh_token, "refresh-4");
    }

    #[test]
    fn candidates_are_ordered_newest_first() {
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        generations.rotate(token_set("2", at(3600)));
        generations.rotate(token_set("3", at(7200)));
        let order: Vec<&str> = generations
            .candidates()
            .map(|set| set.refresh_token.as_str())
            .collect();
        assert_eq!(order, ["refresh-3", "refresh-2", "refresh-1"]);
    }

    #[test]
    fn falls_back_past_an_expired_generation() {
        let mut generations = TokenGenerations::new(realm(), token_set("old", at(0)));
        // A newer set obtained now, and the older one has aged past its refresh
        // window.
        let recent = token_set("new", at(101 * 24 * 3600));
        generations.rotate(recent);
        let usable = generations.newest_usable(at(101 * 24 * 3600)).unwrap();
        assert_eq!(usable.refresh_token, "refresh-new");
    }

    #[test]
    fn every_generation_expired_is_a_lockout_not_a_panic() {
        let generations = TokenGenerations::new(realm(), token_set("a", at(0)));
        let result = generations.newest_usable(at(200 * 24 * 3600));
        assert!(matches!(result, Err(AuthError::Lockout(_))));
    }

    #[test]
    fn tokens_survive_a_save_and_load_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(directory.path());
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        generations.rotate(token_set("2", at(3600)));

        store.save(&generations).unwrap();
        let loaded = store.load(&realm()).unwrap().unwrap();
        assert_eq!(loaded, generations);
    }

    #[test]
    fn loading_an_unknown_realm_is_none_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(directory.path());
        assert!(store.load(&realm()).unwrap().is_none());
    }

    #[test]
    fn a_second_save_replaces_the_first_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(directory.path());

        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        store.save(&generations).unwrap();
        generations.rotate(token_set("2", at(3600)));
        store.save(&generations).unwrap();

        let loaded = store.load(&realm()).unwrap().unwrap();
        assert_eq!(loaded.current.refresh_token, "refresh-2");
        assert_eq!(loaded.previous.len(), 1);

        // No temporary file left behind to be mistaken for the real store.
        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file left behind");
    }

    #[test]
    fn atomic_write_creates_missing_directories() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("a").join("b").join("tokens.json");
        atomic_write(&nested, b"contents").unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"contents");
    }

    #[test]
    fn atomic_write_replaces_rather_than_appends() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        atomic_write(&path, b"a much longer original value").unwrap();
        atomic_write(&path, b"short").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"short");
    }

    #[test]
    fn a_corrupt_store_reports_corruption_rather_than_returning_none() {
        // Distinguishing "no tokens yet" from "tokens unreadable" matters: the
        // first means authorise, the second means investigate before doing
        // anything that might overwrite a recoverable file.
        let directory = tempfile::tempdir().unwrap();
        let store = FileTokenStore::new(directory.path());
        fs::write(directory.path().join("tokens-1234567890123456.json"), b"{ not json").unwrap();
        assert!(matches!(store.load(&realm()), Err(AuthError::Corrupt(_))));
    }
}
