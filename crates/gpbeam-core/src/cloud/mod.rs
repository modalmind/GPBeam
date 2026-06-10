//! Cloud mirroring subsystem (async). The cloud worker drains the persisted
//! `cloud_jobs` queue and uploads verified media to a remote destination
//! (Nextcloud via WebDAV) through the [`CloudUploader`] trait.

use crate::cloud::nextcloud::NextcloudUploader;
use crate::config::{CloudConfig, CloudKind};
use crate::credentials::CredentialStore;
use crate::error::{CoreError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

pub mod nextcloud;
pub mod worker;

/// Per-job resume cursor for chunked uploads. Persisted as JSON TEXT in
/// `cloud_jobs.resume_state`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResumeState {
    /// The Nextcloud chunked-upload directory id, once MKCOL has succeeded.
    pub upload_id: Option<String>,
    /// Bytes confirmed uploaded so far (sum of fully-stored chunks).
    pub uploaded_bytes: u64,
}

/// Result of a successful upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOutcome {
    /// The remote path (relative to the configured remote root) the file now lives at.
    pub remote_ref: String,
    /// Total bytes uploaded.
    pub bytes: u64,
    /// Server ETag (OC-ETag) if returned.
    pub etag: Option<String>,
}

/// Progress / lifecycle events emitted by the cloud worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudEvent {
    Uploading {
        file: String,
        uploaded: u64,
        total: u64,
    },
    Mirrored {
        file: String,
    },
    CloudFailed {
        file: String,
        error: String,
    },
    Deleted {
        file: String,
    },
    /// Post-upload delete-after-verify cleanup failed. NON-FATAL: the upload
    /// itself succeeded and the job is Done — consumers must not count this
    /// as a failed file (the CLI prints a warning; the UI folds it as an
    /// informational message).
    DeleteFailed {
        file: String,
        error: String,
    },
}

/// A pluggable cloud upload backend. Implemented by `NextcloudUploader`.
#[async_trait]
pub trait CloudUploader: Send + Sync {
    /// True if a remote object at `remote` already exists with byte size `size`.
    async fn already_present(&self, remote: &str, size: u64) -> Result<bool>;

    /// Upload `local` to `remote`. `total` is the file size in bytes. `resume`,
    /// if present, lets a chunked upload continue. `progress` is called with the
    /// cumulative bytes uploaded so far.
    async fn upload(
        &self,
        local: &Path,
        remote: &str,
        total: u64,
        resume: Option<ResumeState>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<UploadOutcome>;
}

/// True if `url` is an acceptable cloud base URL: `https://` with a non-empty
/// host (any host), or `http://` **only** for a loopback host. Plain http to a
/// remote host would send the app password and all uploaded footage in
/// cleartext (the uploader uses HTTP Basic auth), so it is rejected both at
/// GUI save time (src-tauri `config_io` delegates here) and at uploader build
/// time ([`build_uploader`], so hand-edited/pre-M3 configs cannot bypass it).
/// Kept dependency-light (no `url` crate) — full WebDAV validation happens
/// when the uploader actually connects.
pub fn is_valid_base_url(url: &str) -> bool {
    let (rest, is_https) = if let Some(r) = url.strip_prefix("https://") {
        (r, true)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, false)
    } else {
        return false;
    };
    // Authority is everything up to the first '/'; it must be non-empty.
    let authority = rest.split('/').next().unwrap_or("");
    if authority.trim().is_empty() {
        return false;
    }
    is_https || is_loopback_host(authority)
}

/// True for a loopback authority — `localhost`, `127.0.0.1`, or `::1` — with or
/// without a `[...]` IPv6 wrapper and/or a trailing `:port`.
pub fn is_loopback_host(authority: &str) -> bool {
    // Strip any `userinfo@` prefix first: the real host is after the last '@'.
    // Otherwise `http://[::1]@evil.com` would parse as loopback but actually
    // connect to evil.com in cleartext (an M3 bypass).
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(after_bracket) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[host]` optionally followed by `:port`. A missing
        // closing bracket, or any junk after `]` other than a numeric port,
        // is malformed and rejected (e.g. `[::1]extra` must not pass).
        let (ipv6, suffix) = match after_bracket.split_once(']') {
            Some(parts) => parts,
            None => return false,
        };
        let suffix_ok = suffix.is_empty()
            || (suffix.starts_with(':') && suffix[1..].chars().all(|c| c.is_ascii_digit()));
        if !suffix_ok {
            return false;
        }
        ipv6
    } else {
        // `host` / `host:port` -> strip a single trailing numeric port. A bare
        // IPv6 like `::1` has multiple colons and no port, so only strip when
        // the part before the last colon is itself colon-free.
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => authority,
        }
    };
    // Hostnames are case-insensitive (RFC 1035/1123); IPv6 hex is too.
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

/// Build the concrete cloud uploader for a `CloudConfig`, looking its secret up
/// in `store` by `destination_id`. Returns `CoreError::Config` (non-retryable)
/// when no credential is configured for that destination.
pub fn build_uploader(
    cfg: &CloudConfig,
    store: &dyn CredentialStore,
) -> Result<Arc<dyn CloudUploader>> {
    // M3 parity at runtime: the GUI save path rejects cleartext-http base URLs,
    // but a hand-edited or pre-M3 config reaches the worker/CLI through this
    // function — re-check here so Basic auth + footage are never sent over
    // plain http to a non-loopback host.
    if !is_valid_base_url(&cfg.base_url) {
        return Err(CoreError::Config(format!(
            "cloud base_url {:?} must be https:// with a host (http:// is allowed \
             only for loopback hosts like localhost/127.0.0.1)",
            cfg.base_url
        )));
    }
    let mut secret = store.get(&cfg.destination_id)?.ok_or_else(|| {
        CoreError::Config(format!(
            "no credentials configured for cloud destination '{}'",
            cfg.destination_id
        ))
    })?;
    // Keychain-only (pure GUI) setups store just the app-password; the username
    // lives in `[cloud] username`. Fall back to it so the DAV path and Basic
    // auth use the real account instead of an empty string (which 401s).
    if secret.username.is_empty() {
        secret.username = cfg.username.clone();
    }
    match cfg.kind {
        CloudKind::Nextcloud => {
            let up = NextcloudUploader::new(cfg, &secret)?;
            Ok(Arc::new(up))
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::error::CoreError;
    use std::sync::Mutex;

    /// Records calls and returns a canned outcome or an injected error.
    /// Used by worker tests in Phase 4.
    pub struct MockUploader {
        pub outcome: UploadOutcome,
        /// If `Some`, `upload` returns this error instead of `outcome`.
        pub fail_with: Option<CoreError>,
        /// `already_present` return value.
        pub present: bool,
        pub calls: Mutex<Vec<String>>,
    }

    impl MockUploader {
        pub fn new(outcome: UploadOutcome) -> Self {
            MockUploader {
                outcome,
                fail_with: None,
                present: false,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CloudUploader for MockUploader {
        async fn already_present(&self, remote: &str, _size: u64) -> Result<bool> {
            self.calls.lock().unwrap().push(format!("present:{remote}"));
            Ok(self.present)
        }

        async fn upload(
            &self,
            _local: &Path,
            remote: &str,
            total: u64,
            _resume: Option<ResumeState>,
            progress: &mut (dyn FnMut(u64) + Send),
        ) -> Result<UploadOutcome> {
            self.calls.lock().unwrap().push(format!("upload:{remote}"));
            if let Some(err) = &self.fail_with {
                return Err(clone_err(err));
            }
            progress(total);
            Ok(self.outcome.clone())
        }
    }

    /// Cheap clone for the injected-error case (CoreError is not Clone).
    fn clone_err(err: &CoreError) -> CoreError {
        match err {
            CoreError::CloudAuth(m) => CoreError::CloudAuth(m.clone()),
            CoreError::Http { status, msg } => CoreError::Http {
                status: *status,
                msg: msg.clone(),
            },
            CoreError::Config(m) => CoreError::Config(m.clone()),
            other => CoreError::Config(format!("{other}")),
        }
    }
}

#[cfg(test)]
mod build_uploader_tests {
    use super::*;
    use crate::config::{CloudConfig, CloudKind, MirrorMode};
    use crate::credentials::EnvConfigStore;

    fn cfg() -> CloudConfig {
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

    #[test]
    fn missing_secret_is_a_config_error() {
        let store = EnvConfigStore::empty(None, None);
        // `Arc<dyn CloudUploader>` is not `Debug`, so match the result directly
        // rather than `unwrap_err()`.
        match build_uploader(&cfg(), &store) {
            Err(crate::error::CoreError::Config(msg)) => {
                assert!(
                    msg.contains("home-nc"),
                    "message should name the destination: {msg}"
                );
            }
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected a Config error, got an uploader"),
        }
    }

    #[test]
    fn present_secret_builds_an_uploader() {
        let toml = r#"
[credentials.home-nc]
username = "alice"
app_password = "abcd-efgh-ijkl"
"#;
        let store = EnvConfigStore::from_toml_str(toml, None, None).unwrap();
        let up = build_uploader(&cfg(), &store).expect("uploader builds with a present secret");
        // It is an Arc<dyn CloudUploader>; just confirm we got one.
        assert_eq!(std::sync::Arc::strong_count(&up), 1);
    }

    fn store_with_password() -> EnvConfigStore {
        EnvConfigStore::from_toml_str(
            "[credentials.home-nc]\nusername = \"alice\"\napp_password = \"pw\"\n",
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn http_non_loopback_base_url_is_a_config_error() {
        // The GUI save path already rejects cleartext http (M3), but a
        // hand-edited/pre-M3 config reaches the worker/CLI through
        // build_uploader — it must refuse to send Basic auth + footage over
        // plain http to a non-loopback host.
        let mut cfg = cfg();
        cfg.base_url = "http://nas.example.com".into();
        match build_uploader(&cfg, &store_with_password()) {
            Err(crate::error::CoreError::Config(msg)) => {
                assert!(
                    msg.contains("https"),
                    "message should point the user at https: {msg}"
                );
            }
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected a Config error, got an uploader"),
        }
    }

    #[test]
    fn http_loopback_base_url_still_builds() {
        // CRITICAL: the wiremock suites (and a self-hosted local Nextcloud)
        // use http://127.0.0.1 — the loopback exemption must hold at runtime.
        for base in [
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://[::1]:9000",
        ] {
            let mut cfg = cfg();
            cfg.base_url = base.into();
            assert!(
                build_uploader(&cfg, &store_with_password()).is_ok(),
                "loopback http base_url {base} must build"
            );
        }
    }

    #[test]
    fn core_is_valid_base_url_matches_gui_contract() {
        // Behavior pin: identical to the former src-tauri/config_io.rs impl
        // (which now delegates here): https any host, http loopback only.
        assert!(is_valid_base_url("https://cloud.example.com"));
        assert!(is_valid_base_url("https://192.168.1.10:8443/nextcloud"));
        assert!(is_valid_base_url("http://localhost:8080"));
        assert!(is_valid_base_url("http://127.0.0.1"));
        assert!(is_valid_base_url("http://[::1]:8080/nextcloud"));
        assert!(!is_valid_base_url("http://cloud.example.com"));
        assert!(!is_valid_base_url("http://192.168.1.10:8080"));
        assert!(!is_valid_base_url("ftp://cloud.example.com"));
        assert!(!is_valid_base_url("https://"));
        // userinfo must not spoof loopback.
        assert!(!is_valid_base_url("http://[::1]@evil.com"));
        assert!(!is_valid_base_url("http://127.0.0.1@evil.com"));
        assert!(is_valid_base_url("http://user@127.0.0.1"));
    }

    #[tokio::test]
    async fn empty_secret_username_falls_back_to_cloud_config_username() {
        // Keychain-only (pure GUI) setup: the keyring store resolves a Secret
        // whose username is "" (only the app-password lives in the keychain;
        // the username lives in `[cloud] username`). The uploader must use
        // cfg.username for BOTH the DAV path and Basic auth — not "".
        use wiremock::matchers::{basic_auth, method as wm_method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"><d:response>
  <d:propstat><d:prop><d:getcontentlength>2048</d:getcontentlength></d:prop>
  <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
</d:response></d:multistatus>"#;
        Mock::given(wm_method("PROPFIND"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/clip.mp4"))
            .and(basic_auth("alice", "env-pw"))
            .respond_with(ResponseTemplate::new(207).set_body_raw(xml, "application/xml"))
            .expect(1)
            .mount(&server)
            .await;

        // Resolves Secret { username: "", app_password: "env-pw" } — same shape
        // as KeyringCredentialStore with a keychain-only password.
        let store = EnvConfigStore::empty(None, Some("env-pw".into()));
        let mut cfg = cfg();
        cfg.base_url = server.uri(); // http://127.0.0.1:<port> — loopback-exempt
        let up = build_uploader(&cfg, &store).expect("uploader builds");

        assert!(
            up.already_present("clip.mp4", 2048).await.unwrap(),
            "request must hit /files/alice/ with basic auth alice:env-pw"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockUploader;
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn mock_uploader_implements_trait() {
        let outcome = UploadOutcome {
            remote_ref: "GoPro/clip.mp4".into(),
            bytes: 1024,
            etag: Some("\"abc\"".into()),
        };
        let up = MockUploader::new(outcome.clone());

        let mut seen = 0u64;
        let mut cb = |n: u64| seen = n;
        let got = up
            .upload(
                Path::new("/tmp/clip.mp4"),
                "GoPro/clip.mp4",
                1024,
                None,
                &mut cb,
            )
            .await
            .unwrap();

        assert_eq!(got, outcome);
        assert_eq!(seen, 1024);
        assert!(!up.already_present("GoPro/clip.mp4", 1024).await.unwrap());
        assert_eq!(
            up.calls.lock().unwrap().as_slice(),
            &[
                "upload:GoPro/clip.mp4".to_string(),
                "present:GoPro/clip.mp4".to_string()
            ]
        );
    }
}
