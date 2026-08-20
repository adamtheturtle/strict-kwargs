//! Persistent on-disk diagnostic cache (issue #68).
//!
//! Cached results are stored together in `{cache_dir}/diagnostics.json`.
//! The manifest records separate dependency and project fingerprints covering
//! everything that could affect the checker's output (tool version, config,
//! Python environment, ty binary, and all first-party source files).
//! Dependency changes invalidate the whole manifest. Project-only changes may
//! reuse entries after validating that every changed file's semantic token
//! stream is unchanged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::check::is_prunable_dir;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::resolve::{discover_site_packages, discover_site_packages_in_environment};

// ---------------------------------------------------------------------------
// FNV-1a 64-bit hasher
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash state.
///
/// Uses the standard FNV-1a basis and prime so the hashes are stable across
/// process restarts and platforms (no randomisation, fixed endianness via
/// `to_le_bytes`).
pub struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    /// FNV-1a 64-bit offset basis.
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Create a new hasher seeded with the FNV-1a 64-bit offset basis.
    pub(crate) const fn new() -> Self {
        Self { state: Self::BASIS }
    }

    /// Mix one byte into the hash.
    fn write_byte(&mut self, byte: u8) {
        self.state ^= u64::from(byte);
        self.state = self.state.wrapping_mul(Self::PRIME);
    }

    /// Mix a byte slice into the hash, length-prefixed to prevent
    /// `("ab","c")` colliding with `("a","bc")`.
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        // 8-byte LE length prefix disambiguates differently-split inputs.
        for b in (bytes.len() as u64).to_le_bytes() {
            self.write_byte(b);
        }
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Return the current hash value.
    pub(crate) const fn finish(self) -> u64 {
        self.state
    }
}

// ---------------------------------------------------------------------------
// Cache fingerprints
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FingerprintFile {
    path: PathBuf,
    mtime: Option<[u8; 16]>,
    #[serde(default)]
    content: Option<u64>,
}

impl FingerprintFile {
    pub fn from_path(path: PathBuf) -> Self {
        let mtime = mtime_nanos(&path);
        let content = content_fingerprint(&path);
        Self {
            path,
            mtime,
            content,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Independent cache-invalidation domains.
///
/// A dependency change can alter every result and always invalidates the
/// manifest. A project-only change may retain entries when every changed file
/// has the same semantic token stream as before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheFingerprints {
    dependency: u64,
    project: u64,
}

impl CacheFingerprints {
    #[cfg(test)]
    fn combined(self) -> u64 {
        let mut h = FnvHasher::new();
        h.write_bytes(&self.dependency.to_le_bytes());
        h.write_bytes(&self.project.to_le_bytes());
        h.finish()
    }
}

fn hash_fingerprint_files(files: &[FingerprintFile], h: &mut FnvHasher) {
    for file in files {
        h.write_bytes(file.path.as_os_str().as_encoded_bytes());
        if let Some(mtime) = file.mtime {
            h.write_bytes(&mtime);
        }
        if let Some(content) = file.content {
            h.write_bytes(&content.to_le_bytes());
        }
    }
}

fn content_fingerprint(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = FnvHasher::new();
    h.write_bytes(&bytes);
    Some(h.finish())
}

fn hash_path_content(path: &Path, h: &mut FnvHasher) {
    if let Some(content) = content_fingerprint(path) {
        h.write_bytes(&content.to_le_bytes());
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
    hash_path_content(&ty_path, h);
}

/// Collect all first-party `.py`/`.pyi` files under `root`, then mix each
/// file's path, mtime, and content digest into `h`.
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
        hash_path_content(path, h);
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
/// - `python_env` path + metadata/content (if provided)
/// - `ty` binary path + metadata/content (located via `PATH`)
/// - every `.py`/`.pyi` file under `project_root`, sorted by path, each
///   contributing its canonical path bytes, mtime, and content digest
/// - every configured first-party source root, including absolute roots
///   outside `project_root`
/// - every `.py`/`.pyi` file in automatically discovered or explicitly
///   selected `site-packages`
///
/// Content digests make cache correctness independent of timestamp behavior:
/// editors, archive extraction, and copy tools may legitimately preserve or
/// restore mtimes while changing bytes.
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
#[cfg(test)]
pub fn compute_global_fingerprint_with_project_files(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
    first_party_roots: &[PathBuf],
    project_files: Option<&[FingerprintFile]>,
) -> u64 {
    compute_cache_fingerprints_with_project_files(
        project_root,
        config_json,
        python_env,
        first_party_roots,
        project_files,
    )
    .combined()
}

/// Compute dependency and project fingerprints separately so a project-only
/// change can be validated against the previous semantic token streams.
pub fn compute_cache_fingerprints_with_project_files(
    project_root: &Path,
    config_json: &str,
    python_env: Option<&Path>,
    first_party_roots: &[PathBuf],
    project_files: Option<&[FingerprintFile]>,
) -> CacheFingerprints {
    let mut dependency = FnvHasher::new();

    // Tool version — changing the binary invalidates all cached results.
    dependency.write_bytes(env!("CARGO_PKG_VERSION").as_bytes());

    // Serialised config.
    dependency.write_bytes(config_json.as_bytes());

    // Python environment path + mtime.
    if let Some(env_path) = python_env {
        dependency.write_bytes(env_path.as_os_str().as_encoded_bytes());
        if let Some(mtime) = mtime_nanos(env_path) {
            dependency.write_bytes(&mtime);
        }
        hash_path_content(env_path, &mut dependency);
    }

    // `ty` binary path + mtime.
    hash_ty_binary(&mut dependency);

    let mut project = FnvHasher::new();
    for root in fingerprint_walk_roots(project_root, first_party_roots) {
        if root == project_root {
            project.write_bytes(root.as_os_str().as_encoded_bytes());
            if let Some(project_files) = project_files {
                hash_fingerprint_files(project_files, &mut project);
                continue;
            }
            hash_py_file_mtimes(&root, &mut project);
            continue;
        }
        // A configured root outside the ordinary project walk behaves like a
        // dependency: it is not necessarily among the checked files whose
        // entries can be selectively refreshed.
        dependency.write_bytes(root.as_os_str().as_encoded_bytes());
        hash_py_file_mtimes(&root, &mut dependency);
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
        dependency.write_bytes(root.as_os_str().as_encoded_bytes());
        hash_py_file_mtimes(&root, &mut dependency);
    }

    CacheFingerprints {
        dependency: dependency.finish(),
        project: project.finish(),
    }
}

// ---------------------------------------------------------------------------
// DiagnosticCache
// ---------------------------------------------------------------------------

const MANIFEST_NAME: &str = "diagnostics.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Deserialize, Serialize)]
struct CacheEntry {
    diagnostics: Vec<CachedDiagnostic>,
    /// `(line, column)` of each `KW002` unused-directive diagnostic. Kept in
    /// its own list so the far more common `KW001` entries stay a flat tuple,
    /// and so a manifest written before `KW002` existed still loads.
    #[serde(default)]
    unused_noqa: Vec<(usize, usize)>,
    semantic_fingerprint: Option<u64>,
}

impl CacheEntry {
    fn to_diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .iter()
            .map(|diagnostic| diagnostic.to_diagnostic(path))
            .chain(unused_noqa_diagnostics(path, &self.unused_noqa))
            .collect()
    }

    fn into_diagnostics(self, path: &Path) -> Vec<Diagnostic> {
        self.diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.into_diagnostic(path))
            .chain(unused_noqa_diagnostics(path, &self.unused_noqa))
            .collect()
    }
}

/// Rebuild one file's `KW002` diagnostics from their cached positions.
fn unused_noqa_diagnostics<'a>(
    path: &'a Path,
    positions: &'a [(usize, usize)],
) -> impl Iterator<Item = Diagnostic> + 'a {
    positions
        .iter()
        .map(|&(line, column)| Diagnostic::unused_noqa(path.to_path_buf(), line, column))
}

/// The positions of one file's `KW002` diagnostics, which the entry stores
/// apart from its `KW001` ones.
fn unused_noqa_positions<'a>(
    diagnostics: impl IntoIterator<Item = &'a Diagnostic>,
) -> Vec<(usize, usize)> {
    diagnostics
        .into_iter()
        .filter(|diagnostic| matches!(diagnostic.kind, DiagnosticKind::UnusedNoqa))
        .map(|diagnostic| (diagnostic.line, diagnostic.column))
        .collect()
}

/// On-disk `KW001` diagnostic fields shared with the containing entry's path.
///
/// A large project can have hundreds of diagnostics per file, so storing the
/// path once in the entry avoids repeating the same absolute string in every
/// diagnostic. Tuple fields are line, column, callee, positional count, and
/// maximum positional count, in that order.
#[derive(Clone, Deserialize, Serialize)]
struct CachedDiagnostic(usize, usize, String, usize, usize);

impl CachedDiagnostic {
    /// The cacheable form of a `KW001` diagnostic; `None` for other rules,
    /// which the entry stores in their own list.
    #[cfg(test)]
    fn from_diagnostic(diagnostic: &Diagnostic) -> Option<Self> {
        BorrowedCachedDiagnostic::from_diagnostic(diagnostic).map(|borrowed| {
            Self(
                borrowed.0,
                borrowed.1,
                borrowed.2.to_owned(),
                borrowed.3,
                borrowed.4,
            )
        })
    }

    fn into_diagnostic(self, path: &Path) -> Diagnostic {
        Diagnostic::too_many_positional(path.to_path_buf(), self.0, self.1, self.2, self.3, self.4)
    }

    fn to_diagnostic(&self, path: &Path) -> Diagnostic {
        self.clone().into_diagnostic(path)
    }
}

/// A diagnostic serialized straight from the final result vector.
///
/// The cache write is terminal, so borrowing the callee avoids cloning every
/// string merely to encode it and then immediately drop the clone.
#[derive(Serialize)]
struct BorrowedCachedDiagnostic<'a>(usize, usize, &'a str, usize, usize);

impl<'a> BorrowedCachedDiagnostic<'a> {
    /// The cacheable form of a `KW001` diagnostic; `None` for other rules,
    /// which are stored in their own list.
    fn from_diagnostic(diagnostic: &'a Diagnostic) -> Option<Self> {
        match &diagnostic.kind {
            DiagnosticKind::TooManyPositional {
                callee,
                positional_count,
                max_positional,
            } => Some(Self(
                diagnostic.line,
                diagnostic.column,
                callee,
                *positional_count,
                *max_positional,
            )),
            DiagnosticKind::UnusedNoqa => None,
        }
    }
}

#[derive(Serialize)]
struct BorrowedCacheEntry<'a> {
    diagnostics: Vec<BorrowedCachedDiagnostic<'a>>,
    unused_noqa: Vec<(usize, usize)>,
    semantic_fingerprint: Option<u64>,
}

#[derive(Deserialize)]
struct CacheManifest {
    dependency_fingerprint: u64,
    project_fingerprint: u64,
    project_files: Option<Vec<FingerprintFile>>,
    entries: Vec<(PathBuf, CacheEntry)>,
}

/// Persistent bounded on-disk diagnostic cache.
///
/// The complete manifest is read at most once and written at most once per
/// invocation. Replacing the single manifest naturally evicts results made
/// obsolete by changed dependency or project fingerprints.
pub struct DiagnosticCache {
    dir: PathBuf,
    fingerprints: CacheFingerprints,
    project_files: Option<Vec<FingerprintFile>>,
    previous_project_files: Option<Vec<FingerprintFile>>,
    entries: BTreeMap<PathBuf, CacheEntry>,
    needs_project_validation: bool,
    dirty: bool,
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
    pub fn open(
        dir: &Path,
        fingerprints: CacheFingerprints,
        project_files: Option<&[FingerprintFile]>,
    ) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(dir)?;
        let manifest = std::fs::read(dir.join(MANIFEST_NAME))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheManifest>(&bytes).ok())
            .filter(|manifest| manifest.dependency_fingerprint == fingerprints.dependency)
            .filter(|manifest| {
                manifest.project_fingerprint == fingerprints.project || project_files.is_some()
            })
            .map(|manifest| {
                let CacheManifest {
                    dependency_fingerprint: _,
                    project_fingerprint,
                    project_files: previous_project_files,
                    entries,
                } = manifest;
                let entries = entries
                    .into_iter()
                    .map(|(path, mut entry)| {
                        entry
                            .diagnostics
                            .sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
                        (path, entry)
                    })
                    .collect::<BTreeMap<_, _>>();
                (project_fingerprint, previous_project_files, entries)
            });
        let (entries, previous_project_files, needs_project_validation) = manifest.map_or_else(
            || (BTreeMap::new(), None, false),
            |(project_fingerprint, previous_project_files, entries)| {
                let changed = project_fingerprint != fingerprints.project;
                (entries, previous_project_files, changed)
            },
        );
        Ok(Self {
            dir: dir.to_path_buf(),
            fingerprints,
            project_files: project_files.map(<[FingerprintFile]>::to_vec),
            previous_project_files,
            entries,
            needs_project_validation,
            dirty: false,
        })
    }

    /// Whether project sources changed while tool/config/dependency inputs
    /// stayed stable, allowing reparsed token streams to validate reuse.
    pub const fn needs_project_validation(&self) -> bool {
        self.needs_project_validation
    }

    /// Validate stale project entries against the rebuilt project's tokens.
    ///
    /// Entries are reusable only when every changed existing file has the same
    /// semantic token stream as before. Changed files themselves are rescanned
    /// so layout-sensitive diagnostic positions stay current.
    pub fn validate_project(
        &mut self,
        current_files: &[PathBuf],
        current_semantic_fingerprints: &BTreeMap<PathBuf, u64>,
    ) -> bool {
        let current_metadata: BTreeMap<_, _> = self
            .project_files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|file| (file.path.clone(), (file.mtime, file.content)))
            .collect();
        let old_metadata: BTreeMap<_, _> = self
            .previous_project_files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|file| (file.path.clone(), (file.mtime, file.content)))
            .collect();
        let same_paths = old_metadata.len() == current_metadata.len()
            && old_metadata
                .keys()
                .all(|path| current_metadata.contains_key(path));
        let semantically_unchanged = same_paths
            && current_metadata.iter().all(|(path, metadata)| {
                old_metadata.get(path) == Some(metadata)
                    || self.entries.get(path).is_some_and(|entry| {
                        entry.semantic_fingerprint
                            == current_semantic_fingerprints.get(path).copied()
                    })
            });
        let reusable = semantically_unchanged;
        if reusable {
            let current_paths: std::collections::BTreeSet<_> =
                current_files.iter().map(PathBuf::as_path).collect();
            self.entries.retain(|path, _entry| {
                current_paths.contains(path.as_path())
                    && old_metadata.get(path) == current_metadata.get(path)
            });
        } else {
            self.entries.clear();
        }
        self.previous_project_files = self.project_files.clone();
        self.needs_project_validation = false;
        self.dirty = true;
        reusable
    }

    /// Persist metadata-only refreshes when a validated project change leaves
    /// no files to scan.
    #[cfg_attr(coverage, coverage(off))]
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(json) = self.serialize_manifest() {
            self.write_manifest_atomic(&json);
            self.dirty = false;
        }
    }

    /// Return cached diagnostics for one successfully checked file.
    pub fn get(&self, path: &Path) -> Option<Vec<Diagnostic>> {
        self.entries
            .get(path)
            .map(|entry| entry.to_diagnostics(path))
    }

    /// Whether `path` has a successfully checked cache entry.
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Move one successfully checked entry out of this invocation's cache.
    ///
    /// Used only by terminal warm-cache paths that will not rewrite the
    /// manifest, avoiding a clone of every diagnostic before returning.
    pub fn take(&mut self, path: &Path) -> Option<Vec<Diagnostic>> {
        self.entries
            .remove(path)
            .map(|entry| entry.into_diagnostics(path))
    }

    /// Add successfully checked-file results and write the manifest once.
    ///
    /// Errors are silently ignored: a failed cache write only causes a cold
    /// recomputation on the next run.
    #[cfg_attr(coverage, coverage(off))]
    #[cfg(test)]
    pub fn put_all(&mut self, entries: Vec<(PathBuf, Vec<Diagnostic>, Option<u64>)>) {
        if entries.is_empty() {
            self.flush();
            return;
        }
        self.entries.extend(entries.into_iter().map(
            |(path, diagnostics, semantic_fingerprint)| {
                let entry = CacheEntry {
                    diagnostics: diagnostics
                        .iter()
                        .filter_map(CachedDiagnostic::from_diagnostic)
                        .collect(),
                    unused_noqa: unused_noqa_positions(&diagnostics),
                    semantic_fingerprint,
                };
                (path, entry)
            },
        ));
        self.dirty = true;
        self.flush();
    }

    /// Write new results by borrowing the caller's final diagnostic vector.
    ///
    /// This terminal fast path does not retain the new entries in memory.
    /// Existing hits are included in the replacement manifest, while new
    /// diagnostic strings are serialized without first cloning them into
    /// cache-owned entries.
    #[cfg_attr(coverage, coverage(off))]
    pub fn put_all_borrowed(&mut self, entries: &[(PathBuf, Vec<&Diagnostic>, Option<u64>)]) {
        if entries.is_empty() {
            self.flush();
            return;
        }

        let mut encoded: Vec<(PathBuf, Vec<u8>)> = self
            .entries
            .par_iter()
            .filter_map(|(path, entry)| {
                serde_json::to_vec(&(path, entry))
                    .ok()
                    .map(|json| (path.clone(), json))
            })
            .collect();
        let mut new_encoded: Vec<(PathBuf, Vec<u8>)> = entries
            .par_iter()
            .filter_map(|(path, diagnostics, semantic_fingerprint)| {
                let entry = BorrowedCacheEntry {
                    diagnostics: diagnostics
                        .iter()
                        .copied()
                        .filter_map(BorrowedCachedDiagnostic::from_diagnostic)
                        .collect(),
                    unused_noqa: unused_noqa_positions(diagnostics.iter().copied()),
                    semantic_fingerprint: *semantic_fingerprint,
                };
                serde_json::to_vec(&(path, entry))
                    .ok()
                    .map(|json| (path.clone(), json))
            })
            .collect();
        if new_encoded.len() != entries.len() || encoded.len() != self.entries.len() {
            return;
        }
        encoded.append(&mut new_encoded);
        encoded.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let encoded = encoded
            .into_iter()
            .map(|(_path, json)| json)
            .collect::<Vec<_>>();
        if let Some(json) = self.serialize_encoded_manifest(&encoded) {
            self.write_manifest_atomic(&json);
            self.dirty = false;
        }
    }

    /// Serialize each per-file entry in parallel, then join the already-valid
    /// JSON fragments into the single on-disk manifest.
    #[cfg_attr(coverage, coverage(off))]
    fn serialize_manifest(&self) -> Option<Vec<u8>> {
        let entries: Vec<_> = self.entries.iter().collect();
        let encoded: Vec<Vec<u8>> = entries
            .par_iter()
            .map(|(path, entry)| serde_json::to_vec(&(path, entry)))
            .collect::<Result<_, _>>()
            .ok()?;
        self.serialize_encoded_manifest(&encoded)
    }

    fn serialize_encoded_manifest(&self, encoded: &[Vec<u8>]) -> Option<Vec<u8>> {
        let project_files = serde_json::to_vec(&self.project_files).ok()?;
        let payload_len: usize = encoded.iter().map(Vec::len).sum();
        let dependency_fingerprint = self.fingerprints.dependency.to_string();
        let project_fingerprint = self.fingerprints.project.to_string();
        let mut json = Vec::with_capacity(
            payload_len
                + encoded.len()
                + project_files.len()
                + dependency_fingerprint.len()
                + project_fingerprint.len()
                + 100,
        );
        json.extend_from_slice(b"{\"dependency_fingerprint\":");
        json.extend_from_slice(dependency_fingerprint.as_bytes());
        json.extend_from_slice(b",\"project_fingerprint\":");
        json.extend_from_slice(project_fingerprint.as_bytes());
        json.extend_from_slice(b",\"project_files\":");
        json.extend_from_slice(&project_files);
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
        Diagnostic::too_many_positional(
            PathBuf::from("pkg/mod.py"),
            3,
            1,
            "pkg.mod.func".to_string(),
            3,
            1,
        )
    }

    fn open_cache(dir: &Path, fingerprint: u64) -> DiagnosticCache {
        DiagnosticCache::open(
            dir,
            CacheFingerprints {
                dependency: fingerprint,
                project: fingerprint,
            },
            None,
        )
        .expect("open cache")
    }

    fn fingerprint_file(path: &str, marker: u8) -> FingerprintFile {
        FingerprintFile {
            path: PathBuf::from(path),
            mtime: Some([marker; 16]),
            content: Some(u64::from(marker)),
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
                content: None,
            }],
            &mut actual,
        );

        let mut expected = FnvHasher::new();
        expected.write_bytes(path.as_os_str().as_encoded_bytes());
        assert_eq!(actual.finish(), expected.finish());
    }

    #[test]
    fn fingerprint_content_changes_with_preserved_mtime() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("main.py");
        std::fs::write(&path, "f(1)\n").expect("write old source");
        let old = FingerprintFile::from_path(path.clone());
        std::fs::write(&path, "f(x)\n").expect("write new source");
        let mut new = FingerprintFile::from_path(path);
        new.mtime = old.mtime;

        let mut old_hash = FnvHasher::new();
        hash_fingerprint_files(&[old], &mut old_hash);
        let mut new_hash = FnvHasher::new();
        hash_fingerprint_files(&[new], &mut new_hash);
        assert_ne!(old_hash.finish(), new_hash.finish());
    }

    // ---- DiagnosticCache ----------------------------------------------------

    #[test]
    fn cache_open_creates_directory() {
        let base = tempdir().expect("tempdir");
        let cache_dir = base.path().join("nested").join("cache");
        let _cache = open_cache(&cache_dir, 1);
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempdir().expect("tempdir");
        let cache = open_cache(dir.path(), 1);
        assert!(cache.get(Path::new("missing.py")).is_none());
    }

    #[test]
    fn cache_put_all_get_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut later = sample_diagnostic();
        later.line = 8;
        let mut earlier = sample_diagnostic();
        earlier.line = 2;
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all(vec![(path.clone(), vec![later, earlier], None)]);

        let mut cache = open_cache(dir.path(), 1);
        assert!(cache.contains(&path));
        let got = cache.take(&path).expect("cache hit");
        assert_eq!(
            got.iter()
                .map(|diagnostic| diagnostic.line)
                .collect::<Vec<_>>(),
            [2, 8]
        );
        assert_eq!(got[0].callee(), Some("pkg.mod.func"));
        assert!(!cache.contains(&path));
        assert!(cache.take(&path).is_none());
    }

    #[test]
    fn borrowed_cache_write_matches_owned_manifest() {
        let owned_dir = tempdir().expect("owned tempdir");
        let borrowed_dir = tempdir().expect("borrowed tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut later = sample_diagnostic();
        later.line = 8;
        let mut earlier = sample_diagnostic();
        earlier.line = 2;
        let diagnostics = vec![later, earlier];

        let mut owned = open_cache(owned_dir.path(), 1);
        owned.put_all(vec![(path.clone(), diagnostics.clone(), Some(42))]);
        let mut borrowed = open_cache(borrowed_dir.path(), 1);
        borrowed.put_all_borrowed(&[(path, diagnostics.iter().collect(), Some(42))]);

        let owned_manifest =
            std::fs::read(owned_dir.path().join(MANIFEST_NAME)).expect("read owned manifest");
        let borrowed_manifest =
            std::fs::read(borrowed_dir.path().join(MANIFEST_NAME)).expect("read borrowed manifest");
        assert_eq!(borrowed_manifest, owned_manifest);
    }

    #[test]
    fn borrowed_cache_write_preserves_existing_entries() {
        let dir = tempdir().expect("tempdir");
        let old_path = PathBuf::from("old.py");
        let new_path = PathBuf::from("new.py");
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all_borrowed(&[(old_path.clone(), Vec::new(), Some(10))]);

        let new_diagnostics = [sample_diagnostic()];
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all_borrowed(&[(new_path.clone(), new_diagnostics.iter().collect(), Some(20))]);

        let cache = open_cache(dir.path(), 1);
        assert!(cache.contains(&old_path));
        assert_eq!(cache.get(&new_path).expect("new cache entry").len(), 1);
    }

    #[test]
    fn cache_put_all_records_clean_files() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("clean.py");
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all(vec![(path.clone(), Vec::new(), None)]);

        let cache = open_cache(dir.path(), 1);
        let got = cache.get(&path).expect("cache hit");
        assert_eq!(got.len(), 0);
    }

    #[test]
    fn cache_get_corrupt_returns_none() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_NAME), b"not json").expect("write");
        let cache = open_cache(dir.path(), 1);
        assert!(cache.get(Path::new("mod.py")).is_none());
    }

    #[test]
    fn cache_stores_diagnostic_path_once_per_entry() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all(vec![(
            path.clone(),
            vec![sample_diagnostic(), sample_diagnostic()],
            None,
        )]);

        let cache = open_cache(dir.path(), 1);
        let diagnostics = cache.get(&path).expect("cache hit");
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().all(|diagnostic| diagnostic.path == path));

        let manifest =
            std::fs::read_to_string(dir.path().join(MANIFEST_NAME)).expect("read manifest");
        assert_eq!(manifest.matches("pkg/mod.py").count(), 1);
    }

    #[test]
    fn cache_fingerprint_mismatch_invalidates_manifest() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("pkg/mod.py");
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all(vec![(path.clone(), vec![sample_diagnostic()], None)]);

        let cache = open_cache(dir.path(), 2);
        assert!(cache.get(&path).is_none());
    }

    #[test]
    fn project_validation_keeps_entries_after_semantically_unchanged_edit() {
        let dir = tempdir().expect("tempdir");
        let safe = PathBuf::from("safe.py");
        let changed = PathBuf::from("changed.py");
        let old_files = vec![
            fingerprint_file("changed.py", 1),
            fingerprint_file("safe.py", 1),
        ];
        let old_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 20,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), old_fingerprints, Some(&old_files)).expect("open");
        cache.put_all(vec![
            (safe.clone(), Vec::new(), Some(100)),
            (changed.clone(), Vec::new(), Some(200)),
        ]);

        let new_files = vec![
            fingerprint_file("changed.py", 2),
            fingerprint_file("safe.py", 1),
        ];
        let new_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 21,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), new_fingerprints, Some(&new_files)).expect("reopen");
        assert!(cache.needs_project_validation());
        let current_semantics = BTreeMap::from([(safe.clone(), 100), (changed.clone(), 200)]);
        assert!(cache.validate_project(&[safe.clone(), changed.clone()], &current_semantics));
        assert!(cache.contains(&safe));
        assert!(!cache.contains(&changed));

        cache.flush();
        let cache =
            DiagnosticCache::open(dir.path(), new_fingerprints, Some(&new_files)).expect("reopen");
        assert!(cache.contains(&safe));
        assert!(!cache.needs_project_validation());
    }

    #[test]
    fn project_validation_discards_entries_when_project_paths_change() {
        let dir = tempdir().expect("tempdir");
        let old_path = PathBuf::from("old.py");
        let old_files = vec![fingerprint_file("old.py", 1)];
        let old_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 20,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), old_fingerprints, Some(&old_files)).expect("open");
        cache.put_all(vec![(old_path.clone(), Vec::new(), Some(100))]);

        let new_path = PathBuf::from("new.py");
        let new_files = vec![fingerprint_file("old.py", 1), fingerprint_file("new.py", 1)];
        let new_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 21,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), new_fingerprints, Some(&new_files)).expect("reopen");
        assert!(!cache.validate_project(
            &[old_path.clone(), new_path.clone()],
            &BTreeMap::from([(old_path.clone(), 100), (new_path, 200)])
        ));
        assert!(!cache.contains(&old_path));
    }

    #[test]
    fn project_validation_drops_entries_outside_current_selection() {
        let dir = tempdir().expect("tempdir");
        let selected = PathBuf::from("selected.py");
        let unselected = PathBuf::from("unselected.py");
        let old_files = vec![
            fingerprint_file("selected.py", 1),
            fingerprint_file("unselected.py", 1),
        ];
        let old_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 20,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), old_fingerprints, Some(&old_files)).expect("open");
        cache.put_all(vec![
            (selected.clone(), Vec::new(), Some(100)),
            (unselected.clone(), Vec::new(), Some(200)),
        ]);

        let new_files = vec![
            fingerprint_file("selected.py", 2),
            fingerprint_file("unselected.py", 1),
        ];
        let new_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 21,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), new_fingerprints, Some(&new_files)).expect("reopen");
        assert!(cache.validate_project(
            std::slice::from_ref(&selected),
            &BTreeMap::from([(selected.clone(), 100), (unselected.clone(), 200)])
        ));
        assert!(!cache.contains(&selected));
        assert!(!cache.contains(&unselected));
    }

    #[test]
    fn semantic_source_change_discards_every_project_entry() {
        let dir = tempdir().expect("tempdir");
        let path = PathBuf::from("mod.py");
        let old_files = vec![fingerprint_file("mod.py", 1)];
        let old_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 20,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), old_fingerprints, Some(&old_files)).expect("open");
        cache.put_all(vec![(path.clone(), Vec::new(), Some(100))]);

        let new_files = vec![fingerprint_file("mod.py", 2)];
        let new_fingerprints = CacheFingerprints {
            dependency: 10,
            project: 21,
        };
        let mut cache =
            DiagnosticCache::open(dir.path(), new_fingerprints, Some(&new_files)).expect("reopen");
        assert!(!cache.validate_project(
            std::slice::from_ref(&path),
            &BTreeMap::from([(path.clone(), 101)])
        ));
        assert!(!cache.contains(&path));
    }

    #[test]
    fn cache_file_selection_mismatch_is_a_miss() {
        let dir = tempdir().expect("tempdir");
        let mut cache = open_cache(dir.path(), 1);
        cache.put_all(vec![(PathBuf::from("a.py"), Vec::new(), None)]);

        let cache = open_cache(dir.path(), 1);
        assert!(cache.get(Path::new("b.py")).is_none());
    }

    #[test]
    fn cache_replacement_keeps_storage_bounded() {
        let dir = tempdir().expect("tempdir");
        for fingerprint in 0..20 {
            let mut cache = open_cache(dir.path(), fingerprint);
            cache.put_all(vec![(
                PathBuf::from("mod.py"),
                vec![sample_diagnostic()],
                None,
            )]);
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
