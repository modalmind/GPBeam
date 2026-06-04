//! `CredentialStore` implementation backed by the OS keychain.
//!
//! Precedence for the app-password is **env > keychain > fallback**. Only the
//! app-password lives in the keychain; the username comes from
//! `CloudConfig.username` at call sites (the design keeps the core crate free
//! of any keychain dependency — `keyring` is a `src-tauri`-only dep).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
}
