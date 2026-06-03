//! Nextcloud WebDAV uploader. Built incrementally across Phase 2.

/// Percent-encode each path segment, preserving the `/` separators. Encodes
/// spaces, `#`, `?`, `+`, and other reserved/unsafe bytes per segment.
pub fn encode_path_segments(rel: &str) -> String {
    rel.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// RFC 3986 unreserved set is kept verbatim; everything else is %XX-encoded.
fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// `<base>/remote.php/dav/files/<user>/<encoded rel>`.
pub fn files_url(base_url: &str, username: &str, remote_rel: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let enc = encode_path_segments(remote_rel);
    format!("{base}/remote.php/dav/files/{username}/{enc}")
}

/// `<base>/remote.php/dav/uploads/<user>/<upload_id>[/<part>]`.
pub fn uploads_url(base_url: &str, username: &str, upload_id: &str, part: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    match part {
        Some(p) => format!("{base}/remote.php/dav/uploads/{username}/{upload_id}/{p}"),
        None => format!("{base}/remote.php/dav/uploads/{username}/{upload_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spaces_and_hash_per_segment_keeps_slash() {
        assert_eq!(
            encode_path_segments("GoPro Clips/my #1 video.mp4"),
            "GoPro%20Clips/my%20%231%20video.mp4"
        );
    }

    #[test]
    fn keeps_unreserved_bytes_verbatim() {
        assert_eq!(encode_path_segments("a-b_c.d~e/f"), "a-b_c.d~e/f");
    }

    #[test]
    fn encodes_plus_and_question_mark() {
        assert_eq!(encode_path_segments("a+b?c"), "a%2Bb%3Fc");
    }

    #[test]
    fn files_url_shape_matches_contract() {
        assert_eq!(
            files_url("https://cloud.example.com", "alice", "GoPro/clip 1.mp4"),
            "https://cloud.example.com/remote.php/dav/files/alice/GoPro/clip%201.mp4"
        );
    }

    #[test]
    fn files_url_trims_trailing_slash_on_base() {
        assert_eq!(
            files_url("https://cloud.example.com/", "alice", "x.mp4"),
            "https://cloud.example.com/remote.php/dav/files/alice/x.mp4"
        );
    }

    #[test]
    fn uploads_url_dir_and_part_shapes() {
        assert_eq!(
            uploads_url("https://c.example.com", "bob", "gpbeam-123", None),
            "https://c.example.com/remote.php/dav/uploads/bob/gpbeam-123"
        );
        assert_eq!(
            uploads_url("https://c.example.com", "bob", "gpbeam-123", Some("00001")),
            "https://c.example.com/remote.php/dav/uploads/bob/gpbeam-123/00001"
        );
    }
}
