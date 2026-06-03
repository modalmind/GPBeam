use crate::error::{io_at, Result};
use std::path::Path;

/// True if `dest_dir`'s volume has room for `needed` bytes plus `headroom`.
pub fn has_room(dest_dir: &Path, needed: u64, headroom: u64) -> Result<bool> {
    let avail = fs4::available_space(dest_dir).map_err(io_at(dest_dir))?;
    Ok(avail >= needed.saturating_add(headroom))
}

/// Available bytes on the volume backing `dest_dir`.
pub fn available(dest_dir: &Path) -> Result<u64> {
    fs4::available_space(dest_dir).map_err(io_at(dest_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn zero_need_always_fits() {
        let d = TempDir::new().unwrap();
        assert!(has_room(d.path(), 0, 0).unwrap());
    }

    #[test]
    fn absurd_need_does_not_fit() {
        let d = TempDir::new().unwrap();
        // u64::MAX bytes plus headroom can never fit on any real volume.
        assert!(!has_room(d.path(), u64::MAX - 1, 1).unwrap());
    }
}
