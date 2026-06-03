use std::path::{Path, PathBuf};
use serde::Deserialize;

/// True for folder names like 100GOPRO, 101GOPRO ... 999GOPRO.
pub(crate) fn is_gopro_media_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 8
        && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2].is_ascii_digit()
        && &name[3..] == "GOPRO"
}

/// A removable volume is treated as a GoPro card iff `<root>/DCIM/<NNN>GOPRO/`
/// exists. We deliberately do NOT require MISC/version.txt (absent on freshly
/// formatted cards). This is format-agnostic and works for future models.
pub fn is_gopro_card(vol_root: &Path) -> bool {
    let dcim = vol_root.join("DCIM");
    let Ok(rd) = std::fs::read_dir(&dcim) else { return false };
    rd.filter_map(|e| e.ok()).any(|e| {
        e.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && e.file_name().to_str().map(is_gopro_media_folder).unwrap_or(false)
    })
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GoProVersion {
    #[serde(rename = "info version", default)]         pub info_version: String,
    #[serde(rename = "firmware version", default)]     pub firmware_version: String,
    #[serde(rename = "wifi mac", default)]             pub wifi_mac: String,
    #[serde(rename = "camera type", default)]          pub camera_type: String,
    #[serde(rename = "camera serial number", default)] pub camera_serial_number: String,
}

/// GoPro writes invalid JSON: a trailing comma before `}` and (HERO10/11+)
/// embedded literal newlines. Strip both before parsing.
fn sanitize_version_txt(raw: &str) -> String {
    let s = raw.replace(['\n', '\r'], "");
    if let Some(pos) = s.rfind('}') {
        let (head, tail) = s.split_at(pos);
        let head = head.trim_end().strip_suffix(',').unwrap_or(head.trim_end());
        format!("{head}{tail}")
    } else {
        s
    }
}

/// Read & parse `<root>/MISC/version.txt`. None if absent or unparseable.
pub fn read_version(vol_root: &Path) -> Option<GoProVersion> {
    let p: PathBuf = vol_root.join("MISC").join("version.txt");
    let raw = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&sanitize_version_txt(&raw)).ok()
}

/// Firmware-version prefix -> model family. None for unknown/future models so
/// callers fall through to extension-based classification.
pub fn model_family(fw_version: &str) -> Option<&'static str> {
    let prefix = fw_version.split('.').next().unwrap_or("");
    Some(match prefix {
        "HD2" | "HD3" => "HERO3", "HD4" => "HERO4", "HX" => "HERO Session",
        "HD5" => "HERO5", "HD6" => "HERO6", "HD7" => "HERO7", "HD8" => "HERO8",
        "HD9" => "HERO9", "H19" => "MAX", "H21" => "HERO10", "H22" => "HERO11",
        "H23" => "HERO12",
        "H24" => return match fw_version.split('.').nth(1) {
            Some("01") => Some("HERO13"),
            Some("02") => Some("MAX2"),
            Some("03") => Some("HERO (2024)"),
            _ => Some("HERO13/MAX2 family"),
        },
        "H25" => "LIT HERO",
        "H26" => return match fw_version.split('.').nth(1) {
            Some("01") => Some("MISSION 1 PRO"),
            Some("02") => Some("MISSION 1"),
            _ => Some("MISSION 1 family"),
        },
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video, Video360, Photo, RawPhoto, Burst, LowResProxy, Thumbnail, Audio, Unknown,
}

impl MediaKind {
    pub fn is_proxy(self) -> bool { matches!(self, MediaKind::LowResProxy) }
    pub fn is_thumbnail(self) -> bool { matches!(self, MediaKind::Thumbnail) }
    pub fn is_photo(self) -> bool { matches!(self, MediaKind::Photo | MediaKind::RawPhoto | MediaKind::Burst) }
}

/// Classify by name. Tier 1: known prefix+ext. Tier 2: extension-only fallback
/// so media from an unknown/future camera is never dropped. `name` = file name only.
pub fn classify(name: &str) -> MediaKind {
    use MediaKind::*;
    let upper = name.to_ascii_uppercase();
    let (stem, ext) = match upper.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => return Unknown,
    };
    // Tier 1
    match (stem, ext) {
        (s, "MP4") if s.starts_with("GH") || s.starts_with("GX") || s.starts_with("GG") => return Video,
        (s, "MP4") if s.starts_with("GOPR") || s.starts_with("GP") => return Video,
        (s, "JPG") | (s, "JPEG") if s.starts_with("GOPR") || s.starts_with("GS") => return Photo,
        (s, "360") if s.starts_with("GS") => return Video360,
        (s, "JPG") if s.len() > 1 && s.starts_with('G') && s.as_bytes()[1].is_ascii_digit() => return Burst,
        _ => {}
    }
    // Tier 2: extension-only fallback
    match ext {
        "LRV" => LowResProxy,
        "THM" => Thumbnail,
        "GPR" => RawPhoto,
        "WAV" => Audio,
        "360" => Video360,
        "MP4" => Video,
        "JPG" | "JPEG" => Photo,
        _ => Unknown,
    }
}

/// Recording-group key (file_number, chapter) so chapters+sidecars copy together.
/// HERO/360 scheme: PREFIX + 2 chapter digits + 4 file-number digits.
pub fn hero_group_key(stem: &str) -> Option<(u32, u32)> {
    let digits: String = stem.chars().skip_while(|c| !c.is_ascii_digit()).collect();
    if digits.len() == 6 {
        let chapter = digits[0..2].parse().ok()?;
        let file_no = digits[2..6].parse().ok()?;
        Some((file_no, chapter))
    } else {
        None
    }
}

#[cfg(test)]
#[path = "../tests/fixtures.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use super::fixtures;

    #[test]
    fn detects_gopro_card_by_dcim_folder() {
        let c = fixtures::hero11_card();
        assert!(is_gopro_card(c.root()));
    }

    #[test]
    fn rejects_non_gopro_volume() {
        let c = fixtures::not_a_gopro();
        assert!(!is_gopro_card(c.root()));
    }

    #[test]
    fn reads_hero11_version_with_quirks() {
        let c = fixtures::hero11_card();
        let v = read_version(c.root()).expect("version parsed");
        assert_eq!(v.firmware_version, "H22.01.02.32.00");
        assert_eq!(v.camera_serial_number, "C3461324500001");
        assert_eq!(v.camera_type, "HERO11 Black");
    }

    #[test]
    fn absent_version_is_none() {
        let c = fixtures::not_a_gopro();
        assert!(read_version(c.root()).is_none());
    }

    #[test]
    fn maps_firmware_prefix_to_model() {
        assert_eq!(model_family("H22.01.02.32.00"), Some("HERO11"));
        assert_eq!(model_family("H24.02.00.00.00"), Some("MAX2"));
        assert_eq!(model_family("H26.01.00.00.00"), Some("MISSION 1 PRO"));
        assert_eq!(model_family("ZZ99.00"), None); // unknown -> fall through
    }

    #[test]
    fn classifies_known_and_fallback() {
        use MediaKind::*;
        assert_eq!(classify("GX010001.MP4"), Video);
        assert_eq!(classify("GH010001.MP4"), Video);
        assert_eq!(classify("GS010003.360"), Video360);
        assert_eq!(classify("GOPR0002.JPG"), Photo);
        assert_eq!(classify("GX010001.LRV"), LowResProxy);
        assert_eq!(classify("GX010001.THM"), Thumbnail);
        assert_eq!(classify("GOPR0002.GPR"), RawPhoto);
        // unknown future camera, unknown prefix, known media extension -> still copied
        assert_eq!(classify("ZZ999999.MP4"), Video);
        assert_eq!(classify("WHATEVER.XYZ"), Unknown);
    }

    #[test]
    fn groups_chapters_by_file_number() {
        // GX010001 and GX020001 are chapters 01,02 of file number 0001
        assert_eq!(hero_group_key("GX010001"), Some((1, 1)));
        assert_eq!(hero_group_key("GX020001"), Some((1, 2)));
        assert_eq!(hero_group_key("NOPE"), None);
    }
}
