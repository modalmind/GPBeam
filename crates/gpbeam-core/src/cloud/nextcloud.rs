//! Nextcloud WebDAV uploader. Built incrementally across Phase 2.

use crate::config::CloudConfig;
use crate::credentials::Secret;
use crate::error::{CoreError, Result};
use async_trait::async_trait;
use reqwest::{Client, Method};
use std::path::Path;

use crate::cloud::{CloudUploader, ResumeState, UploadOutcome};

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getcontentlength/>
    <d:getetag/>
    <d:resourcetype/>
  </d:prop>
</d:propfind>"#;

/// Extract the first `<d:getcontentlength>` value from a PROPFIND 207 body.
/// Namespace-agnostic: matches any element whose local name is `getcontentlength`.
pub(crate) fn parse_first_contentlength(xml: &str) -> Option<u64> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_len = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name_eq(e.name().as_ref(), b"getcontentlength") {
                    in_len = true;
                }
            }
            Ok(Event::Text(t)) if in_len => {
                if let Ok(s) = t.unescape() {
                    if let Ok(n) = s.trim().parse::<u64>() {
                        return Some(n);
                    }
                }
                in_len = false;
            }
            Ok(Event::End(e)) => {
                if local_name_eq(e.name().as_ref(), b"getcontentlength") {
                    in_len = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Compare an XML qualified name's local part (after any `:`) to `local`.
fn local_name_eq(qname: &[u8], local: &[u8]) -> bool {
    let tail = match qname.iter().rposition(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    };
    tail == local
}

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

impl NextcloudUploader {
    /// PROPFIND Depth:0 the file URL. 207 with a content-length present => exists;
    /// 404 => missing; 401 => CloudAuth; other => Http.
    pub(crate) async fn propfind_present(&self, remote: &str, _size: u64) -> Result<bool> {
        let rel = self.remote_rel(remote);
        let url = files_url(&self.base_url, &self.username, &rel);
        let method = Method::from_bytes(b"PROPFIND").expect("valid method");
        let resp = self
            .client
            .request(method, &url)
            .basic_auth(&self.username, Some(&self.app_password))
            .header("Depth", "0")
            .header(reqwest::header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(PROPFIND_BODY)
            .send()
            .await
            .map_err(transport_err)?;

        match resp.status().as_u16() {
            207 => {
                let body = resp.text().await.map_err(transport_err)?;
                Ok(parse_first_contentlength(&body).is_some())
            }
            404 => Ok(false),
            401 => Err(CoreError::CloudAuth(
                "PROPFIND rejected (401); generate a Nextcloud app password".into(),
            )),
            s => Err(CoreError::Http {
                status: Some(s),
                msg: format!("PROPFIND {url} -> {s}"),
            }),
        }
    }
}

/// Map a reqwest transport error to a retryable `Http { status: None, .. }`.
pub(crate) fn transport_err(e: reqwest::Error) -> CoreError {
    CoreError::Http { status: None, msg: e.to_string() }
}

#[async_trait]
impl CloudUploader for NextcloudUploader {
    async fn already_present(&self, remote: &str, size: u64) -> Result<bool> {
        self.propfind_present(remote, size).await
    }

    async fn upload(
        &self,
        _local: &Path,
        _remote: &str,
        _total: u64,
        _resume: Option<ResumeState>,
        _progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<UploadOutcome> {
        // Implemented in Task 2.9 (dispatcher) atop put_simple/put_chunked.
        Err(CoreError::Config("upload not yet implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudConfig, CloudKind, MirrorMode};
    use crate::credentials::Secret;
    use std::path::PathBuf;
    use wiremock::matchers::{header, method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn cfg_for(base_url: &str) -> CloudConfig {
        let mut c = test_cfg(None);
        c.base_url = base_url.to_string();
        c.remote_root = "GoPro".into();
        c
    }

    #[tokio::test]
    async fn already_present_false_on_404() {
        let server = MockServer::start().await;
        Mock::given(wm_method("PROPFIND"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/clip.mp4"))
            .and(header("Depth", "0"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        assert!(!up.already_present("clip.mp4", 1024).await.unwrap());
    }

    #[tokio::test]
    async fn already_present_true_on_207_with_size() {
        let server = MockServer::start().await;
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/files/alice/GoPro/clip.mp4</d:href>
    <d:propstat>
      <d:prop><d:getcontentlength>2048</d:getcontentlength></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        Mock::given(wm_method("PROPFIND"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/clip.mp4"))
            .respond_with(ResponseTemplate::new(207).set_body_raw(xml, "application/xml"))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        assert!(up.already_present("clip.mp4", 2048).await.unwrap());
    }

    #[test]
    fn parse_contentlength_extracts_size() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response>
            <d:prop><d:getcontentlength>4096</d:getcontentlength></d:prop>
        </d:response></d:multistatus>"#;
        assert_eq!(parse_first_contentlength(xml), Some(4096));
        assert_eq!(parse_first_contentlength("<empty/>"), None);
    }
}
