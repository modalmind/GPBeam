//! GPBeam M1 tray shell. A window-less menu-bar / tray app that runs the
//! `gpbeam-core` offload engine in a background worker, swaps a tray icon to
//! reflect idle/working/error, and fires native notifications on completion.
//! The rich popover/settings UI (and folder picker, cloud, history) is M3.

mod config_io;

use std::path::{Path, PathBuf};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpbeam_core::cloud::{build_uploader, worker::CloudWorker, CloudEvent};
use gpbeam_core::config::{config_path, load_config, Config};
use gpbeam_core::credentials::EnvConfigStore;
use gpbeam_core::error::CoreError;
use gpbeam_core::ledger::Ledger;
use gpbeam_core::orchestrator::RunSummary;
use gpbeam_core::orchestrator::{run_offload, RunEvent as Ev};

use crate::app_state::AppState;
use crate::cloud_runtime::CloudRuntime;
use crate::commands::AppCtx;
use crate::keyring_store::{KeyringBackend, KeyringCredentialStore, SystemKeyring};

mod cloud_runtime;
mod commands;
mod keyring_store;

mod app_state;

/// Lock a `Mutex`, recovering the guard even if a previous holder panicked while
/// holding it (L2). `AppState`/`CloudRuntime` are plain snapshots, so the
/// post-panic data is still structurally valid; bricking every command AND the
/// cloud loop permanently on one stray panic (the old `.expect(...)` behavior)
/// is strictly worse than proceeding with recovered state.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Last applied tray-icon state, encoded (0 idle / 1 working / 2 error;
/// `u8::MAX` = nothing applied yet). The tray is re-derived from EVERY folded
/// snapshot — including the ~100 `Progress` folds per file — so unchanged
/// states must skip the PNG decode + OS `set_icon` call instead of hammering
/// the tray API. Process-global like the tray itself.
static LAST_TRAY_CODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

/// Swap the tray icon for the current state ("idle" | "working" | "error").
/// Idempotent: re-applying the current state is a no-op (see [`LAST_TRAY_CODE`]).
fn set_tray_state(app: &AppHandle, state: &str) {
    let code: u8 = match state {
        "working" => 1,
        "error" => 2,
        _ => 0,
    };
    if LAST_TRAY_CODE.swap(code, Ordering::SeqCst) == code {
        return;
    }
    let bytes: &[u8] = match state {
        "working" => include_bytes!("../icons/working.png"),
        "error" => include_bytes!("../icons/error.png"),
        _ => include_bytes!("../icons/idle.png"),
    };
    if let (Some(tray), Ok(img)) = (app.tray_by_id("main"), Image::from_bytes(bytes)) {
        let _ = tray.set_icon(Some(img));
    }
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// The terminal toast for a finished run, or `None` when nothing was copied
/// (a no-op run must stay silent). On failures it returns the failure toast.
/// Pure and shared by the SD (`handle_mount`) and wired offload paths so their
/// completion notifications cannot drift.
fn summary_notification(s: &RunSummary) -> Option<(&'static str, String)> {
    if s.failed > 0 {
        return Some(("GPBeam", format!("{} file(s) failed to copy", s.failed)));
    }
    if s.copied > 0 {
        return Some((
            "GPBeam",
            format!("Copied {} file(s), {} skipped", s.copied, s.skipped),
        ));
    }
    None
}

/// The SINGLE source of truth for the tray icon, derived from the folded
/// `AppState` snapshot (never set per-event): `Error` status wins; otherwise an
/// in-flight offload run OR an in-flight cloud upload shows `"working"`; else
/// `"idle"`. Replaces the old scattered per-event `set_tray_state` calls, where
/// a cloud `Mirrored` mid-offload flipped the tray to idle and could clear an
/// error state. Pure; called by the broadcast helpers after every fold.
fn derive_tray_state(state: &AppState) -> &'static str {
    if state.status == crate::app_state::Status::Error {
        "error"
    } else if state.run.is_some() || state.cloud.uploading.is_some() {
        "working"
    } else {
        "idle"
    }
}

/// Whether a finished wired run must re-arm the detector so the still-connected
/// camera re-fires `CameraFound` (a retry without unplug/replug): a hard run
/// error or any per-file failure. Clean runs (including all-skipped no-ops)
/// must NOT re-arm — that would re-offload the same camera forever. Pure.
fn rearm_needed(summary: &Result<RunSummary, CoreError>) -> bool {
    !matches!(summary, Ok(s) if s.failed == 0)
}

/// Cap on CONSECUTIVE automatic wired retries per camera. A persistent in-run
/// failure (disk full, a clip that fails verify every time) would otherwise
/// loop offload → re-arm → offload every ~2s poll tick, raising an error toast
/// each cycle, for as long as the camera stays plugged in.
const MAX_WIRED_REARMS: u32 = 2;

/// Per-camera re-arm budget: transient failures get [`MAX_WIRED_REARMS`]
/// automatic retries; after that the camera is left alone until it is
/// physically re-plugged (a `CameraFound` we did not cause via `rearm` resets
/// the budget). Pure bookkeeping — the caller performs the actual re-arm.
struct RearmBudget {
    streak: std::collections::HashMap<std::net::IpAddr, u32>,
    rearmed_last: std::collections::HashSet<std::net::IpAddr>,
}

impl RearmBudget {
    fn new() -> Self {
        RearmBudget {
            streak: std::collections::HashMap::new(),
            rearmed_last: std::collections::HashSet::new(),
        }
    }

    /// Call when a `CameraFound` arrives, BEFORE the offload. An event we did
    /// not cause via re-arm means the camera was (re)plugged — fresh budget.
    fn note_event(&mut self, ip: std::net::IpAddr) {
        if !self.rearmed_last.contains(&ip) {
            self.streak.insert(ip, 0);
        }
    }

    /// Decide whether to re-arm after a run (`retry` = `rearm_needed(..)`),
    /// updating the per-camera bookkeeping.
    fn should_rearm(&mut self, ip: std::net::IpAddr, retry: bool) -> bool {
        if !retry {
            self.streak.insert(ip, 0);
            self.rearmed_last.remove(&ip);
            return false;
        }
        let streak = self.streak.entry(ip).or_insert(0);
        if *streak < MAX_WIRED_REARMS {
            *streak += 1;
            self.rearmed_last.insert(ip);
            true
        } else {
            // Budget exhausted: stop retrying until a physical replug.
            self.rearmed_last.remove(&ip);
            false
        }
    }
}

/// Map one `CloudEvent` to native-notification side effects ONLY. The tray icon
/// is derived from the folded snapshot in `broadcast_cloud_event` (single source
/// of truth, see `derive_tray_state`), and the full UI state flows on
/// `"gpbeam://state"`. The match stays exhaustive over the locked contract so a
/// new variant fails compilation here instead of being silently swallowed.
fn forward_cloud_event(app: &AppHandle, ev: CloudEvent) {
    match ev {
        // In-flight upload: activity shows via the derived tray; no notification.
        CloudEvent::Uploading { .. } => {}
        CloudEvent::Mirrored { file } => {
            notify(app, "GPBeam", &format!("Mirrored {file} to cloud"));
        }
        CloudEvent::CloudFailed { file, error } => {
            notify(app, "GPBeam cloud error", &format!("{file}: {error}"));
        }
        CloudEvent::Deleted { file } => {
            notify(app, "GPBeam", &format!("Freed card space: {file}"));
        }
        // NON-FATAL (the upload succeeded; only the post-upload card cleanup
        // failed): warn without an error-grade title, mirroring the reducer.
        CloudEvent::DeleteFailed { file, error } => {
            notify(
                app,
                "GPBeam",
                &format!("card delete-after-verify failed for {file}: {error}"),
            );
        }
    }
}

/// Lock the shared `AppState`, fold one `RunEvent` into it at `now_unix`, bump
/// the emit sequence UNDER the lock, and return a clone of the resulting
/// snapshot for emission. Pure (no `AppHandle`) so the offload->state mapping is
/// unit-testable; `broadcast_run_event` emits the returned snapshot.
fn fold_run_event(state: &Arc<Mutex<AppState>>, ev: &Ev, now_unix: i64) -> AppState {
    let mut st = lock_recover(state);
    st.apply_run_event(ev, now_unix);
    st.bump_seq();
    st.clone()
}

/// Lock the shared `AppState`, fold one `CloudEvent` into it, bump the emit
/// sequence under the lock, and return a clone of the resulting snapshot for
/// emission. Pure; `broadcast_cloud_event` emits it.
fn fold_cloud_event(state: &Arc<Mutex<AppState>>, ev: &CloudEvent) -> AppState {
    let mut st = lock_recover(state);
    st.apply_cloud_event(ev);
    st.bump_seq();
    st.clone()
}

/// Lock the shared `AppState`, fold a HARD offload error (mkdir/ledger-open/run
/// `Err` — no `RunComplete` will ever arrive) via `AppState::apply_run_error`,
/// bump the emit sequence under the lock, and return the snapshot. Pure half of
/// `fold_run_error`, split out so it is unit-testable without an `AppHandle`.
fn fold_run_error_state(state: &Arc<Mutex<AppState>>, msg: &str) -> AppState {
    let mut st = lock_recover(state);
    st.apply_run_error(msg);
    st.bump_seq();
    st.clone()
}

/// Fold a hard offload error into shared state, re-derive the tray icon, and
/// broadcast the snapshot. Called on EVERY hard-error path of both offload
/// drivers so the popover never keeps a phantom in-flight run after a
/// destination/ledger/run failure.
fn fold_run_error(app: &AppHandle, state: &Arc<Mutex<AppState>>, msg: &str) {
    let snap = fold_run_error_state(state, msg);
    set_tray_state(app, derive_tray_state(&snap));
    emit_state(app, &snap);
}

/// Fold one `RunEvent`, set the tray from the DERIVED snapshot, and broadcast it.
/// The only run-event path to the tray + `gpbeam://state`, shared by the SD and
/// wired drivers.
fn broadcast_run_event(app: &AppHandle, state: &Arc<Mutex<AppState>>, ev: &Ev, now_unix: i64) {
    let snap = fold_run_event(state, ev, now_unix);
    set_tray_state(app, derive_tray_state(&snap));
    emit_state(app, &snap);
}

/// Fold one `CloudEvent`, set the tray from the derived snapshot, and broadcast
/// it. The only cloud-event path to the tray + `gpbeam://state`.
fn broadcast_cloud_event(app: &AppHandle, state: &Arc<Mutex<AppState>>, ev: &CloudEvent) {
    let snap = fold_cloud_event(state, ev);
    set_tray_state(app, derive_tray_state(&snap));
    emit_state(app, &snap);
}

/// Sequence of the last snapshot actually emitted on `gpbeam://state`.
/// Process-global because every window shares the one event channel.
static LAST_EMITTED_SEQ: AtomicU64 = AtomicU64::new(0);

/// Out-of-order emit guard: atomically raise `last_emitted` to `seq` and return
/// whether THIS frame advanced it. Frames whose seq is <= the last emitted one
/// are stale (the fold happened under the lock, but the emit raced after the
/// lock was released) and must be dropped so an older frame can't mask a newer,
/// possibly terminal, one. Pure over an injected counter for unit tests.
fn should_emit(last_emitted: &AtomicU64, seq: u64) -> bool {
    last_emitted.fetch_max(seq, Ordering::SeqCst) < seq
}

/// Emit a full `AppState` snapshot to every window on `"gpbeam://state"`. The UI
/// replaces its store wholesale on each event (no TS-side reducer). Stale frames
/// (see `should_emit`) are dropped — every caller MUST have bumped `seq` under
/// the state lock (the fold helpers and the mutating commands do), otherwise its
/// frame would be permanently dropped. Serialization failures are swallowed: a
/// dropped frame is recoverable (the next event re-emits the full state).
/// Generic over the runtime so the runtime-generic commands share this single
/// guarded path (one global guard = one total order).
pub(crate) fn emit_state<R: tauri::Runtime>(app: &AppHandle<R>, snapshot: &AppState) {
    if !should_emit(&LAST_EMITTED_SEQ, snapshot.seq) {
        return;
    }
    let _ = app.emit("gpbeam://state", snapshot);
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The BOOTSTRAP destination: `$GPBEAM_DEST`, else `~/GPBeam`.
///
/// Note the deliberate divergence from the CLI: the tray app anchors its config
/// and ledger at this bootstrap root even when `gpbeam.toml`'s `dest_root`
/// points somewhere else (offloads honor the configured `dest_root`; the ledger
/// stays at `<bootstrap>/.gpbeam-ledger.sqlite`), whereas the CLI keeps the
/// ledger under the *configured* `dest_root`. This stands on purpose: existing
/// GUI installs already have their dedup/cloud-queue history in the bootstrap
/// ledger, and silently re-anchoring it on a Settings change would "forget"
/// every prior import (re-copy everything) and orphan queued cloud jobs. The
/// README documents the divergence.
fn dest_root() -> PathBuf {
    match std::env::var("GPBEAM_DEST") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home_dir().join("GPBeam"),
    }
}

/// The app-wide ledger location under the BOOTSTRAP destination — see
/// [`dest_root`] for why this intentionally does NOT follow the configured
/// `dest_root` (install continuity; the CLI differs).
fn ledger_path(dest: &Path) -> PathBuf {
    dest.join(".gpbeam-ledger.sqlite")
}

/// Load `gpbeam.toml` from `$GPBEAM_CONFIG` (or `<dest>/gpbeam.toml`). On any
/// error — missing file, parse failure — fall back to the exact M1 defaults so
/// a no-config install behaves identically to M1. The destination chosen in the
/// GUI (wizard/Settings, stored as the config's `dest_root`) is honored; an
/// explicit `$GPBEAM_DEST` still overrides it, and a config missing a dest_root
/// falls back to the default.
fn load_or_default_config(dest: &Path) -> Config {
    let path = config_path(std::env::var("GPBEAM_CONFIG").ok(), dest);
    match load_config(&path) {
        Ok(mut cfg) => {
            // Honor the destination chosen in the GUI wizard/Settings. An explicit
            // GPBEAM_DEST env still wins (M1/M2 power-user override); a config with
            // no dest_root falls back to the default destination.
            let env_dest = std::env::var("GPBEAM_DEST").ok().filter(|s| !s.is_empty());
            if env_dest.is_some() || cfg.dest_root.as_os_str().is_empty() {
                cfg.dest_root = dest.to_path_buf();
            }
            cfg
        }
        Err(_) => Config::new(dest.to_path_buf()),
    }
}

/// Assemble the managed `AppCtx` from a loaded `Config`, the resolved paths, and a
/// keychain backend. Kept pure (no `AppHandle`) so it is unit-testable: `run()`
/// passes the real `SystemKeyring`; tests pass a `MemoryKeyring`. Credential
/// precedence (env > keychain > toml fallback) lives in `KeyringCredentialStore`;
/// here we just thread the env overrides and the optional toml fallback through.
#[allow(clippy::too_many_arguments)]
fn build_app_ctx(
    cfg: &Config,
    dest_root: PathBuf,
    config_path: PathBuf,
    ledger_path: PathBuf,
    backend: Arc<dyn KeyringBackend>,
    env_username: Option<String>,
    env_app_password: Option<String>,
    fallback: Option<EnvConfigStore>,
) -> AppCtx {
    let creds = KeyringCredentialStore::new(
        "com.gpbeam.app",
        backend,
        env_username,
        env_app_password,
        fallback,
    );
    let wired = wired_ingest_enabled(cfg);
    AppCtx {
        state: Arc::new(Mutex::new(AppState::default())),
        paused: Arc::new(AtomicBool::new(false)),
        wired_enabled: Arc::new(AtomicBool::new(wired)),
        wired_offload_active: Arc::new(AtomicBool::new(false)),
        // The poller starts paused when wired ingest is disabled (no probe
        // traffic); save_config re-derives it on every wired_ingest toggle.
        detector_paused: Arc::new(AtomicBool::new(commands::detector_should_pause(
            false, wired,
        ))),
        creds: Arc::new(creds),
        runtime: Arc::new(Mutex::new(CloudRuntime::from_config(cfg))),
        offload_lock: Arc::new(tokio::sync::Mutex::new(())),
        dest_root,
        config_path,
        ledger_path,
    }
}

/// A throwaway `AppCtx` over `/tmp` paths + a `MemoryKeyring` for unit tests
/// (used by commands.rs's recompute test; the IPC smoke test builds its own
/// over a real tempdir).
#[cfg(test)]
pub(crate) fn build_app_ctx_for_tests() -> AppCtx {
    let cfg = Config::new(PathBuf::from("/tmp/gpbeam-test-dest"));
    build_app_ctx(
        &cfg,
        PathBuf::from("/tmp/gpbeam-test-dest"),
        PathBuf::from("/tmp/gpbeam-test-dest/gpbeam.toml"),
        PathBuf::from("/tmp/gpbeam-test-dest/.gpbeam-ledger.sqlite"),
        Arc::new(crate::keyring_store::MemoryKeyring::new()),
        None,
        None,
        None,
    )
}

/// Startup crash recovery + (pending, failed) counts for the cold-window seed.
/// Opens the ledger, resets any job orphaned in `Uploading` by a prior crash
/// back to `Queued` (H1), then returns the pending count AND the terminal
/// failed count — without the failed count a restart would show 0 failed and
/// the popover's Retry button (disabled when failed == 0) would be unreachable.
/// A missing/unopenable ledger (fresh install) yields (0, 0). Kept separate
/// from `seed_cloud_state` so the recovery is unit-testable without a Tauri
/// runtime.
fn recover_and_count_pending(ledger_path: &Path) -> (usize, usize) {
    let mut ledger = match Ledger::open(ledger_path) {
        Ok(l) => l,
        Err(_) => return (0, 0),
    };
    let _ = ledger.reclaim_orphaned_uploading();
    let pending = ledger.pending_cloud_count().unwrap_or(0);
    let failed = ledger.failed_cloud_count().unwrap_or(0) as usize;
    (pending, failed)
}

/// Seed a fresh `AppState.cloud` from the loaded config + the ledger's pending
/// and terminal-failed counts at startup, so a window opened before any event
/// still shows the right configured/pending/failed counts. Pure: the ledger
/// counts are read by the caller and passed in. `configured` is true iff a
/// `[cloud]` table exists.
fn seed_cloud_state(state: &mut AppState, cfg: &Config, pending: usize, failed: usize) {
    state.cloud.configured = cfg.cloud.is_some();
    state.cloud.pending = pending;
    state.cloud.failed = failed;
}

/// Initial value for the LIVE `wired_enabled` toggle in `AppCtx` (the
/// `wired_ingest` config flag, default true). The USB camera poller is now
/// always spawned; this flag gates each `CameraFound` event (and is re-checked
/// inside `run_wired_offload_for_camera` from the freshly-loaded config), and
/// `save_config` updates it in place — so flipping the Settings toggle takes
/// effect without a restart. Pure so the decision stays unit-tested.
fn wired_ingest_enabled(cfg: &Config) -> bool {
    cfg.wired_ingest
}

/// The single long-lived cloud-mirror loop. Ticks every 5s; on each tick it reads
/// the swappable `CloudRuntime` and the `paused` flag from the managed `AppCtx`. If
/// `should_drain` is false it idles; otherwise it builds an uploader through the
/// keychain-backed credential store, drains the queue via `CloudWorker`, folds each
/// `CloudEvent` into the shared `AppState` (broadcasting `gpbeam://state`), and runs
/// the tray-icon/notification side effects. `save_config` swaps `runtime.config`
/// in place, so the next tick uses the new settings — no task abort needed.
fn spawn_cloud_loop(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        // Once-guard for uploader-build failures: a persistent misconfiguration
        // (missing app password, non-loopback http base URL) would otherwise
        // toast "GPBeam cloud disabled" EVERY 5s tick while jobs are pending.
        // See `cloud_runtime::should_surface_build_error`.
        let mut last_build_error: Option<String> = None;
        loop {
            ticker.tick().await;

            let ctx = app.state::<AppCtx>();
            let paused = ctx.paused.load(std::sync::atomic::Ordering::SeqCst);
            let runtime = lock_recover(&ctx.runtime).clone();

            if !cloud_runtime::should_drain(paused, &runtime) {
                continue;
            }
            let cloud = match runtime.config.clone() {
                Some(c) => c,
                None => continue,
            };

            // L4: skip the (TLS-handshaking) uploader build on idle ticks. With
            // Auto mirror enabled but an empty queue this loop fires every 5s, so
            // a cheap pending-count check avoids rebuilding a reqwest Client for
            // nothing. Fail OPEN: if the count can't be read, fall through and let
            // the worker (which re-checks + reclaims orphans) make the decision.
            let has_work = Ledger::open(&ctx.ledger_path)
                .ok()
                .and_then(|l| l.pending_cloud_count().ok())
                .map(|n| n > 0)
                .unwrap_or(true);
            if !has_work {
                continue;
            }

            // Build the uploader through the keychain-backed credential store.
            let uploader = match build_uploader(&cloud, ctx.creds.as_ref()) {
                Ok(u) => {
                    // Reset the once-guard so the SAME error re-notifies if the
                    // build regresses again later.
                    last_build_error = None;
                    u
                }
                Err(e) => {
                    // Misconfigured cloud (e.g. missing app password, non-loopback
                    // http base URL) must NOT take down the offload path: surface
                    // ONCE per distinct message and retry next tick.
                    let msg = e.to_string();
                    if cloud_runtime::should_surface_build_error(&mut last_build_error, &msg) {
                        notify(&app, "GPBeam cloud disabled", &msg);
                    }
                    continue;
                }
            };

            let worker = CloudWorker::new(
                ctx.ledger_path.clone(),
                uploader,
                cloud.destination_id.clone(),
                cloud.max_concurrency,
                cloud.max_attempts,
                runtime.delete_after_verify,
            );

            let app2 = app.clone();
            let state = ctx.state.clone();
            let mut emit = move |ev: CloudEvent| {
                // Fold into shared state, derive tray + broadcast, then notify.
                broadcast_cloud_event(&app2, &state, &ev);
                forward_cloud_event(&app2, ev);
            };
            if let Err(e) = worker.run_until_drained(&mut emit).await {
                let _ = app.emit("gpbeam://cloud", format!("worker error: {e}"));
            }
        }
    });
}

/// Run one offload pass for a freshly mounted volume. Blocking I/O — call via
/// `spawn_blocking` so the async runtime is never stalled. Each `RunEvent` folds
/// into the shared `AppState` and is broadcast to the UI on `"gpbeam://state"`;
/// the tray icon + notifications still follow the terminal summary.
fn handle_mount(app: &AppHandle, state: &Arc<Mutex<AppState>>, mount: PathBuf) {
    // Ignore non-GoPro volumes (thumb drives, phones, etc.) before any side
    // effects: no tray flash, no destination dir, no ledger for a random disk.
    if !gpbeam_core::gopro::is_gopro_card(&mount) {
        return;
    }
    // M6: serialize against the wired path (and any other SD mount) sharing this
    // dest_root/ledger. handle_mount runs on a spawn_blocking thread, so a
    // blocking acquire is correct here; the guard is held across the offload.
    let ctx = app.state::<AppCtx>();
    let _offload_guard = ctx.offload_lock.blocking_lock();
    let dest = dest_root();
    if let Err(e) = std::fs::create_dir_all(&dest) {
        let msg = format!("cannot create destination: {e}");
        notify(app, "GPBeam error", &msg);
        fold_run_error(app, state, &msg);
        return;
    }
    let cfg = load_or_default_config(&dest);
    let mut ledger = match Ledger::open(&ledger_path(&dest)) {
        Ok(l) => l,
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
            fold_run_error(app, state, &e.to_string());
            return;
        }
    };

    // The tray flips to "working" via the derived state of the first folded
    // event (CardDetected/Scanned) — no per-driver set_tray_state anymore.
    let app2 = app.clone();
    let state2 = state.clone();
    let summary = run_offload(&mount, &cfg, &mut ledger, &mut |e: Ev| {
        // Fold every event into the shared AppState, derive the tray, broadcast.
        broadcast_run_event(&app2, &state2, &e, cloud_runtime::now_unix());
    });

    match summary {
        Ok(s) => {
            // Terminal tray state came from folding RunComplete (Idle/Error).
            if let Some((title, body)) = summary_notification(&s) {
                notify(app, title, &body);
            }
        }
        Err(e) => {
            // Hard run error: no RunComplete will arrive — clear the phantom
            // in-flight run, flip status/tray to error, broadcast.
            notify(app, "GPBeam error", &e.to_string());
            fold_run_error(app, state, &e.to_string());
        }
    }
}

/// Run one wired (USB GoPro) offload pass for a freshly-detected camera at `ip`.
/// Async — driven on the tokio runtime (NOT `spawn_blocking`): `run_wired_offload`
/// is itself async I/O over HTTP. Each `RunEvent` folds into the shared `AppState`
/// and broadcasts on `"gpbeam://state"` using the SAME M3 helpers as the SD path;
/// the terminal tray icon + notification reuse the shared Task 6.2 helpers, so the
/// two paths are behaviorally identical at the boundary. Cloud jobs enqueued by
/// the offload are drained by the existing M2 cloud loop, unchanged.
///
/// Returns `true` when the run needs a retry (hard `Err` or per-file failures,
/// see `rearm_needed`): the caller re-arms the detector for `ip` so the
/// still-connected camera re-fires `CameraFound` on the next poll tick.
/// Returns `false` on clean runs AND on the pre-offload local failures
/// (destination/ledger) — those would fail identically every ~2s tick and loop
/// an error toast forever; like before re-arm existed, they wait for a replug.
async fn run_wired_offload_for_camera(
    app: &AppHandle,
    state: &Arc<Mutex<AppState>>,
    ip: std::net::IpAddr,
) -> bool {
    // M6: serialize against the SD path (handle_mount) sharing this dest_root/
    // ledger. This future is async, so an awaited acquire keeps it Send; the
    // guard is held across the whole wired offload.
    let ctx = app.state::<AppCtx>();
    let _offload_guard = ctx.offload_lock.lock().await;
    let dest = dest_root();
    // Live wired_ingest toggle (re-checked per run, not just at startup): a
    // Settings save that disabled wired ingest after this CameraFound was
    // queued must turn the run into a no-op. No retry — false.
    let cfg = load_or_default_config(&dest);
    if !cfg.wired_ingest {
        return false;
    }
    if let Err(e) = std::fs::create_dir_all(&dest) {
        let msg = format!("cannot create destination: {e}");
        notify(app, "GPBeam error", &msg);
        fold_run_error(app, state, &msg);
        return false;
    }
    let mut ledger = match Ledger::open(&ledger_path(&dest)) {
        Ok(l) => l,
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
            fold_run_error(app, state, &e.to_string());
            return false;
        }
    };

    // The tray flips to "working" via the derived state of the first folded
    // event — no per-driver set_tray_state anymore.
    let client = gpbeam_core::wired::client::GoProClient::new(ip);
    let app2 = app.clone();
    let state2 = state.clone();
    let summary =
        gpbeam_core::wired::offload::run_wired_offload(&client, &cfg, &mut ledger, &mut |e: Ev| {
            // Fold every event into the shared AppState, derive the tray, and
            // broadcast the snapshot (identical to handle_mount's emitter).
            broadcast_run_event(&app2, &state2, &e, cloud_runtime::now_unix());
        })
        .await;

    let rearm = rearm_needed(&summary);
    match summary {
        Ok(s) => {
            // Terminal tray state came from folding RunComplete (Idle/Error).
            if let Some((title, body)) = summary_notification(&s) {
                notify(app, title, &body);
            }
        }
        Err(e) => {
            // Hard run error: fold BEFORE returning true so the popover drops
            // the phantom in-flight run even though the detector will re-arm.
            notify(app, "GPBeam error", &e.to_string());
            fold_run_error(app, state, &e.to_string());
        }
    }
    rearm
}

/// The application's REAL command handler — the single `generate_handler!`
/// list. Factored out of `run()` and generic over the runtime so the
/// `ipc_smoke` tests register the EXACT production list on a `MockRuntime`
/// app: a command rename/removal that desyncs this list from `COMMAND_NAMES`
/// or `ui/src/lib/bindings.ts` fails a test instead of dying at runtime.
fn invoke_handler<R: tauri::Runtime>(
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        commands::get_state,
        commands::get_config,
        commands::get_config_path,
        commands::save_config,
        commands::pick_folder,
        commands::open_path,
        commands::reveal_path,
        commands::open_settings,
        commands::set_nextcloud_credentials,
        commands::clear_nextcloud_credentials,
        commands::migrate_plaintext_credentials,
        commands::pause_cloud,
        commands::resume_cloud,
        commands::retry_failed_cloud,
        commands::get_history,
        commands::get_autostart,
        commands::set_autostart,
        commands::is_first_run,
        commands::complete_wizard,
        commands::quit,
    ]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Resolve paths + load config ONCE, before the builder, so we can seed the
    // managed AppCtx (and its AppState/CloudRuntime) up front.
    let dest = dest_root();
    let cfg = load_or_default_config(&dest);
    let cfg_path = config_path(std::env::var("GPBEAM_CONFIG").ok(), &dest);
    let led_path = ledger_path(&dest);

    // Keychain-backed credential store: env (GPBEAM_NC_*) > keychain > toml fallback.
    let env_username = std::env::var("GPBEAM_NC_USERNAME").ok();
    let env_app_password = std::env::var("GPBEAM_NC_APP_PASSWORD").ok();
    let fallback = std::fs::read_to_string(&cfg_path).ok().and_then(|s| {
        EnvConfigStore::from_toml_str(&s, env_username.clone(), env_app_password.clone()).ok()
    });
    let ctx = build_app_ctx(
        &cfg,
        dest.clone(),
        cfg_path.clone(),
        led_path.clone(),
        Arc::new(SystemKeyring),
        env_username,
        env_app_password,
        fallback,
    );

    // Seed AppState.cloud from config + ledger pending/failed counts for a cold
    // window. recover_and_count_pending also reclaims any job orphaned in
    // Uploading by a prior crash (H1) before the counts are read.
    {
        let (pending, failed) = recover_and_count_pending(&led_path);
        let mut st = lock_recover(&ctx.state);
        seed_cloud_state(&mut st, &cfg, pending, failed);
    }

    tauri::Builder::default()
        // single-instance MUST be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("settings") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(ctx)
        .invoke_handler(invoke_handler())
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            #[cfg(desktop)]
            {
                use tauri_plugin_autostart::MacosLauncher;
                app.handle().plugin(tauri_plugin_autostart::init(
                    MacosLauncher::LaunchAgent,
                    None::<Vec<&str>>,
                ))?;
                app.handle().plugin(tauri_plugin_positioner::init())?;
            }

            let idle = Image::from_bytes(include_bytes!("../icons/idle.png"))?;
            let open_i = MenuItem::with_id(app, "open", "Open GPBeam", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open_i, &sep, &quit_i])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(idle)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("settings") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("popover") {
                            use tauri_plugin_positioner::{Position, WindowExt};
                            // Always show at the tray; dismissal is handled by the
                            // popover's focus-lost handler (click outside to close).
                            let _ = w.move_window(Position::TrayCenter);
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Background worker: poll for removable mounts and offload each.
            let handle = app.handle().clone();
            let state = app.state::<AppCtx>().state.clone();
            tauri::async_runtime::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                tauri::async_runtime::spawn(gpbeam_core::detect::poll_removable_mounts(tx));
                while let Some(mount) = rx.recv().await {
                    let h = handle.clone();
                    let st = state.clone();
                    // run_offload is blocking I/O -> keep it off the async runtime.
                    let _ =
                        tauri::async_runtime::spawn_blocking(move || handle_mount(&h, &st, mount))
                            .await;
                }
            });

            // M4: USB-wired GoPro detector. Spawned UNCONDITIONALLY; the live
            // `wired_enabled` toggle in AppCtx (seeded from the startup config,
            // updated by save_config) gates each CameraFound event instead, so
            // flipping the Settings toggle takes effect without a restart.
            // On each CameraFound we run the async wired offload ON the tokio runtime
            // (NOT spawn_blocking — it is async HTTP I/O), folding every RunEvent into
            // the shared AppState + emitting "gpbeam://state" exactly like the SD path.
            // Cloud jobs it enqueues are drained by the M2 cloud loop below, unchanged.
            {
                let handle = app.handle().clone();
                let state = app.state::<AppCtx>().state.clone();
                // The Open GoPro HTTP server handles ONE client at a time, so the poller
                // must NOT probe the camera while an offload is downloading from it —
                // and it must not probe at all while wired ingest is disabled. Both
                // inputs live in AppCtx; `recompute_detector_pause` re-derives the
                // shared pause flag whenever either changes (here and in save_config),
                // so a disabled toggle can never leave the pause stuck on.
                let detector_paused = app.state::<AppCtx>().detector_paused.clone();
                tauri::async_runtime::spawn(async move {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                    // Re-arm handle: a failed offload forgets the camera's IP from the
                    // poller's de-bounce set, so the STILL-CONNECTED camera re-fires
                    // CameraFound on the next tick and the offload is retried without
                    // an unplug/replug. Clean runs never re-arm (no infinite loop).
                    let detector = gpbeam_core::wired::detect::DetectorHandle::new();
                    tauri::async_runtime::spawn(
                        gpbeam_core::wired::detect::poll_for_camera_with_rearm(
                            tx,
                            detector_paused,
                            detector.clone(),
                        ),
                    );
                    // Bounded retries: a persistently failing camera must not
                    // loop offload + error toast every ~2s tick (see RearmBudget).
                    let mut budget = RearmBudget::new();
                    while let Some(found) = rx.recv().await {
                        let ctx = handle.state::<AppCtx>();
                        // Live wired_ingest toggle: drop CameraFound events while
                        // disabled. Checked BEFORE flagging the offload active so a
                        // disabled toggle can never wedge the pause bookkeeping.
                        if !ctx.wired_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                            continue;
                        }
                        budget.note_event(found.ip);
                        // One camera at a time (design §5): each detection runs to completion
                        // before the next is handled. Pause probing so the offload owns the camera.
                        ctx.wired_offload_active
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        commands::recompute_detector_pause(&ctx);
                        let retry = run_wired_offload_for_camera(&handle, &state, found.ip).await;
                        let ctx = handle.state::<AppCtx>();
                        ctx.wired_offload_active
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        commands::recompute_detector_pause(&ctx);
                        if budget.should_rearm(found.ip, retry) {
                            detector.rearm(found.ip);
                        }
                    }
                });
            }

            // M3: one long-lived cloud-mirror loop. It self-guards each tick via
            // `should_drain`, so with no [cloud] table (or mirror_mode = Off) it is
            // an idle no-op and the process behaves like M1/M2 defaults. The runtime
            // it reads is seeded in build_app_ctx and swapped live by save_config.
            spawn_cloud_loop(&app.handle().clone());

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building GPBeam")
        .run(|app, event| {
            if let RunEvent::WindowEvent { event, label, .. } = event {
                match event {
                    // Window-less app: closing a window only HIDES it (the tray
                    // "Quit" is the real exit). We must both prevent the close AND
                    // hide — prevent_close alone leaves the window stuck on screen.
                    WindowEvent::CloseRequested { api, .. } => {
                        if label == "popover" || label == "settings" {
                            api.prevent_close();
                            if let Some(w) = app.get_webview_window(&label) {
                                let _ = w.hide();
                            }
                        }
                    }
                    // The tray popover auto-dismisses when it loses focus (i.e. the
                    // user clicks anywhere outside it), like a native menu-bar popover.
                    WindowEvent::Focused(false) if label == "popover" => {
                        if let Some(w) = app.get_webview_window("popover") {
                            let _ = w.hide();
                        }
                    }
                    _ => {}
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring_store::MemoryKeyring;
    use gpbeam_core::config::{CloudConfig, CloudKind, Config, MirrorMode};
    use std::sync::Arc;

    fn cfg_with_cloud() -> Config {
        let mut cfg = Config::new(std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        cfg.delete_after_verify = true;
        cfg.cloud = Some(CloudConfig {
            kind: CloudKind::Nextcloud,
            destination_id: "nc1".into(),
            base_url: "https://example.com".into(),
            username: "alice".into(),
            remote_root: "/GPBeam".into(),
            mirror_mode: MirrorMode::Auto,
            chunk_threshold: 10_000_000,
            tls_ca_pem: None,
            max_concurrency: 2,
            max_attempts: 3,
        });
        cfg
    }

    #[test]
    fn lock_recover_yields_guard_after_poison() {
        // L2: a panic while holding the lock poisons it; lock_recover must still
        // hand back the (intact) data instead of propagating the poison, so one
        // stray panic can't permanently brick the cloud loop and all commands.
        let m = std::sync::Arc::new(std::sync::Mutex::new(7i32));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(m.lock().is_err(), "mutex is poisoned after the panic");
        let g = lock_recover(&m);
        assert_eq!(*g, 7, "data is recovered intact");
    }

    #[test]
    fn build_app_ctx_seeds_runtime_from_cloud_config() {
        let cfg = cfg_with_cloud();
        let backend = Arc::new(MemoryKeyring::new());
        let ctx = build_app_ctx(
            &cfg,
            std::path::PathBuf::from("/tmp/gpbeam-test-dest"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/gpbeam.toml"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/.gpbeam-ledger.sqlite"),
            backend,
            None, // env_username
            None, // env_app_password
            None, // fallback EnvConfigStore
        );
        let rt = ctx.runtime.lock().unwrap();
        assert!(rt.config.is_some());
        assert!(rt.delete_after_verify);
        drop(rt);
        assert_eq!(
            ctx.dest_root,
            std::path::PathBuf::from("/tmp/gpbeam-test-dest")
        );
        assert!(!ctx.paused.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            ctx.wired_enabled.load(std::sync::atomic::Ordering::SeqCst),
            "wired_enabled seeded true from the default config"
        );
        // Fresh AppState defaults to Idle with no run.
        let st = ctx.state.lock().unwrap();
        assert_eq!(st.status, crate::app_state::Status::Idle);
        assert!(st.run.is_none());
    }

    #[test]
    fn build_app_ctx_seeds_wired_enabled_false_from_config() {
        let mut cfg = Config::new(std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        cfg.wired_ingest = false;
        let ctx = build_app_ctx(
            &cfg,
            std::path::PathBuf::from("/tmp/gpbeam-test-dest"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/gpbeam.toml"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/.gpbeam-ledger.sqlite"),
            Arc::new(MemoryKeyring::new()),
            None,
            None,
            None,
        );
        assert!(
            !ctx.wired_enabled.load(std::sync::atomic::Ordering::SeqCst),
            "wired_ingest=false seeds the live toggle off"
        );
        assert!(
            ctx.detector_paused
                .load(std::sync::atomic::Ordering::SeqCst),
            "disabled wired ingest starts the camera poller paused"
        );
        assert!(!ctx
            .wired_offload_active
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn fold_run_event_threads_through_appstate() {
        use crate::app_state::Status;
        use gpbeam_core::orchestrator::RunEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        // A Scanned event must flip status to Working and seed totals.
        let snap = fold_run_event(
            &state,
            &RunEvent::Scanned {
                new_files: 3,
                total_bytes: 9_000,
            },
            1_600_000_000,
        );
        assert_eq!(snap.status, Status::Working);
        let run = snap.run.expect("run present after Scanned");
        assert_eq!(run.files_total, 3);
        assert_eq!(run.bytes_total, 9_000);
        assert_eq!(run.started_at_unix, 1_600_000_000);
        // The shared state must have been mutated, not just the returned clone.
        let shared = state.lock().unwrap();
        assert_eq!(shared.status, Status::Working);
    }

    #[test]
    fn run_event_sequence_folds_to_terminal_idle() {
        use crate::app_state::Status;
        use gpbeam_core::orchestrator::RunEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let seq = [
            RunEvent::CardDetected {
                model: Some("HERO12".into()),
                serial: Some("C123".into()),
            },
            RunEvent::Scanned {
                new_files: 1,
                total_bytes: 100,
            },
            RunEvent::Copying {
                file: "a.mp4".into(),
                index: 1,
                total: 1,
            },
            RunEvent::Progress {
                file: "a.mp4".into(),
                copied: 100,
                total: 100,
            },
            RunEvent::Verified {
                file: "a.mp4".into(),
            },
            RunEvent::RunComplete {
                copied: 1,
                skipped: 0,
                failed: 0,
                bytes: 100,
            },
        ];
        let mut last = crate::app_state::AppState::default();
        for ev in &seq {
            last = fold_run_event(&state, ev, 1_700_000_000);
        }
        assert_eq!(last.status, Status::Idle);
        assert!(last.run.is_none());
        let lr = last.last_run.expect("last_run set on RunComplete");
        assert_eq!(lr.copied, 1);
        assert_eq!(lr.bytes, 100);
    }

    #[test]
    fn fold_cloud_event_threads_through_appstate() {
        use gpbeam_core::cloud::CloudEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let snap = fold_cloud_event(
            &state,
            &CloudEvent::Uploading {
                file: "a.mp4".into(),
                uploaded: 10,
                total: 100,
            },
        );
        let up = snap.cloud.uploading.expect("uploading present");
        assert_eq!(up.file, "a.mp4");
        assert_eq!(up.uploaded, 10);
        assert_eq!(up.total, 100);
        let shared = state.lock().unwrap();
        assert!(shared.cloud.uploading.is_some());
    }

    #[test]
    fn cloud_failed_folds_to_error_state() {
        use crate::app_state::Status;
        use gpbeam_core::cloud::CloudEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        // Prime a pending upload so the failure has something to decrement.
        let _ = fold_cloud_event(
            &state,
            &CloudEvent::Uploading {
                file: "x.mp4".into(),
                uploaded: 0,
                total: 10,
            },
        );
        let snap = fold_cloud_event(
            &state,
            &CloudEvent::CloudFailed {
                file: "x.mp4".into(),
                error: "boom".into(),
            },
        );
        assert_eq!(snap.status, Status::Error);
        assert!(snap.cloud.uploading.is_none());
        assert_eq!(snap.cloud.failed, 1);
        assert!(snap.message.is_some());
    }

    #[test]
    fn recover_and_count_pending_reclaims_orphaned_uploading() {
        // H1 startup recovery: any job left in Uploading by a prior crash must be
        // reset to Queued before the cold-window pending count is read, so it is
        // re-drained (not silently stalled) and the UI shows it as pending.
        use gpbeam_core::ledger::JobState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        {
            let mut l = Ledger::open(&path).unwrap();
            let imp = l.record("C", "f.MP4", 1, 1, "/d/f", None).unwrap();
            l.enqueue_cloud_job(imp, "nc1", "/d/f", "r", 1, None)
                .unwrap();
            l.claim_due_cloud_jobs(0, 10).unwrap(); // -> Uploading, then "crash"
            assert_eq!(
                l.list_cloud_jobs(Some(JobState::Uploading)).unwrap().len(),
                1
            );
        }

        let (pending, failed) = recover_and_count_pending(&path);
        assert_eq!(pending, 1, "the reclaimed job counts as pending");
        assert_eq!(failed, 0, "nothing terminally failed yet");

        let l = Ledger::open(&path).unwrap();
        assert_eq!(
            l.list_cloud_jobs(Some(JobState::Queued)).unwrap().len(),
            1,
            "orphaned Uploading was reset to Queued at startup"
        );
        assert_eq!(
            l.list_cloud_jobs(Some(JobState::Uploading)).unwrap().len(),
            0
        );
    }

    #[test]
    fn recover_and_count_pending_is_zero_for_missing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        // No ledger file written: a fresh install has nothing pending or failed.
        assert_eq!(
            recover_and_count_pending(&dir.path().join("absent.sqlite")),
            (0, 0)
        );
    }

    #[test]
    fn recover_and_count_pending_counts_terminal_failed_for_the_seed() {
        // Restart visibility (finding 2): a job the worker terminally gave up on
        // (state='failed', next_retry_at NULL) must be counted at startup, or the
        // popover shows 0 failed and the Retry button stays unreachable forever.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        {
            let mut l = Ledger::open(&path).unwrap();
            let mk = |l: &mut Ledger, name: &str, mt: i64| -> i64 {
                let imp = l.record("C", name, 1, mt, "/d", None).unwrap();
                l.enqueue_cloud_job(imp, "nc1", "/d", name, 1, None)
                    .unwrap()
            };
            let _queued = mk(&mut l, "q.MP4", 1);
            let retrying = mk(&mut l, "r.MP4", 2);
            let terminal = mk(&mut l, "t.MP4", 3);
            l.mark_job_failed(retrying, "transient", Some(9_999))
                .unwrap();
            l.mark_job_failed(terminal, "fatal", None).unwrap();
        }
        let (pending, failed) = recover_and_count_pending(&path);
        assert_eq!(pending, 2, "queued + retry-scheduled count as pending");
        assert_eq!(failed, 1, "only the terminal failure counts as failed");
    }

    #[test]
    fn seed_cloud_state_marks_configured_when_cloud_present() {
        let mut cfg = gpbeam_core::config::Config::new(std::path::PathBuf::from("/tmp/x"));
        cfg.cloud = Some(gpbeam_core::config::CloudConfig {
            kind: gpbeam_core::config::CloudKind::Nextcloud,
            destination_id: "nc1".into(),
            base_url: "https://example.com".into(),
            username: "alice".into(),
            remote_root: "/GPBeam".into(),
            mirror_mode: gpbeam_core::config::MirrorMode::Auto,
            chunk_threshold: 10_000_000,
            tls_ca_pem: None,
            max_concurrency: 2,
            max_attempts: 3,
        });
        let mut st = crate::app_state::AppState::default();
        seed_cloud_state(&mut st, &cfg, 7, 3);
        assert!(st.cloud.configured);
        assert_eq!(st.cloud.pending, 7);
        assert_eq!(st.cloud.failed, 3, "failed is seeded alongside pending");
    }

    #[test]
    fn seed_cloud_state_leaves_unconfigured_without_cloud() {
        let cfg = gpbeam_core::config::Config::new(std::path::PathBuf::from("/tmp/x"));
        let mut st = crate::app_state::AppState::default();
        seed_cloud_state(&mut st, &cfg, 0, 0);
        assert!(!st.cloud.configured);
        assert_eq!(st.cloud.pending, 0);
        assert_eq!(st.cloud.failed, 0);
    }

    #[test]
    fn build_app_ctx_without_cloud_leaves_runtime_idle() {
        let cfg = Config::new(std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        let backend = Arc::new(MemoryKeyring::new());
        let ctx = build_app_ctx(
            &cfg,
            std::path::PathBuf::from("/tmp/gpbeam-test-dest"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/gpbeam.toml"),
            std::path::PathBuf::from("/tmp/gpbeam-test-dest/.gpbeam-ledger.sqlite"),
            backend,
            None,
            None,
            None,
        );
        let rt = ctx.runtime.lock().unwrap();
        assert!(rt.config.is_none());
    }

    #[test]
    fn wired_ingest_enabled_follows_config_flag() {
        // Default config: wired_ingest defaults to true (Phase 5).
        let mut cfg = Config::new(std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        assert!(
            wired_ingest_enabled(&cfg),
            "default config enables wired ingest"
        );
        // Explicitly disabled -> CameraFound events are ignored (the poller still
        // runs; the live AppCtx.wired_enabled toggle gates each event).
        cfg.wired_ingest = false;
        assert!(
            !wired_ingest_enabled(&cfg),
            "wired_ingest=false seeds the event gate off"
        );
    }

    #[test]
    fn summary_notification_clean_run_with_copies_announces_counts() {
        use gpbeam_core::orchestrator::RunSummary;
        let s = RunSummary {
            copied: 3,
            skipped: 1,
            failed: 0,
            bytes: 4096,
            queued: 2,
        };
        let (title, body) = summary_notification(&s).expect("clean copy toast");
        assert_eq!(title, "GPBeam");
        assert_eq!(body, "Copied 3 file(s), 1 skipped");
    }

    #[test]
    fn summary_notification_nothing_copied_is_silent() {
        use gpbeam_core::orchestrator::RunSummary;
        // A no-op run (nothing new) must not raise a toast.
        let s = RunSummary {
            copied: 0,
            skipped: 5,
            failed: 0,
            bytes: 0,
            queued: 0,
        };
        assert!(summary_notification(&s).is_none());
    }

    #[test]
    fn summary_notification_failures_announce_failure_count() {
        use gpbeam_core::orchestrator::RunSummary;
        let s = RunSummary {
            copied: 2,
            skipped: 0,
            failed: 1,
            bytes: 20,
            queued: 0,
        };
        let (title, body) = summary_notification(&s).expect("failure toast");
        assert_eq!(title, "GPBeam");
        assert_eq!(body, "1 file(s) failed to copy");
    }

    #[test]
    fn derive_tray_state_error_status_wins() {
        use crate::app_state::{AppState, Status};
        let mut s = AppState {
            status: Status::Error,
            ..AppState::default()
        };
        assert_eq!(derive_tray_state(&s), "error");
        // Error wins even with work in flight.
        s.apply_run_event(
            &gpbeam_core::orchestrator::RunEvent::CardDetected {
                model: None,
                serial: None,
            },
            0,
        );
        s.status = Status::Error;
        assert_eq!(derive_tray_state(&s), "error");
    }

    #[test]
    fn derive_tray_state_working_while_run_or_upload_in_flight() {
        use crate::app_state::AppState;
        use gpbeam_core::cloud::CloudEvent;
        // Run in flight -> working.
        let mut s = AppState::default();
        s.apply_run_event(
            &gpbeam_core::orchestrator::RunEvent::Scanned {
                new_files: 1,
                total_bytes: 10,
            },
            0,
        );
        assert_eq!(derive_tray_state(&s), "working");

        // Cloud upload in flight (no run) -> working.
        let mut s = AppState::default();
        s.apply_cloud_event(&CloudEvent::Uploading {
            file: "a.mp4".into(),
            uploaded: 1,
            total: 10,
        });
        assert_eq!(derive_tray_state(&s), "working");
    }

    #[test]
    fn derive_tray_state_mirrored_mid_offload_stays_working() {
        // The original bug: a cloud Mirrored arriving mid-offload flipped the
        // tray to idle. Derived from the snapshot, the in-flight run keeps it
        // on "working".
        use crate::app_state::AppState;
        use gpbeam_core::cloud::CloudEvent;
        let mut s = AppState::default();
        s.apply_run_event(
            &gpbeam_core::orchestrator::RunEvent::Scanned {
                new_files: 2,
                total_bytes: 10,
            },
            0,
        );
        s.cloud.pending = 1;
        s.apply_cloud_event(&CloudEvent::Mirrored {
            file: "a.mp4".into(),
        });
        assert_eq!(derive_tray_state(&s), "working");
    }

    #[test]
    fn derive_tray_state_idle_when_nothing_in_flight() {
        let s = crate::app_state::AppState::default();
        assert_eq!(derive_tray_state(&s), "idle");
    }

    #[test]
    fn should_emit_drops_stale_and_duplicate_frames() {
        use std::sync::atomic::AtomicU64;
        let last = AtomicU64::new(0);
        assert!(should_emit(&last, 1), "first frame advances");
        assert!(!should_emit(&last, 1), "duplicate seq is dropped");
        assert!(should_emit(&last, 3), "newer frame advances (gaps ok)");
        assert!(
            !should_emit(&last, 2),
            "an older frame arriving late is dropped"
        );
        assert!(should_emit(&last, 4));
    }

    #[test]
    fn fold_helpers_bump_seq_under_the_lock() {
        use gpbeam_core::cloud::CloudEvent;
        use gpbeam_core::orchestrator::RunEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let a = fold_run_event(
            &state,
            &RunEvent::Scanned {
                new_files: 1,
                total_bytes: 1,
            },
            0,
        );
        assert_eq!(a.seq, 1, "fold_run_event bumps");
        let b = fold_cloud_event(&state, &CloudEvent::Mirrored { file: "x".into() });
        assert_eq!(b.seq, 2, "fold_cloud_event bumps");
        let c = fold_run_error_state(&state, "boom");
        assert_eq!(c.seq, 3, "fold_run_error_state bumps");
    }

    #[test]
    fn fold_run_error_state_clears_run_and_reports() {
        use crate::app_state::Status;
        use gpbeam_core::orchestrator::RunEvent;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let _ = fold_run_event(
            &state,
            &RunEvent::Scanned {
                new_files: 2,
                total_bytes: 10,
            },
            0,
        );
        let snap = fold_run_error_state(&state, "ledger open failed");
        assert!(snap.run.is_none(), "phantom in-flight run cleared");
        assert_eq!(snap.status, Status::Error);
        assert_eq!(snap.message.as_deref(), Some("ledger open failed"));
        assert_eq!(derive_tray_state(&snap), "error");
        // The shared state was mutated too, not just the returned clone.
        assert!(state.lock().unwrap().run.is_none());
    }

    #[test]
    fn rearm_budget_bounds_consecutive_retries_and_resets_on_replug() {
        let ip: std::net::IpAddr = "172.26.122.51".parse().unwrap();
        let mut b = RearmBudget::new();

        // First sight + two consecutive failures: both retries granted.
        b.note_event(ip);
        assert!(b.should_rearm(ip, true), "1st failure: retry");
        b.note_event(ip); // rearm-triggered event: budget NOT reset
        assert!(b.should_rearm(ip, true), "2nd failure: retry");
        b.note_event(ip);
        assert!(
            !b.should_rearm(ip, true),
            "3rd consecutive failure: budget exhausted, no auto-retry"
        );

        // The exhausted camera fires again only on a physical replug (no
        // preceding rearm) — fresh budget.
        b.note_event(ip);
        assert!(b.should_rearm(ip, true), "replug resets the budget");

        // A clean run resets the streak entirely.
        b.note_event(ip);
        assert!(!b.should_rearm(ip, false), "clean run never re-arms");
        b.note_event(ip);
        assert!(b.should_rearm(ip, true), "streak restarted after success");
    }

    #[test]
    fn rearm_needed_only_for_failed_runs() {
        use gpbeam_core::error::CoreError;
        use gpbeam_core::orchestrator::RunSummary;
        // Clean runs — including all-skipped no-ops — must NOT re-arm: re-firing
        // CameraFound for a successfully-offloaded camera would loop forever.
        let clean = Ok(RunSummary {
            copied: 1,
            skipped: 0,
            failed: 0,
            bytes: 1,
            queued: 0,
        });
        assert!(!rearm_needed(&clean));
        let noop = Ok(RunSummary {
            copied: 0,
            skipped: 3,
            failed: 0,
            bytes: 0,
            queued: 0,
        });
        assert!(!rearm_needed(&noop));
        // Per-file failures and hard run errors re-arm so the still-connected
        // camera is retried without an unplug/replug.
        let with_failures = Ok(RunSummary {
            copied: 0,
            skipped: 0,
            failed: 2,
            bytes: 0,
            queued: 0,
        });
        assert!(rearm_needed(&with_failures));
        let hard_err: Result<RunSummary, CoreError> = Err(CoreError::Config("boom".into()));
        assert!(rearm_needed(&hard_err));
    }

    #[test]
    fn run_wired_offload_for_camera_signature_is_pinned() {
        // Compile-time pin: existence + exact signature of the async wired driver.
        // The poller loop in setup() calls it as
        //   let retry = run_wired_offload_for_camera(&handle, &state, ip).await
        // and re-arms the detector when `retry` is true. The returned future borrows
        // `app`/`state`, so the boxed future's lifetime is tied to the inputs (`'a`);
        // it MUST be `Send` so the setup() spawn (which requires `Future + Send`)
        // compiles.
        fn pin_check<'a>(
            h: &'a tauri::AppHandle,
            s: &'a std::sync::Arc<std::sync::Mutex<crate::app_state::AppState>>,
            ip: std::net::IpAddr,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            Box::pin(run_wired_offload_for_camera(h, s, ip))
        }
        let _f = pin_check; // silence unused; this is a type-level assertion only.
        let _ = _f;
    }
}

/// IPC smoke tests over the REAL `#[tauri::command]` glue: a `MockRuntime` app
/// with a managed `AppCtx` invokes commands through `generate_handler!` exactly
/// like the webview does, so a command rename/arg mismatch (which the pure
/// `_impl` unit tests cannot see) fails a test instead of only failing at
/// runtime in the app.
#[cfg(test)]
mod ipc_smoke {
    use super::*;
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};

    fn test_app(dir: &Path) -> tauri::App<MockRuntime> {
        let cfg = Config::new(dir.to_path_buf());
        let ctx = build_app_ctx(
            &cfg,
            dir.to_path_buf(),
            dir.join("gpbeam.toml"),
            dir.join("ledger.sqlite"),
            Arc::new(crate::keyring_store::MemoryKeyring::new()),
            None,
            None,
            None,
        );
        mock_builder()
            .manage(ctx)
            // The REAL production handler list (see `invoke_handler`), not a
            // test-local subset: every registered command's IPC glue compiles
            // for this runtime, and renames desync-fail here.
            .invoke_handler(invoke_handler())
            .build(mock_context(noop_assets()))
            .expect("mock app builds")
    }

    fn invoke(
        webview: &tauri::WebviewWindow<MockRuntime>,
        cmd: &str,
    ) -> Result<tauri::ipc::InvokeResponseBody, serde_json::Value> {
        tauri::test::get_ipc_response(
            webview,
            tauri::webview::InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
    }

    #[test]
    fn ipc_get_config_path_and_get_state_resolve_through_the_real_handler() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();

        let path = invoke(&webview, "get_config_path")
            .expect("get_config_path resolves")
            .deserialize::<String>()
            .unwrap();
        assert_eq!(
            path,
            dir.path().join("gpbeam.toml").to_string_lossy(),
            "returns the resolved gpbeam.toml path from AppCtx"
        );

        let state = invoke(&webview, "get_state")
            .expect("get_state resolves")
            .deserialize::<serde_json::Value>()
            .unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["cloud"]["pending"], 0);
    }

    #[test]
    fn ipc_pause_cloud_bumps_seq_and_flips_paused() {
        // End-to-end coverage that a mutating command bumps the emit seq under
        // the lock (item: out-of-order emits) and reflects the flag.
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();

        let state = invoke(&webview, "pause_cloud")
            .expect("pause_cloud resolves")
            .deserialize::<serde_json::Value>()
            .unwrap();
        assert_eq!(state["cloud"]["paused"], true);
        assert_eq!(
            state["seq"], 1,
            "mutating command bumped seq before emitting"
        );
    }
}
