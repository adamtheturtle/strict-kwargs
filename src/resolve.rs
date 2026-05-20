//! Module resolver: maps a dotted import name to source text, mirroring how
//! ty/pyright resolve modules — first-party, then vendored typeshed stdlib,
//! then the environment's site-packages (PEP 561).

use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir};

use crate::config::SourceRoots;
use crate::source::read_python_source_lossy;

/// Vendored typeshed `stdlib/` stubs, embedded at the pinned commit recorded
/// in `vendored/typeshed/COMMIT`.
static TYPESHED_STDLIB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/vendored/typeshed/stdlib");

pub struct ModuleResolver {
    /// First-party search roots (the project itself).
    first_party: Vec<PathBuf>,
    /// Configured namespace-package directories.
    namespace_packages: Option<Vec<PathBuf>>,
    /// Discovered `site-packages` directories (third-party / PEP 561 stubs).
    site_packages: Vec<PathBuf>,
}

impl ModuleResolver {
    pub(crate) fn new(project_root: &Path, source_roots: &SourceRoots) -> Self {
        let namespace_packages = source_roots.namespace_packages();
        Self {
            first_party: source_roots.first_party_for_resolution(),
            namespace_packages: (!namespace_packages.is_empty())
                .then(|| namespace_packages.to_vec()),
            site_packages: discover_site_packages(project_root),
        }
    }

    /// Resolve a dotted module name (e.g. ``os.path``) to its source.
    /// Search order matches ty: first-party, stdlib, then site-packages.
    pub fn resolve(&self, dotted: &str) -> Option<ResolvedModule> {
        let rel = dotted.replace('.', "/");

        // 1. First-party source (`.py` then `.pyi`).
        if let Some(namespace_packages) = &self.namespace_packages {
            for root in &self.first_party {
                if let Some(m) = read_module(root, &rel, &["py", "pyi"]) {
                    return Some(m);
                }
                let namespace_dir = root.join(&rel);
                if namespace_dir.is_dir()
                    && is_namespace_package(namespace_packages, &namespace_dir)
                {
                    return Some(ResolvedModule::namespace_package());
                }
            }
        } else {
            for root in &self.first_party {
                if let Some(m) = read_module(root, &rel, &["py", "pyi"]) {
                    return Some(m);
                }
            }
        }

        // 2. Vendored typeshed stdlib (`.pyi` only). Typeshed is all valid
        // UTF-8, so folding `contents_utf8()` into the same `Option` keeps
        // the (unreachable) non-UTF-8 case from being a separate branch.
        if let Some(text) = TYPESHED_STDLIB
            .get_file(format!("{rel}.pyi"))
            .and_then(include_dir::File::contents_utf8)
        {
            return Some(ResolvedModule::stdlib_module(text));
        }
        if let Some(text) = TYPESHED_STDLIB
            .get_file(format!("{rel}/__init__.pyi"))
            .and_then(include_dir::File::contents_utf8)
        {
            return Some(ResolvedModule::stdlib_package(text));
        }

        // 3. Third-party in site-packages, honoring PEP 561 stub packages.
        let top = dotted.split('.').next().unwrap_or(dotted);
        let stub_rel = match dotted.split_once('.') {
            Some((_, rest)) => format!("{top}-stubs/{}", rest.replace('.', "/")),
            None => format!("{top}-stubs"),
        };
        for sp in &self.site_packages {
            // Prefer dedicated `*-stubs` distributions, then inline packages.
            if let Some(m) = read_module(sp, &stub_rel, &["pyi"]) {
                return Some(m);
            }
            if let Some(m) = read_module(sp, &rel, &["pyi", "py"]) {
                return Some(m);
            }
        }

        None
    }
}

fn is_namespace_package(namespace_packages: &[PathBuf], path: &Path) -> bool {
    namespace_packages.iter().any(|namespace| namespace == path)
}

/// A resolved module's source and whether it is a package (`__init__`),
/// which determines the base for relative imports inside it.
pub struct ResolvedModule {
    pub source: String,
    pub is_package: bool,
    pub guard_nesting: bool,
}

impl ResolvedModule {
    fn module(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            is_package: false,
            guard_nesting: true,
        }
    }
    fn package(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            is_package: true,
            guard_nesting: true,
        }
    }
    fn stdlib_module(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            is_package: false,
            guard_nesting: false,
        }
    }
    fn stdlib_package(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            is_package: true,
            guard_nesting: false,
        }
    }
    const fn namespace_package() -> Self {
        Self {
            source: String::new(),
            is_package: true,
            guard_nesting: true,
        }
    }
}

/// Try ``<root>/<rel>.<ext>`` (a module) then ``<root>/<rel>/__init__.<ext>``
/// (a package).
fn read_module(root: &Path, rel: &str, exts: &[&str]) -> Option<ResolvedModule> {
    for ext in exts {
        if let Some(text) = read_python_source_lossy(&root.join(format!("{rel}.{ext}"))) {
            return Some(ResolvedModule::module(text));
        }
        if let Some(text) =
            read_python_source_lossy(&root.join(rel).join(format!("__init__.{ext}")))
        {
            return Some(ResolvedModule::package(text));
        }
    }
    None
}

/// Locate `site-packages` from the active venv (`VIRTUAL_ENV`) or a project
/// `.venv`, covering Unix (`lib/pythonX.Y/site-packages`) and Windows
/// (`Lib/site-packages`) layouts.
fn discover_site_packages(project_root: &Path) -> Vec<PathBuf> {
    let mut venvs: Vec<PathBuf> = Vec::new();
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        if !venv.is_empty() {
            venvs.push(PathBuf::from(venv));
        }
    }
    venvs.push(project_root.join(".venv"));

    let mut found = Vec::new();
    for venv in venvs {
        // Windows layout.
        let win = venv.join("Lib").join("site-packages");
        if win.is_dir() {
            found.push(win);
        }
        // Unix layout: lib/python*/site-packages (any minor version).
        let lib = venv.join("lib");
        if let Ok(entries) = std::fs::read_dir(&lib) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("python") {
                    let sp = entry.path().join("site-packages");
                    if sp.is_dir() {
                        found.push(sp);
                    }
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `discover_site_packages` reads `VIRTUAL_ENV`; serialize the tests that
    // mutate it so they cannot race each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolves_first_party_then_stdlib_module_and_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("mypkg.py"), "def f(): ...\n").expect("write");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots);

        // First-party `.py`.
        let first = resolver.resolve("mypkg").expect("first-party module");
        assert!(first.source.contains("def f"));
        assert!(!first.is_package);

        // Vendored typeshed stdlib module (`<name>.pyi`).
        let stdlib = resolver.resolve("types").expect("stdlib module");
        assert!(!stdlib.source.is_empty());
        assert!(!stdlib.is_package);

        // Vendored typeshed stdlib package (`<name>/__init__.pyi`).
        let pkg = resolver.resolve("os").expect("stdlib package");
        assert!(pkg.is_package);
        assert!(!pkg.source.is_empty());

        // Nothing resolves: unknown name.
        assert!(resolver.resolve("this_module_does_not_exist_xyz").is_none());
    }

    #[test]
    fn resolves_first_party_package_and_pyi() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
        std::fs::write(root.join("pkg").join("__init__.pyi"), "x: int\n").expect("write");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots);
        let module = resolver.resolve("pkg").expect("package");
        assert!(module.is_package);
    }

    #[test]
    fn resolves_configured_source_root_and_namespace_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let namespace = root.join("src").join("airflow").join("providers");
        std::fs::create_dir_all(&namespace).expect("mkdir namespace");
        std::fs::write(namespace.join("tasks.py"), "def run(a: int) -> None: ...\n")
            .expect("write");
        let config = crate::config::Config {
            src: vec![PathBuf::from("src")],
            namespace_packages: vec![PathBuf::from("src/airflow/providers")],
            ..crate::config::Config::default()
        };
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots);

        let namespace = resolver
            .resolve("airflow.providers")
            .expect("namespace package");
        assert!(namespace.is_package);
        assert!(namespace.source.is_empty());
        assert!(resolver
            .resolve("airflow.providers.tasks")
            .expect("module under namespace")
            .source
            .contains("def run"));
    }

    #[test]
    fn resolves_site_packages_stub_and_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let sp = root
            .join(".venv")
            .join("lib")
            .join("python3.11")
            .join("site-packages");
        std::fs::create_dir_all(sp.join("vendor-stubs")).expect("mkdir");
        std::fs::write(sp.join("vendor-stubs").join("sub.pyi"), "y: int\n").expect("write");
        std::fs::write(sp.join("inline.pyi"), "z: int\n").expect("write");

        let _guard = ENV_LOCK.lock().expect("lock");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots);
        // `*-stubs` distribution is preferred for a submodule.
        assert!(resolver
            .resolve("vendor.sub")
            .expect("stub")
            .source
            .contains('y'));
        // Inline `.pyi` in site-packages.
        assert!(resolver
            .resolve("inline")
            .expect("inline")
            .source
            .contains('z'));
        // Top-level only (no dotted rest) and unknown.
        assert!(resolver.resolve("vendor").is_none());
    }

    /// Run `f` with `VIRTUAL_ENV` set to `value` (or removed when `None`),
    /// restoring the previous state afterwards. Nesting calls makes the
    /// previous-value `Some`/`None` restore arms both reachable.
    fn with_virtual_env<R>(value: Option<&std::ffi::OsStr>, f: impl FnOnce() -> R) -> R {
        let previous = std::env::var_os("VIRTUAL_ENV");
        match value {
            Some(value) => std::env::set_var("VIRTUAL_ENV", value),
            None => std::env::remove_var("VIRTUAL_ENV"),
        }
        let result = f();
        match previous {
            Some(previous) => std::env::set_var("VIRTUAL_ENV", previous),
            None => std::env::remove_var("VIRTUAL_ENV"),
        }
        result
    }

    #[test]
    fn discover_site_packages_honors_virtual_env_and_layouts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let venv = dir.path().join("venv");
        // Windows layout.
        std::fs::create_dir_all(venv.join("Lib").join("site-packages")).expect("mkdir win");
        // Unix layout (a `python*` directory with `site-packages`).
        std::fs::create_dir_all(venv.join("lib").join("python3.12").join("site-packages"))
            .expect("mkdir unix");
        // A `python*` directory *without* `site-packages` (is_dir() false arm).
        std::fs::create_dir_all(venv.join("lib").join("python3.9")).expect("mkdir bare");
        // A non-`python*` entry under `lib/` is ignored.
        std::fs::create_dir_all(venv.join("lib").join("other")).expect("mkdir");

        let _guard = ENV_LOCK.lock().expect("lock");
        // Outer layer establishes a pre-existing value so the inner
        // `with_virtual_env` restores via the `Some(previous)` arm.
        let found = with_virtual_env(Some(std::ffi::OsStr::new("sentinel")), || {
            with_virtual_env(Some(venv.as_os_str()), || {
                discover_site_packages(dir.path())
            })
        });

        assert!(found.contains(&venv.join("Lib").join("site-packages")));
        assert!(found.contains(&venv.join("lib").join("python3.12").join("site-packages")));
        assert!(!found
            .iter()
            .any(|p| p.starts_with(venv.join("lib").join("python3.9"))));
    }

    #[test]
    fn discover_site_packages_ignores_empty_and_unset_virtual_env() {
        let _guard = ENV_LOCK.lock().expect("lock");
        let dir = tempfile::tempdir().expect("tempdir");
        // Outer `None` layer clears any ambient `VIRTUAL_ENV` so the inner
        // calls deterministically restore via the `None(previous)` arm.
        with_virtual_env(None, || {
            // Empty value: pushed nowhere.
            let empty = with_virtual_env(Some(std::ffi::OsStr::new("")), || {
                discover_site_packages(dir.path())
            });
            assert!(empty.is_empty());
            // Unset (covers the `None` value arm).
            let unset = with_virtual_env(None, || discover_site_packages(dir.path()));
            assert!(unset.is_empty());
        });
    }
}
