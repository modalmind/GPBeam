use crate::config::{Config, MirrorMode};
use crate::copy::copy_verified;
use crate::diskguard;
use crate::progress::ProgressThrottle;
use crate::eject::{default_ejector, Ejector};
use crate::error::{CoreError, Result};
use crate::gopro::{is_gopro_card, model_family, read_version};
use crate::ledger::Ledger;
use crate::scanner::scan_with_skips;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    NotGoPro(std::path::PathBuf),
    CardDetected { model: Option<String>, serial: Option<String> },
    Scanned { new_files: usize, total_bytes: u64 },
    InsufficientSpace { need: u64, have: u64 },
    Copying { file: String, index: usize, total: usize },
    /// `copied` is the CUMULATIVE bytes for the current file (not a delta); `total`
    /// is the current file's expected size. Emitted live during a transfer.
    Progress { file: String, copied: u64, total: u64 },
    Verified { file: String },
    Skipped { file: String },
    Failed { file: String, error: String },
    CloudQueued { file: String },
    CardFileDeleted { file: String },
    Ejected { mount: String },
    RunComplete { copied: usize, skipped: usize, failed: usize, bytes: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary { pub copied: usize, pub skipped: usize, pub failed: usize, pub bytes: u64, pub queued: usize }

/// Join the configured `remote_root` with a destination filename to form a
/// remote-relative path. Always uses '/' (WebDAV remote paths use '/' regardless
/// of host OS) and trims redundant slashes between the two parts.
pub(crate) fn remote_path_for(remote_root: &str, dest_name: &str) -> String {
    let root = remote_root.trim_end_matches('/');
    let name = dest_name.trim_start_matches('/');
    if root.is_empty() {
        name.to_string()
    } else {
        format!("{root}/{name}")
    }
}

/// Whether the orchestrator should delete the card source file *inline* right
/// after a local verify. True only when the file verified locally AND the
/// mirror mode is not `Auto` — under `Auto`, deletion is deferred to the cloud
/// worker once the upload reaches `Done`.
pub fn should_delete_card(local_verified: bool, mirror: MirrorMode) -> bool {
    local_verified && mirror != MirrorMode::Auto
}

/// Whether the card should be auto-ejected after a run. Opt-in (`auto_eject`),
/// but suppressed when `delete_after_verify && mirror == Auto`: in that combo
/// the cloud worker still needs the card mounted to delete each source file
/// after its upload reaches `Done`, so ejecting here would break it (M2
/// limitation — see Contract G4).
pub fn should_auto_eject(auto_eject: bool, delete_after_verify: bool, mirror: MirrorMode) -> bool {
    auto_eject && !(delete_after_verify && mirror == MirrorMode::Auto)
}

/// Run one offload pass for a mounted volume `card_root` into `cfg.dest_root`,
/// using the platform default ejector for `auto_eject`.
///
/// Emits `RunEvent`s for UI/CLI/notification consumers. Non-destructive to the
/// card's media unless the opt-in `delete_after_verify`/`auto_eject` flags are
/// set. Idempotent: ledger dedup prevents re-copying.
pub fn run_offload(
    card_root: &Path,
    cfg: &Config,
    ledger: &mut Ledger,
    emit: &mut dyn FnMut(RunEvent),
) -> Result<RunSummary> {
    let ejector = default_ejector();
    run_offload_with_ejector(card_root, cfg, ledger, ejector.as_ref(), emit)
}

/// Like `run_offload`, but with an injected `Ejector` (for tests / custom seams).
pub fn run_offload_with_ejector(
    card_root: &Path,
    cfg: &Config,
    ledger: &mut Ledger,
    ejector: &dyn Ejector,
    emit: &mut dyn FnMut(RunEvent),
) -> Result<RunSummary> {
    if !is_gopro_card(card_root) {
        emit(RunEvent::NotGoPro(card_root.to_path_buf()));
        return Ok(RunSummary { copied: 0, skipped: 0, failed: 0, bytes: 0, queued: 0 });
    }

    let version = read_version(card_root);
    let serial = version.as_ref().map(|v| v.camera_serial_number.clone())
        .filter(|s| !s.is_empty());
    let model = version.as_ref()
        .and_then(|v| model_family(&v.firmware_version))
        .map(|m| m.to_string());
    emit(RunEvent::CardDetected { model: model.clone(), serial: serial.clone() });

    let (plan, skipped) = scan_with_skips(card_root, cfg, ledger, serial.as_deref(), model.as_deref())?;
    let total_bytes: u64 = plan.iter().map(|p| p.size).sum();
    emit(RunEvent::Scanned { new_files: plan.len(), total_bytes });

    // Free-space guard before any copy.
    let needed = total_bytes.saturating_add(cfg.space_headroom);
    if !diskguard::has_room(&cfg.dest_root, total_bytes, cfg.space_headroom)? {
        let have = diskguard::available(&cfg.dest_root)?;
        emit(RunEvent::InsufficientSpace { need: needed, have });
        return Err(CoreError::InsufficientSpace { need: needed, have });
    }

    // L1: when `version.txt` has no serial, the dedup key degrades to the
    // literal "unknown". Two DIFFERENT serial-less cameras with a same-named,
    // same-sized, same-second-mtime file then collide on this key, so the
    // second mount's file is treated as already-imported and silently skipped.
    // Very low probability today (one camera at a time); a future multi-camera
    // discriminator (content/inode, or refusing to dedupe under "unknown")
    // should be a conscious behavior change, not an accident. See [`Ledger`].
    let serial_key = serial.as_deref().unwrap_or("unknown");
    let total = plan.len();
    let mut copied = 0usize;
    let mut failed = 0usize;
    let mut bytes = 0u64;
    let mut skipped_recovered = 0usize;
    let mut queued = 0usize;

    for (i, item) in plan.iter().enumerate() {
        // Record-or-recover: a verified dest file may already exist from a prior
        // run that crashed after copy+verify but before record() committed. The
        // scanner, seeing that on-disk file, has already bumped `dest_path` to a
        // `_1` collision name; the original verified file sits at the un-bumped
        // `canonical_dest_path`. If that file is present with the expected byte
        // length and the ledger has no matching row, adopt it (record) rather than
        // re-copying under the `_1` suffix.
        if !ledger.is_imported(serial_key, &item.name, item.size, item.mtime_unix)? {
            if let Ok(meta) = std::fs::metadata(&item.canonical_dest_path) {
                if meta.is_file() && meta.len() == item.size {
                    ledger.record(
                        serial_key,
                        &item.name,
                        item.size,
                        item.mtime_unix,
                        &item.canonical_dest_path.to_string_lossy(),
                        None,
                    )?;
                    skipped_recovered += 1;
                    emit(RunEvent::Skipped { file: item.name.clone() });
                    continue;
                }
            }
        }

        emit(RunEvent::Copying { file: item.name.clone(), index: i + 1, total });
        // Forward live, throttled progress. copy_verified calls back with the
        // cumulative bytes copied per 1 MiB read; ProgressThrottle gates that to ~one
        // Progress per integer-percent (plus a guaranteed terminal tick). `copied` is
        // the current file's cumulative count — the reducer adds it to its
        // completed-files base. A post-completion Progress + Verified still follow.
        let expected = item.size;
        let name = item.name.clone();
        let mut throttle = ProgressThrottle::new(expected);
        let copy_result = {
            let mut on_progress = |cum: u64| {
                if throttle.should_emit(cum) {
                    emit(RunEvent::Progress { file: name.clone(), copied: cum, total: expected });
                }
            };
            copy_verified(&item.src, &item.dest_path, cfg.verify, &mut on_progress)
        };
        match copy_result {
            Ok(out) => {
                let n = out.bytes;
                // Shared commit: record the import + (for Auto|Manual) enqueue a
                // cloud job. card_src is the on-card source so the worker can delete
                // it after a verified upload (Auto + delete-after-verify).
                let src_str = item.src.to_string_lossy();
                let imported_id = crate::transfer::commit_imported(
                    ledger,
                    cfg,
                    serial_key,
                    &item.name,
                    item.size,
                    item.mtime_unix,
                    &item.dest_path,
                    &item.dest_name,
                    out.hash.as_deref(),
                    Some(&src_str),
                )?;
                let _ = imported_id; // returned id not needed inline (kept for parity)
                bytes += n;
                copied += 1;
                emit(RunEvent::Progress { file: item.name.clone(), copied: n, total: n });
                emit(RunEvent::Verified { file: item.name.clone() });

                // Cloud mirror: the job was enqueued inside commit_imported for
                // Auto|Manual; emit the matching CloudQueued event + bump the
                // counter here (the helper is event-free by design — Contract G3).
                let mirror = cfg
                    .cloud
                    .as_ref()
                    .map(|c| c.mirror_mode)
                    .unwrap_or(MirrorMode::Off);
                if cfg.cloud.is_some() && matches!(mirror, MirrorMode::Auto | MirrorMode::Manual)
                {
                    queued += 1;
                    emit(RunEvent::CloudQueued { file: item.name.clone() });
                }

                // delete-after-verify (opt-in, default OFF): once the file is
                // verified locally, delete the card SOURCE for the non-Auto path.
                // Under Auto, deletion is deferred to the cloud worker after the
                // upload reaches Done (the card_src is retained on the queued job).
                if cfg.delete_after_verify && should_delete_card(true, mirror) {
                    match std::fs::remove_file(&item.src) {
                        Ok(()) => emit(RunEvent::CardFileDeleted { file: item.name.clone() }),
                        Err(e) => emit(RunEvent::Failed {
                            file: item.name.clone(),
                            error: format!("delete-after-verify failed: {e}"),
                        }),
                    }
                }
            }
            Err(e) => {
                failed += 1;
                emit(RunEvent::Failed { file: item.name.clone(), error: e.to_string() });
            }
        }
    }

    let skipped = skipped + skipped_recovered;
    let summary = RunSummary { copied, skipped, failed, bytes, queued };
    emit(RunEvent::RunComplete { copied, skipped, failed, bytes });

    // Auto-eject (opt-in, default OFF; sync-path only — Contract G4). Gated by
    // `should_auto_eject`: suppressed for the `delete_after_verify && Auto`
    // combo, where the worker still needs the card mounted. The non-GoPro early
    // return above means a non-GoPro volume is never ejected.
    let mirror = cfg
        .cloud
        .as_ref()
        .map(|c| c.mirror_mode)
        .unwrap_or(MirrorMode::Off);
    if should_auto_eject(cfg.auto_eject, cfg.delete_after_verify, mirror) {
        match ejector.eject(card_root) {
            Ok(()) => emit(RunEvent::Ejected { mount: card_root.to_string_lossy().into_owned() }),
            Err(e) => emit(RunEvent::Failed {
                file: card_root.to_string_lossy().into_owned(),
                error: format!("auto-eject failed: {e}"),
            }),
        }
    }

    Ok(summary)
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/fixtures.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_gopro_emits_event_and_returns_zero() {
        let card = fixtures::not_a_gopro();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();
        assert_eq!(summary.copied, 0);
        assert!(matches!(events.first(), Some(RunEvent::NotGoPro(_))));
    }

    #[test]
    fn copies_all_new_media_and_is_idempotent() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();

        // First run: copies 4 files (2 MP4 + 1 JPG + 1 .360; proxies/thumbs skipped).
        let mut events = Vec::new();
        let s1 = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();
        assert_eq!(s1.copied, 4);
        assert_eq!(s1.failed, 0);
        assert!(events.iter().any(|e| matches!(e, RunEvent::CardDetected { .. })));
        assert!(events.iter().filter(|e| matches!(e, RunEvent::Verified { .. })).count() == 4);
        // files exist at destination
        let copied: Vec<_> = std::fs::read_dir(dest.path()).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(copied.len(), 4);

        // Second run: everything already in ledger -> 0 copied, 4 skipped.
        let s2 = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
        assert_eq!(s2.copied, 0);
        assert_eq!(s2.skipped, 4);
        // no duplicate files created
        let after: Vec<_> = std::fs::read_dir(dest.path()).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(after.len(), 4);
    }

    #[test]
    fn insufficient_space_aborts_before_copying() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.space_headroom = u64::MAX - 1; // force the guard to fail
        let mut ledger = Ledger::open_in_memory().unwrap();
        let mut events = Vec::new();
        let err = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e));
        assert!(matches!(err, Err(CoreError::InsufficientSpace { .. })));
        assert!(events.iter().any(|e| matches!(e, RunEvent::InsufficientSpace { .. })));
        // nothing copied
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[test]
    fn second_run_reports_skipped_in_summary() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
        let s2 = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
        assert_eq!(s2.skipped, 4);
        assert_eq!(s2.copied, 0);
    }

    #[test]
    fn emits_live_progress_during_copy() {
        // Regression guard: the per-file loop must FORWARD copy_verified's cumulative
        // progress (not pass a no-op). copy_verified reads in 1 MiB chunks, so a 5 MiB
        // clip yields several mid-file Progress ticks (copied < size) plus a terminal
        // one — the old no-op path emitted a single Progress at the file size.
        let card = fixtures::card_with_one_clip(5 * 1024 * 1024);
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();

        let mut events = Vec::new();
        run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        let size = 5 * 1024 * 1024u64;
        let copied_seq: Vec<u64> = events.iter().filter_map(|e| match e {
            RunEvent::Progress { copied, .. } => Some(*copied),
            _ => None,
        }).collect();
        assert!(copied_seq.len() >= 3, "expected several streaming Progress ticks, got {copied_seq:?}");
        assert!(copied_seq.iter().any(|c| *c < size), "at least one mid-file tick (copied < size)");
        assert!(copied_seq.contains(&size), "a Progress reaches the file size");
        assert!(copied_seq.windows(2).all(|w| w[0] <= w[1]), "cumulative copied is monotonic: {copied_seq:?}");
    }

    fn cloud_cfg(dest: std::path::PathBuf, mode: crate::config::MirrorMode) -> Config {
        use crate::config::{CloudConfig, CloudKind};
        let mut cfg = Config::new(dest);
        cfg.cloud = Some(CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "nc1".into(),
            base_url: "https://nc.example".into(),
            username: "alice".into(),
            remote_root: "GoPro".into(),
            mirror_mode: mode,
            chunk_threshold: 50 * 1024 * 1024,
            tls_ca_pem: None,
            max_concurrency: 2,
            max_attempts: 8,
        });
        cfg
    }

    #[test]
    fn auto_mirror_enqueues_one_job_per_copied_file() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Auto);
        let mut ledger = Ledger::open_in_memory().unwrap();

        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert_eq!(summary.copied, 4);
        assert_eq!(summary.queued, 4, "one cloud job queued per copied file");

        let queued_events = events
            .iter()
            .filter(|e| matches!(e, RunEvent::CloudQueued { .. }))
            .count();
        assert_eq!(queued_events, 4);

        assert_eq!(ledger.pending_cloud_count().unwrap(), 4);

        // remote_path == remote_root + "/" + dest_name (forward-slash join).
        let jobs = ledger.list_cloud_jobs(None).unwrap();
        assert!(jobs.iter().all(|j| j.remote_path.starts_with("GoPro/")));
        assert!(jobs.iter().all(|j| j.destination_id == "nc1"));
        // card_src is the on-card source path (so the worker can delete after upload).
        assert!(jobs
            .iter()
            .all(|j| j.card_src.as_deref().is_some_and(|s| s.contains("DCIM"))));
    }

    #[test]
    fn copied_file_imported_row_matches_its_cloud_job() {
        // Pins the commit_imported wiring: the enqueued cloud job's imported_id
        // must equal the imported row id for that same (serial,name,size,mtime).
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Auto);
        let mut ledger = Ledger::open_in_memory().unwrap();

        run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();

        let jobs = ledger.list_cloud_jobs(None).unwrap();
        assert_eq!(jobs.len(), 4);
        for j in &jobs {
            // Each job's imported_id is a real, present imported row.
            assert!(j.imported_id > 0);
        }
        // The four jobs reference four distinct imported rows.
        let mut ids: Vec<i64> = jobs.iter().map(|j| j.imported_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 4, "one distinct imported row per copied file");
    }

    #[test]
    fn manual_mirror_also_enqueues_jobs() {
        // Contract G3: run_offload enqueues for BOTH Auto and Manual.
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Manual);
        let mut ledger = Ledger::open_in_memory().unwrap();

        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert_eq!(summary.copied, 4);
        assert_eq!(summary.queued, 4, "Manual mirror queues jobs too");
        assert_eq!(ledger.pending_cloud_count().unwrap(), 4);
    }

    #[test]
    fn off_mirror_enqueues_nothing() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Off);
        let mut ledger = Ledger::open_in_memory().unwrap();

        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert_eq!(summary.copied, 4);
        assert_eq!(summary.queued, 0);
        assert!(!events.iter().any(|e| matches!(e, RunEvent::CloudQueued { .. })));
        assert_eq!(ledger.pending_cloud_count().unwrap(), 0);
    }

    #[test]
    fn no_cloud_config_queues_nothing_m1_unchanged() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf()); // cloud = None
        let mut ledger = Ledger::open_in_memory().unwrap();

        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert_eq!(summary.copied, 4);
        assert_eq!(summary.queued, 0);
        assert!(!events.iter().any(|e| matches!(e, RunEvent::CloudQueued { .. })));
        assert_eq!(ledger.pending_cloud_count().unwrap(), 0);
    }

    #[test]
    fn should_delete_card_truth_table() {
        use crate::config::MirrorMode::{Auto, Manual, Off};
        // Verified locally + not Auto -> delete inline.
        assert!(should_delete_card(true, Off));
        assert!(should_delete_card(true, Manual));
        // Verified locally + Auto -> defer to the worker (no inline delete).
        assert!(!should_delete_card(true, Auto));
        // Not verified -> never delete, regardless of mirror.
        assert!(!should_delete_card(false, Off));
        assert!(!should_delete_card(false, Manual));
        assert!(!should_delete_card(false, Auto));
    }

    /// Copy a fixture card into a fresh writable tempdir so the test may delete
    /// its "card source" files without touching the original fixture tempdir.
    fn writable_card_copy(src: &std::path::Path) -> tempfile::TempDir {
        let dst = tempfile::TempDir::new().unwrap();
        copy_tree(src, dst.path());
        dst
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    #[test]
    fn delete_after_verify_off_mirror_removes_card_source() {
        let fixture = fixtures::hero11_card();
        let card = writable_card_copy(fixture.root());
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.delete_after_verify = true; // mirror Off (cloud == None)
        let mut ledger = Ledger::open_in_memory().unwrap();

        let media = card.path().join("DCIM/100GOPRO/GX010001.MP4");
        assert!(media.exists());

        let mut events = Vec::new();
        let summary = run_offload(card.path(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert_eq!(summary.copied, 4);
        assert!(!media.exists(), "card source deleted after local verify");
        assert!(events.iter().any(|e| matches!(e, RunEvent::CardFileDeleted { .. })));
    }

    #[test]
    fn delete_after_verify_auto_mirror_keeps_card_source() {
        let fixture = fixtures::hero11_card();
        let card = writable_card_copy(fixture.root());
        let dest = fixtures::dest();
        let mut cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Auto);
        cfg.delete_after_verify = true; // but mirror == Auto -> defer to worker
        let mut ledger = Ledger::open_in_memory().unwrap();

        let media = card.path().join("DCIM/100GOPRO/GX010001.MP4");
        let mut events = Vec::new();
        run_offload(card.path(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        assert!(media.exists(), "Auto mirror must NOT delete inline; worker deletes after Done");
        assert!(!events.iter().any(|e| matches!(e, RunEvent::CardFileDeleted { .. })));
        // The card_src is recorded on the queued job for the worker to delete later.
        let jobs = ledger.list_cloud_jobs(None).unwrap();
        assert!(jobs.iter().all(|j| j.card_src.is_some()));
    }

    #[test]
    fn delete_after_verify_off_when_flag_unset() {
        let fixture = fixtures::hero11_card();
        let card = writable_card_copy(fixture.root());
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf()); // delete_after_verify = false
        let mut ledger = Ledger::open_in_memory().unwrap();

        let media = card.path().join("DCIM/100GOPRO/GX010001.MP4");
        run_offload(card.path(), &cfg, &mut ledger, &mut |_| {}).unwrap();
        assert!(media.exists(), "deletion is opt-in; flag off keeps card source");
    }

    #[test]
    fn unrecorded_verified_dest_is_adopted_not_recopied() {
        use std::time::{Duration, UNIX_EPOCH};

        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();

        // Plan the copies to learn the exact dest paths + bytes the scanner expects.
        // Use the same serial/model `run_offload` derives from version.txt so the
        // dedup key and resolved dest paths line up exactly.
        let plan = crate::scanner::scan_card(
            card.root(),
            &cfg,
            &ledger,
            Some("C3461324500001"),
            Some("HERO11"),
        )
        .unwrap();
        assert!(!plan.is_empty(), "fixture must produce a non-empty plan");

        // Pre-place every planned file at its dest path with identical bytes + mtime,
        // simulating a previous run that copied+verified but crashed before record().
        for item in &plan {
            std::fs::copy(&item.src, &item.dest_path).unwrap();
            let mtime = UNIX_EPOCH + Duration::from_secs(item.mtime_unix as u64);
            filetime::set_file_mtime(
                &item.dest_path,
                filetime::FileTime::from_system_time(mtime),
            )
            .unwrap();
        }
        let before: usize = std::fs::read_dir(dest.path()).unwrap().count();

        let mut events = Vec::new();
        let summary = run_offload(card.root(), &cfg, &mut ledger, &mut |e| events.push(e)).unwrap();

        // Adopted, not re-copied: 0 fresh copies, and the recovered files are recorded.
        assert_eq!(summary.copied, 0, "files already on disk must be adopted, not re-copied");
        assert_eq!(summary.failed, 0);
        assert!(summary.skipped >= plan.len());

        // No `_1` collision-suffixed duplicates were created.
        let names: Vec<String> = std::fs::read_dir(dest.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains("_1.")), "no collision-suffixed copies: {names:?}");
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), before, "no new files on disk");

        // Idempotent on a second run too.
        let s2 = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
        assert_eq!(s2.copied, 0);
    }

    use crate::eject::Ejector;
    use std::path::Path as StdPath;
    use std::sync::Mutex;

    struct RecordingEjector {
        calls: Mutex<Vec<std::path::PathBuf>>,
    }
    impl RecordingEjector {
        fn new() -> Self { RecordingEjector { calls: Mutex::new(Vec::new()) } }
    }
    impl Ejector for RecordingEjector {
        fn eject(&self, mount: &StdPath) -> Result<()> {
            self.calls.lock().unwrap().push(mount.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn should_auto_eject_truth_table() {
        use crate::config::MirrorMode::{Auto, Manual, Off};
        // Flag off -> never eject, whatever the rest.
        assert!(!should_auto_eject(false, false, Off));
        assert!(!should_auto_eject(false, true, Auto));
        // Flag on, no delete-after-verify -> eject for every mirror mode.
        assert!(should_auto_eject(true, false, Off));
        assert!(should_auto_eject(true, false, Manual));
        assert!(should_auto_eject(true, false, Auto));
        // Flag on + delete-after-verify: only the Auto combo is suppressed
        // (worker still needs the card); Off/Manual still eject.
        assert!(should_auto_eject(true, true, Off));
        assert!(should_auto_eject(true, true, Manual));
        assert!(!should_auto_eject(true, true, Auto));
    }

    #[test]
    fn auto_eject_true_calls_ejector_once_with_mount() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.auto_eject = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let ej = RecordingEjector::new();

        let summary = run_offload_with_ejector(card.root(), &cfg, &mut ledger, &ej, &mut |_| {}).unwrap();

        assert_eq!(summary.copied, 4);
        let calls = ej.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "ejected exactly once");
        assert_eq!(calls[0], card.root());
    }

    #[test]
    fn auto_eject_false_does_not_call_ejector() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf()); // auto_eject = false
        let mut ledger = Ledger::open_in_memory().unwrap();
        let ej = RecordingEjector::new();

        run_offload_with_ejector(card.root(), &cfg, &mut ledger, &ej, &mut |_| {}).unwrap();

        assert!(ej.calls.lock().unwrap().is_empty(), "deletion opt-in; flag off => no eject");
    }

    #[test]
    fn auto_eject_skipped_for_non_gopro_volume() {
        let card = fixtures::not_a_gopro();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.auto_eject = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let ej = RecordingEjector::new();

        run_offload_with_ejector(card.root(), &cfg, &mut ledger, &ej, &mut |_| {}).unwrap();

        assert!(ej.calls.lock().unwrap().is_empty(), "non-GoPro volume is never ejected");
    }

    #[test]
    fn auto_eject_suppressed_when_delete_after_verify_and_auto_mirror() {
        // Contract G4: with delete_after_verify + Auto mirror, the worker still
        // needs the card mounted to delete sources after upload, so the sync
        // path must NOT eject even though auto_eject is on.
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = cloud_cfg(dest.path().to_path_buf(), crate::config::MirrorMode::Auto);
        cfg.auto_eject = true;
        cfg.delete_after_verify = true;
        let mut ledger = Ledger::open_in_memory().unwrap();
        let ej = RecordingEjector::new();

        run_offload_with_ejector(card.root(), &cfg, &mut ledger, &ej, &mut |_| {}).unwrap();

        assert!(
            ej.calls.lock().unwrap().is_empty(),
            "delete_after_verify + Auto must defer eject (worker needs the card)"
        );
    }
}
