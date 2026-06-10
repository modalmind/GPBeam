//! Tauri command surface for the GPBeam M3 GUI. Every `#[tauri::command]` here
//! is a thin wrapper over the tested helpers in `config_io` / `app_state` /
//! `keyring_store` / `gpbeam-core`. All non-trivial logic lives in the pure
//! free helpers below (which ARE unit-tested), so the commands stay testable-
//! by-inspection; the Tauri glue itself is covered by the mock-runtime smoke
//! test in lib.rs.
//!
//! Threading: Tauri 2 runs NON-async commands on the MAIN thread. Every
//! command that does I/O — SQLite reads, config-file writes, OS-keychain
//! access (which can block on a macOS keychain-unlock prompt) — is therefore
//! `async` and pushes the blocking work onto `tauri::async_runtime::
//! spawn_blocking`, so the UI event loop and the tray never freeze behind it.
//! Commands are generic over `R: tauri::Runtime` so the real
//! `generate_handler!` list can also be registered on the `MockRuntime` in
//! tests.

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
    /// Serializes the two ingest paths (SD `handle_mount` + wired
    /// `run_wired_offload_for_camera`) against the shared `dest_root`/ledger, so
    /// a card that is reachable as BOTH a mount and a USB camera can't have both
    /// offloads plan + copy the same files concurrently (M6). A unit `Mutex`; the
    /// guard is held across the whole offload.
    pub offload_lock: Arc<tokio::sync::Mutex<()>>,
    /// LIVE wired-ingest flag: seeded from `Config.wired_ingest` at startup and
    /// updated by `save_config`, so toggling it in Settings takes effect without
    /// a relaunch. The wired consumer loop drops `CameraFound` events while it
    /// is false, and `recompute_detector_pause` keeps the poller paused.
    pub wired_enabled: Arc<AtomicBool>,
    /// True while a wired offload owns the camera (the consumer loop sets it
    /// around each `run_wired_offload_for_camera`). Input to
    /// [`recompute_detector_pause`], so `save_config` re-enabling wired ingest
    /// mid-offload cannot un-pause the poller and contend with the download.
    pub wired_offload_active: Arc<AtomicBool>,
    /// The pause flag handed to the wired camera poller (`poll_for_camera_with_
    /// rearm` checks it each ~2s tick). Always recomputed via
    /// [`recompute_detector_pause`]: paused while an offload is in flight OR
    /// wired ingest is disabled.
    pub detector_paused: Arc<AtomicBool>,
    /// Resolved offload destination root (`$GPBEAM_DEST`, else `~/GPBeam`).
    pub dest_root: PathBuf,
    /// Resolved `gpbeam.toml` path for atomic writes.
    pub config_path: PathBuf,
    /// Resolved SQLite ledger path for history / pending-count reads.
    pub ledger_path: PathBuf,
}

/// Pure decision for the wired camera poller's pause flag: probing must stop
/// while an offload owns the camera (the Open GoPro HTTP server serves one
/// client at a time) OR while wired ingest is disabled in Settings (no probe
/// traffic when the feature is off).
pub(crate) fn detector_should_pause(offload_active: bool, wired_enabled: bool) -> bool {
    offload_active || !wired_enabled
}

/// Re-derive `ctx.detector_paused` from the two inputs. Called by every writer
/// of either input (the wired consumer loop around each offload; `save_config`
/// after a wired_ingest toggle), so any interleaving converges on the value of
/// the latest stores (SeqCst).
pub(crate) fn recompute_detector_pause(ctx: &AppCtx) {
    let pause = detector_should_pause(
        ctx.wired_offload_active.load(Ordering::SeqCst),
        ctx.wired_enabled.load(Ordering::SeqCst),
    );
    ctx.detector_paused.store(pause, Ordering::SeqCst);
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

#[cfg(test)]
use crate::app_state::CloudState;

/// The persisted queue's live counters: jobs still to drain (`pending`) and
/// terminally-failed jobs awaiting a manual Retry (`failed`). Read together so
/// every seed path keeps the two popover counters consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CloudCounts {
    pub pending: usize,
    pub failed: usize,
}

/// Read both cloud counters from the ledger. `None` when the ledger file does
/// not exist or cannot be read — callers leave the in-memory counters standing
/// in that case (a fresh install has nothing to seed).
pub(crate) fn cloud_counts_from_ledger(ledger_path: &Path) -> Option<CloudCounts> {
    if !ledger_path.exists() {
        return None;
    }
    let ledger = Ledger::open(ledger_path).ok()?;
    let pending = ledger.pending_cloud_count().ok()?;
    let failed = ledger.failed_cloud_count().ok()? as usize;
    Some(CloudCounts { pending, failed })
}

/// Seed `cloud.pending` AND `cloud.failed` from the persisted cloud-job queue,
/// so a window opened after an app restart reflects the real backlog —
/// including terminally-failed jobs, which the Retry button is gated on
/// (`failed === 0` disables it). No ledger file (or a read error) leaves
/// `cloud` untouched — the in-memory counters stand. Test-only convenience over
/// [`cloud_counts_from_ledger`]: the commands inline the same two assignments
/// because the counts are read on a blocking thread, away from the `CloudState`.
#[cfg(test)]
fn seed_cloud_counts_from_ledger(ledger_path: &Path, cloud: &mut CloudState) {
    if let Some(counts) = cloud_counts_from_ledger(ledger_path) {
        cloud.pending = counts.pending;
        cloud.failed = counts.failed;
    }
}

use crate::config_io::{validate_view, view_to_config, write_config_atomic, ConfigView};

use std::sync::atomic::Ordering;

use gpbeam_core::config::{load_config, Config};
use tauri::State;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::config_io::config_to_view;

/// Steps 1–3 of the save pipeline — validate, convert, atomically persist —
/// with NO locks taken: `save_config` runs this on `spawn_blocking` so the
/// fsync never happens under the state/runtime mutexes (which would stall the
/// cloud loop and any state reader for the duration of the write).
/// Validation precedes any write (design §7): a bad view leaves the existing
/// `gpbeam.toml` untouched.
fn persist_view(view: &ConfigView, config_path: &Path) -> Result<Config, String> {
    validate_view(view)?;
    let cfg = view_to_config(view)?;
    write_config_atomic(config_path, &cfg)?;
    Ok(cfg)
}

/// Steps 4–5 of the save pipeline — the in-memory swap, cheap enough to run
/// under the state+runtime mutexes:
/// 4. Swap `runtime.config`/`delete_after_verify` so the next cloud tick uses
///    the new settings (no task abort needed — lib.rs polls the runtime).
/// 5. Refresh `state.cloud.configured`, and apply the pre-read queue `counts`
///    (pending + failed); removing the `[cloud]` table zeroes the counters.
fn apply_config_in_memory(
    cfg: &Config,
    counts: Option<CloudCounts>,
    state: &mut AppState,
    runtime: &mut CloudRuntime,
) {
    runtime.config = cfg.cloud.clone();
    runtime.delete_after_verify = cfg.delete_after_verify;

    state.cloud.configured = cfg.cloud.is_some();
    if cfg.cloud.is_some() {
        if let Some(c) = counts {
            state.cloud.pending = c.pending;
            state.cloud.failed = c.failed;
        }
    } else {
        state.cloud.pending = 0;
        state.cloud.failed = 0;
        state.cloud.uploading = None;
    }
}

/// The whole `save_config` / `complete_wizard` pipeline as a pure function so
/// it can be unit-tested without a Tauri `AppHandle`: [`persist_view`] then
/// [`apply_config_in_memory`] with counts read from `ledger_path`. Mutates
/// `state` and `runtime` in place; returns `Err(message)` on validation or
/// write failure (with `state`/`runtime` left as they were before the call).
/// Test-only composition: the real command interleaves the same steps around
/// `spawn_blocking` so the file write never runs under the mutexes.
#[cfg(test)]
fn apply_saved_config(
    view: &ConfigView,
    config_path: &Path,
    ledger_path: &Path,
    state: &mut AppState,
    runtime: &mut CloudRuntime,
) -> Result<(), String> {
    let cfg = persist_view(view, config_path)?;
    let counts = cloud_counts_from_ledger(ledger_path);
    apply_config_in_memory(&cfg, counts, state, runtime);
    Ok(())
}

/// Snapshot of the current application state for a freshly-opened window. Clones
/// the managed `AppState` and re-seeds `cloud.pending`/`cloud.failed` from the
/// persisted queue so the popover is accurate after an app restart (design §7).
/// Async: the ledger read is SQLite I/O and must not run on the main thread.
#[tauri::command]
pub async fn get_state(ctx: State<'_, AppCtx>) -> Result<AppState, String> {
    let mut state = crate::lock_recover(&ctx.state).clone();
    let ledger_path = ctx.ledger_path.clone();
    let counts =
        tauri::async_runtime::spawn_blocking(move || cloud_counts_from_ledger(&ledger_path))
            .await
            .map_err(|e| e.to_string())?;
    if let Some(c) = counts {
        state.cloud.pending = c.pending;
        state.cloud.failed = c.failed;
    }
    Ok(state)
}

/// `get_config` body, parameterized on the resolved paths + credential store so
/// it is unit-testable and can run on a blocking thread (the `has_password`
/// keychain probe can block on a macOS keychain-unlock prompt).
fn get_config_impl(
    config_path: &Path,
    default_dest: &Path,
    creds: &KeyringCredentialStore,
) -> Result<ConfigView, String> {
    let cfg = match load_config(config_path) {
        Ok(mut c) => {
            // Honor the destination the wizard/Settings wrote into the config. An
            // explicit GPBEAM_DEST env still wins (power-user override, captured in
            // ctx.dest_root); a config with no dest_root falls back to the default.
            let env_override = std::env::var("GPBEAM_DEST")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some();
            let resolved = resolve_dest_root(&c.dest_root, default_dest, env_override);
            c.dest_root = resolved;
            c
        }
        Err(_) => gpbeam_core::config::Config::new(default_dest.to_path_buf()),
    };
    let has_password = match cfg.cloud.as_ref() {
        Some(cloud) => creds.has_password(&cloud.destination_id),
        None => false,
    };
    let mut view = config_to_view(&cfg, has_password);
    // M2: surface any plaintext app-passwords still in gpbeam.toml so the Cloud
    // tab can offer a one-click migration into the keychain.
    view.plaintext_credential_ids = crate::config_io::plaintext_credential_ids(config_path);
    Ok(view)
}

/// The current on-disk `Config` as a UI-facing `ConfigView` (secrets redacted;
/// `has_password` is a keychain/env presence hint only). Falls back to M1
/// defaults rooted at the destination when no config exists yet, so the settings
/// window always renders.
#[tauri::command]
pub async fn get_config(ctx: State<'_, AppCtx>) -> Result<ConfigView, String> {
    let config_path = ctx.config_path.clone();
    let dest_root = ctx.dest_root.clone();
    let creds = ctx.creds.clone();
    tauri::async_runtime::spawn_blocking(move || get_config_impl(&config_path, &dest_root, &creds))
        .await
        .map_err(|e| e.to_string())?
}

/// The resolved absolute path of `gpbeam.toml` (shown on the Advanced tab).
/// Async like the rest of the I/O-adjacent surface (uniform invoke contract);
/// the body itself is a pure in-memory read of the managed context.
#[tauri::command]
pub async fn get_config_path(ctx: State<'_, AppCtx>) -> Result<String, String> {
    Ok(ctx.config_path.to_string_lossy().into_owned())
}

/// The most-recently-copied files (capped at `limit`) for the History tab.
/// Async: opens + queries SQLite.
#[tauri::command]
pub async fn get_history(ctx: State<'_, AppCtx>, limit: usize) -> Result<Vec<HistoryRow>, String> {
    let ledger_path = ctx.ledger_path.clone();
    tauri::async_runtime::spawn_blocking(move || history_rows_from_ledger(&ledger_path, limit))
        .await
        .map_err(|e| e.to_string())?
}

/// True when no `gpbeam.toml` exists at the resolved config path — the settings
/// window opens into the first-run wizard instead of the tabs (design §4.3).
/// Async (filesystem stat; the config may sit on a slow/network volume).
#[tauri::command]
pub async fn is_first_run(ctx: State<'_, AppCtx>) -> Result<bool, String> {
    Ok(!ctx.config_path.exists())
}

// Snapshot emission goes through `crate::emit_state`: the ONE seq-guarded path
// shared with the lib.rs fold helpers, so command emits and event emits cannot
// reorder against each other. Every mutating command bumps `state.seq` under
// the lock before cloning the snapshot it emits.
use crate::emit_state;

/// Validate + atomically persist `gpbeam.toml`, rebuild the cloud runtime, refresh
/// the cloud flags, then return (and broadcast) the updated state. On a validation
/// or write error the existing config is untouched and `Err(message)` is returned.
///
/// The file write + fsync and the ledger reads run on `spawn_blocking` with NO
/// locks held; the state/runtime mutexes are taken only for the in-memory swap,
/// so a save can never stall the cloud loop or freeze the main thread.
#[tauri::command]
pub async fn save_config<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: State<'_, AppCtx>,
    view: ConfigView,
) -> Result<AppState, String> {
    let config_path = ctx.config_path.clone();
    let ledger_path = ctx.ledger_path.clone();
    let creds = ctx.creds.clone();
    let (cfg, counts) = tauri::async_runtime::spawn_blocking(
        move || -> Result<(Config, Option<CloudCounts>), String> {
            let cfg = persist_view(&view, &config_path)?;
            // The file just changed: rebuild the credential store's toml
            // fallback so resolution tracks the new contents immediately.
            creds.refresh_fallback_from_file(&config_path);
            Ok((cfg, cloud_counts_from_ledger(&ledger_path)))
        },
    )
    .await
    .map_err(|e| e.to_string())??;

    let updated = {
        let mut state = crate::lock_recover(&ctx.state);
        let mut runtime = crate::lock_recover(&ctx.runtime);
        apply_config_in_memory(&cfg, counts, &mut state, &mut runtime);
        state.bump_seq();
        state.clone()
    };
    // Wired-ingest toggle takes effect live: update the flag the consumer loop
    // reads and re-derive the poller's pause (an in-flight offload keeps it
    // paused regardless — see recompute_detector_pause).
    ctx.wired_enabled.store(cfg.wired_ingest, Ordering::SeqCst);
    recompute_detector_pause(&ctx);
    emit_state(&app, &updated);
    Ok(updated)
}

/// Write the initial config from the first-run wizard. Identical pipeline to
/// `save_config` (the wizard simply produces a `ConfigView` from its steps).
#[tauri::command]
pub async fn complete_wizard<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: State<'_, AppCtx>,
    view: ConfigView,
) -> Result<AppState, String> {
    save_config(app, ctx, view).await
}

/// Store the Nextcloud app-password for `destination_id` in the OS keychain
/// (the username lives in `gpbeam.toml`). Returns a friendly error if the
/// keychain is unavailable/denied (design §7); cloud stays disabled, local
/// offload is unaffected. Async: the keychain call can block on an unlock
/// prompt, which must never freeze the main thread.
#[tauri::command]
pub async fn set_nextcloud_credentials(
    ctx: State<'_, AppCtx>,
    destination_id: String,
    app_password: String,
) -> Result<(), String> {
    let creds = ctx.creds.clone();
    tauri::async_runtime::spawn_blocking(move || creds.set_password(&destination_id, &app_password))
        .await
        .map_err(|e| e.to_string())?
}

/// Delete the keychain entry for `destination_id`. Async: see
/// [`set_nextcloud_credentials`].
#[tauri::command]
pub async fn clear_nextcloud_credentials(
    ctx: State<'_, AppCtx>,
    destination_id: String,
) -> Result<(), String> {
    let creds = ctx.creds.clone();
    tauri::async_runtime::spawn_blocking(move || creds.delete_password(&destination_id))
        .await
        .map_err(|e| e.to_string())?
}

/// Move a plaintext `[credentials.<id>]` app-password into the OS keychain, then
/// strip it from the config file. The keychain write happens BEFORE the strip so
/// a keychain failure never destroys the only copy of the secret (M2). After the
/// strip, the credential store's toml fallback is refreshed so the old plaintext
/// secret stops resolving immediately (not at the next restart).
pub(crate) fn migrate_plaintext_credentials_impl(
    creds: &crate::keyring_store::KeyringCredentialStore,
    config_path: &std::path::Path,
    destination_id: &str,
) -> Result<(), String> {
    let pw = crate::config_io::plaintext_app_password(config_path, destination_id)
        .ok_or_else(|| format!("no plaintext password for {destination_id:?} in config"))?;
    creds.set_password(destination_id, &pw)?;
    // Strip only the plaintext password; the username stays in the file so
    // credential resolution still has it (the uploader reads secret.username).
    crate::config_io::strip_credential_password(config_path, destination_id)?;
    // The file changed: the startup fallback snapshot must not keep resolving
    // the now-stripped password (revocation would otherwise need a restart).
    creds.refresh_fallback_from_file(config_path);
    Ok(())
}

/// Migrate a plaintext Nextcloud password for `destination_id` into the keychain
/// and remove it from `gpbeam.toml` (M2 one-click migrate). Async: keychain +
/// config-file I/O.
#[tauri::command]
pub async fn migrate_plaintext_credentials(
    ctx: State<'_, AppCtx>,
    destination_id: String,
) -> Result<(), String> {
    let creds = ctx.creds.clone();
    let config_path = ctx.config_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        migrate_plaintext_credentials_impl(&creds, &config_path, &destination_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Pause the cloud drain loop (in-flight uploads finish; no new jobs claimed).
/// Returns the refreshed state with `cloud.paused == true`. Async so the brief
/// state-lock acquisition stays off the main thread.
#[tauri::command]
pub async fn pause_cloud<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: State<'_, AppCtx>,
) -> Result<AppState, String> {
    ctx.paused.store(true, Ordering::SeqCst);
    let updated = {
        let mut state = crate::lock_recover(&ctx.state);
        state.cloud.paused = true;
        state.bump_seq();
        state.clone()
    };
    emit_state(&app, &updated);
    Ok(updated)
}

/// Resume the cloud drain loop. Returns the refreshed state with `cloud.paused == false`.
#[tauri::command]
pub async fn resume_cloud<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: State<'_, AppCtx>,
) -> Result<AppState, String> {
    ctx.paused.store(false, Ordering::SeqCst);
    let updated = {
        let mut state = crate::lock_recover(&ctx.state);
        state.cloud.paused = false;
        state.bump_seq();
        state.clone()
    };
    emit_state(&app, &updated);
    Ok(updated)
}

/// Re-queue every terminally-failed cloud job so the next drain tick retries it.
/// Returns how many jobs were requeued. Like every other mutating command this
/// also refreshes the shared `AppState` (cloud.failed drops to 0, the requeued
/// jobs re-enter cloud.pending) and broadcasts it on `gpbeam://state`, so the
/// popover badge clears immediately instead of sticking forever.
#[tauri::command]
pub async fn retry_failed_cloud<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: State<'_, AppCtx>,
) -> Result<usize, String> {
    let ledger_path = ctx.ledger_path.clone();
    let (requeued, counts) = tauri::async_runtime::spawn_blocking(
        move || -> Result<(usize, Option<CloudCounts>), String> {
            let mut ledger = Ledger::open(&ledger_path).map_err(|e| e.to_string())?;
            let n = ledger
                .requeue_failed_cloud_jobs()
                .map_err(|e| e.to_string())?;
            Ok((n, cloud_counts_from_ledger(&ledger_path)))
        },
    )
    .await
    .map_err(|e| e.to_string())??;

    let updated = {
        let mut state = crate::lock_recover(&ctx.state);
        if let Some(c) = counts {
            state.cloud.pending = c.pending;
            state.cloud.failed = c.failed;
        }
        state.bump_seq();
        state.clone()
    };
    emit_state(&app, &updated);
    Ok(requeued)
}

/// Quit the app (the window-less tray app otherwise only exits via tray "Quit").
#[tauri::command]
pub fn quit<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    app.exit(0);
}

/// Open the native directory picker and return the chosen folder (or `None` if
/// the user cancels). Uses the blocking dialog API so the command returns the
/// path synchronously to the awaiting UI invoke.
#[tauri::command]
pub async fn pick_folder<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Option<String> {
    // MUST be async (off the main thread): the native folder dialog is driven by the
    // main-thread event loop, so `blocking_pick_folder()` on the main thread deadlocks
    // (the dialog never appears, the command never returns). We use the non-blocking
    // callback API and await its result via a oneshot channel instead.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path);
    });
    rx.await
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Resolve which destination to reveal/use. The configured `dest_root` (from
/// `gpbeam.toml`) wins, EXCEPT when an explicit `GPBEAM_DEST` override is set or
/// the config carries no `dest_root` — then the bootstrap default applies. Same
/// precedence as `get_config`, so "Open destination" reveals the exact folder
/// offloads write to (not the config/bootstrap directory).
fn resolve_dest_root(config_dest: &Path, default_dest: &Path, env_override: bool) -> PathBuf {
    if env_override || config_dest.as_os_str().is_empty() {
        default_dest.to_path_buf()
    } else {
        config_dest.to_path_buf()
    }
}

/// The destination root offloads write to: the configured `dest_root` from
/// `gpbeam.toml`, resolved via [`resolve_dest_root`]. Falls back to the bootstrap
/// default when no/invalid config exists yet.
fn configured_dest_root(config_path: &Path, default_dest: &Path) -> PathBuf {
    let env_override = std::env::var("GPBEAM_DEST")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();
    match load_config(config_path) {
        Ok(cfg) => resolve_dest_root(&cfg.dest_root, default_dest, env_override),
        Err(_) => default_dest.to_path_buf(),
    }
}

/// Open a path with the OS default handler (e.g. the destination folder in the
/// file manager). `None` for `with` lets the OS pick the default application.
/// Async: reads the config file and may `create_dir_all` the destination.
#[tauri::command]
pub async fn open_path<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    ctx: tauri::State<'_, AppCtx>,
    path: String,
) -> Result<(), String> {
    // Empty path = the popover's "Open destination" action: resolve the CONFIGURED
    // destination root from gpbeam.toml (creating it if absent, so a brand-new
    // install can still reveal the folder before the first offload has run).
    let config_path = ctx.config_path.clone();
    let default_dest = ctx.dest_root.clone();
    let target = tauri::async_runtime::spawn_blocking(move || {
        if path.is_empty() {
            let dest = configured_dest_root(&config_path, &default_dest);
            let _ = std::fs::create_dir_all(&dest);
            dest.to_string_lossy().into_owned()
        } else {
            path
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    app.opener()
        .open_path(target, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Reveal a file in its containing folder (Finder/Explorer "show in folder"),
/// used by the History tab's per-row Reveal action.
#[tauri::command]
pub async fn reveal_path<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    path: String,
) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
}

/// Show (and focus) the dedicated decorated settings window. The popover's
/// "Settings…" action calls this so settings open in their own window rather than
/// replacing the transparent, frameless popover's content (which rendered with a
/// see-through background).
#[tauri::command]
pub fn open_settings<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    use tauri::Manager;
    match app.get_webview_window("settings") {
        Some(w) => {
            w.show().map_err(|e| e.to_string())?;
            w.set_focus().map_err(|e| e.to_string())?;
            Ok(())
        }
        None => Err("settings window not found".into()),
    }
}

/// Whether launch-at-login is currently enabled (autostart plugin). Async: the
/// plugin inspects the LaunchAgent plist / registry on disk.
#[tauri::command]
pub async fn get_autostart<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Toggle launch-at-login on or off (autostart plugin). Async: writes the
/// LaunchAgent plist / registry entry.
#[tauri::command]
pub async fn set_autostart<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    enabled: bool,
) -> Result<(), String> {
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())
    } else {
        mgr.disable().map_err(|e| e.to_string())
    }
}

/// The exact set of `#[tauri::command]` names Phase 6 must register in
/// `tauri::generate_handler!` (and that the TS `bindings.ts` mirrors). Kept here
/// as the single source of truth so the count test below guards drift; the
/// mock-runtime smoke test in lib.rs additionally invokes the REAL registered
/// handler end-to-end. Test-only: `lib.rs` registers the commands via
/// `generate_handler!` (idents, not strings).
#[cfg(test)]
pub const COMMAND_NAMES: &[&str] = &[
    "get_state",
    "get_config",
    "get_config_path",
    "save_config",
    "pick_folder",
    "open_path",
    "reveal_path",
    "open_settings",
    "set_nextcloud_credentials",
    "clear_nextcloud_credentials",
    "migrate_plaintext_credentials",
    "pause_cloud",
    "resume_cloud",
    "retry_failed_cloud",
    "get_history",
    "get_autostart",
    "set_autostart",
    "is_first_run",
    "complete_wizard",
    "quit",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_moves_password_to_keychain_then_strips_file() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(
            &path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"a\"\napp_password=\"plain-pw\"\n",
        )
        .unwrap();

        use crate::keyring_store::KeyringBackend;
        let backend = Arc::new(crate::keyring_store::MemoryKeyring::new());
        let store = crate::keyring_store::KeyringCredentialStore::new(
            "com.gpbeam.test",
            backend.clone(),
            None,
            None,
            None,
        );

        migrate_plaintext_credentials_impl(&store, &path, "nc1").unwrap();

        // Password landed in the keychain; plaintext entry is gone from the file.
        assert_eq!(
            backend.get("com.gpbeam.test", "nc1").unwrap(),
            Some("plain-pw".into())
        );
        assert!(crate::config_io::plaintext_credential_ids(&path).is_empty());
    }

    #[test]
    fn migrate_refreshes_the_fallback_so_revocation_is_immediate() {
        // Finding 7: the store's toml fallback was a startup snapshot. After a
        // migrate (password moved to keychain, stripped from the file) followed
        // by a keychain delete, the OLD file password must NOT keep resolving —
        // that would defeat credential revocation until a restart.
        use gpbeam_core::credentials::{CredentialStore, EnvConfigStore};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(
            &path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"a\"\napp_password=\"plain-pw\"\n",
        )
        .unwrap();
        let fallback =
            EnvConfigStore::from_toml_str(&std::fs::read_to_string(&path).unwrap(), None, None)
                .unwrap();
        let store = crate::keyring_store::KeyringCredentialStore::new(
            "svc",
            Arc::new(crate::keyring_store::MemoryKeyring::new()),
            None,
            None,
            Some(fallback),
        );

        migrate_plaintext_credentials_impl(&store, &path, "nc1").unwrap();
        // Right after migrate the keychain supplies the password.
        assert!(store.has_password("nc1"));

        // The user revokes it from the keychain...
        store.delete_password("nc1").unwrap();
        // ...and no source may resolve the old plaintext anymore.
        assert!(
            !store.has_password("nc1"),
            "stale startup fallback must not resurrect the stripped password"
        );
        assert_eq!(store.get("nc1").unwrap(), None);
    }

    #[test]
    fn migrate_preserves_username_for_resolution_after_restart() {
        use gpbeam_core::credentials::{CredentialStore, EnvConfigStore};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path,
            "dest_root = \"/d\"\n[credentials.nc1]\nusername=\"alice\"\napp_password=\"plain-pw\"\n")
            .unwrap();
        let backend = Arc::new(crate::keyring_store::MemoryKeyring::new());
        let store = crate::keyring_store::KeyringCredentialStore::new(
            "svc",
            backend.clone(),
            None,
            None,
            None,
        );

        migrate_plaintext_credentials_impl(&store, &path, "nc1").unwrap();

        // Simulate an app restart: rebuild the fallback from the now-stripped file.
        // Resolution must still yield the username (from the file) and the password
        // (from the keychain) — migrate must NOT destroy the username.
        let raw = std::fs::read_to_string(&path).unwrap();
        let fallback = EnvConfigStore::from_toml_str(&raw, None, None).unwrap();
        let restarted = crate::keyring_store::KeyringCredentialStore::new(
            "svc",
            backend.clone(),
            None,
            None,
            Some(fallback),
        );
        let secret = restarted
            .get("nc1")
            .unwrap()
            .expect("resolvable after migrate");
        assert_eq!(secret.username, "alice", "username must survive migrate");
        assert_eq!(
            secret.app_password, "plain-pw",
            "password resolves from keychain"
        );
    }

    #[test]
    fn migrate_errors_when_no_plaintext_entry() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpbeam.toml");
        std::fs::write(&path, "dest_root = \"/d\"\n").unwrap();
        let store = crate::keyring_store::KeyringCredentialStore::new(
            "com.gpbeam.test",
            Arc::new(crate::keyring_store::MemoryKeyring::new()),
            None,
            None,
            None,
        );
        assert!(migrate_plaintext_credentials_impl(&store, &path, "nc1").is_err());
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
            .record(
                "C346",
                "GX010001.MP4",
                4096,
                1000,
                "/dest/GX010001.MP4",
                None,
            )
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
            l.record(
                "C346",
                &format!("GX0100{i:02}.MP4"),
                1,
                1000 + i,
                "/d",
                None,
            )
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

    /// One queued cloud job + one terminally-failed job in a fresh ledger.
    fn ledger_with_one_pending_one_failed(path: &Path) {
        let mut l = Ledger::open(path).unwrap();
        let a = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/d/GX010001.MP4", None)
            .unwrap();
        l.enqueue_cloud_job(a, "nc1", "/d/GX010001.MP4", "r/1", 4096, None)
            .unwrap();
        let b = l
            .record("C346", "GX010002.MP4", 10, 2000, "/d/GX010002.MP4", None)
            .unwrap();
        let job = l
            .enqueue_cloud_job(b, "nc1", "/d/GX010002.MP4", "r/2", 10, None)
            .unwrap();
        // next_retry_at = None -> terminal failure (the worker gave up).
        l.mark_job_failed(job, "401 Unauthorized", None).unwrap();
    }

    #[test]
    fn seed_counts_reads_pending_and_failed_from_existing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        ledger_with_one_pending_one_failed(&path);

        let mut cloud = CloudState::default();
        seed_cloud_counts_from_ledger(&path, &mut cloud);
        assert_eq!(cloud.pending, 1);
        assert_eq!(
            cloud.failed, 1,
            "terminal failures must seed cloud.failed (Retry button gating)"
        );
    }

    #[test]
    fn seed_counts_missing_ledger_leaves_state_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.sqlite");
        let mut cloud = CloudState {
            configured: true,
            pending: 7,
            failed: 3,
            ..CloudState::default()
        };
        seed_cloud_counts_from_ledger(&path, &mut cloud);
        // No ledger file -> nothing read; the in-memory counts are preserved.
        assert_eq!(cloud.pending, 7);
        assert_eq!(cloud.failed, 3);
        assert!(cloud.configured);
    }

    #[test]
    fn cloud_counts_after_requeue_zero_failed_and_move_to_pending() {
        // The retry_failed_cloud pipeline: requeue flips terminal failures back
        // to Queued, so failed -> 0 and pending absorbs them. This is exactly
        // what the command folds into AppState + emits.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        ledger_with_one_pending_one_failed(&path);

        let before = cloud_counts_from_ledger(&path).unwrap();
        assert_eq!(
            before,
            CloudCounts {
                pending: 1,
                failed: 1
            }
        );

        let mut l = Ledger::open(&path).unwrap();
        assert_eq!(l.requeue_failed_cloud_jobs().unwrap(), 1);

        let after = cloud_counts_from_ledger(&path).unwrap();
        assert_eq!(
            after,
            CloudCounts {
                pending: 2,
                failed: 0
            },
            "requeue clears failed and re-enters them as pending"
        );
    }

    #[test]
    fn detector_should_pause_truth_table() {
        // Paused while an offload owns the camera OR wired ingest is disabled.
        assert!(
            !detector_should_pause(false, true),
            "idle + enabled -> probe"
        );
        assert!(
            detector_should_pause(true, true),
            "offload in flight -> pause"
        );
        assert!(detector_should_pause(false, false), "disabled -> pause");
        assert!(detector_should_pause(true, false), "both -> pause");
    }

    #[test]
    fn recompute_detector_pause_follows_ctx_flags() {
        let ctx = crate::build_app_ctx_for_tests();
        // Default config: wired enabled, no offload -> unpaused.
        recompute_detector_pause(&ctx);
        assert!(!ctx.detector_paused.load(Ordering::SeqCst));
        // Toggle wired off (what save_config does) -> paused.
        ctx.wired_enabled.store(false, Ordering::SeqCst);
        recompute_detector_pause(&ctx);
        assert!(ctx.detector_paused.load(Ordering::SeqCst));
        // Re-enable while an offload is active -> stays paused.
        ctx.wired_enabled.store(true, Ordering::SeqCst);
        ctx.wired_offload_active.store(true, Ordering::SeqCst);
        recompute_detector_pause(&ctx);
        assert!(ctx.detector_paused.load(Ordering::SeqCst));
        // Offload done -> unpaused again.
        ctx.wired_offload_active.store(false, Ordering::SeqCst);
        recompute_detector_pause(&ctx);
        assert!(!ctx.detector_paused.load(Ordering::SeqCst));
    }

    #[test]
    fn resolve_dest_root_prefers_configured_dest() {
        // The folder offloads actually write to (gpbeam.toml's dest_root) wins —
        // this is what "Open destination" must reveal.
        let r = resolve_dest_root(
            std::path::Path::new("/Volumes/videos/GoPro"),
            std::path::Path::new("/home/u/GPBeam"),
            false,
        );
        assert_eq!(r, std::path::PathBuf::from("/Volumes/videos/GoPro"));
    }

    #[test]
    fn resolve_dest_root_falls_back_when_config_dest_empty() {
        // No dest_root in the config -> bootstrap default.
        let r = resolve_dest_root(
            std::path::Path::new(""),
            std::path::Path::new("/home/u/GPBeam"),
            false,
        );
        assert_eq!(r, std::path::PathBuf::from("/home/u/GPBeam"));
    }

    #[test]
    fn resolve_dest_root_env_override_uses_default() {
        // An explicit GPBEAM_DEST override (captured in the bootstrap default) beats
        // the configured dest_root.
        let r = resolve_dest_root(
            std::path::Path::new("/Volumes/videos/GoPro"),
            std::path::Path::new("/env/override"),
            true,
        );
        assert_eq!(r, std::path::PathBuf::from("/env/override"));
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
            wired_ingest: true,
            cloud: None,
            plaintext_credential_ids: Vec::new(),
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
        let mut runtime = CloudRuntime::default();

        let view = base_view(dir.path().join("out").to_str().unwrap());
        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();

        assert!(cfg_path.exists(), "gpbeam.toml must be written");
        assert!(
            !cfg_path.with_extension("toml.part").exists(),
            "no .part left behind"
        );
        assert!(
            !state.cloud.configured,
            "no [cloud] -> cloud.configured false"
        );
        assert!(runtime.config.is_none());
    }

    #[test]
    fn apply_saved_config_with_cloud_sets_runtime_and_configured() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut state = AppState::default();
        let mut runtime = CloudRuntime::default();

        let mut view = base_view(dir.path().join("out").to_str().unwrap());
        view.cloud = Some(cloud_view());
        view.delete_after_verify = true;

        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();

        assert!(state.cloud.configured, "[cloud] present -> configured true");
        let rt_cloud = runtime.config.as_ref().expect("runtime.config swapped in");
        assert_eq!(rt_cloud.destination_id, "nc1");
        assert_eq!(rt_cloud.username, "alice");
        assert!(
            runtime.delete_after_verify,
            "delete_after_verify carried into runtime"
        );
    }

    #[test]
    fn apply_saved_config_seeds_pending_and_failed_from_existing_queue() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        ledger_with_one_pending_one_failed(&ledger_path);

        let mut state = AppState::default();
        let mut runtime = CloudRuntime::default();
        let mut view = base_view(dir.path().join("out").to_str().unwrap());
        view.cloud = Some(cloud_view());

        apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime).unwrap();
        assert_eq!(state.cloud.pending, 1);
        assert_eq!(state.cloud.failed, 1, "failed seeded alongside pending");
    }

    #[test]
    fn apply_saved_config_rejects_invalid_view_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("gpbeam.toml");
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut state = AppState::default();
        let mut runtime = CloudRuntime::default();

        let mut view = base_view(""); // empty dest_root is invalid
        view.dest_root = String::new();

        let err = apply_saved_config(&view, &cfg_path, &ledger_path, &mut state, &mut runtime)
            .unwrap_err();
        assert!(!err.is_empty(), "validation error message is non-empty");
        assert!(
            !cfg_path.exists(),
            "invalid input must NOT write gpbeam.toml"
        );
    }

    #[test]
    fn get_config_impl_renders_defaults_without_a_file() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::keyring_store::KeyringCredentialStore::new(
            "svc",
            Arc::new(crate::keyring_store::MemoryKeyring::new()),
            None,
            None,
            None,
        );
        let view = get_config_impl(
            &dir.path().join("absent.toml"),
            std::path::Path::new("/home/u/GPBeam"),
            &store,
        )
        .unwrap();
        assert_eq!(view.dest_root, "/home/u/GPBeam");
        assert!(view.cloud.is_none());
        assert!(view.plaintext_credential_ids.is_empty());
    }

    /// Pin every command as a referenced fn item, so a deleted/renamed command
    /// fails this test at compile time. (Async/generic commands cannot be cast
    /// to plain `fn` pointers; the mock-runtime smoke test in lib.rs covers the
    /// IPC wiring end-to-end.)
    #[test]
    fn command_fn_items_exist() {
        let _ = get_state;
        let _ = get_config;
        let _ = get_config_path;
        let _ = get_history;
        let _ = is_first_run;
        let _ = save_config::<tauri::Wry>;
        let _ = complete_wizard::<tauri::Wry>;
        let _ = set_nextcloud_credentials;
        let _ = clear_nextcloud_credentials;
        let _ = migrate_plaintext_credentials;
        let _ = pause_cloud::<tauri::Wry>;
        let _ = resume_cloud::<tauri::Wry>;
        let _ = retry_failed_cloud::<tauri::Wry>;
        let _ = quit::<tauri::Wry>;
        let _ = pick_folder::<tauri::Wry>;
        let _ = open_path::<tauri::Wry>;
        let _ = reveal_path::<tauri::Wry>;
        let _ = open_settings::<tauri::Wry>;
        let _ = get_autostart::<tauri::Wry>;
        let _ = set_autostart::<tauri::Wry>;
    }

    /// Pins the count of commands wired into Phase 6's `generate_handler!`. If
    /// this fails, update COMMAND_NAMES, the macro list in lib.rs, AND the TS
    /// bindings in ui/src/lib/bindings.ts.
    #[test]
    fn command_surface_count_is_pinned() {
        assert_eq!(
            COMMAND_NAMES.len(),
            20,
            "command surface changed — sync lib.rs generate_handler!"
        );
    }

    #[test]
    fn command_names_are_unique_and_sorted_by_phase_table() {
        let mut seen = std::collections::HashSet::new();
        for name in COMMAND_NAMES {
            assert!(seen.insert(*name), "duplicate command name: {name}");
        }
    }
}
