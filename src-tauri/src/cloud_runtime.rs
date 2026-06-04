//! The swappable cloud-mirror configuration the long-lived upload loop reads
//! on every tick, plus the two pure helpers that keep `lib.rs` test-free:
//! `should_drain` (the loop's only real decision) and `now_unix` (the shared
//! wall-clock source for event timestamps and ETA math).

use std::time::{SystemTime, UNIX_EPOCH};

use gpbeam_core::config::{CloudConfig, MirrorMode};

/// What the cloud loop needs to decide whether (and how) to drain. `save_config`
/// swaps `config` in place so the next tick picks up new settings without aborting
/// any task. `None` config = cloud not configured = loop idles.
#[derive(Clone, Default)]
pub struct CloudRuntime {
    pub config: Option<CloudConfig>,
    pub delete_after_verify: bool,
}

impl CloudRuntime {
    /// Build a runtime from a freshly-loaded `Config`. `None` cloud table yields
    /// an idle runtime (`config: None`).
    pub fn from_config(cfg: &gpbeam_core::config::Config) -> Self {
        CloudRuntime {
            config: cfg.cloud.clone(),
            delete_after_verify: cfg.delete_after_verify,
        }
    }
}

/// Pure decision the cloud loop delegates to each tick: drain only when not
/// paused AND a cloud destination is configured AND its mirror mode is not `Off`.
/// (`Auto` and `Manual` both drain; the worker itself honors per-job state.)
pub fn should_drain(paused: bool, runtime: &CloudRuntime) -> bool {
    if paused {
        return false;
    }
    match &runtime.config {
        Some(c) => c.mirror_mode != MirrorMode::Off,
        None => false,
    }
}

/// Seconds since the Unix epoch as an `i64`. Used for `apply_run_event`'s
/// `now_unix` argument and ETA math. Before-epoch clocks (never, in practice)
/// clamp to 0 rather than panic.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpbeam_core::config::{CloudConfig, CloudKind, MirrorMode};

    fn cloud_with(mode: MirrorMode) -> CloudConfig {
        CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "nc1".into(),
            base_url: "https://example.com".into(),
            username: "alice".into(),
            remote_root: "/GPBeam".into(),
            mirror_mode: mode,
            chunk_threshold: 10_000_000,
            tls_ca_pem: None,
            max_concurrency: 2,
            max_attempts: 3,
        }
    }

    #[test]
    fn no_config_never_drains() {
        let rt = CloudRuntime::default();
        assert!(!should_drain(false, &rt));
        assert!(!should_drain(true, &rt));
    }

    #[test]
    fn off_mode_never_drains() {
        let rt = CloudRuntime {
            config: Some(cloud_with(MirrorMode::Off)),
            delete_after_verify: false,
        };
        assert!(!should_drain(false, &rt));
    }

    #[test]
    fn auto_mode_drains_when_unpaused() {
        let rt = CloudRuntime {
            config: Some(cloud_with(MirrorMode::Auto)),
            delete_after_verify: true,
        };
        assert!(should_drain(false, &rt));
    }

    #[test]
    fn manual_mode_drains_when_unpaused() {
        let rt = CloudRuntime {
            config: Some(cloud_with(MirrorMode::Manual)),
            delete_after_verify: false,
        };
        assert!(should_drain(false, &rt));
    }

    #[test]
    fn paused_blocks_drain_even_when_configured() {
        let rt = CloudRuntime {
            config: Some(cloud_with(MirrorMode::Auto)),
            delete_after_verify: false,
        };
        assert!(!should_drain(true, &rt));
    }

    #[test]
    fn now_unix_is_after_2020() {
        // 2020-01-01T00:00:00Z = 1_577_836_800; any sane clock is well past it.
        assert!(now_unix() > 1_577_836_800);
    }

    #[test]
    fn from_config_carries_cloud_and_delete_flag() {
        let mut cfg = gpbeam_core::config::Config::new(std::path::PathBuf::from("/tmp/x"));
        cfg.cloud = Some(cloud_with(MirrorMode::Auto));
        cfg.delete_after_verify = true;
        let rt = CloudRuntime::from_config(&cfg);
        assert!(rt.config.is_some());
        assert!(rt.delete_after_verify);
    }
}
