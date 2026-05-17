//! Resolve call targets through `ty`'s type inference by driving a
//! `ty server` (LSP) subprocess and issuing `textDocument/definition`.
//!
//! This gives strict-kwargs ty-grade resolution — inheritance/MRO, return
//! types, annotated parameters, overloads — that a standalone AST resolver
//! cannot do. Everything degrades gracefully: any failure (ty missing, slow,
//! protocol hiccup) yields `None` and the caller falls back to the built-in
//! resolver.

use std::collections::HashSet;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{json, Value};

/// A resolved definition location (0-based line, 0-based UTF-16 column).
pub struct DefLocation {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
}

/// Per-request timeout. ty normally answers in milliseconds; this only
/// bounds pathological hangs. The first failure latches ty OFF for the rest
/// of the run, so a slow ty never multiplies into a timeout storm.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The initialize handshake (project discovery) can be slower than steady
/// state, so allow more headroom.
const INIT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct TyResolver {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    /// Responses that arrived out of order, keyed by request id — required
    /// for pipelining (send many requests, then collect).
    pending: FxPending,
    /// Once true, all further work is skipped (ty died/hung/misbehaved).
    disabled: bool,
}

/// Whether a usable `ty` executable is on `PATH`.
pub fn ty_binary_present() -> bool {
    Command::new("ty")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

type FxPending = std::collections::HashMap<i64, Value>;

impl TyResolver {
    /// Start `ty server` and complete the LSP initialize handshake rooted at
    /// `project_root`. Returns `None` if ty is unavailable or misbehaves —
    /// the caller then runs without the inference fallback.
    pub fn start(project_root: &Path) -> Option<Self> {
        let mut child = Command::new("ty")
            .arg("server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_messages(stdout, &tx));

        let mut resolver = Self {
            child,
            stdin,
            incoming: rx,
            next_id: 1,
            opened: HashSet::new(),
            pending: FxPending::new(),
            disabled: false,
        };

        let id = resolver.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": path_to_uri(project_root),
                "capabilities": {},
            }),
        )?;
        resolver.collect(id, INIT_TIMEOUT)?;
        resolver.notify("initialized", json!({}))?;
        Some(resolver)
    }

    /// Open `path` (idempotent). Returns `None` if ty is disabled.
    pub fn ensure_open(&mut self, path: &Path, text: &str) -> Option<()> {
        if self.disabled {
            return None;
        }
        let key = path.to_path_buf();
        if self.opened.contains(&key) {
            return Some(());
        }
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": "python",
                    "version": 1,
                    "text": text,
                }
            }),
        )?;
        self.opened.insert(key);
        Some(())
    }

    /// Fire a positional request (`textDocument/hover` or `.../definition`)
    /// without waiting, returning its id for a later [`Self::take`]. This is
    /// what enables pipelining: send all, then collect all.
    pub fn ask(&mut self, method: &str, path: &Path, line: u32, character: u32) -> Option<i64> {
        if self.disabled {
            return None;
        }
        self.request(
            method,
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": character },
            }),
        )
    }

    /// Collect the response for a previously [`Self::ask`]ed id.
    pub fn take(&mut self, id: i64) -> Option<Value> {
        self.collect(id, REQUEST_TIMEOUT)
    }

    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
        if self.disabled {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        Some(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Option<()> {
        self.send(json!({
            "jsonrpc": "2.0", "method": method, "params": params
        }))
    }

    fn send(&mut self, msg: Value) -> Option<()> {
        let body = serde_json::to_vec(&msg).ok()?;
        let ok = write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| self.stdin.write_all(&body))
            .and_then(|()| self.stdin.flush())
            .is_ok();
        if !ok {
            self.disabled = true;
            return None;
        }
        Some(())
    }

    /// Wait for the response with `id`, buffering out-of-order responses and
    /// answering server→client requests so ty never blocks. Any timeout or
    /// disconnect latches ty OFF for the remainder of the run.
    fn collect(&mut self, id: i64, timeout: Duration) -> Option<Value> {
        if let Some(value) = self.pending.remove(&id) {
            return Some(value);
        }
        loop {
            match self.incoming.recv_timeout(timeout) {
                Ok(msg) => {
                    if let Some(msg_id) = msg.get("id").and_then(Value::as_i64) {
                        if msg.get("method").is_some() {
                            // Server→client request: reply empty to unblock ty.
                            let _ = self.send(json!({
                                "jsonrpc": "2.0", "id": msg_id, "result": null
                            }));
                        } else if msg_id == id {
                            return Some(msg.get("result").cloned().unwrap_or(Value::Null));
                        } else {
                            self.pending
                                .insert(msg_id, msg.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                    // Notifications (no id) are ignored.
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    self.disabled = true;
                    return None;
                }
            }
        }
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

/// Extract a signature from ty hover text. Handles `def name(params) -> ret`
/// and `bound method Owner.name(params) -> ret`, including multi-line params,
/// and stops at the `---` docstring separator. Returns `None` for plain
/// types (`<class 'A'>`, `list[int]`) — the caller falls back to goto-def.
pub fn parse_hover_signature(value: &str) -> Option<HoverSignature> {
    let head = value.split("\n---").next().unwrap_or(value);
    let head = head.trim();

    let (name, owner) = if let Some(rest) = head.strip_prefix("def ") {
        let name = rest.split('(').next()?.trim().to_string();
        (name, None)
    } else if let Some(rest) = head.strip_prefix("bound method ") {
        let qualified = rest.split('(').next()?.trim();
        let (owner, name) = qualified.rsplit_once('.')?;
        (name.to_string(), Some(owner.to_string()))
    } else {
        return None;
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

impl Drop for TyResolver {
    fn drop(&mut self) {
        let _ = self.send(json!({
            "jsonrpc": "2.0", "id": -1, "method": "shutdown", "params": null
        }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read LSP frames from `stdout` and forward parsed JSON messages.
fn read_messages(stdout: impl Read, tx: &std::sync::mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        // Parse headers up to the blank line.
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        let mut content_length = 0usize;
        loop {
            if reader.read_exact(&mut byte).is_err() {
                return;
            }
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        for line in String::from_utf8_lossy(&header).lines() {
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
            if tx.send(value).is_err() {
                return;
            }
        }
    }
}

/// Extract the first `Location` from a `textDocument/definition` result,
/// which may be a single `Location`, an array, or `LocationLink`s.
pub fn location_from_value(result: &Value) -> Option<DefLocation> {
    let loc = match result {
        Value::Array(items) => items.first()?,
        Value::Object(_) => result,
        _ => return None,
    };
    // LocationLink uses `targetUri`/`targetRange`; Location uses `uri`/`range`.
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = loc.get("range").or_else(|| loc.get("targetRange"))?;
    let start = range.get("start")?;
    Some(DefLocation {
        path: uri_to_path(uri)?,
        line: start.get("line").and_then(Value::as_u64)? as u32,
        character: start.get("character").and_then(Value::as_u64)? as u32,
    })
}

/// Build an RFC 8089 `file://` URI. Uses forward slashes and gives Windows
/// drive paths the leading slash LSP servers expect
/// (`C:\a` -> `file:///C:/a`), so paths round-trip with what ty returns.
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

/// Parse a `file://` URI back to a filesystem path string. Strips the
/// leading slash from `/C:/...` (RFC 8089 Windows form) and
/// percent-decodes, so it round-trips with [`path_to_uri`] and matches
/// the native paths we compare against. Pure/deterministic for testing.
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

/// Minimal `%XX` percent-decoding (LSP servers encode spaces etc.).
fn percent_decode(s: &str) -> String {
    let raw = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hi = (raw[i + 1] as char).to_digit(16);
            let lo = (raw[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
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
pub fn byte_offset_to_lsp(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut col_utf16 = 0u32;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }
    (line, col_utf16)
}

/// Convert an LSP `(line, character)` position back to a byte offset.
pub fn lsp_to_byte_offset(source: &str, line: u32, character: u32) -> Option<usize> {
    let mut cur_line = 0u32;
    let mut col_utf16 = 0u32;
    for (idx, ch) in source.char_indices() {
        if cur_line == line && col_utf16 == character {
            return Some(idx);
        }
        if ch == '\n' {
            if cur_line == line {
                return Some(idx);
            }
            cur_line += 1;
            col_utf16 = 0;
        } else if cur_line == line {
            col_utf16 += ch.len_utf16() as u32;
        }
    }
    if cur_line == line && col_utf16 == character {
        return Some(source.len());
    }
    None
}

#[cfg(test)]
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
}
