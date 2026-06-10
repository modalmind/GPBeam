//! Wired (USB) GoPro ingest over the Open GoPro HTTP API.
//!
//! `client` talks to the camera; `detect` finds it on the IP-over-USB interface
//! (Phase 3); `offload` drives the verify→ledger→cloud pipeline (Phase 4).

pub mod client;
pub mod detect;
pub mod offload;

use crate::error::{CoreError, Result};
use std::path::Path;

/// Run a synchronous, potentially heavy filesystem/hash leg on tokio's blocking
/// pool so it never pins an async worker thread (the SD path does the same via
/// `spawn_blocking`). A panicked/cancelled task surfaces as an `Io` error at
/// `path`.
pub(crate) async fn run_blocking<T, F>(path: &Path, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    match tokio::task::spawn_blocking(f).await {
        Ok(res) => res,
        Err(e) => Err(CoreError::Io {
            path,
            source: std::io::Error::other(e),
        }),
    }
}
