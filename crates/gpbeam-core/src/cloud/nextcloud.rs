//! Nextcloud WebDAV uploader. Built incrementally across Phase 2.

use crate::config::CloudConfig;
use crate::credentials::Secret;
use crate::error::{CoreError, Result};
use reqwest::Client;

/// Percent-encode each path segment, preserving the `/` separators. Encodes
/// spaces, `#`, `?`, `+`, and other reserved/unsafe bytes per segment.
pub fn encode_path_segments(rel: &str) -> String {
    rel.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// RFC 3986 unreserved set is kept verbatim; everything else is %XX-encoded.
fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// `<base>/remote.php/dav/files/<user>/<encoded rel>`.
pub fn files_url(base_url: &str, username: &str, remote_rel: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let enc = encode_path_segments(remote_rel);
    format!("{base}/remote.php/dav/files/{username}/{enc}")
}

/// `<base>/remote.php/dav/uploads/<user>/<upload_id>[/<part>]`.
pub fn uploads_url(base_url: &str, username: &str, upload_id: &str, part: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    match part {
        Some(p) => format!("{base}/remote.php/dav/uploads/{username}/{upload_id}/{p}"),
        None => format!("{base}/remote.php/dav/uploads/{username}/{upload_id}"),
    }
}

/// WebDAV uploader for a single Nextcloud destination.
///
/// `client` and `app_password` are wired here but first consumed by the
/// authenticated WebDAV requests added in Phase 2 Tasks 2.4–2.9; the
/// `allow(dead_code)` keeps the incremental build warning-clean until then.
#[derive(Debug)]
#[allow(dead_code)]
pub struct NextcloudUploader {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    pub(crate) username: String,
    pub(crate) app_password: String,
    pub(crate) remote_root: String,
    pub(crate) chunk_threshold: u64,
}

impl NextcloudUploader {
    /// Build the uploader and its rustls-backed reqwest client. When
    /// `cfg.tls_ca_pem` is set, the PEM is read and merged with the system
    /// trust roots (`tls_certs_merge`).
    pub fn new(cfg: &CloudConfig, secret: &Secret) -> Result<Self> {
        let mut builder = Client::builder();

        if let Some(ca_path) = &cfg.tls_ca_pem {
            let pem = std::fs::read(ca_path).map_err(|e| {
                CoreError::Config(format!(
                    "failed to read tls_ca_pem {}: {e}",
                    ca_path.display()
                ))
            })?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| {
                CoreError::Config(format!(
                    "invalid CA PEM at {}: {e}",
                    ca_path.display()
                ))
            })?;
            builder = builder.tls_certs_merge(std::iter::once(cert));
        }

        let client = builder
            .build()
            .map_err(|e| CoreError::Config(format!("failed to build reqwest client: {e}")))?;

        Ok(NextcloudUploader {
            client,
            base_url: cfg.base_url.clone(),
            username: secret.username.clone(),
            app_password: secret.app_password.clone(),
            remote_root: cfg.remote_root.clone(),
            chunk_threshold: cfg.chunk_threshold,
        })
    }

    /// Join the configured `remote_root` with a per-file relative path.
    /// Consumed by the request builders in Phase 2 Tasks 2.4–2.9.
    #[allow(dead_code)]
    pub(crate) fn remote_rel(&self, remote: &str) -> String {
        let root = self.remote_root.trim_matches('/');
        let file = remote.trim_start_matches('/');
        if root.is_empty() {
            file.to_string()
        } else {
            format!("{root}/{file}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudConfig, CloudKind, MirrorMode};
    use crate::credentials::Secret;
    use std::path::PathBuf;

    fn test_cfg(tls_ca_pem: Option<PathBuf>) -> CloudConfig {
        CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "home-nc".into(),
            base_url: "https://cloud.example.com".into(),
            username: "alice".into(),
            remote_root: "GoPro".into(),
            mirror_mode: MirrorMode::Auto,
            chunk_threshold: 50 * 1024 * 1024,
            tls_ca_pem,
            max_concurrency: 2,
            max_attempts: 8,
        }
    }

    fn test_secret() -> Secret {
        Secret { username: "alice".into(), app_password: "app-pw-1234".into() }
    }

    #[test]
    fn new_ok_without_ca() {
        let up = NextcloudUploader::new(&test_cfg(None), &test_secret()).unwrap();
        assert_eq!(up.base_url, "https://cloud.example.com");
        assert_eq!(up.username, "alice");
        assert_eq!(up.chunk_threshold, 50 * 1024 * 1024);
    }

    #[test]
    fn new_err_on_bogus_ca_path() {
        let cfg = test_cfg(Some(PathBuf::from("/nope/does-not-exist.pem")));
        let err = NextcloudUploader::new(&cfg, &test_secret()).unwrap_err();
        assert!(matches!(err, CoreError::Config(_)), "got {err:?}");
    }

    #[test]
    fn remote_rel_joins_root() {
        let up = NextcloudUploader::new(&test_cfg(None), &test_secret()).unwrap();
        assert_eq!(up.remote_rel("clip.mp4"), "GoPro/clip.mp4");
        assert_eq!(up.remote_rel("/clip.mp4"), "GoPro/clip.mp4");
    }

    #[test]
    fn encodes_spaces_and_hash_per_segment_keeps_slash() {
        assert_eq!(
            encode_path_segments("GoPro Clips/my #1 video.mp4"),
            "GoPro%20Clips/my%20%231%20video.mp4"
        );
    }

    #[test]
    fn keeps_unreserved_bytes_verbatim() {
        assert_eq!(encode_path_segments("a-b_c.d~e/f"), "a-b_c.d~e/f");
    }

    #[test]
    fn encodes_plus_and_question_mark() {
        assert_eq!(encode_path_segments("a+b?c"), "a%2Bb%3Fc");
    }

    #[test]
    fn files_url_shape_matches_contract() {
        assert_eq!(
            files_url("https://cloud.example.com", "alice", "GoPro/clip 1.mp4"),
            "https://cloud.example.com/remote.php/dav/files/alice/GoPro/clip%201.mp4"
        );
    }

    #[test]
    fn files_url_trims_trailing_slash_on_base() {
        assert_eq!(
            files_url("https://cloud.example.com/", "alice", "x.mp4"),
            "https://cloud.example.com/remote.php/dav/files/alice/x.mp4"
        );
    }

    #[test]
    fn uploads_url_dir_and_part_shapes() {
        assert_eq!(
            uploads_url("https://c.example.com", "bob", "gpbeam-123", None),
            "https://c.example.com/remote.php/dav/uploads/bob/gpbeam-123"
        );
        assert_eq!(
            uploads_url("https://c.example.com", "bob", "gpbeam-123", Some("00001")),
            "https://c.example.com/remote.php/dav/uploads/bob/gpbeam-123/00001"
        );
    }
}
