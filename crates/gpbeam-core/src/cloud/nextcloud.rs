//! Nextcloud WebDAV uploader. Built incrementally across Phase 2.

use crate::config::CloudConfig;
use crate::credentials::Secret;
use crate::error::{CoreError, Result};
use async_trait::async_trait;
use reqwest::{Body, Client, Method};
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

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

/// Parse a chunk-dir PROPFIND Depth:1 207 body into `{part_number -> stored bytes}`.
/// Matches `<d:response>` blocks whose href tail is a zero-padded chunk name.
pub(crate) fn parse_chunk_listing(xml: &str) -> std::collections::HashMap<u32, u64> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut map = std::collections::HashMap::new();
    let mut buf = Vec::new();

    let mut cur_part: Option<u32> = None;
    let mut cur_len: Option<u64> = None;
    let mut in_href = false;
    let mut in_len = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                if local_name_eq(name.as_ref(), b"response") {
                    cur_part = None;
                    cur_len = None;
                } else if local_name_eq(name.as_ref(), b"href") {
                    in_href = true;
                } else if local_name_eq(name.as_ref(), b"getcontentlength") {
                    in_len = true;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    if in_href {
                        let tail = s.trim().trim_end_matches('/');
                        let tail = tail.rsplit('/').next().unwrap_or("");
                        if tail.len() == 5 {
                            if let Ok(n) = tail.parse::<u32>() {
                                cur_part = Some(n);
                            }
                        }
                    } else if in_len {
                        cur_len = s.trim().parse::<u64>().ok();
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if local_name_eq(name.as_ref(), b"href") {
                    in_href = false;
                } else if local_name_eq(name.as_ref(), b"getcontentlength") {
                    in_len = false;
                } else if local_name_eq(name.as_ref(), b"response") {
                    if let (Some(p), Some(l)) = (cur_part, cur_len) {
                        map.insert(p, l);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    map
}

/// Lowercase-hex md5 of a file's contents, for the `OC-Checksum` metadata header.
///
/// Streams the file in bounded chunks rather than reading it whole — the
/// chunked-upload path (M1) computes this over multi-GB GoPro clips, which must
/// not be slurped into memory.
pub(crate) fn md5_hex_of(path: &Path) -> Result<String> {
    use md5::{Digest, Md5};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| CoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| CoreError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// File mtime as Unix seconds (positive int), if available.
#[allow(dead_code)]
pub(crate) fn mtime_secs(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    let secs = dur.as_secs() as i64;
    if secs > 0 {
        Some(secs)
    } else {
        None
    }
}

/// Read the `OC-ETag` (or `ETag`) header from a response.
#[allow(dead_code)]
pub(crate) fn read_etag(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("oc-etag")
        .or_else(|| resp.headers().get("etag"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
}

/// Chunk upper bound used to size parts (5 MiB), and the hard cap of 10000 parts.
pub(crate) const CHUNK_SIZE: u64 = 5 * 1024 * 1024;
pub(crate) const MAX_CHUNKS: u32 = 10_000;

/// Zero-padded width-5 chunk name so lexical sort == numeric sort (00001..10000).
pub(crate) fn chunk_name(n: u32) -> String {
    format!("{n:05}")
}

/// Pick a chunk size so the file fits in <= MAX_CHUNKS parts (>= CHUNK_SIZE floor).
pub(crate) fn pick_chunk_size(total: u64) -> u64 {
    let mut size = CHUNK_SIZE;
    while total.div_ceil(size) > MAX_CHUNKS as u64 {
        size *= 2;
    }
    size
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

impl Drop for NextcloudUploader {
    fn drop(&mut self) {
        // L3: wipe the long-lived app-password copy from memory on drop. (The
        // field stays a plain String so the six `basic_auth` call sites are
        // unchanged; reqwest's Client holds no other copy of it.)
        use zeroize::Zeroize;
        self.app_password.zeroize();
    }
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

/// PUT helpers. `put_simple`/`send_put` are wired into the public `upload`
/// dispatcher in Task 2.9; `allow(dead_code)` keeps the incremental build
/// warning-clean until then.
#[allow(dead_code)]
impl NextcloudUploader {
    /// PUT a whole file in one streaming request. Sends `X-OC-Mtime`,
    /// `OC-Checksum: md5:<hex>`, and `X-NC-WebDAV-AutoMkcol: 1`. Treats 201/204
    /// as success, reports `progress(total)`, and returns the server ETag.
    pub(crate) async fn put_simple(
        &self,
        local: &Path,
        remote_rel: &str,
        total: u64,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<UploadOutcome> {
        let url = files_url(&self.base_url, &self.username, remote_rel);
        let resp = self.send_put(local, &url, total).await?;
        let status = resp.status().as_u16();
        if status == 201 || status == 204 {
            progress(total);
            return Ok(UploadOutcome {
                remote_ref: remote_rel.to_string(),
                bytes: total,
                etag: read_etag(&resp),
            });
        }
        if status == 409 || status == 404 {
            // Parent collection(s) missing on a server without AutoMkcol — create
            // them top-down and retry the PUT exactly once.
            self.mkcol_parents(remote_rel).await?;
            let resp2 = self.send_put(local, &url, total).await?;
            return match resp2.status().as_u16() {
                201 | 204 => {
                    progress(total);
                    Ok(UploadOutcome {
                        remote_ref: remote_rel.to_string(),
                        bytes: total,
                        etag: read_etag(&resp2),
                    })
                }
                401 => Err(CoreError::CloudAuth(
                    "PUT rejected (401); generate a Nextcloud app password".into(),
                )),
                s => Err(CoreError::Http {
                    status: Some(s),
                    msg: format!("PUT (retry) {url} -> {s}"),
                }),
            };
        }
        if status == 401 {
            return Err(CoreError::CloudAuth(
                "PUT rejected (401); generate a Nextcloud app password".into(),
            ));
        }
        Err(CoreError::Http {
            status: Some(status),
            msg: format!("PUT {url} -> {status}"),
        })
    }

    /// MKCOL each ancestor collection of `remote_rel`, top-down. Treats 201
    /// (created) and 405 (already exists) as success.
    async fn mkcol_parents(&self, remote_rel: &str) -> Result<()> {
        let mut prefix = String::new();
        let segs: Vec<&str> = remote_rel.split('/').collect();
        // Skip the last segment (the file itself).
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if seg.is_empty() {
                continue;
            }
            if prefix.is_empty() {
                prefix = (*seg).to_string();
            } else {
                prefix = format!("{prefix}/{seg}");
            }
            let url = files_url(&self.base_url, &self.username, &prefix);
            let method = Method::from_bytes(b"MKCOL").expect("valid method");
            let resp = self
                .client
                .request(method, &url)
                .basic_auth(&self.username, Some(&self.app_password))
                .send()
                .await
                .map_err(transport_err)?;
            match resp.status().as_u16() {
                201 | 405 => {}
                401 => {
                    return Err(CoreError::CloudAuth(
                        "MKCOL rejected (401); generate a Nextcloud app password".into(),
                    ))
                }
                s => {
                    return Err(CoreError::Http {
                        status: Some(s),
                        msg: format!("MKCOL {url} -> {s}"),
                    })
                }
            }
        }
        Ok(())
    }

    /// Build and send one streaming PUT. Shared by put_simple and the AutoMkcol retry.
    async fn send_put(&self, local: &Path, url: &str, total: u64) -> Result<reqwest::Response> {
        let md5 = md5_hex_of(local)?;
        let file = File::open(local).await.map_err(|e| CoreError::Io {
            path: local.to_path_buf(),
            source: e,
        })?;
        let body = Body::wrap_stream(ReaderStream::new(file));

        let mut req = self
            .client
            .put(url)
            .basic_auth(&self.username, Some(&self.app_password))
            .header(reqwest::header::CONTENT_LENGTH, total)
            .header("OC-Checksum", format!("md5:{md5}"))
            .header("X-NC-WebDAV-AutoMkcol", "1");
        if let Some(m) = mtime_secs(local) {
            req = req.header("X-OC-Mtime", m.to_string());
        }
        req.body(body).send().await.map_err(transport_err)
    }
}

#[allow(dead_code)]
impl NextcloudUploader {
    /// Chunked upload (Nextcloud chunking v2). MKCOL the upload dir, PUT each
    /// numbered part with `Destination` + `OC-Total-Length`, then MOVE `.file`
    /// to the final path. `progress` reports cumulative uploaded bytes.
    pub(crate) async fn put_chunked(
        &self,
        local: &Path,
        remote_rel: &str,
        total: u64,
        resume: Option<ResumeState>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<UploadOutcome> {
        let dest = files_url(&self.base_url, &self.username, remote_rel);

        // Determine upload id (reuse on resume; fresh otherwise).
        let resumed_id = resume.as_ref().and_then(|r| r.upload_id.clone());
        let upload_id = resumed_id
            .clone()
            .unwrap_or_else(|| format!("gpbeam-{}", uuid::Uuid::new_v4()));
        let dir = uploads_url(&self.base_url, &self.username, &upload_id, None);

        // Probe existing chunks (empty unless resuming).
        let mut present = self.resume_present_chunks(&dir, resume.as_ref()).await?;

        // Resume requested but the dir was gone (404 => empty map) => start over
        // with a fresh id + MKCOL.
        let (upload_id, dir) = if resumed_id.is_some() && present.is_empty() {
            let fresh = format!("gpbeam-{}", uuid::Uuid::new_v4());
            let fresh_dir = uploads_url(&self.base_url, &self.username, &fresh, None);
            present = std::collections::HashMap::new();
            (fresh, fresh_dir)
        } else {
            (upload_id, dir)
        };
        let _ = &upload_id; // id retained for ResumeState persistence by the worker

        // MKCOL the upload dir unless we're continuing an existing one.
        if present.is_empty() {
            self.mkcol_upload_dir(&dir, &dest).await?;
        }

        let chunk_size = pick_chunk_size(total);
        let n_chunks = total.div_ceil(chunk_size).max(1) as u32;
        let mut uploaded: u64 = 0;

        for i in 1..=n_chunks {
            let offset = (i as u64 - 1) * chunk_size;
            let len = chunk_size.min(total - offset);
            if present.get(&i).copied() == Some(len) {
                // Already fully stored — count it and skip.
                uploaded += len;
                progress(uploaded);
                continue;
            }
            self.put_chunk(&dir, &dest, i, local, offset, len, total)
                .await?;
            uploaded += len;
            progress(uploaded);
        }

        let etag = self.move_assemble(&dir, &dest, total, local).await?;
        Ok(UploadOutcome {
            remote_ref: remote_rel.to_string(),
            bytes: total,
            etag,
        })
    }

    async fn mkcol_upload_dir(&self, dir: &str, dest: &str) -> Result<()> {
        let method = Method::from_bytes(b"MKCOL").expect("valid method");
        let resp = self
            .client
            .request(method, dir)
            .basic_auth(&self.username, Some(&self.app_password))
            .header("Destination", dest)
            .send()
            .await
            .map_err(transport_err)?;
        match resp.status().as_u16() {
            201 | 405 => Ok(()),
            401 => Err(CoreError::CloudAuth(
                "MKCOL upload dir rejected (401)".into(),
            )),
            s => Err(CoreError::Http {
                status: Some(s),
                msg: format!("MKCOL {dir} -> {s}"),
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_chunk(
        &self,
        dir: &str,
        dest: &str,
        part: u32,
        local: &Path,
        offset: u64,
        len: u64,
        total: u64,
    ) -> Result<()> {
        let url = format!("{dir}/{}", chunk_name(part));
        let mut file = File::open(local).await.map_err(|e| CoreError::Io {
            path: local.to_path_buf(),
            source: e,
        })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| CoreError::Io {
                path: local.to_path_buf(),
                source: e,
            })?;
        let limited = file.take(len);
        let body = Body::wrap_stream(ReaderStream::new(limited));

        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.app_password))
            .header("Destination", dest)
            .header("OC-Total-Length", total.to_string())
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await
            .map_err(transport_err)?;
        match resp.status().as_u16() {
            201 | 204 => Ok(()),
            401 => Err(CoreError::CloudAuth("chunk PUT rejected (401)".into())),
            s => Err(CoreError::Http {
                status: Some(s),
                msg: format!("PUT chunk {url} -> {s}"),
            }),
        }
    }

    async fn move_assemble(
        &self,
        dir: &str,
        dest: &str,
        total: u64,
        local: &Path,
    ) -> Result<Option<String>> {
        let url = format!("{dir}/.file");
        let method = Method::from_bytes(b"MOVE").expect("valid method");
        let mut req = self
            .client
            .request(method, &url)
            .basic_auth(&self.username, Some(&self.app_password))
            .header("Destination", dest)
            .header("OC-Total-Length", total.to_string());
        // M1: attach the whole-file md5 so Nextcloud verifies the reassembled
        // chunks, matching the integrity guarantee the simple PUT gives small
        // files. BEST-EFFORT: the chunks already live server-side, so if the
        // local copy is no longer readable at assembly time (e.g. a resume where
        // every chunk was uploaded in a prior session and the dest volume has
        // since been unmounted), assemble WITHOUT the checksum rather than
        // failing an otherwise-complete upload — OC-Total-Length still guards size.
        match md5_hex_of(local) {
            Ok(md5) => req = req.header("OC-Checksum", format!("md5:{md5}")),
            Err(e) => eprintln!(
                "gpbeam: assembling {dest} without OC-Checksum (local file unreadable: {e})"
            ),
        }
        if let Some(m) = mtime_secs(local) {
            req = req.header("X-OC-Mtime", m.to_string());
        }
        let resp = req.send().await.map_err(transport_err)?;
        match resp.status().as_u16() {
            201 | 204 => Ok(read_etag(&resp)),
            401 => Err(CoreError::CloudAuth("MOVE rejected (401)".into())),
            s => Err(CoreError::Http {
                status: Some(s),
                msg: format!("MOVE {url} -> {s}"),
            }),
        }
    }

    /// On resume, PROPFIND Depth:1 the upload dir and return present chunk sizes.
    /// A 404 means the upload expired -> return empty (caller MKCOLs a fresh dir;
    /// the upload_id was already regenerated when resume.upload_id was None).
    async fn resume_present_chunks(
        &self,
        dir: &str,
        resume: Option<&ResumeState>,
    ) -> Result<std::collections::HashMap<u32, u64>> {
        // No prior session id => nothing to resume.
        if resume.and_then(|r| r.upload_id.as_ref()).is_none() {
            return Ok(std::collections::HashMap::new());
        }
        let method = Method::from_bytes(b"PROPFIND").expect("valid method");
        let resp = self
            .client
            .request(method, dir)
            .basic_auth(&self.username, Some(&self.app_password))
            .header("Depth", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/></d:prop></d:propfind>"#)
            .send()
            .await
            .map_err(transport_err)?;
        match resp.status().as_u16() {
            207 => {
                let body = resp.text().await.map_err(transport_err)?;
                Ok(parse_chunk_listing(&body))
            }
            404 => Ok(std::collections::HashMap::new()),
            401 => Err(CoreError::CloudAuth("PROPFIND upload dir rejected (401)".into())),
            s => Err(CoreError::Http {
                status: Some(s),
                msg: format!("PROPFIND {dir} -> {s}"),
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
        local: &Path,
        remote: &str,
        total: u64,
        resume: Option<ResumeState>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<UploadOutcome> {
        let remote_rel = self.remote_rel(remote);
        if total < self.chunk_threshold {
            self.put_simple(local, &remote_rel, total, progress).await
        } else {
            self.put_chunked(local, &remote_rel, total, resume, progress)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudConfig, CloudKind, MirrorMode};
    use crate::credentials::Secret;
    use std::path::PathBuf;
    use wiremock::matchers::path_regex as wm_path_regex;
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

    use std::io::Write as _;

    fn tmp_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn put_simple_201_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/clip.mp4"))
            .and(header("X-NC-WebDAV-AutoMkcol", "1"))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"etag-1\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let f = tmp_file(b"hello gopro");
        let total = b"hello gopro".len() as u64;
        let mut seen = 0u64;
        let mut cb = |n: u64| seen = n;

        let out = up
            .put_simple(f.path(), "GoPro/clip.mp4", total, &mut cb)
            .await
            .unwrap();

        assert_eq!(out.remote_ref, "GoPro/clip.mp4");
        assert_eq!(out.bytes, total);
        assert_eq!(out.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(seen, total);
    }

    #[tokio::test]
    async fn put_simple_409_then_mkcol_then_retry_ok() {
        let server = MockServer::start().await;

        // First PUT: parent missing -> 409 (scoped to a single call).
        let _guard = Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/sub/clip.mp4"))
            .respond_with(ResponseTemplate::new(409))
            .up_to_n_times(1)
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        // MKCOL of each parent collection succeeds (201). Tolerates repeats.
        Mock::given(wm_method("MKCOL"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(wm_method("MKCOL"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/sub"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // Retry PUT now succeeds.
        Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/sub/clip.mp4"))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"e2\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let f = tmp_file(b"data");
        let mut cb = |_n: u64| {};
        let out = up
            .put_simple(f.path(), "GoPro/sub/clip.mp4", 4, &mut cb)
            .await
            .unwrap();
        assert_eq!(out.etag.as_deref(), Some("\"e2\""));
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::header_exists;
    use wiremock::{Match, Request};

    /// Custom matcher: assert the chunk PUT carries Destination + OC-Total-Length.
    struct ChunkHeadersOk {
        expected_total: u64,
        seen_dest: Arc<AtomicU64>, // 1 if Destination header present and correct
    }
    impl Match for ChunkHeadersOk {
        fn matches(&self, req: &Request) -> bool {
            let dest_ok = req
                .headers
                .get("destination")
                .map(|v| {
                    v.to_str()
                        .unwrap_or("")
                        .contains("/remote.php/dav/files/alice/GoPro/big.mp4")
                })
                .unwrap_or(false);
            let total_ok = req
                .headers
                .get("oc-total-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                == Some(self.expected_total);
            if dest_ok && total_ok {
                self.seen_dest.store(1, Ordering::SeqCst);
            }
            dest_ok && total_ok
        }
    }

    #[tokio::test]
    async fn put_chunked_mkcol_put_move_201() {
        let server = MockServer::start().await;
        let total: u64 = 12 * 1024 * 1024; // > 5 MiB => 3 chunks at 5 MiB
        let seen = Arc::new(AtomicU64::new(0));

        // MKCOL upload dir.
        Mock::given(wm_method("MKCOL"))
            .and(header_exists("Destination"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        // Chunk PUTs with required headers.
        Mock::given(wm_method("PUT"))
            .and(ChunkHeadersOk {
                expected_total: total,
                seen_dest: seen.clone(),
            })
            .respond_with(ResponseTemplate::new(201))
            .expect(3)
            .mount(&server)
            .await;

        // MOVE .file -> final.
        Mock::given(wm_method("MOVE"))
            .and(header_exists("Destination"))
            .and(header("OC-Total-Length", total.to_string()))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"chunked-etag\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let f = tmp_file(&vec![7u8; total as usize]);
        let mut last = 0u64;
        let mut cb = |n: u64| last = n;

        let out = up
            .put_chunked(f.path(), "GoPro/big.mp4", total, None, &mut cb)
            .await
            .unwrap();

        assert_eq!(out.bytes, total);
        assert_eq!(out.etag.as_deref(), Some("\"chunked-etag\""));
        assert_eq!(last, total, "progress reached total");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "Destination+OC-Total-Length asserted on a chunk PUT"
        );
    }

    #[tokio::test]
    async fn put_chunked_move_carries_oc_checksum() {
        // M1: the assembling MOVE must carry `OC-Checksum: md5:<hex>` of the whole
        // file so Nextcloud verifies the reassembled chunks. Previously only the
        // simple/small-file PUT sent a checksum, so the LARGEST files (the ones
        // that cross chunk_threshold) got the LEAST verification.
        let server = MockServer::start().await;
        let total: u64 = 12 * 1024 * 1024; // 3 chunks @ 5 MiB
        let f = tmp_file(&vec![7u8; total as usize]);
        let expected_md5 = super::md5_hex_of(f.path()).unwrap();

        Mock::given(wm_method("MKCOL"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(wm_method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        // The MOVE only matches if it carries the exact OC-Checksum; otherwise
        // there is no mock and move_assemble sees a 404 -> the call errors.
        Mock::given(wm_method("MOVE"))
            .and(header("OC-Checksum", format!("md5:{expected_md5}").as_str()))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"e\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let mut cb = |_n: u64| {};
        let out = up
            .put_chunked(f.path(), "GoPro/big.mp4", total, None, &mut cb)
            .await
            .unwrap();
        assert_eq!(out.etag.as_deref(), Some("\"e\""));
    }

    #[test]
    fn md5_hex_of_matches_known_digest() {
        // Guards the streaming refactor of md5_hex_of (memory-bounded read for
        // multi-GB clips) against a regression in the digest value.
        let f = tmp_file(b"abc");
        assert_eq!(
            super::md5_hex_of(f.path()).unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[tokio::test]
    async fn put_chunked_assembles_without_checksum_when_local_unreadable() {
        // M1 robustness: if every chunk was already uploaded in a prior session
        // and the local file is no longer readable at assembly time, the MOVE
        // must still assemble (the chunks live server-side) — WITHOUT OC-Checksum
        // — rather than fail a complete upload with an Io error.
        struct NoChecksum;
        impl wiremock::Match for NoChecksum {
            fn matches(&self, req: &wiremock::Request) -> bool {
                req.headers.get("oc-checksum").is_none()
            }
        }

        let server = MockServer::start().await;
        let total: u64 = 10 * 1024 * 1024; // 2 chunks @ 5 MiB
        let chunk = 5u64 * 1024 * 1024;
        let listing = format!(
            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:href>/remote.php/dav/uploads/alice/gpbeam-resume-2/00001</d:href>
    <d:propstat><d:prop><d:getcontentlength>{chunk}</d:getcontentlength></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
  <d:response><d:href>/remote.php/dav/uploads/alice/gpbeam-resume-2/00002</d:href>
    <d:propstat><d:prop><d:getcontentlength>{chunk}</d:getcontentlength></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#
        );
        Mock::given(wm_method("PROPFIND"))
            .and(wm_path("/remote.php/dav/uploads/alice/gpbeam-resume-2"))
            .respond_with(ResponseTemplate::new(207).set_body_raw(listing, "application/xml"))
            .mount(&server)
            .await;
        // Nothing left to PUT.
        Mock::given(wm_method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        // The MOVE must arrive WITHOUT an OC-Checksum (the local file is gone).
        Mock::given(wm_method("MOVE"))
            .and(NoChecksum)
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"e\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let missing = std::path::Path::new("/nonexistent/gpbeam/gone.mp4");
        let resume = ResumeState {
            upload_id: Some("gpbeam-resume-2".into()),
            uploaded_bytes: total,
        };
        let mut cb = |_n: u64| {};
        let out = up
            .put_chunked(missing, "GoPro/big.mp4", total, Some(resume), &mut cb)
            .await
            .unwrap();
        assert_eq!(out.etag.as_deref(), Some("\"e\""));
    }

    #[tokio::test]
    async fn put_chunked_resumes_skipping_present_part1() {
        let server = MockServer::start().await;
        let total: u64 = 10 * 1024 * 1024; // 2 chunks @ 5 MiB
        let chunk = 5u64 * 1024 * 1024;

        // PROPFIND of the upload dir lists part 00001 already stored at full size.
        let listing = format!(
            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/uploads/alice/gpbeam-resume-1/00001</d:href>
    <d:propstat><d:prop><d:getcontentlength>{chunk}</d:getcontentlength></d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#
        );
        Mock::given(wm_method("PROPFIND"))
            .and(wm_path("/remote.php/dav/uploads/alice/gpbeam-resume-1"))
            .respond_with(ResponseTemplate::new(207).set_body_raw(listing, "application/xml"))
            .expect(1)
            .mount(&server)
            .await;

        // Only part 00002 should be PUT (00001 is skipped). No MKCOL expected.
        Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/uploads/alice/gpbeam-resume-1/00002"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        // Guard against part 1 being re-uploaded.
        Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/uploads/alice/gpbeam-resume-1/00001"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        Mock::given(wm_method("MOVE"))
            .and(wm_path("/remote.php/dav/uploads/alice/gpbeam-resume-1/.file"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg_for(&server.uri()), &test_secret()).unwrap();
        let f = tmp_file(&vec![3u8; total as usize]);
        let resume = ResumeState {
            upload_id: Some("gpbeam-resume-1".into()),
            uploaded_bytes: chunk,
        };
        let mut last = 0u64;
        let mut cb = |n: u64| last = n;

        let out = up
            .put_chunked(f.path(), "GoPro/big.mp4", total, Some(resume), &mut cb)
            .await
            .unwrap();
        assert_eq!(out.bytes, total);
        assert_eq!(last, total);
    }

    #[tokio::test]
    async fn upload_small_uses_simple_put() {
        let server = MockServer::start().await;
        let mut cfg = cfg_for(&server.uri());
        cfg.chunk_threshold = 50 * 1024 * 1024; // small file is below threshold

        Mock::given(wm_method("PUT"))
            .and(wm_path("/remote.php/dav/files/alice/GoPro/small.mp4"))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"s1\""))
            .expect(1)
            .mount(&server)
            .await;
        // No chunk dance should occur.
        Mock::given(wm_method("MKCOL"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg, &test_secret()).unwrap();
        let f = tmp_file(b"tiny");
        let mut cb = |_n: u64| {};
        let out = up
            .upload(f.path(), "small.mp4", 4, None, &mut cb)
            .await
            .unwrap();
        assert_eq!(out.remote_ref, "GoPro/small.mp4");
        assert_eq!(out.etag.as_deref(), Some("\"s1\""));
    }

    #[tokio::test]
    async fn upload_large_uses_chunk_dance() {
        let server = MockServer::start().await;
        let mut cfg = cfg_for(&server.uri());
        cfg.chunk_threshold = 1024; // force chunking for anything bigger than 1 KiB
        let total: u64 = 6 * 1024 * 1024; // 2 chunks @ 5 MiB

        Mock::given(wm_method("MKCOL"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(wm_method("PUT"))
            .and(wm_path_regex(r"^/remote\.php/dav/uploads/alice/.+/\d{5}$"))
            .respond_with(ResponseTemplate::new(201))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(wm_method("MOVE"))
            .respond_with(ResponseTemplate::new(201).insert_header("OC-ETag", "\"big\""))
            .expect(1)
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg, &test_secret()).unwrap();
        let f = tmp_file(&vec![1u8; total as usize]);
        let mut cb = |_n: u64| {};
        let out = up
            .upload(f.path(), "big.mp4", total, None, &mut cb)
            .await
            .unwrap();
        assert_eq!(out.bytes, total);
        assert_eq!(out.etag.as_deref(), Some("\"big\""));
    }

    #[tokio::test]
    async fn upload_401_maps_to_cloud_auth() {
        let server = MockServer::start().await;
        let mut cfg = cfg_for(&server.uri());
        cfg.chunk_threshold = 50 * 1024 * 1024;

        Mock::given(wm_method("PUT"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let up = NextcloudUploader::new(&cfg, &test_secret()).unwrap();
        let f = tmp_file(b"x");
        let mut cb = |_n: u64| {};
        let err = up
            .upload(f.path(), "x.mp4", 1, None, &mut cb)
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::CloudAuth(_)), "got {err:?}");
    }
}
