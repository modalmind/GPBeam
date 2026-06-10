use crate::capture::Captured;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Render a destination file name from a template. Tokens:
/// {date} {time} {original} {ext} {camera}=serial {model}.
///
/// The result is sanitized to a SINGLE path component: `{camera}`/`{model}`
/// come from the card's `MISC/version.txt` (attacker-controllable), so a
/// serial like `../../../tmp/evil` would otherwise escape `dest_root` when the
/// scanner joins the rendered name. Sanitizing here covers every caller.
pub fn render_name(
    template: &str,
    original: &str,
    cap: &Captured,
    serial: Option<&str>,
    model: Option<&str>,
) -> String {
    let ext = original.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let rendered = template
        .replace("{date}", &cap.date)
        .replace("{time}", &cap.time)
        .replace("{original}", original)
        .replace("{ext}", ext)
        .replace("{camera}", serial.unwrap_or("unknown"))
        .replace("{model}", model.unwrap_or("GoPro"));
    sanitize_file_name(&rendered)
}

/// Force `name` to be one safe path component: path separators (`/`, `\`) and
/// NUL become `_` (so `../../x` collapses to the harmless `.._.._x`), and a
/// result that is itself empty, `.` or `..` is replaced with `_` so the
/// subsequent `dest_root.join(name)` can never resolve outside `dest_root`.
fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => cleaned,
    }
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

    fn cap() -> Captured {
        Captured {
            date: "2026-06-01".into(),
            time: "143055".into(),
        }
    }

    #[test]
    fn renders_default_template() {
        let n = render_name(
            "{date}_{original}",
            "GX010001.MP4",
            &cap(),
            Some("C346"),
            Some("HERO11"),
        );
        assert_eq!(n, "2026-06-01_GX010001.MP4");
    }

    #[test]
    fn renders_all_tokens() {
        let n = render_name(
            "{date}_{time}_{model}_{camera}_{original}",
            "GX010001.MP4",
            &cap(),
            Some("C346"),
            Some("HERO11"),
        );
        assert_eq!(n, "2026-06-01_143055_HERO11_C346_GX010001.MP4");
    }

    #[test]
    fn traversal_camera_serial_is_neutralized() {
        // {camera} comes from the card's MISC/version.txt — attacker data. A
        // separator-laden serial must not let the joined path escape dest_root.
        let n = render_name(
            "{camera}_{original}",
            "GX010001.MP4",
            &cap(),
            Some("../../../tmp/evil"),
            Some("HERO11"),
        );
        assert!(!n.contains('/'), "no path separators survive: {n:?}");
        assert!(!n.contains('\\'));
        let joined = Path::new("/dest").join(&n);
        assert!(joined.starts_with("/dest"));
        assert!(
            !joined
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "no `..` component survives: {joined:?}"
        );
    }

    #[test]
    fn traversal_model_with_backslashes_is_neutralized() {
        let n = render_name(
            "{model}_{original}",
            "a.MP4",
            &cap(),
            None,
            Some("..\\..\\evil"),
        );
        assert!(
            !n.contains('\\'),
            "windows-style separators neutralized: {n:?}"
        );
        assert!(!n.contains('/'));
    }

    #[test]
    fn rendered_name_that_is_a_dot_component_is_replaced() {
        // A template that renders to exactly "." / ".." / "" must not be
        // joinable as a directory traversal (or a no-op path).
        assert_eq!(render_name("{camera}", "x", &cap(), Some(".."), None), "_");
        assert_eq!(render_name("{camera}", "x", &cap(), Some("."), None), "_");
        assert_eq!(render_name("", "x", &cap(), None, None), "_");
    }

    #[test]
    fn normal_names_are_unchanged_by_sanitization() {
        let n = render_name(
            "{date}_{time}_{model}_{camera}_{original}",
            "GX010001.MP4",
            &cap(),
            Some("C3461324500001"),
            Some("HERO11"),
        );
        assert_eq!(n, "2026-06-01_143055_HERO11_C3461324500001_GX010001.MP4");
    }

    #[test]
    fn collision_appends_suffix_when_target_exists() {
        let d = TempDir::new().unwrap();
        let target = d.path().join("2026-06-01_GX010001.MP4");
        fs::write(&target, b"existing").unwrap();
        let resolved = resolve_collision(d.path(), "2026-06-01_GX010001.MP4", &HashSet::new());
        assert_eq!(
            resolved.file_name().unwrap().to_str().unwrap(),
            "2026-06-01_GX010001_1.MP4"
        );
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
        assert_eq!(
            resolved.file_name().unwrap().to_str().unwrap(),
            "GX010001_1.MP4"
        );
    }
}
