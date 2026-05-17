//! Module resolver: maps a dotted import name to source text, mirroring how
//! ty/pyright resolve modules — first-party, then vendored typeshed stdlib,
//! then the environment's site-packages (PEP 561).

use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir};

/// Vendored typeshed `stdlib/` stubs, embedded at the pinned commit recorded
/// in `vendored/typeshed/COMMIT`.
static TYPESHED_STDLIB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/vendored/typeshed/stdlib");

pub struct ModuleResolver {
    /// First-party search roots (the project itself).
    first_party: Vec<PathBuf>,
    /// Discovered `site-packages` directories (third-party / PEP 561 stubs).
    site_packages: Vec<PathBuf>,
}

impl ModuleResolver {
    pub fn new(project_root: &Path) -> Self {
        Self {
            first_party: vec![project_root.to_path_buf()],
            site_packages: discover_site_packages(project_root),
        }
    }

    /// Resolve a dotted module name (e.g. ``os.path``) to its source text.
    /// Search order matches ty: first-party, stdlib, then site-packages.
    pub fn resolve(&self, dotted: &str) -> Option<String> {
        let rel = dotted.replace('.', "/");

        // 1. First-party source (`.py` then `.pyi`).
        for root in &self.first_party {
            if let Some(src) = read_module(root, &rel, &["py", "pyi"]) {
                return Some(src);
            }
        }

        // 2. Vendored typeshed stdlib (`.pyi` only).
        for candidate in [format!("{rel}.pyi"), format!("{rel}/__init__.pyi")] {
            if let Some(file) = TYPESHED_STDLIB.get_file(&candidate) {
                if let Some(text) = file.contents_utf8() {
                    return Some(text.to_string());
                }
            }
        }

        // 3. Third-party in site-packages, honoring PEP 561 stub packages.
        let top = dotted.split('.').next().unwrap_or(dotted);
        let stub_rel = match dotted.split_once('.') {
            Some((_, rest)) => format!("{top}-stubs/{}", rest.replace('.', "/")),
            None => format!("{top}-stubs"),
        };
        for sp in &self.site_packages {
            // Prefer dedicated `*-stubs` distributions, then inline packages.
            if let Some(src) = read_module(sp, &stub_rel, &["pyi"]) {
                return Some(src);
            }
            if let Some(src) = read_module(sp, &rel, &["pyi", "py"]) {
                return Some(src);
            }
        }

        None
    }
}

/// Try ``<root>/<rel>.<ext>`` then ``<root>/<rel>/__init__.<ext>``.
fn read_module(root: &Path, rel: &str, exts: &[&str]) -> Option<String> {
    for ext in exts {
        for candidate in [
            root.join(format!("{rel}.{ext}")),
            root.join(rel).join(format!("__init__.{ext}")),
        ] {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                return Some(text);
            }
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
