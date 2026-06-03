use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not a GoPro card: {0}")]
    NotGoProCard(PathBuf),
    #[error("insufficient space on destination: need {need} bytes, have {have}")]
    InsufficientSpace { need: u64, have: u64 },
    #[error("verification failed for {0}")]
    VerifyFailed(PathBuf),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Helper to attach a path to an io::Error.
pub(crate) fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> CoreError {
    let path = path.into();
    move |source| CoreError::Io { path, source }
}
