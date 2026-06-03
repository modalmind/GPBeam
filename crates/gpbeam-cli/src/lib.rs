//! Testable CLI run logic for `gpbeam-cli`. `main.rs` is a thin shim over this
//! library: it parses argv and prints the emitted lines, while the actual
//! offload + cloud-mirror orchestration lives here so integration tests can
//! drive it directly.

use gpbeam_core::cloud::nextcloud::NextcloudUploader;
use gpbeam_core::cloud::worker::CloudWorker;
use gpbeam_core::cloud::CloudEvent;
use gpbeam_core::config::{Config, MirrorMode};
use gpbeam_core::credentials::{CredentialStore, EnvConfigStore};
use gpbeam_core::error::{CoreError, Result};
use gpbeam_core::ledger::{CloudJob, JobState, Ledger};
use gpbeam_core::orchestrator::{run_offload, RunEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the SQLite ledger lives for a given destination root. The async cloud
/// worker opens its OWN connection to this same file (WAL + busy_timeout handle
/// concurrency), so we hand it the path, not a Connection.
pub fn ledger_path_for(dest: &Path) -> PathBuf {
    dest.join(".gpbeam-ledger.sqlite")
}

/// Format a sync-offload `RunEvent` as one human-readable line. `Progress` is
/// suppressed (returns `None`) to keep the stream readable.
pub fn format_run_event(e: &RunEvent) -> Option<String> {
    Some(match e {
        RunEvent::NotGoPro(p) => format!("[skip] not a GoPro card: {}", p.display()),
        RunEvent::CardDetected { model, serial } => format!(
            "[detect] {} (serial {})",
            model.clone().unwrap_or_else(|| "GoPro".into()),
            serial.clone().unwrap_or_else(|| "unknown".into())
        ),
        RunEvent::Scanned { new_files, total_bytes } => {
            format!("[scan] {new_files} new file(s), {total_bytes} bytes")
        }
        RunEvent::InsufficientSpace { need, have } => {
            format!("[error] not enough space: need {need}, have {have}")
        }
        RunEvent::Copying { file, index, total } => format!("[copy {index}/{total}] {file}"),
        RunEvent::Progress { .. } => return None,
        RunEvent::Verified { file } => format!("  [ok] {file}"),
        RunEvent::Skipped { file } => format!("  [skip] {file}"),
        RunEvent::Failed { file, error } => format!("  [FAIL] {file}: {error}"),
        RunEvent::CloudQueued { file } => format!("[cloud-queued] {file}"),
        RunEvent::CardFileDeleted { file } => format!("  [deleted] {file}"),
        RunEvent::Ejected { mount } => format!("[ejected] {mount}"),
        RunEvent::RunComplete { copied, skipped, failed, bytes } => {
            format!("[done] copied {copied}, skipped {skipped}, failed {failed}, {bytes} bytes")
        }
    })
}

/// Format an async cloud-worker `CloudEvent` as one human-readable line.
pub fn format_cloud_event(e: &CloudEvent) -> String {
    match e {
        CloudEvent::Uploading { file, uploaded, total } => {
            format!("[uploading] {file} {uploaded}/{total}")
        }
        CloudEvent::Mirrored { file } => format!("[mirrored] {file}"),
        CloudEvent::CloudFailed { file, error } => format!("[cloud-FAIL] {file}: {error}"),
        CloudEvent::Deleted { file } => format!("[deleted] {file}"),
    }
}

/// The M2 fields a CLI `gpbeam.toml` carries: `[cloud]` and the two safety
/// flags. The full M1 offload settings (filename template, layout, ...) are not
/// configured from the CLI in M2 — they keep their `Config::new(dest)` defaults,
/// rooted at the destination the user passed. Everything is `#[serde(default)]`
/// so a config that omits a section (or `dest_root`) still parses.
#[derive(serde::Deserialize)]
struct CliConfigOverlay {
    #[serde(default)]
    cloud: Option<gpbeam_core::config::CloudConfig>,
    #[serde(default)]
    delete_after_verify: bool,
    #[serde(default)]
    auto_eject: bool,
}

/// Read config from `gpbeam.toml` when `config_path` is set, otherwise the M1
/// defaults rooted at `dest`. The destination root ALWAYS comes from `dest` (the
/// path the user offloads to), never from the toml — the CLI config only supplies
/// the `[cloud]` table and the safety flags, so a `gpbeam.toml` that omits
/// `dest_root` is fine. Returns the raw toml text too (or empty) so the caller
/// can build a credential store over the SAME bytes.
pub fn load_or_default_config(dest: &Path, config_path: Option<&Path>) -> Result<(Config, String)> {
    match config_path {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|source| CoreError::Io {
                path: p.to_path_buf(),
                source,
            })?;
            let overlay: CliConfigOverlay =
                toml::from_str(&text).map_err(|e| CoreError::Config(e.to_string()))?;
            let mut cfg = Config::new(dest.to_path_buf());
            cfg.cloud = overlay.cloud;
            cfg.delete_after_verify = overlay.delete_after_verify;
            cfg.auto_eject = overlay.auto_eject;
            Ok((cfg, text))
        }
        None => Ok((Config::new(dest.to_path_buf()), String::new())),
    }
}

/// Run one synchronous offload pass and, when the config requests Auto cloud
/// mirroring, drain the cloud upload queue. `emit` receives one preformatted
/// line per event from both the sync and async phases.
pub async fn run_offload_and_mirror(
    card: &Path,
    dest: &Path,
    config_path: Option<&Path>,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|source| CoreError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let (cfg, toml_text) = load_or_default_config(dest, config_path)?;
    let lpath = ledger_path_for(dest);

    // --- Sync offload (blocking rusqlite + std::fs). Runs on a blocking thread
    //     so we never block the async runtime; its OWN Ledger connection is
    //     dropped before the async worker opens its own. ---
    {
        let cfg = cfg.clone();
        let card = card.to_path_buf();
        let lpath = lpath.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut ledger = Ledger::open(&lpath)?;
            run_offload(&card, &cfg, &mut ledger, &mut |e| {
                let _ = tx.send(e);
            })?;
            Ok(())
        });
        while let Some(ev) = rx.recv().await {
            if let Some(line) = format_run_event(&ev) {
                emit(line);
            }
        }
        join.await
            .map_err(|e| CoreError::Config(format!("offload task panicked: {e}")))??;
    }

    // --- Async cloud mirror, only for Auto mode with a configured cloud. ---
    let Some(cloud) = cfg.cloud.as_ref() else {
        return Ok(());
    };
    if cloud.mirror_mode != MirrorMode::Auto {
        return Ok(());
    }

    // Credentials come from the SAME toml bytes, with env overrides (C5).
    let store = EnvConfigStore::from_toml_str(
        &toml_text,
        std::env::var("GPBEAM_NC_USERNAME").ok(),
        std::env::var("GPBEAM_NC_APP_PASSWORD").ok(),
    )?;
    let secret = store.get(&cloud.destination_id)?.ok_or_else(|| {
        CoreError::Config(format!(
            "no credentials for destination '{}'",
            cloud.destination_id
        ))
    })?;
    let uploader = NextcloudUploader::new(cloud, &secret)?;
    let worker = CloudWorker::new(
        lpath,
        Arc::new(uploader),
        cloud.destination_id.clone(),
        cloud.max_concurrency,
        cloud.max_attempts,
        cfg.delete_after_verify,
    );
    worker
        .run_until_drained(&mut |ev: CloudEvent| emit(format_cloud_event(&ev)))
        .await?;
    Ok(())
}

/// Build the lines for `mirror-status`: every cloud job grouped by state plus a
/// trailing pending-count summary. Opens its own Ledger at the destination
/// (pure read; no uploads).
pub fn mirror_status_lines(dest: &Path) -> Result<Vec<String>> {
    let lpath = ledger_path_for(dest);
    let ledger = Ledger::open(&lpath)?;
    let mut lines = Vec::new();
    for state in [JobState::Uploading, JobState::Queued, JobState::Failed, JobState::Done] {
        let jobs: Vec<CloudJob> = ledger.list_cloud_jobs(Some(state))?;
        if jobs.is_empty() {
            continue;
        }
        lines.push(format!("== {} ({}) ==", state.as_str(), jobs.len()));
        for j in jobs {
            let err = j.last_error.as_deref().unwrap_or("");
            lines.push(format!(
                "  [{}] #{} attempts={} {} -> {} {}",
                j.state.as_str(),
                j.id,
                j.attempts,
                j.local_path,
                j.remote_path,
                if err.is_empty() { String::new() } else { format!("({err})") }
            ));
        }
    }
    let pending = ledger.pending_cloud_count()?;
    lines.push(format!("pending: {pending}"));
    Ok(lines)
}
