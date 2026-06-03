use crate::error::{io_at, CoreError, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone)]
pub struct CopyOutcome { pub dest: PathBuf, pub bytes: u64, pub hash: Option<String> }

/// Copy `src` to `final_path` atomically. The temp file is created on the
/// destination volume so `persist()` is an intra-fs rename (never EXDEV).
/// When `verify` is true, the BLAKE3 hash is computed inline during the copy
/// read-pass, the persisted file is re-hashed (read-back), and the two are
/// compared; on mismatch the destination is removed and VerifyFailed returned.
/// `progress(copied)` is called with cumulative bytes copied.
pub fn copy_verified(
    src: &Path,
    final_path: &Path,
    verify: bool,
    progress: &mut dyn FnMut(u64),
) -> Result<CopyOutcome> {
    let dest_dir = final_path.parent().expect("final_path needs a parent");
    let mut reader = std::fs::File::open(src).map_err(io_at(src))?;
    let mut tmp = NamedTempFile::new_in(dest_dir).map_err(io_at(dest_dir))?;

    let mut hasher = if verify { Some(blake3::Hasher::new()) } else { None };
    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(io_at(src))?;
        if n == 0 { break; }
        if let Some(h) = hasher.as_mut() { h.update(&buf[..n]); }
        tmp.as_file_mut().write_all(&buf[..n]).map_err(io_at(dest_dir))?;
        copied += n as u64;
        progress(copied);
    }
    tmp.as_file_mut().flush().map_err(io_at(dest_dir))?;
    tmp.as_file().sync_all().map_err(io_at(dest_dir))?;

    let src_hash = hasher.map(|h| h.finalize().to_hex().to_string());

    // Atomically place the file (NamedTempFile is dropped/removed on early return above).
    let persisted = tmp.persist(final_path)
        .map_err(|e| CoreError::Io { path: final_path.to_path_buf(), source: e.error })?;
    persisted.sync_all().map_err(io_at(final_path))?;

    if let Some(ref expected) = src_hash {
        // Re-hash the quiescent destination (local disk read, fast).
        let mut dh = blake3::Hasher::new();
        dh.update_mmap_rayon(final_path).map_err(io_at(final_path))?;
        let got = dh.finalize().to_hex().to_string();
        if &got != expected {
            let _ = std::fs::remove_file(final_path);
            return Err(CoreError::VerifyFailed(final_path.to_path_buf()));
        }
    }

    Ok(CopyOutcome { dest: final_path.to_path_buf(), bytes: copied, hash: src_hash })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn copies_bytes_and_verifies() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("GX010001.MP4");
        fs::write(&src, b"hello gopro footage").unwrap();
        let dest = dst_dir.path().join("out.MP4");

        let mut seen = 0u64;
        let out = copy_verified(&src, &dest, true, &mut |c| seen = c).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"hello gopro footage");
        assert_eq!(out.bytes, 19);
        assert!(out.hash.is_some());
        assert_eq!(seen, 19); // progress reached total
    }

    #[test]
    fn no_partial_file_left_when_source_missing() {
        let dst_dir = TempDir::new().unwrap();
        let dest = dst_dir.path().join("out.MP4");
        let missing = dst_dir.path().join("does-not-exist");
        let err = copy_verified(&missing, &dest, true, &mut |_| {});
        assert!(err.is_err());
        assert!(!dest.exists()); // temp discarded, no half-file under real name
        // no stray temp files remain in dst_dir
        let leftovers: Vec<_> = fs::read_dir(dst_dir.path()).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn verify_disabled_skips_hash() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("a.MP4");
        fs::write(&src, b"data").unwrap();
        let out = copy_verified(&src, &dst_dir.path().join("a.MP4"), false, &mut |_| {}).unwrap();
        assert!(out.hash.is_none());
    }
}
