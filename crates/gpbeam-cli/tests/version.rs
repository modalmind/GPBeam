//! `gpbeam-cli --version` reports the crate version (inherited from the Cargo
//! workspace). The flag is wired in `main.rs`; this locks the underlying lib fn.

#[test]
fn version_line_reports_crate_version() {
    let line = gpbeam_cli::version_line();
    assert!(line.starts_with("gpbeam-cli "), "got: {line}");
    assert_eq!(line, format!("gpbeam-cli {}", env!("CARGO_PKG_VERSION")));
}
