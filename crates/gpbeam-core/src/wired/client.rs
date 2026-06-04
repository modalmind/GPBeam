//! Open GoPro HTTP API v2.0 client (USB / GoPro Connect).
//!
//! A USB-connected modern GoPro exposes an HTTP API at `http://<ip>:8080`. This
//! client wraps the handful of endpoints the offload pipeline needs: version
//! probe, camera info, wired-control enable, media list, ranged/resumable
//! download, and delete. Built incrementally across Phase 2; mirrors the
//! reqwest + wiremock style of `crate::cloud::nextcloud`.

use crate::error::{CoreError, Result};
use reqwest::Client;
use serde::Deserialize;
use std::io::Cursor;
use std::net::IpAddr;
use std::path::Path;

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
            http: Client::new(),
            base,
        }
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
        let resp = self.http.get(&url).send().await.map_err(transport_err)?;
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
        let resp = self.http.get(&url).send().await.map_err(transport_err)?;
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
        let resp = self.http.get(url.clone()).send().await.map_err(transport_err)?;
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
        let resp = self.http.get(&url).send().await.map_err(transport_err)?;
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
    /// `Range: bytes=<part_len>-` request. The response body is buffered, then fed
    /// to `crate::transfer::stream_hash_to_part`, which appends to the `.part`
    /// (open-append when `already > 0`) and BLAKE3-hashes the full on-disk file.
    /// `progress` is called with the cumulative bytes on disk. Returns
    /// `(total_bytes_on_disk, blake3_hex)`. Non-2xx -> `Http`.
    pub async fn download_resumable(
        &self,
        m: &RemoteMedia,
        part_path: &Path,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(u64, String)> {
        // Bytes already on disk -> the Range start offset.
        let already = match std::fs::metadata(part_path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                return Err(CoreError::Io {
                    path: part_path.to_path_buf(),
                    source: e,
                })
            }
        };

        let url = self.media_url(m);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::RANGE, format!("bytes={already}-"))
            .send()
            .await
            .map_err(transport_err)?;
        let status = resp.status().as_u16();
        // 200 (full) and 206 (partial) are both acceptable; the helper appends
        // from `already`, so a server that ignores Range and resends from 0 would
        // double-append — but the Open GoPro API honors Range (Accept-Ranges:
        // bytes), and resume only triggers when `already > 0`.
        if status != 200 && status != 206 {
            return Err(CoreError::Http {
                status: Some(status),
                msg: format!("GET {url} (Range bytes={already}-) -> {status}"),
            });
        }

        let body = resp.bytes().await.map_err(transport_err)?;
        let mut reader = Cursor::new(body);
        crate::transfer::stream_hash_to_part(&mut reader, part_path, already, progress)
    }

    /// `GET /gopro/media/delete?path={dir}/{name}` — delete a file from the
    /// camera. 200 -> Ok(()); any other status -> `Http`. The Phase 4 caller
    /// treats an Err as non-fatal.
    pub async fn delete(&self, m: &RemoteMedia) -> Result<()> {
        let base = format!("{}/gopro/media/delete", self.base);
        let path_param = format!("{}/{}", m.dir, m.name);
        let url = with_query(&base, &[("path", path_param.as_str())])?;
        let resp = self.http.get(url.clone()).send().await.map_err(transport_err)?;
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
}

/// Parse a `/gopro/media/list` JSON body into a flat `Vec<RemoteMedia>`.
///
/// The API encodes sizes/timestamps as strings (e.g. "s":"684588850"); we parse
/// them to numbers, defaulting to 0 on missing/unparseable values. Directory
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
            out.push(RemoteMedia {
                dir: group.d.clone(),
                name: f.n,
                size: f.s.parse::<u64>().unwrap_or(0),
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
    let url = url::Url::parse_with_params(base, params.iter().copied()).map_err(|e| {
        CoreError::Http {
            status: None,
            msg: format!("bad url {base}: {e}"),
        }
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
    use std::net::{IpAddr, Ipv4Addr};
    use std::io::Write as _;
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
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"version":"2.0"}"#, "application/json"),
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
        assert!(matches!(err, CoreError::Http { status: Some(404), .. }), "got {err:?}");
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
        assert!(matches!(err, CoreError::Http { status: Some(500), .. }), "got {err:?}");
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
        assert!(matches!(err, CoreError::Http { status: Some(403), .. }), "got {err:?}");
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
                RemoteMedia { dir: "100GOPRO".into(), name: "GX010001.MP4".into(), size: 684588850, captured_unix: 1780515910 },
                RemoteMedia { dir: "100GOPRO".into(), name: "GX010002.MP4".into(), size: 12, captured_unix: 1780600000 },
                RemoteMedia { dir: "101GOPRO".into(), name: "GS010003.360".into(), size: 42, captured_unix: 1780700000 },
            ]
        );
    }

    #[test]
    fn parse_media_list_missing_or_bad_fields_default_to_zero() {
        let json = r#"{"media":[{"d":"100GOPRO","fs":[
            {"n":"GX010001.MP4"},
            {"n":"GX010002.MP4","s":"not-a-number","cre":"also-bad"}
        ]}]}"#;
        let got = parse_media_list(json).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], RemoteMedia { dir: "100GOPRO".into(), name: "GX010001.MP4".into(), size: 0, captured_unix: 0 });
        assert_eq!(got[1], RemoteMedia { dir: "100GOPRO".into(), name: "GX010002.MP4".into(), size: 0, captured_unix: 0 });
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
            vec![RemoteMedia { dir: "100GOPRO".into(), name: "GX010001.MP4".into(), size: 100, captured_unix: 1780515910 }]
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
        assert!(matches!(err, CoreError::Http { status: Some(500), .. }), "got {err:?}");
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
        let head_len = 8u64;                          // pre-existing .part has 8 bytes
        let tail = full[head_len as usize..].to_vec();

        // Only the tail is served, and only for a Range starting at head_len.
        Mock::given(wm_method("GET"))
            .and(wm_path("/videos/DCIM/100GOPRO/GX010001.MP4"))
            .and(RangeFrom { from: head_len })
            .respond_with(ResponseTemplate::new(206).set_body_bytes(tail.clone()))
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
        assert_eq!(hash, blake3_hex(&full), "hash is over the FULL reassembled file");
        assert_eq!(last, full.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), full);
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
        assert!(matches!(err, CoreError::Http { status: Some(404), .. }), "got {err:?}");
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
        assert!(matches!(err, CoreError::Http { status: Some(500), .. }), "got {err:?}");
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
        assert_eq!(c.media_url(&m), "http://10.0.0.1:8080/videos/DCIM/100GOPRO/GX010001.MP4");
    }
}
