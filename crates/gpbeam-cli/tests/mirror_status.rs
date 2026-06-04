use gpbeam_core::ledger::Ledger;

#[test]
fn mirror_status_lists_jobs_by_state_and_pending_count() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let lpath = gpbeam_cli::ledger_path_for(&dest);

    // Seed: one imported row + two cloud jobs, mark one Done.
    {
        let mut ledger = Ledger::open(&lpath).unwrap();
        let imp = ledger
            .record("C123", "GX010001.MP4", 17, 1_700_000_000, "/dest/GX010001.MP4", None)
            .unwrap();
        let job_done = ledger
            .enqueue_cloud_job(imp, "home-nc", "/dest/GX010001.MP4", "GoProBackup/GX010001.MP4", 17, None)
            .unwrap();
        let imp2 = ledger
            .record("C123", "GX010002.MP4", 99, 1_700_000_100, "/dest/GX010002.MP4", None)
            .unwrap();
        let _job_queued = ledger
            .enqueue_cloud_job(imp2, "home-nc", "/dest/GX010002.MP4", "GoProBackup/GX010002.MP4", 99, None)
            .unwrap();
        ledger.mark_job_done(job_done).unwrap();
    }

    let lines = gpbeam_cli::mirror_status_lines(&dest).unwrap();
    let joined = lines.join("\n");
    assert!(joined.contains("GX010001.MP4"), "done job listed: {joined}");
    assert!(joined.contains("GX010002.MP4"), "queued job listed: {joined}");
    assert!(joined.contains("done"), "state label present: {joined}");
    assert!(joined.contains("queued"), "state label present: {joined}");
    // exactly one job is still pending (the queued one).
    assert!(
        lines.iter().any(|l| l.contains("pending: 1")),
        "pending count line: {joined}"
    );
}
