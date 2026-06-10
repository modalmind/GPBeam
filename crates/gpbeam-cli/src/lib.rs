//! Testable CLI run logic for `gpbeam-cli`. `main.rs` is a thin shim over this
//! library: it parses argv and prints the emitted lines, while the actual
//! offload + cloud-mirror orchestration lives here so integration tests can
//! drive it directly.

use gpbeam_core::cloud::worker::CloudWorker;
use gpbeam_core::cloud::{build_uploader, CloudEvent};
use gpbeam_core::config::{Config, MirrorMode};
use gpbeam_core::credentials::EnvConfigStore;
use gpbeam_core::error::{CoreError, Result};
use gpbeam_core::ledger::{CloudJob, JobState, Ledger};
use gpbeam_core::orchestrator::{run_offload, RunEvent, RunSummary};
use std::path::{Path, PathBuf};

/// Where the SQLite ledger lives for a given destination root. The async cloud
/// worker opens its OWN connection to this same file (WAL + busy_timeout handle
/// concurrency), so we hand it the path, not a Connection.
pub fn ledger_path_for(dest: &Path) -> PathBuf {
    dest.join(".gpbeam-ledger.sqlite")
}

/// The `--version` line for the CLI: `gpbeam-cli <semver>`, where the version is
/// the crate's `CARGO_PKG_VERSION` (inherited from the Cargo workspace version).
pub fn version_line() -> String {
    format!("gpbeam-cli {}", env!("CARGO_PKG_VERSION"))
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
        RunEvent::Scanned {
            new_files,
            total_bytes,
        } => {
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
        // Non-fatal: the file itself copied + verified fine; only the card-side
        // delete-after-verify cleanup failed.
        RunEvent::CardDeleteFailed { file, error } => {
            format!("  [warn] {file}: card delete-after-verify failed: {error}")
        }
        RunEvent::Ejected { mount } => format!("[ejected] {mount}"),
        RunEvent::RunComplete {
            copied,
            skipped,
            failed,
            bytes,
        } => {
            format!("[done] copied {copied}, skipped {skipped}, failed {failed}, {bytes} bytes")
        }
    })
}

/// Format an async cloud-worker `CloudEvent` as one human-readable line.
pub fn format_cloud_event(e: &CloudEvent) -> String {
    match e {
        CloudEvent::Uploading {
            file,
            uploaded,
            total,
        } => {
            format!("[uploading] {file} {uploaded}/{total}")
        }
        CloudEvent::Mirrored { file } => format!("[mirrored] {file}"),
        CloudEvent::CloudFailed { file, error } => format!("[cloud-FAIL] {file}: {error}"),
        CloudEvent::Deleted { file } => format!("[deleted] {file}"),
        // Non-fatal: the upload succeeded; only the card-side cleanup failed.
        // Not counted toward the exit code (see `drain_counting_failures`).
        CloudEvent::DeleteFailed { file, error } => {
            format!("[warn] could not delete {file} from card: {error}")
        }
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

/// Build the async `CloudWorker` for a loaded config, or `Ok(None)` when no
/// `[cloud]` section is configured. Shared by `run_offload_and_mirror` (Auto
/// drain after offload) and `run_mirror` (on-demand drain regardless of mode),
/// so the uploader + worker wiring lives in exactly one place.
///
/// Credentials come from the SAME toml bytes the config was parsed from, with
/// `GPBEAM_NC_*` env overrides (Contract C5). The uploader is built via
/// `gpbeam_core::cloud::build_uploader` (which does the secret lookup), and the
/// worker carries `cfg.delete_after_verify` (Contract C4).
fn build_cloud_worker(
    cfg: &Config,
    toml_text: &str,
    ledger_path: PathBuf,
) -> Result<Option<CloudWorker>> {
    let Some(cloud) = cfg.cloud.as_ref() else {
        return Ok(None);
    };
    let store = EnvConfigStore::from_toml_str(
        toml_text,
        std::env::var("GPBEAM_NC_USERNAME").ok(),
        std::env::var("GPBEAM_NC_APP_PASSWORD").ok(),
    )?;
    let uploader = build_uploader(cloud, &store)?;
    Ok(Some(CloudWorker::new(
        ledger_path,
        uploader,
        cloud.destination_id.clone(),
        cloud.max_concurrency,
        cloud.max_attempts,
        cfg.delete_after_verify,
    )))
}

/// Run one synchronous offload pass and, when the config requests Auto cloud
/// mirroring, drain the cloud upload queue. `emit` receives one preformatted
/// line per event from both the sync and async phases.
///
/// Returns the number of files that terminally failed (sync copy failures from
/// `RunSummary.failed` plus cloud jobs the worker gave up on, i.e. `CloudFailed`
/// events). `Ok(0)` strictly means a fully-clean run — `main.rs` exits non-zero
/// on `Ok(n > 0)` so scripts/cron can detect partial failure.
pub async fn run_offload_and_mirror(
    card: &Path,
    dest: &Path,
    config_path: Option<&Path>,
    flags: &SafetyFlags,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<usize> {
    std::fs::create_dir_all(dest).map_err(|source| CoreError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let (mut cfg, toml_text) = load_or_default_config(dest, config_path)?;
    apply_safety_overrides(&mut cfg, flags);
    let lpath = ledger_path_for(dest);

    // --- Sync offload (blocking rusqlite + std::fs). Runs on a blocking thread
    //     so we never block the async runtime; its OWN Ledger connection is
    //     dropped before the async worker opens its own. Per-file copy failures
    //     do NOT error the run — they are tallied in the returned summary. ---
    let summary: RunSummary = {
        let cfg = cfg.clone();
        let card = card.to_path_buf();
        let lpath = lpath.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
        let join = tokio::task::spawn_blocking(move || -> Result<RunSummary> {
            let mut ledger = Ledger::open(&lpath)?;
            run_offload(&card, &cfg, &mut ledger, &mut |e| {
                let _ = tx.send(e);
            })
        });
        while let Some(ev) = rx.recv().await {
            if let Some(line) = format_run_event(&ev) {
                emit(line);
            }
        }
        join.await
            .map_err(|e| CoreError::Config(format!("offload task panicked: {e}")))??
    };

    // --- Async cloud mirror, only for Auto mode with a configured cloud.
    //     Manual-queued jobs are flushed on demand by `run_mirror`. ---
    let Some(cloud) = cfg.cloud.as_ref() else {
        return Ok(summary.failed);
    };
    if cloud.mirror_mode != MirrorMode::Auto {
        return Ok(summary.failed);
    }
    let Some(worker) = build_cloud_worker(&cfg, &toml_text, lpath)? else {
        return Ok(summary.failed);
    };
    let cloud_failed = drain_counting_failures(&worker, emit).await?;
    Ok(summary.failed + cloud_failed)
}

/// Drain the cloud queue, forwarding formatted lines to `emit`, and return how
/// many `CloudFailed` events fired. The worker only emits `CloudFailed` for
/// TERMINAL upload problems (retries exhausted / non-retryable) — retryable
/// failures are rescheduled silently and a post-upload card-delete failure is
/// the separate, non-fatal `DeleteFailed` — so the count is exactly the number
/// of permanently-failed uploads this drain produced. (If another GPBeam
/// process holds the worker lock the drain is skipped with a stderr notice and
/// the count is 0 — nothing was attempted.)
async fn drain_counting_failures(
    worker: &CloudWorker,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<usize> {
    let mut cloud_failed = 0usize;
    worker
        .run_until_drained(&mut |ev: CloudEvent| {
            if matches!(ev, CloudEvent::CloudFailed { .. }) {
                cloud_failed += 1;
            }
            emit(format_cloud_event(&ev))
        })
        .await?;
    Ok(cloud_failed)
}

/// Flush the cloud upload queue ON DEMAND: build the worker from the CLI cloud
/// config + safety flags (the SAME path `run_offload_and_mirror` uses) and
/// `run_until_drained` to drain ALL pending jobs, irrespective of the
/// `mirror_mode` (Auto or Manual). This is what makes `MirrorMode::Manual`
/// usable — jobs the orchestrator enqueued (but never auto-drained) get
/// uploaded here. `emit` receives one preformatted line per `CloudEvent`.
///
/// Returns the number of jobs that TERMINALLY failed during the drain
/// (`CloudFailed` events); `Ok(0)` strictly means everything pending uploaded
/// cleanly, so `main.rs` can exit non-zero on partial failure. Returns
/// `CoreError::Config` when the config has no `[cloud]` section, since there is
/// nothing to flush to.
pub async fn run_mirror(
    dest: &Path,
    config_path: Option<&Path>,
    flags: &SafetyFlags,
    emit: &mut (dyn FnMut(String) + Send),
) -> Result<usize> {
    let (mut cfg, toml_text) = load_or_default_config(dest, config_path)?;
    apply_safety_overrides(&mut cfg, flags);
    let lpath = ledger_path_for(dest);

    let Some(worker) = build_cloud_worker(&cfg, &toml_text, lpath)? else {
        return Err(CoreError::Config(
            "no [cloud] destination configured; nothing to mirror".into(),
        ));
    };
    drain_counting_failures(&worker, emit).await
}

/// Build the lines for `mirror-status`: every cloud job grouped by state plus a
/// trailing pending-count summary. Opens its own Ledger at the destination
/// (pure read; no uploads).
pub fn mirror_status_lines(dest: &Path) -> Result<Vec<String>> {
    let lpath = ledger_path_for(dest);
    let ledger = Ledger::open(&lpath)?;
    let mut lines = Vec::new();
    for state in [
        JobState::Uploading,
        JobState::Queued,
        JobState::Failed,
        JobState::Done,
    ] {
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
                if err.is_empty() {
                    String::new()
                } else {
                    format!("({err})")
                }
            ));
        }
    }
    let pending = ledger.pending_cloud_count()?;
    lines.push(format!("pending: {pending}"));
    Ok(lines)
}

/// Re-queue every terminally-`Failed` cloud job (one the worker gave up on:
/// `next_retry_at IS NULL`) back to `Queued` with `attempts = 0`, so the next
/// worker pass uploads it again. Opens its own Ledger at `dest` (pure ledger
/// edit; no uploads). Returns how many jobs were re-queued.
pub fn retry_cloud(dest: &Path) -> Result<usize> {
    let lpath = ledger_path_for(dest);
    let mut ledger = Ledger::open(&lpath)?;
    ledger.requeue_failed_cloud_jobs()
}

/// CLI overrides for the two M2 safety booleans. `false` means "flag not passed"
/// — absence never clears a `true` coming from gpbeam.toml.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafetyFlags {
    pub delete_after_verify: bool,
    pub auto_eject: bool,
}

/// Apply CLI flags onto a loaded `Config`: a present flag forces the field true;
/// an absent flag leaves whatever the config file already set.
pub fn apply_safety_overrides(cfg: &mut Config, flags: &SafetyFlags) {
    if flags.delete_after_verify {
        cfg.delete_after_verify = true;
    }
    if flags.auto_eject {
        cfg.auto_eject = true;
    }
}

/// Pull `--delete-after-verify` and `--auto-eject` out of an argv slice,
/// returning the remaining positional args and the parsed flags.
///
/// Any OTHER `--token` is a usage error (`Err` with a message naming it) — a
/// typo like `--delete-after-verfy` must never silently become a positional
/// `<card>` argument (which used to route the offload at the wrong paths and
/// write a stray ledger onto the SD card). `--version` is the one long flag
/// passed through, since `main.rs` dispatches on it as a pseudo-subcommand
/// (`-V` has a single dash and passes through untouched).
pub fn parse_safety_flags(
    args: &[String],
) -> std::result::Result<(Vec<String>, SafetyFlags), String> {
    let mut positional = Vec::new();
    let mut flags = SafetyFlags::default();
    for a in args {
        match a.as_str() {
            "--delete-after-verify" => flags.delete_after_verify = true,
            "--auto-eject" => flags.auto_eject = true,
            other if other.starts_with("--") && other != "--version" => {
                return Err(format!("unrecognized flag '{other}'"));
            }
            other => positional.push(other.to_string()),
        }
    }
    Ok((positional, flags))
}

/// Pull `--config <path>` out of an argv slice, returning the remaining args
/// and the config path. A `--config` with no following value is a usage error
/// (`Err`) — it used to be silently treated as a positional argument.
pub fn split_config(
    args: &[String],
) -> std::result::Result<(Vec<String>, Option<PathBuf>), String> {
    let mut rest = Vec::new();
    let mut config = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" {
            let Some(p) = args.get(i + 1) else {
                return Err("'--config' requires a <path> value".into());
            };
            config = Some(PathBuf::from(p));
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    Ok((rest, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_delete_failed_formats_as_warn_not_fail() {
        // The file was copied + verified + recorded; only the card cleanup
        // failed — the CLI line must read as a warning, never as a FAIL.
        let line = format_run_event(&RunEvent::CardDeleteFailed {
            file: "GX010001.MP4".into(),
            error: "permission denied".into(),
        })
        .expect("CardDeleteFailed is printed, not suppressed");
        assert!(line.contains("[warn]"), "warning prefix: {line:?}");
        assert!(line.contains("GX010001.MP4"));
        assert!(line.contains("permission denied"));
        assert!(
            !line.contains("FAIL"),
            "must not look like a failed file: {line:?}"
        );
    }
}
