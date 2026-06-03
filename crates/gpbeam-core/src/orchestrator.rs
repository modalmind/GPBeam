use crate::config::Config;
use crate::copy::copy_verified;
use crate::diskguard;
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
    Progress { file: String, copied: u64, total: u64 },
    Verified { file: String },
    Skipped { file: String },
    Failed { file: String, error: String },
    RunComplete { copied: usize, skipped: usize, failed: usize, bytes: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary { pub copied: usize, pub skipped: usize, pub failed: usize, pub bytes: u64 }

/// Run one offload pass for a mounted volume `card_root` into `cfg.dest_root`.
/// Emits `RunEvent`s for UI/CLI/notification consumers. Non-destructive: never
/// touches the card. Idempotent: ledger dedup prevents re-copying.
pub fn run_offload(
    card_root: &Path,
    cfg: &Config,
    ledger: &mut Ledger,
    emit: &mut dyn FnMut(RunEvent),
) -> Result<RunSummary> {
    if !is_gopro_card(card_root) {
        emit(RunEvent::NotGoPro(card_root.to_path_buf()));
        return Ok(RunSummary { copied: 0, skipped: 0, failed: 0, bytes: 0 });
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

    let serial_key = serial.as_deref().unwrap_or("unknown");
    let total = plan.len();
    let mut copied = 0usize;
    let mut failed = 0usize;
    let mut bytes = 0u64;
    let mut skipped_recovered = 0usize;

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
        // M1: no streaming progress from the orchestrator (the UI Channel is M3).
        // Passing a no-op callback avoids borrowing `emit` twice. We emit one
        // Progress event per file after it completes, then Verified.
        match copy_verified(&item.src, &item.dest_path, cfg.verify, &mut |_| {}) {
            Ok(out) => {
                let n = out.bytes;
                ledger.record(
                    serial_key,
                    &item.name,
                    item.size,
                    item.mtime_unix,
                    &item.dest_path.to_string_lossy(),
                    out.hash.as_deref(),
                )?;
                bytes += n;
                copied += 1;
                emit(RunEvent::Progress { file: item.name.clone(), copied: n, total: n });
                emit(RunEvent::Verified { file: item.name.clone() });
            }
            Err(e) => {
                failed += 1;
                emit(RunEvent::Failed { file: item.name.clone(), error: e.to_string() });
            }
        }
    }

    let skipped = skipped + skipped_recovered;
    let summary = RunSummary { copied, skipped, failed, bytes };
    emit(RunEvent::RunComplete { copied, skipped, failed, bytes });
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
}
