//! Reading Python source from disk, robust to encoding (issue #53).
//!
//! `std::fs::read_to_string` assumes UTF-8 and turns *any* non-UTF-8 byte
//! into a fatal `io::Error`. In a check/fix loop that aborted the whole run
//! (exit 2) and — worse — masked real violations in every other file behind
//! one stray file (a binary fixture, vendored data, a legacy-encoded module).
//!
//! Instead this module:
//!
//! 1. honours a UTF-8 BOM and a [PEP 263] `# -*- coding: <enc> -*-`
//!    declaration in the first two lines, so legacy-encoded but perfectly
//!    valid Python (e.g. `latin-1`) is decoded rather than rejected; and
//! 2. classifies a file that still cannot be decoded as *skippable*
//!    ([`Source::Undecodable`]) rather than fatal, so the caller can warn and
//!    move on while the rest of the run still reports genuine violations.
//!
//! Only the encodings that cover the overwhelming majority of real source —
//! `utf-8`, `latin-1`/`iso-8859-1`, and `ascii` — are decoded directly (no
//! third-party codec dependency). Any other *declared* encoding degrades to
//! the same graceful skip: still robust (no crash, no masking), just not
//! analysed. A genuine filesystem error (missing file, permission denied) is
//! still surfaced — that is a real error, not a stray file.
//!
//! [PEP 263]: https://peps.python.org/pep-0263/

use std::path::Path;

/// The UTF-8 byte-order mark. PEP 263: its presence forces UTF-8 and it is
/// stripped from the decoded text.
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Outcome of reading a Python source file.
#[derive(Debug)]
pub enum Source {
    /// Successfully decoded source text.
    Decoded(String),
    /// The bytes are not decodable as Python source (binary file, non-UTF-8
    /// with no usable PEP 263 declaration, or a declared encoding this tool
    /// does not implement). The caller skips the file with a warning instead
    /// of aborting the run. The string is a human-readable reason.
    Undecodable(String),
}

/// The handful of codecs decoded without a third-party dependency.
enum Codec {
    /// UTF-8 (the Python 3 default).
    Utf8,
    /// ISO-8859-1 / latin-1: every byte maps 1:1 to U+0000..=U+00FF.
    Latin1,
    /// 7-bit US-ASCII.
    Ascii,
}

/// Map a PEP 263 codec name (case- and separator-insensitive) to a codec we
/// can decode, or `None` for one we deliberately do not implement.
fn codec_for(name: &str) -> Option<Codec> {
    let normalized = name.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    match normalized.as_str() {
        "utf-8" | "utf8" | "utf" | "u8" | "cp65001" => Some(Codec::Utf8),
        "latin-1" | "latin1" | "latin" | "iso-8859-1" | "iso8859-1" | "8859" | "cp819" | "l1" => {
            Some(Codec::Latin1)
        }
        "ascii" | "us-ascii" | "646" => Some(Codec::Ascii),
        _ => None,
    }
}

/// PEP 263 magic-comment scan of a single physical line: optional leading
/// whitespace, `#`, then `coding` immediately followed by `:` or `=`,
/// optional spaces/tabs, then the encoding name (`[-\w.]+`).
fn find_coding(line: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < line.len() && matches!(line[i], b' ' | b'\t' | b'\x0c') {
        i += 1;
    }
    if line.get(i) != Some(&b'#') {
        return None;
    }
    let rest = &line[i + 1..];
    let needle = b"coding";
    let mut j = 0;
    while j + needle.len() <= rest.len() {
        if &rest[j..j + needle.len()] == needle {
            let mut k = j + needle.len();
            if matches!(rest.get(k), Some(b':' | b'=')) {
                k += 1;
                while k < rest.len() && matches!(rest[k], b' ' | b'\t') {
                    k += 1;
                }
                let start = k;
                while k < rest.len()
                    && (rest[k].is_ascii_alphanumeric() || matches!(rest[k], b'-' | b'_' | b'.'))
                {
                    k += 1;
                }
                if k > start {
                    return Some(String::from_utf8_lossy(&rest[start..k]).into_owned());
                }
                return None;
            }
        }
        j += 1;
    }
    None
}

/// Search the first two physical lines for a PEP 263 coding declaration.
fn sniff_coding(bytes: &[u8]) -> Option<String> {
    let mut start = 0;
    for _ in 0..2 {
        if start >= bytes.len() {
            break;
        }
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| start + p);
        if let Some(name) = find_coding(&bytes[start..end]) {
            return Some(name);
        }
        start = end + 1;
    }
    None
}

/// Decode raw file bytes into Python source text, honouring a UTF-8 BOM and a
/// PEP 263 coding declaration. Never panics; an undecodable file becomes
/// [`Source::Undecodable`] with a reason rather than an error.
pub fn decode_python_source(bytes: &[u8]) -> Source {
    if let Some(rest) = bytes.strip_prefix(UTF8_BOM) {
        return match std::str::from_utf8(rest) {
            Ok(text) => Source::Decoded(text.to_owned()),
            Err(_) => Source::Undecodable("has a UTF-8 BOM but is not valid UTF-8".to_owned()),
        };
    }
    let declared = sniff_coding(bytes);
    let codec = match &declared {
        None => Codec::Utf8,
        Some(name) => match codec_for(name) {
            Some(codec) => codec,
            None => {
                return Source::Undecodable(format!(
                    "unsupported PEP 263 encoding declaration `{name}`"
                ))
            }
        },
    };
    match codec {
        Codec::Utf8 => match std::str::from_utf8(bytes) {
            Ok(text) => Source::Decoded(text.to_owned()),
            Err(_) => Source::Undecodable(if declared.is_some() {
                "declared as utf-8 but is not valid UTF-8".to_owned()
            } else {
                "is not valid UTF-8 and has no PEP 263 encoding declaration".to_owned()
            }),
        },
        Codec::Latin1 => Source::Decoded(bytes.iter().copied().map(char::from).collect()),
        Codec::Ascii => {
            if bytes.is_ascii() {
                Source::Decoded(bytes.iter().copied().map(char::from).collect())
            } else {
                Source::Undecodable("declared as ascii but contains non-ASCII bytes".to_owned())
            }
        }
    }
}

/// Read a Python source file, honouring a BOM / PEP 263 encoding declaration.
///
/// `Ok(Source::Undecodable)` means the bytes are not decodable as Python
/// source; the caller warns and skips the file (issue #53) rather than
/// aborting the run.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] for a genuine filesystem failure
/// (file not found, permission denied) — a real error, distinct from a stray
/// non-UTF-8 file, and still fatal.
pub fn read_python_source(path: &Path) -> std::io::Result<Source> {
    Ok(decode_python_source(&std::fs::read(path)?))
}

/// Like [`read_python_source`] but collapsing every failure (filesystem error
/// *or* undecodable) to `None`. Used by lazy module resolution, which already
/// fails closed: a dependency it cannot read simply does not resolve.
pub fn read_python_source_lossy(path: &Path) -> Option<String> {
    match read_python_source(path) {
        Ok(Source::Decoded(text)) => Some(text),
        Ok(Source::Undecodable(_)) | Err(_) => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn decoded(bytes: &[u8]) -> String {
        match decode_python_source(bytes) {
            Source::Decoded(text) => text,
            Source::Undecodable(reason) => panic!("expected decoded, got: {reason}"),
        }
    }

    fn reason(bytes: &[u8]) -> String {
        match decode_python_source(bytes) {
            Source::Decoded(text) => panic!("expected undecodable, got: {text:?}"),
            Source::Undecodable(reason) => reason,
        }
    }

    #[test]
    fn plain_utf8_no_declaration_decodes() {
        assert_eq!(decoded(b"x = 1\n"), "x = 1\n");
        // Multibyte UTF-8 with no declaration is fine.
        assert_eq!(decoded("x = '\u{e9}'\n".as_bytes()), "x = '\u{e9}'\n");
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"x = 1\n");
        assert_eq!(decoded(&bytes), "x = 1\n");
    }

    #[test]
    fn utf8_bom_followed_by_invalid_utf8_is_undecodable() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.push(0xFF);
        assert!(reason(&bytes).contains("UTF-8 BOM"));
    }

    #[test]
    fn invalid_utf8_without_declaration_is_undecodable() {
        let reason = reason(b"x = \"\xe9\"\n");
        assert!(reason.contains("not valid UTF-8"));
        assert!(reason.contains("no PEP 263"));
    }

    #[test]
    fn pep263_latin1_decodes_high_bytes() {
        // `# -*- coding: latin-1 -*-` then a lone 0xE9 (é in latin-1).
        let bytes = b"# -*- coding: latin-1 -*-\nx = \"\xe9\"\n";
        assert_eq!(
            decoded(bytes),
            "# -*- coding: latin-1 -*-\nx = \"\u{e9}\"\n"
        );
    }

    #[test]
    fn pep263_declaration_on_second_line_after_shebang() {
        let bytes = b"#!/usr/bin/env python\n# coding: iso-8859-1\nx = \"\xe9\"\n";
        assert_eq!(
            decoded(bytes),
            "#!/usr/bin/env python\n# coding: iso-8859-1\nx = \"\u{e9}\"\n"
        );
    }

    #[test]
    fn pep263_declaration_is_not_read_past_two_lines() {
        // A declaration on line 3 is ignored; the bytes are then plain UTF-8.
        assert_eq!(
            decoded(b"a = 1\nb = 2\n# coding: latin-1\n"),
            "a = 1\nb = 2\n# coding: latin-1\n"
        );
        // ... and if such a file is *not* valid UTF-8, it is undecodable
        // (the late declaration does not rescue it).
        assert!(reason(b"a = 1\nb = 2\n# coding: latin-1\n\xe9").contains("not valid UTF-8"));
    }

    #[test]
    fn explicit_utf8_declaration_that_lies_is_undecodable() {
        let reason = reason(b"# coding: utf-8\nx = \"\xff\"\n");
        assert!(reason.contains("declared as utf-8"));
    }

    #[test]
    fn explicit_utf8_declaration_decodes() {
        assert_eq!(
            decoded(b"# coding=utf-8\nx = 1\n"),
            "# coding=utf-8\nx = 1\n"
        );
    }

    #[test]
    fn ascii_declaration_accepts_ascii_and_rejects_non_ascii() {
        assert_eq!(
            decoded(b"# coding: ascii\nx = 1\n"),
            "# coding: ascii\nx = 1\n"
        );
        assert!(reason(b"# coding: ascii\nx = \"\xe9\"\n").contains("non-ASCII"));
    }

    #[test]
    fn unsupported_declared_encoding_is_undecodable_not_fatal() {
        let reason = reason(b"# coding: shift_jis\nx = 1\n");
        assert!(reason.contains("unsupported"));
        assert!(reason.contains("shift_jis"));
    }

    #[test]
    fn codec_name_aliases_and_normalization() {
        assert!(matches!(codec_for("UTF-8"), Some(Codec::Utf8)));
        assert!(matches!(codec_for("u8"), Some(Codec::Utf8)));
        assert!(matches!(codec_for("cp65001"), Some(Codec::Utf8)));
        assert!(matches!(codec_for("utf"), Some(Codec::Utf8)));
        assert!(matches!(codec_for(" Latin_1 "), Some(Codec::Latin1)));
        assert!(matches!(codec_for("ISO8859-1"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("8859"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("cp819"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("l1"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("latin"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("latin1"), Some(Codec::Latin1)));
        assert!(matches!(codec_for("US-ASCII"), Some(Codec::Ascii)));
        assert!(matches!(codec_for("646"), Some(Codec::Ascii)));
        assert!(matches!(codec_for("ascii"), Some(Codec::Ascii)));
        assert!(codec_for("ebcdic").is_none());
    }

    #[test]
    fn find_coding_edge_cases() {
        // No `#` at all.
        assert_eq!(find_coding(b"x = 1"), None);
        // `#` but no `coding` token.
        assert_eq!(find_coding(b"# just a comment"), None);
        // `coding` without a `:`/`=` separator.
        assert_eq!(find_coding(b"# coding latin-1"), None);
        // `coding:` with no name following.
        assert_eq!(find_coding(b"# coding:"), None);
        assert_eq!(find_coding(b"# coding:   "), None);
        // Leading whitespace (space, tab, form-feed) before `#`.
        assert_eq!(
            find_coding(b" \t\x0c# vim: set coding=latin-1 :"),
            Some("latin-1".to_owned())
        );
        // `=` separator and a `.`-containing name.
        assert_eq!(find_coding(b"#coding=ansi.x3"), Some("ansi.x3".to_owned()));
        // Empty line.
        assert_eq!(find_coding(b""), None);
    }

    #[test]
    fn sniff_coding_handles_short_inputs() {
        // Empty input: the loop breaks immediately.
        assert_eq!(sniff_coding(b""), None);
        // Single line, no trailing newline, no declaration.
        assert_eq!(sniff_coding(b"x = 1"), None);
        // Two non-declaration lines: loop completes without a match.
        assert_eq!(sniff_coding(b"a = 1\nb = 2\n"), None);
        // Declaration on the very first line returns early.
        assert_eq!(
            sniff_coding(b"# coding: latin-1\n"),
            Some("latin-1".to_owned())
        );
    }

    #[test]
    fn read_python_source_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.py");
        std::fs::write(&good, b"# coding: latin-1\nx = \"\xe9\"\n").expect("write");
        match read_python_source(&good).expect("read") {
            Source::Decoded(text) => assert!(text.contains('\u{e9}')),
            Source::Undecodable(reason) => panic!("unexpected skip: {reason}"),
        }
        assert_eq!(
            read_python_source_lossy(&good).as_deref(),
            Some("# coding: latin-1\nx = \"\u{e9}\"\n")
        );

        let bad = dir.path().join("bad.py");
        std::fs::write(&bad, b"\xe9\xe9\xe9").expect("write");
        assert!(matches!(
            read_python_source(&bad).expect("read"),
            Source::Undecodable(_)
        ));
        assert_eq!(read_python_source_lossy(&bad), None);

        let missing = dir.path().join("missing.py");
        assert!(read_python_source(&missing).is_err());
        assert_eq!(read_python_source_lossy(&missing), None);
    }

    #[test]
    fn source_enum_derives_debug() {
        assert!(format!("{:?}", Source::Decoded("x".to_owned())).starts_with("Decoded"));
        assert!(format!("{:?}", Source::Undecodable("why".to_owned())).starts_with("Undecodable"));
    }
}
