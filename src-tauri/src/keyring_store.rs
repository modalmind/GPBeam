//! `CredentialStore` implementation backed by the OS keychain.
//!
//! Precedence for the app-password is **env > keychain > fallback**. Only the
//! app-password lives in the keychain; the username comes from
//! `CloudConfig.username` at call sites (the design keeps the core crate free
//! of any keychain dependency — `keyring` is a `src-tauri`-only dep).

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, RwLock};

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
/// Test-only (the production store always gets `SystemKeyring`), so it is
/// compiled out of release builds.
#[cfg(test)]
pub struct MemoryKeyring {
    entries: Mutex<HashMap<(String, String), String>>,
}

#[cfg(test)]
impl MemoryKeyring {
    pub fn new() -> Self {
        MemoryKeyring {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
impl Default for MemoryKeyring {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl KeyringBackend for MemoryKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let map = self.entries.lock().map_err(|e| e.to_string())?;
        Ok(map
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        let mut map = self.entries.lock().map_err(|e| e.to_string())?;
        map.insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
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
///
/// The TOML fallback is REFRESHABLE (behind an `RwLock`): it must track the
/// on-disk `gpbeam.toml`, not the startup snapshot. Otherwise migrating a
/// plaintext password into the keychain (which strips it from the file) or a
/// GUI save that rewrites the file would leave the old secret resolvable —
/// and `has_password` true — until the next app restart, so credential
/// revocation would not take effect. `migrate_plaintext_credentials` and
/// `save_config` call [`Self::refresh_fallback_from_file`] after touching the
/// file.
pub struct KeyringCredentialStore {
    service: String,
    backend: Arc<dyn KeyringBackend>,
    env_username: Option<String>,
    env_app_password: Option<String>,
    fallback: RwLock<Option<EnvConfigStore>>,
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
            fallback: RwLock::new(fallback),
        }
    }

    /// Rebuild the TOML fallback from the config file at `path`, exactly like
    /// the startup construction in `run()`: a missing or unparsable file clears
    /// the fallback. Called after every code path that rewrites `gpbeam.toml`
    /// (credential migrate, GUI save) so revocation/changes apply immediately.
    pub fn refresh_fallback_from_file(&self, path: &std::path::Path) {
        let rebuilt = std::fs::read_to_string(path).ok().and_then(|s| {
            EnvConfigStore::from_toml_str(
                &s,
                self.env_username.clone(),
                self.env_app_password.clone(),
            )
            .ok()
        });
        // Poisoning recovery mirrors crate::lock_recover: the data is a plain
        // snapshot, so recovering it beats bricking credential resolution.
        let mut guard = self
            .fallback
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = rebuilt;
    }

    /// The app-password stored in the keychain for `destination_id`, if any.
    fn keychain_password(&self, destination_id: &str) -> Result<Option<String>, String> {
        self.backend.get(&self.service, destination_id)
    }

    /// The fallback `Secret` for `destination_id`, if the fallback store has one.
    fn fallback_secret(&self, destination_id: &str) -> CoreResult<Option<Secret>> {
        let guard = self
            .fallback
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(store) => store.get(destination_id),
            None => Ok(None),
        }
    }

    /// Store the app-password for `destination_id` in the keychain (overwrites).
    pub fn set_password(&self, destination_id: &str, app_password: &str) -> Result<(), String> {
        self.backend
            .set(&self.service, destination_id, app_password)
    }

    /// Remove the keychain entry for `destination_id`. Missing entries are a no-op.
    pub fn delete_password(&self, destination_id: &str) -> Result<(), String> {
        self.backend.delete(&self.service, destination_id)
    }

    /// True if any source (env, keychain, or fallback) supplies an app-password
    /// for `destination_id`. UI hint only — does not resolve a full `Secret`.
    pub fn has_password(&self, destination_id: &str) -> bool {
        if self.env_app_password.is_some() {
            return true;
        }
        if matches!(self.keychain_password(destination_id), Ok(Some(_))) {
            return true;
        }
        matches!(self.fallback_secret(destination_id), Ok(Some(s)) if !s.app_password.is_empty())
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, destination_id: &str) -> CoreResult<Option<Secret>> {
        let fallback = self.fallback_secret(destination_id)?;

        // Password precedence: env > keychain > fallback. An EMPTY fallback
        // password counts as absent: a `[credentials.<id>]` entry that keeps
        // only the (non-secret) username after a keychain migration parses
        // with app_password = "" (see core `CredEntry`), and resolving that as
        // a real password would both defeat revocation (the migrated/stripped
        // secret must stop resolving) and hand the uploader a guaranteed-401.
        let keychain_pw = self
            .keychain_password(destination_id)
            .map_err(gpbeam_core::error::CoreError::Config)?;
        let app_password = self.env_app_password.clone().or(keychain_pw).or_else(|| {
            fallback
                .as_ref()
                .map(|s| s.app_password.clone())
                .filter(|pw| !pw.is_empty())
        });

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

    use gpbeam_core::cloud::build_uploader;
    use gpbeam_core::config::{CloudConfig, CloudKind, MirrorMode};

    // Cloud-config fixture mirroring crates/gpbeam-core/src/cloud/mod.rs.
    fn cloud_cfg() -> CloudConfig {
        CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "home-nc".into(),
            base_url: "https://nc.example.com".into(),
            username: "alice".into(),
            remote_root: "GoPro".into(),
            mirror_mode: MirrorMode::Auto,
            chunk_threshold: 50 * 1024 * 1024,
            tls_ca_pem: None,
            max_concurrency: 2,
            max_attempts: 8,
        }
    }

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
        backend
            .set("com.gpbeam.test", "nc1", "keychain-pw")
            .unwrap();
        let store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend,
            None, // no env username
            None, // no env password
            None, // no fallback
        );
        let secret = store
            .get("nc1")
            .unwrap()
            .expect("keychain password present");
        // Only the app-password lives in the keychain; username defaults to "".
        assert_eq!(secret.username, "");
        assert_eq!(secret.app_password, "keychain-pw");
    }

    #[test]
    fn env_password_wins_over_keychain_and_fallback() {
        let backend = Arc::new(MemoryKeyring::new());
        backend
            .set("com.gpbeam.test", "nc1", "keychain-pw")
            .unwrap();
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
        backend
            .set("com.gpbeam.test", "nc1", "keychain-pw")
            .unwrap();
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
        backend
            .set("com.gpbeam.test", "nc1", "keychain-pw")
            .unwrap();
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

    #[test]
    fn set_password_then_get_returns_it() {
        let backend = Arc::new(MemoryKeyring::new());
        let store =
            KeyringCredentialStore::new("com.gpbeam.test", backend.clone(), None, None, None);
        assert!(store.get("nc1").unwrap().is_none());
        store.set_password("nc1", "stored-pw").unwrap();
        let secret = store.get("nc1").unwrap().expect("stored");
        assert_eq!(secret.app_password, "stored-pw");
        // It really landed in the backend under (service, destination_id).
        assert_eq!(
            backend.get("com.gpbeam.test", "nc1").unwrap(),
            Some("stored-pw".to_string())
        );
    }

    #[test]
    fn delete_password_clears_keychain_entry() {
        let backend = Arc::new(MemoryKeyring::new());
        let store =
            KeyringCredentialStore::new("com.gpbeam.test", backend.clone(), None, None, None);
        store.set_password("nc1", "stored-pw").unwrap();
        assert!(store.has_password("nc1"));
        store.delete_password("nc1").unwrap();
        assert_eq!(backend.get("com.gpbeam.test", "nc1").unwrap(), None);
        assert!(!store.has_password("nc1"));
        // Deleting again is a no-op.
        store.delete_password("nc1").unwrap();
    }

    #[test]
    fn has_password_reflects_each_source() {
        // No source -> false.
        let empty = KeyringCredentialStore::new(
            "com.gpbeam.test",
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            None,
        );
        assert!(!empty.has_password("nc1"));

        // env password -> true (even with empty keychain and no fallback).
        let env_store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            Arc::new(MemoryKeyring::new()),
            None,
            Some("env-pw".to_string()),
            None,
        );
        assert!(env_store.has_password("nc1"));

        // keychain password -> true.
        let kc_backend = Arc::new(MemoryKeyring::new());
        kc_backend.set("com.gpbeam.test", "nc1", "kc-pw").unwrap();
        let kc_store = KeyringCredentialStore::new("com.gpbeam.test", kc_backend, None, None, None);
        assert!(kc_store.has_password("nc1"));
        // Different id with nothing stored -> false.
        assert!(!kc_store.has_password("other"));

        // fallback password -> true.
        let fb_store = KeyringCredentialStore::new(
            "com.gpbeam.test",
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            Some(fallback_with_nc1()),
        );
        assert!(fb_store.has_password("nc1"));
        assert!(!fb_store.has_password("unknown"));
    }

    #[test]
    fn refresh_fallback_tracks_the_rewritten_config_file() {
        // Finding: the toml fallback was parsed once at startup and immutable,
        // so stripping the file password (migrate) kept resolving the old secret
        // until restart. After a refresh the fallback must reflect the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(
            &path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"alice\"\napp_password=\"old-pw\"\n",
        )
        .unwrap();
        let initial =
            EnvConfigStore::from_toml_str(&std::fs::read_to_string(&path).unwrap(), None, None)
                .unwrap();
        let store = KeyringCredentialStore::new(
            "svc",
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            Some(initial),
        );
        assert!(
            store.has_password("nc1"),
            "startup snapshot resolves the file pw"
        );

        // The file's password is stripped (what migrate does)...
        crate::config_io::strip_credential_password(&path, "nc1").unwrap();
        // ...but WITHOUT a refresh the stale snapshot would still resolve it.
        store.refresh_fallback_from_file(&path);

        assert!(
            !store.has_password("nc1"),
            "after refresh the stripped password is no longer resolvable"
        );
        assert_eq!(
            store.get("nc1").unwrap(),
            None,
            "no source supplies a password"
        );
    }

    #[test]
    fn refresh_fallback_clears_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeyringCredentialStore::new(
            "svc",
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            Some(fallback_with_nc1()),
        );
        assert!(store.has_password("nc1"));
        store.refresh_fallback_from_file(&dir.path().join("absent.toml"));
        assert!(
            !store.has_password("nc1"),
            "missing file -> no fallback at all"
        );
    }

    #[test]
    fn refresh_fallback_picks_up_a_newly_written_credential() {
        // The refresh works in both directions: a credential ADDED to the file
        // becomes resolvable without a restart too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let store =
            KeyringCredentialStore::new("svc", Arc::new(MemoryKeyring::new()), None, None, None);
        assert!(!store.has_password("nc1"));
        std::fs::write(
            &path,
            "[credentials.nc1]\nusername=\"a\"\napp_password=\"new-pw\"\n",
        )
        .unwrap();
        store.refresh_fallback_from_file(&path);
        let secret = store.get("nc1").unwrap().expect("resolvable after refresh");
        assert_eq!(secret.app_password, "new-pw");
    }

    #[test]
    fn build_uploader_succeeds_with_keychain_backed_secret() {
        let backend = Arc::new(MemoryKeyring::new());
        // Store the app-password under the destination id, as the UI would.
        backend
            .set("com.gpbeam.app", "home-nc", "abcd-efgh-ijkl")
            .unwrap();
        let store = KeyringCredentialStore::new("com.gpbeam.app", backend, None, None, None);
        // `build_uploader` takes `&dyn CredentialStore`; our store qualifies.
        match build_uploader(&cloud_cfg(), &store) {
            Ok(up) => assert_eq!(Arc::strong_count(&up), 1),
            Err(e) => panic!("expected an uploader, got error: {e:?}"),
        }
    }

    #[test]
    fn build_uploader_fails_when_no_password_anywhere() {
        let store = KeyringCredentialStore::new(
            "com.gpbeam.app",
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            None,
        );
        match build_uploader(&cloud_cfg(), &store) {
            Err(gpbeam_core::error::CoreError::Config(msg)) => {
                assert!(
                    msg.contains("home-nc"),
                    "message names the destination: {msg}"
                );
            }
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected a Config error, got an uploader"),
        }
    }
}
