use crate::cloud::ResumeState;
use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;

pub struct Ledger {
    conn: Connection,
}

/// Lifecycle of a cloud upload job. Stored as lowercase words in `cloud_jobs.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Uploading,
    Done,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Uploading => "uploading",
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }

    // Name is part of the LOCKED Shared Contract (`as_str`/`from_str`); keep it
    // even though it shadows `std::str::FromStr::from_str`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<JobState> {
        match s {
            "queued" => Some(JobState::Queued),
            "uploading" => Some(JobState::Uploading),
            "done" => Some(JobState::Done),
            "failed" => Some(JobState::Failed),
            _ => None,
        }
    }
}

/// One row of the `cloud_jobs` queue.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudJob {
    pub id: i64,
    pub imported_id: i64,
    pub destination_id: String,
    pub local_path: String,
    pub remote_path: String,
    pub card_src: Option<String>,
    pub state: JobState,
    pub attempts: u32,
    pub next_retry_at: Option<i64>,
    pub last_error: Option<String>,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub resume_state: Option<ResumeState>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // WAL + busy_timeout let the sync offload connection and the async cloud
        // worker's own connection share this file without "database is locked".
        // journal_mode returns a row, so use query_row (execute_batch would error
        // on the result set). WAL is a no-op / "memory" for in-memory dbs.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_r| Ok(()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE imported (
                    id            INTEGER PRIMARY KEY,
                    camera_serial TEXT NOT NULL,
                    name          TEXT NOT NULL,
                    size          INTEGER NOT NULL,
                    mtime_unix    INTEGER NOT NULL,
                    dest_path     TEXT NOT NULL,
                    hash          TEXT,
                    copied_at     TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE(camera_serial, name, size, mtime_unix)
                 );
                 PRAGMA user_version = 1;",
            )?;
        }
        if version < 2 {
            // Additive v2 migration: cloud-job queue + per-file cloud status.
            // cloud_jobs includes card_src from the start (Shared Contract C3).
            conn.execute_batch(
                "ALTER TABLE imported ADD COLUMN cloud_status TEXT;
                 CREATE TABLE cloud_jobs (
                     id             INTEGER PRIMARY KEY,
                     imported_id    INTEGER NOT NULL,
                     destination_id TEXT NOT NULL,
                     local_path     TEXT NOT NULL,
                     remote_path    TEXT NOT NULL,
                     card_src       TEXT,
                     state          TEXT NOT NULL,
                     attempts       INTEGER NOT NULL DEFAULT 0,
                     next_retry_at  INTEGER,
                     last_error     TEXT,
                     total_bytes    INTEGER NOT NULL DEFAULT 0,
                     uploaded_bytes INTEGER NOT NULL DEFAULT 0,
                     resume_state   TEXT,
                     created_at     TEXT NOT NULL DEFAULT (datetime('now')),
                     FOREIGN KEY (imported_id) REFERENCES imported(id)
                 );
                 CREATE INDEX idx_cloud_jobs_state ON cloud_jobs(state, next_retry_at);
                 PRAGMA user_version = 2;",
            )?;
        }
        Ok(Ledger { conn })
    }

    /// Cheap dedup pre-check — never hashes the file.
    pub fn is_imported(&self, serial: &str, name: &str, size: u64, mtime_unix: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM imported
             WHERE camera_serial=?1 AND name=?2 AND size=?3 AND mtime_unix=?4",
            rusqlite::params![serial, name, size as i64, mtime_unix],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record (or update) an imported file. Returns the `imported` row id,
    /// read back via the UNIQUE key so it is correct even when
    /// `INSERT OR REPLACE` allocates a fresh rowid.
    pub fn record(
        &mut self,
        serial: &str,
        name: &str,
        size: u64,
        mtime_unix: i64,
        dest_path: &str,
        hash: Option<&str>,
    ) -> Result<i64> {
        // Upsert (ON CONFLICT DO UPDATE) rather than INSERT OR REPLACE so the
        // existing rowid is preserved on a re-record of the same UNIQUE key
        // (REPLACE would delete + re-insert, allocating a fresh rowid).
        self.conn.execute(
            "INSERT INTO imported
             (camera_serial, name, size, mtime_unix, dest_path, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(camera_serial, name, size, mtime_unix)
             DO UPDATE SET dest_path=excluded.dest_path, hash=excluded.hash",
            rusqlite::params![serial, name, size as i64, mtime_unix, dest_path, hash],
        )?;
        // Read the id back by UNIQUE key (robust to rowid reassignment).
        let id = self
            .imported_id(serial, name, size, mtime_unix)?
            .expect("row just inserted must exist");
        Ok(id)
    }

    /// Look up the `imported` row id for a UNIQUE key, if present.
    pub fn imported_id(
        &self,
        serial: &str,
        name: &str,
        size: u64,
        mtime_unix: i64,
    ) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM imported
                 WHERE camera_serial=?1 AND name=?2 AND size=?3 AND mtime_unix=?4",
                rusqlite::params![serial, name, size as i64, mtime_unix],
                |r| r.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(id)
    }

    /// Stamp the per-file cloud status (e.g. "queued", "done").
    pub fn set_cloud_status(&self, imported_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE imported SET cloud_status=?2 WHERE id=?1",
            rusqlite::params![imported_id, status],
        )?;
        Ok(())
    }

    /// Insert a new Queued cloud job for an already-imported file. Returns the job id.
    /// `card_src` is the on-card source path, retained so the worker can delete the
    /// original after a verified cloud upload (Auto + delete-after-verify).
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_cloud_job(
        &mut self,
        imported_id: i64,
        destination_id: &str,
        local_path: &str,
        remote_path: &str,
        total_bytes: u64,
        card_src: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO cloud_jobs
             (imported_id, destination_id, local_path, remote_path, card_src, state,
              attempts, total_bytes, uploaded_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, 0)",
            rusqlite::params![
                imported_id,
                destination_id,
                local_path,
                remote_path,
                card_src,
                JobState::Queued.as_str(),
                total_bytes as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Count jobs not yet finished: Queued, Uploading, or Failed-pending-retry
    /// (Failed with a non-NULL `next_retry_at`). A terminal Failed job
    /// (`next_retry_at` NULL) is NOT counted (Shared Contract C2), so
    /// `run_until_drained` does not spin forever.
    pub fn pending_cloud_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM cloud_jobs
             WHERE state=?1 OR state=?2
                OR (state=?3 AND next_retry_at IS NOT NULL)",
            rusqlite::params![
                JobState::Queued.as_str(),
                JobState::Uploading.as_str(),
                JobState::Failed.as_str(),
            ],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// List cloud jobs, optionally filtered to one state, ordered by id.
    pub fn list_cloud_jobs(&self, state: Option<JobState>) -> Result<Vec<CloudJob>> {
        let mut stmt;
        let mut rows = match state {
            Some(s) => {
                stmt = self.conn.prepare(
                    "SELECT id, imported_id, destination_id, local_path, remote_path,
                            card_src, state, attempts, next_retry_at, last_error,
                            total_bytes, uploaded_bytes, resume_state
                     FROM cloud_jobs WHERE state=?1 ORDER BY id",
                )?;
                stmt.query(rusqlite::params![s.as_str()])?
            }
            None => {
                stmt = self.conn.prepare(
                    "SELECT id, imported_id, destination_id, local_path, remote_path,
                            card_src, state, attempts, next_retry_at, last_error,
                            total_bytes, uploaded_bytes, resume_state
                     FROM cloud_jobs ORDER BY id",
                )?;
                stmt.query([])?
            }
        };

        let mut jobs = Vec::new();
        while let Some(row) = rows.next()? {
            jobs.push(row_to_cloud_job(row)?);
        }
        Ok(jobs)
    }
}

/// Map a `cloud_jobs` row to a [`CloudJob`]. The SELECT column order is fixed
/// (see `list_cloud_jobs` / `claim_due_cloud_jobs`): id, imported_id,
/// destination_id, local_path, remote_path, card_src, state, attempts,
/// next_retry_at, last_error, total_bytes, uploaded_bytes, resume_state.
/// Decodes the `resume_state` JSON and the lowercase `state` word; a malformed
/// `state` is mapped to a `rusqlite::Error` so `?` propagates as `CoreError::Db`.
fn row_to_cloud_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<CloudJob> {
    let state_str: String = row.get(6)?;
    let state = JobState::from_str(&state_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown job state {state_str:?}"),
            )),
        )
    })?;
    let resume_json: Option<String> = row.get(12)?;
    let resume_state = match resume_json {
        Some(s) => Some(serde_json::from_str::<ResumeState>(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?),
        None => None,
    };
    let total_bytes: i64 = row.get(10)?;
    let uploaded_bytes: i64 = row.get(11)?;
    let attempts: i64 = row.get(7)?;
    Ok(CloudJob {
        id: row.get(0)?,
        imported_id: row.get(1)?,
        destination_id: row.get(2)?,
        local_path: row.get(3)?,
        remote_path: row.get(4)?,
        card_src: row.get(5)?,
        state,
        attempts: attempts as u32,
        next_retry_at: row.get(8)?,
        last_error: row.get(9)?,
        total_bytes: total_bytes as u64,
        uploaded_bytes: uploaded_bytes as u64,
        resume_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Ledger { Ledger::open_in_memory().unwrap() }

    fn user_version(l: &Ledger) -> i64 {
        l.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_in_memory_ledger_is_v2() {
        let l = mem();
        assert_eq!(user_version(&l), 2);
    }

    #[test]
    fn fresh_file_ledger_is_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 2);
    }

    #[test]
    fn existing_v1_db_upgrades_to_v2_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");

        // Hand-build the exact M1 (v1) schema with one row, then close it.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE imported (
                     id            INTEGER PRIMARY KEY,
                     camera_serial TEXT NOT NULL,
                     name          TEXT NOT NULL,
                     size          INTEGER NOT NULL,
                     mtime_unix    INTEGER NOT NULL,
                     dest_path     TEXT NOT NULL,
                     hash          TEXT,
                     copied_at     TEXT NOT NULL DEFAULT (datetime('now')),
                     UNIQUE(camera_serial, name, size, mtime_unix)
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO imported (camera_serial, name, size, mtime_unix, dest_path, hash)
                 VALUES ('C346', 'GX010001.MP4', 4096, 1000, '/old', NULL)",
                [],
            )
            .unwrap();
        }

        // Re-open through Ledger -> should migrate to v2 in place.
        let l = Ledger::open(&path).unwrap();
        assert_eq!(user_version(&l), 2);

        // The pre-existing v1 row is still there and still dedup-detectable.
        assert!(l.is_imported("C346", "GX010001.MP4", 4096, 1000).unwrap());

        // The new column exists and defaults to NULL for the legacy row.
        let status: Option<String> = l
            .conn
            .query_row(
                "SELECT cloud_status FROM imported WHERE camera_serial='C346'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, None);

        // The new cloud_jobs table exists.
        let n: i64 = l
            .conn
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='cloud_jobs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn new_file_is_not_a_duplicate_then_is_after_record() {
        let mut l = mem();
        let serial = "C346";
        assert!(!l.is_imported(serial, "GX010001.MP4", 4096, 1000).unwrap());
        l.record(serial, "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", Some("deadbeef")).unwrap();
        assert!(l.is_imported(serial, "GX010001.MP4", 4096, 1000).unwrap());
    }

    #[test]
    fn different_size_or_mtime_is_not_duplicate() {
        let mut l = mem();
        l.record("C346", "GX010001.MP4", 4096, 1000, "/d/a", None).unwrap();
        assert!(!l.is_imported("C346", "GX010001.MP4", 9999, 1000).unwrap()); // size differs
        assert!(!l.is_imported("C346", "GX010001.MP4", 4096, 2000).unwrap()); // mtime differs
        assert!(!l.is_imported("OTHER", "GX010001.MP4", 4096, 1000).unwrap()); // serial differs
    }

    #[test]
    fn record_is_idempotent_on_same_key() {
        let mut l = mem();
        l.record("C346", "GX010001.MP4", 4096, 1000, "/d/a", None).unwrap();
        l.record("C346", "GX010001.MP4", 4096, 1000, "/d/a", Some("hash")).unwrap(); // no error
        assert!(l.is_imported("C346", "GX010001.MP4", 4096, 1000).unwrap());
    }

    #[test]
    fn record_returns_positive_id_and_is_stable_for_same_key() {
        let mut l = mem();
        let id1 = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/d/a", Some("h1"))
            .unwrap();
        assert!(id1 > 0, "expected a positive imported id, got {id1}");

        // INSERT OR REPLACE on the same UNIQUE key must resolve to the same row id.
        let id2 = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/d/a", Some("h2"))
            .unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn imported_id_finds_recorded_row_and_none_otherwise() {
        let mut l = mem();
        assert_eq!(l.imported_id("C346", "GX010001.MP4", 4096, 1000).unwrap(), None);
        let id = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/d/a", None)
            .unwrap();
        assert_eq!(
            l.imported_id("C346", "GX010001.MP4", 4096, 1000).unwrap(),
            Some(id)
        );
    }

    #[test]
    fn set_cloud_status_updates_the_row() {
        let mut l = mem();
        let id = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/d/a", None)
            .unwrap();
        l.set_cloud_status(id, "queued").unwrap();
        let status: Option<String> = l
            .conn
            .query_row(
                "SELECT cloud_status FROM imported WHERE id=?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status.as_deref(), Some("queued"));
    }

    fn enqueue_sample(l: &mut Ledger) -> (i64, i64) {
        let imported_id = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", None)
            .unwrap();
        let job_id = l
            .enqueue_cloud_job(
                imported_id,
                "nc1",
                "/dest/GX010001.MP4",
                "videos/GX010001.MP4",
                4096,
                None,
            )
            .unwrap();
        (imported_id, job_id)
    }

    #[test]
    fn enqueue_then_list_returns_a_queued_job() {
        let mut l = mem();
        let (imported_id, job_id) = enqueue_sample(&mut l);
        assert!(job_id > 0);

        let jobs = l.list_cloud_jobs(Some(JobState::Queued)).unwrap();
        assert_eq!(jobs.len(), 1);
        let j = &jobs[0];
        assert_eq!(j.id, job_id);
        assert_eq!(j.imported_id, imported_id);
        assert_eq!(j.destination_id, "nc1");
        assert_eq!(j.local_path, "/dest/GX010001.MP4");
        assert_eq!(j.remote_path, "videos/GX010001.MP4");
        assert_eq!(j.card_src, None);
        assert_eq!(j.state, JobState::Queued);
        assert_eq!(j.attempts, 0);
        assert_eq!(j.next_retry_at, None);
        assert_eq!(j.last_error, None);
        assert_eq!(j.total_bytes, 4096);
        assert_eq!(j.uploaded_bytes, 0);
        assert_eq!(j.resume_state, None);
    }

    #[test]
    fn enqueue_persists_card_src_when_provided() {
        let mut l = mem();
        let imported_id = l
            .record("C346", "GX010001.MP4", 4096, 1000, "/dest/GX010001.MP4", None)
            .unwrap();
        l.enqueue_cloud_job(
            imported_id,
            "nc1",
            "/dest/GX010001.MP4",
            "videos/GX010001.MP4",
            4096,
            Some("/Volumes/GOPRO/DCIM/100GOPRO/GX010001.MP4"),
        )
        .unwrap();
        let jobs = l.list_cloud_jobs(None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].card_src.as_deref(),
            Some("/Volumes/GOPRO/DCIM/100GOPRO/GX010001.MP4")
        );
    }

    #[test]
    fn pending_cloud_count_counts_queued_jobs() {
        let mut l = mem();
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
        enqueue_sample(&mut l);
        assert_eq!(l.pending_cloud_count().unwrap(), 1);
    }

    #[test]
    fn pending_cloud_count_excludes_terminal_failed() {
        // A Failed job with next_retry_at NULL is terminal and must NOT be
        // counted as pending (Shared Contract C2); otherwise run_until_drained
        // would spin forever on it.
        let mut l = mem();
        let (_, job_id) = enqueue_sample(&mut l);
        l.conn
            .execute(
                "UPDATE cloud_jobs SET state='failed', next_retry_at=NULL WHERE id=?1",
                rusqlite::params![job_id],
            )
            .unwrap();
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
    }

    #[test]
    fn pending_cloud_count_includes_failed_pending_retry() {
        let mut l = mem();
        let (_, job_id) = enqueue_sample(&mut l);
        l.conn
            .execute(
                "UPDATE cloud_jobs SET state='failed', next_retry_at=9999 WHERE id=?1",
                rusqlite::params![job_id],
            )
            .unwrap();
        assert_eq!(l.pending_cloud_count().unwrap(), 1);
    }

    #[test]
    fn pending_cloud_count_excludes_done() {
        let mut l = mem();
        let (_, job_id) = enqueue_sample(&mut l);
        l.conn
            .execute(
                "UPDATE cloud_jobs SET state='done' WHERE id=?1",
                rusqlite::params![job_id],
            )
            .unwrap();
        assert_eq!(l.pending_cloud_count().unwrap(), 0);
    }

    #[test]
    fn list_cloud_jobs_none_filter_returns_all() {
        let mut l = mem();
        enqueue_sample(&mut l);
        assert_eq!(l.list_cloud_jobs(None).unwrap().len(), 1);
        assert_eq!(l.list_cloud_jobs(Some(JobState::Done)).unwrap().len(), 0);
    }

    #[test]
    fn job_state_round_trips_as_lowercase_words() {
        assert_eq!(JobState::Queued.as_str(), "queued");
        assert_eq!(JobState::Uploading.as_str(), "uploading");
        assert_eq!(JobState::Done.as_str(), "done");
        assert_eq!(JobState::Failed.as_str(), "failed");
        assert_eq!(JobState::from_str("queued"), Some(JobState::Queued));
        assert_eq!(JobState::from_str("uploading"), Some(JobState::Uploading));
        assert_eq!(JobState::from_str("done"), Some(JobState::Done));
        assert_eq!(JobState::from_str("failed"), Some(JobState::Failed));
        assert_eq!(JobState::from_str("bogus"), None);
    }
}
