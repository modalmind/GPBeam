use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
    /// Reserved for a future strict mode. M1 reports non-GoPro volumes via
    /// `RunEvent::NotGoPro` and returns an empty summary instead of this error.
    #[error("not a GoPro card: {0}")]
    NotGoProCard(PathBuf),
    #[error("insufficient space on destination: need {need} bytes, have {have}")]
    InsufficientSpace { need: u64, have: u64 },
    #[error("verification failed for {0}")]
    VerifyFailed(PathBuf),
    /// An HTTP-layer failure. `status` is the response code when one was
    /// received; `None` means a transport-level error (no response).
    #[error("http error {status:?}: {msg}")]
    Http { status: Option<u16>, msg: String },
    /// A run was deliberately aborted mid-plan by a guard (e.g. the wired
    /// offload circuit breaker tripping when the camera goes offline). It is a
    /// control signal, NOT a network error — `Display` is the bare message so it
    /// reaches the user without an `http error None:` prefix, and it is
    /// non-retryable (the run stopped on purpose; the caller re-arms instead).
    #[error("{0}")]
    RunAborted(String),
    /// 401 / invalid app-password. Non-retryable; surfaced to the user with
    /// guidance to generate a Nextcloud app password.
    #[error("cloud auth error: {0}")]
    CloudAuth(String),
    /// Configuration / `gpbeam.toml` parse error.
    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Helper to attach a path to an io::Error.
pub(crate) fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> CoreError {
    let path = path.into();
    move |source| CoreError::Io { path, source }
}

/// Classify an error for the cloud retry loop.
///
/// Retryable: transport errors (`Http { status: None, .. }`) and HTTP
/// 429 (rate limit), 408 (request timeout), and any 5xx. Everything else —
/// including 4xx client errors, `CloudAuth`, `Config`, `Db`, `Io`, and the
/// local-offload errors — is non-retryable.
pub fn is_retryable(err: &CoreError) -> bool {
    match err {
        CoreError::Http { status: None, .. } => true,
        CoreError::Http {
            status: Some(code), ..
        } => *code >= 500 || *code == 429 || *code == 408,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_display_includes_status_and_msg() {
        let e = CoreError::Http {
            status: Some(500),
            msg: "boom".into(),
        };
        assert_eq!(e.to_string(), "http error Some(500): boom");
        let e = CoreError::Http {
            status: None,
            msg: "transport".into(),
        };
        assert_eq!(e.to_string(), "http error None: transport");
    }

    #[test]
    fn server_errors_are_retryable() {
        for s in [500u16, 502, 503, 408, 429] {
            let e = CoreError::Http {
                status: Some(s),
                msg: "x".into(),
            };
            assert!(is_retryable(&e), "status {s} should be retryable");
        }
    }

    #[test]
    fn transport_error_is_retryable() {
        let e = CoreError::Http {
            status: None,
            msg: "connection reset".into(),
        };
        assert!(is_retryable(&e));
    }

    #[test]
    fn client_errors_are_not_retryable() {
        for s in [400u16, 401, 403, 404, 409, 412] {
            let e = CoreError::Http {
                status: Some(s),
                msg: "x".into(),
            };
            assert!(!is_retryable(&e), "status {s} should NOT be retryable");
        }
    }

    #[test]
    fn run_aborted_is_not_retryable_and_displays_bare_message() {
        let e = CoreError::RunAborted("camera offline: aborting run".into());
        // A deliberate abort must never be re-driven as a transient error.
        assert!(!is_retryable(&e));
        // And it must NOT carry the "http error None:" prefix into the toast.
        assert_eq!(e.to_string(), "camera offline: aborting run");
    }

    #[test]
    fn auth_config_db_io_are_not_retryable() {
        assert!(!is_retryable(&CoreError::CloudAuth("nope".into())));
        assert!(!is_retryable(&CoreError::Config("bad toml".into())));
        assert!(!is_retryable(&CoreError::VerifyFailed(
            std::path::PathBuf::from("/x")
        )));
        let io = CoreError::Io {
            path: std::path::PathBuf::from("/x"),
            source: std::io::Error::other("io"),
        };
        assert!(!is_retryable(&io));
    }
}
