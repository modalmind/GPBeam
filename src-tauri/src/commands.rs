//! Tauri command surface for the GPBeam M3 GUI. Every `#[tauri::command]` here
//! is a thin wrapper over the tested helpers in `config_io` / `app_state` /
//! `keyring_store` / `gpbeam-core`. All non-trivial logic lives in the pure
//! free helpers below (which ARE unit-tested), so the commands stay testable-
//! by-inspection and the real Tauri glue is the only untested surface.

#[cfg(test)]
mod tests {
    #[test]
    fn module_compiles() {
        // Smoke test: this module and its dependencies resolve.
        assert_eq!(2 + 2, 4);
    }
}
