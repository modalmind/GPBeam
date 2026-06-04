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

    let flags = gpbeam_cli::SafetyFlags { delete_after_verify: true, auto_eject: true };
    gpbeam_cli::apply_safety_overrides(&mut cfg, &flags);

    assert!(cfg.delete_after_verify);
    assert!(cfg.auto_eject);
}

#[test]
fn absent_flags_preserve_config_values() {
    let mut cfg = base_cfg();
    cfg.delete_after_verify = true; // came from gpbeam.toml
    cfg.auto_eject = true;

    let flags = gpbeam_cli::SafetyFlags { delete_after_verify: false, auto_eject: false };
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
    let (positional, flags) = gpbeam_cli::parse_safety_flags(&argv);
    assert_eq!(positional, vec!["offload", "/card", "/dest"]);
    assert!(flags.delete_after_verify);
    assert!(flags.auto_eject);
}
