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
}

/// Walk `<card_root>/DCIM/<NNN>GOPRO/*`, classify each file, drop proxies/
/// thumbnails unless enabled, skip files already in the ledger, and compute a
/// destination path (Flat layout) via the filename template. Returns the plan.
pub fn scan_card(
    card_root: &Path,
    cfg: &Config,
    ledger: &Ledger,
    serial: Option<&str>,
) -> Result<Vec<PlannedCopy>> {
    let dcim = card_root.join("DCIM");
    let mut plan: Vec<PlannedCopy> = Vec::new();
    let mut used_names: HashSet<PathBuf> = HashSet::new();

    let folders = std::fs::read_dir(&dcim).map_err(io_at(&dcim))?;
    let mut media_dirs: Vec<PathBuf> = folders
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
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
            let name = match src.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
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
                continue;
            }

            let cap: Captured = resolve_capture(&src, kind, mtime);
            let dest_name = render_name(&cfg.filename_template, &name, &cap, serial, None);
            // Resolve collisions against the real fs AND names already planned this run.
            let dest_path = resolve_collision(&cfg.dest_root, &dest_name, &used_names);
            used_names.insert(dest_path.clone());

            plan.push(PlannedCopy {
                src,
                name,
                kind,
                size,
                mtime_unix,
                dest_name: dest_path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
                dest_path,
            });
        }
    }
    Ok(plan)
}

#[cfg(test)]
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
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346")).unwrap();
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
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346")).unwrap();
        assert!(plan.iter().any(|p| p.name.ends_with(".LRV")));
    }

    #[test]
    fn dest_name_uses_template() {
        let card = fixtures::hero11_card();
        let dest = fixtures::dest();
        let cfg = Config::new(dest.path().to_path_buf());
        let ledger = Ledger::open_in_memory().unwrap();
        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346")).unwrap();
        let mp4 = plan.iter().find(|p| p.name == "GX010001.MP4").unwrap();
        // default template {date}_{original}; date derived from mtime (varies), so just check shape
        assert!(mp4.dest_name.ends_with("_GX010001.MP4"));
        assert_eq!(mp4.dest_path.parent().unwrap(), dest.path());
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
        let mtime = md.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        ledger.record("C346", "GX010001.MP4", md.len(), mtime, "/old", None).unwrap();

        let plan = scan_card(card.root(), &cfg, &ledger, Some("C346")).unwrap();
        assert!(!plan.iter().any(|p| p.name == "GX010001.MP4"));
        assert!(plan.iter().any(|p| p.name == "GS010003.360")); // others still planned
    }
}
