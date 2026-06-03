use crate::backoff::backoff_delay;
use crate::cloud::{CloudEvent, CloudUploader, ResumeState, UploadOutcome};
use crate::error::{is_retryable, Result};
use crate::ledger::{CloudJob, JobState, Ledger};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinSet;

/// Drives the persisted `cloud_jobs` queue: claims due jobs, uploads each via
/// the injected `CloudUploader`, and records terminal state. Opens its OWN
/// `Ledger` at `ledger_path` per call so network I/O never holds the DB lock.
pub struct CloudWorker {
    ledger_path: PathBuf,
    uploader: Arc<dyn CloudUploader>,
    // `destination_id` is part of the LOCKED ctor but not yet consumed.
    #[allow(dead_code)]
    destination_id: String,
    max_concurrency: usize,
    max_attempts: u32,
    delete_after_verify: bool,
}

/// Carries an upload result back from a spawned `JoinSet` task. When
/// `skipped` is set, `already_present` reported the remote object already
/// exists (G2) and `upload` was NOT called; the drain loop marks the job Done
/// without consulting `result`.
struct JobResult {
    job: CloudJob,
    /// `Ok(Some(_))` on a completed upload, `Ok(None)` when skipped as already
    /// present, `Err(_)` on an upload (or pre-flight `already_present`) failure.
    result: Result<Option<UploadOutcome>>,
}

impl CloudWorker {
    pub fn new(
        ledger_path: PathBuf,
        uploader: Arc<dyn CloudUploader>,
        destination_id: String,
        max_concurrency: usize,
        max_attempts: u32,
        delete_after_verify: bool,
    ) -> Self {
        CloudWorker {
            ledger_path,
            uploader,
            destination_id,
            max_concurrency,
            max_attempts,
            delete_after_verify,
        }
    }

    /// Claim up to `max_concurrency` due jobs, upload them concurrently, and
    /// record results. Returns the number of jobs that reached a TERMINAL
    /// state (Done or Failed) in this pass.
    pub async fn run_once(
        &self,
        now_unix: i64,
        emit: &mut (dyn FnMut(CloudEvent) + Send),
    ) -> Result<usize> {
        // Claim due jobs under our OWN connection, then release the lock before I/O.
        let claimed = {
            let mut ledger = Ledger::open(&self.ledger_path)?;
            ledger.claim_due_cloud_jobs(now_unix, self.max_concurrency)?
        };
        if claimed.is_empty() {
            return Ok(0);
        }

        let mut set: JoinSet<JobResult> = JoinSet::new();
        for job in claimed {
            let uploader = Arc::clone(&self.uploader);
            let ledger_path_for_task = self.ledger_path.clone();
            set.spawn(async move {
                let local = PathBuf::from(&job.local_path);
                let total = job.total_bytes;
                let job_id = job.id;
                // G1: a deterministic upload id derived from the job id so a
                // chunked upload resumes the SAME remote session across restarts.
                let upload_id = format!("gpbeam-{job_id}");
                let progress_ledger_path = ledger_path_for_task.clone();

                // G2 idempotent skip: if the remote object already exists, do
                // NOT upload — signal a skip and let the drain loop mark Done.
                match uploader.already_present(&job.remote_path, total).await {
                    Ok(true) => {
                        return JobResult { job, result: Ok(None) };
                    }
                    Ok(false) => {}
                    Err(e) => {
                        return JobResult { job, result: Err(e) };
                    }
                }

                // G1: carry the deterministic id + persisted byte count into
                // every attempt so `put_chunked` continues the same session.
                let resume = Some(ResumeState {
                    upload_id: Some(upload_id.clone()),
                    uploaded_bytes: job.uploaded_bytes,
                });

                // Persist progress as bytes arrive so an interrupted upload can
                // resume — keep the deterministic upload_id so it survives restarts.
                let progress_upload_id = upload_id.clone();
                let mut on_progress = |uploaded: u64| {
                    if let Ok(mut l) = Ledger::open(&progress_ledger_path) {
                        let state = ResumeState {
                            upload_id: Some(progress_upload_id.clone()),
                            uploaded_bytes: uploaded,
                        };
                        let _ = l.save_job_progress(job_id, uploaded, &state);
                    }
                };

                let result = uploader
                    .upload(&local, &job.remote_path, total, resume, &mut on_progress)
                    .await
                    .map(Some);
                JobResult { job, result }
            });
        }

        let mut terminal = 0usize;
        while let Some(joined) = set.join_next().await {
            // A spawned task panicking is a bug; surface it.
            let JobResult { job, result } = joined.expect("upload task panicked");

            // G2: a job whose remote object already existed was skipped (no
            // upload). Mark it Done + Mirrored without emitting an Uploading
            // event, then move on.
            if matches!(result, Ok(None)) {
                let mut ledger = Ledger::open(&self.ledger_path)?;
                ledger.mark_job_done(job.id)?;
                ledger.set_cloud_status(job.imported_id, "done")?;
                emit(CloudEvent::Mirrored { file: job.remote_path.clone() });
                terminal += 1;
                continue;
            }

            emit(CloudEvent::Uploading {
                file: job.remote_path.clone(),
                uploaded: 0,
                total: job.total_bytes,
            });

            match result {
                Ok(outcome) => {
                    let mut ledger = Ledger::open(&self.ledger_path)?;
                    // Record full byte count on success, then mark Done. A
                    // verified upload means the whole file landed; persist the
                    // deterministic upload_id so a later resume can reuse it.
                    let resume = ResumeState {
                        upload_id: Some(format!("gpbeam-{}", job.id)),
                        uploaded_bytes: job.total_bytes,
                    };
                    ledger.save_job_progress(job.id, job.total_bytes, &resume)?;
                    ledger.mark_job_done(job.id)?;
                    ledger.set_cloud_status(job.imported_id, "done")?;
                    emit(CloudEvent::Mirrored { file: job.remote_path.clone() });
                    let _ = outcome; // remote_ref/etag retained for future use

                    // Auto-mirror delete-after-verify: now that the cloud copy is
                    // Done, the on-card source may be removed. A post-upload delete
                    // failure must NOT regress the successful upload — surface it via
                    // CloudFailed (logged) per Corrections #Minor, but the Mirrored
                    // success already stands.
                    if self.delete_after_verify {
                        if let Some(src) = job.card_src.as_deref() {
                            match std::fs::remove_file(src) {
                                Ok(()) => emit(CloudEvent::Deleted { file: src.to_string() }),
                                Err(e) => emit(CloudEvent::CloudFailed {
                                    file: src.to_string(),
                                    error: format!("post-upload delete failed: {e}"),
                                }),
                            }
                        }
                    }
                    terminal += 1;
                }
                Err(e) => {
                    // `mark_job_failed` increments attempts (attempts+1); the
                    // ledger does NOT bump attempts at claim time. So the attempt
                    // number that just failed is `job.attempts + 1`, and after the
                    // mark the row's attempts will equal that. Use it for both the
                    // exhaustion check and the backoff schedule.
                    let attempt_num = job.attempts.saturating_add(1);
                    let err_text = e.to_string();
                    let mut ledger = Ledger::open(&self.ledger_path)?;
                    if is_retryable(&e) && attempt_num < self.max_attempts {
                        // Reschedule: park in Failed with a future next_retry_at.
                        let delay = backoff_delay(attempt_num, crate::backoff::jitter_ms());
                        let next = now_unix.saturating_add(delay.as_secs() as i64);
                        ledger.mark_job_failed(job.id, &err_text, Some(next))?;
                        // NOT terminal: do not increment `terminal`.
                    } else {
                        // Terminal failure (non-retryable, or retries exhausted):
                        // park in Failed with no retry, mark the imported row, and
                        // notify.
                        ledger.mark_job_failed(job.id, &err_text, None)?;
                        ledger.set_cloud_status(job.imported_id, "failed")?;
                        emit(CloudEvent::CloudFailed {
                            file: job.remote_path.clone(),
                            error: err_text,
                        });
                        terminal += 1;
                    }
                }
            }
        }

        Ok(terminal)
    }

    /// Loop `run_once` until no cloud jobs remain. When a pass makes no terminal
    /// progress but jobs are still pending (parked in `Failed` awaiting a future
    /// `next_retry_at`), sleep until the nearest retry is due before looping
    /// again, so the loop never busy-spins.
    pub async fn run_until_drained(
        &self,
        emit: &mut (dyn FnMut(CloudEvent) + Send),
    ) -> Result<()> {
        loop {
            // Are there any jobs left to do (Queued or retry-due/parked Failed)?
            let pending = {
                let ledger = Ledger::open(&self.ledger_path)?;
                ledger.pending_cloud_count()?
            };
            if pending == 0 {
                return Ok(());
            }

            let now = now_unix();
            let terminal = self.run_once(now, emit).await?;

            if terminal == 0 {
                // No job became terminal this pass. Either nothing was due yet
                // (all parked Failed with a future next_retry_at) or all due jobs
                // got rescheduled. Sleep until the soonest retry to avoid spinning.
                let sleep_secs = self.secs_until_next_retry(now)?;
                if let Some(secs) = sleep_secs {
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                } else {
                    // Pending but nothing due and no retry time known: yield once,
                    // then re-check. Prevents a hot loop in degenerate states.
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    /// Seconds until the soonest `next_retry_at` among parked Failed jobs that are
    /// still in the future relative to `now`. `None` if nothing is scheduled.
    fn secs_until_next_retry(&self, now: i64) -> Result<Option<u64>> {
        let ledger = Ledger::open(&self.ledger_path)?;
        let failed = ledger.list_cloud_jobs(Some(JobState::Failed))?;
        let soonest = failed
            .iter()
            .filter_map(|j| j.next_retry_at)
            .filter(|&t| t > now)
            .min();
        Ok(soonest.map(|t| (t - now).max(0) as u64))
    }
}

/// Current Unix time in seconds. Isolated so production code has a single
/// clock source; tests drive `run_once` with explicit timestamps instead.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{CloudUploader, ResumeState, UploadOutcome};
    use crate::error::CoreError;
    use crate::ledger::{JobState, Ledger};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A scriptable uploader for worker tests. Never touches the network.
    /// `behaviors` is consumed one entry per `upload` call (round-robins the
    /// last entry if calls exceed the script), so successive attempts can
    /// differ. Each behavior optionally fires the progress callback first.
    struct MockUploader {
        behaviors: Vec<Behavior>,
        calls: AtomicUsize,
        present: bool,
        /// The `ResumeState` handed to the LAST `upload()` call, captured for
        /// G1 assertions (`None` until `upload` is invoked once).
        last_resume: Mutex<Option<ResumeState>>,
    }

    struct Behavior {
        /// Bytes to report via the progress callback before returning.
        progress_to: Option<u64>,
        /// The result of this attempt.
        outcome: AttemptOutcome,
    }

    enum AttemptOutcome {
        Ok { bytes: u64, etag: Option<String> },
        // Consumed by the scripted error tests added in Tasks 3.3+.
        #[allow(dead_code)]
        Err(CoreError),
    }

    /// Rebuild a `CoreError` by value from a borrow (CoreError is not `Clone`).
    /// Mirrors the helper in `cloud::mod::test_support`.
    fn clone_err(err: &CoreError) -> CoreError {
        match err {
            CoreError::CloudAuth(m) => CoreError::CloudAuth(m.clone()),
            CoreError::Http { status, msg } => CoreError::Http {
                status: *status,
                msg: msg.clone(),
            },
            CoreError::Config(m) => CoreError::Config(m.clone()),
            other => CoreError::Config(format!("{other}")),
        }
    }

    impl MockUploader {
        fn ok(bytes: u64) -> Self {
            MockUploader {
                behaviors: vec![Behavior {
                    progress_to: None,
                    outcome: AttemptOutcome::Ok { bytes, etag: Some("etag-1".into()) },
                }],
                calls: AtomicUsize::new(0),
                present: false,
                last_resume: Mutex::new(None),
            }
        }

        #[allow(dead_code)]
        fn scripted(behaviors: Vec<Behavior>) -> Self {
            MockUploader {
                behaviors,
                calls: AtomicUsize::new(0),
                present: false,
                last_resume: Mutex::new(None),
            }
        }

        /// An uploader whose `already_present` always returns `true` (G2 skip).
        fn already_present() -> Self {
            MockUploader {
                behaviors: vec![Behavior {
                    progress_to: None,
                    outcome: AttemptOutcome::Ok { bytes: 0, etag: None },
                }],
                calls: AtomicUsize::new(0),
                present: true,
                last_resume: Mutex::new(None),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The `ResumeState` captured from the last `upload()` call, if any.
        fn last_resume(&self) -> Option<ResumeState> {
            self.last_resume.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CloudUploader for MockUploader {
        async fn already_present(&self, _remote: &str, _size: u64) -> Result<bool> {
            Ok(self.present)
        }

        async fn upload(
            &self,
            _local: &Path,
            _remote: &str,
            total: u64,
            resume: Option<ResumeState>,
            progress: &mut (dyn FnMut(u64) + Send),
        ) -> Result<UploadOutcome> {
            *self.last_resume.lock().unwrap() = resume;
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let b = self
                .behaviors
                .get(idx)
                .or_else(|| self.behaviors.last())
                .expect("at least one behavior");
            if let Some(p) = b.progress_to {
                progress(p);
            }
            match &b.outcome {
                AttemptOutcome::Ok { bytes, etag } => Ok(UploadOutcome {
                    remote_ref: "remote/ref".into(),
                    bytes: if *bytes == 0 { total } else { *bytes },
                    etag: etag.clone(),
                }),
                AttemptOutcome::Err(e) => Err(clone_err(e)),
            }
        }
    }

    /// Build a ledger at a temp path with one queued job, returning the temp
    /// dir (keep it alive) and the job id.
    fn ledger_with_one_job(dir: &TempDir) -> (PathBuf, i64) {
        let path = dir.path().join("ledger.sqlite");
        let mut l = Ledger::open(&path).unwrap();
        let imported_id = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", Some("h"))
            .unwrap();
        let job_id = l
            .enqueue_cloud_job(
                imported_id,
                "nc1",
                "/dest/GX010001.MP4",
                "GX010001.MP4",
                4096,
                None,
            )
            .unwrap();
        (path, job_id)
    }

    #[tokio::test]
    async fn one_job_uploads_to_done_emits_mirrored_and_sets_cloud_status() {
        let dir = TempDir::new().unwrap();
        let (ledger_path, job_id) = ledger_with_one_job(&dir);

        let uploader = Arc::new(MockUploader::ok(4096));
        let worker = CloudWorker::new(
            ledger_path.clone(),
            uploader.clone(),
            "nc1".into(),
            2,
            8,
            false,
        );

        let mut events: Vec<CloudEvent> = Vec::new();
        let terminal = worker
            .run_once(1000, &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(terminal, 1, "one job reached a terminal state");
        assert_eq!(uploader.call_count(), 1);

        // Job is Done in the ledger.
        let l = Ledger::open(&ledger_path).unwrap();
        let jobs = l.list_cloud_jobs(Some(JobState::Done)).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        assert_eq!(jobs[0].state, JobState::Done);
        assert_eq!(jobs[0].uploaded_bytes, 4096);
        assert_eq!(l.pending_cloud_count().unwrap(), 0);

        // A Mirrored event was emitted for the file.
        assert!(events.iter().any(|e| matches!(
            e,
            CloudEvent::Mirrored { file } if file == "GX010001.MP4"
        )));
        // An Uploading event was emitted at least once (start-of-upload).
        assert!(events.iter().any(|e| matches!(e, CloudEvent::Uploading { .. })));
    }

    #[tokio::test]
    async fn retryable_error_reschedules_then_succeeds_on_next_run() {
        let dir = TempDir::new().unwrap();
        let (ledger_path, job_id) = ledger_with_one_job(&dir);

        // First attempt: retryable HTTP 503. Second attempt: success.
        let uploader = Arc::new(MockUploader::scripted(vec![
            Behavior {
                progress_to: None,
                outcome: AttemptOutcome::Err(CoreError::Http {
                    status: Some(503),
                    msg: "service unavailable".into(),
                }),
            },
            Behavior {
                progress_to: None,
                outcome: AttemptOutcome::Ok { bytes: 4096, etag: Some("e".into()) },
            },
        ]));
        let worker =
            CloudWorker::new(ledger_path.clone(), uploader.clone(), "nc1".into(), 2, 8, false);

        // Pass 1 at t=1000: fails retryably, reschedules to next_retry_at = 1000 + ~2s.
        let mut ev1: Vec<CloudEvent> = Vec::new();
        let t1 = worker.run_once(1000, &mut |e| ev1.push(e)).await.unwrap();
        assert_eq!(t1, 0, "a rescheduled job is NOT terminal");
        assert!(
            !ev1.iter().any(|e| matches!(e, CloudEvent::CloudFailed { .. })),
            "retryable failure must not emit CloudFailed"
        );

        {
            let l = Ledger::open(&ledger_path).unwrap();
            let failed = l.list_cloud_jobs(Some(JobState::Failed)).unwrap();
            assert_eq!(failed.len(), 1, "job is parked in Failed awaiting retry");
            assert_eq!(failed[0].attempts, 1);
            assert!(failed[0].next_retry_at.unwrap() >= 1000 + 2);
            assert!(failed[0].last_error.as_deref().unwrap().contains("503"));
            assert_eq!(l.pending_cloud_count().unwrap(), 1, "still pending");
        }

        // Pass 2 well after the retry window: claim_due picks up the due Failed job.
        let mut ev2: Vec<CloudEvent> = Vec::new();
        let t2 = worker.run_once(100_000, &mut |e| ev2.push(e)).await.unwrap();
        assert_eq!(t2, 1, "now terminal (Done)");
        assert_eq!(uploader.call_count(), 2);

        let l = Ledger::open(&ledger_path).unwrap();
        let done = l.list_cloud_jobs(Some(JobState::Done)).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].id, job_id);
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn non_retryable_auth_error_is_terminal_and_emits_cloud_failed() {
        let dir = TempDir::new().unwrap();
        let (ledger_path, job_id) = ledger_with_one_job(&dir);

        let uploader = Arc::new(MockUploader::scripted(vec![Behavior {
            progress_to: None,
            outcome: AttemptOutcome::Err(CoreError::CloudAuth("bad app password".into())),
        }]));
        let worker =
            CloudWorker::new(ledger_path.clone(), uploader.clone(), "nc1".into(), 2, 8, false);

        let mut events: Vec<CloudEvent> = Vec::new();
        let terminal = worker.run_once(1000, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(terminal, 1, "a non-retryable failure IS terminal");
        assert_eq!(uploader.call_count(), 1);

        // CloudFailed emitted with the error text.
        assert!(events.iter().any(|e| matches!(
            e,
            CloudEvent::CloudFailed { file, error }
                if file == "GX010001.MP4" && error.contains("bad app password")
        )));

        let l = Ledger::open(&ledger_path).unwrap();
        let failed = l.list_cloud_jobs(Some(JobState::Failed)).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, job_id);
        // Terminal failure: no future retry scheduled.
        assert!(failed[0].next_retry_at.is_none());
        // pending_cloud_count counts only Queued + retry-due Failed; a terminal
        // Failed with no next_retry_at is NOT pending.
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn exhausting_max_attempts_makes_a_retryable_error_terminal() {
        let dir = TempDir::new().unwrap();
        let (ledger_path, _job_id) = ledger_with_one_job(&dir);

        // Always a retryable 503; max_attempts = 1 so the first failure is terminal.
        let uploader = Arc::new(MockUploader::scripted(vec![Behavior {
            progress_to: None,
            outcome: AttemptOutcome::Err(CoreError::Http {
                status: Some(503),
                msg: "still down".into(),
            }),
        }]));
        let worker =
            CloudWorker::new(ledger_path.clone(), uploader.clone(), "nc1".into(), 2, 1, false);

        let mut events: Vec<CloudEvent> = Vec::new();
        let terminal = worker.run_once(1000, &mut |e| events.push(e)).await.unwrap();
        assert_eq!(terminal, 1, "retries exhausted -> terminal");
        assert!(events.iter().any(|e| matches!(e, CloudEvent::CloudFailed { .. })));

        let l = Ledger::open(&ledger_path).unwrap();
        let failed = l.list_cloud_jobs(Some(JobState::Failed)).unwrap();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].next_retry_at.is_none(), "no retry after exhaustion");
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn progress_is_persisted_and_next_claim_resumes() {
        let dir = TempDir::new().unwrap();
        // A larger job so partial progress is meaningful.
        let path = dir.path().join("ledger.sqlite");
        let job_id = {
            let mut l = Ledger::open(&path).unwrap();
            let imported_id = l
                .record("C346", "GX010002.MP4", 100_000, 1000, "/dest/GX010002.MP4", Some("h"))
                .unwrap();
            l.enqueue_cloud_job(imported_id, "nc1", "/dest/GX010002.MP4", "GX010002.MP4", 100_000, None)
                .unwrap()
        };

        // Attempt 1: report 60_000 bytes via progress, THEN fail retryably.
        // Attempt 2: succeed (and must SEE the persisted resume state).
        let uploader = Arc::new(MockUploader::scripted(vec![
            Behavior {
                progress_to: Some(60_000),
                outcome: AttemptOutcome::Err(CoreError::Http {
                    status: Some(429),
                    msg: "slow down".into(),
                }),
            },
            Behavior {
                progress_to: Some(100_000),
                outcome: AttemptOutcome::Ok { bytes: 100_000, etag: Some("e".into()) },
            },
        ]));
        let worker = CloudWorker::new(path.clone(), uploader.clone(), "nc1".into(), 2, 8, false);

        // Pass 1: partial then retryable fail.
        worker.run_once(1000, &mut |_| {}).await.unwrap();

        // The 60_000 bytes of progress are persisted on the (now Failed) row.
        {
            let l = Ledger::open(&path).unwrap();
            let jobs = l.list_cloud_jobs(Some(JobState::Failed)).unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].id, job_id);
            assert_eq!(jobs[0].uploaded_bytes, 60_000, "progress persisted across the failure");
            let resume = jobs[0].resume_state.as_ref().expect("resume_state persisted");
            assert_eq!(resume.uploaded_bytes, 60_000);
        }

        // Pass 2 after the retry window: resumes and finishes.
        worker.run_once(100_000, &mut |_| {}).await.unwrap();
        {
            let l = Ledger::open(&path).unwrap();
            let done = l.list_cloud_jobs(Some(JobState::Done)).unwrap();
            assert_eq!(done.len(), 1);
            assert_eq!(done[0].uploaded_bytes, 100_000);
            assert_eq!(l.pending_cloud_count().unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn already_present_remote_skips_upload_and_marks_done() {
        // G2 idempotent skip: a queued job whose remote object already exists
        // must reach Done and emit Mirrored WITHOUT the uploader being called.
        let dir = TempDir::new().unwrap();
        let (ledger_path, job_id) = ledger_with_one_job(&dir);

        let uploader = Arc::new(MockUploader::already_present());
        let worker = CloudWorker::new(
            ledger_path.clone(),
            uploader.clone(),
            "nc1".into(),
            2,
            8,
            false,
        );

        let mut events: Vec<CloudEvent> = Vec::new();
        let terminal = worker
            .run_once(1000, &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(terminal, 1, "the skipped job reached a terminal state");
        assert_eq!(
            uploader.call_count(),
            0,
            "upload() must NEVER be called when already_present is true"
        );

        // Job is Done in the ledger; imported row marked done; queue drained.
        let l = Ledger::open(&ledger_path).unwrap();
        let done = l.list_cloud_jobs(Some(JobState::Done)).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].id, job_id);
        assert_eq!(l.pending_cloud_count().unwrap(), 0);

        // A Mirrored event was emitted for the file.
        assert!(events.iter().any(|e| matches!(
            e,
            CloudEvent::Mirrored { file } if file == "GX010001.MP4"
        )));
    }

    #[tokio::test]
    async fn worker_passes_deterministic_resume_id_and_carries_uploaded_bytes() {
        // G1 deterministic chunked resume: the worker must derive
        // upload_id = "gpbeam-{job.id}" and pass it (plus the persisted
        // uploaded_bytes) into upload() on every attempt. Seed a NON-ZERO
        // uploaded_bytes so the carry-through across a retry is proven.
        let dir = TempDir::new().unwrap();
        let (ledger_path, job_id) = ledger_with_one_job(&dir);

        // Persist partial progress (2048 of 4096) so the job carries a non-zero
        // uploaded_bytes into the next claim.
        {
            let mut l = Ledger::open(&ledger_path).unwrap();
            let resume = ResumeState { upload_id: None, uploaded_bytes: 2048 };
            l.save_job_progress(job_id, 2048, &resume).unwrap();
        }

        let uploader = Arc::new(MockUploader::ok(4096));
        let worker = CloudWorker::new(
            ledger_path.clone(),
            uploader.clone(),
            "nc1".into(),
            2,
            8,
            false,
        );

        worker.run_once(1000, &mut |_| {}).await.unwrap();

        assert_eq!(uploader.call_count(), 1, "upload was attempted once");
        let resume = uploader
            .last_resume()
            .expect("worker passed a ResumeState into upload()");
        assert_eq!(
            resume.upload_id,
            Some(format!("gpbeam-{job_id}")),
            "deterministic upload_id derived from the job id"
        );
        assert_eq!(
            resume.uploaded_bytes, 2048,
            "the persisted uploaded_bytes is carried into the upload"
        );
    }

    #[tokio::test]
    async fn run_until_drained_finishes_all_queued_jobs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let (id_a, id_b) = {
            let mut l = Ledger::open(&path).unwrap();
            let imp_a = l
                .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", Some("h"))
                .unwrap();
            let imp_b = l
                .record("C346", "GX010002.MP4", 8192, 1001, "/dest/GX010002.MP4", Some("h"))
                .unwrap();
            let a = l
                .enqueue_cloud_job(imp_a, "nc1", "/dest/GX010001.MP4", "GX010001.MP4", 4096, None)
                .unwrap();
            let b = l
                .enqueue_cloud_job(imp_b, "nc1", "/dest/GX010002.MP4", "GX010002.MP4", 8192, None)
                .unwrap();
            (a, b)
        };

        // Always succeeds; behavior round-robins the last entry across both jobs.
        let uploader = Arc::new(MockUploader::ok(0)); // 0 => report `total`
        // max_concurrency = 1 forces TWO run_once passes, exercising the loop.
        let worker = CloudWorker::new(path.clone(), uploader.clone(), "nc1".into(), 1, 8, false);

        let mut events: Vec<CloudEvent> = Vec::new();
        worker
            .run_until_drained(&mut |e| events.push(e))
            .await
            .unwrap();

        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.pending_cloud_count().unwrap(), 0, "queue fully drained");
        let done = l.list_cloud_jobs(Some(JobState::Done)).unwrap();
        let done_ids: Vec<i64> = done.iter().map(|j| j.id).collect();
        assert!(done_ids.contains(&id_a));
        assert!(done_ids.contains(&id_b));
        assert_eq!(done.len(), 2);

        // Both files mirrored.
        let mirrored = events
            .iter()
            .filter(|e| matches!(e, CloudEvent::Mirrored { .. }))
            .count();
        assert_eq!(mirrored, 2);
    }

    /// Fake uploader: succeeds or fails deterministically; never touches a network.
    struct FakeUploader {
        succeed: bool,
    }

    #[async_trait]
    impl CloudUploader for FakeUploader {
        async fn already_present(&self, _remote: &str, _size: u64) -> Result<bool> {
            Ok(false)
        }
        async fn upload(
            &self,
            _local: &Path,
            remote: &str,
            total: u64,
            _resume: Option<ResumeState>,
            progress: &mut (dyn FnMut(u64) + Send),
        ) -> Result<UploadOutcome> {
            progress(total);
            if self.succeed {
                Ok(UploadOutcome { remote_ref: remote.to_string(), bytes: total, etag: Some("\"e\"".into()) })
            } else {
                Err(CoreError::Http { status: Some(500), msg: "boom".into() })
            }
        }
    }

    /// Seed an on-disk ledger with one queued job whose card_src points at a
    /// real temp file (so the worker can actually delete it).
    fn seed_job(succeed_card: &Path) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("ledger.sqlite");
        let mut l = Ledger::open(&ledger_path).unwrap();
        let imported_id = l
            .record("C346", "GX010001.MP4", 8, 1000, "/dest/GX010001.MP4", None)
            .unwrap();
        l.enqueue_cloud_job(
            imported_id,
            "nc1",
            "/dest/GX010001.MP4",
            "GoPro/GX010001.MP4",
            8,
            Some(&succeed_card.to_string_lossy()),
        )
        .unwrap();
        (dir, ledger_path)
    }

    #[tokio::test]
    async fn worker_deletes_card_src_after_done() {
        let card_dir = tempfile::TempDir::new().unwrap();
        let card_file = card_dir.path().join("GX010001.MP4");
        std::fs::write(&card_file, b"12345678").unwrap();

        let (_keep, ledger_path) = seed_job(&card_file);
        let uploader: Arc<dyn CloudUploader> = Arc::new(FakeUploader { succeed: true });
        let worker = CloudWorker::new(ledger_path, uploader, "nc1".into(), 2, 8, true);

        let mut events = Vec::new();
        worker
            .run_until_drained(&mut |e| events.push(e))
            .await
            .unwrap();

        assert!(!card_file.exists(), "card source deleted after cloud Done");
        assert!(events.iter().any(|e| matches!(e, CloudEvent::Deleted { .. })));
    }

    #[tokio::test]
    async fn worker_keeps_card_src_on_failure() {
        let card_dir = tempfile::TempDir::new().unwrap();
        let card_file = card_dir.path().join("GX010001.MP4");
        std::fs::write(&card_file, b"12345678").unwrap();

        let (_keep, ledger_path) = seed_job(&card_file);
        let uploader: Arc<dyn CloudUploader> = Arc::new(FakeUploader { succeed: false });
        // max_attempts = 1 so the job exhausts in a single run_once.
        let worker = CloudWorker::new(ledger_path, uploader, "nc1".into(), 2, 1, true);

        let mut events = Vec::new();
        worker.run_once(1000, &mut |e| events.push(e)).await.unwrap();

        assert!(card_file.exists(), "failed upload must NOT delete the card source");
        assert!(!events.iter().any(|e| matches!(e, CloudEvent::Deleted { .. })));
    }
}
