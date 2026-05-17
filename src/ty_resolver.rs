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

pub struct TyResolver {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    timeout: Duration,
}

impl TyResolver {
    /// Start `ty server` and complete the LSP initialize handshake rooted at
    /// `project_root`. Returns `None` if ty is unavailable or misbehaves.
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
            timeout: Duration::from_secs(10),
        };

        let root_uri = path_to_uri(project_root);
        let id = resolver.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {},
            }),
        )?;
        resolver.await_response(id)?;
        resolver.notify("initialized", json!({}))?;
        Some(resolver)
    }

    /// Resolve the definition of the symbol at `(line, character)` (0-based,
    /// UTF-16) in `path`, opening the document on first use.
    pub fn definition(
        &mut self,
        path: &Path,
        text: &str,
        line: u32,
        character: u32,
    ) -> Option<DefLocation> {
        self.ensure_open(path, text)?;
        let uri = path_to_uri(path);
        let id = self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            }),
        )?;
        let result = self.await_response(id)?;
        location_from_result(&result)
    }

    fn ensure_open(&mut self, path: &Path, text: &str) -> Option<()> {
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

    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
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
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).ok()?;
        self.stdin.write_all(&body).ok()?;
        self.stdin.flush().ok()?;
        Some(())
    }

    /// Wait for the response with `id`, discarding notifications and
    /// unrelated server requests until it arrives or we time out.
    fn await_response(&mut self, id: i64) -> Option<Value> {
        loop {
            match self.incoming.recv_timeout(self.timeout) {
                Ok(msg) => {
                    if msg.get("id").and_then(Value::as_i64) == Some(id) {
                        return msg.get("result").cloned();
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    return None;
                }
            }
        }
    }
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
fn location_from_result(result: &Value) -> Option<DefLocation> {
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

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
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
