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

#[tokio::test]
async fn run_offload_and_mirror_uploads_one_file() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    make_card(&card);

    // Mock Nextcloud: PROPFIND -> 404 (absent), PUT -> 201 (created).
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path_regex(r"^/remote\.php/dav/files/.+$"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/remote\.php/dav/files/.+$"))
        .respond_with(ResponseTemplate::new(201).insert_header("oc-etag", "\"abc123\""))
        .expect(1)
        .mount(&server)
        .await;

    // gpbeam.toml: dest, cloud config (Auto), and inline credentials.
    let toml = format!(
        r#"
[cloud]
kind = "nextcloud"
destination_id = "home-nc"
base_url = "{base}"
username = "alice"
remote_root = "GoProBackup"
mirror_mode = "auto"

[credentials.home-nc]
username = "alice"
app_password = "test-app-pw"
"#,
        base = server.uri()
    );
    let cfg_path = tmp.path().join("gpbeam.toml");
    std::fs::write(&cfg_path, toml).unwrap();

    let mut lines: Vec<String> = Vec::new();
    let flags = gpbeam_cli::SafetyFlags::default();
    let failed =
        gpbeam_cli::run_offload_and_mirror(&card, &dest, Some(&cfg_path), &flags, &mut |l| {
            lines.push(l)
        })
        .await
        .expect("offload+mirror ok");
    assert_eq!(failed, 0, "fully-clean run reports zero failures");

    assert!(
        lines.iter().any(|l| l.contains("[cloud-queued]")),
        "expected a cloud-queued line, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("[mirrored]")),
        "expected a mirrored line, got: {lines:?}"
    );
}
