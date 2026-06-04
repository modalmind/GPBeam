//! GPBeam M1 tray shell. A window-less menu-bar / tray app that runs the
//! `gpbeam-core` offload engine in a background worker, swaps a tray icon to
//! reflect idle/working/error, and fires native notifications on completion.
//! The rich popover/settings UI (and folder picker, cloud, history) is M3.

use std::path::{Path, PathBuf};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

use gpbeam_core::cloud::{build_uploader, worker::CloudWorker, CloudEvent};
use gpbeam_core::config::{config_path, load_config, Config, MirrorMode};
use gpbeam_core::credentials::EnvConfigStore;
use gpbeam_core::ledger::Ledger;
use gpbeam_core::orchestrator::{run_offload, RunEvent as Ev};

mod app_state;

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

/// Map one `CloudEvent` to UI side effects. The full event is always forwarded
/// to the popover on `gpbeam://cloud`; terminal events also drive the tray icon
/// and a native notification, reusing the M1 `set_tray_state` / `notify` paths.
/// `CloudEvent` has exactly four variants (no `Ejected` — auto-eject is the sync
/// offload path's job and surfaces as `RunEvent::Ejected`), so this match stays
/// exhaustive over the locked contract.
fn forward_cloud_event(app: &AppHandle, ev: CloudEvent) {
    // Always surface the raw event to the popover UI.
    let _ = app.emit("gpbeam://cloud", format!("{ev:?}"));

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
/// a no-config install behaves identically to M1. The chosen destination root
/// always wins over whatever a stale config might claim, matching M1 behavior.
fn load_or_default_config(dest: &Path) -> Config {
    let path = config_path(std::env::var("GPBEAM_CONFIG").ok(), dest);
    match load_config(&path) {
        Ok(mut cfg) => {
            cfg.dest_root = dest.to_path_buf();
            cfg
        }
        Err(_) => Config::new(dest.to_path_buf()),
    }
}

/// Spawn the async cloud-upload worker loop on the Tauri runtime. Called only
/// when the loaded config has a `[cloud]` table whose `mirror_mode` is `Auto`.
/// The worker opens its OWN rusqlite `Ledger` at `ledger_path` (WAL + busy
/// timeout); it shares no `Connection` with the sync offload path, so it never
/// blocks card ejection or the UI thread. Credentials come from the same
/// `gpbeam.toml` the offload side loaded, honoring `GPBEAM_NC_*` env overrides.
fn spawn_cloud_worker(
    app: &AppHandle,
    cloud: gpbeam_core::config::CloudConfig,
    delete_after_verify: bool,
    ledger_path: PathBuf,
) {
    let cfg_path = config_path(std::env::var("GPBEAM_CONFIG").ok(), &dest_root());
    let store = match std::fs::read_to_string(&cfg_path).ok().and_then(|s| {
        EnvConfigStore::from_toml_str(
            &s,
            std::env::var("GPBEAM_NC_USERNAME").ok(),
            std::env::var("GPBEAM_NC_APP_PASSWORD").ok(),
        )
        .ok()
    }) {
        Some(s) => s,
        None => EnvConfigStore::empty(None, None),
    };

    let uploader = match build_uploader(&cloud, &store) {
        Ok(u) => u,
        Err(e) => {
            // Misconfigured cloud (e.g. missing app password) must NOT take down
            // the M1 offload path: report once and skip the worker.
            notify(app, "GPBeam cloud disabled", &e.to_string());
            return;
        }
    };

    let worker = CloudWorker::new(
        ledger_path,
        uploader,
        cloud.destination_id.clone(),
        cloud.max_concurrency,
        cloud.max_attempts,
        delete_after_verify,
    );

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            ticker.tick().await;
            let app2 = app.clone();
            let mut emit = move |ev: CloudEvent| forward_cloud_event(&app2, ev);
            // `run_until_drained` carries its own retry-aware sleep between
            // passes; the outer ticker re-checks for jobs enqueued by later
            // offload runs without busy-spinning.
            if let Err(e) = worker.run_until_drained(&mut emit).await {
                let _ = app.emit("gpbeam://cloud", format!("worker error: {e}"));
            }
        }
    });
}

/// Run one offload pass for a freshly mounted volume. Blocking I/O — call via
/// `spawn_blocking` so the async runtime is never stalled.
fn handle_mount(app: &AppHandle, mount: PathBuf) {
    // Ignore non-GoPro volumes (thumb drives, phones, etc.) before any side
    // effects: no tray flash, no destination dir, no ledger for a random disk.
    if !gpbeam_core::gopro::is_gopro_card(&mount) {
        return;
    }
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
    let summary = run_offload(&mount, &cfg, &mut ledger, &mut |e: Ev| {
        // Forward every event to the popover UI; tray state follows terminal events.
        let _ = app2.emit("gpbeam://event", format!("{e:?}"));
    });

    match summary {
        Ok(s) if s.failed == 0 => {
            set_tray_state(app, "idle");
            if s.copied > 0 {
                notify(
                    app,
                    "GPBeam",
                    &format!("Copied {} file(s), {} skipped", s.copied, s.skipped),
                );
            }
        }
        Ok(s) => {
            set_tray_state(app, "error");
            notify(app, "GPBeam", &format!("{} file(s) failed to copy", s.failed));
        }
        Err(e) => {
            set_tray_state(app, "error");
            notify(app, "GPBeam error", &e.to_string());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // single-instance MUST be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("settings") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
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
                            let _ = w.move_window(Position::TrayCenter);
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Background worker: poll for removable mounts and offload each.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                tauri::async_runtime::spawn(gpbeam_core::detect::poll_removable_mounts(tx));
                while let Some(mount) = rx.recv().await {
                    let h = handle.clone();
                    // run_offload is blocking I/O -> keep it off the async runtime.
                    let _ = tauri::async_runtime::spawn_blocking(move || handle_mount(&h, mount)).await;
                }
            });

            // M2: if cloud mirroring is configured for Auto, run the upload worker
            // alongside the offload worker. No [cloud] table (or a non-Auto mode)
            // -> nothing spawned and the process behaves byte-for-byte like M1.
            let dest = dest_root();
            let cfg = load_or_default_config(&dest);
            if let Some(cloud) = cfg.cloud {
                if cloud.mirror_mode == MirrorMode::Auto {
                    spawn_cloud_worker(
                        &app.handle().clone(),
                        cloud,
                        cfg.delete_after_verify,
                        ledger_path(&dest),
                    );
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building GPBeam")
        .run(|_app, event| {
            // Window-less app: closing a window hides it; only the tray "Quit" exits.
            if let RunEvent::WindowEvent {
                event: WindowEvent::CloseRequested { api, .. },
                label,
                ..
            } = event
            {
                if label == "popover" || label == "settings" {
                    api.prevent_close();
                }
            }
        });
}
