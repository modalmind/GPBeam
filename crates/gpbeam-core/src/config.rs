use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}
