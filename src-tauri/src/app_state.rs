//! Pure, Tauri-free application-state snapshot for the M3 GUI.
//!
//! `AppState` is the single source of truth the popover/settings windows read on
//! open (via the `get_state` command) and live-patch thereafter. The reducers
//! [`AppState::apply_run_event`] and [`AppState::apply_cloud_event`] fold the
//! CORE event enums (`gpbeam_core::orchestrator::RunEvent` /
//! `gpbeam_core::cloud::CloudEvent`) directly into state — there is no separate
//! UI-event mirror to keep in sync. Everything here is pure: no Tauri, no I/O,
//! no clock reads (callers pass `now_unix`), so it is exhaustively unit-tested.

use gpbeam_core::cloud::CloudEvent;
use gpbeam_core::orchestrator::RunEvent;

/// Coarse app status the tray icon + UI follow.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Idle,
    Working,
    Error,
}

/// Live progress for the in-flight (or just-finished) offload run.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunProgress {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current_file: Option<String>,
    pub started_at_unix: i64,
    /// Bytes from files already fully verified this run. The in-flight file's
    /// `bytes_done` is `completed_bytes + <current file's cumulative bytes>`.
    /// Internal bookkeeping — not serialized to the UI.
    #[serde(skip)]
    pub completed_bytes: u64,
}

/// A single cloud upload in flight.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UploadProgress {
    pub file: String,
    pub uploaded: u64,
    pub total: u64,
}

/// Cloud-mirror state surfaced to the UI.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudState {
    pub configured: bool,
    pub pending: usize,
    pub failed: usize,
    pub paused: bool,
    pub uploading: Option<UploadProgress>,
}

/// Terminal summary of the most recent run (mirrors `RunEvent::RunComplete`).
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSummaryView {
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
}

/// The whole snapshot serialized to the UI on every reducer apply.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub status: Status,
    pub run: Option<RunProgress>,
    pub last_run: Option<RunSummaryView>,
    pub cloud: CloudState,
    pub message: Option<String>,
}

impl RunProgress {
    /// A fresh run stamped at `now_unix` with zeroed counters.
    fn started(now_unix: i64) -> Self {
        RunProgress {
            model: None,
            serial: None,
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            current_file: None,
            started_at_unix: now_unix,
            completed_bytes: 0,
        }
    }

    /// Seconds remaining, derived from observed throughput.
    ///
    /// Returns `None` when an estimate is meaningless:
    /// - no bytes copied yet (`bytes_done == 0`) — no rate to extrapolate from,
    /// - non-positive elapsed time (`now_unix <= started_at_unix`),
    /// - the run is byte-complete (`bytes_done >= bytes_total`).
    ///
    /// Otherwise `ceil((bytes_total - bytes_done) / (bytes_done / elapsed))`.
    ///
    /// Tested reference for the formula the popover mirrors client-side (the UI needs
    /// a live countdown that ticks between state emits); also available to any non-UI
    /// consumer (CLI status, notifications). Not yet called from production Rust.
    #[allow(dead_code)]
    pub fn eta_secs(&self, now_unix: i64) -> Option<u64> {
        let elapsed = now_unix - self.started_at_unix;
        if self.bytes_done == 0 || elapsed <= 0 || self.bytes_done >= self.bytes_total {
            return None;
        }
        let rate = self.bytes_done as f64 / elapsed as f64;
        let remaining = (self.bytes_total - self.bytes_done) as f64;
        Some((remaining / rate).ceil() as u64)
    }
}

impl AppState {
    /// Fold one core [`RunEvent`] into the snapshot. `now_unix` stamps run start
    /// (used later by [`RunProgress::eta_secs`]). Pure; no I/O.
    pub fn apply_run_event(&mut self, ev: &RunEvent, now_unix: i64) {
        match ev {
            RunEvent::Scanned { new_files, total_bytes } => {
                let mut run = self.run.take().unwrap_or_else(|| RunProgress::started(now_unix));
                run.files_total = *new_files;
                run.bytes_total = *total_bytes;
                run.files_done = 0;
                run.bytes_done = 0;
                run.completed_bytes = 0;
                run.current_file = None;
                run.started_at_unix = now_unix;
                self.run = Some(run);
                self.status = Status::Working;
                self.message = None;
            }
            RunEvent::CardDetected { model, serial } => {
                let run = self
                    .run
                    .get_or_insert_with(|| RunProgress::started(now_unix));
                run.model = model.clone();
                run.serial = serial.clone();
            }
            RunEvent::Copying { file, index, .. } => {
                let run = self
                    .run
                    .get_or_insert_with(|| RunProgress::started(now_unix));
                run.current_file = Some(file.clone());
                // `index` is 1-based; files fully done before this one is index-1.
                run.files_done = index.saturating_sub(1);
                run.bytes_done = run.completed_bytes;
                self.status = Status::Working;
            }
            RunEvent::Progress { copied, .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.bytes_done = run.completed_bytes.saturating_add(*copied);
                }
            }
            RunEvent::Verified { .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.files_done = (run.files_done + 1).min(run.files_total);
                    run.completed_bytes = run.bytes_done;
                }
            }
            RunEvent::Failed { file, error } => {
                self.status = Status::Error;
                self.message = Some(format!("{file}: {error}"));
            }
            RunEvent::InsufficientSpace { need, have } => {
                self.status = Status::Error;
                self.message = Some(format!(
                    "insufficient space: need {need} bytes, have {have}"
                ));
            }
            RunEvent::RunComplete { copied, skipped, failed, bytes } => {
                self.last_run = Some(RunSummaryView {
                    copied: *copied,
                    skipped: *skipped,
                    failed: *failed,
                    bytes: *bytes,
                });
                self.run = None;
                self.status = if *failed > 0 { Status::Error } else { Status::Idle };
            }
            RunEvent::CloudQueued { .. } => {
                self.cloud.pending += 1;
                self.cloud.configured = true;
            }
            // Informational / no snapshot change: surfaced via tray/notifications
            // (lib.rs), not part of the UI state machine.
            RunEvent::NotGoPro(_)
            | RunEvent::Skipped { .. }
            | RunEvent::CardFileDeleted { .. }
            | RunEvent::Ejected { .. } => {}
        }
    }

    /// Fold one core [`CloudEvent`] into the snapshot. Pure; no I/O.
    pub fn apply_cloud_event(&mut self, ev: &CloudEvent) {
        match ev {
            CloudEvent::Uploading { file, uploaded, total } => {
                self.cloud.uploading = Some(UploadProgress {
                    file: file.clone(),
                    uploaded: *uploaded,
                    total: *total,
                });
            }
            CloudEvent::Mirrored { .. } => {
                self.cloud.uploading = None;
                self.cloud.pending = self.cloud.pending.saturating_sub(1);
            }
            CloudEvent::CloudFailed { file, error } => {
                self.cloud.uploading = None;
                self.cloud.failed += 1;
                self.cloud.pending = self.cloud.pending.saturating_sub(1);
                self.status = Status::Error;
                self.message = Some(format!("{file}: {error}"));
            }
            CloudEvent::Deleted { file } => {
                self.message = Some(format!("freed card space: {file}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_app_state_is_idle_and_empty() {
        let s = AppState::default();
        assert_eq!(s.status, Status::Idle);
        assert!(s.run.is_none());
        assert!(s.last_run.is_none());
        assert!(s.message.is_none());
        assert_eq!(s.cloud, CloudState::default());
        assert_eq!(s.cloud.pending, 0);
        assert_eq!(s.cloud.failed, 0);
        assert!(!s.cloud.configured);
        assert!(!s.cloud.paused);
        assert!(s.cloud.uploading.is_none());
    }

    #[test]
    fn structs_construct_and_compare() {
        let run = RunProgress {
            model: Some("HERO11".into()),
            serial: Some("C346".into()),
            files_done: 1,
            files_total: 4,
            bytes_done: 100,
            bytes_total: 1000,
            current_file: Some("GX010001.MP4".into()),
            started_at_unix: 1_000,
            completed_bytes: 0,
        };
        let up = UploadProgress { file: "clip.mp4".into(), uploaded: 5, total: 10 };
        let summary = RunSummaryView { copied: 3, skipped: 1, failed: 0, bytes: 4096 };
        let cloud = CloudState {
            configured: true,
            pending: 2,
            failed: 0,
            paused: false,
            uploading: Some(up.clone()),
        };
        let state = AppState {
            status: Status::Working,
            run: Some(run.clone()),
            last_run: Some(summary.clone()),
            cloud: cloud.clone(),
            message: Some("copying".into()),
        };
        // PartialEq + Clone round-trip.
        assert_eq!(state.clone(), state);
        assert_eq!(state.run.unwrap(), run);
        assert_eq!(state.last_run.unwrap(), summary);
        assert_eq!(state.cloud.uploading.unwrap(), up);
    }

    #[test]
    fn full_happy_path_scan_to_complete() {
        let now = 1_000i64;
        let mut s = AppState::default();

        // Scanned: 2 files, 1000 bytes total. status -> Working, run created.
        s.apply_run_event(&RunEvent::Scanned { new_files: 2, total_bytes: 1000 }, now);
        assert_eq!(s.status, Status::Working);
        let run = s.run.as_ref().expect("run created on Scanned");
        assert_eq!(run.files_total, 2);
        assert_eq!(run.bytes_total, 1000);
        assert_eq!(run.files_done, 0);
        assert_eq!(run.bytes_done, 0);
        assert_eq!(run.started_at_unix, now);
        assert_eq!(run.current_file, None);

        // CardDetected: model/serial set on the existing run.
        s.apply_run_event(
            &RunEvent::CardDetected {
                model: Some("HERO11".into()),
                serial: Some("C346".into()),
            },
            now,
        );
        let run = s.run.as_ref().unwrap();
        assert_eq!(run.model.as_deref(), Some("HERO11"));
        assert_eq!(run.serial.as_deref(), Some("C346"));

        // File 1: Copying{index:1} -> current_file set, files_done = 0.
        s.apply_run_event(
            &RunEvent::Copying { file: "A.MP4".into(), index: 1, total: 2 },
            now,
        );
        let run = s.run.as_ref().unwrap();
        assert_eq!(run.current_file.as_deref(), Some("A.MP4"));
        assert_eq!(run.files_done, 0);

        // Progress: bytes_done += copied.
        s.apply_run_event(
            &RunEvent::Progress { file: "A.MP4".into(), copied: 400, total: 400 },
            now,
        );
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 400);

        // Verified: files_done -> 1.
        s.apply_run_event(&RunEvent::Verified { file: "A.MP4".into() }, now);
        assert_eq!(s.run.as_ref().unwrap().files_done, 1);

        // File 2.
        s.apply_run_event(
            &RunEvent::Copying { file: "B.MP4".into(), index: 2, total: 2 },
            now,
        );
        let run = s.run.as_ref().unwrap();
        assert_eq!(run.current_file.as_deref(), Some("B.MP4"));
        assert_eq!(run.files_done, 1); // index - 1

        s.apply_run_event(
            &RunEvent::Progress { file: "B.MP4".into(), copied: 600, total: 600 },
            now,
        );
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 1000);

        s.apply_run_event(&RunEvent::Verified { file: "B.MP4".into() }, now);
        assert_eq!(s.run.as_ref().unwrap().files_done, 2);

        // RunComplete: run -> last_run, status Idle (no failures).
        s.apply_run_event(
            &RunEvent::RunComplete { copied: 2, skipped: 0, failed: 0, bytes: 1000 },
            now,
        );
        assert!(s.run.is_none(), "run cleared on completion");
        assert_eq!(s.status, Status::Idle);
        let last = s.last_run.as_ref().expect("last_run set");
        assert_eq!(*last, RunSummaryView { copied: 2, skipped: 0, failed: 0, bytes: 1000 });
    }

    #[test]
    fn card_detected_before_scan_creates_run() {
        // CardDetected may arrive first (orchestrator emits it before Scanned).
        let mut s = AppState::default();
        s.apply_run_event(
            &RunEvent::CardDetected { model: Some("HERO12".into()), serial: None },
            42,
        );
        let run = s.run.as_ref().expect("run created by CardDetected when absent");
        assert_eq!(run.model.as_deref(), Some("HERO12"));
        assert_eq!(run.serial, None);
        assert_eq!(run.files_total, 0);
        assert_eq!(run.started_at_unix, 42);
    }

    #[test]
    fn verified_caps_files_done_at_total() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 10 }, 0);
        s.apply_run_event(&RunEvent::Verified { file: "x".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 1);
        // A spurious extra Verified must not exceed files_total.
        s.apply_run_event(&RunEvent::Verified { file: "x".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 1);
    }

    #[test]
    fn failed_event_sets_error_and_message() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 10 }, 0);
        assert_eq!(s.status, Status::Working);
        s.apply_run_event(
            &RunEvent::Failed { file: "A.MP4".into(), error: "disk read error".into() },
            0,
        );
        assert_eq!(s.status, Status::Error);
        assert_eq!(s.message.as_deref(), Some("A.MP4: disk read error"));
    }

    #[test]
    fn insufficient_space_sets_error_and_message() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::InsufficientSpace { need: 500, have: 100 }, 0);
        assert_eq!(s.status, Status::Error);
        assert_eq!(
            s.message.as_deref(),
            Some("insufficient space: need 500 bytes, have 100")
        );
    }

    #[test]
    fn run_complete_with_failures_is_error_status() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 3, total_bytes: 30 }, 0);
        s.apply_run_event(
            &RunEvent::RunComplete { copied: 2, skipped: 0, failed: 1, bytes: 20 },
            0,
        );
        assert_eq!(s.status, Status::Error);
        assert!(s.run.is_none());
        assert_eq!(
            s.last_run.as_ref().unwrap(),
            &RunSummaryView { copied: 2, skipped: 0, failed: 1, bytes: 20 }
        );
    }

    #[test]
    fn scanned_clears_a_stale_error_message() {
        // A new run starting should clear a leftover error from a prior run.
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Failed { file: "x".into(), error: "boom".into() }, 0);
        assert_eq!(s.status, Status::Error);
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 5 }, 10);
        assert_eq!(s.status, Status::Working);
        assert!(s.message.is_none(), "Scanned resets the message for the new run");
    }

    #[test]
    fn cloud_queued_increments_pending_and_marks_configured() {
        let mut s = AppState::default();
        assert!(!s.cloud.configured);
        assert_eq!(s.cloud.pending, 0);

        s.apply_run_event(&RunEvent::CloudQueued { file: "A.MP4".into() }, 0);
        assert!(s.cloud.configured);
        assert_eq!(s.cloud.pending, 1);

        s.apply_run_event(&RunEvent::CloudQueued { file: "B.MP4".into() }, 0);
        assert_eq!(s.cloud.pending, 2);
        assert!(s.cloud.configured);
    }

    #[test]
    fn multi_file_files_done_bookkeeping_from_copying_index() {
        // files_done tracks the 1-based Copying index minus one, then Verified
        // bumps it; the two paths must stay consistent across several files.
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 3, total_bytes: 300 }, 0);

        s.apply_run_event(&RunEvent::Copying { file: "1".into(), index: 1, total: 3 }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 0);
        s.apply_run_event(&RunEvent::Verified { file: "1".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 1);

        s.apply_run_event(&RunEvent::Copying { file: "2".into(), index: 2, total: 3 }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 1); // index-1 keeps it at 1
        s.apply_run_event(&RunEvent::Verified { file: "2".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 2);

        s.apply_run_event(&RunEvent::Copying { file: "3".into(), index: 3, total: 3 }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 2);
        s.apply_run_event(&RunEvent::Verified { file: "3".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().files_done, 3);
        assert_eq!(s.run.as_ref().unwrap().current_file.as_deref(), Some("3"));
    }

    #[test]
    fn multiple_progress_per_file_accumulate_via_base() {
        // Live progress sends several cumulative values for ONE file; bytes_done must
        // track the cumulative value, NOT sum the deltas (the old additive bug).
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 1000 }, 0);
        s.apply_run_event(&RunEvent::Copying { file: "a".into(), index: 1, total: 1 }, 0);

        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 200, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 200);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 500, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 500, "cumulative, not 200+500");
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 1000, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 1000);

        s.apply_run_event(&RunEvent::Verified { file: "a".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().completed_bytes, 1000, "verified file rolled into base");
    }

    #[test]
    fn terminal_and_post_completion_ticks_are_idempotent() {
        // Both the throttle's 100% tick and the loop's explicit post-completion
        // Progress carry copied == file size. Under the cumulative reducer the second
        // one is an idempotent assignment, NOT a second add — bytes_done stays == size.
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 500 }, 0);
        s.apply_run_event(&RunEvent::Copying { file: "a".into(), index: 1, total: 1 }, 0);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 500, total: 500 }, 0); // throttle terminal tick
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 500, total: 500 }, 0); // post-completion emit
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 500, "idempotent, not 1000");
        s.apply_run_event(&RunEvent::Verified { file: "a".into() }, 0);
        assert_eq!(s.run.as_ref().unwrap().completed_bytes, 500);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 500);
    }

    #[test]
    fn second_file_progress_adds_to_completed_base() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 2, total_bytes: 1000 }, 0);
        s.apply_run_event(&RunEvent::Copying { file: "a".into(), index: 1, total: 2 }, 0);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 400, total: 400 }, 0);
        s.apply_run_event(&RunEvent::Verified { file: "a".into() }, 0);

        // File 2 starts: bytes_done resets to the completed base (400), not 0.
        s.apply_run_event(&RunEvent::Copying { file: "b".into(), index: 2, total: 2 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 400, "in-flight file starts from the base");
        s.apply_run_event(&RunEvent::Progress { file: "b".into(), copied: 250, total: 600 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 650, "base 400 + current 250");
    }

    #[test]
    fn failed_file_does_not_advance_base() {
        // A file that streams partway then fails emits no Verified; its partial bytes
        // must not leak into the base, so the next file starts from the real base.
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 2, total_bytes: 1000 }, 0);
        s.apply_run_event(&RunEvent::Copying { file: "a".into(), index: 1, total: 2 }, 0);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 300, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 300);
        // No Verified for "a" (it failed). Next file's Copying drops the partial.
        s.apply_run_event(&RunEvent::Copying { file: "b".into(), index: 2, total: 2 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 0, "failed partial dropped");
        assert_eq!(s.run.as_ref().unwrap().completed_bytes, 0);
    }

    #[test]
    fn resume_first_progress_starts_mid_file() {
        // Cross-run resume: the first cumulative value for a file is large (the .part
        // prefix already on disk). bytes_done jumps to it, then continues.
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 1, total_bytes: 1000 }, 0);
        s.apply_run_event(&RunEvent::Copying { file: "a".into(), index: 1, total: 1 }, 0);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 800, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 800);
        s.apply_run_event(&RunEvent::Progress { file: "a".into(), copied: 1000, total: 1000 }, 0);
        assert_eq!(s.run.as_ref().unwrap().bytes_done, 1000);
    }

    #[test]
    fn completed_bytes_is_not_serialized() {
        let run = RunProgress { completed_bytes: 123, ..RunProgress::started(0) };
        let v = serde_json::to_value(&run).unwrap();
        assert!(v.get("completedBytes").is_none(), "internal base must not leak to UI");
        assert!(v.get("completed_bytes").is_none());
    }

    #[test]
    fn skipped_and_other_noop_events_do_not_change_run() {
        let mut s = AppState::default();
        s.apply_run_event(&RunEvent::Scanned { new_files: 2, total_bytes: 20 }, 0);
        let before = s.run.clone();
        s.apply_run_event(&RunEvent::Skipped { file: "dup.MP4".into() }, 0);
        s.apply_run_event(&RunEvent::CardFileDeleted { file: "dup.MP4".into() }, 0);
        s.apply_run_event(&RunEvent::Ejected { mount: "/Volumes/GoPro".into() }, 0);
        s.apply_run_event(
            &RunEvent::NotGoPro(std::path::PathBuf::from("/Volumes/USB")),
            0,
        );
        assert_eq!(s.run, before, "no-op events leave run untouched");
        assert_eq!(s.status, Status::Working);
    }

    #[test]
    fn cloud_uploading_sets_current_upload() {
        let mut s = AppState::default();
        s.apply_cloud_event(&CloudEvent::Uploading {
            file: "clip.mp4".into(),
            uploaded: 30,
            total: 100,
        });
        assert_eq!(
            s.cloud.uploading,
            Some(UploadProgress { file: "clip.mp4".into(), uploaded: 30, total: 100 })
        );
    }

    #[test]
    fn cloud_mirrored_clears_upload_and_decrements_pending() {
        let mut s = AppState::default();
        s.cloud.pending = 2;
        s.apply_cloud_event(&CloudEvent::Uploading {
            file: "clip.mp4".into(),
            uploaded: 100,
            total: 100,
        });
        assert!(s.cloud.uploading.is_some());
        s.apply_cloud_event(&CloudEvent::Mirrored { file: "clip.mp4".into() });
        assert!(s.cloud.uploading.is_none());
        assert_eq!(s.cloud.pending, 1);
    }

    #[test]
    fn cloud_mirrored_pending_saturates_at_zero() {
        let mut s = AppState::default();
        // pending already 0 -> Mirrored must not underflow.
        s.apply_cloud_event(&CloudEvent::Mirrored { file: "clip.mp4".into() });
        assert_eq!(s.cloud.pending, 0);
    }

    #[test]
    fn cloud_failed_sets_error_increments_failed_and_decrements_pending() {
        let mut s = AppState::default();
        s.cloud.pending = 1;
        s.apply_cloud_event(&CloudEvent::Uploading {
            file: "clip.mp4".into(),
            uploaded: 10,
            total: 100,
        });
        s.apply_cloud_event(&CloudEvent::CloudFailed {
            file: "clip.mp4".into(),
            error: "401 Unauthorized".into(),
        });
        assert!(s.cloud.uploading.is_none());
        assert_eq!(s.cloud.failed, 1);
        assert_eq!(s.cloud.pending, 0);
        assert_eq!(s.status, Status::Error);
        assert_eq!(s.message.as_deref(), Some("clip.mp4: 401 Unauthorized"));
    }

    #[test]
    fn cloud_failed_pending_saturates_at_zero() {
        let mut s = AppState::default();
        s.apply_cloud_event(&CloudEvent::CloudFailed {
            file: "clip.mp4".into(),
            error: "boom".into(),
        });
        assert_eq!(s.cloud.pending, 0);
        assert_eq!(s.cloud.failed, 1);
    }

    #[test]
    fn cloud_deleted_sets_message_only() {
        let mut s = AppState::default();
        s.cloud.pending = 3;
        s.apply_cloud_event(&CloudEvent::Deleted { file: "clip.mp4".into() });
        // No counter movement; status unchanged; an informational message is fine.
        assert_eq!(s.cloud.pending, 3);
        assert_eq!(s.cloud.failed, 0);
        assert_eq!(s.status, Status::Idle);
        assert_eq!(s.message.as_deref(), Some("freed card space: clip.mp4"));
    }

    fn rp(bytes_done: u64, bytes_total: u64, started_at_unix: i64) -> RunProgress {
        RunProgress {
            model: None,
            serial: None,
            files_done: 0,
            files_total: 0,
            bytes_done,
            bytes_total,
            current_file: None,
            started_at_unix,
            completed_bytes: 0,
        }
    }

    #[test]
    fn eta_none_when_no_bytes_copied() {
        // bytes_done == 0 -> no rate to extrapolate.
        assert_eq!(rp(0, 1000, 0).eta_secs(10), None);
    }

    #[test]
    fn eta_none_when_elapsed_not_positive() {
        // now <= started_at -> elapsed <= 0.
        assert_eq!(rp(500, 1000, 100).eta_secs(100), None); // elapsed == 0
        assert_eq!(rp(500, 1000, 100).eta_secs(50), None); // elapsed < 0
    }

    #[test]
    fn eta_none_when_byte_complete() {
        // bytes_done >= bytes_total -> nothing left.
        assert_eq!(rp(1000, 1000, 0).eta_secs(10), None);
        assert_eq!(rp(1200, 1000, 0).eta_secs(10), None);
    }

    #[test]
    fn eta_concrete_normal_case() {
        // 400 of 1000 bytes in 10s -> rate 40 B/s; remaining 600 -> 15s.
        assert_eq!(rp(400, 1000, 0).eta_secs(10), Some(15));
    }

    #[test]
    fn eta_ceils_fractional_seconds() {
        // 300 of 1000 in 10s -> rate 30 B/s; remaining 700 / 30 = 23.33 -> ceil 24.
        assert_eq!(rp(300, 1000, 0).eta_secs(10), Some(24));
    }

    #[test]
    fn app_state_json_shape_is_camel_and_lowercase() {
        let state = AppState {
            status: Status::Working,
            run: Some(RunProgress {
                model: Some("HERO11".into()),
                serial: Some("C346".into()),
                files_done: 1,
                files_total: 4,
                bytes_done: 100,
                bytes_total: 1000,
                current_file: Some("GX010001.MP4".into()),
                started_at_unix: 1_700_000_000,
                completed_bytes: 0,
            }),
            last_run: Some(RunSummaryView { copied: 3, skipped: 1, failed: 0, bytes: 4096 }),
            cloud: CloudState {
                configured: true,
                pending: 2,
                failed: 0,
                paused: false,
                uploading: Some(UploadProgress {
                    file: "clip.mp4".into(),
                    uploaded: 5,
                    total: 10,
                }),
            },
            message: Some("copying".into()),
        };

        let v = serde_json::to_value(&state).expect("AppState serializes");

        // Status is a lowercase string token.
        assert_eq!(v["status"], serde_json::json!("working"));

        // Top-level + nested fields are camelCase.
        let run = &v["run"];
        assert_eq!(run["filesDone"], serde_json::json!(1));
        assert_eq!(run["filesTotal"], serde_json::json!(4));
        assert_eq!(run["bytesDone"], serde_json::json!(100));
        assert_eq!(run["bytesTotal"], serde_json::json!(1000));
        assert_eq!(run["currentFile"], serde_json::json!("GX010001.MP4"));
        assert_eq!(run["startedAtUnix"], serde_json::json!(1_700_000_000i64));
        assert!(run.get("files_done").is_none(), "no snake_case keys leak");

        let last = &v["lastRun"];
        assert_eq!(last["copied"], serde_json::json!(3));
        assert_eq!(last["skipped"], serde_json::json!(1));

        let cloud = &v["cloud"];
        assert_eq!(cloud["configured"], serde_json::json!(true));
        assert_eq!(cloud["pending"], serde_json::json!(2));
        assert_eq!(cloud["uploading"]["total"], serde_json::json!(10));

        assert_eq!(v["message"], serde_json::json!("copying"));
        assert!(v.get("last_run").is_none(), "top-level key is camelCase lastRun");
    }

    #[test]
    fn default_app_state_json_shape() {
        // Defaults: status "idle", run/lastRun/message null, cloud all-false/zero.
        let v = serde_json::to_value(AppState::default()).unwrap();
        assert_eq!(v["status"], serde_json::json!("idle"));
        assert_eq!(v["run"], serde_json::Value::Null);
        assert_eq!(v["lastRun"], serde_json::Value::Null);
        assert_eq!(v["message"], serde_json::Value::Null);
        assert_eq!(v["cloud"]["pending"], serde_json::json!(0));
        assert_eq!(v["cloud"]["configured"], serde_json::json!(false));
        assert_eq!(v["cloud"]["uploading"], serde_json::Value::Null);
    }
}
