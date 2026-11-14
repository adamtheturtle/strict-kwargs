use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;

use serde_json::{json, Value};

pub struct TyResolver {
    engine: ty_backend::Engine,
    pending: HashMap<i64, Value>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    disabled: bool,
}

impl TyResolver {
    pub fn start(session: &super::TySession) -> Option<Self> {
        let engine = session
            .engine()
            .map_err(|e| eprintln!("embedded ty: {e}"))
            .ok()?;
        Some(Self {
            engine,
            pending: HashMap::new(),
            next_id: 1,
            opened: HashSet::new(),
            disabled: false,
        })
    }

    pub fn ensure_open(&mut self, path: &Path, source: &str) -> Option<()> {
        if self.disabled {
            return None;
        }
        if !self.opened.contains(path) {
            if self.engine.open_source(path, source).is_none() {
                self.disabled = true;
                return None;
            }
            self.opened.insert(path.to_path_buf());
        }
        Some(())
    }

    pub fn ask(&mut self, method: &str, path: &Path, line: u32, character: u32) -> Option<i64> {
        if self.disabled {
            return None;
        }
        let offset = self.engine.offset(path, line, character)?;
        let result = match method {
            "textDocument/hover" => self.engine.hover(path, offset)
                .map_or(Value::Null, |value| json!({"contents": {"value": value}})),
            "textDocument/definition" => self.engine.definitions(path, offset)
                .map_or(Value::Null, |locations| Value::Array(locations.into_iter().map(|(path, line, character)| {
                    json!({"uri": super::absolute_uri(&path), "range": {"start": {"line": line, "character": character}}})
                }).collect())),
            _ => return None,
        };
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, result);
        Some(id)
    }

    pub fn take(&mut self, id: i64) -> Option<Value> {
        self.pending.remove(&id)
    }
    pub const fn is_disabled(&self) -> bool {
        self.disabled
    }
    pub const fn reenable(&mut self) {
        self.disabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hover(resolver: &mut TyResolver, path: &Path, source: &str, line: u32) -> String {
        resolver.ensure_open(path, source).expect("open source");
        let id = resolver
            .ask("textDocument/hover", path, line, 0)
            .expect("hover request");
        let response = resolver.take(id).expect("hover response");
        response
            .get("contents")
            .and_then(|contents| contents.get("value"))
            .and_then(Value::as_str)
            .expect("callable hover")
            .to_owned()
    }

    #[test]
    fn changed_source_is_private_to_one_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let original = "def target(value): ...\ntarget(1)\n";
        std::fs::write(&path, original).expect("write source");
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut first = TyResolver::start(&session).expect("first worker");
        let mut second = TyResolver::start(&session).expect("second worker");
        let changed = "def target(value, extra): ...\ntarget(1)\n";
        assert!(hover(&mut first, &path, changed, 1).contains("extra"));
        assert!(!hover(&mut second, &path, original, 1).contains("extra"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read source"),
            original
        );
    }

    #[test]
    fn non_utf8_source_uses_the_decoded_buffer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let bytes = b"# coding: latin-1\ncaption = '\xe9'\ndef target(value): ...\ntarget(1)\n";
        std::fs::write(&path, bytes).expect("write Latin-1 source");
        let decoded = "# coding: latin-1\ncaption = '\u{e9}'\ndef target(value): ...\ntarget(1)\n";
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut resolver = TyResolver::start(&session).expect("worker");
        let result = hover(&mut resolver, &path, decoded, 3);
        assert!(result.contains("target"), "{result}");
        assert!(result.contains("value"), "{result}");
        assert_eq!(std::fs::read(&path).expect("read source"), bytes);
    }

    #[test]
    fn separate_runs_do_not_share_cached_dependency_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let dependency = temp.path().join("library.py");
        let source = "from library import target\ntarget(1)\n";
        std::fs::write(&path, source).expect("write caller");
        std::fs::write(&dependency, "def target(value): ...\n").expect("write dependency");
        let first_session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut first = TyResolver::start(&first_session).expect("first run");
        assert!(!hover(&mut first, &path, source, 1).contains("extra"));
        std::fs::write(&dependency, "def target(value, extra): ...\n").expect("change dependency");
        let second_session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut second = TyResolver::start(&second_session).expect("second run");
        assert!(hover(&mut second, &path, source, 1).contains("extra"));
        assert!(!hover(&mut first, &path, source, 1).contains("extra"));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_project_path_cannot_start_inference() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        let session = crate::ty_resolver::TySession::new(&root, None);
        assert!(TyResolver::start(&session).is_none());
    }

    #[test]
    fn source_failure_disables_queries_until_reenabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let source = "def target(value): ...\ntarget(1)\n";
        std::fs::write(&path, source).expect("write source");
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut resolver = TyResolver::start(&session).expect("worker");
        let missing = temp.path().join("missing.py");
        assert!(resolver.ensure_open(&missing, source).is_none());
        assert!(resolver.is_disabled());
        assert!(resolver.ensure_open(&path, source).is_none());
        assert!(resolver.ask("textDocument/hover", &path, 1, 0).is_none());
        resolver.reenable();
        assert!(!resolver.is_disabled());
        assert!(hover(&mut resolver, &path, source, 1).contains("target"));
    }
}
