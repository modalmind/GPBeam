use crate::error::{CoreError, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// A resolved credential for a cloud destination.
///
/// `ZeroizeOnDrop` wipes the `app_password` (and `username`) bytes from memory
/// when the `Secret` is dropped, so a resolved credential does not linger in a
/// heap buffer after use (finding L3, defense-in-depth against memory dumps).
#[derive(Clone, PartialEq, Eq, zeroize::ZeroizeOnDrop)]
pub struct Secret {
    pub username: String,
    pub app_password: String,
}

/// Manual `Debug` so a `{:?}` (logs, error context, `dbg!`) can never leak the
/// cleartext app password — companion to the `ZeroizeOnDrop` hygiene above.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("username", &self.username)
            .field("app_password", &"[redacted]")
            .finish()
    }
}

/// Looks up a [`Secret`] for a given destination id.
pub trait CredentialStore: Send + Sync {
    fn get(&self, destination_id: &str) -> Result<Option<Secret>>;
}

#[derive(Debug, Deserialize)]
struct CredEntry {
    username: String,
    // Optional so a `[credentials.<id>]` entry that keeps only the (non-secret)
    // username after a keychain migration still parses (finding M2). A missing
    // app_password resolves to empty, which the keychain/env password then wins
    // over in `KeyringCredentialStore::get`.
    #[serde(default)]
    app_password: String,
}

#[derive(Debug, Deserialize, Default)]
struct CredFile {
    #[serde(default)]
    credentials: HashMap<String, CredEntry>,
}

/// A [`CredentialStore`] backed by the `[credentials.<id>]` tables of a config
/// string, with an optional environment override that wins over file values.
///
/// The override values are injected at construction (rather than read from the
/// real process environment) so tests are deterministic.
#[derive(Debug)]
pub struct EnvConfigStore {
    entries: HashMap<String, Secret>,
    env_username: Option<String>,
    env_app_password: Option<String>,
}

impl EnvConfigStore {
    /// Parse the `[credentials.*]` tables out of a TOML string. The two env
    /// override values (typically read from `GPBEAM_NC_USERNAME` /
    /// `GPBEAM_NC_APP_PASSWORD` by the caller) are injected here.
    pub fn from_toml_str(
        s: &str,
        env_username: Option<String>,
        env_app_password: Option<String>,
    ) -> Result<Self> {
        let parsed: CredFile = toml::from_str(s).map_err(|e| CoreError::Config(e.to_string()))?;
        let entries = parsed
            .credentials
            .into_iter()
            .map(|(id, e)| {
                (
                    id,
                    Secret {
                        username: e.username,
                        app_password: e.app_password,
                    },
                )
            })
            .collect();
        Ok(EnvConfigStore {
            entries,
            env_username,
            env_app_password,
        })
    }

    /// An empty store with no file entries; only the injected env override (if
    /// any) can produce a [`Secret`].
    pub fn empty(env_username: Option<String>, env_app_password: Option<String>) -> Self {
        EnvConfigStore {
            entries: HashMap::new(),
            env_username,
            env_app_password,
        }
    }
}

impl CredentialStore for EnvConfigStore {
    fn get(&self, destination_id: &str) -> Result<Option<Secret>> {
        let base = self.entries.get(destination_id).cloned();
        // The env override wins: if either env value is present, it replaces
        // the corresponding field of the (possibly empty) base secret. A bare
        // env app-password with no file entry and no env username still yields
        // a Secret, using an empty username only if none is available.
        match (&self.env_username, &self.env_app_password) {
            (None, None) => Ok(base),
            (env_user, env_pw) => {
                let username = env_user
                    .clone()
                    .or_else(|| base.as_ref().map(|b| b.username.clone()))
                    .unwrap_or_default();
                let app_password = env_pw
                    .clone()
                    .or_else(|| base.as_ref().map(|b| b.app_password.clone()))
                    .unwrap_or_default();
                Ok(Some(Secret {
                    username,
                    app_password,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [credentials.nc1]
        username = "alice"
        app_password = "file-pw-aaaa"

        [credentials.nc2]
        username = "bob"
        app_password = "file-pw-bbbb"
    "#;

    #[test]
    fn secret_debug_redacts_app_password() {
        // Companion to zeroize hygiene: a `{:?}` in logs or error context must
        // never print the cleartext app password.
        let s = Secret {
            username: "alice".into(),
            app_password: "super-secret-pw".into(),
        };
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("super-secret-pw"),
            "Debug leaked the password: {dbg}"
        );
        assert!(
            dbg.contains("[redacted]"),
            "Debug should mark the redaction: {dbg}"
        );
        assert!(dbg.contains("alice"), "username stays visible: {dbg}");
    }

    #[test]
    fn secret_zeroizes_on_drop() {
        // L3: the resolved credential must wipe its app_password from memory on
        // drop. Compile-time assertion that the ZeroizeOnDrop derive is present
        // (removing it breaks this test's bound).
        fn assert_zod<T: zeroize::ZeroizeOnDrop>() {}
        assert_zod::<Secret>();
    }

    #[test]
    fn known_id_returns_file_secret() {
        let store = EnvConfigStore::from_toml_str(SAMPLE, None, None).unwrap();
        let s = store.get("nc1").unwrap().expect("nc1 present");
        assert_eq!(s.username, "alice");
        assert_eq!(s.app_password, "file-pw-aaaa");
    }

    #[test]
    fn missing_id_returns_none() {
        let store = EnvConfigStore::from_toml_str(SAMPLE, None, None).unwrap();
        assert!(store.get("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn env_app_password_overrides_file() {
        let store =
            EnvConfigStore::from_toml_str(SAMPLE, None, Some("env-pw-zzzz".into())).unwrap();
        let s = store.get("nc1").unwrap().expect("nc1 present");
        // username still from file, app_password from env override.
        assert_eq!(s.username, "alice");
        assert_eq!(s.app_password, "env-pw-zzzz");
    }

    #[test]
    fn env_username_and_password_both_override() {
        let store = EnvConfigStore::from_toml_str(
            SAMPLE,
            Some("env-user".into()),
            Some("env-pw-zzzz".into()),
        )
        .unwrap();
        let s = store.get("nc1").unwrap().expect("nc1 present");
        assert_eq!(s.username, "env-user");
        assert_eq!(s.app_password, "env-pw-zzzz");
    }

    #[test]
    fn empty_store_with_env_yields_secret() {
        let store = EnvConfigStore::empty(Some("env-user".into()), Some("env-pw-zzzz".into()));
        let s = store
            .get("anything")
            .unwrap()
            .expect("env produces a secret");
        assert_eq!(s.username, "env-user");
        assert_eq!(s.app_password, "env-pw-zzzz");
    }

    #[test]
    fn empty_store_no_env_returns_none() {
        let store = EnvConfigStore::empty(None, None);
        assert!(store.get("nc1").unwrap().is_none());
    }

    #[test]
    fn entry_without_app_password_parses_with_empty_password() {
        // After a keychain migration the file keeps only the username; that
        // entry must still parse (M2) so resolution can supply the username.
        let store =
            EnvConfigStore::from_toml_str("[credentials.nc1]\nusername = \"alice\"\n", None, None)
                .unwrap();
        let s = store.get("nc1").unwrap().expect("nc1 present");
        assert_eq!(s.username, "alice");
        assert_eq!(s.app_password, "");
    }

    #[test]
    fn invalid_toml_maps_to_config_error() {
        let err = EnvConfigStore::from_toml_str("= = bad", None, None).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)));
    }
}
