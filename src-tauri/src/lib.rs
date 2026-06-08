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

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use gpbeam_core::cloud::{build_uploader, worker::CloudWorker, CloudEvent};
use gpbeam_core::config::{config_path, load_config, Config};
use gpbeam_core::credentials::EnvConfigStore;
use gpbeam_core::ledger::Ledger;
use gpbeam_core::orchestrator::{run_offload, RunEvent as Ev};
use gpbeam_core::error::CoreError;
use gpbeam_core::orchestrator::RunSummary;

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

/// Swap the tray icon for the current state ("idle" | "working" | "error").
fn set_tray_state(app: &AppHandle, state: &str) {
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

/// The terminal tray-icon state for a finished run: `"idle"` on a clean run,
/// `"error"` on any per-file failure or a hard run error. Pure; shared by both
/// offload paths.
fn tray_state_for_summary(summary: &Result<RunSummary, CoreError>) -> &'static str {
    match summary {
        Ok(s) if s.failed == 0 => "idle",
        _ => "error",
    }
}

/// Map one `CloudEvent` to tray-icon + native-notification side effects. The full
/// UI state now flows on `"gpbeam://state"` (see `spawn_cloud_loop`), so this no
/// longer emits a per-event debug string. `CloudEvent` has exactly four variants,
/// so this match stays exhaustive over the locked contract.
fn forward_cloud_event(app: &AppHandle, ev: CloudEvent) {
    match ev {
        CloudEvent::Uploading { .. } => {
            // In-flight upload: reflect activity in the tray, no notification.
            set_tray_state(app, "working");
        }
        CloudEvent::Mirrored { file } => {
            set_tray_state(app, "idle");
            notify(app, "GPBeam", &format!("Mirrored {file} to cloud"));
        }
        CloudEvent::CloudFailed { file, error } => {
            set_tray_state(app, "error");
            notify(app, "GPBeam cloud error", &format!("{file}: {error}"));
        }
        CloudEvent::Deleted { file } => {
            notify(app, "GPBeam", &format!("Freed card space: {file}"));
        }
    }
}

/// Lock the shared `AppState`, fold one `RunEvent` into it at `now_unix`, and
/// return a clone of the resulting snapshot for emission. Pure (no `AppHandle`) so
/// the offload->state mapping is unit-testable; the caller emits the returned
/// snapshot on `"gpbeam://state"`.
fn fold_run_event(state: &Arc<Mutex<AppState>>, ev: &Ev, now_unix: i64) -> AppState {
    let mut st = lock_recover(state);
    st.apply_run_event(ev, now_unix);
    st.clone()
}

/// Lock the shared `AppState`, fold one `CloudEvent` into it, and return a clone of
/// the resulting snapshot for emission. Pure; the caller emits it.
fn fold_cloud_event(state: &Arc<Mutex<AppState>>, ev: &CloudEvent) -> AppState {
    let mut st = lock_recover(state);
    st.apply_cloud_event(ev);
    st.clone()
}

/// Emit a full `AppState` snapshot to every window on `"gpbeam://state"`. The UI
/// replaces its store wholesale on each event (no TS-side reducer). Serialization
/// failures are swallowed: a dropped frame is recoverable (the next event re-emits
/// the full state), and there is nothing useful to do on a transport error here.
fn emit_state(app: &AppHandle, snapshot: &AppState) {
    let _ = app.emit("gpbeam://state", snapshot);
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// M1 destination: $GPBEAM_DEST, else ~/GPBeam. (Configurable folder picker is M3.)
fn dest_root() -> PathBuf {
    match std::env::var("GPBEAM_DEST") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home_dir().join("GPBeam"),
    }
}

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
    AppCtx {
        state: Arc::new(Mutex::new(AppState::default())),
        paused: Arc::new(AtomicBool::new(false)),
        creds: Arc::new(creds),
        runtime: Arc::new(Mutex::new(CloudRuntime::from_config(cfg))),
        offload_lock: Arc::new(tokio::sync::Mutex::new(())),
        dest_root,
        config_path,
        ledger_path,
    }
}

/// Startup crash recovery + pending count for the cold-window seed. Opens the
/// ledger, resets any job orphaned in `Uploading` by a prior crash back to
/// `Queued` (H1), then returns the pending count. A missing/unopenable ledger
/// (fresh install) yields 0. Kept separate from `seed_cloud_state` so the
/// recovery is unit-testable without a Tauri runtime.
fn recover_and_count_pending(ledger_path: &Path) -> usize {
    let mut ledger = match Ledger::open(ledger_path) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    let _ = ledger.reclaim_orphaned_uploading();
    ledger.pending_cloud_count().unwrap_or(0)
}

/// Seed a fresh `AppState.cloud` from the loaded config + the ledger's pending
/// count at startup, so a window opened before any event still shows the right
/// configured/pending counts. Pure: the ledger count is read by the caller and
/// passed in. `configured` is true iff a `[cloud]` table exists.
fn seed_cloud_state(state: &mut AppState, cfg: &Config, pending: usize) {
    state.cloud.configured = cfg.cloud.is_some();
    state.cloud.pending = pending;
}

/// Whether the USB-wired GoPro detector should be spawned. Gated by the
/// `wired_ingest` config flag (default true): with it off, the poller is never
/// started and the process behaves byte-for-byte like M1–M3. Pure so `setup()`'s
/// (untestable) spawn branch reduces to a single named, tested decision.
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
                Ok(u) => u,
                Err(e) => {
                    // Misconfigured cloud (e.g. missing app password) must NOT take
                    // down the offload path: surface once and retry next tick.
                    notify(&app, "GPBeam cloud disabled", &e.to_string());
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
                // Fold into shared state + broadcast, then run tray/notification FX.
                let snap = fold_cloud_event(&state, &ev);
                emit_state(&app2, &snap);
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
        notify(app, "GPBeam error", &format!("cannot create destination: {e}"));
        set_tray_state(app, "error");
        return;
    }
    let cfg = load_or_default_config(&dest);
    let mut ledger = match Ledger::open(&ledger_path(&dest)) {
        Ok(l) => l,
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
            set_tray_state(app, "error");
            return;
        }
    };

    set_tray_state(app, "working");
    let app2 = app.clone();
    let state2 = state.clone();
    let summary = run_offload(&mount, &cfg, &mut ledger, &mut |e: Ev| {
        // Fold every event into the shared AppState and broadcast the snapshot.
        let snap = fold_run_event(&state2, &e, cloud_runtime::now_unix());
        emit_state(&app2, &snap);
    });

    set_tray_state(app, tray_state_for_summary(&summary));
    match summary {
        Ok(s) => {
            if let Some((title, body)) = summary_notification(&s) {
                notify(app, title, &body);
            }
        }
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
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
async fn run_wired_offload_for_camera(
    app: &AppHandle,
    state: &Arc<Mutex<AppState>>,
    ip: std::net::IpAddr,
) {
    // M6: serialize against the SD path (handle_mount) sharing this dest_root/
    // ledger. This future is async, so an awaited acquire keeps it Send; the
    // guard is held across the whole wired offload.
    let ctx = app.state::<AppCtx>();
    let _offload_guard = ctx.offload_lock.lock().await;
    let dest = dest_root();
    if let Err(e) = std::fs::create_dir_all(&dest) {
        notify(app, "GPBeam error", &format!("cannot create destination: {e}"));
        set_tray_state(app, "error");
        return;
    }
    let cfg = load_or_default_config(&dest);
    let mut ledger = match Ledger::open(&ledger_path(&dest)) {
        Ok(l) => l,
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
            set_tray_state(app, "error");
            return;
        }
    };

    set_tray_state(app, "working");
    let client = gpbeam_core::wired::client::GoProClient::new(ip);
    let app2 = app.clone();
    let state2 = state.clone();
    let summary = gpbeam_core::wired::offload::run_wired_offload(
        &client,
        &cfg,
        &mut ledger,
        &mut |e: Ev| {
            // Fold every event into the shared AppState and broadcast the snapshot
            // (identical to handle_mount's emitter).
            let snap = fold_run_event(&state2, &e, cloud_runtime::now_unix());
            emit_state(&app2, &snap);
        },
    )
    .await;

    set_tray_state(app, tray_state_for_summary(&summary));
    match summary {
        Ok(s) => {
            if let Some((title, body)) = summary_notification(&s) {
                notify(app, title, &body);
            }
        }
        Err(e) => {
            notify(app, "GPBeam error", &e.to_string());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Resolve paths + load config ONCE, before the builder, so we can seed the
    // managed AppCtx (and its AppState/CloudRuntime) up front.
    let dest = dest_root();
    let cfg = load_or_default_config(&dest);
    // Captured for the setup() closure's wired-ingest spawn gate (Task 6.4). A
    // copied bool avoids moving `cfg` (still needed below to build the AppCtx).
    let wired_ingest = wired_ingest_enabled(&cfg);
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

    // Seed AppState.cloud from config + ledger pending count for a cold window.
    // recover_and_count_pending also reclaims any job orphaned in Uploading by a
    // prior crash (H1) before the count is read.
    {
        let pending = recover_and_count_pending(&led_path);
        let mut st = lock_recover(&ctx.state);
        seed_cloud_state(&mut st, &cfg, pending);
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
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_config,
            commands::save_config,
            commands::pick_folder,
            commands::open_path,
            commands::reveal_path,
            commands::open_settings,
            commands::set_nextcloud_credentials,
            commands::clear_nextcloud_credentials,
            commands::pause_cloud,
            commands::resume_cloud,
            commands::retry_failed_cloud,
            commands::get_history,
            commands::get_autostart,
            commands::set_autostart,
            commands::is_first_run,
            commands::complete_wizard,
            commands::quit,
        ])
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
                    let _ = tauri::async_runtime::spawn_blocking(move || {
                        handle_mount(&h, &st, mount)
                    })
                    .await;
                }
            });

            // M4: USB-wired GoPro detector. Gated by `wired_ingest` (default true);
            // when off, this is never spawned and behavior is byte-for-byte M1–M3.
            // On each CameraFound we run the async wired offload ON the tokio runtime
            // (NOT spawn_blocking — it is async HTTP I/O), folding every RunEvent into
            // the shared AppState + emitting "gpbeam://state" exactly like the SD path.
            // Cloud jobs it enqueues are drained by the M2 cloud loop below, unchanged.
            if wired_ingest {
                let handle = app.handle().clone();
                let state = app.state::<AppCtx>().state.clone();
                tauri::async_runtime::spawn(async move {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                    // The Open GoPro HTTP server handles ONE client at a time, so the poller
                    // must NOT probe the camera while an offload is downloading from it. We
                    // pause the poller for the duration of each offload (exclusive access).
                    let paused = Arc::new(AtomicBool::new(false));
                    tauri::async_runtime::spawn(gpbeam_core::wired::detect::poll_for_camera(
                        tx,
                        paused.clone(),
                    ));
                    while let Some(found) = rx.recv().await {
                        // One camera at a time (design §5): each detection runs to completion
                        // before the next is handled. Pause probing so the offload owns the camera.
                        paused.store(true, std::sync::atomic::Ordering::Relaxed);
                        run_wired_offload_for_camera(&handle, &state, found.ip).await;
                        paused.store(false, std::sync::atomic::Ordering::Relaxed);
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
        assert_eq!(ctx.dest_root, std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        assert!(!ctx.paused.load(std::sync::atomic::Ordering::SeqCst));
        // Fresh AppState defaults to Idle with no run.
        let st = ctx.state.lock().unwrap();
        assert_eq!(st.status, crate::app_state::Status::Idle);
        assert!(st.run.is_none());
    }

    #[test]
    fn fold_run_event_threads_through_appstate() {
        use crate::app_state::Status;
        use gpbeam_core::orchestrator::RunEvent;
        let state = std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        // A Scanned event must flip status to Working and seed totals.
        let snap = fold_run_event(
            &state,
            &RunEvent::Scanned { new_files: 3, total_bytes: 9_000 },
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
        let state = std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let seq = [
            RunEvent::CardDetected { model: Some("HERO12".into()), serial: Some("C123".into()) },
            RunEvent::Scanned { new_files: 1, total_bytes: 100 },
            RunEvent::Copying { file: "a.mp4".into(), index: 1, total: 1 },
            RunEvent::Progress { file: "a.mp4".into(), copied: 100, total: 100 },
            RunEvent::Verified { file: "a.mp4".into() },
            RunEvent::RunComplete { copied: 1, skipped: 0, failed: 0, bytes: 100 },
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
        let state = std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        let snap = fold_cloud_event(
            &state,
            &CloudEvent::Uploading { file: "a.mp4".into(), uploaded: 10, total: 100 },
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
        let state = std::sync::Arc::new(std::sync::Mutex::new(crate::app_state::AppState::default()));
        // Prime a pending upload so the failure has something to decrement.
        let _ = fold_cloud_event(
            &state,
            &CloudEvent::Uploading { file: "x.mp4".into(), uploaded: 0, total: 10 },
        );
        let snap = fold_cloud_event(
            &state,
            &CloudEvent::CloudFailed { file: "x.mp4".into(), error: "boom".into() },
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
            l.enqueue_cloud_job(imp, "nc1", "/d/f", "r", 1, None).unwrap();
            l.claim_due_cloud_jobs(0, 10).unwrap(); // -> Uploading, then "crash"
            assert_eq!(l.list_cloud_jobs(Some(JobState::Uploading)).unwrap().len(), 1);
        }

        let pending = recover_and_count_pending(&path);
        assert_eq!(pending, 1, "the reclaimed job counts as pending");

        let l = Ledger::open(&path).unwrap();
        assert_eq!(
            l.list_cloud_jobs(Some(JobState::Queued)).unwrap().len(),
            1,
            "orphaned Uploading was reset to Queued at startup"
        );
        assert_eq!(l.list_cloud_jobs(Some(JobState::Uploading)).unwrap().len(), 0);
    }

    #[test]
    fn recover_and_count_pending_is_zero_for_missing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        // No ledger file written: a fresh install has nothing pending.
        assert_eq!(recover_and_count_pending(&dir.path().join("absent.sqlite")), 0);
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
        seed_cloud_state(&mut st, &cfg, 7);
        assert!(st.cloud.configured);
        assert_eq!(st.cloud.pending, 7);
    }

    #[test]
    fn seed_cloud_state_leaves_unconfigured_without_cloud() {
        let cfg = gpbeam_core::config::Config::new(std::path::PathBuf::from("/tmp/x"));
        let mut st = crate::app_state::AppState::default();
        seed_cloud_state(&mut st, &cfg, 0);
        assert!(!st.cloud.configured);
        assert_eq!(st.cloud.pending, 0);
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
        // Default config: wired_ingest defaults to true (Phase 5), so the poller spawns.
        let mut cfg = Config::new(std::path::PathBuf::from("/tmp/gpbeam-test-dest"));
        assert!(
            wired_ingest_enabled(&cfg),
            "default config enables wired ingest"
        );
        // Explicitly disabled -> the poller is NOT spawned (byte-for-byte M1–M3).
        cfg.wired_ingest = false;
        assert!(
            !wired_ingest_enabled(&cfg),
            "wired_ingest=false suppresses the camera poller"
        );
    }

    #[test]
    fn summary_notification_clean_run_with_copies_announces_counts() {
        use gpbeam_core::orchestrator::RunSummary;
        let s = RunSummary { copied: 3, skipped: 1, failed: 0, bytes: 4096, queued: 2 };
        let (title, body) = summary_notification(&s).expect("clean copy toast");
        assert_eq!(title, "GPBeam");
        assert_eq!(body, "Copied 3 file(s), 1 skipped");
    }

    #[test]
    fn summary_notification_nothing_copied_is_silent() {
        use gpbeam_core::orchestrator::RunSummary;
        // A no-op run (nothing new) must not raise a toast.
        let s = RunSummary { copied: 0, skipped: 5, failed: 0, bytes: 0, queued: 0 };
        assert!(summary_notification(&s).is_none());
    }

    #[test]
    fn summary_notification_failures_announce_failure_count() {
        use gpbeam_core::orchestrator::RunSummary;
        let s = RunSummary { copied: 2, skipped: 0, failed: 1, bytes: 20, queued: 0 };
        let (title, body) = summary_notification(&s).expect("failure toast");
        assert_eq!(title, "GPBeam");
        assert_eq!(body, "1 file(s) failed to copy");
    }

    #[test]
    fn tray_state_for_summary_maps_results_to_icon() {
        use gpbeam_core::error::CoreError;
        use gpbeam_core::orchestrator::RunSummary;
        let clean = Ok(RunSummary { copied: 1, skipped: 0, failed: 0, bytes: 1, queued: 0 });
        assert_eq!(tray_state_for_summary(&clean), "idle");
        let with_failures = Ok(RunSummary { copied: 0, skipped: 0, failed: 2, bytes: 0, queued: 0 });
        assert_eq!(tray_state_for_summary(&with_failures), "error");
        let hard_err: Result<RunSummary, CoreError> = Err(CoreError::Config("boom".into()));
        assert_eq!(tray_state_for_summary(&hard_err), "error");
    }

    #[test]
    fn run_wired_offload_for_camera_signature_is_pinned() {
        // Compile-time pin: existence + exact signature of the async wired driver.
        // The poller loop in setup() calls it as
        //   run_wired_offload_for_camera(&handle, &state, ip).await
        // The returned future borrows `app`/`state`, so the boxed future's lifetime
        // is tied to the inputs (`'a`); it MUST be `Send` so the setup() spawn (which
        // requires `Future + Send`) compiles.
        fn pin_check<'a>(
            h: &'a tauri::AppHandle,
            s: &'a std::sync::Arc<std::sync::Mutex<crate::app_state::AppState>>,
            ip: std::net::IpAddr,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(run_wired_offload_for_camera(h, s, ip))
        }
        let _f = pin_check; // silence unused; this is a type-level assertion only.
        let _ = _f;
    }
}
