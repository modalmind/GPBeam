//! Pure, Tauri-free application-state snapshot for the M3 GUI.
//!
//! `AppState` is the single source of truth the popover/settings windows read on
//! open (via the `get_state` command) and live-patch thereafter. The reducers
//! [`AppState::apply_run_event`] and [`AppState::apply_cloud_event`] fold the
//! CORE event enums (`gpbeam_core::orchestrator::RunEvent` /
//! `gpbeam_core::cloud::CloudEvent`) directly into state — there is no separate
//! UI-event mirror to keep in sync. Everything here is pure: no Tauri, no I/O,
//! no clock reads (callers pass `now_unix`), so it is exhaustively unit-tested.

#[allow(unused_imports)]
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
        }
    }

    /// Seconds remaining, derived from observed throughput.
    ///
    /// Returns `None` when an estimate is meaningless:
    /// - no bytes copied yet (`bytes_done == 0`) — no rate to extrapolate from,
    /// - non-positive elapsed time (`now_unix <= started_at_unix`),
    /// - the run is byte-complete (`bytes_done >= bytes_total`).
    /// Otherwise `ceil((bytes_total - bytes_done) / (bytes_done / elapsed))`.
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
                self.status = Status::Working;
            }
            RunEvent::Progress { copied, .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.bytes_done = run.bytes_done.saturating_add(*copied);
                }
            }
            RunEvent::Verified { .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.files_done = (run.files_done + 1).min(run.files_total);
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
}
