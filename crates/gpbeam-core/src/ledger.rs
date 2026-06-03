use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;

pub struct Ledger {
    conn: Connection,
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

    pub fn record(&mut self, serial: &str, name: &str, size: u64, mtime_unix: i64,
                  dest_path: &str, hash: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO imported
             (camera_serial, name, size, mtime_unix, dest_path, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![serial, name, size as i64, mtime_unix, dest_path, hash],
        )?;
        Ok(())
    }
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
}
