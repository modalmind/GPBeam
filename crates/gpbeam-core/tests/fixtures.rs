//! Shared test helper: builds synthetic GoPro card directory trees in a tempdir.
//! Included by other test files via `#[path = "fixtures.rs"] mod fixtures;`.
#![allow(dead_code)]
use std::fs;
use std::path::Path;
use tempfile::TempDir;

pub struct Card { pub dir: TempDir }

impl Card {
    pub fn root(&self) -> &Path { self.dir.path() }

    /// Write a file of `size` bytes (deterministic content from a seed byte).
    fn write_file(&self, rel: &str, size: usize, seed: u8) {
        let p = self.root().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let data: Vec<u8> = (0..size).map(|i| seed.wrapping_add(i as u8)).collect();
        fs::write(p, data).unwrap();
    }
}

/// A HERO11 (firmware H22) card with: two chapters of one HEVC clip, a photo,
/// a 360 clip, and proxy+thumbnail sidecars. version.txt has the real GoPro
/// quirks (trailing comma before brace + an embedded newline).
pub fn hero11_card() -> Card {
    let card = Card { dir: TempDir::new().unwrap() };
    card.write_file("DCIM/100GOPRO/GX010001.MP4", 4096, 1);
    card.write_file("DCIM/100GOPRO/GX020001.MP4", 2048, 2);
    card.write_file("DCIM/100GOPRO/GOPR0002.JPG", 1024, 3);
    card.write_file("DCIM/100GOPRO/GS010003.360", 8192, 4);
    card.write_file("DCIM/100GOPRO/GX010001.LRV", 512, 5);   // proxy
    card.write_file("DCIM/100GOPRO/GX010001.THM", 128, 6);   // thumbnail
    // version.txt: invalid JSON exactly like GoPro writes it.
    let version = "{\n\"info version\":\"2.0\",\n\"firmware version\":\"H22.01.02.32.00\",\n\"wifi mac\":\"aabbccddeeff\",\n\"camera type\":\"HERO11 Black\",\n\"camera serial number\":\"C3461324500001\",\n}";
    let misc = card.root().join("MISC");
    fs::create_dir_all(&misc).unwrap();
    fs::write(misc.join("version.txt"), version).unwrap();
    card
}

/// A minimal HERO11 card with a single MP4 clip of `mp4_bytes` bytes — used to
/// exercise multi-chunk streaming progress (copy_verified reads in 1 MiB chunks).
pub fn card_with_one_clip(mp4_bytes: usize) -> Card {
    let card = Card { dir: TempDir::new().unwrap() };
    card.write_file("DCIM/100GOPRO/GX010001.MP4", mp4_bytes, 1);
    let version = "{\n\"info version\":\"2.0\",\n\"firmware version\":\"H22.01.02.32.00\",\n\"wifi mac\":\"aabbccddeeff\",\n\"camera type\":\"HERO11 Black\",\n\"camera serial number\":\"C3461324500001\",\n}";
    let misc = card.root().join("MISC");
    fs::create_dir_all(&misc).unwrap();
    fs::write(misc.join("version.txt"), version).unwrap();
    card
}

/// A non-GoPro removable volume (no DCIM/NNNGOPRO).
pub fn not_a_gopro() -> Card {
    let card = Card { dir: TempDir::new().unwrap() };
    card.write_file("DCIM/123ANDRO/IMG_0001.JPG", 256, 9);
    card.write_file("readme.txt", 16, 9);
    card
}

/// Helper: a fresh empty destination directory.
pub fn dest() -> TempDir { TempDir::new().unwrap() }

#[test]
fn fixture_hero11_has_expected_layout() {
    let c = hero11_card();
    assert!(c.root().join("DCIM/100GOPRO/GX010001.MP4").exists());
    assert!(c.root().join("MISC/version.txt").exists());
    let v = std::fs::read_to_string(c.root().join("MISC/version.txt")).unwrap();
    assert!(v.contains("H22.01.02.32.00"));
    // GoPro's invalid-JSON quirks: a trailing comma before the closing brace
    // (separated by a newline) AND embedded newlines. Mirror sanitize_version_txt:
    // after stripping newlines, the comma sits directly before the brace.
    assert!(v.contains('\n'), "embedded-newline quirk present");
    assert!(v.replace(['\n', '\r'], "").ends_with(",}"), "trailing-comma quirk present");
}
