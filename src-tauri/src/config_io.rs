//! Pure conversions between the UI-facing `ConfigView`/`CloudView` (serde
//! camelCase, what the Svelte settings/wizard send) and the core
//! `gpbeam_core::config::Config`. Also the atomic config writer used by the
//! `save_config` / `complete_wizard` commands. No Tauri types here — every
//! function is pure and unit-tested.

use std::path::Path;

use gpbeam_core::config::{CloudConfig, CloudKind, Config, MirrorMode};

/// UI view of a `[cloud]` table. `has_password` is a UI-only hint (true when a
/// credential exists in env/keychain/fallback) and is NOT persisted to TOML.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CloudView {
    pub destination_id: String,
    pub base_url: String,
    pub username: String,
    pub remote_root: String,
    pub mirror_mode: String, // "off" | "auto" | "manual"
    pub chunk_threshold: u64,
    pub max_concurrency: usize,
    pub max_attempts: u32,
    pub has_password: bool,
}

/// UI view of the whole `Config`. `layout` is omitted (only `Flat` exists in M3).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    pub dest_root: String,
    pub filename_template: String,
    pub include_proxies: bool,
    pub include_thumbnails: bool,
    pub verify: bool,
    pub space_headroom: u64,
    pub delete_after_verify: bool,
    pub auto_eject: bool,
    pub cloud: Option<CloudView>,
}

/// Map a `MirrorMode` to its lowercase serde string.
fn mirror_mode_to_str(m: MirrorMode) -> &'static str {
    match m {
        MirrorMode::Off => "off",
        MirrorMode::Auto => "auto",
        MirrorMode::Manual => "manual",
    }
}

/// Build the UI view from a core `Config`. `has_password` is supplied by the
/// caller (Phase 5 reads it from the keyring credential store) and is copied
/// onto the resulting `CloudView` when a cloud table is present.
pub fn config_to_view(cfg: &Config, has_password: bool) -> ConfigView {
    ConfigView {
        dest_root: cfg.dest_root.to_string_lossy().into_owned(),
        filename_template: cfg.filename_template.clone(),
        include_proxies: cfg.include_proxies,
        include_thumbnails: cfg.include_thumbnails,
        verify: cfg.verify,
        space_headroom: cfg.space_headroom,
        delete_after_verify: cfg.delete_after_verify,
        auto_eject: cfg.auto_eject,
        cloud: cfg.cloud.as_ref().map(|c| cloud_to_view(c, has_password)),
    }
}

fn cloud_to_view(c: &CloudConfig, has_password: bool) -> CloudView {
    CloudView {
        destination_id: c.destination_id.clone(),
        base_url: c.base_url.clone(),
        username: c.username.clone(),
        remote_root: c.remote_root.clone(),
        mirror_mode: mirror_mode_to_str(c.mirror_mode).to_string(),
        chunk_threshold: c.chunk_threshold,
        max_concurrency: c.max_concurrency,
        max_attempts: c.max_attempts,
        has_password,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_cloud() -> CloudConfig {
        CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "nc1".into(),
            base_url: "https://cloud.example.com".into(),
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
    fn config_to_view_without_cloud() {
        let cfg = Config::new(PathBuf::from("/Users/alice/GPBeam"));
        let view = config_to_view(&cfg, false);
        assert_eq!(view.dest_root, "/Users/alice/GPBeam");
        assert_eq!(view.filename_template, "{date}_{original}");
        assert!(!view.include_proxies);
        assert!(!view.include_thumbnails);
        assert!(view.verify);
        assert_eq!(view.space_headroom, 1024 * 1024 * 1024);
        assert!(!view.delete_after_verify);
        assert!(!view.auto_eject);
        assert!(view.cloud.is_none());
    }

    #[test]
    fn config_to_view_with_cloud_maps_mirror_mode_and_has_password() {
        let mut cfg = Config::new(PathBuf::from("/Users/alice/GPBeam"));
        cfg.cloud = Some(sample_cloud());
        let view = config_to_view(&cfg, true);
        let cloud = view.cloud.expect("cloud view present");
        assert_eq!(cloud.destination_id, "nc1");
        assert_eq!(cloud.base_url, "https://cloud.example.com");
        assert_eq!(cloud.username, "alice");
        assert_eq!(cloud.remote_root, "GoPro");
        assert_eq!(cloud.mirror_mode, "auto");
        assert_eq!(cloud.chunk_threshold, 50 * 1024 * 1024);
        assert_eq!(cloud.max_concurrency, 2);
        assert_eq!(cloud.max_attempts, 8);
        assert!(cloud.has_password);
    }

    #[test]
    fn config_to_view_serializes_camelcase() {
        let mut cfg = Config::new(PathBuf::from("/d"));
        cfg.cloud = Some(sample_cloud());
        let json = serde_json::to_value(config_to_view(&cfg, false)).unwrap();
        // Top-level camelCase keys.
        assert!(json.get("destRoot").is_some());
        assert!(json.get("filenameTemplate").is_some());
        assert!(json.get("deleteAfterVerify").is_some());
        // Nested cloud camelCase keys.
        let cloud = json.get("cloud").unwrap();
        assert!(cloud.get("destinationId").is_some());
        assert!(cloud.get("baseUrl").is_some());
        assert!(cloud.get("mirrorMode").is_some());
        assert!(cloud.get("maxConcurrency").is_some());
        assert!(cloud.get("hasPassword").is_some());
    }
}
