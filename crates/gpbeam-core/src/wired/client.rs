//! Open GoPro HTTP API v2.0 client (USB / GoPro Connect).
//!
//! A USB-connected modern GoPro exposes an HTTP API at `http://<ip>:8080`. This
//! client wraps the handful of endpoints the offload pipeline needs: version
//! probe, camera info, wired-control enable, media list, ranged/resumable
//! download, and delete. Built incrementally across Phase 2; mirrors the
//! reqwest + wiremock style of `crate::cloud::nextcloud`.

use crate::error::{io_at, CoreError, Result};
use reqwest::Client;
use serde::Deserialize;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

/// TCP connect timeout. Candidate probing can hit routed-but-unanswering IPs
/// (VPN/Docker 172.2x ranges), and without this the OS default (~75s) stalls
/// the detector's 2s tick — probes run serially inside `poll_once`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Default overall deadline for short control requests (version / info /
/// wired-control / media list / delete) and for a download's response headers.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
/// Default maximum gap between download body chunks before the transfer is
/// declared stalled and fails with a retryable transport error. Downloads get
/// NO whole-request timeout — multi-GB clips take arbitrarily long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Identity of a connected camera, from `GET /gopro/camera/info`.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
}

/// One media file as reported by `GET /gopro/media/list`, flattened across the
/// per-directory grouping. `captured_unix` is the camera's `cre` (creation)
/// timestamp in Unix seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteMedia {
    pub dir: String,
    pub name: String,
    pub size: u64,
    pub captured_unix: i64,
}

/// HTTP client for one GoPro camera at `http://<ip>:8080`.
#[derive(Debug, Clone)]
pub struct GoProClient {
    http: Client,
    base: String,
    control_timeout: Duration,
    idle_timeout: Duration,
}

impl GoProClient {
    /// Build a client for a camera at `ip` (port 8080, plain HTTP over USB).
    pub fn new(ip: IpAddr) -> Self {
        Self::with_base(format!("http://{ip}:8080"))
    }

    /// Build a client pointed at an explicit base URL. Used by tests to target a
    /// wiremock server. A trailing slash is trimmed so URL joins are clean.
    pub fn with_base(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        GoProClient {
            // A connect timeout is mandatory: a dead routed IP or a camera that
            // drops mid-handshake must not hang for the OS default. Building
            // this plain client cannot fail in practice; panicking on builder
            // failure matches `Client::new()`'s own documented behavior.
            http: Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("reqwest client"),
            base,
            control_timeout: CONTROL_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
        }
    }

    /// Override the control-request and body-idle timeouts (defaults:
    /// [`CONTROL_TIMEOUT`] / [`IDLE_TIMEOUT`]). The detector's probes and the
    /// tests use short ones.
    pub fn with_timeouts(mut self, control: Duration, idle: Duration) -> Self {
        self.control_timeout = control;
        self.idle_timeout = idle;
        self
    }

    /// The base URL (`http://<ip>:8080`), trailing slash trimmed.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Full download URL for a media file: `{base}/videos/DCIM/{dir}/{name}`.
    pub(crate) fn media_url(&self, m: &RemoteMedia) -> String {
        format!("{}/videos/DCIM/{}/{}", self.base, m.dir, m.name)
    }

    /// `GET /gopro/version` -> the API version string (e.g. "2.0"). A 200 with a
    /// missing/blank `version` field still returns Ok("") (defensive); non-200 ->
    /// `Http`.
    pub async fn version(&self) -> Result<String> {
        #[derive(Deserialize, Default)]
        struct VersionBody {
            #[serde(default)]
            version: String,
        }
        let url = format!("{}/gopro/version", self.base);
        let resp = self
            .http
            .get(&url)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} -> {status}"),
            });
        }
        let text = resp.text().await.map_err(transport_err)?;
        let body: VersionBody = serde_json::from_str(&text).map_err(|e| CoreError::Http {
            status: None,
            msg: format!("GET {url} parse error: {e}"),
        })?;
        Ok(body.version)
    }

    /// `GET /gopro/camera/info` -> `CameraInfo`. Defensive parse: each field
    /// defaults to "" when absent; unknown fields ignored. Non-200 -> `Http`.
    pub async fn info(&self) -> Result<CameraInfo> {
        #[derive(Deserialize, Default)]
        struct InfoBody {
            #[serde(default)]
            model_name: String,
            #[serde(default)]
            serial_number: String,
            #[serde(default)]
            firmware_version: String,
        }
        let url = format!("{}/gopro/camera/info", self.base);
        let resp = self
            .http
            .get(&url)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} -> {status}"),
            });
        }
        let text = resp.text().await.map_err(transport_err)?;
        let body: InfoBody = serde_json::from_str(&text).map_err(|e| CoreError::Http {
            status: None,
            msg: format!("GET {url} parse error: {e}"),
        })?;
        Ok(CameraInfo {
            model: body.model_name,
            serial: body.serial_number,
            firmware: body.firmware_version,
        })
    }

    /// `GET /gopro/camera/control/wired_usb?p=1` — enable wired control. Many
    /// cameras work without this; the Phase 4 caller treats an Err as non-fatal.
    /// 200 -> Ok(()); any other status -> `Http`.
    pub async fn enable_wired_control(&self) -> Result<()> {
        let base = format!("{}/gopro/camera/control/wired_usb", self.base);
        let url = with_query(&base, &[("p", "1")])?;
        let resp = self
            .http
            .get(url.clone())
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        if status == 200 {
            Ok(())
        } else {
            Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} -> {status}"),
            })
        }
    }

    /// `GET /gopro/media/list` -> a flat list of every media file on the card.
    /// Non-200 -> `Http`.
    pub async fn media_list(&self) -> Result<Vec<RemoteMedia>> {
        let url = format!("{}/gopro/media/list", self.base);
        let resp = self
            .http
            .get(&url)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} -> {status}"),
            });
        }
        let text = resp.text().await.map_err(transport_err)?;
        parse_media_list(&text)
    }

    /// Download `m` into `part_path`, resuming from its current byte length via a
    /// `Range: bytes=<part_len>-` request. The body is streamed chunk-by-chunk
    /// straight into the `.part` (append on a confirmed-offset 206 resume,
    /// truncate otherwise) while an incremental BLAKE3 covers the full on-disk
    /// file (a resumed prefix is re-hashed first, on the blocking pool). A
    /// `.part` already at the advertised size skips the network entirely and is
    /// only re-hashed. A 416, or a 206 whose `Content-Range` start differs from
    /// the resume offset, marks the `.part` stale: it is discarded and the
    /// download restarts once from 0. Response headers must arrive within the
    /// control timeout and each body chunk within the idle timeout (there is no
    /// whole-request timeout — multi-GB clips); a breach fails with a retryable
    /// transport `Http` error. The file is flushed AND fsynced before returning
    /// (the caller renames + verifies + ledger-commits — and may delete the
    /// camera original — immediately after). `progress` is called with the
    /// cumulative bytes on disk. Returns `(total_bytes_on_disk, blake3_hex)`.
    /// Other non-2xx -> `Http`.
    pub async fn download_resumable(
        &self,
        m: &RemoteMedia,
        part_path: &Path,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(u64, String)> {
        // Bytes already on disk -> the Range start offset.
        let mut already = match std::fs::metadata(part_path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                return Err(CoreError::Io {
                    path: part_path.to_path_buf(),
                    source: e,
                })
            }
        };

        // M7: a `.part` longer than the advertised size is stale/corrupt (a
        // leftover from different content, or a truncation artifact). Discard it
        // and restart from scratch, so the Range start never points past the end
        // of the remote file — `Range: bytes=<too-big>-` is answered with 416.
        if already > m.size {
            let _ = std::fs::remove_file(part_path);
            already = 0;
        }

        // A fully-downloaded `.part` (== the advertised size) cannot be
        // trusted: hashing it here would be self-verifying (the caller's
        // verify step compares against the hash WE derive from these very
        // bytes), so a torn write left by a crash — e.g. a full-length but
        // partially-persisted file from a pre-fsync build — would be imported
        // as authentic and could then trigger delete-after-verify against the
        // only good copy on the camera. There is no camera-side checksum to
        // compare with, so the only trustworthy source is the camera itself:
        // discard the `.part` and re-download from offset 0. (Rare path — it
        // needs a crash in the window between download end and rename.)
        if already == m.size && already > 0 {
            let _ = std::fs::remove_file(part_path);
            already = 0;
        }

        let url = self.media_url(m);
        let mut restarted = false;
        let (mut resp, resume) = loop {
            // Bound only the HEADERS with the control timeout; the body gets a
            // per-chunk idle bound below (a whole-request timeout would kill
            // legitimate multi-GB transfers).
            let send = self
                .http
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes={already}-"))
                .send();
            let resp = tokio::time::timeout(self.control_timeout, send)
                .await
                .map_err(|_| timeout_err(&url, "response headers", self.control_timeout))?
                .map_err(transport_err)?;
            let status = resp.status().as_u16();
            // 416: the server says our resume offset is unsatisfiable — the
            // `.part` is stale. Discard it and restart once from offset 0.
            if status == 416 && !restarted {
                restarted = true;
                let _ = std::fs::remove_file(part_path);
                already = 0;
                continue;
            }
            if status != 200 && status != 206 {
                return Err(CoreError::Http {
                    status: Some(status),
                    msg: format!("GET {url} (Range bytes={already}-) -> {status}"),
                });
            }
            // Only a 206 with prior bytes is a candidate resume (append). A 200 means the
            // server (re)sent the whole file, so restart from scratch and truncate any
            // stale `.part`.
            if status == 206 && already > 0 {
                // Trust-but-verify the resume offset: a server restarting from a
                // different offset would land bytes at the wrong position — a
                // corruption the streamed BLAKE3 cannot catch (it hashes exactly
                // what we write), and one that total-size checks can miss. Require
                // Content-Range to start at `already`; anything else (including a
                // missing header) discards the `.part` and restarts from 0.
                match content_range_start(resp.headers()) {
                    Some(start) if start == already => break (resp, true),
                    got => {
                        if restarted {
                            return Err(CoreError::Http {
                                status: Some(206),
                                msg: format!(
                                    "GET {url}: Content-Range start {got:?} != resume offset {already}"
                                ),
                            });
                        }
                        restarted = true;
                        let _ = std::fs::remove_file(part_path);
                        already = 0;
                        continue;
                    }
                }
            }
            break (resp, false);
        };

        // Templates may include subfolders (e.g. `{date}/...`), so make the parent exist.
        if let Some(parent) = part_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
        }

        // Hash the WHOLE on-disk file: seed with the resumed prefix (re-read on
        // the blocking pool — it can be multi-GB), then the streamed bytes.
        let (mut hasher, mut total) = if resume {
            (hash_part_prefix(part_path).await?, already)
        } else {
            (blake3::Hasher::new(), 0u64)
        };

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resume)
            .truncate(!resume)
            .open(part_path)
            .await
            .map_err(|e| CoreError::Io {
                path: part_path.to_path_buf(),
                source: e,
            })?;

        // Stream chunk-by-chunk: bounded memory (no full-file buffering), live progress,
        // and an incremental BLAKE3 — essential for multi-GB GoPro clips. Each chunk
        // must arrive within the idle timeout: a camera stalling with the TCP
        // connection open would otherwise hang the offload (and the shared offload
        // lock blocking SD ingest) forever.
        use tokio::io::AsyncWriteExt;
        loop {
            let chunk = tokio::time::timeout(self.idle_timeout, resp.chunk())
                .await
                .map_err(|_| timeout_err(&url, "body stalled", self.idle_timeout))?
                .map_err(transport_err)?;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).await.map_err(|e| CoreError::Io {
                path: part_path.to_path_buf(),
                source: e,
            })?;
            hasher.update(&chunk);
            total += chunk.len() as u64;
            progress(total);
        }
        file.flush().await.map_err(|e| CoreError::Io {
            path: part_path.to_path_buf(),
            source: e,
        })?;
        // Durability before the caller renames/verifies/ledger-commits (and possibly
        // deletes the camera original): flush only reaches the page cache. Matches
        // the SD path's `stream_hash_to_part` (flush THEN sync_all).
        file.sync_all().await.map_err(|e| CoreError::Io {
            path: part_path.to_path_buf(),
            source: e,
        })?;

        Ok((total, hasher.finalize().to_hex().to_string()))
    }

    /// `GET /gopro/media/delete?path={dir}/{name}` — delete a file from the
    /// camera by its on-card path. 200 -> Ok(()); any other status -> `Http`.
    /// The Phase 4 caller treats an Err as non-fatal.
    pub async fn delete_path(&self, dir: &str, name: &str) -> Result<()> {
        let base = format!("{}/gopro/media/delete", self.base);
        let path_param = format!("{dir}/{name}");
        let url = with_query(&base, &[("path", path_param.as_str())])?;
        let resp = self
            .http
            .get(url.clone())
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        if status == 200 {
            Ok(())
        } else {
            Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} -> {status}"),
            })
        }
    }

    /// Delete a listed [`RemoteMedia`] from the camera (uses its dir + name only).
    pub async fn delete(&self, m: &RemoteMedia) -> Result<()> {
        self.delete_path(&m.dir, &m.name).await
    }
}

/// BLAKE3-hash an existing `.part` prefix on the blocking pool: it can be
/// multi-GB of synchronous read I/O, which must never pin an async worker.
async fn hash_part_prefix(part_path: &Path) -> Result<blake3::Hasher> {
    let p = part_path.to_path_buf();
    crate::wired::run_blocking(part_path, move || {
        let mut hasher = blake3::Hasher::new();
        let existing = std::fs::File::open(&p).map_err(io_at(&p))?;
        hasher.update_reader(existing).map_err(io_at(&p))?;
        Ok(hasher)
    })
    .await
}

/// Parse the start offset of a `Content-Range: bytes <start>-<end>/<total>`
/// header. `None` for a missing, foreign-unit, or unparseable header.
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, _total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    start.trim().parse().ok()
}

/// An elapsed tokio deadline as a retryable transport-style `Http` error
/// (`status: None`, like `transport_err`).
fn timeout_err(url: &str, what: &str, after: Duration) -> CoreError {
    CoreError::Http {
        status: None,
        msg: format!("GET {url}: {what} timed out after {after:?}"),
    }
}

/// Parse a `/gopro/media/list` JSON body into a flat `Vec<RemoteMedia>`.
///
/// The API encodes sizes/timestamps as strings (e.g. "s":"684588850"). An entry
/// whose size is missing or unparseable is SKIPPED: a defaulted 0 would pass
/// planning, download the whole file, then always fail the caller's size check
/// (re-downloading multi-GB on every connect), and a zeroed identity would also
/// silently drop deferred camera-deletes in the reap. `cre` still defaults to 0
/// — a wrong capture date is recoverable where a wrong size is not. Directory
/// groups (`media[].d` + `media[].fs[]`) are flattened in order. Unknown JSON
/// fields are ignored.
fn parse_media_list(json: &str) -> Result<Vec<RemoteMedia>> {
    #[derive(Deserialize, Default)]
    struct ListBody {
        #[serde(default)]
        media: Vec<DirGroup>,
    }
    #[derive(Deserialize, Default)]
    struct DirGroup {
        #[serde(default)]
        d: String,
        #[serde(default)]
        fs: Vec<FileEntry>,
    }
    #[derive(Deserialize, Default)]
    struct FileEntry {
        #[serde(default)]
        n: String,
        #[serde(default)]
        s: String,
        #[serde(default)]
        cre: String,
    }

    let body: ListBody = serde_json::from_str(json).map_err(|e| CoreError::Http {
        status: None,
        msg: format!("media list parse error: {e}"),
    })?;

    let mut out = Vec::new();
    for group in body.media {
        for f in group.fs {
            let size = match f.s.parse::<u64>() {
                Ok(s) => s,
                Err(_) => {
                    // No log facade in gpbeam-core; match cloud::worker's stderr style.
                    eprintln!(
                        "gpbeam wired: skipping media entry {}/{} with unparseable size {:?}",
                        group.d, f.n, f.s
                    );
                    continue;
                }
            };
            out.push(RemoteMedia {
                dir: group.d.clone(),
                name: f.n,
                size,
                captured_unix: f.cre.parse::<i64>().unwrap_or(0),
            });
        }
    }
    Ok(out)
}

/// Build a URL string with percent-encoded query parameters. reqwest is built
/// without its `query` feature here, so we use `url::Url::parse_with_params`
/// (the `url` crate is already a dependency) to attach + encode params.
fn with_query(base: &str, params: &[(&str, &str)]) -> Result<String> {
    let url =
        url::Url::parse_with_params(base, params.iter().copied()).map_err(|e| CoreError::Http {
            status: None,
            msg: format!("bad url {base}: {e}"),
        })?;
    Ok(url.into())
}

/// Map a reqwest transport error (no HTTP response) to a retryable
/// `Http { status: None, .. }`, matching `cloud::nextcloud::transport_err`.
fn transport_err(e: reqwest::Error) -> CoreError {
    CoreError::Http {
        status: None,
        msg: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{IpAddr, Ipv4Addr};
    use wiremock::matchers::{method as wm_method, path as wm_path, query_param};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    /// Match a GET whose `Range` header starts at exactly `from` (`bytes=<from>-`).
    struct RangeFrom {
        from: u64,
    }
    impl Match for RangeFrom {
        fn matches(&self, req: &Request) -> bool {
            req.headers
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s == format!("bytes={}-", self.from))
                .unwrap_or(false)
        }
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[tokio::test]
    async fn version_parses_version_field() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(r#"{"version":"2.0"}"#, "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        assert_eq!(c.version().await.unwrap(), "2.0");
    }

    #[tokio::test]
    async fn version_404_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let err = c.version().await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(404),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_path_hits_media_delete_endpoint() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/media/delete"))
            .and(query_param("path", "100GOPRO/GX010001.MP4"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let c = GoProClient::with_base(server.uri());
        c.delete_path("100GOPRO", "GX010001.MP4").await.unwrap();
    }

    #[tokio::test]
    async fn info_parses_model_serial_firmware() {
        let server = MockServer::start().await;
        let body = r#"{
            "model_name": "Mission 1 Pro",
            "model_number": 99,
            "serial_number": "C3575424520622",
            "firmware_version": "H26.01.01.10.00",
            "ap_mac_address": "deadbeef"
        }"#;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/info"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let info = c.info().await.unwrap();
        assert_eq!(
            info,
            CameraInfo {
                model: "Mission 1 Pro".into(),
                serial: "C3575424520622".into(),
                firmware: "H26.01.01.10.00".into(),
            }
        );
    }

    #[tokio::test]
    async fn info_missing_fields_default_to_empty() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/info"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{}"#, "application/json"))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let info = c.info().await.unwrap();
        assert_eq!(info.model, "");
        assert_eq!(info.serial, "");
        assert_eq!(info.firmware, "");
    }

    #[tokio::test]
    async fn info_500_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/info"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let err = c.info().await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(500),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn enable_wired_control_sends_p1_and_succeeds() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/control/wired_usb"))
            .and(query_param("p", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        c.enable_wired_control().await.unwrap();
    }

    #[tokio::test]
    async fn enable_wired_control_403_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/control/wired_usb"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let err = c.enable_wired_control().await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(403),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_media_list_flattens_dirs_and_parses_string_numbers() {
        let json = r#"{
          "id": "1",
          "media": [
            {
              "d": "100GOPRO",
              "fs": [
                {"n":"GX010001.MP4","s":"684588850","cre":"1780515910","mod":"1780515912"},
                {"n":"GX010002.MP4","s":"12","cre":"1780600000","mod":"1780600001"}
              ]
            },
            {
              "d": "101GOPRO",
              "fs": [
                {"n":"GS010003.360","s":"42","cre":"1780700000","mod":"1780700001"}
              ]
            }
          ]
        }"#;
        let got = parse_media_list(json).unwrap();
        assert_eq!(
            got,
            vec![
                RemoteMedia {
                    dir: "100GOPRO".into(),
                    name: "GX010001.MP4".into(),
                    size: 684588850,
                    captured_unix: 1780515910
                },
                RemoteMedia {
                    dir: "100GOPRO".into(),
                    name: "GX010002.MP4".into(),
                    size: 12,
                    captured_unix: 1780600000
                },
                RemoteMedia {
                    dir: "101GOPRO".into(),
                    name: "GS010003.360".into(),
                    size: 42,
                    captured_unix: 1780700000
                },
            ]
        );
    }

    #[test]
    fn parse_media_list_skips_entries_with_unparseable_size() {
        // A defaulted size of 0 would pass planning, download the WHOLE file,
        // then fail "size mismatch: got N, expected 0" on EVERY connect — and a
        // zeroed identity also drops deferred camera-deletes in the reap. Such
        // entries must be skipped, not zeroed.
        let json = r#"{"media":[{"d":"100GOPRO","fs":[
            {"n":"NO_SIZE.MP4"},
            {"n":"BAD_SIZE.MP4","s":"not-a-number","cre":"1780600000"},
            {"n":"GOOD.MP4","s":"42","cre":"also-bad"}
        ]}]}"#;
        let got = parse_media_list(json).unwrap();
        // Only the entry with a parseable size survives; its bad `cre` still
        // defaults to 0 (a wrong capture date is recoverable, a wrong size not).
        assert_eq!(
            got,
            vec![RemoteMedia {
                dir: "100GOPRO".into(),
                name: "GOOD.MP4".into(),
                size: 42,
                captured_unix: 0
            }]
        );
    }

    #[test]
    fn parse_media_list_empty_media_is_empty_vec() {
        assert_eq!(parse_media_list(r#"{"media":[]}"#).unwrap(), vec![]);
        assert_eq!(parse_media_list(r#"{}"#).unwrap(), vec![]);
    }

    #[tokio::test]
    async fn media_list_fetches_and_parses() {
        let server = MockServer::start().await;
        let body = r#"{"media":[{"d":"100GOPRO","fs":[
            {"n":"GX010001.MP4","s":"100","cre":"1780515910","mod":"1780515912"}
        ]}]}"#;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/media/list"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let list = c.media_list().await.unwrap();
        assert_eq!(
            list,
            vec![RemoteMedia {
                dir: "100GOPRO".into(),
                name: "GX010001.MP4".into(),
                size: 100,
                captured_unix: 1780515910
            }]
        );
    }

    #[tokio::test]
    async fn media_list_500_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/media/list"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let err = c.media_list().await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(500),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn download_resumable_fresh_streams_and_hashes() {
        let server = MockServer::start().await;
        let full = b"hello gopro wired download".to_vec();

        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 0 })
            .respond_with(ResponseTemplate::new(206).set_body_bytes(full.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64,
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");

        let mut last = 0u64;
        let mut cb = |n: u64| last = n;
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();

        assert_eq!(total, full.len() as u64);
        assert_eq!(hash, blake3_hex(&full));
        assert_eq!(last, full.len() as u64, "progress reached total");
        assert_eq!(std::fs::read(&part).unwrap(), full);
    }

    #[tokio::test]
    async fn download_resumable_resumes_from_existing_part() {
        let server = MockServer::start().await;
        let full = b"0123456789ABCDEFGHIJ".to_vec(); // 20 bytes
        let head_len = 8u64; // pre-existing .part has 8 bytes
        let tail = full[head_len as usize..].to_vec();

        // Only the tail is served, and only for a Range starting at head_len. The
        // Content-Range start must match the resume offset or the client restarts
        // from scratch (wrong-offset corruption guard).
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: head_len })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", format!("bytes {head_len}-19/20").as_str())
                    .set_body_bytes(tail.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Guard: a fresh (bytes=0-) request must NOT happen.
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 0 })
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64,
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        // Pre-create the .part with the first head_len bytes.
        {
            let mut f = std::fs::File::create(&part).unwrap();
            f.write_all(&full[..head_len as usize]).unwrap();
            f.flush().unwrap();
        }

        let mut last = 0u64;
        let mut cb = |n: u64| last = n;
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();

        assert_eq!(total, full.len() as u64);
        assert_eq!(
            hash,
            blake3_hex(&full),
            "hash is over the FULL reassembled file"
        );
        assert_eq!(last, full.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), full);
    }

    #[tokio::test]
    async fn download_resumable_discards_oversized_part() {
        // M7: a .part longer than the advertised media size is stale/corrupt and
        // must be discarded + re-fetched fresh (Range: bytes=0-), never resumed
        // with a Range start past the end of the file (a 416 trap).
        let server = MockServer::start().await;
        let full = b"0123456789ABCDEFGHIJ".to_vec(); // 20 bytes

        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 0 })
            .respond_with(ResponseTemplate::new(206).set_body_bytes(full.clone()))
            .expect(1)
            .mount(&server)
            .await;
        // Guard: a resume from the oversized length (30) must NOT happen.
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 30 })
            .respond_with(ResponseTemplate::new(416))
            .expect(0)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64, // 20
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        // Pre-create an oversized .part (30 bytes > media.size 20).
        {
            let mut f = std::fs::File::create(&part).unwrap();
            f.write_all(&[1u8; 30]).unwrap();
            f.flush().unwrap();
        }

        let mut last = 0u64;
        let mut cb = |n: u64| last = n;
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();

        assert_eq!(total, full.len() as u64);
        assert_eq!(
            hash,
            blake3_hex(&full),
            "hash is over the freshly downloaded file"
        );
        assert_eq!(last, full.len() as u64);
        assert_eq!(
            std::fs::read(&part).unwrap(),
            full,
            "oversized .part was replaced"
        );
    }

    #[tokio::test]
    async fn download_resumable_distrusts_complete_part_and_redownloads() {
        // A pre-existing `.part` already at the FULL advertised size cannot be
        // trusted: hashing it locally would be self-verifying (a torn write
        // from a crash would import as authentic and could then trigger
        // delete-after-verify against the only good copy on the camera).
        // The stale bytes must be DISCARDED and the file re-downloaded whole.
        let server = MockServer::start().await;
        let full = b"0123456789ABCDEFGHIJ".to_vec(); // 20 bytes
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(full.clone()))
            .expect(1) // exactly one full re-download
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64,
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        // Simulate a torn write: full LENGTH, wrong bytes. The old skip-network
        // path would have imported this corruption as authentic.
        let torn = vec![0u8; full.len()];
        std::fs::write(&part, &torn).unwrap();

        let mut last = 0u64;
        let mut cb = |n: u64| last = n;
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();
        assert_eq!(total, full.len() as u64);
        assert_eq!(
            hash,
            blake3_hex(&full),
            "hash covers the CAMERA's bytes, not the torn on-disk bytes"
        );
        assert_eq!(last, full.len() as u64, "progress reported the new total");
        assert_eq!(
            std::fs::read(&part).unwrap(),
            full,
            ".part replaced with the re-downloaded content"
        );
        // `server` drop verifies the `.expect(1)`.
    }

    #[tokio::test]
    async fn download_resumable_416_discards_stale_part_and_restarts() {
        // Defensive 416 handling: even when our offset looks in-range, a server
        // answering 416 means the `.part` is stale — discard it and restart from
        // 0 instead of failing permanently on every connect.
        let server = MockServer::start().await;
        let full = b"0123456789ABCDEFGHIJ".to_vec(); // 20 bytes

        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 8 })
            .respond_with(ResponseTemplate::new(416))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 0 })
            .respond_with(ResponseTemplate::new(206).set_body_bytes(full.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64,
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        std::fs::write(&part, [9u8; 8]).unwrap(); // stale 8-byte prefix

        let mut cb = |_n: u64| {};
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();
        assert_eq!(total, full.len() as u64);
        assert_eq!(
            hash,
            blake3_hex(&full),
            "stale prefix was discarded, not resumed"
        );
        assert_eq!(std::fs::read(&part).unwrap(), full);
    }

    #[tokio::test]
    async fn download_resumable_wrong_offset_206_restarts_from_zero() {
        // A 206 that does NOT resume from our offset (Content-Range start !=
        // already) would land bytes at the wrong position — corruption BLAKE3
        // cannot catch. The client must discard the `.part` and restart from 0.
        let server = MockServer::start().await;
        let full = b"0123456789ABCDEFGHIJ".to_vec(); // 20 bytes
        let head_len = 8u64;

        // The resume attempt is answered 206 but restarting from offset 0.
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: head_len })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-19/20")
                    .set_body_bytes(full.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
        // The restart then fetches the whole file fresh.
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: 0 })
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-19/20")
                    .set_body_bytes(full.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: full.len() as u64,
            captured_unix: 1780515910,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        std::fs::write(&part, &full[..head_len as usize]).unwrap();

        let mut cb = |_n: u64| {};
        let (total, hash) = c.download_resumable(&m, &part, &mut cb).await.unwrap();
        // Pre-fix this appended the full body to the 8-byte prefix (total 28).
        assert_eq!(total, full.len() as u64);
        assert_eq!(hash, blake3_hex(&full));
        assert_eq!(
            std::fs::read(&part).unwrap(),
            full,
            "no double-write corruption"
        );
    }

    #[test]
    fn content_range_start_parses_standard_and_rejects_garbage() {
        use reqwest::header::{HeaderMap, HeaderValue, CONTENT_RANGE};
        let mut h = HeaderMap::new();
        assert_eq!(content_range_start(&h), None, "missing header");
        h.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 8-19/20"));
        assert_eq!(content_range_start(&h), Some(8));
        h.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-19/*"));
        assert_eq!(content_range_start(&h), Some(0));
        h.insert(CONTENT_RANGE, HeaderValue::from_static("bytes */20"));
        assert_eq!(
            content_range_start(&h),
            None,
            "unsatisfied-range form has no start"
        );
        h.insert(CONTENT_RANGE, HeaderValue::from_static("chickens 8-19/20"));
        assert_eq!(content_range_start(&h), None, "foreign unit");
    }

    #[tokio::test]
    async fn control_request_timeout_is_retryable_http_not_a_hang() {
        // A camera that accepts the connection but never answers a control
        // request must fail fast with a retryable transport error, not hang the
        // detector/offload forever.
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"version":"2.0"}"#, "application/json")
                    .set_delay(std::time::Duration::from_secs(10)),
            )
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri()).with_timeouts(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(100),
        );
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), c.version())
            .await
            .expect("version() must fail fast, not hang")
            .unwrap_err();
        assert!(
            matches!(err, CoreError::Http { status: None, .. }),
            "got {err:?}"
        );
        assert!(
            crate::error::is_retryable(&err),
            "timeouts must be retryable"
        );
    }

    #[tokio::test]
    async fn download_stalled_mid_body_times_out_as_retryable_http() {
        // A camera that stalls mid-transfer with the TCP connection open must
        // trip the per-chunk idle timeout — pre-fix, resp.chunk().await hung
        // forever, wedging the offload and the shared offload lock. wiremock
        // cannot trickle-then-stall a body, so use a raw TCP server.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await; // consume the request head
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                .await
                .unwrap();
            sock.flush().await.unwrap();
            // Hold the connection open without sending the remaining 93 bytes.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });

        let c = GoProClient::with_base(format!("http://{addr}")).with_timeouts(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(200),
        );
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: 100,
            captured_unix: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        let mut cb = |_n: u64| {};
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            c.download_resumable(&m, &part, &mut cb),
        )
        .await
        .expect("a stalled body must fail fast, not hang the offload")
        .unwrap_err();
        assert!(
            matches!(err, CoreError::Http { status: None, .. }),
            "got {err:?}"
        );
        assert!(crate::error::is_retryable(&err), "stall must be retryable");
        server.abort();
    }

    #[tokio::test]
    async fn download_resumable_404_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: 10,
            captured_unix: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("GX010001.MP4.part");
        let mut cb = |_n: u64| {};
        let err = c.download_resumable(&m, &part, &mut cb).await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(404),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_sends_path_query_and_succeeds() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/media/delete"))
            .and(query_param("path", "100GOPRO/GX010001.MP4"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
            .expect(1)
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: 10,
            captured_unix: 0,
        };
        c.delete(&m).await.unwrap();
    }

    #[tokio::test]
    async fn delete_500_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/media/delete"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = GoProClient::with_base(server.uri());
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: 10,
            captured_unix: 0,
        };
        let err = c.delete(&m).await.unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Http {
                    status: Some(500),
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn new_builds_base_from_ip() {
        let c = GoProClient::new(IpAddr::V4(Ipv4Addr::new(172, 26, 122, 51)));
        assert_eq!(c.base(), "http://172.26.122.51:8080");
    }

    #[test]
    fn with_base_uses_given_url_verbatim_trimming_trailing_slash() {
        let c = GoProClient::with_base("http://127.0.0.1:9999/");
        assert_eq!(c.base(), "http://127.0.0.1:9999");
    }

    #[test]
    fn media_url_joins_dir_and_name() {
        let c = GoProClient::with_base("http://10.0.0.1:8080");
        let m = RemoteMedia {
            dir: "100GOPRO".into(),
            name: "GX010001.MP4".into(),
            size: 10,
            captured_unix: 1780515910,
        };
        assert_eq!(
            c.media_url(&m),
            "http://10.0.0.1:8080/videos/DCIM/100GOPRO/GX010001.MP4"
        );
    }
}
