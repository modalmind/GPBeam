//! `run_mirror` flushes the cloud queue on demand, irrespective of `mirror_mode`.
//!
//! This closes the Correction-G3 gap: `MirrorMode::Manual` enqueues jobs (via the
//! orchestrator) with no auto-drain, so without a `mirror` command those jobs would
//! never upload. Here we seed a Queued job under a `manual`-mode config, run
//! `run_mirror`, and assert the job reaches `Done` (real flush, not a mock).

use gpbeam_core::ledger::{JobState, Ledger};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn run_mirror_flushes_manual_queued_job_to_done() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    // A real local file for the uploader to PUT.
    let local = dest.join("GX010001.MP4");
    std::fs::write(&local, b"hello gopro media").unwrap();
    let total = std::fs::metadata(&local).unwrap().len();

    // Seed: one imported row + one Queued cloud job at the SAME ledger path
    // `run_mirror` will open (so it picks the job up).
    let lpath = gpbeam_cli::ledger_path_for(&dest);
    let job_id = {
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
            .unwrap()
    };

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

    // gpbeam.toml: cloud config in MANUAL mode (no auto-drain) + inline creds.
    let toml = format!(
        r#"
[cloud]
kind = "nextcloud"
destination_id = "home-nc"
base_url = "{base}"
username = "alice"
remote_root = "GoProBackup"
mirror_mode = "manual"

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
    gpbeam_cli::run_mirror(&dest, Some(&cfg_path), &flags, &mut |l| lines.push(l))
        .await
        .expect("on-demand mirror flush ok");

    assert!(
        lines.iter().any(|l| l.contains("[mirrored]")),
        "expected a mirrored line, got: {lines:?}"
    );

    // Real flush: the seeded Queued job is now Done in the persisted ledger.
    let ledger = Ledger::open(&lpath).unwrap();
    let done = ledger.list_cloud_jobs(Some(JobState::Done)).unwrap();
    assert_eq!(done.len(), 1, "exactly one job reached Done: {done:?}");
    assert_eq!(done[0].id, job_id, "the seeded job is the one that flushed");
    assert_eq!(ledger.pending_cloud_count().unwrap(), 0, "nothing left pending");
}
