//! Cloud mirroring subsystem (async). Phase 1 only needs `ResumeState`, which
//! the ledger serializes to JSON in the `cloud_jobs` queue.

use serde::{Deserialize, Serialize};

/// Per-job resume cursor for chunked uploads. Persisted as JSON TEXT in
/// `cloud_jobs.resume_state`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResumeState {
    /// The Nextcloud chunked-upload directory id, once MKCOL has succeeded.
    pub upload_id: Option<String>,
    /// Bytes confirmed uploaded so far (sum of fully-stored chunks).
    pub uploaded_bytes: u64,
}
