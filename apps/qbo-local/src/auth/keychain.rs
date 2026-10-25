//! The production token backend: the macOS keychain. `DESIGN.md` §5.1,
//! `HANDOFF.md` §2.1.
//!
//! One keychain item per realm, holding the whole [`TokenGenerations`] value —
//! current set plus the retained previous generations. That is the property
//! [`FileTokenStore`](super::FileTokenStore) gets from `atomic_write`, restated
//! for a different medium: a rotation is one write of one item, so there is no
//! crash point at which the current generation and its fallbacks disagree.
//!
//! The file backend's `write → fsync → rename → fsync directory` sequence has no
//! keychain equivalent to reproduce, and needs none. `SecItemUpdate` replaces an
//! item's data inside `securityd` as a single operation; the caller never holds
//! a half-written item. What the keychain does *not* do is keep history, which
//! is why the generations travel inside the value rather than as separate items.
//!
//! The encoding lives outside the `cfg` gate so it is tested on every platform.
//! Only the thin keychain wrapper is macOS-specific.

use crate::domain::RealmId;

use super::{AuthError, TokenGenerations};

/// Keychain service name every realm's item is filed under. The account is the
/// realm id, so the id itself lives only in the keychain — never in a tracked
/// file.
pub const KEYCHAIN_SERVICE: &str = "qbo-local.oauth-tokens";

/// Serialise for storage. Compact rather than pretty: nobody reads a keychain
/// item by eye, and the fingerprints in the rotation log are what to read
/// instead.
///
/// Called only by the macOS backend outside tests, but kept platform-independent
/// so its tests run on Linux CI too.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn encode(generations: &TokenGenerations) -> Result<Vec<u8>, AuthError> {
    Ok(serde_json::to_vec(generations)?)
}

/// Deserialise a stored item, and refuse one filed under the wrong realm.
///
/// An item whose contents name a different realm from the account it sits under
/// means something wrote the wrong tokens to the wrong place. Using them would
/// point one company's sync at another company's credentials, so it is treated
/// as corruption — investigate, do not overwrite.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn decode(realm: &RealmId, bytes: &[u8]) -> Result<TokenGenerations, AuthError> {
    let generations: TokenGenerations = serde_json::from_slice(bytes)?;
    if &generations.realm_id != realm {
        return Err(AuthError::RealmMismatch {
            expected: realm.clone(),
            found: generations.realm_id,
        });
    }
    Ok(generations)
}

#[cfg(target_os = "macos")]
pub use macos::KeychainTokenStore;

#[cfg(target_os = "macos")]
mod macos {
    use security_framework::passwords::{get_generic_password, set_generic_password};

    use super::{decode, encode, KEYCHAIN_SERVICE};
    use crate::auth::{AuthError, TokenGenerations, TokenStore};
    use crate::domain::RealmId;

    /// `errSecItemNotFound`. The only keychain failure that means "no tokens
    /// yet" rather than "something is wrong".
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    /// [`TokenStore`] over the user's login keychain.
    ///
    /// Generic-password items are local to this Mac: they are not iCloud
    /// Keychain items and are not files anywhere Dropbox can reach, which is the
    /// point of `DESIGN.md` §8's "keychain only".
    #[derive(Clone, Debug)]
    pub struct KeychainTokenStore {
        service: String,
    }

    impl Default for KeychainTokenStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl KeychainTokenStore {
        pub fn new() -> Self {
            Self::with_service(KEYCHAIN_SERVICE)
        }

        /// A store filed under a different service name. For tests, so they
        /// never touch the items the app itself uses.
        pub fn with_service(service: impl Into<String>) -> Self {
            KeychainTokenStore {
                service: service.into(),
            }
        }

        pub fn service(&self) -> &str {
            &self.service
        }
    }

    impl TokenStore for KeychainTokenStore {
        fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError> {
            match get_generic_password(&self.service, realm.as_str()) {
                Ok(bytes) => decode(realm, &bytes).map(Some),
                Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                // A locked keychain or a denied access prompt lands here, and
                // must not read as "no tokens" — that would send the caller off
                // to re-authorise and overwrite a perfectly good item.
                Err(error) => Err(keychain_error(error)),
            }
        }

        fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError> {
            let bytes = encode(generations)?;
            // Adds the item if absent, otherwise replaces its data in place.
            set_generic_password(&self.service, generations.realm_id.as_str(), &bytes)
                .map_err(keychain_error)
        }
    }

    /// The OS status and its message. Never the item's contents.
    fn keychain_error(error: security_framework::base::Error) -> AuthError {
        AuthError::Keychain(format!("{error} (OSStatus {})", error.code()))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::auth::{TokenSet, RETAINED_GENERATIONS};

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

    fn three_generations() -> TokenGenerations {
        let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
        generations.rotate(token_set("2", at(3600)));
        generations.rotate(token_set("3", at(7200)));
        generations
    }

    #[test]
    fn the_stored_unit_keeps_every_retained_generation() {
        // The whole reason the value is one item: a rotation that kept only the
        // current set would throw away the fallback §5.2 exists to provide.
        let generations = three_generations();
        let decoded = decode(&realm(), &encode(&generations).unwrap()).unwrap();
        assert_eq!(decoded, generations);
        assert_eq!(decoded.previous.len(), RETAINED_GENERATIONS);
    }

    #[test]
    fn an_item_for_another_realm_is_refused_not_used() {
        let other = RealmId::parse("9999999999999999").unwrap();
        let bytes = encode(&three_generations()).unwrap();
        assert!(matches!(
            decode(&other, &bytes),
            Err(AuthError::RealmMismatch { .. })
        ));
    }

    #[test]
    fn an_unreadable_item_is_corruption_not_absence() {
        assert!(matches!(
            decode(&realm(), b"{ not json"),
            Err(AuthError::Corrupt(_))
        ));
    }

    /// Against the real login keychain, under a throwaway service name that is
    /// removed afterwards. Ignored by default so `cargo test` never depends on
    /// an unlocked keychain; run on a Mac with
    /// `cargo test -p qbo-local keychain -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the real login keychain"]
    fn tokens_survive_a_round_trip_through_the_real_keychain() {
        use crate::auth::TokenStore;
        use security_framework::passwords::delete_generic_password;

        let service = format!("{KEYCHAIN_SERVICE}.test-{}", uuid::Uuid::now_v7());
        let store = KeychainTokenStore::with_service(&service);

        let outcome = std::panic::catch_unwind(|| {
            assert!(store.load(&realm()).unwrap().is_none());

            let mut generations = TokenGenerations::new(realm(), token_set("1", at(0)));
            store.save(&generations).unwrap();
            // A rotation replaces the item rather than adding a second one.
            generations.rotate(token_set("2", at(3600)));
            store.save(&generations).unwrap();

            // A fresh handle, as after an app restart.
            let reopened = KeychainTokenStore::with_service(&service);
            assert_eq!(reopened.load(&realm()).unwrap(), Some(generations));
        });

        // This test's own item, under its own service name — never the app's.
        let _ = delete_generic_password(&service, realm().as_str());
        outcome.unwrap();
    }
}
