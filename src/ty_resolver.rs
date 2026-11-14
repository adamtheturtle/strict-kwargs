//! Resolve call targets through the bundled ty inference engine.

use std::path::{Path, PathBuf};

#[cfg(test)]
use serde_json::json;
use serde_json::Value;

mod engine;
pub use engine::TyResolver;
pub use ty_backend::Session as TySession;

/// A resolved definition location (0-based line, 0-based UTF-16 column).
pub struct DefLocation {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
}

/// Per-file line index for converting byte offsets to LSP positions.
///
/// Pending ty requests are clustered per file and can number in the
/// thousands. Building line starts once keeps each conversion bounded to the
/// current line instead of rescanning the whole source from byte 0.
pub struct LspLineIndex {
    line_starts: Vec<usize>,
}

impl LspLineIndex {
    pub fn new(source: &str) -> Self {
        // Share the tool's universal-newline line splitting so LSP positions
        // agree with diagnostic positions on `\r`-delimited source (issue #270).
        Self {
            line_starts: crate::ast_util::line_starts(source),
        }
    }

    pub fn position(&self, source: &str, offset: usize) -> (u32, u32) {
        let line_index = self.line_starts.partition_point(|&start| start <= offset);
        let zero_based_line = line_index.saturating_sub(1);
        let line_start = self.line_starts.get(zero_based_line).copied().unwrap_or(0);
        let mut col_utf16 = 0usize;
        if let Some(line_suffix) = source.get(line_start..) {
            for (relative, ch) in line_suffix.char_indices() {
                if line_start + relative >= offset || ch == '\n' || ch == '\r' {
                    break;
                }
                col_utf16 += ch.len_utf16();
            }
        }
        (
            u32::try_from(zero_based_line).unwrap_or(u32::MAX),
            u32::try_from(col_utf16).unwrap_or(u32::MAX),
        )
    }
}

/// Parse a ty hover `contents.value` into a callable signature description.
pub struct HoverSignature {
    /// `def NAME(` / `bound method T.NAME(` — the callee's display name.
    pub name: String,
    /// Owning type for a bound method (`list[int]` etc.), if any.
    pub owner: Option<String>,
    /// The parameter-list text between the outermost parentheses.
    pub params: String,
}

/// Extract a signature from ty hover text. Handles `def name(params) -> ret`,
/// `class Name(params)` (a constructor; ty renders the `__init__`/`__new__`
/// parameters with `self` already omitted), and `bound method Owner.name(params)
/// -> ret`, including multi-line params, and stops at the `---` docstring
/// separator. Returns `None` for plain types (`<class 'A'>`, `list[int]`) —
/// the caller falls back to goto-def.
///
/// Parsing the `class Name(...)` shape directly matters because ty's
/// goto-definition for a re-exported stdlib class resolves into the runtime
/// `.py` shim and lands on the `from … import …` statement (not the class),
/// so the goto-def fallback silently drops the diagnostic in environments
/// where ty resolves against a runtime interpreter rather than its vendored
/// typeshed stubs (issue #195). The hover carries the constructor signature
/// consistently regardless of which the environment ty discovers.
pub fn parse_hover_signature(value: &str) -> Option<HoverSignature> {
    let head = value.split("\n---").next().unwrap_or(value);
    let head = head.trim();

    let (name, owner) = if let Some(rest) = head.strip_prefix("def ") {
        let name = rest.split('(').next()?.trim().to_string();
        (name, None)
    } else if let Some(rest) = head.strip_prefix("class ") {
        // A constructor: ty already omits `self`, so the parameter list maps
        // to the call site exactly like an unbound `def`.
        let name = rest.split('(').next()?.trim().to_string();
        (name, None)
    } else {
        let rest = head.strip_prefix("bound method ")?;
        let qualified = rest.split('(').next()?.trim();
        let (owner, name) = qualified.rsplit_once('.')?;
        (name.to_string(), Some(owner.to_string()))
    };
    if name.is_empty() {
        return None;
    }

    // Balanced extraction of the parameter list.
    let open = head.find('(')?;
    let mut depth = 0i32;
    let mut end = None;
    for (i, ch) in head[open..].char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let params = head[open + 1..end?].trim().to_string();
    Some(HoverSignature {
        name,
        owner,
        params,
    })
}

/// Split `s` on top-level `sep` (bracket/paren/brace depth 0 only).
pub fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// If `s` begins with `(` whose matching `)` is the final char, return the
/// inside; else `None`.
///
/// Part of the ty-wire parsing layer: in production these helpers are
/// reached only through the (excluded) `resolve_pending_with_ty` glue,
/// where real `ty` output never exercises their malformed-input branches.
/// Their behaviour — including those edge branches — is verified by the
/// `#[coverage(off)]` unit tests below, so they are excluded from the gate
/// for the same reason as the rest of the ty glue.
#[cfg_attr(coverage, coverage(off))]
fn unwrap_enclosing_parens(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return if i == s.len() - 1 {
                        Some(&s[1..i])
                    } else {
                        None
                    };
                }
            }
            _ => {}
        }
    }
    None
}

/// Balanced leading `(...)` group of `s`, if it is immediately followed
/// (modulo spaces) by `->`. Returns the inside (the parameter-list text).
fn leading_callable_params(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    let rest = s[i + 1..].trim_start();
                    return rest.starts_with("->").then(|| s[1..i].trim());
                }
            }
            _ => {}
        }
    }
    None
}

/// Like [`leading_callable_params`], but for an entry whose parameter list is
/// preceded by a callable's display name — `bound method Owner.name(p) -> r`
/// or `def name(p) -> r`. ty renders a single *resolved* signature this way
/// (rather than as an `Overload[…]` callable type) once it narrows an inferred
/// receiver: as of ty 0.0.42 `sys.stdout.write` hovers as
/// `(bound method TextIO.write(s: str, /) -> int) | Any`, where 0.0.40 used
/// `(Overload[(s: Buffer, /) -> int, (s: str, /) -> int]) | Any`. The `(p)`
/// group still carries the positional-only `/`, so strip the name and read it
/// directly, avoiding the goto-definition fallback that drops `/` (issue #14).
///
/// Ty-wire parsing layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (unit-tested, production-reached only via the
/// excluded ty glue).
#[cfg_attr(coverage, coverage(off))]
fn named_callable_params(s: &str) -> Option<&str> {
    let rest = s
        .trim()
        .strip_prefix("bound method ")
        .or_else(|| s.trim().strip_prefix("def "))?;
    // The (possibly dotted) name runs up to the first `(`, with no spaces —
    // anything else is not a `name(params)` head.
    let open = rest.find('(')?;
    let name = rest[..open].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    leading_callable_params(&rest[open..])
}

/// Parse ty hover text that is a *callable type* (not a bare `def`/`bound
/// method` display): `(p) -> r`, `(Overload[(p1) -> r1, (p2) -> r2]) | Any`,
/// or a single resolved signature rendered as `(bound method Owner.name(p) ->
/// r) | Any` / `(def name(p) -> r) | Any`, optionally wrapped in a top-level
/// union. Returns one parameter-list string per overload — `self` is already
/// excluded, as ty renders bound-method types without it. Crucially this
/// preserves typeshed positional-only `/` markers, which the goto-definition
/// fallback loses when it lands on runtime stdlib `.py` source (see issue #14).
///
/// Ty-wire parsing layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (unit-tested, production-reached only via the
/// excluded ty glue).
#[cfg_attr(coverage, coverage(off))]
pub fn parse_callable_type_overloads(value: &str) -> Vec<String> {
    let head = value.split("\n---").next().unwrap_or(value).trim();

    // Pick the callable arm of a top-level union (drop `Any`, `None`, …).
    let Some(callable) = split_top_level(head, '|')
        .into_iter()
        .map(str::trim)
        .find(|s| s.starts_with("Overload[") || (s.starts_with('(') && s.contains("->")))
    else {
        return Vec::new();
    };

    // `(Overload[…])` / `(… ) -> …` may be wrapped in one enclosing paren.
    let callable = match unwrap_enclosing_parens(callable) {
        Some(inner) if leading_callable_params(callable).is_none() => inner,
        _ => callable,
    };

    let entries: Vec<&str> = if let Some(inner) = callable
        .strip_prefix("Overload[")
        .and_then(|s| s.strip_suffix(']'))
    {
        split_top_level(inner, ',')
    } else {
        vec![callable]
    };

    entries
        .into_iter()
        .filter_map(|e| {
            leading_callable_params(e)
                .or_else(|| named_callable_params(e))
                .map(str::to_string)
        })
        .collect()
}

/// Extract every `Location` from a `textDocument/definition` result, which
/// may be a single `Location`, an array, or `LocationLink`s. This preserves
/// the wire order; callers that choose one target must apply a stable order
/// because ty's internal file IDs depend on file loading history.
pub fn locations_from_value(result: &Value) -> Vec<DefLocation> {
    match result {
        Value::Array(items) => items.iter().filter_map(single_location).collect(),
        Value::Object(_) => single_location(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Decode one LSP location object. `LocationLink` uses
/// `targetUri`/`targetRange`; `Location` uses `uri`/`range`.
fn single_location(loc: &Value) -> Option<DefLocation> {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = loc.get("range").or_else(|| loc.get("targetRange"))?;
    let start = range.get("start")?;
    Some(DefLocation {
        path: uri_to_path(uri)?,
        line: u32::try_from(start.get("line").and_then(Value::as_u64)?).ok()?,
        character: u32::try_from(start.get("character").and_then(Value::as_u64)?).ok()?,
    })
}

/// Like [`path_to_uri`], but absolutizes a relative path against the current
/// directory first. Everything sent *to* ty goes through this: ty resolves
/// files and answers queries by absolute URI, so a relative CLI path
/// (`check .`) must not leak into the wire format. Resolution against the
/// process CWD matches how the same relative path is read from disk.
///
/// Host-specific glue like [`path_to_uri`] (the absolutized form depends on
/// the CWD); the pure URI encoding underneath is unit-tested directly.
#[cfg_attr(coverage, coverage(off))]
fn absolute_uri(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    path_to_uri(&absolute)
}

/// Build an RFC 8089 `file://` URI. Uses forward slashes and gives Windows
/// drive paths the leading slash LSP servers expect
/// (`C:\a` -> `file:///C:/a`), so paths round-trip with what ty returns.
///
/// Ty-wire layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (the POSIX/Windows arms are unit-tested but
/// only one is taken on a given host).
#[cfg_attr(coverage, coverage(off))]
fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        // POSIX absolute path: `file://` + `/abs` == `file:///abs`.
        format!("file://{s}")
    } else {
        // Windows drive path (`C:/a`) or other: needs the extra slash.
        format!("file:///{s}")
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri_to_path_string(uri).map(PathBuf::from)
}

/// Whether two paths denote the same file. ty returns URIs that decode to
/// forward-slash paths (`C:/a/x.py`) while the paths we hold on Windows use
/// native backslashes (`C:\a\x.py`); a plain `==` is lexicographic and would
/// never match, so the current file would be needlessly re-read from disk on
/// every ty-resolved call. Normalizing separators fixes that. Pure for tests.
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || a.to_string_lossy().replace('\\', "/") == b.to_string_lossy().replace('\\', "/")
}

/// Parse a `file://` URI back to a filesystem path string. Strips the
/// leading slash from `/C:/...` (RFC 8089 Windows form) and
/// percent-decodes, so it round-trips with [`path_to_uri`] and matches
/// the native paths we compare against. Pure/deterministic for testing.
///
/// Ty-wire layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (drive-letter vs POSIX arms unit-tested but
/// host-dependent).
#[cfg_attr(coverage, coverage(off))]
fn uri_to_path_string(uri: &str) -> Option<String> {
    let rest = percent_decode(uri.strip_prefix("file://")?);
    let bytes = rest.as_bytes();
    // `/C:/path` -> `C:/path` (drive-letter form).
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        Some(rest[1..].to_string())
    } else {
        Some(rest)
    }
}

/// A single hex digit's value (`0..=15`), or `None` if not `[0-9A-Fa-f]`.
/// Returns `u8` directly so percent-decoding needs no fallible (and thus
/// uncoverable) numeric conversion.
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Minimal `%XX` percent-decoding (LSP servers encode spaces etc.).
fn percent_decode(s: &str) -> String {
    let raw = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(raw[i + 1]), hex_nibble(raw[i + 2])) {
                // Each nibble is `0..=15`, so `hi * 16 + lo` is `0..=255`
                // and always fits a `u8` without conversion.
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert a byte offset in `source` to an LSP `(line, character)` position
/// (0-based line, 0-based UTF-16 code units), as the LSP spec requires.
#[cfg(test)]
fn byte_offset_to_lsp(source: &str, offset: usize) -> (u32, u32) {
    LspLineIndex::new(source).position(source, offset)
}

/// Convert an LSP `(line, character)` position back to a byte offset.
pub fn lsp_to_byte_offset(source: &str, line: u32, character: u32) -> Option<usize> {
    let mut cur_line = 0u32;
    let mut col_utf16 = 0u32;
    let mut chars = source.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if cur_line == line && col_utf16 == character {
            return Some(idx);
        }
        if ch == '\n' || ch == '\r' {
            if cur_line == line {
                return Some(idx);
            }
            cur_line += 1;
            col_utf16 = 0;
            // Python's universal newline rule treats CRLF as one newline.
            if ch == '\r' {
                let _ = chars.next_if(|&(_, next)| next == '\n');
            }
        } else if cur_line == line {
            col_utf16 += u32::try_from(ch.len_utf16()).unwrap_or(1);
        }
    }
    if cur_line == line && col_utf16 == character {
        return Some(source.len());
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::path::Path;

    // These run on every CI platform (incl. windows-latest) and need no
    // `ty` binary, so the Windows file-URI handling is actually exercised.

    #[test]
    fn posix_path_to_uri() {
        assert_eq!(
            path_to_uri(Path::new("/home/u/a.py")),
            "file:///home/u/a.py"
        );
    }

    #[test]
    fn windows_path_to_uri_is_rfc8089() {
        // Backslashes are replaced and the drive gets the leading slash;
        // the old `file://{lossy}` produced `file://C:\...` (the bug).
        assert_eq!(
            path_to_uri(Path::new(r"C:\Users\a\x.py")),
            "file:///C:/Users/a/x.py"
        );
    }

    #[test]
    fn windows_uri_to_path_strips_leading_slash() {
        // ty returns RFC 8089 `file:///C:/...`; the old code yielded the
        // invalid `/C:/...`, silently disabling the fallback on Windows.
        assert_eq!(
            uri_to_path_string("file:///C:/Users/a/x.py").as_deref(),
            Some("C:/Users/a/x.py")
        );
    }

    #[test]
    fn posix_uri_to_path_keeps_leading_slash() {
        assert_eq!(
            uri_to_path_string("file:///home/u/a.py").as_deref(),
            Some("/home/u/a.py")
        );
    }

    #[test]
    fn uri_percent_decoded() {
        assert_eq!(
            uri_to_path_string("file:///C:/Program%20Files/x.py").as_deref(),
            Some("C:/Program Files/x.py")
        );
    }

    #[test]
    fn windows_path_uri_round_trips() {
        // We don't percent-encode on output; decoding on input is still
        // exercised by `uri_percent_decoded`.
        let uri = path_to_uri(Path::new(r"C:\a b\x.py"));
        assert_eq!(uri, "file:///C:/a b/x.py");
        assert_eq!(uri_to_path_string(&uri).as_deref(), Some("C:/a b/x.py"));
    }

    #[test]
    fn posix_path_uri_round_trips() {
        let uri = path_to_uri(Path::new("/home/u/a.py"));
        assert_eq!(uri_to_path_string(&uri).as_deref(), Some("/home/u/a.py"));
    }

    #[test]
    fn callable_type_overloads_parses_overload_union() {
        // The exact ty hover for `sys.stdout.write` (issue #14): the `/`
        // positional-only markers must survive so the call is not flagged.
        assert_eq!(
            parse_callable_type_overloads(
                "(Overload[(s: Buffer, /) -> int, (s: str, /) -> int]) | Any"
            ),
            vec!["s: Buffer, /".to_string(), "s: str, /".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_parses_wrapped_bound_method() {
        // ty 0.0.42 renders `sys.stdout.write` as a single resolved bound
        // method wrapped in `| Any` (0.0.40 used `Overload[…]`). The
        // positional-only `/` must still be recovered from the hover so the
        // call is not flagged — otherwise we fall through to goto-definition,
        // which drops `/` on the inferred stdlib receiver (issue #14 redux).
        assert_eq!(
            parse_callable_type_overloads("(bound method TextIO.write(s: str, /) -> int) | Any"),
            vec!["s: str, /".to_string()],
        );
        // The same wrapping around a free-function `def` display.
        assert_eq!(
            parse_callable_type_overloads("(def f(a: int, b: str) -> None) | Any"),
            vec!["a: int, b: str".to_string()],
        );
        // A `def`/`bound method` head with an empty name is not a signature
        // and yields nothing (no crash).
        assert_eq!(
            parse_callable_type_overloads("(def () -> int) | Any").len(),
            0
        );
    }

    #[test]
    fn callable_type_overloads_single_and_bare_overload() {
        assert_eq!(
            parse_callable_type_overloads("(x: int) -> str"),
            vec!["x: int".to_string()],
        );
        assert_eq!(
            parse_callable_type_overloads("Overload[(a: int, /) -> int, (a: str, /) -> str]"),
            vec!["a: int, /".to_string(), "a: str, /".to_string()],
        );
        // Union in the return type, not the params.
        assert_eq!(
            parse_callable_type_overloads("(x: int) -> int | None"),
            vec!["x: int".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_keeps_callable_typed_param_intact() {
        // A callable-typed parameter must not be mistaken for a second
        // overload — only the leading `(...) ->` group is the signature.
        assert_eq!(
            parse_callable_type_overloads("(cb: (int) -> str, /) -> None"),
            vec!["cb: (int) -> str, /".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_rejects_non_callables() {
        assert_eq!(parse_callable_type_overloads("<class 'C'>").len(), 0);
        assert_eq!(
            parse_callable_type_overloads("<method-wrapper 'startswith' of string 'abc'>").len(),
            0
        );
        assert_eq!(parse_callable_type_overloads("list[int]").len(), 0);
    }

    #[test]
    fn same_path_tolerates_separator_mismatch() {
        // ty's decoded URI uses forward slashes; the path we hold on Windows
        // uses backslashes. They denote the same file and must compare equal.
        assert!(same_path(
            &uri_to_path("file:///C:/Users/a/x.py").unwrap(),
            Path::new(r"C:\Users\a\x.py"),
        ));
        assert!(same_path(
            Path::new("/home/u/a.py"),
            Path::new("/home/u/a.py")
        ));
        assert!(!same_path(
            Path::new("/home/u/a.py"),
            Path::new("/home/u/b.py")
        ));
    }

    // ----- Pure parsing/encoding helpers -------------------------------

    #[test]
    fn parse_hover_signature_def_and_bound_method() {
        let def = parse_hover_signature("def f(a: int, b: str) -> None").unwrap();
        assert_eq!(def.name, "f");
        assert!(def.owner.is_none());
        assert_eq!(def.params, "a: int, b: str");

        let bound = parse_hover_signature("bound method list[int].append(x: int) -> None").unwrap();
        assert_eq!(bound.name, "append");
        assert_eq!(bound.owner.as_deref(), Some("list[int]"));
        assert_eq!(bound.params, "x: int");

        // Docstring separator is stripped before parsing.
        let doc = parse_hover_signature("def g(a: int) -> int\n---\nDocs here").unwrap();
        assert_eq!(doc.params, "a: int");

        // Nested brackets in the parameter list keep `depth != 0` until the
        // outermost `)` (exercises the balance loop's non-terminating arm).
        let nested = parse_hover_signature("def h(a: dict[str, list[int]]) -> None").unwrap();
        assert_eq!(nested.params, "a: dict[str, list[int]]");
    }

    #[test]
    fn parse_hover_signature_class_constructor() {
        // ty renders a constructor hover as `class Name(params)` with `self`
        // omitted; it parses like an unbound `def` (issue #195).
        let single = parse_hover_signature("class FileReader(loader: FileLoader)").unwrap();
        assert_eq!(single.name, "FileReader");
        assert!(single.owner.is_none());
        assert_eq!(single.params, "loader: FileLoader");

        // Multi-line constructor hover with a docstring tail, as ty emits for
        // `BytesGenerator`.
        let multiline = parse_hover_signature(
            "class BytesGenerator(\n    \
             outfp: SupportsWrite[bytes],\n    \
             *,\n    \
             policy: Policy[_MessageT]\n\
             )\n---\nCreate the generator.",
        )
        .unwrap();
        assert_eq!(multiline.name, "BytesGenerator");
        assert!(multiline.owner.is_none());
        assert!(multiline.params.contains("outfp: SupportsWrite[bytes]"));
        assert!(multiline.params.contains("policy: Policy[_MessageT]"));

        // A bare class type display (no constructor parentheses) is not a
        // signature and still falls back to goto-def.
        assert!(parse_hover_signature("<class 'C'>").is_none());
    }

    #[test]
    fn parse_hover_signature_rejects_non_signatures() {
        // Neither `def ` nor `bound method `.
        assert!(parse_hover_signature("<class 'C'>").is_none());
        // `bound method ` with no `.` to split owner/name.
        assert!(parse_hover_signature("bound method noselfdot(a) -> None").is_none());
        // Empty callee name.
        assert!(parse_hover_signature("def (a) -> None").is_none());
    }

    #[test]
    fn split_top_level_respects_bracket_depth() {
        assert_eq!(split_top_level("a, b, c", ','), vec!["a", " b", " c"]);
        // Commas inside brackets are not split points.
        assert_eq!(
            split_top_level("a, f(x, y), b[1, 2]", ','),
            vec!["a", " f(x, y)", " b[1, 2]"]
        );
    }

    #[test]
    fn unwrap_enclosing_parens_cases() {
        assert_eq!(unwrap_enclosing_parens("(abc)"), Some("abc"));
        // Does not start with `(`.
        assert_eq!(unwrap_enclosing_parens("abc"), None);
        // First group closes before the end => not enclosing.
        assert_eq!(unwrap_enclosing_parens("(a)b"), None);
        // Never balances.
        assert_eq!(unwrap_enclosing_parens("(a"), None);
    }

    #[test]
    fn leading_callable_params_cases() {
        assert_eq!(leading_callable_params("(x: int) -> str"), Some("x: int"));
        // Not starting with `(`.
        assert_eq!(leading_callable_params("x: int"), None);
        // Balanced group not followed by `->`.
        assert_eq!(leading_callable_params("(x: int) : str"), None);
        // Never balances.
        assert_eq!(leading_callable_params("(x: int"), None);
    }

    #[test]
    fn parse_callable_type_overloads_edges() {
        // Enclosing parens whose inside is itself a direct `(...) -> ...`
        // keeps the wrapper (the guard's false arm).
        assert_eq!(
            parse_callable_type_overloads("((x: int) -> str)"),
            vec!["x: int".to_string()]
        );
        // `Overload[` without a closing `]` is treated as a single entry.
        assert_eq!(parse_callable_type_overloads("Overload[(a) -> b").len(), 0);
        // No callable arm in the union at all.
        assert_eq!(parse_callable_type_overloads("None | int").len(), 0);
    }

    #[test]
    fn locations_from_value_variants() {
        // Plain `Location`.
        let locs = locations_from_value(&json!({
            "uri": "file:///a/x.py",
            "range": { "start": { "line": 3, "character": 5 } }
        }));
        let [loc] = locs.as_slice() else {
            panic!("expected one location, got {}", locs.len());
        };
        assert_eq!(loc.path, PathBuf::from("/a/x.py"));
        assert_eq!((loc.line, loc.character), (3, 5));

        // Array of locations: all are kept, in ty's order, and an
        // undecodable entry is skipped without dropping the rest.
        let locs = locations_from_value(&json!([
            { "uri": "file:///a/y.py", "range": { "start": { "line": 0, "character": 0 } } },
            { "uri": "file:///a/no-range.py" },
            { "uri": "file:///a/z.py", "range": { "start": { "line": 7, "character": 1 } } }
        ]));
        assert_eq!(
            locs.iter().map(|l| l.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("/a/y.py"), PathBuf::from("/a/z.py")]
        );

        // `LocationLink` form (`targetUri`/`targetRange`).
        assert_eq!(
            locations_from_value(&json!({
                "targetUri": "file:///a/z.py",
                "targetRange": { "start": { "line": 1, "character": 2 } }
            }))
            .len(),
            1
        );

        // Non-object/array => empty.
        assert!(locations_from_value(&Value::Null).is_empty());
        // Empty array => empty.
        assert!(locations_from_value(&json!([])).is_empty());
        // Missing range => empty.
        assert!(locations_from_value(&json!({ "uri": "file:///a/x.py" })).is_empty());
        // Line out of u32 range => empty.
        assert!(locations_from_value(&json!({
            "uri": "file:///a/x.py",
            "range": { "start": { "line": 99_999_999_999u64, "character": 0 } }
        }))
        .is_empty());
    }

    #[test]
    fn byte_offset_and_lsp_round_trip_with_multibyte() {
        // `é` is 2 UTF-8 bytes / 1 UTF-16 unit; `𝄞` is 4 bytes / 2 units.
        let src = "ab\né𝄞x\ny";
        assert_eq!(byte_offset_to_lsp(src, 0), (0, 0));
        assert_eq!(byte_offset_to_lsp(src, 3), (1, 0));
        // After `é` (col 1) then `𝄞` (cols 1+2 = 3).
        let off_x = src.find('x').unwrap();
        assert_eq!(byte_offset_to_lsp(src, off_x), (1, 3));

        // lsp_to_byte_offset: start, within line, end-of-line, past end.
        assert_eq!(lsp_to_byte_offset(src, 0, 0), Some(0));
        assert_eq!(lsp_to_byte_offset(src, 1, 3), Some(off_x));
        // Column past the line's end returns the newline offset.
        assert_eq!(lsp_to_byte_offset("abc\ndef", 0, 99), Some(3));
        // Final position with no trailing newline => source length.
        assert_eq!(lsp_to_byte_offset("abc", 0, 3), Some(3));
        // Unreachable line => None.
        assert_eq!(lsp_to_byte_offset("abc", 9, 0), None);
    }

    #[test]
    fn lsp_to_byte_offset_uses_universal_newlines() {
        let src = "first\rsecond\r\nthird";
        assert_eq!(lsp_to_byte_offset(src, 1, 0), Some("first\r".len()));
        assert_eq!(lsp_to_byte_offset(src, 1, 99), Some("first\rsecond".len()));
        assert_eq!(
            lsp_to_byte_offset(src, 2, 0),
            Some("first\rsecond\r\n".len())
        );
    }

    #[test]
    fn percent_decode_and_uri_paths() {
        assert_eq!(percent_decode("a%20b"), "a b");
        // Invalid escape is passed through verbatim (bad nibble).
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        // Trailing `%` with no two following chars is passed through.
        assert_eq!(percent_decode("a%"), "a%");
        // Upper- and lower-case hex both decode.
        assert_eq!(percent_decode("%2F%2f"), "//");
        // Drive-letter URI strips the leading slash; POSIX keeps it.
        assert_eq!(
            uri_to_path_string("file:///C:/a/b").as_deref(),
            Some("C:/a/b")
        );
        assert_eq!(
            uri_to_path_string("file:///srv/a").as_deref(),
            Some("/srv/a")
        );
        // Not a `file://` URI.
        assert!(uri_to_path_string("http://x/y").is_none());
    }
}
