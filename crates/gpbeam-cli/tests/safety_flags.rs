use gpbeam_core::config::Config;
use std::path::PathBuf;

fn base_cfg() -> Config {
    Config::new(PathBuf::from("/tmp/dest"))
}

#[test]
fn flags_set_both_booleans_true() {
    let mut cfg = base_cfg();
    assert!(!cfg.delete_after_verify);
    assert!(!cfg.auto_eject);

    let flags = gpbeam_cli::SafetyFlags {
        delete_after_verify: true,
        auto_eject: true,
    };
    gpbeam_cli::apply_safety_overrides(&mut cfg, &flags);

    assert!(cfg.delete_after_verify);
    assert!(cfg.auto_eject);
}

#[test]
fn absent_flags_preserve_config_values() {
    let mut cfg = base_cfg();
    cfg.delete_after_verify = true; // came from gpbeam.toml
    cfg.auto_eject = true;

    let flags = gpbeam_cli::SafetyFlags {
        delete_after_verify: false,
        auto_eject: false,
    };
    gpbeam_cli::apply_safety_overrides(&mut cfg, &flags);

    // No flag passed => the config booleans are NOT cleared.
    assert!(cfg.delete_after_verify);
    assert!(cfg.auto_eject);
}

#[test]
fn parse_safety_flags_extracts_and_strips_argv() {
    let argv = vec![
        "offload".to_string(),
        "--delete-after-verify".to_string(),
        "/card".to_string(),
        "--auto-eject".to_string(),
        "/dest".to_string(),
    ];
    let (positional, flags) = gpbeam_cli::parse_safety_flags(&argv).expect("recognized flags");
    assert_eq!(positional, vec!["offload", "/card", "/dest"]);
    assert!(flags.delete_after_verify);
    assert!(flags.auto_eject);
}

#[test]
fn parse_safety_flags_rejects_unknown_long_flag() {
    // The typo that used to silently become a positional <card> argument.
    let argv = vec![
        "offload".to_string(),
        "--delete-after-verfy".to_string(),
        "/card".to_string(),
        "/dest".to_string(),
    ];
    let err = gpbeam_cli::parse_safety_flags(&argv).expect_err("typo'd flag must be rejected");
    assert!(
        err.contains("--delete-after-verfy"),
        "error names the flag: {err}"
    );
}

#[test]
fn parse_safety_flags_passes_version_flags_through() {
    for flag in ["--version", "-V"] {
        let argv = vec![flag.to_string()];
        let (positional, flags) =
            gpbeam_cli::parse_safety_flags(&argv).expect("version flag stays valid");
        assert_eq!(positional, vec![flag]);
        assert_eq!(flags, gpbeam_cli::SafetyFlags::default());
    }
}

#[test]
fn split_config_extracts_path() {
    let argv = vec![
        "offload".to_string(),
        "--config".to_string(),
        "/etc/gpbeam.toml".to_string(),
        "/card".to_string(),
    ];
    let (rest, cfg) = gpbeam_cli::split_config(&argv).expect("--config with value is valid");
    assert_eq!(rest, vec!["offload", "/card"]);
    assert_eq!(cfg, Some(PathBuf::from("/etc/gpbeam.toml")));
}

#[test]
fn split_config_rejects_value_less_config() {
    // A trailing `--config` used to be silently treated as a positional arg.
    let argv = vec![
        "offload".to_string(),
        "/card".to_string(),
        "/dest".to_string(),
        "--config".to_string(),
    ];
    let err = gpbeam_cli::split_config(&argv).expect_err("bare --config must be rejected");
    assert!(err.contains("--config"), "error names the flag: {err}");
}
