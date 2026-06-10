use gpbeam_core::ledger::{JobState, Ledger};

/// `retry_cloud` re-queues only the terminally-`Failed` jobs (the worker gave up:
/// `next_retry_at IS NULL`), resetting them to `Queued` with `attempts = 0` so the
/// next worker pass picks them up. A `Failed`-but-pending-retry job (non-NULL
/// `next_retry_at`) is left untouched.
#[test]
fn retry_cloud_requeues_only_terminally_failed_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let lpath = gpbeam_cli::ledger_path_for(&dest);

    let (terminal_id, pending_id) = {
        let mut ledger = Ledger::open(&lpath).unwrap();

        // Job A: driven to a TERMINAL Failed state (worker gave up -> next_retry_at NULL).
        let imp_a = ledger
            .record(
                "C123",
                "GX010003.MP4",
                42,
                1_700_000_200,
                "/dest/GX010003.MP4",
                None,
            )
            .unwrap();
        let job_a = ledger
            .enqueue_cloud_job(
                imp_a,
                "home-nc",
                "/dest/GX010003.MP4",
                "GoProBackup/GX010003.MP4",
                42,
                None,
            )
            .unwrap();
        ledger
            .mark_job_failed(job_a, "401 unauthorized", None)
            .unwrap();

        // Job B: Failed but still PENDING a retry (non-NULL next_retry_at) -> must NOT be touched.
        let imp_b = ledger
            .record(
                "C123",
                "GX010004.MP4",
                7,
                1_700_000_300,
                "/dest/GX010004.MP4",
                None,
            )
            .unwrap();
        let job_b = ledger
            .enqueue_cloud_job(
                imp_b,
                "home-nc",
                "/dest/GX010004.MP4",
                "GoProBackup/GX010004.MP4",
                7,
                None,
            )
            .unwrap();
        ledger
            .mark_job_failed(job_b, "503 transient", Some(9_999_999_999))
            .unwrap();

        // Only the pending-retry job is counted as pending; the terminal one is not (Contract C2).
        assert_eq!(ledger.pending_cloud_count().unwrap(), 1);
        (job_a, job_b)
    };

    let requeued = gpbeam_cli::retry_cloud(&dest).unwrap();
    assert_eq!(requeued, 1, "exactly one terminally-failed job re-queued");

    let ledger = Ledger::open(&lpath).unwrap();

    let queued = ledger.list_cloud_jobs(Some(JobState::Queued)).unwrap();
    assert_eq!(queued.len(), 1, "the terminal job is Queued again");
    let job = &queued[0];
    assert_eq!(job.id, terminal_id);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 0, "attempts reset to 0");
    assert_eq!(job.next_retry_at, None, "next_retry_at cleared");
    assert_eq!(job.last_error, None, "error cleared");

    // The pending-retry job is still Failed and untouched.
    let failed = ledger.list_cloud_jobs(Some(JobState::Failed)).unwrap();
    assert_eq!(failed.len(), 1, "the pending-retry job stays Failed");
    assert_eq!(failed[0].id, pending_id);
    assert_eq!(failed[0].next_retry_at, Some(9_999_999_999));

    // Both jobs now count as pending (the requeued Queued + the pending-retry Failed).
    assert_eq!(ledger.pending_cloud_count().unwrap(), 2);
}
