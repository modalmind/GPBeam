//! End-to-end exit-code contract for the `gpbeam-cli` binary:
//! 2 = usage error, 1 = runtime error / partial failure, 0 = fully-clean run.
//!
//! These spawn the real binary (CARGO_BIN_EXE) because the contract under test
//! IS the process exit status, which lib-level calls can't observe.

use std::path::Path;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gpbeam-cli"))
}

/// Build a minimal HERO11-shaped card: 100GOPRO/ with one small MP4 and the
/// version.txt that makes `is_gopro_card` return true.
fn make_card(root: &Path) {
    let dcim = root.join("DCIM").join("100GOPRO");
    std::fs::create_dir_all(&dcim).unwrap();
    std::fs::write(dcim.join("GX010001.MP4"), b"hello gopro media").unwrap();
    let misc = root.join("MISC");
    std::fs::create_dir_all(&misc).unwrap();
    std::fs::write(
        misc.join("version.txt"),
        r#"{"firmware version":"H22.01.01.10.00","camera serial number":"C1234567890123"}"#,
    )
    .unwrap();
}

#[test]
fn unknown_long_flag_is_usage_error_and_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&card).unwrap();

    // The historical bug: the typo'd flag became <card>, the real card became
    // <dest>, and a stray .gpbeam-ledger.sqlite was written ONTO the SD card.
    let out = bin()
        .args([
            "offload",
            "--delete-after-verfy", // typo, on purpose
            card.to_str().unwrap(),
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage error must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("usage:"),
        "stderr must show usage, got: {stderr}"
    );
    assert!(
        stderr.contains("--delete-after-verfy"),
        "stderr must name the offending flag, got: {stderr}"
    );
    assert!(
        !card.join(".gpbeam-ledger.sqlite").exists(),
        "a usage error must not write a ledger onto the card"
    );
    assert!(
        !dest.exists(),
        "a usage error must not create the destination"
    );
}

#[test]
fn bare_trailing_config_is_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&card).unwrap();

    let out = bin()
        .args([
            "offload",
            card.to_str().unwrap(),
            dest.to_str().unwrap(),
            "--config",
        ])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "bare --config must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("usage:"),
        "stderr must show usage, got: {stderr}"
    );
    assert!(
        !dest.exists(),
        "a usage error must not create the destination"
    );
}

#[test]
fn version_flags_exit_zero() {
    for flag in ["--version", "-V"] {
        let out = bin().arg(flag).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{flag} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with("gpbeam-cli "), "{flag} stdout: {stdout}");
    }
}

#[test]
fn valid_flags_and_clean_run_exit_zero() {
    // Recognized safety flags + a non-GoPro dir: prints [skip], nothing failed,
    // so the run is fully clean and must keep exiting 0.
    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&card).unwrap();

    let out = bin()
        .args([
            "--delete-after-verify",
            "--auto-eject",
            "offload",
            card.to_str().unwrap(),
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean run must exit 0; stderr: {stderr}"
    );
    assert!(
        stdout.contains("[skip] not a GoPro card"),
        "expected the skip line, got: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn offload_partial_failure_exits_one() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let card = tmp.path().join("card");
    let dest = tmp.path().join("dest");
    make_card(&card);

    // A second clip that cannot be read: the copy fails, tallied into
    // summary.failed (run_offload still returns Ok).
    let bad = card.join("DCIM").join("100GOPRO").join("GX010002.MP4");
    std::fs::write(&bad, b"unreadable clip").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = bin()
        .args(["offload", card.to_str().unwrap(), dest.to_str().unwrap()])
        .output()
        .unwrap();

    // Restore perms so tempdir cleanup never trips on the file.
    let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "partial failure must exit 1; stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        stderr.contains("1 file(s) failed"),
        "stderr must report the failed count, got: {stderr}"
    );
    assert!(
        stdout.contains("failed 1"),
        "summary line still printed, got: {stdout}"
    );
    assert!(
        stdout.contains("[ok] GX010001.MP4"),
        "the readable clip must still be copied, got: {stdout}"
    );
}
