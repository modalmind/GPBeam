//! Offload from a USB-connected GoPro via the Open GoPro HTTP API. Mirrors the
//! filesystem orchestrator's per-item rules (classify → skip proxies/thumbnails →
//! ledger dedup → naming/collision) but sources media over HTTP and reuses the
//! shared `commit_imported` leaf helper. Emits the existing `RunEvent`s.

use crate::capture::Captured;
use crate::config::Config;
use crate::error::Result;
use crate::gopro::classify;
use crate::ledger::Ledger;
use crate::naming::{render_name, resolve_collision};
use crate::wired::client::RemoteMedia;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn media(name: &str, size: u64, cre: i64) -> RemoteMedia {
        RemoteMedia { dir: "100GOPRO".into(), name: name.into(), size, captured_unix: cre }
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
}
