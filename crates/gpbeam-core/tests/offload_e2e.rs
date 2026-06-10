#[path = "fixtures.rs"]
mod fixtures;

use gpbeam_core::config::Config;
use gpbeam_core::ledger::Ledger;
use gpbeam_core::orchestrator::run_offload;

#[test]
fn full_offload_then_replug_is_idempotent_and_verified() {
    let card = fixtures::hero11_card();
    let dest = fixtures::dest();
    let cfg = Config::new(dest.path().to_path_buf());
    let ledger_file = tempfile::NamedTempFile::new().unwrap();
    let mut ledger = Ledger::open(ledger_file.path()).unwrap();

    let s1 = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
    assert_eq!(s1.copied, 4);
    assert_eq!(s1.failed, 0);

    // Destination contents match source bytes for the MP4 (verify the copy is faithful).
    let src = std::fs::read(card.root().join("DCIM/100GOPRO/GX010001.MP4")).unwrap();
    let copied_name = std::fs::read_dir(dest.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().ends_with("_GX010001.MP4"))
        .unwrap()
        .path();
    assert_eq!(std::fs::read(&copied_name).unwrap(), src);

    // Re-plug: nothing new.
    let s2 = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
    assert_eq!(s2.copied, 0);
    assert_eq!(s2.skipped, 4);
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 4);
}

#[test]
fn ledger_persists_across_reopen() {
    let card = fixtures::hero11_card();
    let dest = fixtures::dest();
    let cfg = Config::new(dest.path().to_path_buf());
    let ledger_file = tempfile::NamedTempFile::new().unwrap();

    {
        let mut ledger = Ledger::open(ledger_file.path()).unwrap();
        run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
    }
    // Reopen the same DB file -> dedup still holds.
    let mut ledger = Ledger::open(ledger_file.path()).unwrap();
    let s = run_offload(card.root(), &cfg, &mut ledger, &mut |_| {}).unwrap();
    assert_eq!(s.copied, 0);
    assert_eq!(s.skipped, 4);
}
