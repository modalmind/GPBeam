use std::path::Path;

/// True for folder names like 100GOPRO, 101GOPRO ... 999GOPRO.
fn is_gopro_media_folder(name: &str) -> bool {
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
}
