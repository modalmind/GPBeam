use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};

use tokio::sync::mpsc::UnboundedSender;

use crate::wired::client::GoProClient;

/// Enumerate the host's IPv4 interface addresses via the `if-addrs` crate.
/// Returns an empty vec on enumeration failure (e.g. restricted CI sandboxes).
fn host_ipv4s() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|i| match i.addr.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                std::net::IpAddr::V6(_) => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Pure: given the host's IPv4 interface addresses, return the GoPro Connect camera IPs to
/// probe — the `.51` host on each GoPro-Connect-range `/24`.
///
/// GoPro Connect (IP-over-USB) assigns the host an address in `172.20.0.0`–`172.29.255.255`
/// and exposes the camera's Open GoPro API at `.51` on that same `/24`. We therefore map
/// every in-range host address `172.X.Y.Z` (with `X` in `20..=29`) to its candidate camera
/// `172.X.Y.51`. Addresses outside the GoPro-Connect range (Docker's `172.17.x.x`, `10.x`,
/// `192.168.x`, loopback, link-local) are skipped — a `/gopro/version` probe would never
/// confirm them anyway, but excluding them up front avoids needless probes. Host addresses
/// that are themselves `.51` are skipped (don't probe ourselves). Results are de-duplicated
/// while preserving first-seen order.
pub fn candidate_camera_ips(host_ips: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut seen: HashSet<Ipv4Addr> = HashSet::new();
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for ip in host_ips {
        let [a, b, c, d] = ip.octets();
        // GoPro Connect host range: 172.20.x.x – 172.29.x.x.
        if a != 172 || !(20..=29).contains(&b) {
            continue;
        }
        // Don't probe ourselves if the host is already the camera octet.
        if d == 51 {
            continue;
        }
        let candidate = Ipv4Addr::new(a, b, c, 51);
        if seen.insert(candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Pure de-bounce: the candidate IPs present **now** that were **absent** the previous
/// tick. Mirrors `crate::detect::newly_appeared` but on probe-confirmed IPs. Returns the
/// absent→present edges so the async poller fires `CameraFound` exactly once per present
/// camera and re-arms after it disappears.
pub fn probe_edge(before: &HashSet<IpAddr>, now: &HashSet<IpAddr>) -> Vec<IpAddr> {
    now.difference(before).cloned().collect()
}

/// Probe one camera base URL's `/gopro/version`. Returns `true` only when a GoPro answered
/// successfully (confirming a real Open GoPro device, not just any host on a `172.x` net).
/// Any error — non-2xx, transport failure, or unparseable body — yields `false`.
pub(crate) async fn probe_version(base: &str) -> bool {
    GoProClient::with_base(base).version().await.is_ok()
}

/// A USB-connected GoPro confirmed by a `/gopro/version` probe, with its identity.
#[derive(Debug, Clone)]
pub struct CameraFound {
    pub ip: IpAddr,
    pub info: crate::wired::client::CameraInfo,
}

/// Build the Open GoPro base URL for a camera IP: `http://{ip}:8080`.
fn base_for(ip: IpAddr) -> String {
    format!("http://{ip}:8080")
}

/// One detection tick (testable seam). Enumerate candidate camera IPs from the host IPs
/// produced by `ip_src`, probe each via `build_base(ip)`'s `/gopro/version`, compute the
/// absent→present edge against `present`, and for each newly-present camera fetch `info()`
/// and send `CameraFound` on `tx`. Updates `present` in place to the probe-confirmed set so
/// the next tick de-bounces. Returns early (without panicking) if `tx` is closed.
async fn poll_once<S, B>(
    ip_src: &S,
    build_base: &B,
    present: &mut HashSet<IpAddr>,
    tx: &UnboundedSender<CameraFound>,
) where
    S: Fn() -> Vec<Ipv4Addr>,
    B: Fn(IpAddr) -> String,
{
    // Confirm which candidates currently answer as GoPros.
    let mut confirmed: HashSet<IpAddr> = HashSet::new();
    for v4 in candidate_camera_ips(&ip_src()) {
        let ip = IpAddr::V4(v4);
        if probe_version(&build_base(ip)).await {
            confirmed.insert(ip);
        }
    }
    // Fire once per absent->present edge.
    for ip in probe_edge(present, &confirmed) {
        match GoProClient::with_base(build_base(ip)).info().await {
            Ok(info) => {
                if tx.send(CameraFound { ip, info }).is_err() {
                    return; // receiver dropped -> caller will stop the loop
                }
            }
            // info() failed despite version() succeeding: skip this tick; a later tick
            // re-probes. Do NOT mark it present, so it can fire again once info() works.
            Err(_) => {
                confirmed.remove(&ip);
            }
        }
    }
    *present = confirmed;
}

/// Async detector: every ~2s enumerate the host's IPv4 interface addresses (`if-addrs`),
/// derive GoPro-Connect camera candidates, probe `http://{ip}:8080/gopro/version`, and on an
/// absent→present edge fetch `info()` and send `CameraFound` on `tx`. De-bounced so a present
/// camera fires once and re-arms after it disconnects. Mirrors
/// `crate::detect::poll_removable_mounts`. Stops when the receiver is dropped.
pub async fn poll_for_camera(tx: UnboundedSender<CameraFound>) {
    let mut present: HashSet<IpAddr> = HashSet::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticker.tick().await;
        poll_once(&host_ipv4s, &base_for, &mut present, &tx).await;
        if tx.is_closed() {
            return; // receiver dropped -> stop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_addrs_enumeration_links_and_returns_ipv4_vec() {
        // We don't assert any specific interface exists (CI may have none);
        // we only prove the `if-addrs` dep links and host_ipv4s() is callable.
        let v: Vec<Ipv4Addr> = host_ipv4s();
        // loopback, if present, must be a valid v4 (sanity on the mapping).
        for ip in &v {
            assert_eq!(ip.octets().len(), 4);
        }
    }

    #[test]
    fn maps_gopro_range_host_to_dot51_on_same_24() {
        // The live-confirmed case: host 172.26.122.56 -> camera 172.26.122.51.
        let host = vec![Ipv4Addr::new(172, 26, 122, 56)];
        assert_eq!(
            candidate_camera_ips(&host),
            vec![Ipv4Addr::new(172, 26, 122, 51)]
        );
    }

    #[test]
    fn covers_whole_172_20_to_172_29_second_octet_range() {
        // Second octet 20..=29 inclusive are GoPro-Connect; 19 and 30 are not.
        for o2 in 20u8..=29 {
            let host = vec![Ipv4Addr::new(172, o2, 5, 100)];
            assert_eq!(
                candidate_camera_ips(&host),
                vec![Ipv4Addr::new(172, o2, 5, 51)],
                "172.{o2}.5.100 should map to .51"
            );
        }
        assert!(candidate_camera_ips(&[Ipv4Addr::new(172, 19, 5, 100)]).is_empty());
        assert!(candidate_camera_ips(&[Ipv4Addr::new(172, 30, 5, 100)]).is_empty());
    }

    #[test]
    fn excludes_docker_default_bridge_172_17() {
        // Docker's default bridge is 172.17.x.x — must NOT be treated as a GoPro.
        let host = vec![Ipv4Addr::new(172, 17, 0, 1)];
        assert!(candidate_camera_ips(&host).is_empty());
    }

    #[test]
    fn excludes_private_10_and_192_168_and_loopback() {
        let hosts = vec![
            Ipv4Addr::new(10, 0, 0, 5),
            Ipv4Addr::new(192, 168, 1, 23),
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(169, 254, 9, 9),
        ];
        assert!(candidate_camera_ips(&hosts).is_empty());
    }

    #[test]
    fn excludes_host_that_is_already_dot51() {
        // If the host itself is .51 we don't want to "probe ourselves".
        let host = vec![Ipv4Addr::new(172, 26, 122, 51)];
        assert!(candidate_camera_ips(&host).is_empty());
    }

    #[test]
    fn dedups_when_multiple_hosts_share_a_24() {
        // Two host addrs on the same /24 yield a single candidate.
        let hosts = vec![
            Ipv4Addr::new(172, 26, 122, 56),
            Ipv4Addr::new(172, 26, 122, 99),
        ];
        assert_eq!(
            candidate_camera_ips(&hosts),
            vec![Ipv4Addr::new(172, 26, 122, 51)]
        );
    }

    #[test]
    fn distinct_24s_yield_distinct_candidates_in_first_seen_order() {
        let hosts = vec![
            Ipv4Addr::new(172, 26, 122, 56),
            Ipv4Addr::new(172, 21, 7, 2),
        ];
        assert_eq!(
            candidate_camera_ips(&hosts),
            vec![
                Ipv4Addr::new(172, 26, 122, 51),
                Ipv4Addr::new(172, 21, 7, 51),
            ]
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(candidate_camera_ips(&[]).is_empty());
    }

    use std::net::IpAddr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn edge_reports_only_newly_present_ips() {
        let before: HashSet<IpAddr> = [v4(172, 26, 122, 51)].into_iter().collect();
        let now: HashSet<IpAddr> =
            [v4(172, 26, 122, 51), v4(172, 21, 7, 51)].into_iter().collect();
        let mut appeared = probe_edge(&before, &now);
        appeared.sort();
        assert_eq!(appeared, vec![v4(172, 21, 7, 51)]);
    }

    #[test]
    fn edge_debounces_a_still_present_camera() {
        // Same camera present two ticks in a row -> no new edge the second time.
        let s: HashSet<IpAddr> = [v4(172, 26, 122, 51)].into_iter().collect();
        assert!(probe_edge(&s, &s).is_empty());
    }

    #[test]
    fn edge_rearms_after_disappearance() {
        let present: HashSet<IpAddr> = [v4(172, 26, 122, 51)].into_iter().collect();
        let gone: HashSet<IpAddr> = HashSet::new();
        // camera leaves -> nothing newly present
        assert!(probe_edge(&present, &gone).is_empty());
        // camera returns -> fires again
        assert_eq!(
            probe_edge(&gone, &present),
            vec![v4(172, 26, 122, 51)]
        );
    }

    #[test]
    fn edge_empty_when_nothing_present() {
        let empty: HashSet<IpAddr> = HashSet::new();
        assert!(probe_edge(&empty, &empty).is_empty());
    }

    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn probe_version_true_when_gopro_answers_200() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(br#"{"version":"2.0"}"#.to_vec(), "application/json"),
            )
            .mount(&server)
            .await;
        assert!(probe_version(&server.uri()).await);
    }

    #[tokio::test]
    async fn probe_version_false_on_404() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(!probe_version(&server.uri()).await);
    }

    #[tokio::test]
    async fn probe_version_false_on_unreachable_host() {
        // Nothing listening on this base -> transport error -> false (no panic).
        assert!(!probe_version("http://127.0.0.1:1").await);
    }

    use crate::wired::client::CameraInfo;

    #[tokio::test]
    async fn poll_once_emits_camera_found_then_debounces() {
        let server = MockServer::start().await;
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(br#"{"version":"2.0"}"#.to_vec(), "application/json"),
            )
            .mount(&server)
            .await;
        // Cross-phase invariant: info() parses TOP-LEVEL model_name/serial_number/
        // firmware_version, so the mock must serve them top-level (not nested under "info").
        let info_body = br#"{"model_name":"HERO12 Black","serial_number":"C3575424520622","firmware_version":"H23.01.02.10.00"}"#.to_vec();
        Mock::given(wm_method("GET"))
            .and(wm_path("/gopro/camera/info"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(info_body, "application/json"),
            )
            .mount(&server)
            .await;

        // Inject: one fake in-range host IP; base builder points every candidate at the mock.
        let base = server.uri();
        let host_ips = vec![Ipv4Addr::new(172, 26, 122, 56)];
        let ip_src = || host_ips.clone();
        let build_base = |_ip: IpAddr| base.clone();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CameraFound>();
        let mut present: HashSet<IpAddr> = HashSet::new();

        // Tick 1: absent -> present edge -> exactly one CameraFound.
        poll_once(&ip_src, &build_base, &mut present, &tx).await;
        let found = rx.try_recv().expect("expected a CameraFound on the first tick");
        assert_eq!(found.ip, IpAddr::V4(Ipv4Addr::new(172, 26, 122, 51)));
        assert_eq!(
            found.info,
            CameraInfo {
                model: "HERO12 Black".into(),
                serial: "C3575424520622".into(),
                firmware: "H23.01.02.10.00".into(),
            }
        );
        assert!(rx.try_recv().is_err(), "only one CameraFound expected");

        // Tick 2: still present -> de-bounced -> nothing sent.
        poll_once(&ip_src, &build_base, &mut present, &tx).await;
        assert!(rx.try_recv().is_err(), "present camera must not re-fire");
    }

    #[tokio::test]
    async fn poll_once_emits_nothing_when_no_gopro_answers() {
        // In-range host IP, but the probed base has no GoPro -> no edge, no send.
        let host_ips = vec![Ipv4Addr::new(172, 26, 122, 56)];
        let ip_src = || host_ips.clone();
        let build_base = |_ip: IpAddr| "http://127.0.0.1:1".to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CameraFound>();
        let mut present: HashSet<IpAddr> = HashSet::new();
        poll_once(&ip_src, &build_base, &mut present, &tx).await;
        assert!(rx.try_recv().is_err());
        assert!(present.is_empty(), "no camera -> present set stays empty");
    }
}
