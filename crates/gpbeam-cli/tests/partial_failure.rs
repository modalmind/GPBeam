//! Partial failures must surface in the run functions' return values so
//! `main.rs` can exit non-zero: `run_offload_and_mirror` / `run_mirror` return
//! the count of terminally-failed files (sync copy failures + cloud jobs the
//! worker gave up on). 0 strictly means a fully-clean run.

use gpbeam_core::ledger::Ledger;
use std::path::Path;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a minimal HERO11-shaped card: 100GOPRO/ with one small MP4 and the
/// version.txt that makes `is_gopro_card` return true.
fn make_card(root: &Path) {
    let dcim = root.join("DCIM").join("100GOPRO");
    std::fs::create_dir_all(&dcim).unwrap();
    std::fs::write(dcim.join("GX010001.MP4"), b"hello gopro media").unwrap();
    let misc = root.join("MISC");
    std::fs::create_dir_all(&misc).unwrap();
    std::fs::write(
        misc.join("version.txt"),
        r#"{"firmware version":"H22.01.01.10.00","camera serial number":"C1234567890123"}"#,
    )
    .unwrap();
}

/// A `[cloud]` config in `mode` whose PUTs all hit `base` and that gives up
/// after ONE attempt, so the first upload failure is terminal (CloudFailed).
fn failing_cloud_toml(base: &str, mode: &str) -> String {
    format!(
        r#"
[cloud]
kind = "nextcloud"
destination_id = "home-nc"
base_url = "{base}"
username = "alice"
remote_root = "GoProBackup"
mirror_mode = "{mode}"
max_attempts = 1

[credentials.home-nc]
username = "alice"
app_password = "test-app-pw"
"#
    )
}

/// Mock Nextcloud where every upload fails: PROPFIND -> 404 (absent),
/// PUT -> 500 (server error).
async fn failing_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path_regex(r"^/remote\.php/dav/files/.+$"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/remote\.php/dav/files/.+$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

#[cfg(unix)]
#[tokio::test]
async fn offload_returns_sync_copy_failure_count() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    make_card(&card);

    // One unreadable clip: copy fails, tallied into RunSummary.failed.
    let bad = card.join("DCIM").join("100GOPRO").join("GX010002.MP4");
    std::fs::write(&bad, b"unreadable clip").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut lines: Vec<String> = Vec::new();
    let flags = gpbeam_cli::SafetyFlags::default();
    let failed =
        gpbeam_cli::run_offload_and_mirror(&card, &dest, None, &flags, &mut |l| lines.push(l))
            .await
            .expect("partial failure is Ok(n), not Err");

    let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));

    assert_eq!(failed, 1, "one copy failed; lines: {lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("[FAIL]")),
        "expected a FAIL line, got: {lines:?}"
    );
}

#[tokio::test]
async fn auto_mirror_returns_terminal_cloud_failure_count() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    make_card(&card);

    let server = failing_server().await;
    let cfg_path = tmp.path().join("gpbeam.toml");
    std::fs::write(&cfg_path, failing_cloud_toml(&server.uri(), "auto")).unwrap();

    let mut lines: Vec<String> = Vec::new();
    let flags = gpbeam_cli::SafetyFlags::default();
    let failed =
        gpbeam_cli::run_offload_and_mirror(&card, &dest, Some(&cfg_path), &flags, &mut |l| {
            lines.push(l)
        })
        .await
        .expect("terminal cloud failure is Ok(n), not Err");

    assert_eq!(failed, 1, "one upload terminally failed; lines: {lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("[cloud-FAIL]")),
        "expected a cloud-FAIL line, got: {lines:?}"
    );
}

#[tokio::test]
async fn run_mirror_returns_terminal_cloud_failure_count() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    // A real local file for the uploader to PUT.
    let local = dest.join("GX010001.MP4");
    std::fs::write(&local, b"hello gopro media").unwrap();
    let total = std::fs::metadata(&local).unwrap().len();

    // Seed: one imported row + one Queued cloud job at the SAME ledger path
    // `run_mirror` will open.
    let lpath = gpbeam_cli::ledger_path_for(&dest);
    {
        let mut ledger = Ledger::open(&lpath).unwrap();
        let imp = ledger
            .record(
                "C1234567890123",
                "GX010001.MP4",
                total,
                1_700_000_000,
                local.to_str().unwrap(),
                None,
            )
            .unwrap();
        ledger
            .enqueue_cloud_job(
                imp,
                "home-nc",
                local.to_str().unwrap(),
                "GoProBackup/GX010001.MP4",
                total,
                None,
            )
            .unwrap();
    }

    let server = failing_server().await;
    let cfg_path = tmp.path().join("gpbeam.toml");
    std::fs::write(&cfg_path, failing_cloud_toml(&server.uri(), "manual")).unwrap();

    let mut lines: Vec<String> = Vec::new();
    let flags = gpbeam_cli::SafetyFlags::default();
    let failed = gpbeam_cli::run_mirror(&dest, Some(&cfg_path), &flags, &mut |l| lines.push(l))
        .await
        .expect("terminal cloud failure is Ok(n), not Err");

    assert_eq!(
        failed, 1,
        "the seeded job terminally failed; lines: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("[cloud-FAIL]")),
        "expected a cloud-FAIL line, got: {lines:?}"
    );
}
