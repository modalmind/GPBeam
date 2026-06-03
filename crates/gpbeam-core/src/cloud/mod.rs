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
    Uploading { file: String, uploaded: u64, total: u64 },
    Mirrored { file: String },
    CloudFailed { file: String, error: String },
    Deleted { file: String },
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

/// Build the concrete cloud uploader for a `CloudConfig`, looking its secret up
/// in `store` by `destination_id`. Returns `CoreError::Config` (non-retryable)
/// when no credential is configured for that destination.
pub fn build_uploader(
    cfg: &CloudConfig,
    store: &dyn CredentialStore,
) -> Result<Arc<dyn CloudUploader>> {
    let secret = store.get(&cfg.destination_id)?.ok_or_else(|| {
        CoreError::Config(format!(
            "no credentials configured for cloud destination '{}'",
            cfg.destination_id
        ))
    })?;
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
            self.calls
                .lock()
                .unwrap()
                .push(format!("present:{remote}"));
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
                assert!(msg.contains("home-nc"), "message should name the destination: {msg}");
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
            .upload(Path::new("/tmp/clip.mp4"), "GoPro/clip.mp4", 1024, None, &mut cb)
            .await
            .unwrap();

        assert_eq!(got, outcome);
        assert_eq!(seen, 1024);
        assert!(!up.already_present("GoPro/clip.mp4", 1024).await.unwrap());
        assert_eq!(
            up.calls.lock().unwrap().as_slice(),
            &["upload:GoPro/clip.mp4".to_string(), "present:GoPro/clip.mp4".to_string()]
        );
    }
}
