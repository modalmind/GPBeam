//! Offload from a USB-connected GoPro via the Open GoPro HTTP API. Mirrors the
//! filesystem orchestrator's per-item rules (classify → skip proxies/thumbnails →
//! ledger dedup → naming/collision) but sources media over HTTP and reuses the
//! shared `commit_imported` leaf helper. Emits the existing `RunEvent`s.

use crate::capture::Captured;
use crate::config::{Config, MirrorMode};
use crate::diskguard;
use crate::error::{io_at, CoreError, Result};
use crate::gopro::classify;
use crate::ledger::Ledger;
use crate::naming::{render_name, resolve_collision};
use crate::orchestrator::{RunEvent, RunSummary};
use crate::progress::ProgressThrottle;
use crate::transfer::commit_imported;
use crate::wired::client::{GoProClient, RemoteMedia};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One planned wired download: the source media plus its resolved destination.
#[derive(Debug, Clone, PartialEq)]
struct PlannedWired {
    media: RemoteMedia,
    dest_name: String,
    dest_path: PathBuf,
}

/// `dest_path` + ".part" (the temp file streamed into, then atomically renamed).
fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_os_string();
    p.push(".part");
    PathBuf::from(p)
}

/// Build the per-run work list from a media listing: drop proxies/thumbnails unless the
/// config includes them, skip already-imported files (ledger dedup on serial+name+size+
/// captured_unix), and resolve a collision-free dest name/path per item (collision-aware
/// within this run via `reserved`). Returns (plan, skipped_count). Pure aside from the
/// read-only ledger lookups.
fn plan_wired(
    media: Vec<RemoteMedia>,
    cfg: &Config,
    ledger: &Ledger,
    serial: &str,
    model: Option<&str>,
) -> Result<(Vec<PlannedWired>, usize)> {
    let mut plan = Vec::new();
    let mut skipped = 0usize;
    let mut reserved: HashSet<PathBuf> = HashSet::new();
    for m in media {
        let kind = classify(&m.name);
        if kind.is_proxy() && !cfg.include_proxies {
            skipped += 1;
            continue;
        }
        if kind.is_thumbnail() && !cfg.include_thumbnails {
            skipped += 1;
            continue;
        }
        if ledger.is_imported(serial, &m.name, m.size, m.captured_unix)? {
            skipped += 1;
            continue;
        }
        let cap = Captured::from_unix(m.captured_unix);
        let dest_name = render_name(&cfg.filename_template, &m.name, &cap, Some(serial), model);
        let dest_path = resolve_collision(&cfg.dest_root, &dest_name, &reserved);
        reserved.insert(dest_path.clone());
        plan.push(PlannedWired { media: m, dest_name, dest_path });
    }
    Ok((plan, skipped))
}

/// Offload a USB-connected GoPro (reachable via `client`) into `cfg.dest_root`, reusing the
/// shared verify/ledger/cloud pipeline. Emits `RunEvent`s. Non-destructive unless
/// `cfg.delete_after_verify` is set (then each verified file is deleted from the CAMERA via
/// the API — inline, since the cloud worker uploads the local copy and can't reach the camera).
pub async fn run_wired_offload(
    client: &GoProClient,
    cfg: &Config,
    ledger: &mut Ledger,
    emit: &mut (dyn FnMut(RunEvent) + Send),
) -> Result<RunSummary> {
    let info = client.info().await?;
    let serial = if info.serial.is_empty() { "unknown".to_string() } else { info.serial.clone() };
    let model = (!info.model.is_empty()).then(|| info.model.clone());
    emit(RunEvent::CardDetected { model: model.clone(), serial: Some(serial.clone()) });

    // Best-effort: enable wired control. Non-fatal — many cameras work without it.
    let _ = client.enable_wired_control().await;

    let listing = client.media_list().await?;
    let (plan, skipped) = plan_wired(listing, cfg, ledger, &serial, model.as_deref())?;
    let total_bytes: u64 = plan.iter().map(|p| p.media.size).sum();
    emit(RunEvent::Scanned { new_files: plan.len(), total_bytes });

    // Low-disk guard before downloading anything.
    if !diskguard::has_room(&cfg.dest_root, total_bytes, cfg.space_headroom)? {
        let have = diskguard::available(&cfg.dest_root)?;
        let need = total_bytes.saturating_add(cfg.space_headroom);
        emit(RunEvent::InsufficientSpace { need, have });
        return Err(CoreError::InsufficientSpace { need, have });
    }
    std::fs::create_dir_all(&cfg.dest_root).map_err(io_at(&cfg.dest_root))?;

    let mirror = cfg.cloud.as_ref().map(|c| c.mirror_mode).unwrap_or(MirrorMode::Off);
    let total = plan.len();
    let (mut copied, mut failed, mut bytes, mut queued) = (0usize, 0usize, 0u64, 0usize);

    for (i, p) in plan.iter().enumerate() {
        emit(RunEvent::Copying { file: p.media.name.clone(), index: i + 1, total });
        let part = part_path(&p.dest_path);
        // Forward live, throttled progress. download_resumable calls back with the
        // cumulative on-disk byte count per chunk; ProgressThrottle gates that to ~one
        // Progress per integer-percent (plus a guaranteed terminal tick) so the GUI
        // snapshot channel isn't flooded on multi-GB clips. `copied` is the current
        // file's cumulative count — the reducer adds it to its completed-files base.
        let expected = p.media.size;
        let name = p.media.name.clone();
        let mut throttle = ProgressThrottle::new(expected);
        let outcome = {
            let mut on_progress = |cum: u64| {
                if throttle.should_emit(cum) {
                    emit(RunEvent::Progress { file: name.clone(), copied: cum, total: expected });
                }
            };
            client.download_resumable(&p.media, &part, &mut on_progress).await
        };
        match outcome {
            Ok((nbytes, hash)) => {
                if nbytes != p.media.size {
                    failed += 1;
                    emit(RunEvent::Failed {
                        file: p.media.name.clone(),
                        error: format!("size mismatch: got {nbytes}, expected {}", p.media.size),
                    });
                    // keep the `.part` for a later resume
                    continue;
                }
                std::fs::rename(&part, &p.dest_path).map_err(io_at(&p.dest_path))?;

                // H2: re-read the persisted file and confirm it matches the
                // streamed BLAKE3, exactly like the SD path's copy_verified. This
                // honors cfg.verify (previously dead code on the wired path) and,
                // on a storage write/flush mismatch, removes the file instead of
                // recording a corrupt copy. The camera-delete below is gated on a
                // successful copy, so a failed verify can never erase the only
                // other copy (see M4).
                if let Err(e) = crate::copy::verify_dest_hash(&p.dest_path, &hash, cfg.verify) {
                    failed += 1;
                    emit(RunEvent::Failed { file: p.media.name.clone(), error: e.to_string() });
                    continue;
                }

                bytes += nbytes;
                copied += 1;
                emit(RunEvent::Progress { file: p.media.name.clone(), copied: nbytes, total: nbytes });
                emit(RunEvent::Verified { file: p.media.name.clone() });

                // Record + (for Auto|Manual) enqueue the cloud job. card_src=None: the worker
                // uploads the local copy; it can't delete from the camera.
                commit_imported(
                    ledger, cfg, &serial, &p.media.name, p.media.size, p.media.captured_unix,
                    &p.dest_path, &p.dest_name, Some(&hash), None,
                )?;
                if matches!(mirror, MirrorMode::Auto | MirrorMode::Manual) {
                    queued += 1;
                    emit(RunEvent::CloudQueued { file: p.media.name.clone() });
                }

                // delete-after-verify (opt-in): delete from the camera inline,
                // but only when the mirror mode does NOT defer deletion to the
                // cloud (M4). Under Auto the SD path defers the card delete until
                // the upload reaches Done — the wired worker can't do that (it
                // uploads the LOCAL copy and can't reach the camera), so deferral
                // is impossible. Keeping the camera original is the safe choice:
                // never erase the only other copy before the cloud confirms it.
                // Manual/Off still delete inline after the now-real verify (H2).
                if cfg.delete_after_verify
                    && crate::orchestrator::should_delete_card(true, mirror)
                {
                    match client.delete(&p.media).await {
                        Ok(()) => emit(RunEvent::CardFileDeleted { file: p.media.name.clone() }),
                        Err(e) => emit(RunEvent::Failed {
                            file: p.media.name.clone(),
                            error: format!("delete-after-verify failed: {e}"),
                        }),
                    }
                }
            }
            Err(e) => {
                failed += 1;
                emit(RunEvent::Failed { file: p.media.name.clone(), error: e.to_string() });
                // `.part` retained on disk -> resumes via Range on the next run.
            }
        }
    }

    let summary = RunSummary { copied, skipped, failed, bytes, queued };
    emit(RunEvent::RunComplete { copied, skipped, failed, bytes });
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::RunEvent;
    use crate::wired::client::GoProClient;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn media(name: &str, size: u64, cre: i64) -> RemoteMedia {
        RemoteMedia { dir: "100GOPRO".into(), name: name.into(), size, captured_unix: cre }
    }

    // Serve the minimal Open GoPro surface run_wired_offload needs against a mock server.
    async fn mock_camera(server: &MockServer, files: &[(&str, &[u8], i64)]) {
        // info() parses TOP-LEVEL keys (Phase 2: model_name/serial_number/firmware_version).
        Mock::given(method("GET")).and(path("/gopro/camera/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_name": "MISSION 1 PRO", "serial_number": "C3575424520622", "firmware_version": "H26.01"
            })))
            .mount(server).await;
        Mock::given(method("GET")).and(path_regex(r"^/gopro/camera/control/wired_usb"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(server).await;
        let fs: Vec<_> = files.iter().map(|(n, b, cre)| serde_json::json!({
            "n": n, "s": b.len().to_string(), "cre": cre.to_string(), "mod": cre.to_string()
        })).collect();
        Mock::given(method("GET")).and(path("/gopro/media/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "1", "media": [ { "d": "100GOPRO", "fs": fs } ]
            })))
            .mount(server).await;
        for (n, b, _) in files {
            Mock::given(method("GET")).and(path(format!("/videos/DCIM/100GOPRO/{n}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b.to_vec()))
                .mount(server).await;
        }
    }

    #[tokio::test]
    async fn happy_path_offloads_new_files_and_records_them() {
        let server = MockServer::start().await;
        mock_camera(&server, &[
            ("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910),
            ("GX010199.MP4", &vec![9u8; 8192], 1_780_515_924),
        ]).await;

        let dest = tempfile::tempdir().unwrap();
        let cfg = Config::new(dest.path().to_path_buf()); // verify on, no cloud, delete off
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        let summary = run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();

        assert_eq!(summary.copied, 2);
        assert_eq!(summary.failed, 0);
        assert!(events.iter().any(|e| matches!(e, RunEvent::CardDetected { serial: Some(s), .. } if s == "C3575424520622")));
        assert!(events.iter().filter(|e| matches!(e, RunEvent::Verified { .. })).count() == 2);
        assert!(events.iter().any(|e| matches!(e, RunEvent::RunComplete { copied: 2, .. })));
        // both files landed at the dest with the right sizes
        let landed: Vec<_> = std::fs::read_dir(dest.path()).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(landed.len(), 2);
        // ledger now dedups them
        assert!(ledger.is_imported("C3575424520622", "GX010198.MP4", 4096, 1_780_515_910).unwrap());
    }

    #[tokio::test]
    async fn second_run_is_idempotent() {
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        let dest = tempfile::tempdir().unwrap();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let s1 = run_wired_offload(&client, &cfg, &mut ledger, &mut |_| {}).await.unwrap();
        assert_eq!(s1.copied, 1);
        let s2 = run_wired_offload(&client, &cfg, &mut ledger, &mut |_| {}).await.unwrap();
        assert_eq!(s2.copied, 0, "already imported -> nothing new");
        assert_eq!(s2.skipped, 1);
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 1, "no duplicate file");
    }

    #[tokio::test]
    async fn size_mismatch_fails_and_keeps_part() {
        // media_list reports 4096 bytes but the download serves only 10 -> size mismatch.
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/gopro/camera/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_name": "M", "serial_number": "C357", "firmware_version": "1" })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path_regex(r"^/gopro/camera/control/wired_usb"))
            .respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("GET")).and(path("/gopro/media/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": [ { "d": "100GOPRO", "fs": [ { "n": "GX010198.MP4", "s": "4096", "cre": "1", "mod": "1" } ] } ] })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/videos/DCIM/100GOPRO/GX010198.MP4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 10]))
            .mount(&server).await;

        let dest = tempfile::tempdir().unwrap();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        let s = run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(s.copied, 0);
        assert_eq!(s.failed, 1);
        assert!(events.iter().any(|e| matches!(e, RunEvent::Failed { .. })));
        // final file NOT present; a `.part` remains for resume
        assert!(!ledger.is_imported("C357", "GX010198.MP4", 4096, 1).unwrap());
        let names: Vec<String> = std::fs::read_dir(dest.path()).unwrap()
            .filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.ends_with(".part")), "a .part should remain: {names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".MP4") && !n.ends_with(".part")));
    }

    #[test]
    fn plan_skips_proxies_thumbnails_dedups_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(dir.path().to_path_buf());
        cfg.filename_template = "{date}_{original}".into(); // default
        let mut ledger = Ledger::open_in_memory().unwrap();
        // Pretend GX010196.MP4 was already imported (serial+name+size+cre dedup key).
        ledger
            .record("C357", "GX010196.MP4", 100, 1_780_334_487, "/old", None)
            .unwrap();

        let listing = vec![
            media("GX010196.MP4", 100, 1_780_334_487), // already imported -> skip
            media("GX010198.MP4", 684_588_850, 1_780_515_910), // new video -> plan
            media("GX010198.LRV", 5_251_966, 1_780_515_910),   // proxy -> skip (default)
            media("GX010198.THM", 12_345, 1_780_515_910),       // thumbnail -> skip (default)
        ];

        let (plan, skipped) = plan_wired(listing, &cfg, &ledger, "C357", Some("MISSION 1 PRO")).unwrap();
        assert_eq!(skipped, 3, "1 dedup + 1 proxy + 1 thumbnail");
        assert_eq!(plan.len(), 1);
        let p = &plan[0];
        assert_eq!(p.media.name, "GX010198.MP4");
        // {date}_{original}; date derived from cre via Captured::from_unix (local tz) — assert shape.
        assert!(p.dest_name.ends_with("_GX010198.MP4"), "got {}", p.dest_name);
        assert_eq!(p.dest_path, dir.path().join(&p.dest_name));
    }

    #[test]
    fn plan_includes_proxies_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(dir.path().to_path_buf());
        cfg.include_proxies = true;
        let ledger = Ledger::open_in_memory().unwrap();
        let (plan, skipped) =
            plan_wired(vec![media("GX010198.LRV", 10, 1)], &cfg, &ledger, "C357", None).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(plan.len(), 1);
    }

    use crate::config::{CloudConfig, CloudKind};

    fn cloud_cfg(dest: std::path::PathBuf, mode: MirrorMode) -> Config {
        let mut cfg = Config::new(dest);
        cfg.cloud = Some(CloudConfig {
            kind: CloudKind::Nextcloud, destination_id: "nc1".into(),
            base_url: "https://nc.example".into(), username: "alice".into(),
            remote_root: "GoPro".into(), mirror_mode: mode, chunk_threshold: 50 * 1024 * 1024,
            tls_ca_pem: None, max_concurrency: 2, max_attempts: 8,
        });
        cfg
    }

    #[tokio::test]
    async fn auto_mirror_enqueues_one_cloud_job_per_file() {
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        let dest = tempfile::tempdir().unwrap();
        let cfg = cloud_cfg(dest.path().to_path_buf(), MirrorMode::Auto);
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        let s = run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(s.copied, 1);
        assert_eq!(s.queued, 1);
        assert_eq!(ledger.pending_cloud_count().unwrap(), 1);
        assert!(events.iter().any(|e| matches!(e, RunEvent::CloudQueued { .. })));
    }

    #[tokio::test]
    async fn delete_after_verify_calls_camera_delete() {
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        // Expect exactly one delete call for the file.
        Mock::given(method("GET")).and(path("/gopro/media/delete"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server).await;

        let dest = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.delete_after_verify = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();
        assert!(events.iter().any(|e| matches!(e, RunEvent::CardFileDeleted { .. })));
        // `server` drop verifies the `.expect(1)` on the delete mock.
    }

    #[tokio::test]
    async fn delete_after_verify_skips_camera_delete_under_auto_mirror() {
        // M4: under Auto mirror the wired worker uploads the LOCAL copy and
        // cannot reach the camera to defer the delete, so the camera original
        // must be KEPT — never erased before the cloud confirms it. The delete
        // endpoint must not be hit; the file is still queued for mirroring.
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        Mock::given(method("GET")).and(path("/gopro/media/delete"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server).await;

        let dest = tempfile::tempdir().unwrap();
        let mut cfg = cloud_cfg(dest.path().to_path_buf(), MirrorMode::Auto);
        cfg.delete_after_verify = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        let s = run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(s.copied, 1);
        assert_eq!(s.queued, 1, "still enqueued for the cloud mirror");
        assert!(
            !events.iter().any(|e| matches!(e, RunEvent::CardFileDeleted { .. })),
            "no camera delete under Auto+wired"
        );
        // `server` drop verifies the `.expect(0)` on the delete mock.
    }

    #[tokio::test]
    async fn delete_after_verify_deletes_under_manual_mirror() {
        // Contrast to Auto: under Manual mirror there is no deferral expectation,
        // so the inline camera delete still happens after the (now real) verify.
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        Mock::given(method("GET")).and(path("/gopro/media/delete"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server).await;

        let dest = tempfile::tempdir().unwrap();
        let mut cfg = cloud_cfg(dest.path().to_path_buf(), MirrorMode::Manual);
        cfg.delete_after_verify = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();
        assert!(events.iter().any(|e| matches!(e, RunEvent::CardFileDeleted { .. })));
    }

    #[tokio::test]
    async fn emits_live_progress_during_download() {
        // Regression guard: the per-file loop must FORWARD download_resumable's
        // cumulative progress (not pass a no-op). The throttle always fires the
        // terminal tick and the loop also emits a post-completion Progress, so a
        // single file yields >= 2 Progress events; the old no-op path emitted one.
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        let dest = tempfile::tempdir().unwrap();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await.unwrap();

        let copied_seq: Vec<u64> = events.iter().filter_map(|e| match e {
            RunEvent::Progress { copied, .. } => Some(*copied),
            _ => None,
        }).collect();
        assert!(copied_seq.len() >= 2, "expected live + post-completion Progress, got {copied_seq:?}");
        assert!(copied_seq.iter().all(|c| *c <= 4096), "cumulative within file size: {copied_seq:?}");
        assert!(copied_seq.contains(&4096), "a Progress reaches the file size");
        assert!(copied_seq.windows(2).all(|w| w[0] <= w[1]), "cumulative copied is monotonic: {copied_seq:?}");
    }

    #[tokio::test]
    async fn insufficient_space_aborts_before_download() {
        let server = MockServer::start().await;
        mock_camera(&server, &[("GX010198.MP4", &vec![7u8; 4096], 1_780_515_910)]).await;
        let dest = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.space_headroom = u64::MAX - 1; // force the guard to fail
        let mut ledger = Ledger::open_in_memory().unwrap();
        let client = GoProClient::with_base(server.uri());

        let mut events = Vec::new();
        let err = run_wired_offload(&client, &cfg, &mut ledger, &mut |e| events.push(e)).await;
        assert!(matches!(err, Err(CoreError::InsufficientSpace { .. })));
        assert!(events.iter().any(|e| matches!(e, RunEvent::InsufficientSpace { .. })));
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0, "nothing downloaded");
    }
}
