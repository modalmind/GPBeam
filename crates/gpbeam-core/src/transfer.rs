//! Shared leaf helpers used by BOTH the filesystem offload (`orchestrator.rs`)
//! and the wired GoPro offload (`wired/offload.rs`, added in M4 Phase 4):
//!
//! * [`stream_hash_to_part`] — stream a reader into a `.part` file (append-aware
//!   for resume), hashing every on-disk byte with BLAKE3.
//! * [`commit_imported`] — after a verified file exists at its destination,
//!   record it in the ledger and (per the config's mirror mode) enqueue a cloud
//!   job. Returns the `imported` row id.

use crate::error::{io_at, Result};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

/// Stream `reader` into `part_path`, hashing ALL on-disk bytes with BLAKE3, and
/// report cumulative progress. Returns `(total_bytes_on_disk, blake3_hex)`.
///
/// * `already == 0` — fresh transfer: the `.part` is created/truncated, so any
///   stale partial is discarded and the hash covers only the newly written bytes.
/// * `already > 0` — resume: the `.part` is opened in append mode and the bytes
///   already on disk are folded into the hasher first, so the returned hash and
///   total cover the WHOLE file (pre-existing prefix + freshly appended suffix).
///   `progress` is called with the cumulative on-disk byte count (i.e. it starts
///   from `already` and grows as the reader is drained).
///
/// Shared by `copy_verified` (filesystem) and the wired download path so the two
/// never diverge on hashing/resume semantics.
pub fn stream_hash_to_part(
    reader: &mut dyn Read,
    part_path: &Path,
    already: u64,
    progress: &mut dyn FnMut(u64),
) -> Result<(u64, String)> {
    let mut hasher = blake3::Hasher::new();
    let mut on_disk: u64 = 0;

    // Open the .part: truncate for a fresh start, append for a resume.
    let mut file = if already > 0 {
        // Resume: fold the existing prefix into the hasher first so the final
        // hash covers the whole file, then append the new bytes.
        let mut existing = std::fs::File::open(part_path).map_err(io_at(part_path))?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = existing.read(&mut buf).map_err(io_at(part_path))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            on_disk += n as u64;
        }
        OpenOptions::new()
            .append(true)
            .open(part_path)
            .map_err(io_at(part_path))?
    } else {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(part_path)
            .map_err(io_at(part_path))?
    };

    // Report the starting cumulative count (e.g. the resume offset) before reading.
    progress(on_disk);

    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(io_at(part_path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(io_at(part_path))?;
        on_disk += n as u64;
        progress(on_disk);
    }
    file.flush().map_err(io_at(part_path))?;
    file.sync_all().map_err(io_at(part_path))?;

    Ok((on_disk, hasher.finalize().to_hex().to_string()))
}

/// After a verified file exists at `dest_path`, persist it: record the import in
/// the ledger, then (when `cfg.cloud`'s mirror mode is `Auto` or `Manual`, per
/// Contract G3) enqueue a single cloud job. Returns the `imported` row id so the
/// caller can stamp cloud status or correlate events.
///
/// `card_src` is the on-card source path retained on the cloud job so the worker
/// can delete the original after a verified upload (`Some` for the filesystem
/// path, `None` for the wired path, which has no on-disk source to delete).
///
/// This helper performs NO `RunEvent` emission — the caller emits `CloudQueued`
/// (and any other events) itself, keeping the fs and wired call sites in control
/// of their own event streams while sharing the ledger/enqueue logic.
#[allow(clippy::too_many_arguments)]
pub fn commit_imported(
    ledger: &mut crate::ledger::Ledger,
    cfg: &crate::config::Config,
    serial: &str,
    name: &str,
    size: u64,
    captured_unix: i64,
    dest_path: &Path,
    dest_name: &str,
    hash: Option<&str>,
    card_src: Option<&str>,
) -> Result<i64> {
    use crate::config::MirrorMode;

    let imported_id = ledger.record(
        serial,
        name,
        size,
        captured_unix,
        &dest_path.to_string_lossy(),
        hash,
    )?;

    if let Some(cloud) = &cfg.cloud {
        if matches!(cloud.mirror_mode, MirrorMode::Auto | MirrorMode::Manual) {
            let remote = crate::orchestrator::remote_path_for(&cloud.remote_root, dest_name);
            ledger.enqueue_cloud_job(
                imported_id,
                &cloud.destination_id,
                &dest_path.to_string_lossy(),
                &remote,
                size,
                card_src,
            )?;
        }
    }

    Ok(imported_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn blake3_hex(bytes: &[u8]) -> String {
        let mut h = blake3::Hasher::new();
        h.update(bytes);
        h.finalize().to_hex().to_string()
    }

    #[test]
    fn fresh_write_streams_hashes_and_reports_progress() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("GX010001.MP4.part");
        let payload = b"hello gopro wired footage".to_vec();

        let mut reader = Cursor::new(payload.clone());
        let mut seen = 0u64;
        let (total, hash) =
            stream_hash_to_part(&mut reader, &part, 0, &mut |c| seen = c).unwrap();

        assert_eq!(total, payload.len() as u64);
        assert_eq!(seen, payload.len() as u64, "progress reaches the on-disk total");
        assert_eq!(hash, blake3_hex(&payload));
        assert_eq!(std::fs::read(&part).unwrap(), payload);
    }

    #[test]
    fn empty_reader_produces_empty_hash_and_zero_total() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("empty.part");
        let mut reader = Cursor::new(Vec::<u8>::new());
        let (total, hash) =
            stream_hash_to_part(&mut reader, &part, 0, &mut |_| {}).unwrap();
        assert_eq!(total, 0);
        assert_eq!(hash, blake3_hex(b""));
        assert_eq!(std::fs::read(&part).unwrap().len(), 0);
    }

    #[test]
    fn resume_appends_and_hashes_whole_on_disk_file() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("GX020001.MP4.part");

        // First pass wrote the first half to the .part file.
        let first = b"FIRST-HALF-".to_vec();
        std::fs::write(&part, &first).unwrap();

        // Resume: open append, stream the rest. `already` = current on-disk len.
        let rest = b"SECOND-HALF".to_vec();
        let already = first.len() as u64;
        let mut reader = Cursor::new(rest.clone());
        let mut seen = 0u64;
        let (total, hash) =
            stream_hash_to_part(&mut reader, &part, already, &mut |c| seen = c).unwrap();

        let whole: Vec<u8> = first.iter().chain(rest.iter()).copied().collect();
        assert_eq!(total, whole.len() as u64, "total covers pre-existing + appended bytes");
        assert_eq!(seen, whole.len() as u64, "progress is cumulative over the whole file");
        assert_eq!(hash, blake3_hex(&whole), "hash is over the WHOLE on-disk file");
        assert_eq!(std::fs::read(&part).unwrap(), whole);
    }

    #[test]
    fn resume_with_already_zero_truncates_for_fresh_start() {
        // already == 0 means a fresh transfer: any stale .part is overwritten,
        // never appended to (so a re-download from scratch is correct).
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("stale.part");
        std::fs::write(&part, b"STALE GARBAGE THAT IS LONGER").unwrap();

        let payload = b"clean".to_vec();
        let mut reader = Cursor::new(payload.clone());
        let (total, hash) =
            stream_hash_to_part(&mut reader, &part, 0, &mut |_| {}).unwrap();

        assert_eq!(total, payload.len() as u64);
        assert_eq!(hash, blake3_hex(&payload));
        assert_eq!(std::fs::read(&part).unwrap(), payload, "stale .part fully replaced");
    }

    use crate::config::{CloudConfig, CloudKind, Config, MirrorMode};
    use crate::ledger::{JobState, Ledger};
    use std::path::PathBuf;

    fn cfg_no_cloud() -> Config {
        Config::new(PathBuf::from("/dest"))
    }

    fn cfg_with_cloud(mode: MirrorMode) -> Config {
        let mut cfg = Config::new(PathBuf::from("/dest"));
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
    fn commit_records_but_enqueues_nothing_when_no_cloud() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let cfg = cfg_no_cloud();
        let dest = PathBuf::from("/dest/2026_GX010001.MP4");

        let id = commit_imported(
            &mut ledger,
            &cfg,
            "C346",
            "GX010001.MP4",
            4096,
            1000,
            &dest,
            "2026_GX010001.MP4",
            Some("deadbeef"),
            None,
        )
        .unwrap();

        assert!(id > 0);
        assert!(ledger.is_imported("C346", "GX010001.MP4", 4096, 1000).unwrap());
        assert_eq!(ledger.pending_cloud_count().unwrap(), 0, "no cloud => no jobs");
    }

    #[test]
    fn commit_off_mirror_enqueues_nothing() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let cfg = cfg_with_cloud(MirrorMode::Off);
        let dest = PathBuf::from("/dest/2026_GX010001.MP4");

        commit_imported(
            &mut ledger, &cfg, "C346", "GX010001.MP4", 4096, 1000, &dest,
            "2026_GX010001.MP4", None, None,
        )
        .unwrap();

        assert_eq!(ledger.pending_cloud_count().unwrap(), 0, "Off mirror enqueues nothing");
    }

    #[test]
    fn commit_auto_mirror_enqueues_one_job_with_remote_path_and_card_src() {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let cfg = cfg_with_cloud(MirrorMode::Auto);
        let dest = PathBuf::from("/dest/2026_GX010001.MP4");

        let id = commit_imported(
            &mut ledger,
            &cfg,
            "C346",
            "GX010001.MP4",
            4096,
            1000,
            &dest,
            "2026_GX010001.MP4",
            Some("cafef00d"),
            Some("/Volumes/GOPRO/DCIM/100GOPRO/GX010001.MP4"),
        )
        .unwrap();

        assert_eq!(ledger.pending_cloud_count().unwrap(), 1);
        let jobs = ledger.list_cloud_jobs(Some(JobState::Queued)).unwrap();
        assert_eq!(jobs.len(), 1);
        let j = &jobs[0];
        assert_eq!(j.imported_id, id);
        assert_eq!(j.destination_id, "nc1");
        assert_eq!(j.local_path, "/dest/2026_GX010001.MP4");
        assert_eq!(j.remote_path, "GoPro/2026_GX010001.MP4", "remote_root + '/' + dest_name");
        assert_eq!(j.total_bytes, 4096);
        assert_eq!(
            j.card_src.as_deref(),
            Some("/Volumes/GOPRO/DCIM/100GOPRO/GX010001.MP4")
        );
    }

    #[test]
    fn commit_manual_mirror_also_enqueues() {
        // Contract G3: enqueue for BOTH Auto and Manual.
        let mut ledger = Ledger::open_in_memory().unwrap();
        let cfg = cfg_with_cloud(MirrorMode::Manual);
        let dest = PathBuf::from("/dest/clip.MP4");

        commit_imported(
            &mut ledger, &cfg, "C346", "clip.MP4", 10, 1, &dest, "clip.MP4", None, None,
        )
        .unwrap();

        assert_eq!(ledger.pending_cloud_count().unwrap(), 1, "Manual mirror queues a job");
    }

    #[test]
    fn commit_wired_passes_no_card_src() {
        // The wired path has no card source path; card_src=None must round-trip.
        let mut ledger = Ledger::open_in_memory().unwrap();
        let cfg = cfg_with_cloud(MirrorMode::Auto);
        let dest = PathBuf::from("/dest/wired.MP4");

        commit_imported(
            &mut ledger, &cfg, "C346", "wired.MP4", 7, 2, &dest, "wired.MP4",
            Some("abc123"), None,
        )
        .unwrap();

        let jobs = ledger.list_cloud_jobs(None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].card_src, None, "wired enqueues with no card_src");
    }
}
