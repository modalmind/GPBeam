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
use std::net::IpAddr;

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
    #[allow(dead_code)]
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
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
