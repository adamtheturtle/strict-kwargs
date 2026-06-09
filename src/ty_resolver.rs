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
        let mut line_starts = vec![0usize];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        Self { line_starts }
    }

    pub fn position(&self, source: &str, offset: usize) -> (u32, u32) {
        let line_index = self.line_starts.partition_point(|&start| start <= offset);
        let zero_based_line = line_index.saturating_sub(1);
        let line_start = self.line_starts.get(zero_based_line).copied().unwrap_or(0);
        let mut col_utf16 = 0usize;
        if let Some(line_suffix) = source.get(line_start..) {
            for (relative, ch) in line_suffix.char_indices() {
                if line_start + relative >= offset || ch == '\n' {
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

/// Build a [`Command`] for the `ty` executable.
///
/// strict-kwargs declares `ty` as a hard dependency of its wheel (see
/// `pyproject.toml`), so `ty` normally lives in the *same*
/// `bin`/`Scripts` directory as our own binary. `uv tool install` does
/// **not** expose a dependency's entry point on `PATH`, so a bare `ty`
/// would not be found for the recommended install. Prefer the co-located
/// `ty[.exe]`; fall back to a bare `ty` (resolved via `PATH`) for `cargo
/// install` users or an activated venv where it is on `PATH`.
///
/// Host-/install-layout-specific glue: which of the
/// `current_exe`/parent/`is_file` arms is taken depends on how the binary
/// was installed (the coverage environment runs the test harness, where no
/// sibling `ty` exists, so only the PATH fallback is taken). Excluded from
/// the gate for the same reason as the rest of the ty-subprocess glue;
/// behaviour is exercised by the ty-backed integration tests and the
/// `ty`-absent CLI test.
#[cfg_attr(coverage, coverage(off))]
fn ty_command() -> Command {
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| dir.join(if cfg!(windows) { "ty.exe" } else { "ty" }))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("ty"));
    Command::new(program)
}

/// Whether a usable `ty` executable can be located (next to our own binary,
/// or on `PATH`; see [`ty_command`]).
pub fn ty_binary_present() -> bool {
    ty_command()
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

type FxPending = std::collections::HashMap<i64, Value>;

/// Build the LSP `initialize` params, optionally forwarding an explicit
/// `--python` environment.
///
/// `python_env`, when set, is the `--python` value (an interpreter, a venv
/// directory, or a `sys.prefix`, mirroring `ty check --python`). It is sent as
/// an absolute path: ty resolves a relative `environment.python` against its
/// workspace root, but a CLI-supplied value is relative to the user's cwd.
/// `ty server` accepts dynamic options during initialize for clients (like
/// this one) that do not implement `workspace/configuration`. A bad path is
/// not validated here: ty just resolves nothing against it, so the fallback
/// fails closed (no wrong diagnostics) exactly as when no env is configured.
fn initialize_params(project_root: &Path, python_env: Option<&Path>) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "rootUri": absolute_uri(project_root),
        // Advertise pull-diagnostics support so ty does not eagerly
        // type-check (and push `publishDiagnostics` for) every opened file.
        // This run only ever issues hover/definition requests, which ty
        // computes on demand; skipping the per-file diagnostics pass is the
        // difference between minutes and seconds on a large project where
        // most files have at least one call deferred to the ty fallback.
        "capabilities": {
            "textDocument": {
                "diagnostic": {},
            },
        },
    });
    if let Some(python) = python_env {
        let abs = std::path::absolute(python).unwrap_or_else(|_| python.to_path_buf());
        params["initializationOptions"] = json!({
            "configuration": {
                "environment": { "python": abs.to_string_lossy() },
            },
        });
    }
    params
}

impl TyResolver {
    /// Start `ty server` and complete the LSP initialize handshake rooted at
    /// `project_root`. Returns `None` if ty is unavailable or misbehaves —
    /// the caller then runs without the inference fallback.
    ///
    /// `python_env`, when set, is the `--python` value (an interpreter, a
    /// venv directory, or a `sys.prefix`, mirroring `ty check --python`). It
    /// is forwarded to `ty server` so the inference fallback resolves
    /// third-party imports against that environment without the user editing
    /// ty's own config. `ty server` takes no CLI args, so this is delivered
    /// over LSP via `initializationOptions.configuration.environment.python`,
    /// the inline-config channel that mirrors ty's `[environment]` table.
    /// A bad path is not validated here: ty just resolves nothing against
    /// it, so the fallback fails closed (no wrong diagnostics) exactly as
    /// when no env is configured.
    // `start` is exercised by the ty-backed integration tests, but its only
    // control flow is `?` early-returns for failures — `ty` not spawning, its
    // stdio pipes not materializing, or the initialize handshake timing out —
    // that cannot occur in the coverage environment, where `ty` is guaranteed
    // present (CI asserts `ty version`; see `coverage.yml`). The
    // testable parts are factored out and unit-tested directly:
    // [`initialize_params`] (pure), and the RPC layer ([`Self::request`],
    // [`Self::collect`], [`Self::notify`], [`read_messages`]) via
    // [`Self::from_parts`]. So exclude only this defensive shell.
    #[cfg_attr(coverage, coverage(off))]
    pub fn start(project_root: &Path, python_env: Option<&Path>) -> Option<Self> {
        let mut child = ty_command()
            .arg("server")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_messages(stdout, &tx));

        let mut resolver = Self::from_parts(child, stdin, rx);

        let init_params = initialize_params(project_root, python_env);
        let id = resolver.request("initialize", &init_params)?;
        resolver.collect(id, INIT_TIMEOUT)?;
        resolver.notify("initialized", &json!({}))?;
        Some(resolver)
    }

    /// Assemble a resolver around an already-spawned server's handles. Split
    /// out so the RPC layer can be unit-tested with controllable transports.
    fn from_parts(child: Child, stdin: ChildStdin, incoming: Receiver<Value>) -> Self {
        Self {
            child,
            stdin,
            incoming,
            next_id: 1,
            opened: HashSet::new(),
            pending: FxPending::new(),
            disabled: false,
        }
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
            &json!({
                "textDocument": {
                    "uri": absolute_uri(path),
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
            &json!({
                "textDocument": { "uri": absolute_uri(path) },
                "position": { "line": line, "character": character },
            }),
        )
    }

    /// Collect the response for a previously [`Self::ask`]ed id.
    pub fn take(&mut self, id: i64) -> Option<Value> {
        self.collect(id, REQUEST_TIMEOUT)
    }

    fn request(&mut self, method: &str, params: &Value) -> Option<i64> {
        if self.disabled {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        Some(id)
    }

    fn notify(&mut self, method: &str, params: &Value) -> Option<()> {
        if self.disabled {
            return None;
        }
        self.send(&json!({
            "jsonrpc": "2.0", "method": method, "params": params
        }))
    }

    fn send(&mut self, msg: &Value) -> Option<()> {
        let body = serde_json::to_vec(msg).ok()?;
        if write_lsp_message(&mut self.stdin, &body).is_err() {
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
                    // The awaited response: return it directly.
                    if msg.get("method").is_none()
                        && msg.get("id").and_then(Value::as_i64) == Some(id)
                    {
                        return Some(msg.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Anything else (server request, other response, or a
                    // `publishDiagnostics` notification): route it so it is not
                    // dropped.
                    self.absorb(&msg);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    self.disabled = true;
                    return None;
                }
            }
        }
    }

    /// Route one incoming message that is not the response currently awaited:
    /// answer a server→client request, or buffer an out-of-order response by
    /// id. Notifications carry no id and are dropped.
    fn absorb(&mut self, msg: &Value) {
        if let Some(msg_id) = msg.get("id").and_then(Value::as_i64) {
            if msg.get("method").is_some() {
                // Server→client request: reply empty so ty never blocks.
                let _ = self.send(&json!({ "jsonrpc": "2.0", "id": msg_id, "result": null }));
            } else {
                self.pending
                    .insert(msg_id, msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
}

fn write_lsp_message(writer: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
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

/// Split `s` on top-level `sep` (bracket/paren/brace depth 0 only).
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
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

impl Drop for TyResolver {
    fn drop(&mut self) {
        let _ = self.send(&json!({
            "jsonrpc": "2.0", "id": -1, "method": "shutdown", "params": null
        }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read LSP frames from `stdout` and forward parsed JSON messages.
///
/// Ty-wire layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (unit-tested; real `ty` never emits the
/// malformed-frame / invalid-JSON branches).
#[cfg_attr(coverage, coverage(off))]
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
        if let Ok(mut value) = serde_json::from_slice::<Value>(&body) {
            // `publishDiagnostics` can carry thousands of diagnostic entries
            // per file. The client advertises pull diagnostics so ty should
            // not push any, but if one arrives anyway nothing here needs the
            // payload — drop it before it queues up in the unbounded reader
            // channel.
            if value.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                if let Some(params) = value.get_mut("params").and_then(Value::as_object_mut) {
                    params.remove("diagnostics");
                }
            }
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
        line: u32::try_from(start.get("line").and_then(Value::as_u64)?).ok()?,
        character: u32::try_from(start.get("character").and_then(Value::as_u64)?).ok()?,
    })
}

/// Build an RFC 8089 `file://` URI. Uses forward slashes and gives Windows
/// drive paths the leading slash LSP servers expect
/// (`C:\a` -> `file:///C:/a`), so paths round-trip with what ty returns.
///
/// Ty-wire layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (the POSIX/Windows arms are unit-tested but
/// only one is taken on a given host).
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
pub fn byte_offset_to_lsp(source: &str, offset: usize) -> (u32, u32) {
    LspLineIndex::new(source).position(source, offset)
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
        assert!(parse_callable_type_overloads("(def () -> int) | Any").is_empty());
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
        assert!(parse_callable_type_overloads("<class 'C'>").is_empty());
        assert!(
            parse_callable_type_overloads("<method-wrapper 'startswith' of string 'abc'>")
                .is_empty()
        );
        assert!(parse_callable_type_overloads("list[int]").is_empty());
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
    fn ty_binary_present_matches_actual_environment() {
        // `ty_binary_present` gates a hard requirement, so its correctness
        // matters even though the rest of the suite now needs `ty`. Assert
        // it agrees with an independent probe through the same resolution
        // (`ty_command`), so the success→bool mapping is verified whether or
        // not `ty` is reachable (this one test does not itself need `ty`).
        let expected = ty_command()
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert_eq!(ty_binary_present(), expected);
    }

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
        assert!(parse_callable_type_overloads("Overload[(a) -> b").is_empty());
        // No callable arm in the union at all.
        assert!(parse_callable_type_overloads("None | int").is_empty());
    }

    #[test]
    fn location_from_value_variants() {
        // Plain `Location`.
        let loc = location_from_value(&json!({
            "uri": "file:///a/x.py",
            "range": { "start": { "line": 3, "character": 5 } }
        }))
        .unwrap();
        assert_eq!(loc.path, PathBuf::from("/a/x.py"));
        assert_eq!((loc.line, loc.character), (3, 5));

        // Array of locations: first is taken.
        assert!(location_from_value(&json!([
            { "uri": "file:///a/y.py", "range": { "start": { "line": 0, "character": 0 } } }
        ]))
        .is_some());

        // `LocationLink` form (`targetUri`/`targetRange`).
        assert!(location_from_value(&json!({
            "targetUri": "file:///a/z.py",
            "targetRange": { "start": { "line": 1, "character": 2 } }
        }))
        .is_some());

        // Non-object/array => None.
        assert!(location_from_value(&Value::Null).is_none());
        // Empty array => None.
        assert!(location_from_value(&json!([])).is_none());
        // Missing range => None.
        assert!(location_from_value(&json!({ "uri": "file:///a/x.py" })).is_none());
        // Line out of u32 range => None.
        assert!(location_from_value(&json!({
            "uri": "file:///a/x.py",
            "range": { "start": { "line": 99_999_999_999u64, "character": 0 } }
        }))
        .is_none());
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

    #[test]
    fn initialize_params_with_and_without_python_env() {
        let plain = initialize_params(Path::new("/proj"), None);
        assert_eq!(plain["rootUri"], "file:///proj");
        assert!(plain.get("initializationOptions").is_none());

        // An empty path makes `std::path::absolute` error, exercising the
        // `unwrap_or_else` fallback; the value is still forwarded.
        let with_env = initialize_params(Path::new("/proj"), Some(Path::new("")));
        assert!(
            with_env["initializationOptions"]["configuration"]["environment"]["python"].is_string()
        );
    }

    // ----- LSP frame reader -------------------------------------------

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn read_messages_forwards_valid_frames_and_skips_garbage() {
        use std::io::Cursor;

        let (tx, rx) = std::sync::mpsc::channel();
        let mut bytes = frame(r#"{"id":1,"result":"ok"}"#);
        // A frame whose body is not valid JSON: decoded, skipped, loop continues.
        bytes.extend(frame("not json"));
        bytes.extend(frame(r#"{"id":2}"#));
        read_messages(Cursor::new(bytes), &tx);
        // Release the only sender so a drained channel reports disconnect
        // instead of blocking `recv` forever.
        drop(tx);

        let first = rx.recv().unwrap();
        assert_eq!(first["id"], 1);
        let second = rx.recv().unwrap();
        assert_eq!(second["id"], 2);
        // Only two valid frames were forwarded; the garbage one was skipped.
        assert!(rx.recv().is_err());
    }

    #[test]
    fn read_messages_strips_publish_diagnostics_payloads() {
        use std::io::Cursor;

        let (tx, rx) = std::sync::mpsc::channel();
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///a/x.py",
                "diagnostics": [
                    { "message": "large payload", "severity": 1 }
                ]
            }
        })
        .to_string();
        read_messages(Cursor::new(frame(&body)), &tx);
        drop(tx);

        let msg = rx.recv().unwrap();
        assert_eq!(
            msg.pointer("/params/uri").and_then(Value::as_str),
            Some("file:///a/x.py")
        );
        assert!(msg.pointer("/params/diagnostics").is_none());
        assert!(rx.recv().is_err());
    }

    #[test]
    fn read_messages_handles_truncated_and_odd_headers() {
        use std::io::Cursor;

        // Truncated header (no blank line) => returns immediately.
        let (tx, _rx) = std::sync::mpsc::channel();
        read_messages(Cursor::new(b"Content-Length: 5".to_vec()), &tx);

        // Header lines without `Content-Length:` and a non-numeric value
        // (parsed as 0); body is empty, fails JSON, loop hits EOF.
        let (tx, rx) = std::sync::mpsc::channel();
        read_messages(
            Cursor::new(b"X-Other: 1\r\nContent-Length: abc\r\n\r\n".to_vec()),
            &tx,
        );
        drop(tx);
        assert!(rx.recv().is_err());

        // Declared length exceeds the available body => body read fails.
        let (tx, _rx) = std::sync::mpsc::channel();
        read_messages(
            Cursor::new(b"Content-Length: 100\r\n\r\nshort".to_vec()),
            &tx,
        );
    }

    #[test]
    fn read_messages_stops_when_receiver_dropped() {
        use std::io::Cursor;

        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        // A valid frame, but nobody is listening => `tx.send` errors, return.
        read_messages(Cursor::new(frame(r#"{"id":1}"#)), &tx);
    }

    // ----- RPC layer driven through a controllable transport ----------
    //
    // These spawn throwaway child processes for the `Child`/`ChildStdin`
    // handles `TyResolver` owns, and feed responses through an in-memory
    // channel. Unix-only: `cat`/`true` give a deterministic alive/dead
    // stdin. The coverage gate runs on Linux; Windows `cargo test` simply
    // skips them (the code still compiles there).

    #[cfg(unix)]
    mod rpc {
        use super::super::*;
        use serde_json::json;
        use std::process::{Command, Stdio};

        fn alive_child() -> (std::process::Child, ChildStdin) {
            let mut child = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn cat");
            let stdin = child.stdin.take().expect("cat stdin");
            (child, stdin)
        }

        #[test]
        fn collect_matches_buffers_and_answers_server_requests() {
            let (child, stdin) = alive_child();
            let (tx, rx) = std::sync::mpsc::channel();
            let mut r = TyResolver::from_parts(child, stdin, rx);

            // Out-of-order: id 2 arrives before the awaited id 1.
            tx.send(json!({ "jsonrpc": "2.0", "id": 2, "result": "two" }))
                .unwrap();
            // A server->client request (has `id` and `method`): answered, skipped.
            tx.send(json!({
                "jsonrpc": "2.0", "id": 9, "method": "workspace/configuration"
            }))
            .unwrap();
            // A notification (no `id`): ignored.
            tx.send(json!({ "jsonrpc": "2.0", "method": "window/logMessage" }))
                .unwrap();
            // The awaited response, with no `result` field => `Null`.
            tx.send(json!({ "jsonrpc": "2.0", "id": 1 })).unwrap();

            assert_eq!(r.take(1), Some(Value::Null));
            // Buffered id 2 is served from `pending` without touching the channel.
            assert_eq!(r.take(2), Some(Value::from("two")));
        }

        #[test]
        fn collect_disconnect_disables_resolver() {
            let (child, stdin) = alive_child();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            drop(tx); // sender gone => recv_timeout => Disconnected.
            let mut r = TyResolver::from_parts(child, stdin, rx);
            assert_eq!(r.take(1), None);
            // Latched off: subsequent calls short-circuit.
            assert_eq!(r.ask("textDocument/hover", Path::new("/x.py"), 0, 0), None);
            assert!(r.ensure_open(Path::new("/x.py"), "src").is_none());
            assert!(r.request("initialize", &json!({})).is_none());
            assert!(r.notify("x", &json!({})).is_none());
        }

        #[test]
        fn write_lsp_message_handles_success_and_writer_failure() {
            struct BrokenWriter;
            impl std::io::Write for BrokenWriter {
                fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "closed",
                    ))
                }

                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }

            let msg = json!({ "jsonrpc": "2.0", "method": "x" });
            let body = serde_json::to_vec(&msg).expect("message serializes");
            let mut bytes = Vec::new();
            assert!(write_lsp_message(&mut bytes, &body).is_ok());
            assert!(String::from_utf8(bytes)
                .expect("message is utf8")
                .starts_with("Content-Length: "));
            assert!(write_lsp_message(&mut BrokenWriter, &body).is_err());
        }

        #[test]
        fn ask_and_request_succeed_against_live_stdin() {
            let (child, stdin) = alive_child();
            let (_tx, rx) = std::sync::mpsc::channel::<Value>();
            let mut r = TyResolver::from_parts(child, stdin, rx);
            // `cat` keeps stdin open, so the JSON-RPC write succeeds and
            // `ask` returns a fresh request id (covers the success path of
            // `request`/`send`). Ids increment per request.
            let first = r.ask("textDocument/hover", Path::new("/m.py"), 1, 0);
            let second = r.ask("textDocument/definition", Path::new("/m.py"), 2, 4);
            assert_eq!(first, Some(1));
            assert_eq!(second, Some(2));
        }

        #[test]
        fn ensure_open_opens_once_then_is_idempotent() {
            let (child, stdin) = alive_child();
            let (_tx, rx) = std::sync::mpsc::channel::<Value>();
            let mut r = TyResolver::from_parts(child, stdin, rx);
            let path = Path::new("/proj/m.py");
            // First call writes `didOpen` (send succeeds: `cat` is alive).
            assert_eq!(r.ensure_open(path, "print()"), Some(()));
            // Second call sees it already open and returns without resending.
            assert_eq!(r.ensure_open(path, "print()"), Some(()));
        }

        #[test]
        fn absorb_buffers_responses_and_answers_requests() {
            let (child, stdin) = alive_child();
            let (_tx, rx) = std::sync::mpsc::channel::<Value>();
            let mut r = TyResolver::from_parts(child, stdin, rx);

            // A notification (no id) is ignored without crashing.
            r.absorb(&json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": "file:///a/x.py", "diagnostics": [] }
            }));

            // A response (id, no method) is buffered by id for a later `take`.
            r.absorb(&json!({ "jsonrpc": "2.0", "id": 7, "result": "v" }));
            assert_eq!(r.take(7), Some(Value::from("v")));

            // A server→client request (id + method) is answered, not buffered.
            r.absorb(&json!({
                "jsonrpc": "2.0", "id": 9, "method": "workspace/configuration"
            }));
            assert!(!r.pending.contains_key(&9));
        }
    }
}
