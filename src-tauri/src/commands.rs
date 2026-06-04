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

use crate::config_io::{validate_view, view_to_config, write_config_atomic, ConfigView};

use gpbeam_core::config::load_config;
use tauri::State;

use crate::config_io::config_to_view;

/// The shared `save_config` / `complete_wizard` pipeline as a pure function so
/// it can be unit-tested without a Tauri `AppHandle`.
///
/// Steps (validation precedes any write, per design §7 — a bad view leaves the
/// existing `gpbeam.toml` untouched):
/// 1. `validate_view` — reject malformed input up front.
/// 2. `view_to_config` — build the core `Config`.
/// 3. `write_config_atomic` — `.part` + fsync + rename; existing `[credentials.*]`
///    preserved by the writer.
/// 4. Swap `runtime.config`/`delete_after_verify` so the next cloud tick uses the
///    new settings (no task abort needed — lib.rs polls the runtime each pass).
/// 5. Refresh `state.cloud.configured` from whether a `[cloud]` table is present,
///    and re-seed `state.cloud.pending` from the persisted queue.
///
/// Mutates `state` and `runtime` in place; returns `Err(message)` on validation
/// or write failure (with `state`/`runtime` left as they were before the call).
fn apply_saved_config(
    view: &ConfigView,
    config_path: &Path,
    ledger_path: &Path,
    state: &mut AppState,
    runtime: &mut CloudRuntime,
) -> Result<(), String> {
    validate_view(view)?;
    let cfg = view_to_config(view)?;
    write_config_atomic(config_path, &cfg)?;

    // Swap the cloud runtime so the next tick honors the new settings.
    runtime.config = cfg.cloud.clone();
    runtime.delete_after_verify = cfg.delete_after_verify;

    // Refresh the cloud flags the UI renders.
    state.cloud.configured = cfg.cloud.is_some();
    if cfg.cloud.is_some() {
        seed_pending_from_ledger(ledger_path, &mut state.cloud);
    } else {
        state.cloud.pending = 0;
        state.cloud.failed = 0;
        state.cloud.uploading = None;
    }
    Ok(())
}

/// Snapshot of the current application state for a freshly-opened window. Clones
/// the managed `AppState` and re-seeds `cloud.pending` from the persisted queue
/// so the popover is accurate after an app restart (design §7).
#[tauri::command]
pub fn get_state(ctx: State<'_, AppCtx>) -> AppState {
    let mut state = ctx.state.lock().expect("AppState mutex poisoned").clone();
    seed_pending_from_ledger(&ctx.ledger_path, &mut state.cloud);
    state
}

/// The current on-disk `Config` as a UI-facing `ConfigView` (secrets redacted;
/// `has_password` is a keychain/env presence hint only). Falls back to M1
/// defaults rooted at the destination when no config exists yet, so the settings
/// window always renders.
#[tauri::command]
pub fn get_config(ctx: State<'_, AppCtx>) -> Result<ConfigView, String> {
    let cfg = match load_config(&ctx.config_path) {
        Ok(mut c) => {
            c.dest_root = ctx.dest_root.clone();
            c
        }
        Err(_) => gpbeam_core::config::Config::new(ctx.dest_root.clone()),
    };
    let has_password = match cfg.cloud.as_ref() {
        Some(cloud) => ctx.creds.has_password(&cloud.destination_id),
        None => false,
    };
    Ok(config_to_view(&cfg, has_password))
}

/// The most-recently-copied files (capped at `limit`) for the History tab.
#[tauri::command]
pub fn get_history(ctx: State<'_, AppCtx>, limit: usize) -> Result<Vec<HistoryRow>, String> {
    history_rows_from_ledger(&ctx.ledger_path, limit)
}

/// True when no `gpbeam.toml` exists at the resolved config path — the settings
/// window opens into the first-run wizard instead of the tabs (design §4.3).
#[tauri::command]
pub fn is_first_run(ctx: State<'_, AppCtx>) -> bool {
    !ctx.config_path.exists()
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

    use crate::cloud_runtime::CloudRuntime;
    use crate::config_io::{CloudView, ConfigView};

    fn base_view(dest: &str) -> ConfigView {
        ConfigView {
            dest_root: dest.to_string(),
            filename_template: "{name}".into(),
            include_proxies: false,
            include_thumbnails: false,
            verify: true,
            space_headroom: 0,
            delete_after_verify: false,
            auto_eject: false,
            cloud: None,
        }
    }

    fn cloud_view() -> CloudView {
        CloudView {
            destination_id: "nc1".into(),
            base_url: "https://cloud.example.com".into(),
            username: "alice".into(),
            remote_root: "GoPro".into(),
            mirror_mode: "auto".into(),
            chunk_threshold: 10_000_000,
            max_concurrency: 2,
            max_attempts: 3,
            has_password: true,
        }
    }

    #[test]
    fn apply_saved_config_writes_toml_and_marks_unconfigured_without_cloud() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut state = AppState::default();
        state.cloud.configured = true; // stale; should be cleared by a cloud-less save
        let mut runtime = CloudRuntime::empty();

        let view = base_view(dir.path().join("out").to_str().unwrap());
        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();

        assert!(cfg_path.exists(), "gpbeam.toml must be written");
        assert!(!cfg_path.with_extension("toml.part").exists(), "no .part left behind");
        assert!(!state.cloud.configured, "no [cloud] -> cloud.configured false");
        assert!(runtime.config.is_none());
    }

    #[test]
    fn apply_saved_config_with_cloud_sets_runtime_and_configured() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut state = AppState::default();
        let mut runtime = CloudRuntime::empty();

        let mut view = base_view(dir.path().join("out").to_str().unwrap());
        view.cloud = Some(cloud_view());
        view.delete_after_verify = true;

        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();

        assert!(state.cloud.configured, "[cloud] present -> configured true");
        let rt_cloud = runtime.config.as_ref().expect("runtime.config swapped in");
        assert_eq!(rt_cloud.destination_id, "nc1");
        assert_eq!(rt_cloud.username, "alice");
        assert!(runtime.delete_after_verify, "delete_after_verify carried into runtime");
    }

    #[test]
    fn apply_saved_config_seeds_pending_from_existing_queue() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        // Pre-seed one queued job.
        {
            let mut l = Ledger::open(&ledger_path).unwrap();
            let imp = l.record("C346", "GX010001.MP4", 1, 1, "/d/GX010001.MP4", None).unwrap();
            l.enqueue_cloud_job(imp, "nc1", "/d/GX010001.MP4", "r/x", 1, None).unwrap();
        }
        let mut state = AppState::default();
        let mut runtime = CloudRuntime::empty();
        let mut view = base_view(dir.path().join("out").to_str().unwrap());
        view.cloud = Some(cloud_view());

        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();
        assert_eq!(state.cloud.pending, 1);
    }

    #[test]
    fn apply_saved_config_rejects_invalid_view_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut state = AppState::default();
        let mut runtime = CloudRuntime::empty();

        let mut view = base_view(""); // empty dest_root is invalid
        view.dest_root = String::new();

        let err = apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime)
            .unwrap_err();
        assert!(!err.is_empty(), "validation error message is non-empty");
        assert!(!cfg_path.exists(), "invalid input must NOT write gpbeam.toml");
    }

    #[test]
    fn state_reading_commands_exist() {
        // Reference each command as a fn item so a signature drift fails to compile.
        let _ = get_state as fn(tauri::State<'_, AppCtx>) -> AppState;
        let _ = get_config as fn(tauri::State<'_, AppCtx>) -> Result<crate::config_io::ConfigView, String>;
        let _ = get_history as fn(tauri::State<'_, AppCtx>, usize) -> Result<Vec<HistoryRow>, String>;
        let _ = is_first_run as fn(tauri::State<'_, AppCtx>) -> bool;
    }
}
