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

    #[test]
    fn zero_byte_file_copies_and_verifies() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("empty.MP4");
        fs::write(&src, b"").unwrap();
        let dest = dst_dir.path().join("empty.MP4");
        let out = copy_verified(&src, &dest, true, &mut |_| {}).unwrap();
        assert_eq!(out.bytes, 0);
        assert!(out.hash.is_some(), "zero-byte file should still produce a hash");
        assert!(dest.exists());
        assert_eq!(fs::read(&dest).unwrap().len(), 0);
    }

    #[test]
    fn recopy_overwrites_existing_dest() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("a.MP4");
        fs::write(&src, b"NEW CONTENT").unwrap();
        let dest = dst_dir.path().join("a.MP4");
        fs::write(&dest, b"OLD STALE GARBAGE THAT IS LONGER").unwrap();
        let out = copy_verified(&src, &dest, true, &mut |_| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"NEW CONTENT");
        assert_eq!(out.bytes, 11);
        assert_eq!(fs::read_dir(dst_dir.path()).unwrap().count(), 1); // no temp leftover
    }

    #[cfg(unix)]
    #[test]
    fn write_error_leaves_no_partial_file() {
        use std::os::unix::fs::PermissionsExt;
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("a.MP4");
        fs::write(&src, vec![0u8; 4096]).unwrap();
        let final_path = dst_dir.path().join("a.MP4");
        let mut perms = fs::metadata(dst_dir.path()).unwrap().permissions();
        perms.set_mode(0o500); // read+execute, no write
        fs::set_permissions(dst_dir.path(), perms).unwrap();
        let res = copy_verified(&src, &final_path, true, &mut |_| {});
        let mut perms = fs::metadata(dst_dir.path()).unwrap().permissions();
        perms.set_mode(0o700); // restore so TempDir can clean up
        fs::set_permissions(dst_dir.path(), perms).unwrap();
        assert!(res.is_err(), "must fail when dest dir is not writable");
        assert!(!final_path.exists(), "no partial file under the real name");
    }
}
