//! Pure, Tauri-free application-state snapshot for the M3 GUI.
//!
//! `AppState` is the single source of truth the popover/settings windows read on
//! open (via the `get_state` command) and live-patch thereafter. The reducers
//! [`AppState::apply_run_event`] and [`AppState::apply_cloud_event`] fold the
//! CORE event enums (`gpbeam_core::orchestrator::RunEvent` /
//! `gpbeam_core::cloud::CloudEvent`) directly into state — there is no separate
//! UI-event mirror to keep in sync. Everything here is pure: no Tauri, no I/O,
//! no clock reads (callers pass `now_unix`), so it is exhaustively unit-tested.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired() {
        // Compiling + running this proves `mod app_state;` is declared in lib.rs
        // and the crate builds. Real behavior arrives in later tasks.
        assert_eq!(2 + 2, 4);
    }
}
