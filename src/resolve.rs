//! Module resolver: maps a dotted import name to source text, mirroring how
//! ty/pyright resolve modules — first-party, then vendored typeshed stdlib,
//! then the environment's site-packages (PEP 561).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use include_dir::{include_dir, Dir};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::SourceRoots;
use crate::source::read_python_source_lossy;

/// Vendored typeshed `stdlib/` stubs, embedded at the pinned commit recorded
/// in `vendored/typeshed/COMMIT`.
static TYPESHED_STDLIB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/vendored/typeshed/stdlib");
/// Direct path lookup for the embedded typeshed files.
///
/// `include_dir::Dir::get_file` recursively scans every entry, which makes
/// repeated resolution proportional to the entire vendored stdlib. Build the
/// small index on first use so each module lookup is constant-time instead.
static TYPESHED_STDLIB_FILES: LazyLock<
    FxHashMap<&'static Path, &'static include_dir::File<'static>>,
> = LazyLock::new(|| {
    fn add_files(
        dir: &'static Dir<'static>,
        files: &mut FxHashMap<&'static Path, &'static include_dir::File<'static>>,
    ) {
        files.extend(dir.files().map(|file| (file.path(), file)));
        for child in dir.dirs() {
            add_files(child, files);
        }
    }

    let mut files = FxHashMap::default();
    add_files(&TYPESHED_STDLIB, &mut files);
    files
});

pub struct ModuleResolver {
    /// First-party search roots (the project itself).
    first_party: Vec<PathBuf>,
    /// Configured namespace-package directories.
    namespace_packages: Option<Vec<PathBuf>>,
    /// Discovered `site-packages` directories (third-party / PEP 561 stubs).
    site_packages: Vec<PathBuf>,
}

impl ModuleResolver {
    pub(crate) fn new(
        project_root: &Path,
        source_roots: &SourceRoots,
        python_env: Option<&Path>,
    ) -> Self {
        let namespace_packages = source_roots.namespace_packages();
        Self {
            first_party: source_roots.first_party_for_resolution(),
            namespace_packages: (!namespace_packages.is_empty())
                .then(|| namespace_packages.to_vec()),
            site_packages: python_env.map_or_else(
                || discover_site_packages(project_root),
                discover_site_packages_in_environment,
            ),
        }
    }

    /// Resolve a dotted module name (e.g. ``os.path``) to its source.
    /// Search order matches ty: first-party, stdlib, then site-packages.
    pub fn resolve(&self, dotted: &str) -> Option<ResolvedModule> {
        let rel = dotted.replace('.', "/");

        // 1. First-party source (`.pyi` then `.py` so adjacent stubs win).
        if let Some(namespace_packages) = &self.namespace_packages {
            for root in &self.first_party {
                if let Some(m) = read_module(root, &rel, &["pyi", "py"]) {
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
                if let Some(m) = read_module(root, &rel, &["pyi", "py"]) {
                    return Some(m);
                }
            }
        }

        // 2. Vendored typeshed stdlib (`.pyi` only). Typeshed is all valid
        // UTF-8, so folding `contents_utf8()` into the same `Option` keeps
        // the (unreachable) non-UTF-8 case from being a separate branch.
        if let Some(text) = TYPESHED_STDLIB_FILES
            .get(Path::new(&format!("{rel}.pyi")))
            .and_then(|file| file.contents_utf8())
        {
            return Some(ResolvedModule::stdlib_module(text));
        }
        if let Some(text) = TYPESHED_STDLIB_FILES
            .get(Path::new(&format!("{rel}/__init__.pyi")))
            .and_then(|file| file.contents_utf8())
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
            if let Some(m) = read_module(sp, &rel, &["pyi"]) {
                return Some(m);
            }
            // Runtime package source is authoritative only when the
            // distribution opts into PEP 561. Otherwise leave the call for
            // ty, which can apply its full untyped-package inference instead
            // of treating our partial source index as a complete signature.
            if has_py_typed_marker(sp, dotted) {
                if let Some(m) = read_module(sp, &rel, &["py"]) {
                    return Some(m);
                }
            }
        }

        None
    }
}

fn has_py_typed_marker(site_packages: &Path, dotted: &str) -> bool {
    let mut package = site_packages.to_path_buf();
    dotted.split('.').any(|component| {
        package.push(component);
        package.join("py.typed").is_file()
    })
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
        // Python prefers a package directory over a same-name module file.
        if let Some(text) =
            read_python_source_lossy(&root.join(rel).join(format!("__init__.{ext}")))
        {
            return Some(ResolvedModule::package(text));
        }
        if let Some(text) = read_python_source_lossy(&root.join(format!("{rel}.{ext}"))) {
            return Some(ResolvedModule::module(text));
        }
    }
    None
}

/// Locate `site-packages` from the active venv (`VIRTUAL_ENV`) or a project
/// `.venv`, covering Unix (`lib/pythonX.Y/site-packages`) and Windows
/// (`Lib/site-packages`) layouts.
pub fn discover_site_packages(project_root: &Path) -> Vec<PathBuf> {
    let mut venvs: Vec<PathBuf> = Vec::new();
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        if !venv.is_empty() {
            venvs.push(PathBuf::from(venv));
        }
    }
    venvs.push(project_root.join(".venv"));

    discover_site_packages_in_venvs(&venvs)
}

/// Locate `site-packages` below an explicit interpreter, virtual environment,
/// or `sys.prefix` path accepted by `--python`.
pub fn discover_site_packages_in_environment(python_env: &Path) -> Vec<PathBuf> {
    let Some(environment_root) = python_environment_root(python_env) else {
        return Vec::new();
    };
    let found = discover_site_packages_in_venvs(&[environment_root]);
    filter_site_packages_for_interpreter(python_env, found)
}

/// Prefer `lib/pythonX.Y/site-packages` when `--python` names that minor.
/// Covered by unit tests; excluded from the line/branch gate with the version
/// tag parser.
#[cfg_attr(coverage, coverage(off))]
fn filter_site_packages_for_interpreter(python_env: &Path, found: Vec<PathBuf>) -> Vec<PathBuf> {
    // A versioned interpreter (`…/bin/python3.9`) must prefer that minor's
    // `lib/python3.9/site-packages` over a lexicographically later sibling
    // such as `python3.12` (#529).
    if let Some(tag) = interpreter_version_tag(python_env) {
        let matching: Vec<PathBuf> = found
            .iter()
            .filter(|path| {
                path.parent()
                    .and_then(|parent| parent.file_name())
                    .is_some_and(|name| name == std::ffi::OsStr::new(&tag))
            })
            .cloned()
            .collect();
        if !matching.is_empty() {
            return matching;
        }
    }
    found
}

/// `python3.9` / `python3.12.exe` → `python3.9` / `python3.12`; unversioned
/// `python` / `python3` → `None`.
#[cfg_attr(coverage, coverage(off))]
fn interpreter_version_tag(python_env: &Path) -> Option<String> {
    let name = python_env.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let rest = lower.strip_prefix("python")?;
    let rest = rest.strip_suffix(".exe").unwrap_or(rest);
    // Require a minor (`3.9`) so bare `python3` stays unscoped.
    let (major, minor) = rest.split_once('.')?;
    if major.is_empty()
        || !major.chars().all(|c| c.is_ascii_digit())
        || minor.is_empty()
        || !minor.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(format!("python{rest}"))
}

/// Whether a `--python` path has the shape of an interpreter, virtual
/// environment, or Python installation prefix.
#[must_use]
#[cfg_attr(coverage, coverage(off))]
pub fn is_python_environment(python_env: &Path) -> bool {
    if python_env.is_file() {
        let Some(parent_name) = python_env.parent().and_then(Path::file_name) else {
            return false;
        };
        let Some(file_name) = python_env.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        return (parent_name.eq_ignore_ascii_case("bin")
            || parent_name.eq_ignore_ascii_case("scripts"))
            && file_name.to_ascii_lowercase().starts_with("python");
    }
    if !python_env.is_dir() {
        return false;
    }
    if python_env.join("pyvenv.cfg").is_file()
        || python_env.join("Lib").join("site-packages").is_dir()
    {
        return true;
    }
    ["bin", "Scripts"].iter().any(|directory| {
        std::fs::read_dir(python_env.join(directory)).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_file())
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.to_ascii_lowercase().starts_with("python"))
            })
        })
    }) || discover_site_packages_in_venvs(&[python_env.to_path_buf()])
        .into_iter()
        .next()
        .is_some()
}

/// Normalize the path shapes accepted by `--python` to an environment root.
///
/// Excluded from the coverage gate because the parentless and nameless path
/// arms are platform-specific `Path` representation details. The directory,
/// Unix `bin/python`, Windows `Scripts/python.exe`, and unrelated-file shapes
/// are all covered by unit tests.
#[cfg_attr(coverage, coverage(off))]
fn python_environment_root(python_env: &Path) -> Option<PathBuf> {
    if python_env.is_dir() {
        return Some(python_env.to_path_buf());
    }
    let bin_dir = python_env.parent()?;
    let name = bin_dir.file_name()?;
    if name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("scripts") {
        bin_dir.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn discover_site_packages_in_venvs(venvs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for venv in venvs {
        let mut in_venv = Vec::new();
        // Windows layout.
        let win = venv.join("Lib").join("site-packages");
        if win.is_dir() {
            in_venv.push(win);
        }
        // Unix layout: lib/python*/site-packages (any minor version).
        let lib = venv.join("lib");
        if let Ok(entries) = std::fs::read_dir(&lib) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("python") {
                    let sp = entry.path().join("site-packages");
                    if sp.is_dir() {
                        in_venv.push(sp);
                    }
                }
            }
        }
        // Directory iteration order is not stable, but virtual-environment
        // order is significant: an active VIRTUAL_ENV takes precedence over
        // the project's fallback .venv.
        in_venv.sort();
        found.extend(in_venv);
    }
    let mut seen = FxHashSet::default();
    found.retain(|path| seen.insert(path.clone()));
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
        let resolver = ModuleResolver::new(root, &source_roots, None);

        // First-party `.py`.
        let first = resolver.resolve("mypkg").expect("first-party module");
        assert!(first.source.contains("def f"));
        assert!(!first.is_package);

        // Vendored typeshed stdlib module (`<name>.pyi`).
        let stdlib = resolver.resolve("types").expect("stdlib module");
        assert_ne!(stdlib.source, "");
        assert!(!stdlib.is_package);

        // Vendored typeshed stdlib package (`<name>/__init__.pyi`).
        let pkg = resolver.resolve("os").expect("stdlib package");
        assert!(pkg.is_package);
        assert_ne!(pkg.source, "");

        // Nested vendored module: confirms the recursive index includes files
        // below more than one embedded directory level.
        let nested = resolver
            .resolve("xml.etree.ElementTree")
            .expect("nested stdlib module");
        assert!(!nested.is_package);
        assert_ne!(nested.source, "");

        // Nothing resolves: unknown name.
        assert!(resolver.resolve("this_module_does_not_exist_xyz").is_none());
    }

    /// Configured `src` roots are searched before the repository root, which
    /// is the fallback. Putting the root first resolved a module present in
    /// both places from the root (issue #1086).
    #[test]
    fn configured_src_roots_are_searched_before_the_repository_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("src").join("shared.py"), "FROM_SRC = True\n").expect("write src");
        std::fs::write(root.join("shared.py"), "FROM_SRC = False\n").expect("write root");
        let config = crate::config::Config {
            src: vec![std::path::PathBuf::from("src")],
            ..crate::config::Config::default()
        };
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots, None);
        let module = resolver.resolve("shared").expect("module");
        assert!(
            module.source.contains("FROM_SRC = True"),
            "the configured src root must win, got: {}",
            module.source
        );
    }

    /// The repository root still resolves modules that no configured root
    /// provides (issue #1086).
    #[test]
    fn the_repository_root_remains_a_fallback_behind_configured_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(root.join("only_at_root.py"), "AT_ROOT = True\n").expect("write root");
        let config = crate::config::Config {
            src: vec![std::path::PathBuf::from("src")],
            ..crate::config::Config::default()
        };
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots, None);
        let module = resolver.resolve("only_at_root").expect("module");
        assert!(module.source.contains("AT_ROOT = True"));
    }

    #[test]
    fn resolves_first_party_package_and_pyi() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
        std::fs::write(root.join("pkg").join("__init__.pyi"), "x: int\n").expect("write");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots, None);
        let module = resolver.resolve("pkg").expect("package");
        assert!(module.is_package);
    }

    #[test]
    fn prefers_package_directory_over_same_name_module_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("collision.py"), "PACKAGE = False\n").expect("write module");
        std::fs::create_dir_all(root.join("collision")).expect("mkdir package");
        std::fs::write(
            root.join("collision").join("__init__.py"),
            "PACKAGE = True\n",
        )
        .expect("write package");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let module_resolver = ModuleResolver::new(root, &source_roots, None);
        let resolved = module_resolver.resolve("collision").expect("package wins");
        assert!(resolved.is_package);
        assert!(resolved.source.contains("PACKAGE = True"));
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
        let resolver = ModuleResolver::new(root, &source_roots, None);

        let namespace = resolver
            .resolve("airflow.providers")
            .expect("namespace package");
        assert!(namespace.is_package);
        assert_eq!(namespace.source, "");
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
        let resolver = ModuleResolver::new(root, &source_roots, None);
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

    #[test]
    fn explicit_python_environment_overrides_project_venv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let project_site = root.join(".venv/lib/python3.12/site-packages");
        let external_env = root.join("external-env");
        let external_site = external_env.join("lib/python3.12/site-packages");
        for (site, source) in [
            (&project_site, "def f(value): ...\n"),
            (&external_site, "def f(value, /): ...\n"),
        ] {
            let package = site.join("dep");
            std::fs::create_dir_all(&package).expect("mkdir package");
            std::fs::write(package.join("__init__.py"), source).expect("write package");
            std::fs::write(package.join("py.typed"), "").expect("write marker");
        }
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);

        let resolver = ModuleResolver::new(root, &source_roots, Some(&external_env));

        assert!(resolver
            .resolve("dep")
            .expect("external package")
            .source
            .contains("value, /"));
    }

    #[test]
    fn explicit_python_environment_ignores_untyped_runtime_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let package = root
            .join("external-env/lib/python3.12/site-packages")
            .join("dep");
        std::fs::create_dir_all(&package).expect("mkdir package");
        std::fs::write(package.join("__init__.py"), "def f(value): ...\n").expect("write package");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots, Some(&root.join("external-env")));

        assert!(resolver.resolve("dep").is_none());
    }

    #[test]
    fn explicit_python_environment_honors_subpackage_py_typed_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let package = root
            .join("external-env/lib/python3.12/site-packages")
            .join("namespace/typed");
        std::fs::create_dir_all(&package).expect("mkdir package");
        std::fs::write(package.join("py.typed"), "").expect("write marker");
        std::fs::write(package.join("module.py"), "def f(value): ...\n").expect("write module");
        let config = crate::config::Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let resolver = ModuleResolver::new(root, &source_roots, Some(&root.join("external-env")));

        assert!(resolver
            .resolve("namespace.typed.module")
            .expect("typed namespace subpackage")
            .source
            .contains("def f"));
    }

    #[test]
    fn python_environment_validation_rejects_arbitrary_existing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("README.md");
        let directory = dir.path().join("ordinary");
        std::fs::write(&file, "text").expect("write");
        std::fs::create_dir(&directory).expect("mkdir");
        assert!(!is_python_environment(&file));
        assert!(!is_python_environment(&directory));
    }

    #[test]
    fn python_environment_validation_accepts_documented_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let environment = dir.path().join("venv");
        let bin = environment.join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let interpreter = bin.join("python3");
        std::fs::write(&interpreter, "").expect("write");
        assert!(is_python_environment(&interpreter));
        assert!(is_python_environment(&environment));

        let configured = dir.path().join("configured");
        std::fs::create_dir(&configured).expect("mkdir");
        std::fs::write(configured.join("pyvenv.cfg"), "").expect("write");
        assert!(is_python_environment(&configured));
    }

    #[test]
    fn discovers_site_packages_from_explicit_environment_shapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let unix = dir.path().join("lib/python3.12/site-packages");
        let windows = dir.path().join("Lib/site-packages");
        std::fs::create_dir_all(&unix).expect("mkdir unix site-packages");
        std::fs::create_dir_all(&windows).expect("mkdir windows site-packages");

        let expected = vec![windows, unix];
        assert_eq!(discover_site_packages_in_environment(dir.path()), expected);
        assert_eq!(
            discover_site_packages_in_environment(&dir.path().join("bin/python")),
            expected
        );
        assert_eq!(
            discover_site_packages_in_environment(&dir.path().join("Scripts/python.exe")),
            expected
        );
        assert_eq!(
            discover_site_packages_in_environment(&dir.path().join("python")).len(),
            0
        );
    }

    #[test]
    fn versioned_interpreter_selects_matching_site_packages() {
        // Issue #529: `python3.9` must not prefer lexicographically later
        // `python3.12/site-packages` when both layouts exist.
        let dir = tempfile::tempdir().expect("tempdir");
        let sp39 = dir.path().join("lib/python3.9/site-packages");
        let sp312 = dir.path().join("lib/python3.12/site-packages");
        std::fs::create_dir_all(&sp39).expect("mkdir 3.9");
        std::fs::create_dir_all(&sp312).expect("mkdir 3.12");
        std::fs::create_dir_all(dir.path().join("bin")).expect("mkdir bin");
        let interpreter = dir.path().join("bin/python3.9");
        std::fs::write(&interpreter, "").expect("write interpreter");

        assert_eq!(
            discover_site_packages_in_environment(&interpreter),
            vec![sp39]
        );
        // Unversioned interpreter keeps every layout (sorted).
        let all = discover_site_packages_in_environment(&dir.path().join("bin/python3"));
        assert_eq!(all.len(), 2);
        assert!(all
            .iter()
            .any(|path| path.ends_with("python3.9/site-packages")));
        assert!(all
            .iter()
            .any(|path| path.ends_with("python3.12/site-packages")));

        // Versioned interpreter with no matching layout keeps every site-packages.
        let missing = discover_site_packages_in_environment(&dir.path().join("bin/python3.11"));
        assert_eq!(missing.len(), 2);
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
            assert_eq!(empty.len(), 0);
            // Unset (covers the `None` value arm).
            let unset = with_virtual_env(None, || discover_site_packages(dir.path()));
            assert_eq!(unset.len(), 0);
        });
    }

    #[test]
    fn discover_site_packages_preserves_virtual_env_precedence() {
        let _guard = ENV_LOCK.lock().expect("lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let active = dir.path().join("z-active");
        let project = dir.path().join("a-project");
        let active_site_packages = active.join("lib/python3.12/site-packages");
        let project_site_packages = project.join(".venv/lib/python3.12/site-packages");
        std::fs::create_dir_all(&active_site_packages).expect("mkdir active");
        std::fs::create_dir_all(&project_site_packages).expect("mkdir project");

        let found = with_virtual_env(Some(active.as_os_str()), || {
            discover_site_packages(&project)
        });

        assert_eq!(found, vec![active_site_packages, project_site_packages]);
    }
}
