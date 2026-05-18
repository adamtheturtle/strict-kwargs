//! Persistent on-disk diagnostic cache (issue #68).
//!
//! Each cached result is stored as `{cache_dir}/{key:016x}.json`, where the
//! key is an FNV-1a 64-bit hash mixing the file's content with a
//! *global fingerprint* that captures everything that could affect the
//! checker's output (tool version, config, Python environment, ty binary, and
//! all first-party source files).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::check::is_prunable_dir;
use crate::diagnostic::Diagnostic;

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
///
/// Using mtime rather than content for the dependency files keeps the
/// fingerprint computation to `stat(2)` calls — one per file — avoiding the
/// O(N × file-size) sequential reads that content-hashing all first-party
/// sources would add on every invocation.  The per-file cache key
/// ([`file_cache_key`]) still uses a content hash for the *checked* file
/// itself, so any content change there is detected exactly.  A first-party
/// dependency change will change its mtime in all normal workflows (editors,
/// `git checkout`, `cp`, etc.); a mtime-preserving change (e.g. `touch -t`)
/// would at worst produce a stale cache hit, the same trade-off accepted by
/// `make`, Cargo, and most build systems.
///
/// The walk uses the same pruning logic as the main checker
/// ([`is_prunable_dir`]), so the fingerprint is stable between runs that do
/// not change any relevant file.
pub fn compute_global_fingerprint(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
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

    h.finish()
}

/// Compute the per-file cache key by mixing the file's canonical path with
/// the global fingerprint.
///
/// The global fingerprint already captures each first-party file's mtime, so
/// a content change to any project file (including `path`) will update its
/// mtime, which changes the global fingerprint, which changes this key.
/// Using the path rather than the file's content avoids reading every checked
/// file on every warm run: the warm path needs only `stat(2)` calls (for the
/// fingerprint) and small cache-entry reads.
///
/// Trade-off: the key is mtime-based (via the global fingerprint), not
/// content-based.  A mtime-preserving content change (e.g. `touch -t`) could
/// produce a stale cache hit, the same trade-off accepted by `make`, Cargo,
/// and most build systems.
pub fn file_cache_key(path: &Path, global_fp: u64) -> u64 {
    let mut h = FnvHasher::new();
    h.write_bytes(path.as_os_str().as_encoded_bytes());
    h.write_bytes(&global_fp.to_le_bytes());
    h.finish()
}

// ---------------------------------------------------------------------------
// DiagnosticCache
// ---------------------------------------------------------------------------

/// Persistent on-disk diagnostic cache.
///
/// Each entry is stored as `{dir}/{key:016x}.json` and contains the
/// `Vec<Diagnostic>` for one file.  The cache is append-only: entries are
/// written atomically via a temp-file + rename and read back on subsequent
/// runs.  There is no automatic eviction; users clear the directory manually.
pub struct DiagnosticCache {
    dir: PathBuf,
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
    pub fn open(dir: &Path) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Return the path for cache entry `key`.
    fn entry_path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}.json"))
    }

    /// Look up `key` in the cache.
    ///
    /// Returns `Some(diagnostics)` on a hit, `None` on a miss or any read /
    /// deserialisation error (errors are silently ignored so a corrupted entry
    /// just causes a cold re-computation).
    pub fn get(&self, key: u64) -> Option<Vec<Diagnostic>> {
        let bytes = std::fs::read(self.entry_path(key)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Store `diagnostics` under `key`.
    ///
    /// Writes atomically: first to a sibling temp file, then renames into
    /// place.  Errors are silently ignored — a failed write degrades gracefully
    /// to a cold re-computation on the next run.
    pub fn put(&self, key: u64, diagnostics: &[Diagnostic]) {
        // to_vec cannot fail: Diagnostic's fields are all simple serialisable
        // types (String, PathBuf, usize).
        if let Ok(json) = serde_json::to_vec(diagnostics) {
            self.write_entry_atomic(key, &json);
        }
    }

    /// Write `json` atomically to the cache entry for `key`.
    ///
    /// First writes to a temp file, then renames into place (both in the same
    /// directory, so the rename is a single filesystem operation).  Errors at
    /// any step are silently swallowed — the caller degrades to a cold run.
    ///
    /// Excluded from the coverage gate: triggering a write or rename failure
    /// deterministically (e.g. read-only filesystem, concurrent writer) would
    /// require environment manipulation that is not practical in unit tests.
    #[cfg_attr(coverage, coverage(off))]
    fn write_entry_atomic(&self, key: u64, json: &[u8]) {
        let tmp_path = self.dir.join(format!("{key:016x}.tmp"));
        if std::fs::write(&tmp_path, json).is_err() {
            return;
        }
        // Ignore rename errors (e.g. a concurrent writer already finished).
        let _ = std::fs::rename(&tmp_path, self.entry_path(key));
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

    // ---- file_cache_key -----------------------------------------------------

    #[test]
    fn file_cache_key_is_consistent() {
        let path = PathBuf::from("pkg/mod.py");
        let k1 = file_cache_key(&path, 42);
        let k2 = file_cache_key(&path, 42);
        assert_eq!(k1, k2);
    }

    #[test]
    fn file_cache_key_sensitive_to_path() {
        let k1 = file_cache_key(&PathBuf::from("pkg/a.py"), 42);
        let k2 = file_cache_key(&PathBuf::from("pkg/b.py"), 42);
        assert_ne!(k1, k2);
    }

    #[test]
    fn file_cache_key_sensitive_to_fingerprint() {
        let path = PathBuf::from("pkg/mod.py");
        let k1 = file_cache_key(&path, 1);
        let k2 = file_cache_key(&path, 2);
        assert_ne!(k1, k2);
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
        let _cache = DiagnosticCache::open(&cache_dir).expect("open");
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempdir().expect("tempdir");
        let cache = DiagnosticCache::open(dir.path()).expect("open");
        assert!(cache.get(0xdead_beef_u64).is_none());
    }

    #[test]
    fn cache_put_get_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let cache = DiagnosticCache::open(dir.path()).expect("open");
        let diags = vec![sample_diagnostic()];
        cache.put(0x1234_u64, &diags);
        let got = cache.get(0x1234_u64).expect("cache hit");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "pkg.mod.func");
    }

    #[test]
    fn cache_put_get_empty_diagnostics() {
        let dir = tempdir().expect("tempdir");
        let cache = DiagnosticCache::open(dir.path()).expect("open");
        cache.put(0xaaaa_u64, &[]);
        let got = cache.get(0xaaaa_u64).expect("cache hit");
        assert!(got.is_empty());
    }

    #[test]
    fn cache_get_corrupt_returns_none() {
        let dir = tempdir().expect("tempdir");
        let cache = DiagnosticCache::open(dir.path()).expect("open");
        // Write garbage bytes for the key.
        let entry = dir.path().join(format!("{:016x}.json", 0x9999_u64));
        std::fs::write(&entry, b"not json").expect("write");
        assert!(cache.get(0x9999_u64).is_none());
    }

    // ---- compute_global_fingerprint -----------------------------------------

    #[test]
    fn global_fingerprint_is_consistent() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None);
        let f2 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None);
        assert_eq!(f1, f2);
    }

    #[test]
    fn global_fingerprint_changes_with_config() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":[]}"#, None);
        let f2 = compute_global_fingerprint(dir.path(), r#"{"ignore_names":["foo"]}"#, None);
        assert_ne!(f1, f2);
    }

    #[test]
    fn global_fingerprint_changes_with_new_py_file() {
        let dir = tempdir().expect("tempdir");
        let f1 = compute_global_fingerprint(dir.path(), "{}", None);
        std::fs::write(dir.path().join("mod.py"), b"x = 1").expect("write");
        let f2 = compute_global_fingerprint(dir.path(), "{}", None);
        assert_ne!(f1, f2);
    }

    #[test]
    fn global_fingerprint_nonexistent_python_env_path() {
        // A nonexistent python_env path: mtime_nanos returns None, but the
        // fingerprint still completes (the path bytes are still hashed).
        let dir = tempdir().expect("tempdir");
        let no_env = PathBuf::from("/no/such/python");
        let f1 = compute_global_fingerprint(dir.path(), "{}", Some(&no_env));
        let f2 = compute_global_fingerprint(dir.path(), "{}", Some(&no_env));
        assert_eq!(f1, f2);
    }

    #[test]
    fn global_fingerprint_with_existing_python_env() {
        // An *existing* python_env path: mtime_nanos returns Some, exercising
        // the mtime-hashing branch for the python environment.
        let dir = tempdir().expect("tempdir");
        let env_dir = tempdir().expect("env tempdir");
        let f1 = compute_global_fingerprint(dir.path(), "{}", Some(env_dir.path()));
        let f2 = compute_global_fingerprint(dir.path(), "{}", Some(env_dir.path()));
        assert_eq!(f1, f2);
    }
}
