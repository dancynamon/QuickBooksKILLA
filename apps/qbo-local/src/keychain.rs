//! The production [`TokenStore`]: the OS keychain. `DESIGN.md` §5, HANDOFF §2.1.
//!
//! [`FileTokenStore`](crate::auth::FileTokenStore) is the behavioural reference.
//! The stored unit is the same — one [`TokenGenerations`] value, current plus
//! the retained previous generations — so a rotation is one keychain write
//! rather than several items that could disagree after a crash. That single
//! write is what the file store's `write → fsync → rename → fsync` sequence buys
//! on disk; the keychain gives it by updating one item in one call.
//!
//! The keychain itself sits behind [`SecretBackend`], so everything above the
//! raw get/set is tested here without one. The macOS backend is the only code in
//! the crate that cannot run in CI, and it is kept to a few lines for that
//! reason.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::auth::{AuthError, TokenGenerations, TokenStore};
use crate::domain::RealmId;

/// Keychain service name. One item per realm, the realm id as the account.
pub const DEFAULT_SERVICE: &str = "com.aquamentor.qbo-local.tokens";

/// Raw secret storage, keyed by service and account.
///
/// `set` must replace an existing entry in one operation — never delete then
/// add, which leaves a window where a crash loses the only copy of a refresh
/// token Intuit has already rotated.
pub trait SecretBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, AuthError>;
    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), AuthError>;
}

/// Token generations persisted as one keychain item per realm.
pub struct KeychainTokenStore<B: SecretBackend> {
    backend: B,
    service: String,
}

impl<B: SecretBackend> KeychainTokenStore<B> {
    pub fn with_backend(backend: B, service: impl Into<String>) -> Self {
        KeychainTokenStore {
            backend,
            service: service.into(),
        }
    }
}

#[cfg(target_os = "macos")]
impl KeychainTokenStore<MacKeychain> {
    /// The login keychain, under [`DEFAULT_SERVICE`].
    pub fn new() -> Self {
        KeychainTokenStore::with_backend(MacKeychain, DEFAULT_SERVICE)
    }
}

#[cfg(target_os = "macos")]
impl Default for KeychainTokenStore<MacKeychain> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: SecretBackend> TokenStore for KeychainTokenStore<B> {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError> {
        let Some(bytes) = self.backend.get(&self.service, realm.as_str())? else {
            return Ok(None);
        };
        let generations: TokenGenerations = serde_json::from_slice(&bytes)?;
        // An item filed under one realm holding another's tokens would have
        // the client refresh against the wrong company. Refuse rather than
        // guess which half is right.
        if generations.realm_id != *realm {
            return Err(AuthError::Keychain(format!(
                "entry for realm {realm} holds tokens for realm {}",
                generations.realm_id
            )));
        }
        Ok(Some(generations))
    }

    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(generations)?;
        self.backend
            .set(&self.service, generations.realm_id.as_str(), &bytes)
    }
}

/// The macOS login keychain, as generic-password items.
#[cfg(target_os = "macos")]
pub struct MacKeychain;

#[cfg(target_os = "macos")]
impl SecretBackend for MacKeychain {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, AuthError> {
        use security_framework::passwords::{generic_password, PasswordOptions};

        // errSecItemNotFound. "No tokens yet" means authorise; every other
        // failure means investigate, so the two must not collapse into None.
        const ITEM_NOT_FOUND: i32 = -25300;

        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(AuthError::Keychain(error.to_string())),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), AuthError> {
        // SecItemAdd, falling back to SecItemUpdate on errSecDuplicateItem: the
        // existing item is updated in place, never deleted and re-added.
        security_framework::passwords::set_generic_password(service, account, secret)
            .map_err(|error| AuthError::Keychain(error.to_string()))
    }
}

/// In-memory backend for tests and for platforms with no keychain wired up.
/// Holds secrets only for the life of the process — never a production store.
#[derive(Default)]
pub struct MemoryBackend {
    items: RefCell<HashMap<(String, String), Vec<u8>>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite an entry directly, bypassing serialisation. For tests that
    /// need a corrupt or mismatched item.
    pub fn put_raw(&self, service: &str, account: &str, secret: &[u8]) {
        self.items
            .borrow_mut()
            .insert((service.to_string(), account.to_string()), secret.to_vec());
    }
}

impl SecretBackend for MemoryBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, AuthError> {
        Ok(self
            .items
            .borrow()
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), AuthError> {
        self.put_raw(service, account, secret);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{TokenSet, RETAINED_GENERATIONS};
    use chrono::{DateTime, Utc};

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn other_realm() -> RealmId {
        RealmId::parse("6543210987654321").unwrap()
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

    fn store() -> KeychainTokenStore<MemoryBackend> {
        KeychainTokenStore::with_backend(MemoryBackend::new(), DEFAULT_SERVICE)
    }

    #[test]
    fn tokens_survive_a_save_and_load_round_trip() {
        let store = store();
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        generations.rotate(token_set("2", at(3600)));

        store.save(&generations).unwrap();
        assert_eq!(store.load(&realm()).unwrap().unwrap(), generations);
    }

    #[test]
    fn loading_an_unknown_realm_is_none_not_an_error() {
        assert!(store().load(&realm()).unwrap().is_none());
    }

    #[test]
    fn generation_retention_survives_the_keychain() {
        // The reason the stored unit is the whole generations value: a crash
        // between Intuit rotating and the new token landing must still leave a
        // previous generation to fall back through.
        let store = store();
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        store.save(&generations).unwrap();
        for index in 2..=5 {
            generations.rotate(token_set(&index.to_string(), at(index * 3600)));
            store.save(&generations).unwrap();
        }

        let loaded = store.load(&realm()).unwrap().unwrap();
        assert_eq!(loaded.current.refresh_token, "refresh-5");
        assert_eq!(loaded.previous.len(), RETAINED_GENERATIONS);
        let order: Vec<&str> = loaded
            .candidates()
            .map(|set| set.refresh_token.as_str())
            .collect();
        assert_eq!(order, ["refresh-5", "refresh-4", "refresh-3"]);
    }

    #[test]
    fn realms_are_stored_separately() {
        let store = store();
        store
            .save(&TokenGenerations::new(realm(), token_set("a", at(0))))
            .unwrap();
        store
            .save(&TokenGenerations::new(other_realm(), token_set("b", at(0))))
            .unwrap();

        assert_eq!(
            store.load(&realm()).unwrap().unwrap().current.refresh_token,
            "refresh-a"
        );
        assert_eq!(
            store
                .load(&other_realm())
                .unwrap()
                .unwrap()
                .current
                .refresh_token,
            "refresh-b"
        );
    }

    #[test]
    fn a_corrupt_item_reports_corruption_rather_than_returning_none() {
        // Same distinction the file store draws: "no tokens" means authorise,
        // "unreadable tokens" means stop before overwriting something
        // recoverable.
        let backend = MemoryBackend::new();
        backend.put_raw(DEFAULT_SERVICE, realm().as_str(), b"{ not json");
        let store = KeychainTokenStore::with_backend(backend, DEFAULT_SERVICE);
        assert!(matches!(store.load(&realm()), Err(AuthError::Corrupt(_))));
    }

    #[test]
    fn an_item_holding_another_realms_tokens_is_refused() {
        let backend = MemoryBackend::new();
        let foreign = TokenGenerations::new(other_realm(), token_set("x", at(0)));
        backend.put_raw(
            DEFAULT_SERVICE,
            realm().as_str(),
            &serde_json::to_vec(&foreign).unwrap(),
        );
        let store = KeychainTokenStore::with_backend(backend, DEFAULT_SERVICE);
        assert!(matches!(store.load(&realm()), Err(AuthError::Keychain(_))));
    }

    #[test]
    fn the_service_name_scopes_the_items() {
        // Two stores under different services never see each other's tokens,
        // so a development build cannot overwrite the production item.
        let backend = MemoryBackend::new();
        let production = KeychainTokenStore::with_backend(backend, DEFAULT_SERVICE);
        production
            .save(&TokenGenerations::new(realm(), token_set("prod", at(0))))
            .unwrap();

        let KeychainTokenStore { backend, .. } = production;
        let development = KeychainTokenStore::with_backend(backend, "qbo-local.dev");
        assert!(development.load(&realm()).unwrap().is_none());
    }
}
