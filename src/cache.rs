//! Persistent on-disk diagnostic cache (issue #68).
//!
//! Cached results are stored together in `{cache_dir}/diagnostics.json`.
//! The manifest records the *global fingerprint* that captures everything
//! that could affect the checker's output (tool version, config, Python
//! environment, ty binary, and all first-party source files). A changed
//! fingerprint invalidates the whole manifest, matching the dependency model
//! without one filesystem operation per checked file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

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
pub fn compute_global_fingerprint(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
    first_party_roots: &[PathBuf],
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

    // All first-party `.py`/`.pyi` files under `project_root`, sorted by
    // path, each contributing path bytes + mtime.  Mtime-based hashing keeps
    // this to stat(2) calls (cheap) rather than full file reads (expensive).
    hash_py_file_mtimes(project_root, &mut h);
    for root in first_party_roots {
        if root != project_root {
            h.write_bytes(root.as_os_str().as_encoded_bytes());
            hash_py_file_mtimes(root, &mut h);
        }
    }

    // Third-party modules and stubs can change resolution just like
    // first-party source. The project walk intentionally prunes `.venv`, so
    // fingerprint each site-packages root separately. Include both the
    // resolver's automatic environments and the explicit `--python` target.
    let mut site_packages = discover_site_packages(project_root);
    if let Some(env_path) = python_env {
        site_packages.extend(discover_site_packages_in_environment(env_path));
    }
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

#[derive(Deserialize, Serialize)]
struct CacheManifest {
    global_fingerprint: u64,
    files_fingerprint: u64,
    diagnostics: Vec<Diagnostic>,
}

fn files_fingerprint(files: &[PathBuf]) -> u64 {
    let mut h = FnvHasher::new();
    for path in files {
        h.write_bytes(path.as_os_str().as_encoded_bytes());
    }
    h.finish()
}

/// Persistent bounded on-disk diagnostic cache.
///
/// The complete manifest is read at most once and written at most once per
/// invocation. Replacing the single manifest naturally evicts results made
/// obsolete by a changed global fingerprint.
pub struct DiagnosticCache {
    dir: PathBuf,
    global_fingerprint: u64,
    manifest: Option<CacheManifest>,
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
        let manifest = std::fs::read(dir.join(MANIFEST_NAME))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheManifest>(&bytes).ok())
            .filter(|manifest| manifest.global_fingerprint == global_fingerprint);
        Ok(Self {
            dir: dir.to_path_buf(),
            global_fingerprint,
            manifest,
        })
    }

    /// Return cached diagnostics when the ordered checked-file set matches.
    pub fn get_all(&self, files: &[PathBuf]) -> Option<Vec<Diagnostic>> {
        let manifest = self.manifest.as_ref()?;
        (manifest.files_fingerprint == files_fingerprint(files))
            .then(|| manifest.diagnostics.clone())
    }

    /// Replace checked-file results and write the manifest once.
    ///
    /// Errors are silently ignored: a failed cache write only causes a cold
    /// recomputation on the next run.
    #[cfg_attr(coverage, coverage(off))]
    pub fn put_all(&mut self, files: &[PathBuf], diagnostics: &[Diagnostic]) {
        if files.is_empty() {
            return;
        }
        let manifest = CacheManifest {
            global_fingerprint: self.global_fingerprint,
            files_fingerprint: files_fingerprint(files),
            diagnostics: diagnostics.to_vec(),
        };
        if let Ok(json) = serde_json::to_vec(&manifest) {
            self.write_manifest_atomic(&json);
            self.manifest = Some(manifest);
        }
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
        assert!(cache.get_all(&[PathBuf::from("missing.py")]).is_none());
    }

    #[test]
    fn cache_put_all_get_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(std::slice::from_ref(&path), &[sample_diagnostic()]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        let got = cache
            .get_all(std::slice::from_ref(&path))
            .expect("cache hit");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "pkg.mod.func");
    }

    #[test]
    fn cache_put_all_records_clean_files() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("clean.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(std::slice::from_ref(&path), &[]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        let got = cache
            .get_all(std::slice::from_ref(&path))
            .expect("cache hit");
        assert!(got.is_empty());
    }

    #[test]
    fn cache_get_corrupt_returns_none() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_NAME), b"not json").expect("write");
        let cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        assert!(cache.get_all(&[PathBuf::from("mod.py")]).is_none());
    }

    #[test]
    fn cache_fingerprint_mismatch_invalidates_manifest() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(std::slice::from_ref(&path), &[sample_diagnostic()]);

        let cache = DiagnosticCache::open(dir.path(), 2).expect("reopen");
        assert!(cache.get_all(&[path]).is_none());
    }

    #[test]
    fn cache_file_selection_mismatch_is_a_miss() {
        let dir = tempdir().expect("tempdir");
        let mut cache = DiagnosticCache::open(dir.path(), 1).expect("open");
        cache.put_all(&[PathBuf::from("a.py")], &[]);

        let cache = DiagnosticCache::open(dir.path(), 1).expect("reopen");
        assert!(cache.get_all(&[PathBuf::from("b.py")]).is_none());
    }

    #[test]
    fn cache_replacement_keeps_storage_bounded() {
        let dir = tempdir().expect("tempdir");
        for fingerprint in 0..20 {
            let mut cache = DiagnosticCache::open(dir.path(), fingerprint).expect("open");
            cache.put_all(&[PathBuf::from("mod.py")], &[sample_diagnostic()]);
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
}
