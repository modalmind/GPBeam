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
#[allow(unused_imports)]
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
}
