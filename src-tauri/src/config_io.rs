//! Pure conversions between the UI-facing `ConfigView`/`CloudView` (serde
//! camelCase, what the Svelte settings/wizard send) and the core
//! `gpbeam_core::config::Config`. Also the atomic config writer used by the
//! `save_config` / `complete_wizard` commands. No Tauri types here — every
//! function is pure and unit-tested.
//!
//! These items are the typed UI<->Core bridge consumed by the Phase 5
//! `commands` module (`save_config`, `complete_wizard`, `get_config`); until
//! those commands land they are not yet called from non-test code, so the
//! module opts out of `dead_code` for its public API.
#![allow(dead_code)]

use std::io::Write;
use std::path::Path;

use gpbeam_core::config::{CloudConfig, CloudKind, Config, Layout, MirrorMode};

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
    pub wired_ingest: bool,
    pub cloud: Option<CloudView>,
    /// UI-only (M2): destination ids whose app-password sits in plaintext in
    /// `gpbeam.toml`. Populated by `get_config`; never parsed back into `Config`.
    #[serde(default)]
    pub plaintext_credential_ids: Vec<String>,
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
        wired_ingest: cfg.wired_ingest,
        cloud: cfg.cloud.as_ref().map(|c| cloud_to_view(c, has_password)),
        // Populated by `get_config` (needs the on-disk path); empty here.
        plaintext_credential_ids: Vec::new(),
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

/// Parse the UI mirror-mode string into a `MirrorMode`. The inverse of
/// `mirror_mode_to_str`. Anything else is a user-facing error.
fn parse_mirror_mode(s: &str) -> Result<MirrorMode, String> {
    match s {
        "off" => Ok(MirrorMode::Off),
        "auto" => Ok(MirrorMode::Auto),
        "manual" => Ok(MirrorMode::Manual),
        other => Err(format!("invalid mirror mode {other:?} (want off|auto|manual)")),
    }
}

/// True if `url` is an acceptable Nextcloud base URL: `https://` with a
/// non-empty host (any host), or `http://` **only** for a loopback host. Plain
/// http to a remote host would send the app password and all uploaded footage
/// in cleartext (the uploader uses HTTP Basic auth), so it is rejected here
/// (finding M3). Kept dependency-light (no `url` crate) — full WebDAV validation
/// happens when the uploader actually connects.
fn is_valid_base_url(url: &str) -> bool {
    let (rest, is_https) = if let Some(r) = url.strip_prefix("https://") {
        (r, true)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, false)
    } else {
        return false;
    };
    // Authority is everything up to the first '/'; it must be non-empty.
    let authority = rest.split('/').next().unwrap_or("");
    if authority.trim().is_empty() {
        return false;
    }
    is_https || is_loopback_host(authority)
}

/// True for a loopback authority — `localhost`, `127.0.0.1`, or `::1` — with or
/// without a `[...]` IPv6 wrapper and/or a trailing `:port`.
fn is_loopback_host(authority: &str) -> bool {
    // Strip any `userinfo@` prefix first: the real host is after the last '@'.
    // Otherwise `http://[::1]@evil.com` would parse as loopback but actually
    // connect to evil.com in cleartext (an M3 bypass).
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(after_bracket) = authority.strip_prefix('[') {
        // Bracketed IPv6: `[host]` optionally followed by `:port`. A missing
        // closing bracket, or any junk after `]` other than a numeric port,
        // is malformed and rejected (e.g. `[::1]extra` must not pass).
        let (ipv6, suffix) = match after_bracket.split_once(']') {
            Some(parts) => parts,
            None => return false,
        };
        let suffix_ok = suffix.is_empty()
            || (suffix.starts_with(':') && suffix[1..].chars().all(|c| c.is_ascii_digit()));
        if !suffix_ok {
            return false;
        }
        ipv6
    } else {
        // `host` / `host:port` -> strip a single trailing numeric port. A bare
        // IPv6 like `::1` has multiple colons and no port, so only strip when
        // the part before the last colon is itself colon-free.
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => authority,
        }
    };
    // Hostnames are case-insensitive (RFC 1035/1123); IPv6 hex is too.
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

/// Validate a `ConfigView` from the UI. Returns `Ok(())` when the view can be
/// turned into a `Config`, else a user-facing `Err(String)`. Called by
/// `view_to_config` and by the `save_config`/`complete_wizard` commands.
pub fn validate_view(view: &ConfigView) -> Result<(), String> {
    if view.dest_root.trim().is_empty() {
        return Err("destination folder must not be empty".to_string());
    }
    if view.filename_template.trim().is_empty() {
        return Err("filename template must not be empty".to_string());
    }
    if let Some(cloud) = &view.cloud {
        // Validates the mirror-mode string regardless of which mode it is.
        parse_mirror_mode(&cloud.mirror_mode)?;
        if cloud.destination_id.trim().is_empty() {
            return Err("cloud destination id must not be empty".to_string());
        }
        if cloud.base_url.trim().is_empty() {
            return Err("cloud base url must not be empty".to_string());
        }
        if !is_valid_base_url(&cloud.base_url) {
            return Err(format!(
                "cloud base url {:?} must be https:// with a host (http:// is allowed only \
                 for loopback hosts like localhost/127.0.0.1)",
                cloud.base_url
            ));
        }
        if cloud.remote_root.trim().is_empty() {
            return Err("cloud remote root must not be empty".to_string());
        }
        if cloud.username.trim().is_empty() {
            return Err("cloud username must not be empty".to_string());
        }
        if cloud.max_concurrency == 0 {
            return Err("cloud max concurrency must be at least 1".to_string());
        }
        if cloud.max_attempts == 0 {
            return Err("cloud max attempts must be at least 1".to_string());
        }
    }
    Ok(())
}

/// Turn a validated `ConfigView` into a core `Config`. Validates first, so an
/// `Err` mirrors `validate_view`. `layout` is always `Flat` (M3 has no other),
/// and the `kind` is always `Nextcloud` (the only `CloudKind` in M3).
pub fn view_to_config(view: &ConfigView) -> Result<Config, String> {
    validate_view(view)?;
    let cloud = match &view.cloud {
        None => None,
        Some(c) => Some(CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: c.destination_id.trim().to_string(),
            base_url: c.base_url.trim().to_string(),
            username: c.username.trim().to_string(),
            remote_root: c.remote_root.trim().to_string(),
            mirror_mode: parse_mirror_mode(&c.mirror_mode)?,
            chunk_threshold: c.chunk_threshold,
            tls_ca_pem: None,
            max_concurrency: c.max_concurrency,
            max_attempts: c.max_attempts,
        }),
    };
    Ok(Config {
        dest_root: std::path::PathBuf::from(view.dest_root.trim()),
        filename_template: view.filename_template.clone(),
        include_proxies: view.include_proxies,
        include_thumbnails: view.include_thumbnails,
        layout: Layout::Flat,
        verify: view.verify,
        space_headroom: view.space_headroom,
        cloud,
        delete_after_verify: view.delete_after_verify,
        auto_eject: view.auto_eject,
        wired_ingest: view.wired_ingest,
    })
}

/// Read the existing config file at `path` (if any) and return its top-level
/// `[credentials]` table serialized back to TOML text (e.g.
/// `"[credentials.nc1]\nusername = \"alice\"\n..."`), or `None` when the file
/// is absent, unparsable, or has no credentials table. Used to carry a
/// hand-managed `[credentials.*]` table across a GUI save, since `Config`
/// itself does not model credentials.
/// Destination ids that carry a non-empty plaintext `app_password` in the file's
/// `[credentials]` table (finding M2). Empty when the file is
/// absent/unparseable/has none. Surfaced to the UI so the Cloud tab can offer a
/// one-click migration into the OS keychain.
pub fn plaintext_credential_ids(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(doc) = toml::from_str::<toml::Value>(&raw) else { return Vec::new() };
    let Some(creds) = doc.get("credentials").and_then(|c| c.as_table()) else {
        return Vec::new();
    };
    creds
        .iter()
        .filter(|(_, v)| {
            v.get("app_password").and_then(|p| p.as_str()).is_some_and(|s| !s.is_empty())
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// The plaintext `app_password` for `id` in the file's `[credentials]` table, if
/// present. Used by the M2 migrate command to move it into the keychain.
pub fn plaintext_app_password(path: &Path, id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = toml::from_str(&raw).ok()?;
    doc.get("credentials")?
        .get(id)?
        .get("app_password")?
        .as_str()
        .map(String::from)
}

/// Remove the plaintext `app_password` from `[credentials.<id>]`, preserving the
/// (non-secret) `username` so credential resolution still has it after the secret
/// moves to the keychain (finding M2). Only the password is a liability; the
/// username is not. If removing the password leaves the entry empty (it had no
/// username), the entry is dropped; dropping the last entry drops the whole
/// `[credentials]` table. A missing file is a successful no-op. Preserves the
/// atomic/0600 write discipline.
pub fn strip_credential_password(path: &Path, id: &str) -> Result<(), String> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Ok(()) };
    let mut doc: toml::Value =
        toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if let Some(table) = doc.as_table_mut() {
        let mut table_empty = false;
        if let Some(creds) = table.get_mut("credentials").and_then(|c| c.as_table_mut()) {
            if let Some(entry) = creds.get_mut(id).and_then(|e| e.as_table_mut()) {
                entry.remove("app_password");
                if entry.is_empty() {
                    creds.remove(id);
                }
            }
            table_empty = creds.is_empty();
        }
        if table_empty {
            table.remove("credentials");
        }
    }
    let body = toml::to_string(&doc).map_err(|e| format!("serialize {}: {e}", path.display()))?;
    atomic_write_string(path, &body)
}

/// Atomically write `body` to `path`: `<path>.part` (fsync, 0600 on unix) then
/// rename over `path`. Creates the parent dir if missing. Shared by
/// [`write_config_atomic`] and [`strip_credential_entry`].
fn atomic_write_string(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create config dir {}: {e}", parent.display()))?;
        }
    }
    let part = {
        let mut p = path.as_os_str().to_os_string();
        p.push(".part");
        std::path::PathBuf::from(p)
    };
    {
        let mut f = std::fs::File::create(&part)
            .map_err(|e| format!("create {}: {e}", part.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("write {}: {e}", part.display()))?;
        f.sync_all().map_err(|e| format!("fsync {}: {e}", part.display()))?;
    }
    // Owner-only (0600) BEFORE the rename, so a secret-bearing config is never
    // even briefly group/world-readable. Unix only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set 0600 on {}: {e}", part.display()))?;
    }
    if let Err(e) = std::fs::rename(&part, path) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("rename {} -> {}: {e}", part.display(), path.display()));
    }
    Ok(())
}

fn extract_credentials_table(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = toml::from_str(&raw).ok()?;
    let creds = doc.get("credentials")?;
    // Wrap the captured value back into a single-key document so it serializes
    // as `[credentials.<id>]` tables, then hand back the text.
    let mut wrapper = toml::value::Table::new();
    wrapper.insert("credentials".to_string(), creds.clone());
    toml::to_string(&toml::Value::Table(wrapper)).ok()
}

/// Atomically persist `cfg` to `path` as TOML, preserving any pre-existing
/// top-level `[credentials]` table on disk. Writes `<path>.part`, fsyncs it,
/// then renames over `path` so a reader never sees a half-written file.
pub fn write_config_atomic(path: &Path, cfg: &Config) -> Result<(), String> {
    // Serialize the Config itself. `Config` has no `credentials` field, so this
    // never emits one — we re-attach any preserved table below.
    let mut body =
        toml::to_string(cfg).map_err(|e| format!("serialize config: {e}"))?;

    if let Some(creds) = extract_credentials_table(path) {
        // M2: a plaintext [credentials] table on disk is a liability — it can be
        // replicated off-box when dest_root is a synced/removable volume, and is
        // readable by other users without the 0600 hardening below. Preserve it
        // (don't silently drop a hand-managed fallback) but warn so the user can
        // migrate it into the OS keychain via "Set Nextcloud credentials".
        eprintln!(
            "gpbeam: warning: {} contains a plaintext [credentials] table; consider \
             migrating it into the OS keychain and deleting it from the file",
            path.display()
        );
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
        body.push_str(&creds);
    }

    // Atomic, fsync'd, 0600 write — shared with `strip_credential_entry`.
    atomic_write_string(path, &body)
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

    #[test]
    fn config_to_view_carries_wired_ingest_default_true() {
        let cfg = Config::new(PathBuf::from("/Users/alice/GPBeam"));
        let view = config_to_view(&cfg, false);
        assert!(view.wired_ingest, "Config::new default true -> view true");
    }

    #[test]
    fn config_to_view_serializes_wired_ingest_camelcase() {
        let cfg = Config::new(PathBuf::from("/d"));
        let json = serde_json::to_value(config_to_view(&cfg, false)).unwrap();
        assert!(json.get("wiredIngest").is_some(), "camelCase key wiredIngest present");
        assert_eq!(json.get("wiredIngest").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn view_to_config_maps_wired_ingest_false() {
        let mut v = base_view();
        v.wired_ingest = false;
        let cfg = view_to_config(&v).expect("valid view -> config");
        assert!(!cfg.wired_ingest, "view false -> config false");
    }

    fn base_view() -> ConfigView {
        ConfigView {
            dest_root: "/Users/alice/GPBeam".into(),
            filename_template: "{date}_{original}".into(),
            include_proxies: false,
            include_thumbnails: false,
            verify: true,
            space_headroom: 1024 * 1024 * 1024,
            delete_after_verify: false,
            auto_eject: false,
            wired_ingest: true,
            cloud: None,
            plaintext_credential_ids: Vec::new(),
        }
    }

    fn cloud_view() -> CloudView {
        CloudView {
            destination_id: "nc1".into(),
            base_url: "https://cloud.example.com".into(),
            username: "alice".into(),
            remote_root: "GoPro".into(),
            mirror_mode: "auto".into(),
            chunk_threshold: 50 * 1024 * 1024,
            max_concurrency: 2,
            max_attempts: 8,
            has_password: true,
        }
    }

    #[test]
    fn validate_view_accepts_minimal_no_cloud() {
        assert!(validate_view(&base_view()).is_ok());
    }

    #[test]
    fn validate_view_rejects_empty_dest_root() {
        let mut v = base_view();
        v.dest_root = "   ".into();
        let err = validate_view(&v).unwrap_err();
        assert!(err.to_lowercase().contains("destination"), "got: {err}");
    }

    #[test]
    fn validate_view_rejects_empty_filename_template() {
        let mut v = base_view();
        v.filename_template = String::new();
        assert!(validate_view(&v).is_err());
    }

    #[test]
    fn validate_view_rejects_bad_mirror_mode() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.mirror_mode = "sometimes".into();
        v.cloud = Some(c);
        let err = validate_view(&v).unwrap_err();
        assert!(err.to_lowercase().contains("mirror"), "got: {err}");
    }

    #[test]
    fn validate_view_rejects_auto_with_empty_base_url() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.base_url = String::new();
        v.cloud = Some(c);
        let err = validate_view(&v).unwrap_err();
        assert!(err.to_lowercase().contains("base url") || err.to_lowercase().contains("base_url"),
                "got: {err}");
    }

    #[test]
    fn base_url_https_accepts_any_host() {
        assert!(is_valid_base_url("https://cloud.example.com"));
        assert!(is_valid_base_url("https://192.168.1.10:8443/nextcloud"));
    }

    #[test]
    fn base_url_http_allowed_only_for_loopback() {
        // M3: plain http:// sends the app password + footage in cleartext, so it
        // is permitted only for loopback hosts (a self-hosted instance on the
        // same machine); every other host must use https://.
        assert!(is_valid_base_url("http://localhost:8080"));
        assert!(is_valid_base_url("http://127.0.0.1"));
        assert!(is_valid_base_url("http://[::1]:8080/nextcloud"));
        assert!(!is_valid_base_url("http://cloud.example.com"));
        assert!(!is_valid_base_url("http://192.168.1.10"));
        assert!(!is_valid_base_url("http://192.168.1.10:8080"));
    }

    #[test]
    fn http_loopback_is_case_insensitive() {
        // Hostnames are case-insensitive (RFC 1035/1123); a local Nextcloud at
        // http://LOCALHOST must validate.
        assert!(is_valid_base_url("http://LOCALHOST:8080"));
        assert!(is_valid_base_url("http://LocalHost"));
    }

    #[test]
    fn malformed_ipv6_authority_is_rejected() {
        // Only `[ipv6]` optionally followed by `:port` is a loopback; junk after
        // the bracket (or a missing closing bracket) must be rejected.
        assert!(!is_valid_base_url("http://[::1]extra:8080"));
        assert!(!is_valid_base_url("http://[::1"));
    }

    #[test]
    fn http_userinfo_cannot_spoof_loopback() {
        // M3 hardening: `userinfo@host` must not let a remote host masquerade as
        // loopback — the real host is after the last '@'. Without stripping it,
        // `http://[::1]@evil.com` would connect to evil.com in cleartext.
        assert!(!is_valid_base_url("http://[::1]@evil.com"));
        assert!(!is_valid_base_url("http://127.0.0.1@evil.com"));
        assert!(!is_valid_base_url("http://localhost@evil.com"));
        // A genuine loopback host with userinfo is still allowed.
        assert!(is_valid_base_url("http://user@127.0.0.1"));
    }

    #[test]
    fn validate_view_rejects_http_non_loopback() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.base_url = "http://cloud.example.com".into();
        v.cloud = Some(c);
        assert!(validate_view(&v).is_err());
    }

    #[test]
    fn validate_view_accepts_http_loopback() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.base_url = "http://localhost:8080".into();
        v.cloud = Some(c);
        assert!(validate_view(&v).is_ok());
    }

    #[test]
    fn validate_view_rejects_non_http_base_url() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.base_url = "ftp://cloud.example.com".into();
        v.cloud = Some(c);
        assert!(validate_view(&v).is_err());
    }

    #[test]
    fn validate_view_rejects_base_url_without_host() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.base_url = "https://".into();
        v.cloud = Some(c);
        assert!(validate_view(&v).is_err());
    }

    #[test]
    fn validate_view_rejects_empty_destination_id() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.destination_id = "  ".into();
        v.cloud = Some(c);
        assert!(validate_view(&v).is_err());
    }

    #[test]
    fn validate_view_rejects_zero_concurrency_or_attempts() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.max_concurrency = 0;
        v.cloud = Some(c);
        assert!(validate_view(&v).is_err());

        let mut v2 = base_view();
        let mut c2 = cloud_view();
        c2.max_attempts = 0;
        v2.cloud = Some(c2);
        assert!(validate_view(&v2).is_err());
    }

    #[test]
    fn view_to_config_round_trips_via_config_to_view() {
        let mut v = base_view();
        v.cloud = Some(cloud_view());
        let cfg = view_to_config(&v).expect("valid view -> config");
        // dest_root and primitives survive.
        assert_eq!(cfg.dest_root, PathBuf::from("/Users/alice/GPBeam"));
        assert_eq!(cfg.layout, Layout::Flat); // always Flat in M3
        let cloud = cfg.cloud.as_ref().expect("cloud present");
        assert_eq!(cloud.kind, CloudKind::Nextcloud);
        assert_eq!(cloud.mirror_mode, MirrorMode::Auto);
        // Round-trip back to a view; has_password is a UI hint reset by config_to_view.
        let back = config_to_view(&cfg, true);
        let mut expected = v.clone();
        // config_to_view stamps has_password from its arg, not from the original view.
        if let Some(c) = expected.cloud.as_mut() {
            c.has_password = true;
        }
        assert_eq!(back, expected);
    }

    #[test]
    fn view_to_config_maps_off_and_manual_modes() {
        let mut v = base_view();
        let mut c = cloud_view();
        c.mirror_mode = "off".into();
        v.cloud = Some(c);
        assert_eq!(view_to_config(&v).unwrap().cloud.unwrap().mirror_mode, MirrorMode::Off);

        let mut v2 = base_view();
        let mut c2 = cloud_view();
        c2.mirror_mode = "manual".into();
        v2.cloud = Some(c2);
        assert_eq!(view_to_config(&v2).unwrap().cloud.unwrap().mirror_mode, MirrorMode::Manual);
    }

    #[test]
    fn view_to_config_rejects_invalid_view() {
        let mut v = base_view();
        v.dest_root = String::new();
        assert!(view_to_config(&v).is_err());
    }

    #[test]
    fn write_config_atomic_round_trips_via_load_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let mut cfg = Config::new(dir.path().join("dest"));
        cfg.delete_after_verify = true;
        cfg.cloud = Some(CloudConfig {
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
        });

        write_config_atomic(&path, &cfg).expect("write ok");

        let loaded = gpbeam_core::config::load_config(&path).expect("load ok");
        assert_eq!(loaded.dest_root, cfg.dest_root);
        assert!(loaded.delete_after_verify);
        let lc = loaded.cloud.expect("cloud round-trips");
        assert_eq!(lc.destination_id, "nc1");
        assert_eq!(lc.base_url, "https://cloud.example.com");
        assert_eq!(lc.mirror_mode, MirrorMode::Auto);
    }

    #[cfg(unix)]
    #[test]
    fn write_config_atomic_sets_owner_only_permissions() {
        // M2: gpbeam.toml may carry a plaintext [credentials] fallback and often
        // lives under a user media folder, so it must be owner-only (0600), not
        // the default 0644 a fresh File::create would leave on a multi-user box.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let cfg = Config::new(dir.path().join("dest"));
        write_config_atomic(&path, &cfg).expect("write ok");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be owner-only, got {mode:o}");
    }

    #[test]
    fn write_config_atomic_leaves_no_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let cfg = Config::new(dir.path().join("dest"));
        write_config_atomic(&path, &cfg).expect("write ok");
        let part = dir.path().join("gpbeam.toml.part");
        assert!(!part.exists(), "temp .part file must be cleaned up");
        assert!(path.exists(), "final config must exist");
    }

    #[test]
    fn write_config_atomic_preserves_existing_credentials_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        // Seed a file that already carries a hand-managed [credentials.nc1] table.
        let seed = r#"
            dest_root = "/old/dest"
            filename_template = "{date}_{original}"
            include_proxies = false
            include_thumbnails = false
            layout = "Flat"
            verify = true
            space_headroom = 1073741824

            [credentials.nc1]
            username = "alice"
            app_password = "s3cret-app-pw"
        "#;
        std::fs::write(&path, seed).unwrap();

        // GUI save with new settings (different dest, cloud added).
        let mut cfg = Config::new(std::path::PathBuf::from("/new/dest"));
        cfg.cloud = Some(CloudConfig {
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
        });
        write_config_atomic(&path, &cfg).expect("write ok");

        // The new config is loadable...
        let loaded = gpbeam_core::config::load_config(&path).expect("load ok");
        assert_eq!(loaded.dest_root, std::path::PathBuf::from("/new/dest"));
        assert!(loaded.cloud.is_some());

        // ...and the credentials table survived verbatim (parse the raw TOML).
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&raw).unwrap();
        let creds = doc
            .get("credentials")
            .and_then(|c| c.get("nc1"))
            .expect("[credentials.nc1] preserved");
        assert_eq!(creds.get("username").and_then(|v| v.as_str()), Some("alice"));
        assert_eq!(
            creds.get("app_password").and_then(|v| v.as_str()),
            Some("s3cret-app-pw")
        );
    }

    #[test]
    fn write_config_atomic_no_credentials_when_none_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        let cfg = Config::new(dir.path().join("dest"));
        write_config_atomic(&path, &cfg).expect("write ok");
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&raw).unwrap();
        assert!(doc.get("credentials").is_none(), "no credentials table fabricated");
    }

    // ---- M2: plaintext credential detection / strip / migrate helpers ----

    #[test]
    fn plaintext_credential_ids_lists_entries_with_app_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path,
            "dest_root = \"/d\"\n\
             [credentials.nc1]\nusername=\"a\"\napp_password=\"pw\"\n\
             [credentials.nc2]\nusername=\"b\"\napp_password=\"\"\n").unwrap();
        let ids = plaintext_credential_ids(&path);
        assert_eq!(ids, vec!["nc1".to_string()]); // nc2 has an empty password
    }

    #[test]
    fn plaintext_credential_ids_empty_when_absent_or_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path, "dest_root = \"/d\"\n").unwrap();
        assert!(plaintext_credential_ids(&path).is_empty());
        assert!(plaintext_credential_ids(&dir.path().join("nope.toml")).is_empty());
    }

    #[test]
    fn strip_credential_password_keeps_username_drops_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"alice\"\napp_password=\"pw1\"\n").unwrap();
        strip_credential_password(&path, "nc1").unwrap();
        // No longer flagged (no plaintext password) but the username is preserved.
        assert!(plaintext_credential_ids(&path).is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("alice"), "username preserved for resolution");
        assert!(!raw.contains("pw1"), "plaintext password removed");
        assert!(raw.contains("dest_root"), "other config survives");
    }

    #[test]
    fn strip_credential_password_leaves_other_entries_and_keeps_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path,
            "dest_root = \"/d\"\n\
             [credentials.nc1]\nusername=\"a\"\napp_password=\"pw1\"\n\
             [credentials.nc2]\nusername=\"b\"\napp_password=\"pw2\"\n").unwrap();
        strip_credential_password(&path, "nc1").unwrap();
        assert_eq!(plaintext_credential_ids(&path), vec!["nc2".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn strip_credential_password_drops_empty_entry_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        // An entry with only a password (no username) becomes empty -> dropped,
        // and the now-empty [credentials] table is dropped too.
        std::fs::write(&path,
            "dest_root = \"/d\"\n[credentials.nc1]\napp_password=\"pw\"\n").unwrap();
        strip_credential_password(&path, "nc1").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("credentials"), "empty entry and table dropped");
    }

    #[test]
    fn plaintext_app_password_reads_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"a\"\napp_password=\"sekret\"\n").unwrap();
        assert_eq!(plaintext_app_password(&path, "nc1").as_deref(), Some("sekret"));
        assert_eq!(plaintext_app_password(&path, "nope"), None);
    }

    #[test]
    fn config_view_serializes_plaintext_ids_camelcase() {
        let cfg = Config::new(PathBuf::from("/d"));
        let mut view = config_to_view(&cfg, false);
        view.plaintext_credential_ids = vec!["nc1".into()];
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["plaintextCredentialIds"][0], "nc1");
    }
}
