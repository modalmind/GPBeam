//! `CredentialStore` implementation backed by the OS keychain.
//!
//! Precedence for the app-password is **env > keychain > fallback**. Only the
//! app-password lives in the keychain; the username comes from
//! `CloudConfig.username` at call sites (the design keeps the core crate free
//! of any keychain dependency — `keyring` is a `src-tauri`-only dep).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpbeam_core::credentials::{CredentialStore, EnvConfigStore, Secret};
use gpbeam_core::error::Result as CoreResult;

/// Pluggable secret backend. The real implementation (`SystemKeyring`) talks to
/// the OS keychain via `keyring::Entry`; `MemoryKeyring` is an in-memory fake so
/// unit tests never touch the real keychain.
pub trait KeyringBackend: Send + Sync {
    /// Fetch the secret for `(service, account)`, or `None` if absent.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    /// Store (or overwrite) the secret for `(service, account)`.
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), String>;
    /// Remove the secret for `(service, account)`. Missing entries are a no-op.
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

/// In-memory `KeyringBackend` for tests. Keyed by `(service, account)`.
pub struct MemoryKeyring {
    entries: Mutex<HashMap<(String, String), String>>,
}

impl MemoryKeyring {
    pub fn new() -> Self {
        MemoryKeyring {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryKeyring {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringBackend for MemoryKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let map = self.entries.lock().map_err(|e| e.to_string())?;
        Ok(map.get(&(service.to_string(), account.to_string())).cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        let mut map = self.entries.lock().map_err(|e| e.to_string())?;
        map.insert((service.to_string(), account.to_string()), secret.to_string());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        let mut map = self.entries.lock().map_err(|e| e.to_string())?;
        map.remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
}

/// Real `KeyringBackend` over the OS-native secure store via the `keyring`
/// crate (`macOS` Keychain, Windows Credential Manager, Secret Service on
/// Linux). Each `(service, account)` pair maps to one `keyring::Entry`.
pub struct SystemKeyring;

impl KeyringBackend for SystemKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        entry.set_password(secret).map_err(|e| e.to_string())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting a missing credential is a no-op, mirroring MemoryKeyring.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// `CredentialStore` over the OS keychain with an env override and a TOML
/// fallback. Only the **app-password** is stored in the keychain (keyed by
/// `destination_id`); the username is supplied by callers from
/// `CloudConfig.username`. `get` fills `Secret.username` from `env_username`,
/// else the fallback file entry, else `""`.
///
/// Precedence for the app-password: `env_app_password` > keychain >
/// `fallback.get(id).app_password`.
pub struct KeyringCredentialStore {
    service: String,
    backend: Arc<dyn KeyringBackend>,
    env_username: Option<String>,
    env_app_password: Option<String>,
    fallback: Option<EnvConfigStore>,
}

impl KeyringCredentialStore {
    pub fn new(
        service: impl Into<String>,
        backend: Arc<dyn KeyringBackend>,
        env_username: Option<String>,
        env_app_password: Option<String>,
        fallback: Option<EnvConfigStore>,
    ) -> Self {
        KeyringCredentialStore {
            service: service.into(),
            backend,
            env_username,
            env_app_password,
            fallback,
        }
    }

    /// The app-password stored in the keychain for `destination_id`, if any.
    fn keychain_password(&self, destination_id: &str) -> Result<Option<String>, String> {
        self.backend.get(&self.service, destination_id)
    }

    /// The fallback `Secret` for `destination_id`, if the fallback store has one.
    fn fallback_secret(&self, destination_id: &str) -> CoreResult<Option<Secret>> {
        match &self.fallback {
            Some(store) => store.get(destination_id),
            None => Ok(None),
        }
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, destination_id: &str) -> CoreResult<Option<Secret>> {
        let fallback = self.fallback_secret(destination_id)?;

        // Password precedence: env > keychain > fallback.
        let keychain_pw = self
            .keychain_password(destination_id)
            .map_err(gpbeam_core::error::CoreError::Config)?;
        let app_password = self
            .env_app_password
            .clone()
            .or(keychain_pw)
            .or_else(|| fallback.as_ref().map(|s| s.app_password.clone()));

        let app_password = match app_password {
            Some(pw) => pw,
            // No source had a password -> no resolvable secret.
            None => return Ok(None),
        };

        // Username: env > fallback file entry > "".
        let username = self
            .env_username
            .clone()
            .or_else(|| fallback.as_ref().map(|s| s.username.clone()))
            .unwrap_or_default();

        Ok(Some(Secret {
            username,
            app_password,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // helper: a fallback EnvConfigStore with one file entry for "nc1".
    fn fallback_with_nc1() -> EnvConfigStore {
        let toml = r#"
[credentials.nc1]
username = "file-user"
app_password = "file-pw"
"#;
        EnvConfigStore::from_toml_str(toml, None, None).unwrap()
    }

    #[test]
    fn memory_keyring_set_get_delete_roundtrip() {
        let kr = MemoryKeyring::new();
        // Empty to start.
        assert_eq!(kr.get("svc", "acct").unwrap(), None);
        // Set then get.
        kr.set("svc", "acct", "secret-1").unwrap();
        assert_eq!(kr.get("svc", "acct").unwrap(), Some("secret-1".to_string()));
        // Overwrite.
        kr.set("svc", "acct", "secret-2").unwrap();
        assert_eq!(kr.get("svc", "acct").unwrap(), Some("secret-2".to_string()));
        // Distinct (service, account) keys do not collide.
        kr.set("svc", "other", "secret-x").unwrap();
        assert_eq!(kr.get("svc", "acct").unwrap(), Some("secret-2".to_string()));
        // Delete clears.
        kr.delete("svc", "acct").unwrap();
        assert_eq!(kr.get("svc", "acct").unwrap(), None);
        // Deleting a missing entry is a no-op (Ok).
        kr.delete("svc", "acct").unwrap();
    }

    #[test]
    fn memory_keyring_is_object_safe_behind_arc() {
        let kr: Arc<dyn KeyringBackend> = Arc::new(MemoryKeyring::new());
        kr.set("s", "a", "p").unwrap();
        assert_eq!(kr.get("s", "a").unwrap(), Some("p".to_string()));
    }

    #[test]
    fn system_keyring_constructs_and_is_object_safe() {
        // Never calls get/set/delete: that would hit the real OS keychain. We only
        // prove the type exists, constructs, and is usable behind the trait object.
        let kr: Arc<dyn KeyringBackend> = Arc::new(SystemKeyring);
        // Use the Arc so it is not optimized away; do not invoke keychain methods.
        assert_eq!(Arc::strong_count(&kr), 1);
    }

    #[test]
    fn get_returns_none_when_no_source_has_a_password() {
        let backend = Arc::new(MemoryKeyring::new());
        let store = KeyringCredentialStore::new("com.gpbeam.test", backend, None, None, None);
        assert_eq!(store.get("nc1").unwrap(), None);
    }

    #[test]
    fn keychain_password_is_returned_with_empty_username() {
        let backend = Arc::new(MemoryKeyring::new());
        // Simulate the UI having stored the app-password under the destination id.
        backend.set("com.gpbeam.test", "nc1", "keychain-pw").unwrap();
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            None, // no env username
            None, // no env password
            None, // no fallback
        );
        let secret = store.get("nc1").unwrap().expect("keychain password present");
        // Only the app-password lives in the keychain; username defaults to "".
        assert_eq!(secret.username, "");
        assert_eq!(secret.app_password, "keychain-pw");
    }

    #[test]
    fn env_password_wins_over_keychain_and_fallback() {
        let backend = Arc::new(MemoryKeyring::new());
        backend.set("com.gpbeam.test", "nc1", "keychain-pw").unwrap();
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            Some("env-user".to_string()),
            Some("env-pw".to_string()),
            Some(fallback_with_nc1()),
        );
        let secret = store.get("nc1").unwrap().expect("env produces a secret");
        assert_eq!(secret.username, "env-user");
        assert_eq!(secret.app_password, "env-pw");
    }

    #[test]
    fn env_username_fills_username_when_keychain_supplies_password() {
        let backend = Arc::new(MemoryKeyring::new());
        backend.set("com.gpbeam.test", "nc1", "keychain-pw").unwrap();
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            Some("env-user".to_string()),
            None, // no env password -> keychain wins for the password
            None,
        );
        let secret = store.get("nc1").unwrap().expect("present");
        assert_eq!(secret.username, "env-user");
        assert_eq!(secret.app_password, "keychain-pw");
    }

    #[test]
    fn fallback_used_when_keychain_empty_filling_username_and_password() {
        let backend = Arc::new(MemoryKeyring::new());
        // keychain has nothing for nc1.
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            None,
            None,
            Some(fallback_with_nc1()),
        );
        let secret = store.get("nc1").unwrap().expect("fallback present");
        // Username and password both come from the fallback file entry.
        assert_eq!(secret.username, "file-user");
        assert_eq!(secret.app_password, "file-pw");
    }

    #[test]
    fn keychain_password_beats_fallback_password() {
        let backend = Arc::new(MemoryKeyring::new());
        backend.set("com.gpbeam.test", "nc1", "keychain-pw").unwrap();
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            None,
            None,
            Some(fallback_with_nc1()),
        );
        let secret = store.get("nc1").unwrap().expect("present");
        // Password from keychain; username falls back to the file entry's username.
        assert_eq!(secret.app_password, "keychain-pw");
        assert_eq!(secret.username, "file-user");
    }
}
