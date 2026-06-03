use crate::capture::Captured;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Render a destination file name from a template. Tokens:
/// {date} {time} {original} {ext} {camera}=serial {model}.
pub fn render_name(template: &str, original: &str, cap: &Captured,
                   serial: Option<&str>, model: Option<&str>) -> String {
    let ext = original.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    template
        .replace("{date}", &cap.date)
        .replace("{time}", &cap.time)
        .replace("{original}", original)
        .replace("{ext}", ext)
        .replace("{camera}", serial.unwrap_or("unknown"))
        .replace("{model}", model.unwrap_or("GoPro"))
}

/// Given a destination dir, a desired file name, and the set of paths already
/// reserved earlier in this run, return a free path: the name itself if unused
/// on disk AND unreserved, else `<stem>_<n>.<ext>` for the first free n.
pub fn resolve_collision(dest_dir: &Path, name: &str, reserved: &HashSet<PathBuf>) -> PathBuf {
    let candidate = dest_dir.join(name);
    if !candidate.exists() && !reserved.contains(&candidate) {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    for n in 1u32.. {
        let alt = dest_dir.join(format!("{stem}_{n}{ext}"));
        if !alt.exists() && !reserved.contains(&alt) {
            return alt;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    fn cap() -> Captured { Captured { date: "2026-06-01".into(), time: "143055".into() } }

    #[test]
    fn renders_default_template() {
        let n = render_name("{date}_{original}", "GX010001.MP4", &cap(), Some("C346"), Some("HERO11"));
        assert_eq!(n, "2026-06-01_GX010001.MP4");
    }

    #[test]
    fn renders_all_tokens() {
        let n = render_name("{date}_{time}_{model}_{camera}_{original}", "GX010001.MP4", &cap(),
                            Some("C346"), Some("HERO11"));
        assert_eq!(n, "2026-06-01_143055_HERO11_C346_GX010001.MP4");
    }

    #[test]
    fn collision_appends_suffix_when_target_exists() {
        let d = TempDir::new().unwrap();
        let target = d.path().join("2026-06-01_GX010001.MP4");
        fs::write(&target, b"existing").unwrap();
        let resolved = resolve_collision(d.path(), "2026-06-01_GX010001.MP4", &HashSet::new());
        assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), "2026-06-01_GX010001_1.MP4");
    }

    #[test]
    fn collision_returns_name_when_free() {
        let d = TempDir::new().unwrap();
        let resolved = resolve_collision(d.path(), "GX010001.MP4", &HashSet::new());
        assert_eq!(resolved, d.path().join("GX010001.MP4"));
    }

    #[test]
    fn collision_respects_reserved_paths() {
        let d = TempDir::new().unwrap();
        let mut reserved = HashSet::new();
        reserved.insert(d.path().join("GX010001.MP4")); // planned earlier this run, not yet on disk
        let resolved = resolve_collision(d.path(), "GX010001.MP4", &reserved);
        assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), "GX010001_1.MP4");
    }
}
