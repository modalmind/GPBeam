//! Tauri command surface for the GPBeam M3 GUI. Every `#[tauri::command]` here
//! is a thin wrapper over the tested helpers in `config_io` / `app_state` /
//! `keyring_store` / `gpbeam-core`. All non-trivial logic lives in the pure
//! free helpers below (which ARE unit-tested), so the commands stay testable-
//! by-inspection and the real Tauri glue is the only untested surface.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::app_state::AppState;
use crate::cloud_runtime::CloudRuntime;
use crate::keyring_store::KeyringCredentialStore;

/// Tauri-managed application context. Holds the shared, mutable state every
/// command reads/writes, plus the immutable resolved paths. Registered via
/// `.manage(AppCtx { .. })` in lib.rs (Phase 6).
pub struct AppCtx {
    /// The single source of truth the UI renders. Folded by the reducers in
    /// `app_state` and re-emitted on `gpbeam://state` after every apply.
    pub state: Arc<Mutex<AppState>>,
    /// Pause flag the cloud tick loop checks before claiming jobs.
    pub paused: Arc<AtomicBool>,
    /// Keychain-backed credential store (env > keychain > toml precedence).
    pub creds: Arc<KeyringCredentialStore>,
    /// Mutable cloud settings the tick loop reads each pass; `save_config`
    /// swaps `runtime.config` so the next tick uses the new settings.
    pub runtime: Arc<Mutex<CloudRuntime>>,
    /// Resolved offload destination root (`$GPBEAM_DEST`, else `~/GPBeam`).
    pub dest_root: PathBuf,
    /// Resolved `gpbeam.toml` path for atomic writes.
    pub config_path: PathBuf,
    /// Resolved SQLite ledger path for history / pending-count reads.
    pub ledger_path: PathBuf,
}

/// One recent-transfer row for the History tab. Camel-cased to match the TS
/// `HistoryRow` type in `ui/src/lib/bindings.ts`.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRow {
    pub name: String,
    pub dest_path: String,
    pub size: u64,
    pub copied_at: String,
    pub cloud_status: Option<String>,
}

use std::path::Path;

use gpbeam_core::ledger::Ledger;

/// Map the ledger's recent imports to `HistoryRow`s for the History tab.
///
/// Returns an EMPTY list (not an error) when the ledger file does not exist
/// yet — a first-run install has no transfers and the UI must render cleanly.
/// Any other ledger error (corrupt db, query failure) is surfaced as a string.
fn history_rows_from_ledger(ledger_path: &Path, limit: usize) -> Result<Vec<HistoryRow>, String> {
    if !ledger_path.exists() {
        return Ok(Vec::new());
    }
    let ledger = Ledger::open(ledger_path).map_err(|e| e.to_string())?;
    let rows = ledger.recent_imports(limit).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| HistoryRow {
            name: r.name,
            dest_path: r.dest_path,
            size: r.size,
            copied_at: r.copied_at,
            cloud_status: r.cloud_status,
        })
        .collect())
}

use crate::app_state::CloudState;

/// Seed `cloud.pending` from the persisted cloud-job queue. Used by `get_state`
/// so a window opened after an app restart (before the next drain tick) reflects
/// the real backlog. No ledger file (or a read error) leaves `cloud` untouched —
/// the in-memory counter, if any, stands.
fn seed_pending_from_ledger(ledger_path: &Path, cloud: &mut CloudState) {
    if !ledger_path.exists() {
        return;
    }
    if let Ok(ledger) = Ledger::open(ledger_path) {
        if let Ok(n) = ledger.pending_cloud_count() {
            cloud.pending = n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_compiles() {
        // Smoke test: this module and its dependencies resolve.
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn history_row_serializes_camel_case() {
        let row = HistoryRow {
            name: "GX010001.MP4".into(),
            dest_path: "/dest/GX010001.MP4".into(),
            size: 4096,
            copied_at: "2026-06-03 10:00:00".into(),
            cloud_status: Some("done".into()),
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["name"], "GX010001.MP4");
        assert_eq!(json["destPath"], "/dest/GX010001.MP4");
        assert_eq!(json["size"], 4096);
        assert_eq!(json["copiedAt"], "2026-06-03 10:00:00");
        assert_eq!(json["cloudStatus"], "done");
    }

    #[test]
    fn history_row_null_cloud_status_serializes_as_null() {
        let row = HistoryRow {
            name: "GX010002.MP4".into(),
            dest_path: "/dest/GX010002.MP4".into(),
            size: 10,
            copied_at: "2026-06-03 10:01:00".into(),
            cloud_status: None,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert!(json["cloudStatus"].is_null());
    }

    use gpbeam_core::ledger::Ledger;

    #[test]
    fn history_rows_maps_recent_imports_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let mut l = Ledger::open(&path).unwrap();
        let id1 = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", None)
            .unwrap();
        l.set_cloud_status(id1, "done").unwrap();
        l.record("C346", "GX010002.MP4", 10, 2000, "/dest/GX010002.MP4", None)
            .unwrap();

        let rows = history_rows_from_ledger(&path, 10).unwrap();
        assert_eq!(rows.len(), 2);
        // recent_imports is ORDER BY id DESC -> the second-recorded file leads.
        assert_eq!(rows[0].name, "GX010002.MP4");
        assert_eq!(rows[0].dest_path, "/dest/GX010002.MP4");
        assert_eq!(rows[0].size, 10);
        assert!(rows[0].cloud_status.is_none());
        assert_eq!(rows[1].name, "GX010001.MP4");
        assert_eq!(rows[1].cloud_status.as_deref(), Some("done"));
    }

    #[test]
    fn history_rows_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let mut l = Ledger::open(&path).unwrap();
        for i in 0..5 {
            l.record("C346", &format!("GX0100{i:02}.MP4"), 1, 1000 + i, "/d", None)
                .unwrap();
        }
        let rows = history_rows_from_ledger(&path, 2).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn history_rows_missing_ledger_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.sqlite");
        let rows = history_rows_from_ledger(&path, 10).unwrap();
        assert!(rows.is_empty());
    }

    use crate::app_state::CloudState;

    #[test]
    fn seed_pending_reads_count_from_existing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        // Enqueue one queued cloud job so pending_cloud_count() == 1.
        {
            let mut l = Ledger::open(&path).unwrap();
            let imp = l
                .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", None)
                .unwrap();
            l.enqueue_cloud_job(imp, "nc1", "/dest/GX010001.MP4", "r/GX010001.MP4", 4096, None)
                .unwrap();
        }
        let mut cloud = CloudState::default();
        seed_pending_from_ledger(&path, &mut cloud);
        assert_eq!(cloud.pending, 1);
    }

    #[test]
    fn seed_pending_missing_ledger_leaves_state_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.sqlite");
        let mut cloud = CloudState {
            configured: true,
            pending: 7,
            ..CloudState::default()
        };
        seed_pending_from_ledger(&path, &mut cloud);
        // No ledger file -> nothing read; the in-memory count is preserved.
        assert_eq!(cloud.pending, 7);
        assert!(cloud.configured);
    }
}
