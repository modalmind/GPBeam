//! Long-lived cloud-worker runtime state (Phase 6 owns the tick loop). Phase 5
//! only depends on this struct's SHAPE for `AppCtx`; the fields are locked by
//! the M3 contract, so Phase 6 expands behavior without changing them.

/// The mutable cloud settings the lib.rs tick loop reads each pass. `save_config`
/// swaps `config` so the next tick picks up new settings without aborting a task.
pub struct CloudRuntime {
    pub config: Option<gpbeam_core::config::CloudConfig>,
    pub delete_after_verify: bool,
}

impl CloudRuntime {
    /// An empty runtime: no cloud configured, no card deletion.
    pub fn empty() -> Self {
        CloudRuntime {
            config: None,
            delete_after_verify: false,
        }
    }
}
