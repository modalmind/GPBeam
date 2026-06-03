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
