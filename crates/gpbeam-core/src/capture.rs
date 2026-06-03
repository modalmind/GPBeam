use crate::gopro::MediaKind;
use std::path::Path;
use std::time::SystemTime;
use chrono::{DateTime, Local, TimeZone};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured { pub date: String, pub time: String } // "2026-06-01", "143055"

impl Captured {
    /// From a unix timestamp (local time formatting).
    pub fn from_unix(secs: i64) -> Captured {
        let dt: DateTime<Local> = Local.timestamp_opt(secs, 0).single()
            .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().unwrap());
        Captured { date: dt.format("%Y-%m-%d").to_string(), time: dt.format("%H%M%S").to_string() }
    }

    /// Parse EXIF "YYYY:MM:DD HH:MM:SS" (colons in date, no timezone).
    pub fn from_exif(s: &str) -> Option<Captured> {
        let (date, time) = s.split_once(' ')?;
        let dparts: Vec<&str> = date.split(':').collect();
        let tparts: Vec<&str> = time.split(':').collect();
        if dparts.len() != 3 || tparts.len() != 3 { return None; }
        if dparts.iter().chain(&tparts).any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
            return None;
        }
        Some(Captured {
            date: format!("{}-{}-{}", dparts[0], dparts[1], dparts[2]),
            time: format!("{}{}{}", tparts[0], tparts[1], tparts[2]),
        })
    }
}

/// Resolve capture time for a file: EXIF DateTimeOriginal for photos,
/// file mtime for everything else (GoPro video atom times are unreliable).
pub fn resolve_capture(path: &Path, kind: MediaKind, mtime: SystemTime) -> Captured {
    if kind.is_photo() {
        if let Some(c) = read_exif_datetime(path).and_then(|s| Captured::from_exif(&s)) {
            return c;
        }
    }
    let secs = mtime.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Captured::from_unix(secs)
}

fn read_exif_datetime(path: &Path) -> Option<String> {
    use exif::{In, Reader, Tag};
    let file = std::fs::File::open(path).ok()?;
    let exif = Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
    let field = exif.get_field(Tag::DateTimeOriginal, In::PRIMARY)?;
    Some(field.display_value().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_known_unix_time() {
        // 2021-01-01T00:00:00Z = 1609459200 ; assert via from_unix path
        let c = Captured::from_unix(1_609_459_200);
        assert_eq!(c.date.len(), 10);   // YYYY-MM-DD
        assert_eq!(c.time.len(), 6);    // HHMMSS
    }

    #[test]
    fn parses_exif_datetime_string() {
        let c = Captured::from_exif("2026:06:01 14:30:55").unwrap();
        assert_eq!(c.date, "2026-06-01");
        assert_eq!(c.time, "143055");
    }

    #[test]
    fn rejects_malformed_exif() {
        assert!(Captured::from_exif("not a date").is_none());
    }
}
