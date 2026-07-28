//! Persistent on-disk diagnostic cache (issue #68).
//!
//! Cached results are stored together in `{cache_dir}/diagnostics.json`.
//! The manifest records the *global fingerprint* that captures everything
//! that could affect the checker's output (tool version, config, Python
//! environment, ty binary, and all first-party source files). A changed
//! fingerprint invalidates the whole manifest, matching the dependency model
//! without one filesystem operation per checked file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::Deserialize;

use crate::check::is_prunable_dir;
use crate::diagnostic::Diagnostic;
use crate::resolve::{discover_site_packages, discover_site_packages_in_environment};

// ---------------------------------------------------------------------------
// FNV-1a 64-bit hasher
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash state.
///
/// Uses the standard FNV-1a basis and prime so the hashes are stable across
/// process restarts and platforms (no randomisation, fixed endianness via
/// `to_le_bytes`).
struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    /// FNV-1a 64-bit offset basis.
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Create a new hasher seeded with the FNV-1a 64-bit offset basis.
    const fn new() -> Self {
        Self { state: Self::BASIS }
    }

    /// Mix one byte into the hash.
    fn write_byte(&mut self, byte: u8) {
        self.state ^= u64::from(byte);
        self.state = self.state.wrapping_mul(Self::PRIME);
    }

    /// Mix a byte slice into the hash, length-prefixed to prevent
    /// `("ab","c")` colliding with `("a","bc")`.
    fn write_bytes(&mut self, bytes: &[u8]) {
        // 8-byte LE length prefix disambiguates differently-split inputs.
        for b in (bytes.len() as u64).to_le_bytes() {
            self.write_byte(b);
        }
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Return the current hash value.
    const fn finish(self) -> u64 {
        self.state
    }
}

// ---------------------------------------------------------------------------
// Global fingerprint
// ---------------------------------------------------------------------------

/// The name of the `ty` binary (platform-aware).
#[cfg(windows)]
const TY_BIN: &str = "ty.exe";
#[cfg(not(windows))]
const TY_BIN: &str = "ty";

/// Return the mtime of `path` as nanoseconds since the Unix epoch, or `None`
/// if the metadata cannot be obtained.
///
/// Excluded from the coverage gate: the inner `?` operators for `modified()`
/// and `duration_since()` only fail on platforms that do not support file
/// modification times or when the mtime predates the Unix epoch — both
/// unreachable under normal test conditions.
#[cfg_attr(coverage, coverage(off))]
fn mtime_nanos(path: &Path) -> Option<[u8; 16]> {
    let nanos = path
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(nanos.to_le_bytes())
}

/// One project entry captured during file selection for reuse by the global
/// fingerprint. The path may name any `.py`/`.pyi` entry, not only a regular
/// file, matching [`hash_py_file_mtimes`]'s existing behavior.
pub struct FingerprintFile {
    path: PathBuf,
    mtime: Option<[u8; 16]>,
}

impl FingerprintFile {
    pub fn from_path(path: PathBuf) -> Self {
        let mtime = mtime_nanos(&path);
        Self { path, mtime }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn hash_fingerprint_files(files: &[FingerprintFile], h: &mut FnvHasher) {
    for file in files {
        h.write_bytes(file.path.as_os_str().as_encoded_bytes());
        if let Some(mtime) = file.mtime {
            h.write_bytes(&mtime);
        }
    }
}

/// Find the `ty` binary on `PATH`, returning its path if found.
///
/// The result depends on the execution environment (whether `ty` is installed
/// and where); excluded from the coverage gate so environment-specific
/// branches are not required.
#[cfg_attr(coverage, coverage(off))]
fn find_ty_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(TY_BIN);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Mix the `ty` binary's path and mtime into `h`.
///
/// Excluded from the coverage gate: whether `ty` is found on `PATH` and
/// whether its metadata is readable both depend on the execution environment
/// and cannot be deterministically controlled in unit tests.
#[cfg_attr(coverage, coverage(off))]
fn hash_ty_binary(h: &mut FnvHasher) {
    let Some(ty_path) = find_ty_on_path() else {
        return;
    };
    h.write_bytes(ty_path.as_os_str().as_encoded_bytes());
    if let Some(mtime) = mtime_nanos(&ty_path) {
        h.write_bytes(&mtime);
    }
}

/// Collect all first-party `.py`/`.pyi` files under `root`, then mix each
/// file's path and mtime into `h`.
///
/// Excluded from the coverage gate: the walkdir error arm (requires an OS-level
/// permission fault to trigger) and the mtime-failure arm (requires a file to
/// disappear between the directory walk and the subsequent `stat` call) are
/// both unreachable under normal test conditions.
#[cfg_attr(coverage, coverage(off))]
fn hash_py_file_mtimes(root: &Path, h: &mut FnvHasher) {
    let mut py_files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_prunable_dir(e))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path().to_path_buf();
            let ext = path.extension()?;
            if ext == "py" || ext == "pyi" {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    py_files.sort();

    for path in &py_files {
        h.write_bytes(path.as_os_str().as_encoded_bytes());
        // Mtime: a missing or unreadable file contributes no mtime bytes, so
        // a file that appears or disappears changes the fingerprint (path
        // bytes alone differ between the two runs).
        if let Some(mtime) = mtime_nanos(path) {
            h.write_bytes(&mtime);
        }
    }
}

/// Whether walking `ancestor` with [`hash_py_file_mtimes`] also reaches
/// `descendant`.
///
/// A lexical prefix is insufficient: the walk deliberately prunes dot
/// directories, `venv`, and `__pycache__`, and does not follow symlinked
/// directories. An explicitly configured source root below one of those
/// boundaries still needs its own walk.
fn fingerprint_walk_covers(ancestor: &Path, descendant: &Path) -> bool {
    let Ok(relative) = descendant.strip_prefix(ancestor) else {
        return false;
    };
    let mut current = ancestor.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        current.push(name);
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "venv" || name == "__pycache__" {
            return false;
        }
        if current
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return false;
        }
    }
    true
}

/// Reduce first-party fingerprint roots to the non-overlapping walks needed to
/// cover them. Shallower roots come first so an ordinary configured `src/`
/// below `project_root` is recognized as already covered.
fn fingerprint_walk_roots(project_root: &Path, first_party_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(first_party_roots.len() + 1);
    candidates.push(project_root.to_path_buf());
    candidates.extend(first_party_roots.iter().cloned());
    candidates.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    candidates.dedup();

    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if !roots
            .iter()
            .any(|root| fingerprint_walk_covers(root, &candidate))
        {
            roots.push(candidate);
        }
    }
    roots
}

/// Compute the global fingerprint that captures everything outside a single
/// file that could affect the checker's output.
///
/// Hashes:
/// - tool version (`CARGO_PKG_VERSION`)
/// - `config_json` (serialised `Config`)
/// - `python_env` path + mtime (if provided)
/// - `ty` binary path + mtime (located via `PATH`)
/// - every `.py`/`.pyi` file under `project_root`, sorted by path, each
///   contributing its canonical path bytes and **mtime** (not content)
/// - every configured first-party source root, including absolute roots
///   outside `project_root`
/// - every `.py`/`.pyi` file in automatically discovered or explicitly
///   selected `site-packages`
///
/// Using mtime rather than content keeps the fingerprint computation to
/// `stat(2)` calls — one per file — avoiding the O(N × file-size) sequential
/// reads that content-hashing all first-party sources would add on every
/// invocation. A first-party change updates its mtime in normal workflows
/// (editors, `git checkout`, `cp`, etc.); a deliberately mtime-preserving
/// change could produce a stale cache hit, the same trade-off accepted by
/// `make`, Cargo, and most build systems.
///
/// The walk uses the same pruning logic as the main checker
/// ([`is_prunable_dir`]), so the fingerprint is stable between runs that do
/// not change any relevant file.
#[cfg(test)]
pub fn compute_global_fingerprint(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
    first_party_roots: &[PathBuf],
) -> u64 {
    compute_global_fingerprint_with_project_files(
        project_root,
        config_json,
        python_env,
        first_party_roots,
        None,
    )
}

/// Compute the global fingerprint, reusing an already sorted and complete
/// inventory of the project root when file selection captured one.
pub fn compute_global_fingerprint_with_project_files(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
    first_party_roots: &[PathBuf],
    project_files: Option<&[FingerprintFile]>,
) -> u64 {
    let mut h = FnvHasher::new();

    // Tool version — changing the binary invalidates all cached results.
    h.write_bytes(env!("CARGO_PKG_VERSION").as_bytes());

    // Serialised config.
    h.write_bytes(config_json.as_bytes());

    // Python environment path + mtime.
    if let Some(env_path) = python_env {
        h.write_bytes(env_path.as_os_str().as_encoded_bytes());
        if let Some(mtime) = mtime_nanos(env_path) {
            h.write_bytes(&mtime);
        }
    }

    // `ty` binary path + mtime.
    hash_ty_binary(&mut h);

    // All first-party `.py`/`.pyi` files, sorted within each non-overlapping
    // walk root, each contributing path bytes + mtime. Configured source roots
    // nested under the project are already reached by the project walk; avoid
    // traversing and statting those trees twice.
    for root in fingerprint_walk_roots(project_root, first_party_roots) {
        h.write_bytes(root.as_os_str().as_encoded_bytes());
        if root == project_root {
            if let Some(project_files) = project_files {
                hash_fingerprint_files(project_files, &mut h);
                continue;
            }
        }
        hash_py_file_mtimes(&root, &mut h);
    }

    // Third-party modules and stubs can change resolution just like
    // first-party source. The project walk intentionally prunes `.venv`, so
    // fingerprint each site-packages root separately. Match the resolver:
    // `--python` replaces automatic environment discovery when supplied.
    let mut site_packages = python_env.map_or_else(
        || discover_site_packages(project_root),
        discover_site_packages_in_environment,
    );
    site_packages.sort();
    site_packages.dedup();
    for root in site_packages {
        h.write_bytes(root.as_os_str().as_encoded_bytes());
        hash_py_file_mtimes(&root, &mut h);
    }

    h.finish()
}

// ---------------------------------------------------------------------------
// DiagnosticCache
// ---------------------------------------------------------------------------

const MANIFEST_NAME: &str = "diagnostics.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct CacheManifest {
    global_fingerprint: u64,
    entries: Vec<(PathBuf, Vec<Diagnostic>)>,
}

/// Persistent bounded on-disk diagnostic cache.
///
/// The complete manifest is read at most once and written at most once per
/// invocation. Replacing the single manifest naturally evicts results made
/// obsolete by a changed global fingerprint.
pub struct DiagnosticCache {
    dir: PathBuf,
    global_fingerprint: u64,
    entries: BTreeMap<PathBuf, Vec<Diagnostic>>,
}

impl DiagnosticCache {
    /// Open (or create) the cache rooted at `dir`.
    ///
    /// Creates the directory (and any missing parents) if it does not already
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if the directory cannot be created.
    pub fn open(dir: &Path, global_fingerprint: u64) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(dir)?;
        let entries = std::fs::read(dir.join(MANIFEST_NAME))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheManifest>(&bytes).ok())
            .filter(|manifest| manifest.global_fingerprint == global_fingerprint)
            .map_or_else(BTreeMap::new, |manifest| {
                manifest.entries.into_iter().collect()
            });
        Ok(Self {
            dir: dir.to_path_buf(),
            global_fingerprint,
            entries,
        })
    }

    /// Return cached diagnostics for one successfully checked file.
    pub fn get(&self, path: &Path) -> Option<Vec<Diagnostic>> {
        self.entries.get(path).cloned()
    }

    /// Add successfully checked-file results and write the manifest once.
    ///
    /// Errors are silently ignored: a failed cache write only causes a cold
    /// recomputation on the next run.
    #[cfg_attr(coverage, coverage(off))]
    pub fn put_all(&mut self, entries: Vec<(PathBuf, Vec<Diagnostic>)>) {
        if entries.is_empty() {
            return;
        }
        self.entries.extend(entries);
        if let Some(json) = self.serialize_manifest() {
            self.write_manifest_atomic(&json);
        }
    }

    /// Serialize each per-file entry in parallel, then join the already-valid
    /// JSON fragments into the single on-disk manifest.
    #[cfg_attr(coverage, coverage(off))]
    fn serialize_manifest(&self) -> Option<Vec<u8>> {
        let entries: Vec<_> = self.entries.iter().collect();
        let encoded: Vec<Vec<u8>> = entries
            .par_iter()
            .map(|(path, diagnostics)| serde_json::to_vec(&(path, diagnostics)))
            .collect::<Result<_, _>>()
            .ok()?;
        let payload_len: usize = encoded.iter().map(Vec::len).sum();
        let fingerprint = self.global_fingerprint.to_string();
        let mut json = Vec::with_capacity(payload_len + encoded.len() + fingerprint.len() + 40);
        json.extend_from_slice(b"{\"global_fingerprint\":");
        json.extend_from_slice(fingerprint.as_bytes());
        json.extend_from_slice(b",\"entries\":[");
        for (index, entry) in encoded.iter().enumerate() {
            if index != 0 {
                json.push(b',');
            }
            json.extend_from_slice(entry);
        }
        json.extend_from_slice(b"]}");
        Some(json)
    }

    /// Write a same-directory temporary file, then replace the manifest.
    #[cfg_attr(coverage, coverage(off))]
    fn write_manifest_atomic(&self, json: &[u8]) {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp_path = self.dir.join(format!(
            ".diagnostics-{}-{sequence}.tmp",
            std::process::id()
        ));
        if std::fs::write(&tmp_path, json).is_err() {
            return;
        }
        let manifest_path = self.dir.join(MANIFEST_NAME);
        #[cfg(windows)]
        if manifest_path.exists() {
            // Windows `rename` does not replace an existing destination. A
            // reader in this brief window sees a cache miss, never partial JSON.
            let _ = std::fs::remove_file(&manifest_path);
        }
        if std::fs::rename(&tmp_path, manifest_path).is_err() {
            let _ = std::fs::remove_file(tmp_path);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;
    use crate::diagnostic::Diagnostic;

    fn sample_diagnostic() -> Diagnostic {
        Diagnostic {
            path: PathBuf::from("pkg/mod.py"),
            line: 3,
            column: 1,
            callee: "pkg.mod.func".to_string(),
            positional_count: 3,
            max_positional: 1,
        }
    }

    // ---- FnvHasher ----------------------------------------------------------

    #[test]
    fn fnv_hasher_empty_is_basis() {
        let h = FnvHasher::new();
        assert_eq!(h.finish(), FnvHasher::BASIS);
    }

    #[test]
    fn fnv_hasher_write_bytes_consistency() {
        let mut a = FnvHasher::new();
        a.write_bytes(b"hello");
        let mut b = FnvHasher::new();
        b.write_bytes(b"hello");
        assert_eq!(a.finish(), b.finish());
    }

    #[test]
    fn fnv_hasher_different_inputs_differ() {
        let mut a = FnvHasher::new();
        a.write_bytes(b"hello");
        let mut b = FnvHasher::new();
        b.write_bytes(b"world");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn fnv_hasher_length_prefix_prevents_collision() {
        // ("ab", "c") must not collide with ("a", "bc").
        let mut a = FnvHasher::new();
        a.write_bytes(b"ab");
        a.write_bytes(b"c");

        let mut b = FnvHasher::new();
        b.write_bytes(b"a");
        b.write_bytes(b"bc");

        assert_ne!(a.finish(), b.finish());
    }

    // ---- mtime_nanos --------------------------------------------------------

    #[test]
    fn mtime_nanos_nonexistent_returns_none() {
        assert!(mtime_nanos(&PathBuf::from("/no/such/path/__x__")).is_none());
    }

    #[test]
    fn mtime_nanos_existing_path_returns_some() {
        let dir = tempdir().expect("tempdir");
        assert!(mtime_nanos(dir.path()).is_some());
    }

    #[test]
    fn fingerprint_files_allow_an_entry_without_mtime() {
        let path = PathBuf::from("missing.py");
        let mut actual = FnvHasher::new();
        hash_fingerprint_files(
            &[FingerprintFile {
                path: path.clone(),
                mtime: None,
            }],
            &mut actual,
        );

        let mut expected = FnvHasher::new();
        expected.write_bytes(path.as_os_str().as_encoded_bytes());
        assert_eq!(actual.finish(), expected.finish());
    }

    // ---- DiagnosticCache ----------------------------------------------------

    #[test]
    fn cache_open_creates_directory() {
        let base = tempdir().expect("tempdir");
        let cache_dir = base.path().join("nested").join("cache");
        let _cache = DiagnosticCache::open(&cache_dir, 1).expect("open");
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempdir().expect("tempdir");
        let cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        assert!(cache.get(Path::new("missing.py")).is_none());
    }

    #[test]
    fn cache_put_all_get_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(vec![(path.clone(), vec![sample_diagnostic()])]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        let got = cache.get(&path).expect("cache hit");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "pkg.mod.func");
    }

    #[test]
    fn cache_put_all_records_clean_files() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("clean.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(vec![(path.clone(), Vec::new())]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        let got = cache.get(&path).expect("cache hit");
        assert!(got.is_empty());
    }

    #[test]
    fn cache_get_corrupt_returns_none() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_NAME), b"not json").expect("write");
        let cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        assert!(cache.get(Path::new("mod.py")).is_none());
    }

    #[test]
    fn cache_fingerprint_mismatch_invalidates_manifest() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(vec![(path.clone(), vec![sample_diagnostic()])]);

        let cache = DiagnosticCache::open(dir.path(), 2).expect("reopen");
        assert!(cache.get(&path).is_none());
    }

    #[test]
    fn cache_file_selection_mismatch_is_a_miss() {
        let dir = tempdir().expect("tempdir");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(vec![(PathBuf::from("a.py"), Vec::new())]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        assert!(cache.get(Path::new("b.py")).is_none());
    }

    #[test]
    fn cache_replacement_keeps_storage_bounded() {
        let dir = tempdir().expect("tempdir");
        for fingerprint in 0..20 {
            let mut cache = DiagnosticCache::open(dir.path(), fingerprint).expect("open");
            cache.put_all(vec![(PathBuf::from("mod.py"), vec![sample_diagnostic()])]);
        }

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read cache directory")
            .collect::<Result<_, _>>()
            .expect("read entries");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name(), MANIFEST_NAME);
    }

    // ---- compute_global_fingerprint -----------------------------------------

    #[test]
    fn global_fingerprint_is_consistent() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None, &[]);
        let f2 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None, &[]);
        assert_eq!(f1, f2);
    }

    #[test]
    fn global_fingerprint_changes_with_config() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None, &[]);
        let f2 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":["foo"]}"#, None, &[]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn global_fingerprint_changes_with_new_py_file() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), "{}", None, &[]);
        std::fs::write(dir.path().join("mod.py"), b"x = 1").expect("write");
        let f2 = compute_global_fingerprint(dir.path(), "{}", None, &[]);
        assert_ne!(f1, f2);
    }

    #[test]
    fn global_fingerprint_nonexistent_python_env_path() {
        // A nonexistent python_env path: mtime_nanos returns None, but the
        // fingerprint still completes (the path bytes are still hashed).
        let dir = tempdir().expect("tempdir");
        let no_env = PathBuf::from("/no/such/python");
        let f1 = compute_global_fingerprint(dir.path(), "{}", Some(&no_env), &[]);
        let f2 = compute_global_fingerprint(dir.path(), "{}", Some(&no_env), &[]);
        assert_eq!(f1, f2);
    }

    #[test]
    fn global_fingerprint_with_existing_python_env() {
        // An *existing* python_env path: mtime_nanos returns Some, exercising
        // the mtime-hashing branch for the python environment.
        let dir = tempdir().expect("tempdir");
        let env_dir = tempdir().expect("env tempdir");
        let f1 = compute_global_fingerprint(dir.path(), "{}", Some(env_dir.path()), &[]);
        let f2 = compute_global_fingerprint(dir.path(), "{}", Some(env_dir.path()), &[]);
        assert_eq!(f1, f2);
    }

    #[test]
    fn global_fingerprint_changes_with_explicit_environment_stub() {
        let project = tempdir().expect("project tempdir");
        let env = tempdir().expect("environment tempdir");
        let site_packages = env
            .path()
            .join("lib")
            .join("python3.12")
            .join("site-packages");
        std::fs::create_dir_all(&site_packages).expect("mkdir site-packages");
        let before = compute_global_fingerprint(project.path(), "{}", Some(env.path()), &[]);
        std::fs::write(
            site_packages.join("dep.pyi"),
            "def f(a: int, /) -> None: ...\n",
        )
        .expect("write stub");
        let after = compute_global_fingerprint(project.path(), "{}", Some(env.path()), &[]);
        assert_ne!(before, after);
    }

    #[test]
    fn global_fingerprint_changes_with_external_source_root() {
        let project = tempdir().expect("project tempdir");
        let external = tempdir().expect("external tempdir");
        let roots = vec![project.path().to_path_buf(), external.path().to_path_buf()];
        let before = compute_global_fingerprint(project.path(), "{}", None, &roots);
        std::fs::write(external.path().join("dep.py"), "def f(value): ...\n")
            .expect("write dependency");
        let after = compute_global_fingerprint(project.path(), "{}", None, &roots);

        assert_ne!(before, after);
    }

    #[test]
    fn fingerprint_walk_roots_drop_reachable_nested_roots() {
        let project = tempdir().expect("project tempdir");
        let src = project.path().join("src");
        let nested = src.join("pkg");
        std::fs::create_dir_all(&nested).expect("create nested source root");

        let roots =
            fingerprint_walk_roots(project.path(), &[src, nested, project.path().to_path_buf()]);

        assert_eq!(roots, [project.path()]);
    }

    #[test]
    fn fingerprint_walk_roots_keep_nested_roots_below_pruned_directories() {
        let project = tempdir().expect("project tempdir");
        let hidden = project.path().join(".generated");
        let venv = project.path().join("venv/src");
        let pycache = project.path().join("__pycache__/generated");
        std::fs::create_dir_all(&hidden).expect("create hidden source root");
        std::fs::create_dir_all(&venv).expect("create venv source root");
        std::fs::create_dir_all(&pycache).expect("create pycache source root");

        let roots = fingerprint_walk_roots(
            project.path(),
            &[hidden.clone(), venv.clone(), pycache.clone()],
        );

        assert!(roots.contains(&project.path().to_path_buf()));
        assert!(roots.contains(&hidden));
        assert!(roots.contains(&venv));
        assert!(roots.contains(&pycache));
    }

    #[test]
    fn fingerprint_walk_does_not_cover_a_parent_directory_escape() {
        let project = tempdir().expect("project tempdir");
        let escaped = project.path().join("src/../../external");

        assert!(!fingerprint_walk_covers(project.path(), &escaped));
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_walk_does_not_cover_a_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let project = tempdir().expect("project tempdir");
        let external = tempdir().expect("external tempdir");
        let linked = project.path().join("linked");
        symlink(external.path(), &linked).expect("create directory symlink");

        assert!(!fingerprint_walk_covers(project.path(), &linked));
    }

    #[test]
    fn explicit_environment_fingerprint_ignores_project_venv() {
        let project = tempdir().expect("project tempdir");
        let default_site_packages = project.path().join(".venv/lib/python3.12/site-packages");
        std::fs::create_dir_all(&default_site_packages).expect("create default site-packages");
        let explicit = tempdir().expect("explicit environment tempdir");
        std::fs::create_dir_all(explicit.path().join("lib/python3.12/site-packages"))
            .expect("create explicit site-packages");

        let before = compute_global_fingerprint(project.path(), "{}", Some(explicit.path()), &[]);
        std::fs::write(default_site_packages.join("unused.pyi"), "def f(): ...\n")
            .expect("write unused default-environment stub");
        let after = compute_global_fingerprint(project.path(), "{}", Some(explicit.path()), &[]);

        assert_eq!(before, after);
    }
}
