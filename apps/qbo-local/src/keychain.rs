//! `KeychainTokenStore` — [`TokenStore`] over the macOS login keychain.
//! `HANDOFF.md` §2.1, `DESIGN.md` §5.
//!
//! One generic-password item per realm: service [`DEFAULT_SERVICE`] (or the
//! one passed to [`KeychainTokenStore::with_service`]), account = the realm id,
//! data = the realm's whole [`TokenGenerations`] as JSON. Keeping the stored
//! unit identical to `FileTokenStore`'s means a rotation is still one write of
//! current + two previous generations, never several items that can disagree
//! after a crash. `set_generic_password` updates the existing item in place
//! (`SecItemUpdate`) or adds it when absent; the keychain commits that write
//! atomically, which is what `atomic_write`'s fsync-and-rename sequence buys
//! the file store.
//!
//! macOS only. Every other platform keeps `FileTokenStore`; the selection
//! lives in `config::TokenStoreKind`.

use security_framework::base::Error as SecError;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::auth::{AuthError, TokenGenerations, TokenStore};
use crate::domain::RealmId;

/// The keychain service every realm's item is filed under.
/// BunzBooks-only. The earlier `com.aquamentor.qbo-local.tokens` holds tokens
/// minted for the app shared with the QuickBooks MCPs; it is left untouched
/// and never read.
pub const DEFAULT_SERVICE: &str = "bunzbooks-qbo-tokens";

/// `errSecItemNotFound`.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

pub struct KeychainTokenStore {
    service: String,
}

impl KeychainTokenStore {
    pub fn new() -> Self {
        Self::with_service(DEFAULT_SERVICE)
    }

    /// A store under a different service name — tests use a unique one so a
    /// test run can never read or clobber the real tokens.
    pub fn with_service(service: impl Into<String>) -> Self {
        KeychainTokenStore {
            service: service.into(),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// Remove a realm's item. Not part of [`TokenStore`] on purpose: nothing
    /// in the sync path deletes tokens. Used by the round-trip test to clean
    /// up after itself.
    pub fn delete(&self, realm: &RealmId) -> Result<(), AuthError> {
        match delete_generic_password(&self.service, realm.as_str()) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

impl Default for KeychainTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenStore for KeychainTokenStore {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError> {
        match get_generic_password(&self.service, realm.as_str()) {
            Ok(bytes) => {
                let generations: TokenGenerations = serde_json::from_slice(&bytes)?;
                if generations.realm_id != *realm {
                    return Err(AuthError::Keychain(format!(
                        "item for realm {realm} holds tokens for realm {}",
                        generations.realm_id
                    )));
                }
                Ok(Some(generations))
            }
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(generations)?;
        set_generic_password(&self.service, generations.realm_id.as_str(), &bytes)
            .map_err(keychain_error)
    }
}

fn keychain_error(error: SecError) -> AuthError {
    // `SecError`'s Display is the OS message ("The specified item could not
    // be found in the keychain.") plus the OSStatus — never item data.
    AuthError::Keychain(format!("{error} (OSStatus {})", error.code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenSet;
    use chrono::{TimeZone, Utc};

    fn set(tag: &str) -> TokenSet {
        TokenSet {
            access_token: format!("access-{tag}"),
            refresh_token: format!("refresh-{tag}"),
            obtained_at: Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap(),
            access_expires_in_secs: 3600,
            refresh_expires_in_secs: 8_726_400,
        }
    }

    /// Round-trips through the real login keychain. Ignored by default so
    /// `cargo test` stays hermetic (and CI, which is Linux, never compiles it
    /// anyway); run on the Mac with
    /// `cargo test -p qbo-local keychain -- --ignored`. macOS may show one
    /// "allow access" prompt the first time.
    #[test]
    #[ignore = "touches the real macOS keychain"]
    fn round_trips_generations_through_the_real_keychain() {
        let service = format!("{DEFAULT_SERVICE}.test.{}", uuid::Uuid::now_v7());
        let store = KeychainTokenStore::with_service(&service);
        let realm = RealmId::parse("9999000011112222").unwrap();

        assert_eq!(store.load(&realm).unwrap(), None);

        let mut generations = TokenGenerations::new(realm.clone(), set("g0"));
        store.save(&generations).unwrap();
        assert_eq!(store.load(&realm).unwrap(), Some(generations.clone()));

        // A rotation is one overwrite of the same item, retention intact.
        generations.rotate(set("g1"));
        generations.rotate(set("g2"));
        generations.rotate(set("g3"));
        store.save(&generations).unwrap();
        let loaded = store.load(&realm).unwrap().unwrap();
        assert_eq!(loaded.current.refresh_token, "refresh-g3");
        assert_eq!(
            loaded
                .previous
                .iter()
                .map(|s| s.refresh_token.as_str())
                .collect::<Vec<_>>(),
            vec!["refresh-g2", "refresh-g1"]
        );

        store.delete(&realm).unwrap();
        assert_eq!(store.load(&realm).unwrap(), None);
        store.delete(&realm).unwrap(); // idempotent
    }
}
