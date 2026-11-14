use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

pub struct TyResolver {
    engine: ty_backend::Engine,
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

    pub fn hover(&self, path: &Path, offset: usize) -> Option<String> {
        if self.disabled {
            return None;
        }
        self.engine.hover(path, u32::try_from(offset).ok()?)
    }

    pub fn definitions(&self, path: &Path, offset: usize) -> Vec<super::DefLocation> {
        if self.disabled {
            return Vec::new();
        }
        u32::try_from(offset)
            .ok()
            .and_then(|offset| self.engine.definitions(path, offset))
            .unwrap_or_default()
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
        let offset = source
            .split_inclusive('\n')
            .take(usize::try_from(line).expect("line"))
            .map(str::len)
            .sum();
        resolver.hover(path, offset).expect("callable hover")
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
        assert!(resolver
            .hover(&path, source.find("target(1)").expect("call"))
            .is_none());
        assert!(resolver.definitions(&path, 0).is_empty());
        resolver.reenable();
        assert!(!resolver.is_disabled());
        assert!(hover(&mut resolver, &path, source, 1).contains("target"));
    }

    #[test]
    fn native_queries_preserve_unicode_offsets_and_file_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("module %20 turtle.py");
        let source = "# α\r\ncaption = '🐢'\rdef target(value): ...\r\ntarget(1)\n";
        std::fs::write(&path, source).expect("write source");
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut resolver = TyResolver::start(&session).expect("worker");
        resolver.ensure_open(&path, source).expect("open source");
        let call = source.rfind("target(1)").expect("call");
        let result = resolver.hover(&path, call).expect("signature");
        assert!(result.contains("target(value"), "{result}");
        let definitions = resolver.definitions(&path, call);
        assert!(definitions.iter().any(|target| target.path == path
            && usize::try_from(target.offset).expect("offset")
                == source.find("target(value)").expect("definition")));
    }
    #[test]
    fn call_queries_return_the_signature_without_docstrings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let source = "def target(value: int) -> None:\n    \"\"\"Documentation is not part of a call signature.\"\"\"\n    pass\ntarget(1)\n";
        std::fs::write(&path, source).expect("write source");
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut resolver = TyResolver::start(&session).expect("worker");
        let result = hover(&mut resolver, &path, source, 3);
        assert!(result.contains("target(value: int)"), "{result}");
        assert!(!result.contains("Documentation"), "{result}");
    }

    #[test]
    fn hovering_an_argument_does_not_return_the_called_signature() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.py");
        let source = "def target(value: int) -> None: ...\nargument = 1\ntarget(argument)\n";
        std::fs::write(&path, source).expect("write source");
        let session = crate::ty_resolver::TySession::new(temp.path(), None);
        let mut resolver = TyResolver::start(&session).expect("worker");
        resolver.ensure_open(&path, source).expect("open source");
        let offset = source.find("argument)").expect("argument");
        let result = resolver.hover(&path, offset).expect("type");
        assert!(result.contains("Literal[1]"), "{result}");
        assert!(!result.contains("def target"), "{result}");
    }
}
