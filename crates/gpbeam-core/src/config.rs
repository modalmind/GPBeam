use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    Flat,
}

/// Which cloud backend a destination targets. Google Drive is added in M2b.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudKind {
    Nextcloud,
}

/// How aggressively the cloud mirror runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MirrorMode {
    Off,
    Auto,
    Manual,
}

fn default_mirror_mode() -> MirrorMode {
    MirrorMode::Off
}
fn default_chunk_threshold() -> u64 {
    50 * 1024 * 1024
}
fn default_max_concurrency() -> usize {
    2
}
fn default_max_attempts() -> u32 {
    8
}

/// A single cloud destination, parsed from the `[cloud]` table of `gpbeam.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudConfig {
    pub kind: CloudKind,
    pub destination_id: String,
    pub base_url: String,
    pub username: String,
    pub remote_root: String,
    #[serde(default = "default_mirror_mode")]
    pub mirror_mode: MirrorMode,
    #[serde(default = "default_chunk_threshold")]
    pub chunk_threshold: u64,
    #[serde(default)]
    pub tls_ca_pem: Option<PathBuf>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub dest_root: PathBuf,
    pub filename_template: String,
    pub include_proxies: bool,
    pub include_thumbnails: bool,
    pub layout: Layout,
    pub verify: bool,
    pub space_headroom: u64,
    #[serde(default)]
    pub cloud: Option<CloudConfig>,
    #[serde(default)]
    pub delete_after_verify: bool,
    #[serde(default)]
    pub auto_eject: bool,
}

impl Config {
    pub fn new(dest_root: PathBuf) -> Self {
        Config {
            dest_root,
            filename_template: "{date}_{original}".into(),
            include_proxies: false,
            include_thumbnails: false,
            layout: Layout::Flat,
            verify: true,
            space_headroom: 1024 * 1024 * 1024, // 1 GiB
            cloud: None,
            delete_after_verify: false,
            auto_eject: false,
        }
    }
}

/// Read and parse a `gpbeam.toml` configuration file.
///
/// IO failures map to [`CoreError::Io`]; toml/serde parse failures map to
/// [`CoreError::Config`].
pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).map_err(crate::error::io_at(path))?;
    toml::from_str::<Config>(&text).map_err(|e| CoreError::Config(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::new(PathBuf::from("/tmp/dest"));
        assert_eq!(c.filename_template, "{date}_{original}");
        assert!(!c.include_proxies);
        assert!(!c.include_thumbnails);
        assert_eq!(c.layout, Layout::Flat);
        assert!(c.verify);
        assert_eq!(c.space_headroom, 1024 * 1024 * 1024);
    }

    #[test]
    fn new_sets_cloud_and_safety_defaults() {
        let c = Config::new(PathBuf::from("/tmp/dest"));
        assert!(c.cloud.is_none());
        assert!(!c.delete_after_verify);
        assert!(!c.auto_eject);
    }

    #[test]
    fn cloud_config_serde_defaults() {
        // Minimal CloudConfig table: only required fields present.
        let toml_str = r#"
            kind = "nextcloud"
            destination_id = "nc1"
            base_url = "https://cloud.example.com"
            username = "alice"
            remote_root = "GoPro"
        "#;
        let cc: CloudConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cc.kind, CloudKind::Nextcloud);
        assert_eq!(cc.destination_id, "nc1");
        assert_eq!(cc.mirror_mode, MirrorMode::Off);
        assert_eq!(cc.chunk_threshold, 50 * 1024 * 1024);
        assert!(cc.tls_ca_pem.is_none());
        assert_eq!(cc.max_concurrency, 2);
        assert_eq!(cc.max_attempts, 8);
    }

    #[test]
    fn mirror_mode_and_kind_lowercase_serde() {
        // NB: a TOML document root must be a table, so a bare scalar string is
        // not a valid TOML document. Deserialize the enums through a one-field
        // table wrapper; this exercises the same `rename_all = "lowercase"`
        // serde path the plan intended to assert.
        #[derive(Deserialize)]
        struct M {
            v: MirrorMode,
        }
        #[derive(Deserialize)]
        struct K {
            v: CloudKind,
        }
        assert_eq!(toml::from_str::<M>("v = \"auto\"").unwrap().v, MirrorMode::Auto);
        assert_eq!(toml::from_str::<M>("v = \"manual\"").unwrap().v, MirrorMode::Manual);
        assert_eq!(toml::from_str::<M>("v = \"off\"").unwrap().v, MirrorMode::Off);
        assert_eq!(
            toml::from_str::<K>("v = \"nextcloud\"").unwrap().v,
            CloudKind::Nextcloud
        );
    }

    #[test]
    fn load_config_parses_full_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let sample = r#"
            dest_root = "/Users/alice/GPBeam"
            filename_template = "{date}_{original}"
            include_proxies = false
            include_thumbnails = false
            layout = "Flat"
            verify = true
            space_headroom = 1073741824
            delete_after_verify = true
            auto_eject = false

            [cloud]
            kind = "nextcloud"
            destination_id = "nc1"
            base_url = "https://cloud.example.com"
            username = "alice"
            remote_root = "GoPro"
            mirror_mode = "auto"
        "#;
        std::fs::write(&path, sample).unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.dest_root, PathBuf::from("/Users/alice/GPBeam"));
        assert!(cfg.delete_after_verify);
        assert!(!cfg.auto_eject);

        let cloud = cfg.cloud.expect("cloud table present");
        assert_eq!(cloud.kind, CloudKind::Nextcloud);
        assert_eq!(cloud.destination_id, "nc1");
        assert_eq!(cloud.base_url, "https://cloud.example.com");
        assert_eq!(cloud.username, "alice");
        assert_eq!(cloud.remote_root, "GoPro");
        assert_eq!(cloud.mirror_mode, MirrorMode::Auto);
        // chunk_threshold omitted in the sample -> defaults to 50 MiB.
        assert_eq!(cloud.chunk_threshold, 50 * 1024 * 1024);
        assert_eq!(cloud.max_concurrency, 2);
        assert_eq!(cloud.max_attempts, 8);
    }

    #[test]
    fn load_config_invalid_toml_maps_to_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is = = not valid toml").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(matches!(err, crate::error::CoreError::Config(_)));
    }

    #[test]
    fn load_config_missing_file_maps_to_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let err = load_config(&path).unwrap_err();
        assert!(matches!(err, crate::error::CoreError::Io { .. }));
    }
}
