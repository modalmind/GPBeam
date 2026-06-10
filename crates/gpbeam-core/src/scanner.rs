use crate::capture::{resolve_capture, Captured};
use crate::config::Config;
use crate::error::{io_at, Result};
use crate::gopro::{classify, MediaKind};
use crate::ledger::Ledger;
use crate::naming::{render_name, resolve_collision};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PlannedCopy {
    pub src: PathBuf,
    pub name: String,
    pub kind: MediaKind,
    pub size: u64,
    pub mtime_unix: i64,
    pub dest_name: String,
    pub dest_path: PathBuf,
    /// The rendered destination path BEFORE collision resolution (no `_N` suffix).
    /// When a prior run copied+verified here but crashed before `record()`, the
    /// next scan finds the file on disk and bumps `dest_path` to `_1`; this field
    /// preserves the original target so the orchestrator can adopt (record) the
    /// already-verified file instead of re-copying it. Equals `dest_path` when no
    /// collision occurred.
    pub canonical_dest_path: PathBuf,
}

/// The plan-visible file name for a directory entry. Non-UTF-8 names are
/// rendered lossily (invalid bytes become U+FFFD) rather than silently
/// dropped, so such a file is still planned, copied, and counted: `src` (the
/// exact `OsStr` path) opens the real file, while this string only feeds
/// classification, the ledger dedup key, and the rendered dest name. `None`
/// only for a path with no final component (never true for read_dir entries).
fn entry_name(src: &Path) -> Option<String> {
    src.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Like `scan_card`, but also returns the count of files skipped because they
/// were already recorded in the ledger.
pub fn scan_with_skips(
    card_root: &Path,
    cfg: &Config,
    ledger: &Ledger,
    serial: Option<&str>,
    model: Option<&str>,
) -> Result<(Vec<PlannedCopy>, usize)> {
    let dcim = card_root.join("DCIM");
    let mut plan: Vec<PlannedCopy> = Vec::new();
    let mut used_names: HashSet<PathBuf> = HashSet::new();
    let mut skipped = 0usize;

    let folders = std::fs::read_dir(&dcim).map_err(io_at(&dcim))?;
    let mut media_dirs: Vec<PathBuf> = folders
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(crate::gopro::is_gopro_media_folder)
                    .unwrap_or(false)
        })
        .collect();
    media_dirs.sort();

    let serial_key = serial.unwrap_or("unknown");

    for dir in media_dirs {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(io_at(&dir))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();

        for src in files {
            let name = match entry_name(&src) {
                Some(n) => n,
                None => continue,
            };
            let kind = classify(&name);
            if kind.is_proxy() && !cfg.include_proxies {
                continue;
            }
            if kind.is_thumbnail() && !cfg.include_thumbnails {
                continue;
            }

            let md = std::fs::metadata(&src).map_err(io_at(&src))?;
            let size = md.len();
            let mtime = md.modified().map_err(io_at(&src))?;
            let mtime_unix = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            if ledger.is_imported(serial_key, &name, size, mtime_unix)? {
                skipped += 1;
                continue;
            }

            let cap: Captured = resolve_capture(&src, kind, mtime);
            let dest_name = render_name(&cfg.filename_template, &name, &cap, serial, model);
            // The rendered target before any collision suffix — retained so the
            // orchestrator can adopt a crashed-mid-run verified file at this path.
            let canonical_dest_path = cfg.dest_root.join(&dest_name);
            // Resolve collisions against the real fs AND names already planned this run.
            let dest_path = resolve_collision(&cfg.dest_root, &dest_name, &used_names);
            used_names.insert(dest_path.clone());

            plan.push(PlannedCopy {
                src,
                name,
                kind,
                size,
                mtime_unix,
                dest_name: dest_path.file_name().unwrap().to_str().unwrap().to_string(),
                dest_path,
                canonical_dest_path,
            });
        }
    }
    Ok((plan, skipped))
}

/// Walk `<card_root>/DCIM/<NNN>GOPRO/*`, classify each file, drop proxies/
/// thumbnails unless enabled, skip files already in the ledger, and compute a
/// destination path (Flat layout) via the filename template. Returns the plan.
pub fn scan_card(
    card_root: &Path,
    cfg: &Config,
    ledger: &Ledger,
    serial: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<PlannedCopy>> {
    Ok(scan_with_skips(card_root, cfg, ledger, serial, model)?.0)
}

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/fixtures.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;

    #[test]
    fn skips_proxies_and_thumbnails_by_default() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        let names: Vec<&str> = plan.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"GX010001.MP4"));
        assert!(names.contains(&"GS010003.360"));
        assert!(names.contains(&"GOPR0002.JPG"));
        assert!(!names.iter().any(|n| n.ends_with(".LRV")));
        assert!(!names.iter().any(|n| n.ends_with(".THM")));
    }

    #[test]
    fn includes_proxies_when_enabled() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.include_proxies = true;
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        assert!(plan.iter().any(|p| p.name.ends_with(".LRV")));
    }

    #[test]
    fn dest_name_uses_template() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        let mp4 = plan.iter().find(|p| p.name == "GX010001.MP4").unwrap();
        // default template {date}_{original}; date derived from mtime (varies), so just check shape
        assert!(mp4.dest_name.ends_with("_GX010001.MP4"));
        assert_eq!(mp4.dest_path.parent().unwrap(), dest.path());
    }

    #[test]
    fn includes_thumbnails_when_enabled() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.include_thumbnails = true;
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        assert!(plan.iter().any(|p| p.name.ends_with(".THM")));
    }

    #[test]
    fn unknown_kind_file_is_still_planned() {
        use std::fs;
        let card = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(card.path().join("DCIM/100GOPRO")).unwrap();
        fs::write(card.path().join("DCIM/100GOPRO/GX010001.XYZ"), vec![0u8; 8]).unwrap();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.path(), &cfg, &ledger, Some("C346"), None).unwrap();
        assert!(
            plan.iter().any(|p| p.name == "GX010001.XYZ"),
            "unknown media must still be copied"
        );
    }

    #[test]
    fn ignores_non_gopro_dcim_subfolders() {
        use std::fs;
        let card = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(card.path().join("DCIM/100GOPRO")).unwrap();
        fs::write(
            card.path().join("DCIM/100GOPRO/GX010001.MP4"),
            vec![0u8; 16],
        )
        .unwrap();
        fs::create_dir_all(card.path().join("DCIM/Camera")).unwrap(); // non-GoPro
        fs::write(card.path().join("DCIM/Camera/IMG_0001.JPG"), vec![0u8; 16]).unwrap();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.path(), &cfg, &ledger, Some("C346"), None).unwrap();
        let names: Vec<&str> = plan.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"GX010001.MP4"));
        assert!(
            !names.contains(&"IMG_0001.JPG"),
            "non-GoPro DCIM folders must be ignored"
        );
    }

    #[test]
    fn already_imported_files_are_excluded() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        // Pre-record GX010001.MP4 with its real size+mtime so it's treated as done.
        let src = card.root().join("DCIM/100GOPRO/GX010001.MP4");
        let md = std::fs::metadata(&src).unwrap();
        let mtime = md
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ledger
            .record("C346", "GX010001.MP4", md.len(), mtime, "/old", None)
            .unwrap();

        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        assert!(!plan.iter().any(|p| p.name == "GX010001.MP4"));
        assert!(plan.iter().any(|p| p.name == "GS010003.360")); // others still planned
    }

    #[test]
    fn reports_skipped_count() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let mut ledger = Ledger::open_in_memory().unwrap();
        let src = card.root().join("DCIM/100GOPRO/GX010001.MP4");
        let md = std::fs::metadata(&src).unwrap();
        let mtime = md
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ledger
            .record("C346", "GX010001.MP4", md.len(), mtime, "/old", None)
            .unwrap();
        let (plan, skipped) =
            scan_with_skips(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        assert_eq!(skipped, 1);
        assert!(!plan.iter().any(|p| p.name == "GX010001.MP4"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_entry_name_is_lossy_not_dropped() {
        // F6: a non-UTF-8 file name must not be silently skipped. The name
        // derivation is lossy (U+FFFD) so the entry stays visible/plannable.
        // (Tested on the helper: APFS itself rejects non-UTF-8 names, so the
        // on-disk case cannot be staged on macOS dev machines.)
        use std::os::unix::ffi::OsStrExt;
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(
            b"DCIM/100GOPRO/GX01\xFF0001.MP4",
        ));
        let name = super::entry_name(p).expect("read_dir entries always have a file name");
        assert!(
            name.contains('\u{FFFD}'),
            "invalid bytes render as U+FFFD, keeping the file visible: {name:?}"
        );
        assert!(name.starts_with("GX01"));
        assert!(
            name.ends_with(".MP4"),
            "the extension survives for classification"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_on_disk_is_planned_when_the_fs_allows_it() {
        // End-to-end variant of the above: on filesystems that accept
        // non-UTF-8 names (ext4 etc.), the file must appear in the plan. APFS
        // rejects the creation (EILSEQ) — then there is nothing to test here.
        use std::os::unix::ffi::OsStrExt;
        let card = tempfile::TempDir::new().unwrap();
        let dir = card.path().join("DCIM/100GOPRO");
        std::fs::create_dir_all(&dir).unwrap();
        let bad = std::ffi::OsStr::from_bytes(b"GX01\xFF0001.MP4");
        if std::fs::write(dir.join(bad), vec![0u8; 8]).is_err() {
            return; // fs refuses non-UTF-8 names; covered by the helper test
        }
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.path(), &cfg, &ledger, Some("C346"), None).unwrap();
        assert_eq!(
            plan.len(),
            1,
            "non-UTF-8 names must not be silently dropped"
        );
        assert!(plan[0].name.contains('\u{FFFD}'));
    }

    #[test]
    fn dest_name_uses_model_token() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let mut cfg = Config::new(dest.path().to_path_buf());
        cfg.filename_template = "{model}_{original}".into();
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346"), Some("HERO11")).unwrap();
        assert!(plan.iter().any(|p| p.dest_name == "HERO11_GX010001.MP4"));
    }
}
