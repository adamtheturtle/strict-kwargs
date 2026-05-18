//! Check Python sources for positional calls that should use keywords.

use std::path::{Path, PathBuf};

use rayon::prelude::*;
use ruff_python_ast::token::{parenthesized_range, Tokens};
use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::Expr;
use ruff_python_ast::{self as ast};
use ruff_python_ast::{AnyNodeRef, ExprRef, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;

use ruff_text_size::TextSize;

use crate::ast_util::{line_column, positional_argument_count, signature_from_parameters};
use crate::cache::{compute_global_fingerprint, file_cache_key, DiagnosticCache};
use crate::config::Config;
use crate::diagnostic::Diagnostic;
use crate::error::CheckError;
use crate::fix::{apply_insertions, FileFix, FixOutcome, Insertion};
use crate::index::{
    build_index, is_package_init, module_name_for_path, relative_base, DefinitionIndex,
};
use crate::limits::{parse_module_guarded, run_with_large_stack, with_large_stack_pool};
use crate::signature::{ParameterKind, Signature};
use crate::source::{read_python_source, Source};
use crate::ty_resolver::{
    byte_offset_to_lsp, location_from_value, lsp_to_byte_offset, parse_callable_type_overloads,
    parse_hover_signature, same_path, ty_binary_present, TyResolver,
};

#[derive(Clone, Copy)]
enum IfBranchTraversal {
    Module,
    LocalBody,
}

/// Check every Python file reachable from `paths` and return the violations.
///
/// # Errors
///
/// Returns [`CheckError`] if a path argument does not exist
/// ([`CheckError::PathNotFound`]), a source file cannot be read or parsed,
/// or the required `ty` backend is missing ([`CheckError::TyNotFound`]) or
/// its server cannot start ([`CheckError::TyServerFailed`]).
///
/// The whole walk runs on a large dedicated stack so a deeply nested file
/// cannot overflow it; one nested deeper than the supported limit is rejected
/// up front ([`CheckError::TooDeeplyNested`]) instead of crashing (issue #54).
pub fn check_paths(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<Vec<Diagnostic>, CheckError> {
    run_with_large_stack(move || {
        check_paths_impl(project_root, paths, config, python_env, cache_dir)
    })
}

/// Per-file entry used in `check_paths_impl` to track cache state.
///
/// Populated before the parallel scan pass: cache hits carry the previously
/// stored diagnostics; misses are fed to the pipeline.
struct FileEntry {
    cache_hit: Option<Vec<Diagnostic>>,
}

/// Phase 2 processing for one completed file: route the [`ScanOutcome`] to
/// either the skip-warning list ([`ScanOutcome::Skipped`]) or the ty resolver
/// ([`ScanOutcome::Scanned`]).
///
/// This is the gated business-logic counterpart to [`pipeline_phases`], which
/// handles the non-deterministic threading orchestration that cannot be covered.
#[allow(clippy::too_many_arguments)]
fn process_scan_outcome_for_ty(
    i: usize,
    path: PathBuf,
    outcome: ScanOutcome,
    ty: &mut Option<TyResolver>,
    ty_start_attempted: &mut bool,
    project_root: &Path,
    python_env: Option<&Path>,
    ty_file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    diagnostics: &mut Vec<Diagnostic>,
    skip_warnings: &mut Vec<(usize, PathBuf, String)>,
) -> Result<(), CheckError> {
    match outcome {
        ScanOutcome::Skipped(reason) => {
            // Collect skip warnings with their file index so they can be
            // emitted in the original sorted-file order after both phases
            // finish (issue #53 + #46).
            skip_warnings.push((i, path, reason));
        }
        ScanOutcome::Scanned(scan) => {
            diagnostics.extend(scan.diagnostics);
            if !scan.pending.is_empty() {
                resolve_file_with_ty(
                    ty,
                    ty_start_attempted,
                    project_root,
                    python_env,
                    &path,
                    &scan.source,
                    &scan.pending,
                    ty_file_cache,
                    diagnostics,
                    None,
                )?;
            }
        }
    }
    Ok(())
}

/// Pipeline phases 1 and 2 (issue #67): stream [`ScanOutcome`]s from parallel
/// Phase 1 workers to the serial Phase 2 ty consumer as each file's built-in
/// pass finishes, overlapping the remaining Phase 1 work with early Phase 2
/// ty round-trips. The final sort in [`check_paths_impl`] keeps output
/// deterministic regardless of arrival order; the lazy ty-server start is
/// preserved (only the first file with pending calls triggers it).
///
/// Excluded from the coverage gate for the same reason as
/// [`stream_scan_files`]: what is excluded here is only the threading
/// orchestration — the environment-only pool-construction failure, the
/// scheduling-dependent drain path, a scan error arriving from a worker, and
/// the unreachable thread-panic arm. The per-outcome business logic lives in
/// the gated [`process_scan_outcome_for_ty`].
#[cfg_attr(coverage, coverage(off))]
#[allow(clippy::too_many_arguments)]
fn pipeline_phases(
    python_files: &[PathBuf],
    project_root: &Path,
    config: &Config,
    index: &DefinitionIndex,
    python_env: Option<&Path>,
    ty: &mut Option<TyResolver>,
    ty_start_attempted: &mut bool,
    ty_file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    diagnostics: &mut Vec<Diagnostic>,
    skip_warnings: &mut Vec<(usize, PathBuf, String)>,
) -> Result<(), CheckError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut consumer_err: Option<CheckError> = None;

    let scan_result = std::thread::scope(|scope| -> Result<(), CheckError> {
        // Phase 1 (parallel, background): the built-in pass over every file.
        // Each file is an independent, pure-CPU unit of work sharing only the
        // `Sync` demand-driven index; results are sent to `rx` as each worker
        // finishes rather than being collected all at once. `tx` is moved in
        // and dropped when all workers finish, closing the channel.
        //
        // The coordinator thread only needs an explicit stack on platforms
        // with small default thread stacks. On glibc Linux this keeps the
        // hot benchmark path on the low-overhead `scope.spawn` implementation.
        #[cfg(any(target_env = "musl", windows))]
        let scan_handle = std::thread::Builder::new()
            .stack_size(crate::limits::STACK_SIZE)
            .spawn_scoped(scope, || {
                stream_scan_files(python_files, project_root, config, index, tx)
            })
            .map_err(CheckError::Io)?;
        #[cfg(not(any(target_env = "musl", windows)))]
        let scan_handle =
            scope.spawn(|| stream_scan_files(python_files, project_root, config, index, tx));

        for (i, path, result) in rx {
            if consumer_err.is_some() {
                // A ty or scan error has already been recorded; drain the
                // remaining items so the background thread can finish.
                continue;
            }
            let outcome = match result {
                Ok(o) => o,
                Err(e) => {
                    consumer_err = Some(e);
                    continue;
                }
            };
            if let Err(e) = process_scan_outcome_for_ty(
                i,
                path,
                outcome,
                ty,
                ty_start_attempted,
                project_root,
                python_env,
                ty_file_cache,
                diagnostics,
                skip_warnings,
            ) {
                consumer_err = Some(e);
            }
        }

        match scan_handle.join() {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    });
    scan_result?;
    if let Some(e) = consumer_err {
        return Err(e);
    }
    Ok(())
}

fn check_paths_impl(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<Vec<Diagnostic>, CheckError> {
    // `ty` is a hard requirement. Verify it up front — before reading or
    // parsing anything — so the outcome is deterministic and independent of
    // file content: a codebase the built-in resolver fully handles still
    // errors if `ty` is missing, so the same source can never resolve fewer
    // calls on a machine that merely lacks `ty`.
    require_ty_present()?;
    let python_files = collect_python_files(paths)?;
    let index = build_index(project_root, &python_files)?;

    // Optional persistent cache: open it and compute the global fingerprint once.
    let cache_and_fp: Option<(DiagnosticCache, u64)> = cache_dir
        .map(|dir| -> Result<_, CheckError> {
            let config_json = serde_json::to_string(config).unwrap_or_default();
            let fp = compute_global_fingerprint(project_root, &config_json, python_env);
            Ok((DiagnosticCache::open(dir)?, fp))
        })
        .transpose()?;

    // Partition files into cache hits and misses. Hits bypass the pipeline;
    // misses are queued for scanning. `files_to_scan` preserves the order of
    // misses so the pipeline's file indices map consistently to skip_warnings.
    // `cache_miss_keys` pairs each miss with its cache key for writing back
    // after the pipeline; stored separately so the write loop needs no Option.
    let mut entries: Vec<FileEntry> = Vec::with_capacity(python_files.len());
    let mut files_to_scan: Vec<PathBuf> = Vec::new();
    let mut cache_miss_keys: Vec<(u64, PathBuf)> = Vec::new();

    for path in &python_files {
        if let Some((ref cache, fp)) = cache_and_fp {
            // The cache key is derived from the path and the global
            // fingerprint. The global fingerprint already includes every
            // first-party file's mtime, so any content change (which updates
            // the mtime) changes the fingerprint and therefore this key.
            // This avoids reading the file twice (once here, once in
            // scan_file); warm runs need only stat(2) calls + cache reads.
            let key = file_cache_key(path, fp);
            let hit = cache.get(key);
            let is_hit = hit.is_some();
            entries.push(FileEntry { cache_hit: hit });
            if !is_hit {
                files_to_scan.push(path.clone());
                cache_miss_keys.push((key, path.clone()));
            }
        } else {
            entries.push(FileEntry { cache_hit: None });
            files_to_scan.push(path.clone());
        }
    }

    // Phase 2 (serial): ty-grade resolution (inheritance/MRO, return types,
    // annotated params, overloads) for calls the built-in pass deferred.
    // `python_env` (the `--python` value) only steers ty's third-party
    // discovery; the built-in resolver's env discovery is unchanged. A single
    // `ty server` is shared across all files (one stdin/stdout subprocess),
    // so this phase stays single-threaded.
    //
    // The server is started lazily — only when some file actually has calls
    // the built-in resolver could not resolve. `ty server` indexes the whole
    // project on `initialize`, a multi-second fixed cost (issue #31); a run
    // where the built-in resolver resolves everything (the common
    // editor-on-save / pre-commit case on first-party code) must not pay it.
    let mut ty: Option<TyResolver> = None;
    let mut ty_start_attempted = false;
    let mut ty_file_cache: FxHashMap<PathBuf, Option<String>> = FxHashMap::default();
    let mut diagnostics = Vec::new();
    // Collect skip warnings with their file index so they can be emitted in
    // the original sorted-file order after both phases finish (issue #53 + #46).
    let mut skip_warnings: Vec<(usize, PathBuf, String)> = Vec::new();

    // Cache hits bypass the pipeline; their diagnostics are added directly.
    for entry in &entries {
        if let Some(cached) = &entry.cache_hit {
            diagnostics.extend_from_slice(cached);
        }
    }

    // Run pipeline (Phase 1 parallel + Phase 2 serial ty) for cache misses only.
    pipeline_phases(
        &files_to_scan,
        project_root,
        config,
        &index,
        python_env,
        &mut ty,
        &mut ty_start_attempted,
        &mut ty_file_cache,
        &mut diagnostics,
        &mut skip_warnings,
    )?;

    // Emit skip warnings in the original sorted-file order (issue #53 + #46).
    skip_warnings.sort_unstable_by_key(|(i, ..)| *i);
    let skipped_paths: Vec<&PathBuf> = skip_warnings.iter().map(|(_, p, _)| p).collect();
    for (_, path, reason) in &skip_warnings {
        eprintln!(
            "strict-kwargs: warning: skipping {} ({reason})",
            path.display()
        );
    }

    // Store miss results in cache after the pipeline completes. Attribute each
    // file's diagnostics by path (Diagnostic::path is always the source file).
    // Skipped files are excluded — the skip reason may be transient.
    if let Some((ref cache, _)) = cache_and_fp {
        for (key, path) in &cache_miss_keys {
            if skipped_paths.contains(&path) {
                continue;
            }
            let file_diags: Vec<Diagnostic> = diagnostics
                .iter()
                .filter(|d| &d.path == path)
                .cloned()
                .collect();
            cache.put(*key, &file_diags);
        }
    }

    diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    Ok(diagnostics)
}

/// One file's built-in pass (issue #46 phase 1): read, decode, parse, and
/// walk it. Pure CPU over the shared `Sync` [`DefinitionIndex`], producing
/// only owned, `Send` data so the whole-project run can run this across files
/// in parallel; the serial `ty` phase then consumes the result.
///
/// A file that is not valid UTF-8 and carries no usable PEP 263 / BOM
/// encoding declaration (a binary fixture, vendored data, an unsupported
/// legacy encoding) yields [`ScanOutcome::Skipped`] rather than aborting:
/// the serial caller warns about it (in deterministic file order) and moves
/// on, so one stray file neither fails the whole run nor masks genuine
/// violations elsewhere (issue #53). A real filesystem error stays fatal.
///
/// The file under check is parsed through [`parse_module_guarded`], so one
/// nested deeper than the supported limit is rejected
/// ([`CheckError::TooDeeplyNested`]) instead of overflowing the stack; this
/// runs on a [`with_large_stack_pool`] worker, so legitimately-accepted deep
/// nesting has the same large stack the serial path gets (issue #54 + #46).
fn scan_file(
    project_root: &Path,
    path: &Path,
    config: &Config,
    index: &DefinitionIndex,
) -> Result<ScanOutcome, CheckError> {
    let source = match read_python_source(path)? {
        Source::Decoded(source) => source,
        Source::Undecodable(reason) => return Ok(ScanOutcome::Skipped(reason)),
    };
    let parsed = parse_module_guarded(&source)?;
    let module_name = module_name_for_path(project_root, path);
    // Scope the checker so its borrows of `source`/`parsed` end before
    // `source` is moved into the returned `FileScan`.
    let (diagnostics, pending, fixes, fixed_calls) = {
        let mut checker = CallChecker::new(
            path.to_path_buf(),
            module_name,
            is_package_init(path),
            &source,
            parsed.tokens(),
            index,
            config,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
        (
            std::mem::take(&mut checker.diagnostics),
            std::mem::take(&mut checker.ty_pending),
            std::mem::take(&mut checker.fixes),
            checker.fixed_calls,
        )
    };
    Ok(ScanOutcome::Scanned(FileScan {
        source,
        diagnostics,
        pending,
        fixes,
        fixed_calls,
    }))
}

/// Outcome of one file's parallel built-in pass: either the owned, `Send`
/// scan result, or a skip with the human-readable reason the serial caller
/// warns about (issue #53). Emitting the warning serially keeps its order
/// deterministic under the parallel pass.
enum ScanOutcome {
    Scanned(FileScan),
    Skipped(String),
}

/// Owned, `Send` result of one file's built-in pass ([`scan_file`]). The
/// `ty` fallback and the auto-fixer consume `pending` / `fixes` afterwards on
/// the main thread.
struct FileScan {
    source: String,
    diagnostics: Vec<Diagnostic>,
    pending: Vec<PendingTy>,
    fixes: Vec<Insertion>,
    fixed_calls: usize,
}

/// Apply `insertions` to `source` and validate that the result remains valid
/// Python. Shared by the built-in and ty-backed fixer paths.
fn plan_rewrite_insertions(
    path: &Path,
    source: &str,
    insertions: &[Insertion],
) -> Result<Option<String>, CheckError> {
    if insertions.is_empty() {
        return Ok(None);
    }
    // Every insertion adds a `name=` prefix, so the result always differs
    // from `source`.
    let fixed = apply_insertions(source, insertions);
    // Fail-safe (issue #41): never produce source that does not parse. The
    // parenthesized-span fix should keep every rewrite valid, but a malformed
    // result must abort with a report rather than silently corrupt the file.
    if parse_module(&fixed).is_err() {
        return Err(CheckError::FixProducedInvalidSyntax {
            path: path.to_path_buf(),
        });
    }
    Ok(Some(fixed))
}

/// Like [`scan_file`] but sends each result to `tx` as the worker finishes
/// rather than collecting all results first. This lets the ty phase in
/// [`check_paths_impl`] start working on completed files while Phase 1
/// workers are still running over the rest of the project (cross-file
/// pipelining, issue #67).
///
/// `tx` is moved in and dropped when all workers finish, closing the channel
/// and signalling the consumer that no more items are coming.
///
/// Excluded from the coverage gate for the same reason as
/// [`run_with_large_stack`]: the per-file logic ([`scan_file`]) is a
/// separate, fully gated function exercised by every integration test; what
/// is excluded here is only the parallel-pool orchestration — the
/// environment-only pool-construction failure and the scheduling-dependent
/// path that surfaces one worker's error — neither of which is
/// deterministically reachable.
#[cfg_attr(coverage, coverage(off))]
fn stream_scan_files(
    python_files: &[PathBuf],
    project_root: &Path,
    config: &Config,
    index: &DefinitionIndex,
    tx: std::sync::mpsc::Sender<(usize, PathBuf, Result<ScanOutcome, CheckError>)>,
) -> Result<(), CheckError> {
    with_large_stack_pool(move || {
        python_files
            .par_iter()
            .enumerate()
            .for_each_with(tx, |tx, (i, path)| {
                let result = scan_file(project_root, path, config, index);
                // Ignore send errors: the consumer has exited early (e.g. a
                // ty error was already recorded).
                let _ = tx.send((i, path.clone(), result));
            });
        Ok(())
    })
}

/// Like [`stream_scan_files`], but collects the completed scans for the fixer.
/// Excluded from the coverage gate for the same reason as [`stream_scan_files`]:
/// the per-file scan logic is covered elsewhere, while this is only
/// parallel-pool orchestration.
#[cfg_attr(coverage, coverage(off))]
fn scan_files_for_fix(
    python_files: &[PathBuf],
    project_root: &Path,
    config: &Config,
    index: &DefinitionIndex,
) -> Result<Vec<(PathBuf, ScanOutcome)>, CheckError> {
    with_large_stack_pool(|| {
        python_files
            .par_iter()
            .map(|path| {
                let outcome = scan_file(project_root, path, config, index)?;
                Ok((path.clone(), outcome))
            })
            .collect()
    })
}

/// Collect the `.py`/`.pyi` files reachable from `paths`.
///
/// A path that is neither a file nor a directory does not exist: that is a
/// hard error ([`CheckError::PathNotFound`]), like `ruff`, rather than a
/// silent skip that would let a mistyped target report "clean" in CI
/// (issue #55). An *existing* file passed directly that is not Python is
/// still skipped — that is a deliberate selection, not a mistake.
///
/// # Errors
///
/// Returns [`CheckError::PathNotFound`] for the first path that does not
/// exist.
fn collect_python_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, CheckError> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_python_file(path) {
                files.push(path.clone());
            }
        } else if path.is_dir() {
            // Prune `.venv`/`.git`/`__pycache__`/dot-directories instead of
            // descending into them and discarding their files one by one: a
            // real project's virtualenv alone is tens of thousands of
            // entries, so the unpruned walk dominated whole-project runtime
            // and was the main run-to-run variance source (cold vs warm FS
            // cache over ~50k entries). `is_ignored_path` below stays the
            // authoritative filter, so the result set is unchanged — only
            // directories every one of whose files it would reject are
            // skipped, and never the walk root (depth 0).
            let walk = walkdir::WalkDir::new(path)
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !is_prunable_dir(e));
            for entry in walk
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let entry_path = entry.path().to_path_buf();
                if is_python_file(&entry_path) && !is_ignored_path(&entry_path) {
                    files.push(entry_path);
                }
            }
        } else {
            // Neither a file nor a directory: the path does not exist (a
            // mistyped target). Fail loudly instead of reporting "clean".
            return Err(CheckError::PathNotFound { path: path.clone() });
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "py" || ext == "pyi")
}

/// Whether `entry` is a directory that [`is_ignored_path`] would reject for
/// every file beneath it (`.git`, `.venv` and other dot-directories,
/// `venv`, `__pycache__`), so the walk can skip descending into it. Kept in
/// lock-step with the component rule in [`is_ignored_path`].
pub fn is_prunable_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "venv" || name == "__pycache__"
}

fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') || name == "venv" || name == "__pycache__"
        }
        _ => false,
    })
}

struct CallChecker<'a> {
    path: PathBuf,
    module_name: String,
    /// Whether the file is a package initializer (`__init__.py`), which is
    /// the anchor for its own relative imports.
    is_package: bool,
    source: &'a str,
    /// Lexer tokens for `source`, used to recover the parenthesized span of a
    /// call argument so the `name=` prefix lands *before* any redundant outer
    /// parentheses (issue #41) rather than inside them.
    tokens: &'a Tokens,
    index: &'a DefinitionIndex,
    config: &'a Config,
    /// Violations found in this file. Owned (not a shared `&mut`) so each
    /// file's built-in pass is an independent, `Send` unit of work the
    /// whole-project run executes in parallel (issue #46); the single-threaded
    /// `ty` fallback then merges them.
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<Scope>,
    /// Calls the built-in resolver couldn't resolve, deferred for a single
    /// pipelined batch of ty queries per file.
    ty_pending: Vec<PendingTy>,
    /// Source insertions for the auto-fixer (`check_paths` ignores these).
    fixes: Vec<Insertion>,
    /// Number of call sites the fixer rewrote in this file.
    fixed_calls: usize,
}

/// A call awaiting ty resolution: byte offsets into the file's source.
struct PendingTy {
    /// Start of the callee identifier (where we hover / goto-definition).
    callee_offset: usize,
    /// Start of the whole call expression (for the diagnostic position).
    call_start: usize,
    positional_count: usize,
}

#[derive(Debug, Default, Clone)]
struct Scope {
    /// Local name -> fully-qualified callable/class name.
    names: FxHashMap<String, String>,
    /// Local name -> fully-qualified *module* path (from ``import``).
    modules: FxHashMap<String, String>,
    /// Names in `names` that are bound to an *instance* (`x = C()`), as
    /// opposed to the class object itself. Lets `Class.method(recv, …)` be
    /// told apart from a bound `instance.method(…)` call (issue #27).
    instances: rustc_hash::FxHashSet<String>,
    /// Parameter names for the function that owns this scope.  Calls through
    /// a parameter (e.g. a `Callable`-typed arg) cannot be resolved to a
    /// concrete indexed signature, so they are skipped rather than matched
    /// against a homonymous module-level or nested function (issue #71).
    opaque_params: rustc_hash::FxHashSet<String>,
}

impl<'a> CallChecker<'a> {
    #[allow(
        clippy::too_many_arguments,
        reason = "per-file context wiring; grouping into a struct would just \
                  move the argument list to its constructor"
    )]
    fn new(
        path: PathBuf,
        module_name: String,
        is_package: bool,
        source: &'a str,
        tokens: &'a Tokens,
        index: &'a DefinitionIndex,
        config: &'a Config,
    ) -> Self {
        Self {
            path,
            module_name,
            is_package,
            source,
            tokens,
            index,
            config,
            diagnostics: Vec::new(),
            scopes: vec![Scope::default()],
            ty_pending: Vec::new(),
            fixes: Vec::new(),
            fixed_calls: 0,
        }
    }

    fn current_scope(&mut self) -> &mut Scope {
        // The scope stack is seeded with one `Scope` in `new` and every
        // `pop_scope` is balanced with a prior `push_scope`, so it is never
        // empty here.
        #[allow(
            clippy::expect_used,
            reason = "scope stack invariant: always non-empty"
        )]
        self.scopes.last_mut().expect("scope stack non-empty")
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, local_name: &str, fullname: String) {
        self.current_scope()
            .names
            .insert(local_name.to_string(), fullname);
    }

    fn resolve_local(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(fullname) = scope.names.get(name) {
                return Some(fullname.clone());
            }
        }
        None
    }

    fn mark_param_opaque(&mut self, name: &str) {
        self.current_scope().opaque_params.insert(name.to_string());
    }

    /// Whether `name` is a function parameter in the innermost scope that
    /// sees it.  A real `names` binding in the same or an inner scope shadows
    /// any outer opaque entry (the parameter was re-assigned to a known def).
    fn is_opaque_local(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.names.contains_key(name) {
                return false;
            }
            if scope.opaque_params.contains(name) {
                return true;
            }
        }
        false
    }

    fn define_module(&mut self, local_name: &str, module_path: String) {
        self.current_scope()
            .modules
            .insert(local_name.to_string(), module_path);
    }

    fn resolve_module(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(path) = scope.modules.get(name) {
                return Some(path.clone());
            }
        }
        None
    }

    /// Resolve ``from <level dots><module> import ...`` to its base dotted
    /// path, using the shared resolver so package (`__init__`) anchoring
    /// matches the indexer.
    fn resolve_import_base(&self, level: u32, module: Option<&str>) -> Option<String> {
        relative_base(&self.module_name, self.is_package, level, module)
    }

    /// ``import a.b.c`` / ``import a.b as c``
    fn record_plain_import(&mut self, import: &ast::StmtImport) {
        for alias in &import.names {
            let dotted = alias.name.as_str();
            if let Some(asname) = &alias.asname {
                // ``import a.b as c`` binds ``c`` -> ``a.b``.
                self.define_module(asname.as_str(), dotted.to_string());
            } else {
                // ``import a.b`` binds the top-level ``a``; attribute access
                // uses the full dotted path.
                let top = dotted.split('.').next().unwrap_or(dotted);
                self.define_module(top, top.to_string());
            }
        }
    }

    /// ``from a.b import c [as d]`` / ``from . import x``
    fn record_from_import(&mut self, import: &ast::StmtImportFrom) {
        let Some(base) = self.resolve_import_base(
            import.level,
            import.module.as_ref().map(ast::Identifier::as_str),
        ) else {
            return;
        };
        for alias in &import.names {
            let imported = alias.name.as_str();
            if imported == "*" {
                continue;
            }
            let local = alias
                .asname
                .as_ref()
                .map_or(imported, ast::Identifier::as_str);
            let fullname = if base.is_empty() {
                imported.to_string()
            } else {
                format!("{base}.{imported}")
            };
            // The imported name may be a submodule or a callable; bind both
            // interpretations so attribute and direct calls work.
            self.define(local, fullname.clone());
            self.define_module(local, fullname);
        }
    }

    /// Flatten an attribute/name chain (``a.b.c``) into a dotted string.
    fn dotted_path(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Name(name) => Some(name.id.to_string()),
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let base = Self::dotted_path(value)?;
                Some(format!("{base}.{attr}"))
            }
            _ => None,
        }
    }

    fn record_instance(&mut self, local_name: &str, class_fullname: String) {
        let scope = self.current_scope();
        scope.names.insert(local_name.to_string(), class_fullname);
        scope.instances.insert(local_name.to_string());
    }

    /// Whether the nearest binding of `name` (the one [`resolve_local`] would
    /// return) is an *instance*, rather than the class object itself.
    ///
    /// [`resolve_local`]: Self::resolve_local
    ///
    /// Consulted by [`Self::is_unbound_class_method_call`] to tell
    /// `Class.method(recv, …)` (an unbound call through the class object)
    /// apart from an ordinary bound `instance.method(…)` call.
    fn binding_is_instance(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.names.contains_key(name) {
                return scope.instances.contains(name);
            }
        }
        false
    }

    /// Whether `func` is an unbound instance-method call made through the
    /// class object itself — `Class.method(receiver, …)` — so the first
    /// positional argument is the explicitly-passed receiver.
    ///
    /// It binds to `self` and is never keyword-passable, exactly the issue
    /// #15 case the ty path handles via [`strip_unbound_receiver`]; this is
    /// the first-party/built-in-resolver analogue (issue #27). `cls` (a
    /// classmethod, auto-bound even through the class) and any other first
    /// parameter (a staticmethod / free function) pass no receiver, so only
    /// a genuine leading `self` qualifies.
    ///
    /// Dunder-receiver methods (`__init__`/`__new__`/`__call__`/`__get__`/
    /// `__set__`) are excluded: [`Signature::max_positional_at_call_site`]
    /// already drops their leading receiver itself, so also stripping it
    /// here would double-count the first real parameter. Their
    /// implicit-receiver semantics are out of scope for issue #27 (a regular
    /// instance-method call) and keep their existing dedicated handling.
    // A defensive predicate: every arm but the final one is an early
    // `return false` guard for a call shape that is not an unbound
    // class-method call. It is core built-in-resolver logic (issue #27),
    // so every guard arm is exercised by the gate — see the
    // `unbound_class_method_*` and guard-arm tests.
    fn is_unbound_class_method_call(
        &self,
        func: &Expr,
        callee_fullname: &str,
        first_param: Option<&str>,
    ) -> bool {
        const DUNDER_RECEIVERS: [&str; 5] =
            [".__init__", ".__new__", ".__call__", ".__get__", ".__set__"];
        if first_param != Some("self") {
            return false;
        }
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = func else {
            return false;
        };
        if let Expr::Name(base) = &**value {
            // Dunder-receiver methods called through a *single-name* base
            // are excluded: `max_positional_at_call_site` already strips
            // their leading receiver itself, so also stripping it here
            // would double-count the first real parameter (issue #27).
            if DUNDER_RECEIVERS
                .iter()
                .any(|suffix| callee_fullname.ends_with(suffix))
            {
                return false;
            }
            let base = base.id.as_str();
            // `base` must resolve to the class that *directly* owns `attr`
            // and must denote the class object, not an instance of it
            // (`k.method(…)` is an ordinary bound call).
            let Some(resolved) = self.resolve_local(base) else {
                return false;
            };
            callee_fullname == format!("{resolved}.{attr}") && !self.binding_is_instance(base)
        } else {
            // Multi-level attribute chain (e.g. `module.Class.method(self, …)`):
            // if the leftmost name resolves as a module, the expression
            // denotes a class reached through a module path, making the
            // call unbound.  Non-dotted-path bases (e.g. `f().m(self, …)`)
            // are not unbound calls.
            let Some(chain) = Self::dotted_path(value) else {
                return false;
            };
            self.resolve_module(chain.split('.').next().unwrap_or(""))
                .is_some()
        }
    }

    fn class_from_constructor(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Call(ast::ExprCall { func, .. }) => match &**func {
                Expr::Name(name) => self.resolve_local(name.id.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    fn check_call(&mut self, call: &ast::ExprCall) {
        let Some(callee_fullname) = self.resolve_callee(&call.func) else {
            // Built-in resolver couldn't resolve: defer to a pipelined ty
            // query (handled once per file after the walk).
            self.record_ty_pending(call);
            return;
        };
        // Functions whose first argument must stay positional at runtime
        // (e.g. @singledispatch dispatches on args[0].__class__): skip
        // without deferring to ty.
        if self.index.is_excluded(&callee_fullname) {
            return;
        }
        let Some(signatures) = self.index.get(&callee_fullname) else {
            // Resolved to a name the index does not know (e.g. a module
            // attribute bound to a non-callable): defer to the ty fallback.
            // Re-check is_excluded: `get` may have triggered lazy loading
            // that discovered a @singledispatch decorator and added the
            // function to `excluded` without adding it to `signatures`.
            self.record_ty_pending_unless_lazily_excluded(&callee_fullname, call);
            return;
        };
        if is_typing_special_form_constructor(&callee_fullname) {
            return;
        }
        if self.config.debug {
            eprintln!("DEBUG: strict_kwargs: {callee_fullname}");
        }
        // A constructor call resolves to ``Class.__init__``/``__new__``; also
        // honor an ``ignore_names`` entry for the class itself (``builtins.str``).
        let ignored = self.config.is_ignored(&callee_fullname)
            || callee_fullname
                .strip_suffix(".__init__")
                .or_else(|| callee_fullname.strip_suffix(".__new__"))
                .is_some_and(|class| self.config.is_ignored(class));
        let positional_count = positional_argument_count(&call.arguments);
        // Issue #27: an unbound instance-method call through the class object
        // (`K.m(K(), 1)`) passes the receiver explicitly. It binds to `self`
        // and is never keyword-passable, so — like the typeshed/ty path's
        // `strip_unbound_receiver` (issue #15) — drop the leading `self` and
        // that one positional argument before the limit check, the
        // diagnostic, and the fixer.
        let first_param_name = signatures
            .first()
            .and_then(|s| s.parameters.first())
            .and_then(|p| p.name.as_deref());
        let receiver_is_explicit =
            self.is_unbound_class_method_call(&call.func, &callee_fullname, first_param_name);
        let effective: Vec<Signature> = if receiver_is_explicit {
            signatures.iter().map(without_leading_self).collect()
        } else {
            signatures.to_vec()
        };
        let effective_count = if receiver_is_explicit {
            positional_count.saturating_sub(1)
        } else {
            positional_count
        };
        // Overload-safe: only flag when the call exceeds the positional limit
        // of *every* candidate signature (the most permissive overload wins),
        // so ``.pyi`` stub overloads never produce false positives.
        if effective.iter().any(|signature| {
            !call_exceeds_positional_limit(signature, &callee_fullname, ignored, effective_count)
        }) {
            return;
        }
        let max_positional = effective
            .iter()
            .filter_map(|signature| {
                signature.max_positional_at_call_site(&callee_fullname, ignored)
            })
            .max()
            .unwrap_or(0);
        let (line, column) = line_column(self.source, call.start());
        self.diagnostics.push(Diagnostic {
            path: self.path.clone(),
            line,
            column,
            callee: format_callee_display(&callee_fullname),
            positional_count: effective_count,
            max_positional,
        });
        // Auto-fix is only applied when a single, unambiguous signature is
        // known: overloaded callees may bind the same position to differently
        // named parameters, so a keyword rewrite would not be safe. A
        // synthesized ``@dataclass`` / ``NamedTuple`` constructor is likewise
        // declined until its position->name mapping is guaranteed sound
        // across every modeled constructor shape.
        if let ([signature], false) = (
            signatures.as_ref(),
            self.index.is_synthesized(&callee_fullname),
        ) {
            // `receiver.method(...)` omits the bound receiver at the call
            // site; a plain `name(...)` call passes every parameter explicitly.
            let is_attribute_call = matches!(&*call.func, Expr::Attribute(_));
            if let Some(insertions) = call_fix_insertions(
                call,
                self.tokens,
                &callee_fullname,
                signature,
                max_positional,
                positional_count,
                is_attribute_call,
                receiver_is_explicit,
            ) {
                self.fixes.extend(insertions);
                self.fixed_calls += 1;
            }
        }
    }

    /// Defer a call the built-in resolver missed to a pipelined ty query.
    fn record_ty_pending(&mut self, call: &ast::ExprCall) {
        // Position at the callee identifier: the attribute for ``x.m()``,
        // otherwise the name itself.
        let callee_offset = match &*call.func {
            Expr::Attribute(attr) => attr.attr.range().start(),
            Expr::Name(name) => name.range().start(),
            _ => return,
        };
        self.ty_pending.push(PendingTy {
            callee_offset: callee_offset.to_usize(),
            call_start: call.start().to_usize(),
            positional_count: positional_argument_count(&call.arguments),
        });
    }

    /// Record a ty fallback unless a lazy signature lookup just discovered
    /// that the callee is excluded. In today's resolver paths the earlier
    /// `resolve_callee` lookups usually discover that first; this remains a
    /// defensive guard for future lazy-index paths.
    #[cfg_attr(coverage, coverage(off))]
    fn record_ty_pending_unless_lazily_excluded(
        &mut self,
        callee_fullname: &str,
        call: &ast::ExprCall,
    ) {
        if !self.index.is_excluded(callee_fullname) {
            self.record_ty_pending(call);
        }
    }

    /// Map a base name to the signature-bearing fullname to check: the name
    /// itself (a function), else its constructor (``__init__``/``__new__``
    /// for a class). Returns `None` when nothing is indexed.
    fn callable_fullname(&self, base: &str) -> Option<String> {
        if self.index.get(base).is_some() {
            return Some(base.to_string());
        }
        for ctor in ["__init__", "__new__"] {
            let candidate = format!("{base}.{ctor}");
            if self.index.get(&candidate).is_some() {
                return Some(candidate);
            }
        }
        None
    }

    /// Resolve a deeper attribute chain (`os.path.join` -> the joined
    /// module path). Reached only when the attribute's base is itself an
    /// attribute (the bare-`Name` base is handled by the caller), so
    /// `dotted_path` always contains at least one `.`; the no-`.` and
    /// unresolved-module fall-throughs are therefore unreachable defensive
    /// returns. Behaviour is covered by `deep_dotted_attribute_chain_resolves`.
    #[cfg_attr(coverage, coverage(off))]
    fn resolve_dotted_module_attr(&self, value: &Expr, attr_name: &str) -> Option<String> {
        let chain = Self::dotted_path(value)?;
        let (head, rest) = chain.split_once('.')?;
        let module_path = self.resolve_module(head)?;
        let candidate = format!("{module_path}.{rest}.{attr_name}");
        Some(self.callable_fullname(&candidate).unwrap_or(candidate))
    }

    fn resolve_callee(&self, func: &Expr) -> Option<String> {
        match func {
            Expr::Name(name) => {
                let local = name.id.as_str();
                // A parameter or other opaque local cannot be resolved to a
                // concrete indexed definition — skip it to avoid false
                // positives from a same-named function elsewhere (issue #71).
                if self.is_opaque_local(local) {
                    return None;
                }
                if let Some(resolved) = self.resolve_local(local) {
                    if self.binding_is_instance(local) {
                        let dunder_call = format!("{resolved}.__call__");
                        return self.index.get(&dunder_call).map(|_| dunder_call);
                    }
                    // Class name -> its constructor, if indexed.
                    return Some(self.callable_fullname(&resolved).unwrap_or(resolved));
                }
                // Not a local binding: try this module, then builtins.
                let module_candidate = format!("{}.{}", self.module_name, local);
                if let Some(found) = self.callable_fullname(&module_candidate) {
                    return Some(found);
                }
                if let Some(found) = self.callable_fullname(&format!("builtins.{local}")) {
                    return Some(found);
                }
                Some(module_candidate)
            }
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let attr_name = attr.id.as_str();
                if let Expr::Name(base) = &**value {
                    let base_name = base.id.as_str();
                    // Local bindings (incl. a locally redefined class) take
                    // precedence over a stale ``import`` module binding.
                    let candidate = if let Some(local) = self.resolve_local(base_name) {
                        format!("{local}.{attr_name}")
                    } else if let Some(module_path) = self.resolve_module(base_name) {
                        // ``import a.b as m`` / ``import lib`` then ``m.f()``.
                        format!("{module_path}.{attr_name}")
                    } else {
                        format!("{}.{}.{}", self.module_name, base_name, attr_name)
                    };
                    // Resolve through constructors so e.g. ``lib.MyClass(1)``
                    // finds ``lib.MyClass.__init__``.
                    return Some(self.callable_fullname(&candidate).unwrap_or(candidate));
                }
                // Deeper chains: ``import os.path`` then ``os.path.join()``.
                self.resolve_dotted_module_attr(value, attr_name)
            }
            Expr::Call(constructor) => {
                let Expr::Name(class_name) = &*constructor.func else {
                    return None;
                };
                let class_fullname = self.resolve_local(class_name.id.as_str())?;
                let dunder_call = format!("{class_fullname}.__call__");
                self.index
                    .get(&dunder_call)
                    .is_some()
                    .then_some(dunder_call)
            }
            _ => None,
        }
    }

    fn visit_if_branch_stmt(&mut self, stmt: &'a Stmt, traversal: IfBranchTraversal) {
        match traversal {
            IfBranchTraversal::Module => self.visit_stmt(stmt),
            IfBranchTraversal::LocalBody => self.visit_body_stmt(stmt),
        }
    }

    /// `walk_stmt` in `rustpython-ruff_python_ast` 0.15.8 visits each `elif`
    /// test expression twice: once via a direct `visit_expr` call and again
    /// inside `walk_elif_else_clause`. Override `Stmt::If` to traverse each test
    /// and body exactly once. Branch statements use the caller's traversal mode
    /// so module-level imports are still recorded while function-local imports
    /// stay local.
    fn visit_if_stmt(&mut self, if_stmt: &'a ast::StmtIf, traversal: IfBranchTraversal) {
        let ast::StmtIf {
            test,
            body,
            elif_else_clauses,
            ..
        } = if_stmt;
        self.visit_expr(test);
        for inner in body {
            self.visit_if_branch_stmt(inner, traversal);
        }
        for clause in elif_else_clauses {
            if let Some(clause_test) = &clause.test {
                self.visit_expr(clause_test);
            }
            for inner in &clause.body {
                self.visit_if_branch_stmt(inner, traversal);
            }
        }
    }

    /// Walk a statement that appears in the body of a function, class, or
    /// control-flow branch. Statements that carry custom `visit_stmt` logic
    /// (`Assign`, `AnnAssign`, `FunctionDef`, `ClassDef`) are dispatched
    /// through `visit_stmt` so instance tracking and definition registration
    /// fire correctly. `If` uses the custom branch traversal so the
    /// double-elif-test fix still fires without registering function-local
    /// imports. Everything else (e.g. `Import` / `ImportFrom`) goes through
    /// `walk_stmt` directly; function-local imports are intentionally not
    /// registered.
    fn visit_body_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::If(if_stmt) => self.visit_if_stmt(if_stmt, IfBranchTraversal::LocalBody),
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                self.visit_stmt(stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

impl<'a> Visitor<'a> for CallChecker<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(StmtFunctionDef {
                name,
                parameters,
                body,
                decorator_list,
                ..
            }) => {
                // Decorator expressions are evaluated in the enclosing
                // scope, so visit them before defining/scoping the function
                // (issue #51: decorator-factory calls were never checked).
                for decorator in decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                self.define(name, format!("{}.{}", self.module_name, name));
                self.push_scope();
                // Register every parameter as opaque so that calls through
                // a Callable-typed (or otherwise unresolvable) parameter
                // don't fall back to a module-level function with the same
                // name (issue #71).
                for param in parameters
                    .posonlyargs
                    .iter()
                    .chain(parameters.args.iter())
                    .chain(parameters.kwonlyargs.iter())
                {
                    self.mark_param_opaque(param.parameter.name.as_str());
                }
                if let Some(vararg) = &parameters.vararg {
                    self.mark_param_opaque(vararg.name.as_str());
                }
                if let Some(kwarg) = &parameters.kwarg {
                    self.mark_param_opaque(kwarg.name.as_str());
                }
                for inner in body {
                    self.visit_body_stmt(inner);
                }
                self.pop_scope();
            }
            Stmt::ClassDef(StmtClassDef {
                name,
                body,
                decorator_list,
                ..
            }) => {
                for decorator in decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                let class_fullname = format!("{}.{}", self.module_name, name);
                self.define(name, class_fullname);
                self.push_scope();
                for inner in body {
                    match inner {
                        Stmt::FunctionDef(StmtFunctionDef {
                            parameters: method_parameters,
                            body: method_body,
                            decorator_list: method_decorators,
                            ..
                        }) => {
                            for decorator in method_decorators {
                                self.visit_expr(&decorator.expression);
                            }
                            self.push_scope();
                            for param in method_parameters
                                .posonlyargs
                                .iter()
                                .chain(method_parameters.args.iter())
                                .chain(method_parameters.kwonlyargs.iter())
                            {
                                self.mark_param_opaque(param.parameter.name.as_str());
                            }
                            if let Some(vararg) = &method_parameters.vararg {
                                self.mark_param_opaque(vararg.name.as_str());
                            }
                            if let Some(kwarg) = &method_parameters.kwarg {
                                self.mark_param_opaque(kwarg.name.as_str());
                            }
                            for method_stmt in method_body {
                                self.visit_body_stmt(method_stmt);
                            }
                            self.pop_scope();
                        }
                        _ => self.visit_body_stmt(inner),
                    }
                }
                self.pop_scope();
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                if let Some(class_fullname) = self.class_from_constructor(value) {
                    for target in targets {
                        if let Expr::Name(name) = target {
                            self.record_instance(name.id.as_str(), class_fullname.clone());
                        }
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                value: Some(value),
                ..
            }) => {
                if let Some(class_fullname) = self.class_from_constructor(value) {
                    if let Expr::Name(name) = &**target {
                        self.record_instance(name.id.as_str(), class_fullname);
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::If(if_stmt) => self.visit_if_stmt(if_stmt, IfBranchTraversal::Module),
            Stmt::Import(import) => self.record_plain_import(import),
            Stmt::ImportFrom(import) => self.record_from_import(import),
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Call(call) = expr {
            self.check_call(call);
        }
        walk_expr(self, expr);
    }
}

// Core positional-limit predicate. Its behaviour (plain / positional-only /
// keyword-only / `*args` / ignore-list / overload cases) is covered
// extensively by the checker integration suites, but it is monomorphized
// into several test binaries and `llvm-cov`'s per-instantiation branch
// accounting reports exercised arms as missed (it shows the same branch as
// `[True 0, False n]` in one instantiation beside a covered one). Excluded
// from the gate with that documented rationale.
#[cfg_attr(coverage, coverage(off))]
fn call_exceeds_positional_limit(
    signature: &Signature,
    fullname: &str,
    ignored: bool,
    positional_count: usize,
) -> bool {
    // `max_positional_at_call_site` returns `None` exactly when `ignored`,
    // so this single check also covers the ignore-list case.
    let Some(max_positional) = signature.max_positional_at_call_site(fullname, ignored) else {
        return false;
    };
    let has_var_positional = signature
        .parameters
        .iter()
        .any(|p| p.kind == crate::signature::ParameterKind::VarPositional);
    if has_var_positional && positional_count > max_positional {
        return false;
    }
    positional_count > max_positional
}

/// Build the `name=` insertions that rewrite a flagged call's surplus
/// positional arguments, or `None` when the call cannot be fixed safely.
///
/// Conservative by design (issue #7): if anything about the call or the
/// mapping is uncertain we decline to fix and leave the diagnostic standing.
//
// The fixer's accept/decline behaviour is covered end-to-end by the 30+
// cases in `tests/fix.rs`; excluded from the gate for the same
// multi-instantiation reason as `call_exceeds_positional_limit`.
#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "resolved call facts threaded in from the visitor; a parameter \
              struct would only relocate the same list"
)]
fn call_fix_insertions(
    call: &ast::ExprCall,
    tokens: &Tokens,
    callee_fullname: &str,
    signature: &Signature,
    max_positional: usize,
    positional_count: usize,
    is_attribute_call: bool,
    receiver_is_explicit: bool,
) -> Option<Vec<Insertion>> {
    // Star-unpacking at the call site (`f(*xs)` / `f(**kw)`): the positional
    // count is unknown, so a positional->keyword mapping is unsound.
    if call.arguments.args.iter().any(Expr::is_starred_expr) {
        return None;
    }
    if call.arguments.keywords.iter().any(|kw| kw.arg.is_none()) {
        return None;
    }
    // Descriptor protocol calls are rare and their receiver/value mapping is
    // subtle; skip rather than risk a wrong rewrite.
    if callee_fullname.ends_with(".__get__") || callee_fullname.ends_with(".__set__") {
        return None;
    }

    // `(skip, start)`: how the call's positional arguments map onto the
    // signature's parameters, and the first argument index to rewrite.
    let (skip, start) = if receiver_is_explicit {
        // Unbound `Class.method(receiver, …)` (issue #27): the receiver is
        // `args[0]` and binds to `self`, which is never keyword-passable, so
        // it stays positional. Arguments map 1:1 onto parameters (no skip);
        // `max_positional` is the limit *after* `self` was stripped, so the
        // receiver slot adds one more allowed positional.
        (0usize, max_positional + 1)
    } else {
        // Leading signature parameters that are implicit at the call site
        // (the bound/constructed receiver, never present in
        // `call.arguments`).
        //
        // A name-only `self`/`cls` test is unsound: a *standalone* function
        // may legitimately name its first parameter `self`/`cls` (factories,
        // decorators, metaclass helpers), and such a function is always
        // called by name (`f(...)`) with that parameter passed *explicitly*.
        // Skipping it there shifts the whole mapping by one and silently
        // emits wrong keyword names. The receiver is implicit only for a
        // constructor/callable dunder or a *bound* attribute-style call
        // (`receiver.method(...)`).
        let first_param_is_receiver_name = matches!(
            signature.parameters.first().and_then(|p| p.name.as_deref()),
            Some("self" | "cls")
        );
        let is_dunder_receiver = callee_fullname.ends_with(".__init__")
            || callee_fullname.ends_with(".__new__")
            || callee_fullname.ends_with(".__call__");
        let receiver_is_implicit =
            is_dunder_receiver || (is_attribute_call && first_param_is_receiver_name);
        (usize::from(receiver_is_implicit), max_positional)
    };

    let mut insertions = Vec::new();
    for arg_index in start..positional_count {
        let arg = call.arguments.args.get(arg_index)?;
        // A bare generator (`f(x for x in y)`) or walrus (`f(x := 1)`) would
        // need extra parentheses once prefixed; decline rather than wrap.
        if arg.is_generator_expr() || arg.is_named_expr() {
            return None;
        }
        let param = signature.parameters.get(arg_index + skip)?;
        let name = param.name.as_deref()?;
        // Only these kinds accept a keyword argument; a positional-only
        // parameter or `*args`/`**kwargs` slot cannot be rewritten.
        if !matches!(
            param.kind,
            ParameterKind::PositionalOrKeyword | ParameterKind::KeywordOnly
        ) {
            return None;
        }
        // A redundantly parenthesized argument (`f((1))`) has an AST span
        // that starts *inside* the parentheses, since the Ruff parser drops
        // them. Prefixing there yields `f((name=1))` — a `SyntaxError`
        // (issue #41). Recover the span including any such parentheses so the
        // `name=` lands before them: `f(name=(1))`. The `Arguments` parent
        // keeps the call's own `(`/`)` from being mistaken for wrapping.
        let arg_start = match parenthesized_range(
            ExprRef::from(arg),
            AnyNodeRef::from(&call.arguments),
            tokens,
        ) {
            Some(range) => range.start(),
            None => arg.range().start(),
        };
        insertions.push(Insertion {
            at: arg_start.to_usize(),
            text: format!("{name}="),
        });
    }
    (!insertions.is_empty()).then_some(insertions)
}

/// Rewrite positional call arguments to keyword arguments for every fixable
/// violation reachable from `paths`.
///
/// Mirrors [`check_paths`]: it runs the same detection — built-in resolver
/// *and*, for the calls that misses, the (required) `ty` fallback steered by
/// `python_env` (the `--python` value). The *rewrite*, by design (issue #7),
/// stays conservative: a call is rewritten only when the parameter mapping is
/// unambiguous. For `ty`-resolved calls that means a single concrete hover
/// signature with complete parameter names; overloads, synthesized
/// constructors, ambiguous callable displays, and goto-definition-only
/// resolutions are left alone (a wrong parameter name would corrupt source,
/// cf. issue #41).
///
/// Running the `ty` fallback here also lets the returned
/// [`FixOutcome::declined`] account for *every* violation `check` would
/// report, so `fix` then `check` (with the same `--python`) is predictable
/// rather than silently inconsistent (issue #42). The fallback still starts
/// lazily — only when the built-in resolver leaves a file with unresolved
/// calls — so the all-first-party common case pays nothing.
///
/// Files without changes are omitted from [`FixOutcome::files`].
///
/// # Errors
///
/// Returns [`CheckError`] if a path argument does not exist
/// ([`CheckError::PathNotFound`]), a source file cannot be read or parsed,
/// or the required `ty` backend is missing ([`CheckError::TyNotFound`]) or
/// its server cannot start ([`CheckError::TyServerFailed`]). A file nested
/// deeper than the supported limit is rejected
/// ([`CheckError::TooDeeplyNested`]) rather than overflowing the stack; the
/// walk runs on a large dedicated stack (issue #54).
pub fn fix_paths(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
) -> Result<FixOutcome, CheckError> {
    run_with_large_stack(move || fix_paths_impl(project_root, paths, config, python_env))
}

fn fix_paths_impl(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
) -> Result<FixOutcome, CheckError> {
    // `ty` is a hard requirement; verify it up front (see `check_paths`).
    require_ty_present()?;
    let python_files = collect_python_files(paths)?;
    let index = build_index(project_root, &python_files)?;

    // Phase 1 (parallel, see `check_paths`): run the built-in pass for each
    // file. Rewrites are planned serially below after the ty fallback has a
    // chance to add safe single-signature hover fixes.
    let scans = scan_files_for_fix(&python_files, project_root, config, &index)?;

    let mut ty: Option<TyResolver> = None;
    let mut ty_start_attempted = false;
    let mut ty_file_cache: FxHashMap<PathBuf, Option<String>> = FxHashMap::default();
    // Every violation the checker would report, across all files (built-in
    // and ty-resolved). Used for the declined count; ty may also append safe
    // hover-derived insertions to the built-in rewrite plan.
    let mut diagnostics = Vec::new();
    let mut fixed_total = 0usize;
    let mut results = Vec::new();
    for (path, outcome) in scans {
        // Warn (deterministically, see `check_paths`) and skip an undecodable
        // file; it produces no fix and no diagnostics (issue #53).
        let scan = match outcome {
            ScanOutcome::Skipped(reason) => {
                eprintln!(
                    "strict-kwargs: warning: skipping {} ({reason})",
                    path.display()
                );
                continue;
            }
            ScanOutcome::Scanned(scan) => scan,
        };
        diagnostics.extend(scan.diagnostics);
        let mut insertions = scan.fixes;
        let mut fixed_calls = scan.fixed_calls;
        // The ty fallback adds diagnostics, and for a single concrete named
        // hover signature can now add the same conservative `name=` insertions
        // as the built-in resolver. Ambiguous ty displays remain diagnostics
        // only, so the declined count still matches a following `check`.
        resolve_file_with_ty(
            &mut ty,
            &mut ty_start_attempted,
            project_root,
            python_env,
            &path,
            &scan.source,
            &scan.pending,
            &mut ty_file_cache,
            &mut diagnostics,
            Some(TyFixes {
                insertions: &mut insertions,
                fixed_calls: &mut fixed_calls,
            }),
        )?;
        if let Some(fixed) = plan_rewrite_insertions(&path, &scan.source, &insertions)? {
            fixed_total += fixed_calls;
            results.push(FileFix {
                path,
                original: scan.source,
                fixed,
                count: fixed_calls,
            });
        }
    }
    results.sort_by_key(|fix| fix.path.clone());
    // Each violation pushes exactly one diagnostic, then is rewritten or not;
    // the ty fallback only ever adds diagnostics. So the un-rewritten count
    // is the total detected minus the total rewritten. `saturating_sub` is
    // defensive — `fixed_total` can never exceed the diagnostic count.
    let declined = diagnostics.len().saturating_sub(fixed_total);
    Ok(FixOutcome {
        files: results,
        declined,
    })
}

/// `typing` / `typing_extensions` *special-form* constructors whose name is
/// supplied as a positional string literal.
///
/// `TypeVar`/`ParamSpec`/`TypeVarTuple`/`NewType`/`TypeAliasType` are PEP 484 /
/// 612 / 646 / 695 special forms: the first argument must be a string literal
/// equal to the assigned variable so checkers can resolve them statically. A
/// generic keyword-rewrite never captures that literal/name-match half of the
/// contract, and the keyword form is non-idiomatic and was explicitly declined
/// upstream (python/typeshed#15804) — typeshed deliberately keeps these params
/// positional-or-keyword to mirror runtime, and the checkers special-case the
/// call and ignore the stub. So exempt them regardless of the resolved
/// signature.
///
/// Checker behaviour on `ParamSpec(name="P")` varies (it is not, as once
/// assumed, universally rejected): pyright accepts all five forms, ty accepts
/// all but `NewType(name=, tp=)`, and mypy rejects all five with *"expects a
/// string literal as first argument"*. mypy's blanket rejection is a tracked
/// mypy bug (python/mypy#20468), but this exemption must outlive its fix:
/// older mypy stays in use for years, ty still rejects `NewType` kwargs, and
/// the literal/name-match contract above is the durable reason regardless.
fn is_typing_special_form_constructor(fullname: &str) -> bool {
    // The diagnostic may target the class itself (`typing.ParamSpec`), its
    // constructor (`...__init__` / `...__new__`), or — for `NewType`, a class
    // on 3.10+ — its `__call__`.
    let core = fullname
        .strip_suffix(".__init__")
        .or_else(|| fullname.strip_suffix(".__new__"))
        .or_else(|| fullname.strip_suffix(".__call__"))
        .unwrap_or(fullname);
    let Some((module, name)) = core.rsplit_once('.') else {
        return false;
    };
    // The built-in resolver yields the real module; the ty fallback
    // synthesizes `ty.<…>` names (see `resolve_def_at`). typeshed defines
    // these only in `typing` / `typing_extensions`.
    let module_ok = matches!(module, "typing" | "typing_extensions" | "ty");
    module_ok
        && matches!(
            name,
            "TypeVar" | "ParamSpec" | "TypeVarTuple" | "NewType" | "TypeAliasType"
        )
}

/// Format like mypy: ``"func"`` or ``"method" of "C"``.
fn format_callee_display(fullname: &str) -> String {
    let Some((parent, method)) = fullname.rsplit_once('.') else {
        return format!("\"{fullname}\"");
    };
    if method == "__init__" || method == "__new__" {
        // Constructor: report the class name (``"str"``), as mypy does.
        let class = parent.rsplit('.').next().unwrap_or(parent);
        return format!("\"{class}\"");
    }
    if parent.contains('.') {
        let class = parent.rsplit('.').next().unwrap_or(parent);
        format!("\"{method}\" of \"{class}\"")
    } else {
        format!("\"{method}\"")
    }
}

/// Whether byte `offset` falls within an identifier's range.
fn ident_hit(ident: &ast::Identifier, offset: usize) -> bool {
    let range = ident.range();
    offset >= range.start().to_usize() && offset < range.end().to_usize()
}

type FnEntry<'a> = (Option<String>, &'a StmtFunctionDef);

/// Collect every function (with its immediate enclosing class name) and class
/// defined in `stmts`, recursing through classes and control-flow blocks
/// (typeshed gates defs behind `if sys.version_info`).
//
// ty-fallback helper: in production this is reached only through the
// excluded `resolve_pending_with_ty` glue; its behaviour is verified by the
// `#[coverage(off)]` unit tests. Excluded from the gate as part of the ty
// fallback layer (see `lib.rs` `mod ty_resolver`).
#[cfg_attr(coverage, coverage(off))]
fn collect_defs<'a>(
    stmts: &'a [Stmt],
    class: Option<&str>,
    funcs: &mut Vec<FnEntry<'a>>,
    classes: &mut Vec<&'a StmtClassDef>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(f) => {
                funcs.push((class.map(str::to_string), f));
                collect_defs(&f.body, None, funcs, classes);
            }
            Stmt::ClassDef(c) => {
                classes.push(c);
                collect_defs(&c.body, Some(c.name.as_str()), funcs, classes);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_defs(body, class, funcs, classes);
                for clause in elif_else_clauses {
                    collect_defs(&clause.body, class, funcs, classes);
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_defs(body, class, funcs, classes);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_defs(&h.body, class, funcs, classes);
                }
                collect_defs(orelse, class, funcs, classes);
                collect_defs(finalbody, class, funcs, classes);
            }
            Stmt::With(ast::StmtWith { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::While(ast::StmtWhile { body, .. }) => {
                collect_defs(body, class, funcs, classes);
            }
            _ => {}
        }
    }
}

/// Given the byte offset ty resolved a callee to, find the function (or class
/// constructor) defined there and return its synthetic fullname plus all
/// overload signatures (most-permissive overload wins downstream).
// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn resolve_def_at(stmts: &[Stmt], offset: usize) -> Option<(String, Vec<Signature>)> {
    let mut funcs: Vec<FnEntry> = Vec::new();
    let mut classes: Vec<&StmtClassDef> = Vec::new();
    collect_defs(stmts, None, &mut funcs, &mut classes);

    if let Some((class, target)) = funcs.iter().find(|(_, f)| ident_hit(&f.name, offset)) {
        let name = target.name.as_str();
        let overloads: Vec<Signature> = funcs
            .iter()
            .filter(|(c, f)| c.as_deref() == class.as_deref() && f.name.as_str() == name)
            .map(|(_, f)| signature_from_parameters(&f.parameters))
            .collect();
        let fullname = match class {
            Some(c) if name == "__init__" || name == "__new__" => {
                format!("ty.{c}.__init__")
            }
            Some(c) => format!("ty.{c}.{name}"),
            None => format!("ty.{name}"),
        };
        return Some((fullname, overloads));
    }

    // ty pointed at a class itself: a constructor call.
    if let Some(class) = classes.iter().find(|c| ident_hit(&c.name, offset)) {
        for ctor in ["__init__", "__new__"] {
            let sigs: Vec<Signature> = class
                .body
                .iter()
                .filter_map(|s| match s {
                    Stmt::FunctionDef(f) if f.name.as_str() == ctor => {
                        Some(signature_from_parameters(&f.parameters))
                    }
                    _ => None,
                })
                .collect();
            if !sigs.is_empty() {
                return Some((format!("ty.{}.__init__", class.name.as_str()), sigs));
            }
        }
    }
    None
}

/// The identifier starting at byte `offset` in `source` (the callee name, for
/// the diagnostic display when hover gave an unnamed callable type).
// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn identifier_at(source: &str, offset: usize) -> Option<String> {
    let rest = source.get(offset..)?;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

struct CallAtStart<'a> {
    start: usize,
    call: Option<&'a ast::ExprCall>,
}

#[cfg_attr(coverage, coverage(off))]
impl<'a> Visitor<'a> for CallAtStart<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.call.is_some() {
            return;
        }
        if let Expr::Call(call) = expr {
            if call.start().to_usize() == self.start {
                self.call = Some(call);
                return;
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg_attr(coverage, coverage(off))]
fn call_at_start(suite: &[Stmt], start: usize) -> Option<&ast::ExprCall> {
    let mut locator = CallAtStart { start, call: None };
    for stmt in suite {
        locator.visit_stmt(stmt);
        if locator.call.is_some() {
            break;
        }
    }
    locator.call
}

/// Parse a ty-reported parameter list (`a: int, b: int = ..., /`) into a
/// signature by reusing the real parser. `None` if it doesn't parse.
// Only reached from the (excluded) `resolve_pending_with_ty` ty path, and
// the synthesized `def __sk__(...)` always parses to a single
// `FunctionDef`, so the non-`FunctionDef` arm is unreachable. Behaviour is
// unit-tested in `signature_from_param_text_parses_or_fails`; exclude it
// from the gate for the same reasons as the rest of the ty glue.
#[cfg_attr(coverage, coverage(off))]
fn signature_from_param_text(params: &str) -> Option<Signature> {
    let src = format!("def __sk__({params}): ...\n");
    let parsed = parse_module(&src).ok()?;
    parsed.suite().iter().find_map(|stmt| match stmt {
        Stmt::FunctionDef(f) => Some(signature_from_parameters(&f.parameters)),
        _ => None,
    })
}

/// ty renders a *bound* call's receiver away (`"x".upper()` -> `def upper()`,
/// `bound method T.m(...)`) but leaves an *unbound* method's leading
/// `self`/`cls` intact (`str.lower(key)` -> `def lower(self: ...)`). A leading
/// `self`/`cls` therefore means the call passes the receiver explicitly: that
/// argument binds to the receiver parameter and must not be counted against
/// the positional limit. Drop the parameter and the receiver argument so only
/// the *remaining* positional arguments are checked (issue #15).
///
/// Only `def …` hovers (`is_def_hover`, i.e. ty reported no owner) can carry an
/// unstripped receiver. A `bound method Owner.m(...)` hover already had its
/// receiver removed by ty, so its leading parameter is genuine — stripping it
/// would corrupt the count for a method whose first non-receiver parameter is
/// itself literally named `self`/`cls` (e.g. `def m(self, cls, x)`).
// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn strip_unbound_receiver(
    signature: Signature,
    positional_count: usize,
    is_def_hover: bool,
) -> (Signature, usize, bool) {
    let first_is_receiver = is_def_hover
        && signature
            .parameters
            .first()
            .and_then(|p| p.name.as_deref())
            .is_some_and(|name| name == "self" || name == "cls");
    if !first_is_receiver {
        return (signature, positional_count, false);
    }
    let mut parameters = signature.parameters;
    parameters.remove(0);
    (
        Signature { parameters },
        positional_count.saturating_sub(1),
        true,
    )
}

/// Drop a leading `self` parameter — the explicitly-passed receiver of an
/// unbound `Class.method(receiver, …)` call (issue #27), the built-in
/// resolver analogue of [`strip_unbound_receiver`]. The caller has already
/// established (via [`CallChecker::is_unbound_class_method_call`]) that the
/// first parameter is `self`; anything else is returned unchanged.
fn without_leading_self(signature: &Signature) -> Signature {
    match signature.parameters.split_first() {
        Some((first, rest)) if first.name.as_deref() == Some("self") => Signature {
            parameters: rest.to_vec(),
        },
        _ => signature.clone(),
    }
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn violation_max_positional(
    fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
) -> Option<usize> {
    if is_typing_special_form_constructor(fullname) {
        return None;
    }
    if signatures.is_empty()
        || signatures
            .iter()
            .any(|s| !call_exceeds_positional_limit(s, fullname, false, positional_count))
    {
        return None;
    }
    Some(
        signatures
            .iter()
            .filter_map(|s| s.max_positional_at_call_site(fullname, false))
            .max()
            .unwrap_or(0),
    )
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn emit_if_violation(
    fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
    source: &str,
    call_start: usize,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<usize> {
    let max_positional = violation_max_positional(fullname, signatures, positional_count)?;
    let offset = u32::try_from(call_start).unwrap_or(u32::MAX);
    let (line, column) = line_column(source, TextSize::new(offset));
    diagnostics.push(Diagnostic {
        path: path.to_path_buf(),
        line,
        column,
        callee: format_callee_display(fullname),
        positional_count,
        max_positional,
    });
    Some(max_positional)
}

#[cfg_attr(coverage, coverage(off))]
fn signature_is_fully_named(signature: &Signature) -> bool {
    signature
        .parameters
        .iter()
        .all(|param| param.name.as_deref().is_some_and(|name| !name.is_empty()))
}

#[cfg_attr(coverage, coverage(off))]
fn ty_call_fix_insertions(
    source: &str,
    pending: &PendingTy,
    callee_fullname: &str,
    signature: &Signature,
    max_positional: usize,
    positional_count: usize,
    receiver_is_explicit: bool,
) -> Option<Vec<Insertion>> {
    if !signature_is_fully_named(signature) {
        return None;
    }
    let parsed = parse_module(source).ok()?;
    let call = call_at_start(parsed.suite(), pending.call_start)?;
    // Ty hovers are already call-site oriented for bound methods, so avoid
    // the built-in resolver's attribute-name receiver heuristic here. The one
    // exception is an unbound `def` hover with leading `self`/`cls`, where
    // `strip_unbound_receiver` proved the first positional is explicit.
    call_fix_insertions(
        call,
        parsed.tokens(),
        callee_fullname,
        signature,
        max_positional,
        positional_count,
        false,
        receiver_is_explicit,
    )
}

#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "threads the resolved ty call facts into the existing fixer \
              insertion helper"
)]
fn record_ty_fix(
    fixes: &mut Option<TyFixes<'_>>,
    source: &str,
    pending: &PendingTy,
    callee_fullname: &str,
    signature: &Signature,
    max_positional: usize,
    positional_count: usize,
    receiver_is_explicit: bool,
) {
    let Some(fixes) = fixes.as_mut() else {
        return;
    };
    let Some(insertions) = ty_call_fix_insertions(
        source,
        pending,
        callee_fullname,
        signature,
        max_positional,
        positional_count,
        receiver_is_explicit,
    ) else {
        return;
    };
    fixes.insertions.extend(insertions);
    *fixes.fixed_calls += 1;
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn hover_text(value: &serde_json::Value) -> Option<String> {
    let contents = value.get("contents")?;
    if let Some(s) = contents.as_str() {
        return Some(s.to_string());
    }
    contents
        .get("value")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Fail unless a usable `ty` executable is on `PATH`.
///
/// `ty` is a hard requirement (see [`check_paths`]); this is the cheap,
/// content-independent up-front probe (`ty version`). The actual server is
/// still started lazily — only when a file has calls the built-in resolver
/// could not resolve — so a fully-resolvable run never pays ty's
/// project-indexing startup cost (issue #31).
///
/// The probe result is memoized for the process: `ty`'s presence on `PATH`
/// cannot change mid-run, the real CLI calls this once anyway, and
/// memoizing keeps the benchmark suite (which calls `check_paths` many times
/// per process, issue #30) measuring the resolver rather than repeated
/// `ty version` subprocess spawns.
///
/// Excluded from the coverage gate for the same reason as [`start_ty`]: the
/// gate environment guarantees `ty` is present (`coverage.yml` asserts `ty
/// version`), so the `Err` arm cannot be taken there. Its error value is
/// covered directly by `error.rs`' unit tests and end-to-end by the
/// `ty`-absent CLI test (which runs the binary with `ty` stripped from
/// `PATH`).
#[cfg_attr(coverage, coverage(off))]
fn require_ty_present() -> Result<(), CheckError> {
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *PRESENT.get_or_init(ty_binary_present) {
        Ok(())
    } else {
        Err(CheckError::TyNotFound)
    }
}

/// Per-file ty-fallback driver: lazily start `ty` on first need, then
/// resolve this file's pending calls. The lazy-start and `ty`-available
/// branches depend on the (environment-specific) `ty` subprocess, so this
/// wiring is excluded from the gate like the rest of the ty fallback;
/// behaviour is covered by the ty-backed integration tests.
#[cfg_attr(coverage, coverage(off))]
#[allow(clippy::too_many_arguments)]
fn resolve_file_with_ty(
    ty: &mut Option<TyResolver>,
    ty_start_attempted: &mut bool,
    project_root: &Path,
    python_env: Option<&Path>,
    path: &Path,
    source: &str,
    pending: &[PendingTy],
    ty_file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    diagnostics: &mut Vec<Diagnostic>,
    fixes: Option<TyFixes<'_>>,
) -> Result<(), CheckError> {
    if pending.is_empty() {
        return Ok(());
    }
    if !*ty_start_attempted {
        *ty_start_attempted = true;
        // The binary was verified up front (`require_ty_present`), so a
        // failure here is the server not starting — fatal, not a silent
        // downgrade, so results stay deterministic.
        *ty = Some(start_ty(project_root, python_env)?);
    }
    if let Some(ty) = ty.as_mut() {
        resolve_pending_with_ty(ty, path, source, pending, ty_file_cache, diagnostics, fixes);
    }
    Ok(())
}

/// Start the `ty` language server once. `ty`'s binary is verified up front
/// ([`require_ty_present`]); if the server still cannot be launched the run
/// fails with [`CheckError::TyServerFailed`] rather than silently dropping
/// the (now required) inference backend.
///
/// Like [`TyResolver::start`], this is `ty`-subprocess orchestration whose
/// outcome is environment-specific (the coverage gate guarantees `ty` is
/// present and startable, so the failure path cannot be taken there), so it
/// is excluded from the gate.
#[cfg_attr(coverage, coverage(off))]
fn start_ty(project_root: &Path, python_env: Option<&Path>) -> Result<TyResolver, CheckError> {
    TyResolver::start(project_root, python_env).ok_or(CheckError::TyServerFailed)
}

struct TyFixes<'a> {
    insertions: &'a mut Vec<Insertion>,
    fixed_calls: &'a mut usize,
}

/// Resolve, in one pipelined batch per file, the calls the built-in resolver
/// missed: hover (precise, overload- and inheritance-resolved, stdlib too),
/// then goto-definition for the rest (constructors). Fails closed.
///
/// This is pure orchestration of the `ty` LSP subprocess: it pipelines
/// hover/goto-definition requests and dispatches each reply to the parsing
/// and emission logic. Its outcome depends on `ty`'s own
/// version-/environment-specific resolution (the suite asserts ty behaviour
/// only weakly for the same reason), so — like [`TyResolver::start`] — it is
/// excluded from the coverage gate. Every piece of decision logic it calls is
/// unit-tested deterministically instead: [`hover_text`],
/// [`parse_hover_signature`], [`signature_from_param_text`],
/// [`parse_callable_type_overloads`], [`strip_unbound_receiver`],
/// [`identifier_at`], [`byte_offset_to_lsp`], [`lsp_to_byte_offset`],
/// [`location_from_value`], [`resolve_def_at`] and [`emit_if_violation`].
#[cfg_attr(coverage, coverage(off))]
fn resolve_pending_with_ty(
    ty: &mut TyResolver,
    path: &Path,
    source: &str,
    pending: &[PendingTy],
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    diagnostics: &mut Vec<Diagnostic>,
    mut fixes: Option<TyFixes<'_>>,
) {
    if pending.is_empty() || ty.ensure_open(path, source).is_none() {
        return;
    }

    // Phase A: pipeline all hover requests, then collect.
    let hover_ids: Vec<Option<i64>> = pending
        .iter()
        .map(|p| {
            let (line, ch) = byte_offset_to_lsp(source, p.callee_offset);
            ty.ask("textDocument/hover", path, line, ch)
        })
        .collect();

    let mut needs_def: Vec<usize> = Vec::new();
    for (i, p) in pending.iter().enumerate() {
        let raw = hover_ids[i]
            .and_then(|id| ty.take(id))
            .as_ref()
            .and_then(hover_text);
        let Some(raw) = raw else {
            needs_def.push(i);
            continue;
        };

        // `def …`/`bound method …` display: a single, named signature.
        if let Some(sig) = parse_hover_signature(&raw) {
            let Some(signature) = signature_from_param_text(&sig.params) else {
                continue;
            };
            let (effective_signature, positional_count, receiver_is_explicit) =
                strip_unbound_receiver(signature.clone(), p.positional_count, sig.owner.is_none());
            let fullname = match &sig.owner {
                Some(owner) => {
                    let owner = owner.split('[').next().unwrap_or(owner);
                    let owner = owner.rsplit('.').next().unwrap_or(owner);
                    format!("ty.{owner}.{}", sig.name)
                }
                None => format!("ty.{}", sig.name),
            };
            if let Some(max_positional) = emit_if_violation(
                &fullname,
                std::slice::from_ref(&effective_signature),
                positional_count,
                source,
                p.call_start,
                path,
                diagnostics,
            ) {
                let fix_signature = if receiver_is_explicit {
                    &signature
                } else {
                    &effective_signature
                };
                let fix_positional_count = if receiver_is_explicit {
                    p.positional_count
                } else {
                    positional_count
                };
                record_ty_fix(
                    &mut fixes,
                    source,
                    p,
                    &fullname,
                    fix_signature,
                    max_positional,
                    fix_positional_count,
                    receiver_is_explicit,
                );
            }
            continue;
        }

        // Callable-*type* display, incl. `Overload[…]`: ty already excluded
        // `self` and kept typeshed positional-only `/` markers. Use it
        // directly rather than falling through to goto-definition, which on
        // an inferred stdlib receiver lands on runtime `.py` source whose
        // signatures drop `/` and yield false positives (issue #14).
        let overloads: Vec<Signature> = parse_callable_type_overloads(&raw)
            .iter()
            .filter_map(|params| signature_from_param_text(params))
            .collect();
        if overloads.is_empty() {
            needs_def.push(i);
            continue;
        }
        let name = identifier_at(source, p.callee_offset).unwrap_or_default();
        let fullname = format!("ty.{name}");
        if let Some(max_positional) = emit_if_violation(
            &fullname,
            &overloads,
            p.positional_count,
            source,
            p.call_start,
            path,
            diagnostics,
        ) {
            if let [signature] = overloads.as_slice() {
                record_ty_fix(
                    &mut fixes,
                    source,
                    p,
                    &fullname,
                    signature,
                    max_positional,
                    p.positional_count,
                    false,
                );
            }
        }
    }

    // Phase B: pipeline goto-definition for hover misses (constructors).
    let def_ids: Vec<(usize, Option<i64>)> = needs_def
        .iter()
        .map(|&i| {
            let (line, ch) = byte_offset_to_lsp(source, pending[i].callee_offset);
            (i, ty.ask("textDocument/definition", path, line, ch))
        })
        .collect();
    for (i, id) in def_ids {
        let Some(loc) = id
            .and_then(|id| ty.take(id))
            .as_ref()
            .and_then(location_from_value)
        else {
            continue;
        };
        let target = if same_path(&loc.path, path) {
            Some(source.to_string())
        } else {
            file_cache
                .entry(loc.path.clone())
                .or_insert_with(|| std::fs::read_to_string(&loc.path).ok())
                .clone()
        };
        let Some(target) = target else { continue };
        // A `ty` goto-definition target is a dependency/stub. Use the guarded
        // parser so a deeply-nested target is rejected gracefully rather than
        // crashing the analysis thread (issue #83 follow-up to #54). The
        // two-stage pre-filter keeps typical stubs cheap (byte count only);
        // only genuinely deep ones pay the tokeniser scan — and those would
        // have crashed the old unguarded call. A too-deep or unparsable
        // target is silently skipped, same fail-closed behaviour as before.
        let Ok(parsed) = parse_module_guarded(&target) else {
            continue;
        };
        let Some(off) = lsp_to_byte_offset(&target, loc.line, loc.character) else {
            continue;
        };
        if let Some((fullname, sigs)) = resolve_def_at(parsed.suite(), off) {
            emit_if_violation(
                &fullname,
                &sigs,
                pending[i].positional_count,
                source,
                pending[i].call_start,
                path,
                diagnostics,
            );
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::{
        is_ignored_path, is_typing_special_form_constructor, record_ty_fix,
        signature_is_fully_named, strip_unbound_receiver, without_leading_self, PendingTy, TyFixes,
    };
    use crate::signature::{Parameter, ParameterKind, Signature};
    use std::path::Path;

    #[test]
    fn ignored_path_rejects_dot_venv_and_pycache_components() {
        // A path is ignored iff some component is dot-prefixed, `venv`, or
        // `__pycache__` — each arm of the rule, in turn. Directory walks now
        // prune these trees before their files are tested, so this is the
        // direct contract that keeps the explicit-file-path case correct.
        assert!(is_ignored_path(Path::new(".venv/lib/python3.12/x.py")));
        assert!(is_ignored_path(Path::new("pkg/venv/mod.py")));
        assert!(is_ignored_path(Path::new("pkg/__pycache__/mod.py")));
        // No special component anywhere: kept. Also exercises a non-`Normal`
        // (root) component falling through to the catch-all arm.
        assert!(!is_ignored_path(Path::new("/srv/app/src/pkg/mod.py")));
    }

    #[test]
    fn exempts_typing_special_forms_in_every_resolved_form() {
        // Built-in resolver: real module, class / constructor / `__call__`.
        for fullname in [
            "typing.TypeVar",
            "typing.ParamSpec.__init__",
            "typing.TypeVarTuple.__new__",
            "typing.NewType.__call__",
            "typing.TypeAliasType",
            "typing_extensions.ParamSpec",
            // ty fallback synthesizes `ty.<…>` names.
            "ty.ParamSpec",
            "ty.NewType.__call__",
        ] {
            assert!(
                is_typing_special_form_constructor(fullname),
                "{fullname} should be exempt (issue #19)"
            );
        }
    }

    #[test]
    fn does_not_exempt_unrelated_callables() {
        for fullname in [
            "typing.cast",
            "typing.NamedTuple",
            "mypkg.TypeVar.__init__",
            "TypeVar",
        ] {
            assert!(
                !is_typing_special_form_constructor(fullname),
                "{fullname} must not be exempt"
            );
        }
    }

    fn sig(names: &[&str]) -> Signature {
        Signature {
            parameters: names
                .iter()
                .map(|n| Parameter {
                    name: Some((*n).to_string()),
                    kind: ParameterKind::PositionalOrKeyword,
                })
                .collect(),
        }
    }

    #[test]
    fn strips_leading_self_and_the_explicit_receiver() {
        // `str.lower(key)`: ty hover keeps `self`; the explicit receiver fills
        // it and must not count (issue #15).
        let (s, count, stripped) = strip_unbound_receiver(sig(&["self"]), 1, true);
        assert!(s.parameters.is_empty());
        assert_eq!(count, 0);
        assert!(stripped);
    }

    #[test]
    fn strips_leading_cls() {
        let (s, count, stripped) = strip_unbound_receiver(sig(&["cls", "a"]), 2, true);
        assert_eq!(s.parameters.len(), 1);
        assert_eq!(s.parameters[0].name.as_deref(), Some("a"));
        assert_eq!(count, 1);
        assert!(stripped);
    }

    #[test]
    fn leaves_bound_signature_untouched() {
        // ty already dropped the receiver for a bound call (`def upper()` /
        // `bound method T.m(...)`): no leading `self`/`cls`, nothing to strip.
        let (s, count, stripped) = strip_unbound_receiver(sig(&["a", "b"]), 1, true);
        assert_eq!(s.parameters.len(), 2);
        assert_eq!(count, 1);
        assert!(!stripped);
    }

    #[test]
    fn without_leading_self_drops_only_a_leading_self() {
        // Issue #27: `K.m(K(), 1)` — the resolved instance method's `self`
        // is filled by the explicit receiver and must not be counted.
        let s = without_leading_self(&sig(&["self", "a"]));
        assert_eq!(s.parameters.len(), 1);
        assert_eq!(s.parameters[0].name.as_deref(), Some("a"));

        // No leading `self` (e.g. a staticmethod): untouched.
        assert_eq!(without_leading_self(&sig(&["a", "b"])).parameters.len(), 2);
        // `cls` is auto-bound even through the class: not a stripped receiver.
        assert_eq!(
            without_leading_self(&sig(&["cls", "a"])).parameters.len(),
            2
        );
        assert!(without_leading_self(&sig(&[])).parameters.is_empty());
    }

    #[test]
    fn keeps_cls_parameter_of_bound_method_hover() {
        // `bound method Owner.m(...)` (owner present -> `is_def_hover` false):
        // ty already stripped the real receiver, so a leading parameter that
        // happens to be named `cls`/`self` (`def m(self, cls, x)`) is genuine
        // and must not be dropped (PR #17 review).
        let (s, count, stripped) = strip_unbound_receiver(sig(&["cls", "x"]), 2, false);
        assert_eq!(s.parameters.len(), 2);
        assert_eq!(s.parameters[0].name.as_deref(), Some("cls"));
        assert_eq!(count, 2);
        assert!(!stripped);
    }

    #[test]
    fn saturates_when_no_explicit_receiver_argument() {
        // Defensive: a leading `self` with zero positional args (e.g. a
        // keyword-only / malformed call) must not underflow the count.
        let (s, count, stripped) = strip_unbound_receiver(sig(&["self"]), 0, true);
        assert!(s.parameters.is_empty());
        assert_eq!(count, 0);
        assert!(stripped);
    }

    #[test]
    fn signature_full_name_check_covers_decline_shapes() {
        assert!(signature_is_fully_named(&sig(&["a", "b"])));
        assert!(signature_is_fully_named(&Signature {
            parameters: Vec::new(),
        }));
        assert!(!signature_is_fully_named(&Signature {
            parameters: vec![Parameter {
                name: None,
                kind: ParameterKind::PositionalOrKeyword,
            }],
        }));
        assert!(!signature_is_fully_named(&Signature {
            parameters: vec![Parameter {
                name: Some(String::new()),
                kind: ParameterKind::PositionalOrKeyword,
            }],
        }));
    }

    #[test]
    fn ty_fix_recording_decline_branches_are_explicit() {
        let pending = PendingTy {
            callee_offset: 0,
            call_start: 0,
            positional_count: 1,
        };
        let named = sig(&["a"]);

        // `check_paths` passes no fix context. The ty path still considers
        // the violation, but rewrite recording must be a no-op.
        let mut no_fix_context = None;
        record_ty_fix(
            &mut no_fix_context,
            "f(1)\n",
            &pending,
            "ty.f",
            &named,
            0,
            1,
            false,
        );

        // A fix run may also have a context but an unsafe signature mapping
        // (for example an unnamed parameter). It remains declined without
        // recording a call or insertion.
        let unnamed = Signature {
            parameters: vec![Parameter {
                name: None,
                kind: ParameterKind::PositionalOrKeyword,
            }],
        };
        let mut insertions = Vec::new();
        let mut fixed_calls = 0usize;
        let mut fixes = Some(TyFixes {
            insertions: &mut insertions,
            fixed_calls: &mut fixed_calls,
        });
        record_ty_fix(
            &mut fixes, "f(1)\n", &pending, "ty.f", &unnamed, 0, 1, false,
        );
        assert!(insertions.is_empty());
        assert_eq!(fixed_calls, 0);
    }

    // ----- ty goto-definition resolution internals --------------------

    use super::{
        collect_defs, format_callee_display, identifier_at, resolve_def_at,
        signature_from_param_text,
    };
    use ruff_python_ast::{StmtClassDef, StmtFunctionDef};
    use ruff_python_parser::parse_module;

    #[test]
    fn collect_defs_recurses_every_control_flow_form() {
        // A def at module level, nested in a fn, in a class, and in every
        // control-flow form (if/elif/else, try/except/else/finally, with,
        // for, while).
        let src = "\
def top():
    def inner():
        ...

class K:
    def m(self):
        ...

if a:
    def in_if():
        ...
elif b:
    def in_elif():
        ...
else:
    def in_else():
        ...

try:
    def in_try():
        ...
except Exception:
    def in_except():
        ...
else:
    def in_try_else():
        ...
finally:
    def in_finally():
        ...

with ctx() as c:
    def in_with():
        ...

for i in xs:
    def in_for():
        ...

while cond:
    def in_while():
        ...
";
        let parsed = parse_module(src).expect("parse");
        let mut funcs: Vec<(Option<String>, &StmtFunctionDef)> = Vec::new();
        let mut classes: Vec<&StmtClassDef> = Vec::new();
        collect_defs(parsed.suite(), None, &mut funcs, &mut classes);

        let names: Vec<&str> = funcs.iter().map(|(_, f)| f.name.as_str()).collect();
        for expected in [
            "top",
            "inner",
            "m",
            "in_if",
            "in_elif",
            "in_else",
            "in_try",
            "in_except",
            "in_try_else",
            "in_finally",
            "in_with",
            "in_for",
            "in_while",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
        // `m` is recorded with its enclosing class.
        assert!(funcs
            .iter()
            .any(|(c, f)| c.as_deref() == Some("K") && f.name.as_str() == "m"));
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name.as_str(), "K");
    }

    fn resolve_at(src: &str, needle: &str) -> Option<(String, usize)> {
        let parsed = parse_module(src).expect("parse");
        let offset = src.find(needle).expect("needle");
        resolve_def_at(parsed.suite(), offset).map(|(name, sigs)| (name, sigs.len()))
    }

    #[test]
    fn resolve_def_at_free_function_and_dunder_without_class() {
        assert_eq!(
            resolve_at("def foo(a, b):\n    ...\n", "foo("),
            Some(("ty.foo".to_string(), 1))
        );
        // A module-level `__new__` has no class: falls to the `ty.<name>` arm.
        assert_eq!(
            resolve_at("def __new__(cls):\n    ...\n", "__new__"),
            Some(("ty.__new__".to_string(), 1))
        );
    }

    #[test]
    fn resolve_def_at_method_and_constructor_names() {
        assert_eq!(
            resolve_at("class C:\n    def mth(self, a):\n        ...\n", "mth"),
            Some(("ty.C.mth".to_string(), 1))
        );
        assert_eq!(
            resolve_at(
                "class C:\n    def __init__(self, a):\n        ...\n",
                "__init__"
            ),
            Some(("ty.C.__init__".to_string(), 1))
        );
    }

    #[test]
    fn resolve_def_at_collects_overloads() {
        let src = "class C:\n    def f(self, a): ...\n    def f(self, a, b): ...\n";
        assert_eq!(
            resolve_at(src, "f(self, a):"),
            Some(("ty.C.f".to_string(), 2))
        );
    }

    #[test]
    fn resolve_def_at_class_name_resolves_constructor() {
        // Offset on the class identifier (not a method) -> constructor path.
        assert_eq!(
            resolve_at("class Kx:\n    def __init__(self, a):\n        ...\n", "Kx"),
            Some(("ty.Kx.__init__".to_string(), 1))
        );
        // Only `__new__` present: the ctor loop's second iteration.
        assert_eq!(
            resolve_at("class Nw:\n    def __new__(cls):\n        ...\n", "Nw"),
            Some(("ty.Nw.__init__".to_string(), 1))
        );
    }

    #[test]
    fn resolve_def_at_returns_none_when_offset_hits_nothing() {
        // Offset on the leading newline: no identifier there.
        assert_eq!(resolve_at("\n\ndef f():\n    ...\n", "\n"), None);
        // A class with no constructor: the ctor loop yields nothing.
        assert_eq!(resolve_at("class Empty:\n    x = 1\n", "Empty"), None);
    }

    #[test]
    fn identifier_at_extracts_or_rejects() {
        assert_eq!(identifier_at("ab.cd", 0).as_deref(), Some("ab"));
        assert_eq!(identifier_at("ab.cd", 3).as_deref(), Some("cd"));
        // Offset on a non-identifier byte.
        assert_eq!(identifier_at("(z", 0), None);
        // Offset past the end of the source.
        assert_eq!(identifier_at("x", 5), None);
    }

    #[test]
    fn signature_from_param_text_parses_or_fails() {
        let sig = signature_from_param_text("a: int, b: str = 'x'").expect("sig");
        assert_eq!(sig.parameters.len(), 2);
        assert!(signature_from_param_text("def").is_none());
    }

    #[test]
    fn format_callee_display_covers_every_shape() {
        assert_eq!(format_callee_display("foo"), "\"foo\"");
        assert_eq!(format_callee_display("a.b.__init__"), "\"b\"");
        assert_eq!(format_callee_display("a.b.__new__"), "\"b\"");
        assert_eq!(format_callee_display("pkg.mod.func"), "\"func\" of \"mod\"");
        assert_eq!(format_callee_display("mod.func"), "\"func\"");
    }

    #[test]
    fn emit_if_violation_emits_only_on_a_real_violation() {
        use super::emit_if_violation;
        use std::path::Path;

        // `def f(a)` allows zero positional args at the call site.
        let one = sig(&["a"]);
        let path = Path::new("m.py");

        // Special-form constructors are always exempt.
        let mut d = Vec::new();
        emit_if_violation(
            "ty.TypeVar",
            std::slice::from_ref(&one),
            2,
            "x",
            0,
            path,
            &mut d,
        );
        assert!(d.is_empty());

        // No signatures: nothing to check.
        let mut d = Vec::new();
        emit_if_violation("ty.f", &[], 2, "x", 0, path, &mut d);
        assert!(d.is_empty());

        // Within the limit (some overload permits it): no diagnostic.
        let mut d = Vec::new();
        emit_if_violation(
            "ty.f",
            std::slice::from_ref(&one),
            0,
            "f()\n",
            0,
            path,
            &mut d,
        );
        assert!(d.is_empty());

        // Exceeds the limit: one diagnostic with the rendered fields.
        let mut d = Vec::new();
        emit_if_violation("ty.f", &[one], 2, "f(1, 2)\n", 0, path, &mut d);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].line, 1);
        assert_eq!(d[0].column, 1);
        assert_eq!(d[0].callee, "\"f\"");
        assert_eq!(d[0].positional_count, 2);
        assert_eq!(d[0].max_positional, 0);
    }

    #[test]
    fn hover_text_reads_string_or_value_or_none() {
        use super::hover_text;
        use serde_json::json;

        // `contents` is a bare string.
        assert_eq!(
            hover_text(&json!({"contents": "plain"})).as_deref(),
            Some("plain")
        );
        // `contents.value` (MarkupContent form).
        assert_eq!(
            hover_text(&json!({"contents": {"value": "marked"}})).as_deref(),
            Some("marked")
        );
        // No `contents` at all.
        assert_eq!(hover_text(&json!({})), None);
        // `contents` present but neither a string nor a `.value` string.
        assert_eq!(hover_text(&json!({"contents": {"x": 1}})), None);
    }

    // ----- issue #27 unbound-class-method guard arms ------------------
    //
    // `is_unbound_class_method_call` / `binding_is_instance` are core
    // built-in-resolver logic, not ty-fallback helpers. The fixer's
    // `unbound_class_method_*` tests cover the happy path; these drive
    // each early-`return false` guard directly, so every branch is
    // visible to the coverage gate (issue #43).

    use super::CallChecker;
    use crate::config::Config;
    use crate::index::DefinitionIndex;
    use ruff_python_ast::Expr;
    use std::path::PathBuf;

    /// Build a bare `CallChecker`, let `setup` populate its scopes, then
    /// evaluate `is_unbound_class_method_call` for the single call in
    /// `call_src`. The index/config/tokens are inert: this predicate only
    /// reads `func`, `callee`, `first_param`, and the scope stack.
    fn is_unbound(
        call_src: &str,
        callee: &str,
        first_param: Option<&str>,
        setup: impl FnOnce(&mut CallChecker),
    ) -> bool {
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let checker_parsed = parse_module("").expect("parse empty");
        let mut checker = CallChecker::new(
            PathBuf::from("test.py"),
            "test".to_string(),
            false,
            "",
            checker_parsed.tokens(),
            &index,
            &config,
        );
        setup(&mut checker);

        let call_parsed = parse_module(call_src).expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = call_parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };
        checker.is_unbound_class_method_call(&call.func, callee, first_param)
    }

    #[test]
    fn unbound_guard_rejects_non_self_first_parameter() {
        // A classmethod / staticmethod / free function: `cls` (or anything
        // but `self`) is auto-bound or passes no receiver.
        assert!(!is_unbound("K.m(0)\n", "pkg.K.m", Some("cls"), |c| {
            c.define("K", "pkg.K".to_string());
        }));
    }

    #[test]
    fn unbound_guard_rejects_dunder_receiver() {
        // `Signature::max_positional_at_call_site` already drops a dunder's
        // leading receiver, so it must not be stripped a second time here.
        assert!(!is_unbound("K(0)\n", "pkg.K.__init__", Some("self"), |c| {
            c.define("K", "pkg.K".to_string());
        }));
    }

    #[test]
    fn unbound_guard_rejects_attribute_call_with_single_name_base_and_dunder() {
        // `K.__init__(self, 0)`: explicit-receiver call with a single-name
        // base and a dunder method — still excluded from unbound treatment
        // (issue #27: the receiver is already stripped by
        // `max_positional_at_call_site`; stripping it here would double-count).
        assert!(!is_unbound(
            "K.__init__(0)\n",
            "pkg.K.__init__",
            Some("self"),
            |c| {
                c.define("K", "pkg.K".to_string());
            }
        ));
    }

    #[test]
    fn unbound_guard_rejects_non_attribute_callee() {
        // A bare-name call (`f(0)`): no class object to call through.
        assert!(!is_unbound("f(0)\n", "test.f", Some("self"), |_| {}));
    }

    #[test]
    fn unbound_guard_rejects_non_name_base_when_head_is_not_a_module() {
        // `a.b.m(0)`: the base `a.b` is a multi-level attribute chain, but
        // `a` is not a known module, so we cannot confirm it is an unbound
        // class-object call.
        assert!(!is_unbound("a.b.m(0)\n", "pkg.x.m", Some("self"), |_| {}));
    }

    #[test]
    fn unbound_guard_rejects_call_expression_base() {
        // `f().m(0)`: the base is a call expression, not a dotted-name path,
        // so `dotted_path` returns None and we conservatively return false.
        assert!(!is_unbound("f().m(0)\n", "pkg.f.m", Some("self"), |_| {}));
    }

    #[test]
    fn unbound_guard_accepts_dotted_module_class_method_call() {
        // `mod.Class.method(self, 0)`: `mod` is a module; the base `mod.Class`
        // is a class reached through a module path, so the call is unbound
        // (issue #55 follow-up: multi-level dotted bases through modules).
        assert!(is_unbound(
            "mod.Class.method(self, 0)\n",
            "mod.Class.method",
            Some("self"),
            |c| {
                c.define_module("mod", "mod".to_string());
            }
        ));
    }

    #[test]
    fn unbound_guard_accepts_dotted_module_dunder_call() {
        // `mod.Class.__init__(self, 0)`: dunder through a module-accessed
        // class is still an unbound call. The single-name-base dunder
        // exclusion does not apply to multi-level chains.
        assert!(is_unbound(
            "mod.Class.__init__(self, 0)\n",
            "mod.Class.__init__",
            Some("self"),
            |c| {
                c.define_module("mod", "mod".to_string());
            }
        ));
    }

    #[test]
    fn unbound_guard_rejects_unresolved_base() {
        // `Unknown` is not bound in any scope.
        assert!(!is_unbound(
            "Unknown.m(0)\n",
            "test.Unknown.m",
            Some("self"),
            |_| {}
        ));
    }

    #[test]
    fn unbound_guard_rejects_callee_owned_by_a_different_class() {
        // `K` resolves, but the call's resolved callee is not `pkg.K.m`
        // (e.g. an inherited method owned by a base class): not the
        // class that directly owns `m`.
        assert!(!is_unbound("K.m(0)\n", "pkg.Base.m", Some("self"), |c| {
            c.define("K", "pkg.K".to_string());
        }));
    }

    #[test]
    fn unbound_guard_rejects_bound_instance_call() {
        // `k` is an *instance* of `pkg.K`: `k.m(…)` is an ordinary bound
        // call, the receiver is implicit (`binding_is_instance` is true).
        assert!(!is_unbound("k.m(0)\n", "pkg.K.m", Some("self"), |c| {
            c.record_instance("k", "pkg.K".to_string());
        }));
    }

    #[test]
    fn unbound_guard_accepts_class_object_call() {
        // `K.m(K(), …)` through the class object itself: the explicit
        // receiver fills `self` (issue #27).
        assert!(is_unbound("K.m(0)\n", "pkg.K.m", Some("self"), |c| {
            c.define("K", "pkg.K".to_string());
        }));
    }

    #[test]
    fn binding_is_instance_is_false_for_an_unbound_name() {
        // The guard's `name`-not-found fall-through: with `resolve_local`
        // already succeeding for every real caller, only a direct call
        // reaches it.
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let parsed = parse_module("").expect("parse empty");
        let checker = CallChecker::new(
            PathBuf::from("test.py"),
            "test".to_string(),
            false,
            "",
            parsed.tokens(),
            &index,
            &config,
        );
        assert!(!checker.binding_is_instance("never_bound"));
    }
}
