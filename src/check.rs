//! Check Python sources for positional calls that should use keywords.

use std::path::{Path, PathBuf};

use rayon::prelude::*;
use ruff_python_ast::token::{parenthesized_range, Tokens};
use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::{self as ast};
use ruff_python_ast::{AnyNodeRef, ExprRef, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_ast::{Expr, Number};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use rustc_hash::{FxHashMap, FxHashSet};

use ruff_text_size::TextSize;

use crate::ast_util::{
    line_column, line_column_from_starts, line_starts, positional_argument_count,
    signature_from_parameters,
};
use crate::cache::{compute_global_fingerprint, file_cache_key, DiagnosticCache};
use crate::config::{Config, SourceRoots};
use crate::diagnostic::Diagnostic;
use crate::error::CheckError;
use crate::fix::{apply_insertions, DeclinedFixReason, FixOptIns, Insertion};
use crate::index::{
    build_index_with_sources, is_package_init, module_name_for_path, relative_base,
    DefinitionIndex, IndexedFile,
};
use crate::limits::{parse_module_guarded, run_with_large_stack, with_large_stack_pool};
use crate::noqa::NoqaDirectives;
use crate::signature::{ParameterKind, Signature};
use crate::source::{read_python_source, Source};
use crate::ty_resolver::{
    locations_from_value, lsp_to_byte_offset, parse_callable_type_overloads, parse_hover_signature,
    same_path, ty_binary_present, LspLineIndex, TyResolver,
};

mod file_selection;
mod fix_runner;

pub use file_selection::is_prunable_dir;
use file_selection::{collect_python_files, explicit_python_files};
#[cfg(test)]
use file_selection::{is_ignored_path, FileSelection};
pub use fix_runner::{fix_paths, fix_paths_with_opt_ins};

/// Maximum fallback requests in flight at once. This must stay at 1: `ty
/// server` handles concurrent requests on a thread pool, and its answers for
/// multi-location symbols (re-exported classes, instance `__call__`) depend
/// on which thread populates its inference caches first — two runs feeding
/// byte-identical request streams returned different definitions whenever
/// more than one request was outstanding. Serial round-trips are the only
/// schedule-independent mode, and they cost nothing where it matters: the
/// `CPython` completeness run is bound by ty's per-query inference, not by
/// pipe round-trips (41.6s serial vs 44s with a 128-wide window).
const TY_MAX_IN_FLIGHT: usize = 1;

#[derive(Clone, Copy)]
enum IfBranchTraversal {
    Module,
    LocalBody,
    ClassBody,
}

fn decorator_tail(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(ast::ExprAttribute { attr, .. }) => Some(attr.as_str()),
        Expr::Call(ast::ExprCall { func, .. }) => decorator_tail(func),
        _ => None,
    }
}

fn expr_tail_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(ast::ExprAttribute { attr, .. }) => Some(attr.as_str()),
        _ => None,
    }
}

fn receiver_is_class_object(value: &Expr, class_fullname: &str) -> bool {
    match expr_tail_name(value) {
        Some(receiver_tail) => {
            let class_name = class_fullname.strip_prefix("ty.").unwrap_or(class_fullname);
            receiver_tail == class_name.rsplit('.').next().unwrap_or(class_name)
        }
        None => false,
    }
}

fn has_staticmethod_or_classmethod_decorator(decorator_list: &[ast::Decorator]) -> bool {
    decorator_list.iter().any(|decorator| {
        matches!(
            decorator_tail(&decorator.expression),
            Some("staticmethod" | "classmethod")
        )
    })
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
#[cfg_attr(coverage, coverage(off))]
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
/// either the skip-warning list ([`ScanOutcome::Skipped`]) or the ty work queue
/// ([`ScanOutcome::Scanned`]).
///
/// This is the gated business-logic counterpart to [`pipeline_phases`], which
/// handles the non-deterministic threading orchestration that cannot be covered.
fn process_scan_outcome_for_ty(
    i: usize,
    path: PathBuf,
    outcome: ScanOutcome,
    diagnostics: &mut Vec<Diagnostic>,
    skip_warnings: &mut Vec<(usize, PathBuf, String)>,
    ty_work: &mut Vec<PendingTyWork>,
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
                let source = retained_source_for_pending_scan(scan.source.as_deref())?.to_owned();
                ty_work.push(PendingTyWork {
                    path,
                    source,
                    pending: scan.pending,
                    pending_groups: scan.pending_groups,
                });
            }
        }
    }
    Ok(())
}

#[cfg_attr(coverage, coverage(off))]
fn retained_source_for_pending_scan(source: Option<&str>) -> Result<&str, CheckError> {
    source.ok_or_else(|| {
        CheckError::Io(std::io::Error::other(
            "internal error: scan with ty pending did not retain source",
        ))
    })
}

/// Pipeline phases 1 and 2 (issue #67): stream [`ScanOutcome`]s from parallel
/// Phase 1 workers to the serial Phase 2 coordinator as each file's built-in
/// pass finishes. Files that need ty fallback are queued, then opened and
/// queried in sorted-path order after the scan stream drains. ty computes
/// answers on demand (the client advertises pull diagnostics, so didOpen
/// itself triggers no per-file type-check pass) — but its answers for
/// multi-location symbols can depend on the order files were opened, so the
/// nondeterministic scan arrival order must not leak into didOpen order or
/// runs would flicker on those calls.
/// The final sort in [`check_paths_impl`] keeps output deterministic
/// regardless of arrival order; the lazy ty-server start is preserved (only
/// the first file with pending calls triggers it).
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
    files_to_scan: &[PathBuf],
    all_project_files: &[PathBuf],
    explicit_files: &FxHashSet<PathBuf>,
    project_root: &Path,
    source_roots: &SourceRoots,
    config: &Config,
    index: &DefinitionIndex,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    python_env: Option<&Path>,
    diagnostics: &mut Vec<Diagnostic>,
    skip_warnings: &mut Vec<(usize, PathBuf, String)>,
) -> Result<(), CheckError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut consumer_err: Option<CheckError> = None;
    let mut released_pending_files = 0usize;

    let shard_results = std::thread::scope(|scope| -> Result<Vec<TyShardResult>, CheckError> {
        // Phase 1 (parallel, background): the built-in pass over every
        // file. Each file is an independent, pure-CPU unit of work
        // sharing only the `Sync` demand-driven index; results are sent
        // to `rx` as each worker finishes rather than being collected
        // all at once. `tx` is moved in and dropped when all workers
        // finish, closing the channel.
        //
        // The coordinator thread only needs an explicit stack on
        // platforms with small default thread stacks. On glibc Linux
        // this keeps the hot benchmark path on the low-overhead
        // `scope.spawn` implementation.
        #[cfg(any(target_env = "musl", windows))]
        let scan_handle = std::thread::Builder::new()
            .stack_size(crate::limits::STACK_SIZE)
            .spawn_scoped(scope, || {
                stream_scan_files(
                    files_to_scan,
                    explicit_files,
                    source_roots,
                    config,
                    index,
                    indexed_files,
                    tx,
                )
            })
            .map_err(CheckError::Io)?;
        #[cfg(not(any(target_env = "musl", windows)))]
        let scan_handle = scope.spawn(|| {
            stream_scan_files(
                files_to_scan,
                explicit_files,
                source_roots,
                config,
                index,
                indexed_files,
                tx,
            )
        });

        // Phase 2 (parallel, background): one thread per ty shard. Files
        // with deferred calls stream in as the scan releases them in
        // sorted order; the greedy owner assignment below reproduces the
        // partition a whole-list pass would compute. Each work item is
        // sent only to its owner shard, which both opens and queries it —
        // no shard sees the others' files. This is safe because the only
        // cross-file dependency ty has is goto-definition *location
        // order* (which files were opened earlier reorders a symbol's
        // returned locations), and `resolve_first_def_location` (#233)
        // tries every returned location until one parses, so the order —
        // and therefore the open-set — no longer changes the resolved
        // diagnostic. Dropping the per-shard open-history replay cuts the
        // `didOpen` traffic by `TY_SHARD_COUNT`×: on a venv-backed run
        // where most files defer to ty (e.g. the completeness check),
        // each server now parses only its ~1/N share of the project
        // instead of all of it (issue #240).
        let mut shard_senders = Vec::with_capacity(TY_SHARD_COUNT);
        let mut shard_handles = Vec::with_capacity(TY_SHARD_COUNT);
        for _ in 0..TY_SHARD_COUNT {
            let (shard_tx, shard_rx) = std::sync::mpsc::channel::<(usize, PendingTyWork)>();
            shard_senders.push(shard_tx);
            let handle = std::thread::Builder::new()
                .stack_size(crate::limits::STACK_SIZE)
                .spawn_scoped(scope, move || -> TyShardResult {
                    let mut ty: Option<TyResolver> = None;
                    let mut ty_start_attempted = false;
                    let mut file_cache: FxHashMap<PathBuf, Option<String>> = FxHashMap::default();
                    let mut def_caches = TyDefCaches::default();
                    let mut out: Vec<(usize, Vec<Diagnostic>)> = Vec::new();
                    // A shard that never receives an owned file never
                    // starts its server (`resolve_file_with_ty` starts it
                    // lazily on the first non-empty pending set).
                    for (work_index, work) in shard_rx {
                        let mut file_diagnostics = Vec::new();
                        resolve_file_with_ty(
                            &mut ty,
                            &mut ty_start_attempted,
                            project_root,
                            all_project_files,
                            index,
                            indexed_files,
                            python_env,
                            &work.path,
                            &work.source,
                            &work.pending,
                            &work.pending_groups,
                            config,
                            &mut file_cache,
                            &mut def_caches,
                            &mut file_diagnostics,
                            None,
                        )?;
                        out.push((work_index, file_diagnostics));
                    }
                    Ok(out)
                })
                .map_err(CheckError::Io)?;
            shard_handles.push(handle);
        }
        let mut shard_senders = Some(shard_senders);

        // Coordinator: release scan outcomes in sorted-file order (scan
        // results arrive in nondeterministic worker order, but the order
        // files reach each ty server must be a pure function of the file
        // list for the fallback to be reproducible), assign each pending
        // file to the least-loaded shard, and send it only to that owner.
        let mut releaser = InOrderReleaser::new();
        let mut assigner = TyShardAssigner::new(TY_SHARD_COUNT);
        for (i, path, result) in rx {
            if consumer_err.is_some() {
                // A scan error has already been recorded; drain the
                // remaining items so the background thread can finish.
                continue;
            }
            let outcome = match result {
                Ok(o) => o,
                Err(e) => {
                    consumer_err = Some(e);
                    shard_senders = None;
                    continue;
                }
            };
            for (i, path, outcome) in releaser.push(i, (i, path, outcome)) {
                let mut staged: Vec<PendingTyWork> = Vec::new();
                if let Err(e) = process_scan_outcome_for_ty(
                    i,
                    path,
                    outcome,
                    diagnostics,
                    skip_warnings,
                    &mut staged,
                ) {
                    consumer_err = Some(e);
                    shard_senders = None;
                    break;
                }
                if let (Some(senders), Some(work)) = (&shard_senders, staged.pop()) {
                    let owner = assigner.assign(work.pending.len());
                    let _ = senders[owner].send((released_pending_files, work));
                    released_pending_files += 1;
                }
            }
        }
        // Closing the shard channels lets the shard threads drain and
        // finish; the scoped join below waits for them.
        drop(shard_senders);

        match scan_handle.join() {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }?;
        Ok(shard_handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect())
    })?;
    if let Some(e) = consumer_err {
        return Err(e);
    }
    // Reassemble per-file ty diagnostics in released (sorted-file) order so
    // the output matches what a serial pass over the same shards would emit.
    let mut slots: Vec<Option<Vec<Diagnostic>>> = vec![None; released_pending_files];
    for result in shard_results {
        for (work_index, file_diagnostics) in result? {
            slots[work_index] = Some(file_diagnostics);
        }
    }
    for slot in slots.into_iter().flatten() {
        diagnostics.extend(slot);
    }
    Ok(())
}

/// Fixed shard count for the parallel ty fallback. This must be a constant —
/// never derived from the host's core count — because the shard a file lands
/// in determines which `ty server` answers its queries, and ty's answers for
/// multi-location symbols depend on which files that server saw earlier. A
/// machine-dependent shard count would make diagnostics differ across
/// machines. Four shards: measured sweet spot — each extra server pays a
/// fixed project-indexing cost on start, so a wider fan-out stops paying for
/// itself (issue #46 measurements), while four still hides most of ty's
/// serial per-query inference time on large projects.
const TY_SHARD_COUNT: usize = 4;

/// One shard's outcome: each owned work item's index in the released
/// (sorted-file) pending order paired with the diagnostics its ty queries
/// produced.
type TyShardResult = Result<Vec<(usize, Vec<Diagnostic>)>, CheckError>;

/// Buffers out-of-order `(index, item)` arrivals and yields items in strict
/// index order. The parallel scan finishes files in nondeterministic worker
/// order, but everything ty observes must follow the sorted file list, so
/// the coordinator releases outcomes only once every earlier index has
/// arrived.
struct InOrderReleaser<T> {
    next: usize,
    buffered: FxHashMap<usize, T>,
}

impl<T> InOrderReleaser<T> {
    fn new() -> Self {
        Self {
            next: 0,
            buffered: FxHashMap::default(),
        }
    }

    /// Buffer `item` under `index` and drain the now-contiguous prefix.
    fn push(&mut self, index: usize, item: T) -> Vec<T> {
        self.buffered.insert(index, item);
        let mut released = Vec::new();
        while let Some(item) = self.buffered.remove(&self.next) {
            released.push(item);
            self.next += 1;
        }
        released
    }
}

/// Greedy shard assignment for sorted ty work: each file (in sorted-path
/// order) goes to the shard with the fewest pending calls so far (ties:
/// lowest shard index). Pending-call count is the best static proxy for a
/// file's ty cost, and the greedy rule is a pure function of the sorted work
/// prefix, so the partition — and therefore each ty server's request stream
/// — is reproducible everywhere and can be computed while files stream in.
struct TyShardAssigner {
    loads: Vec<usize>,
}

impl TyShardAssigner {
    fn new(shard_count: usize) -> Self {
        Self {
            loads: vec![0; shard_count],
        }
    }

    /// Assign the next sorted file to a shard, weighting it by its deferred
    /// call count (a file always counts at least 1 so empty-pending files
    /// cannot all pile onto shard 0).
    fn assign(&mut self, pending_calls: usize) -> usize {
        let lightest = self
            .loads
            .iter()
            .enumerate()
            .min_by_key(|(_, load)| **load)
            .map_or(0, |(shard, _)| shard);
        self.loads[lightest] += pending_calls.max(1);
        lightest
    }
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
    let python_files = collect_python_files(project_root, paths, config)?;
    let explicit_files = explicit_python_files(paths);
    let source_roots = SourceRoots::from_config(project_root, config);

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

    let mut diagnostics = Vec::new();

    // Cache hits bypass the pipeline; their diagnostics are added directly.
    for entry in &entries {
        if let Some(cached) = &entry.cache_hit {
            diagnostics.extend_from_slice(cached);
        }
    }

    if files_to_scan.is_empty() {
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
        });
        return Ok(diagnostics);
    }

    let (index, indexed_files) =
        build_index_with_sources(project_root, &python_files, &source_roots);

    // Collect skip warnings with their file index so they can be emitted in
    // the original sorted-file order after both phases finish (issue #53 + #46).
    let mut skip_warnings: Vec<(usize, PathBuf, String)> = Vec::new();

    // Run the pipeline (Phase 1 parallel built-in pass + Phase 2 sharded ty
    // fallback) for cache misses only. Files that need ty fallback stream to
    // the shard servers in sorted-path order as their scan results arrive —
    // the servers work concurrently with the scan; ty computes
    // hover/definition answers on demand (pull diagnostics keep didOpen
    // itself cheap). Files fully handled by the built-in resolver do not
    // force ty work, and the shard servers start lazily — only a shard that
    // actually receives a query pays `ty server`'s project-indexing
    // initialize cost (issue #31), so a run the built-in resolver fully
    // handles (the common editor-on-save / pre-commit case on first-party
    // code) starts no server at all. `python_env` (the `--python` value)
    // only steers ty's third-party discovery; the built-in resolver's env
    // discovery is unchanged.
    pipeline_phases(
        &files_to_scan,
        &python_files,
        &explicit_files,
        project_root,
        &source_roots,
        config,
        &index,
        &indexed_files,
        python_env,
        &mut diagnostics,
        &mut skip_warnings,
    )?;

    // Emit skip warnings in the original sorted-file order (issue #53 + #46).
    skip_warnings.sort_unstable_by_key(|(i, ..)| *i);
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
        let skipped_paths: FxHashSet<PathBuf> = skip_warnings
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        let mut diagnostics_by_path: FxHashMap<PathBuf, Vec<Diagnostic>> = FxHashMap::default();
        for diagnostic in &diagnostics {
            diagnostics_by_path
                .entry(diagnostic.path.clone())
                .or_default()
                .push(diagnostic.clone());
        }
        cache_miss_keys
            .par_iter()
            .filter(|(_, path)| !skipped_paths.contains(path))
            .for_each(|(key, path)| {
                let file_diags = diagnostics_by_path.get(path).map_or(&[][..], Vec::as_slice);
                cache.put(*key, file_diags);
            });
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
#[allow(clippy::too_many_arguments)]
fn scan_file(
    source_roots: &SourceRoots,
    path: &Path,
    config: &Config,
    index: &DefinitionIndex,
    indexed_file: Option<&IndexedFile>,
    fix_opt_ins: FixOptIns,
    plan_fixes: bool,
    skip_parse_errors: bool,
) -> Result<ScanOutcome, CheckError> {
    let source_owned;
    let parsed_owned;
    let (source, parsed) = if let Some(indexed_file) = indexed_file {
        (&indexed_file.source, &indexed_file.parsed)
    } else {
        source_owned = match read_python_source(path)? {
            Source::Decoded(source) => source,
            Source::Undecodable(reason) => return Ok(ScanOutcome::Skipped(reason)),
        };
        parsed_owned = match parse_module_guarded(&source_owned) {
            Ok(parsed) => parsed,
            Err(CheckError::Parse(error)) if skip_parse_errors => {
                return Ok(ScanOutcome::Skipped(format!("could not parse: {error}")));
            }
            Err(error) => return Err(error),
        };
        (&source_owned, &parsed_owned)
    };
    let module_name = module_name_for_path(source_roots, path);
    // Scope the checker so its borrows of `source`/`parsed` end before
    // `source` is moved into the returned `FileScan`.
    let (
        diagnostics,
        pending,
        pending_groups,
        overload_fix_pending,
        fixes,
        fixed_calls,
        declined_fix_reasons,
    ) = {
        let mut checker = CallChecker::new(
            path.to_path_buf(),
            module_name,
            is_package_init(path),
            source,
            parsed.tokens(),
            index,
            config,
            fix_opt_ins,
            plan_fixes,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
        let pending_groups = checker.take_pending_hover_groups();
        (
            std::mem::take(&mut checker.diagnostics),
            std::mem::take(&mut checker.ty_pending),
            pending_groups,
            std::mem::take(&mut checker.ty_overload_fix_pending),
            std::mem::take(&mut checker.fixes),
            checker.fixed_calls,
            std::mem::take(&mut checker.declined_fix_reasons),
        )
    };
    let retain_source = plan_fixes | !pending.is_empty() | !overload_fix_pending.is_empty();
    let retained_source = if retain_source {
        Some(source.to_owned())
    } else {
        None
    };
    Ok(ScanOutcome::Scanned(FileScan {
        source: retained_source,
        diagnostics,
        pending,
        pending_groups,
        overload_fix_pending,
        fixes,
        fixed_calls,
        declined_fix_reasons,
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
    source: Option<String>,
    diagnostics: Vec<Diagnostic>,
    pending: Vec<PendingTy>,
    /// Hover group of each `pending` entry (parallel vector): calls proven
    /// to hover identically share a group so the ty fallback asks once.
    pending_groups: Vec<Option<u32>>,
    overload_fix_pending: Vec<PendingTyOverloadFix>,
    fixes: Vec<Insertion>,
    fixed_calls: usize,
    declined_fix_reasons: Vec<DeclinedFixReason>,
}

struct PendingTyWork {
    path: PathBuf,
    source: String,
    pending: Vec<PendingTy>,
    /// Hover group of each `pending` entry (see [`FileScan::pending_groups`]).
    pending_groups: Vec<Option<u32>>,
}

/// Apply `insertions` to `source` and validate that the result remains valid
/// Python. Shared by the built-in and ty-backed fixer paths.
#[cfg_attr(coverage, coverage(off))]
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
    validate_fixed_python(path, &fixed)?;
    Ok(Some(fixed))
}

#[cfg_attr(coverage, coverage(off))]
fn validate_fixed_python(path: &Path, fixed: &str) -> Result<(), CheckError> {
    if parse_module(fixed).is_err() {
        Err(CheckError::FixProducedInvalidSyntax {
            path: path.to_path_buf(),
        })
    } else {
        Ok(())
    }
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
    explicit_files: &FxHashSet<PathBuf>,
    source_roots: &SourceRoots,
    config: &Config,
    index: &DefinitionIndex,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    tx: std::sync::mpsc::Sender<(usize, PathBuf, Result<ScanOutcome, CheckError>)>,
) -> Result<(), CheckError> {
    with_large_stack_pool(move || {
        python_files
            .par_iter()
            .enumerate()
            .for_each_with(tx, |tx, (i, path)| {
                let result = scan_file(
                    source_roots,
                    path,
                    config,
                    index,
                    indexed_files.get(path),
                    FixOptIns::default(),
                    false,
                    !explicit_files.contains(path),
                );
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
    explicit_files: &FxHashSet<PathBuf>,
    source_roots: &SourceRoots,
    config: &Config,
    index: &DefinitionIndex,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    fix_opt_ins: FixOptIns,
) -> Result<Vec<(PathBuf, ScanOutcome)>, CheckError> {
    with_large_stack_pool(|| {
        python_files
            .par_iter()
            .map(|path| {
                let outcome = scan_file(
                    source_roots,
                    path,
                    config,
                    index,
                    indexed_files.get(path),
                    fix_opt_ins,
                    true,
                    !explicit_files.contains(path),
                )?;
                Ok((path.clone(), outcome))
            })
            .collect()
    })
}

struct CallChecker<'a> {
    path: PathBuf,
    module_name: String,
    /// Whether the file is a package initializer (`__init__.py`), which is
    /// the anchor for its own relative imports.
    is_package: bool,
    source: &'a str,
    /// Lazily-built line-start table for diagnostic positions. Large-repository
    /// runs can emit thousands of diagnostics; rescanning the whole file for
    /// each one made line/column formatting quadratic in file size.
    line_starts: Option<Vec<usize>>,
    /// Lexer tokens for `source`, used to recover the parenthesized span of a
    /// call argument so the `name=` prefix lands *before* any redundant outer
    /// parentheses (issue #41) rather than inside them.
    tokens: &'a Tokens,
    index: &'a DefinitionIndex,
    config: &'a Config,
    fix_opt_ins: FixOptIns,
    /// Violations found in this file. Owned (not a shared `&mut`) so each
    /// file's built-in pass is an independent, `Send` unit of work the
    /// whole-project run executes in parallel (issue #46); the single-threaded
    /// `ty` fallback then merges them.
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<Scope>,
    class_stack: Vec<String>,
    function_stack: Vec<String>,
    local_function_scope_count: usize,
    class_body_depth: usize,
    /// Calls the built-in resolver couldn't resolve, deferred for a single
    /// pipelined batch of ty queries per file.
    ty_pending: Vec<PendingTy>,
    /// Set mirror of `ty_pending` so large files can suppress duplicate ty
    /// requests without a linear scan for every unresolved call.
    ty_pending_seen: FxHashSet<PendingTy>,
    /// Built-in-resolved overload violations that are diagnostics already,
    /// but may be safe to rewrite if ty's hover selects one concrete arm.
    ty_overload_fix_pending: Vec<PendingTyOverloadFix>,
    /// Source insertions for the auto-fixer (`check_paths` ignores these).
    fixes: Vec<Insertion>,
    /// Whether this scan should plan rewrite insertions. Plain checks only
    /// need diagnostics and ty fallback offsets, so they skip fixer-only
    /// safety gates on the hot path.
    plan_fixes: bool,
    /// Number of call sites the fixer rewrote in this file.
    fixed_calls: usize,
    /// Reasons for diagnostics emitted by the built-in pass but not rewritten.
    declined_fix_reasons: Vec<DeclinedFixReason>,
    /// Line-level `# noqa` directives parsed from this file's comments. A
    /// `# noqa`/`# noqa: KW001` on a violating call's line suppresses both the
    /// diagnostic and any auto-fix for that call (issue #185).
    noqa: NoqaDirectives,
    /// Stack of name bindings currently in scope (parameters, imports,
    /// `def`/`class` statements, single assignments), used to group deferred
    /// `recv.m(...)` and bare `f(...)` calls that must hover identically
    /// (same binding, same attribute, same call shape) so the ty fallback
    /// asks once per group instead of once per call site.
    hover_group_frames: Vec<HoverGroupFrame>,
    /// Per-name stacks of indices into `hover_group_frames`, so the innermost
    /// frame for a name is found without scanning the whole frame stack
    /// (module scopes of large files hold one frame per import/def/class/
    /// assignment, and every bare-name poison probe does a lookup).
    hover_frame_index: FxHashMap<String, Vec<usize>>,
    /// Stack of lexical scopes for hover-binding frames (module, function/
    /// lambda, class). Seeded with the module scope; never empty.
    hover_scope_stack: Vec<HoverScope>,
    /// Next fresh hover-scope id for this file.
    next_hover_scope_id: u32,
    /// Next fresh hover-binding context id for this file.
    next_hover_ctx: u32,
    /// (binding context, attribute, call shape) -> hover group id.
    hover_groups: FxHashMap<HoverGroupKey, u32>,
    /// Hover group of each entry in `ty_pending` (parallel vector).
    hover_group_of_pending: Vec<Option<u32>>,
    /// Binding contexts whose receiver may have been rebound or narrowed
    /// (assignment to the name, the bare name escaping into a call, a
    /// `match` statement, ...). Groups in these contexts are dropped.
    poisoned_hover_ctxs: FxHashSet<u32>,
    /// (binding context, attribute) keys whose attribute may have been
    /// rebound or narrowed (any non-callee mention of `recv.attr`).
    poisoned_hover_keys: FxHashSet<(u32, String)>,
    /// Deferred-call index ranges whose entries must be stripped of a given
    /// binding context: when a scope binds a name *after* call sites inside
    /// it were attributed to an enclosing binding of that name, those
    /// entries referred to the (whole-scope-local) shadowing binding all
    /// along, so their group attribution is unsafe.
    hover_retro_poisons: Vec<(u32, usize, usize)>,
    /// Addresses of expressions that are the callee of some visited call,
    /// so the `Attribute` visit can tell `self.m(...)` (groupable) apart
    /// from any other mention of `self.m` (narrowing/rebinding hazard).
    callee_exprs: FxHashSet<usize>,
}

/// One name binding tracked for hover grouping: a parameter, an import, a
/// `def`/`class` statement, or a plain single assignment. Calls on the bare
/// name resolve to the innermost *visible* frame for that name; a call
/// inside a nested function without its own binding legitimately closes
/// over the outer binding and inherits the outer frame.
///
/// `in_class_scope` frames are visible only while the class body itself is
/// the current scope: Python name lookup inside methods skips class scopes,
/// so a class-level binding must not shadow a module binding for code in
/// the class's methods.
///
/// `binding_offset` is the byte offset where the binding is introduced;
/// call sites before it textually refer to an earlier (or no) binding and
/// never join the frame's groups.
struct HoverGroupFrame {
    ctx: u32,
    name: String,
    scope_id: u32,
    in_class_scope: bool,
    binding_offset: usize,
}

/// Identity of one hover group: a receiver binding, an attribute, and the
/// call shape. The shape is part of the key because ty's hover at a callee
/// is call-site sensitive — it reports the overload arm / generic
/// specialization selected for *that* call — so only calls that present the
/// same argument arity and coarse argument kinds may share an answer.
#[derive(Eq, Hash, PartialEq)]
struct HoverGroupKey {
    ctx: u32,
    attr: String,
    shape: String,
}

/// One lexical scope tracked for hover-binding frames: the module, a
/// function/lambda body, or a class body. `frame_start` is the index of the
/// first frame owned by this scope (frames above it are truncated on exit);
/// `pending_start` is the index of the first deferred call recorded while
/// inside it, used to retro-poison entries that were attributed to an
/// enclosing binding before a late local binding of the same name was seen
/// (Python scoping makes such a name local for the *whole* scope).
struct HoverScope {
    id: u32,
    is_class: bool,
    frame_start: usize,
    pending_start: usize,
}

/// Collects every bare name mentioned inside an expression, used to poison
/// all names a `match` subject can narrow.
#[derive(Default)]
struct HoverNameCollector {
    names: Vec<String>,
}

impl Visitor<'_> for HoverNameCollector {
    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Name(name) = expr {
            self.names.push(name.id.to_string());
        }
        walk_expr(self, expr);
    }
}

/// A stable per-walk identity for an expression node: its address in the
/// parsed module, which outlives the whole file walk.
fn expr_addr(expr: &Expr) -> usize {
    std::ptr::from_ref::<Expr>(expr) as usize
}

/// A coarse, deterministic fingerprint of a call's argument list: one tag
/// per positional argument (its AST kind) plus the sorted keyword names.
/// Calls with different fingerprints can make ty select different overload
/// arms or generic specializations, so they never share a hover group.
fn call_shape_fingerprint(arguments: &ast::Arguments) -> String {
    let mut shape = String::new();
    for arg in &arguments.args {
        shape.push(argument_kind_tag(arg));
    }
    let mut keyword_names: Vec<&str> = arguments
        .keywords
        .iter()
        .map(|keyword| keyword.arg.as_ref().map_or("**", ast::Identifier::as_str))
        .collect();
    keyword_names.sort_unstable();
    for name in keyword_names {
        shape.push(',');
        shape.push_str(name);
    }
    shape
}

/// The coarse AST kind of one call argument, for [`call_shape_fingerprint`].
const fn argument_kind_tag(arg: &Expr) -> char {
    match arg {
        Expr::Name(_) => 'n',
        Expr::Attribute(_) => 'a',
        Expr::StringLiteral(_) | Expr::BytesLiteral(_) | Expr::FString(_) => 's',
        Expr::NumberLiteral(_) => '0',
        Expr::BooleanLiteral(_) => 'b',
        Expr::NoneLiteral(_) | Expr::EllipsisLiteral(_) => 'c',
        Expr::Call(_) => 'C',
        Expr::List(_) | Expr::ListComp(_) => 'l',
        Expr::Tuple(_) => 't',
        Expr::Dict(_) | Expr::DictComp(_) => 'd',
        Expr::Set(_) | Expr::SetComp(_) => 'e',
        Expr::UnaryOp(_) => 'u',
        Expr::BinOp(_) => 'p',
        Expr::Starred(_) => '*',
        Expr::Subscript(_) => 'i',
        Expr::Compare(_) | Expr::BoolOp(_) => '?',
        Expr::Lambda(_) => 'L',
        _ => 'x',
    }
}

/// The local name an `import` alias binds: the `as` name when present,
/// otherwise the first segment of the dotted module path.
fn bound_import_name(alias: &ast::Alias) -> &str {
    match &alias.asname {
        Some(asname) => asname.as_str(),
        None => alias
            .name
            .as_str()
            .split('.')
            .next()
            .unwrap_or(alias.name.as_str()),
    }
}

/// A call awaiting ty resolution: byte offsets into the file's source.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct PendingTy {
    /// Start of the callee identifier (where we hover / goto-definition).
    callee_offset: usize,
    /// Start of the whole call expression (for the diagnostic position).
    call_start: usize,
    positional_count: usize,
    rewrite_args_are_statically_precise: bool,
}

struct PendingTyOverloadFix {
    pending: PendingTy,
    callee_fullname: String,
    candidate_signatures: Vec<Signature>,
    rewrite_args_are_statically_precise: bool,
}

#[derive(Debug, Default, Clone)]
struct Scope {
    /// Local name -> fully-qualified callable/class name.
    names: FxHashMap<String, String>,
    /// Local name -> the currently visible local function signature.
    functions: FxHashMap<String, LocalFunction>,
    /// Whether this scope has ever contained a local function binding.
    had_function_binding: bool,
    /// Local name -> fully-qualified *module* path (from ``import``).
    modules: FxHashMap<String, String>,
    /// Names in `names` that are bound to an *instance* (`x = C()`), as
    /// opposed to the class object itself. Lets `Class.method(recv, …)` be
    /// told apart from a bound `instance.method(…)` call (issue #27).
    instances: rustc_hash::FxHashSet<String>,
    /// Names imported directly into this scope. They are common monkeypatch
    /// boundaries, so diagnostics are allowed but auto-fixes stay positional.
    imported_callables: FxHashSet<String>,
    /// Local names whose runtime binding is not known to this resolver.
    /// Calls through these names cannot be resolved to a concrete indexed
    /// signature, so they are skipped rather than matched against a
    /// homonymous module-level or nested function (issue #71).
    opaque_locals: rustc_hash::FxHashSet<String>,
    /// Simple local/parameter annotations, used only as a conservative
    /// overload-fix precondition. A union/`Any`/`object` annotation is not
    /// precise enough to prove one overload arm was selected.
    annotations: FxHashMap<String, String>,
}

#[derive(Debug, Clone)]
struct LocalFunction {
    fullname: String,
    signature: Signature,
}

#[cfg_attr(coverage, coverage(off))]
fn remove_function_binding(scope: &mut Scope, local_name: &str) {
    if scope.had_function_binding {
        scope.functions.remove(local_name);
    }
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
        fix_opt_ins: FixOptIns,
        plan_fixes: bool,
    ) -> Self {
        Self {
            path,
            module_name,
            is_package,
            source,
            line_starts: None,
            tokens,
            index,
            config,
            fix_opt_ins,
            diagnostics: Vec::new(),
            scopes: vec![Scope::default()],
            class_stack: Vec::new(),
            function_stack: Vec::new(),
            local_function_scope_count: 0,
            class_body_depth: 0,
            ty_pending: Vec::new(),
            ty_pending_seen: FxHashSet::default(),
            ty_overload_fix_pending: Vec::new(),
            fixes: Vec::new(),
            plan_fixes,
            fixed_calls: 0,
            declined_fix_reasons: Vec::new(),
            noqa: NoqaDirectives::from_source(source, tokens),
            hover_group_frames: Vec::new(),
            hover_frame_index: FxHashMap::default(),
            hover_scope_stack: vec![HoverScope {
                id: 0,
                is_class: false,
                frame_start: 0,
                pending_start: 0,
            }],
            next_hover_scope_id: 1,
            next_hover_ctx: 0,
            hover_groups: FxHashMap::default(),
            hover_group_of_pending: Vec::new(),
            poisoned_hover_ctxs: FxHashSet::default(),
            poisoned_hover_keys: FxHashSet::default(),
            hover_retro_poisons: Vec::new(),
            callee_exprs: FxHashSet::default(),
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

    fn diagnostic_position(&mut self, offset: TextSize) -> (usize, usize) {
        let line_starts = self
            .line_starts
            .get_or_insert_with(|| line_starts(self.source));
        line_column_from_starts(self.source, line_starts, offset)
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    #[cfg_attr(coverage, coverage(off))]
    fn pop_scope(&mut self) {
        if self
            .scopes
            .last()
            .is_some_and(|scope| scope.had_function_binding)
        {
            self.local_function_scope_count = self.local_function_scope_count.saturating_sub(1);
        }
        self.scopes.pop();
    }

    fn define(&mut self, local_name: &str, fullname: String) {
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.insert(local_name.to_string(), fullname);
        remove_function_binding(scope, local_name);
        scope.modules.remove(local_name);
        scope.instances.remove(local_name);
        if plan_fixes {
            scope.imported_callables.remove(local_name);
        }
        scope.opaque_locals.remove(local_name);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn define_function(&mut self, local_name: &str, fullname: String, signature: Signature) {
        let newly_active_scope = {
            let scope = self.current_scope();
            let newly_active_scope = !scope.had_function_binding;
            scope.had_function_binding = true;
            scope.names.insert(local_name.to_string(), fullname.clone());
            scope.functions.insert(
                local_name.to_string(),
                LocalFunction {
                    fullname,
                    signature,
                },
            );
            scope.modules.remove(local_name);
            scope.instances.remove(local_name);
            scope.opaque_locals.remove(local_name);
            newly_active_scope
        };
        if newly_active_scope {
            self.local_function_scope_count += 1;
        }
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
        self.mark_opaque_local(name);
    }

    fn mark_opaque_local(&mut self, name: &str) {
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.remove(name);
        remove_function_binding(scope, name);
        scope.modules.remove(name);
        scope.instances.remove(name);
        if plan_fixes {
            scope.imported_callables.remove(name);
        }
        scope.opaque_locals.insert(name.to_string());
    }

    fn clear_instance_binding(&mut self, name: &str) {
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.remove(name);
        remove_function_binding(scope, name);
        scope.instances.remove(name);
        if plan_fixes {
            scope.imported_callables.remove(name);
        }
    }

    fn bind_function_parameters(&mut self, parameters: &ast::Parameters) {
        for param in parameters
            .posonlyargs
            .iter()
            .chain(parameters.args.iter())
            .chain(parameters.kwonlyargs.iter())
        {
            self.mark_param_opaque_and_annotation(
                param.parameter.name.as_str(),
                param.parameter.annotation.as_deref(),
            );
        }
        if let Some(vararg) = &parameters.vararg {
            self.mark_param_opaque_and_annotation(
                vararg.name.as_str(),
                vararg.annotation.as_deref(),
            );
        }
        if let Some(kwarg) = &parameters.kwarg {
            self.mark_param_opaque_and_annotation(kwarg.name.as_str(), kwarg.annotation.as_deref());
        }
    }

    fn leading_self_parameter(parameters: &ast::Parameters) -> Option<&str> {
        parameters
            .posonlyargs
            .first()
            .or_else(|| parameters.args.first())
            .map(|param| param.parameter.name.as_str())
            .filter(|name| *name == "self")
    }

    fn bind_method_parameters(
        &mut self,
        parameters: &ast::Parameters,
        class_fullname: &str,
        bind_self: bool,
    ) {
        let self_parameter = if bind_self {
            Self::leading_self_parameter(parameters)
        } else {
            None
        };
        for param in parameters
            .posonlyargs
            .iter()
            .chain(parameters.args.iter())
            .chain(parameters.kwonlyargs.iter())
        {
            let name = param.parameter.name.as_str();
            if Some(name) == self_parameter {
                self.record_instance_and_annotation(
                    name,
                    class_fullname,
                    param.parameter.annotation.as_deref(),
                );
            } else {
                self.mark_param_opaque_and_annotation(name, param.parameter.annotation.as_deref());
            }
        }
        if let Some(vararg) = &parameters.vararg {
            self.mark_param_opaque_and_annotation(
                vararg.name.as_str(),
                vararg.annotation.as_deref(),
            );
        }
        if let Some(kwarg) = &parameters.kwarg {
            self.mark_param_opaque_and_annotation(kwarg.name.as_str(), kwarg.annotation.as_deref());
        }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn record_instance_and_annotation(
        &mut self,
        name: &str,
        class_fullname: &str,
        annotation: Option<&Expr>,
    ) {
        self.record_instance(name, class_fullname.to_string());
        if let Some(annotation) = annotation {
            self.define_annotation(name, annotation);
        }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn define_annotation(&mut self, name: &str, annotation: &Expr) {
        let text = self.source[annotation.range()].to_string();
        self.current_scope()
            .annotations
            .insert(name.to_string(), text);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn mark_param_opaque_and_annotation(&mut self, name: &str, annotation: Option<&Expr>) {
        self.mark_param_opaque(name);
        if let Some(annotation) = annotation {
            self.define_annotation(name, annotation);
        }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn resolve_annotation(&self, name: &str) -> Option<&str> {
        for scope in self.scopes.iter().rev() {
            if let Some(annotation) = scope.annotations.get(name) {
                return Some(annotation);
            }
        }
        None
    }

    fn class_from_annotation(&self, annotation: &str) -> Option<String> {
        const DYNAMIC_ANNOTATIONS: &[&str] =
            &["Any", "typing.Any", "object", "builtins.object", "Unknown"];
        let annotation = annotation.trim().trim_matches(['"', '\'']);
        if annotation.is_empty()
            || annotation.contains('|')
            || DYNAMIC_ANNOTATIONS.contains(&annotation)
        {
            return None;
        }
        let class_name = annotation
            .split_once('[')
            .map_or(annotation, |(head, _)| head);
        let class_name = class_name.strip_prefix("builtins.").unwrap_or(class_name);
        if annotation_is_builtin_receiver_type(class_name) {
            return Some(format!("builtins.{class_name}"));
        }

        Some(if let Some((head, rest)) = class_name.split_once('.') {
            if let Some(local) = self.resolve_local(head) {
                format!("{local}.{rest}")
            } else if let Some(module_path) = self.resolve_module(head) {
                format!("{module_path}.{rest}")
            } else {
                class_name.to_string()
            }
        } else {
            self.resolve_local(class_name)
                .unwrap_or_else(|| format!("{}.{}", self.module_name, class_name))
        })
    }

    fn class_from_name_annotation(&self, name: &str) -> Option<String> {
        self.resolve_annotation(name)
            .and_then(|annotation| self.class_from_annotation(annotation))
    }

    /// Whether `name` is a function parameter in the innermost scope that
    /// sees it.  A real `names` binding in the same or an inner scope shadows
    /// any outer opaque entry (the parameter was re-assigned to a known def).
    fn is_opaque_local(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.names.contains_key(name) {
                return false;
            }
            if scope.opaque_locals.contains(name) {
                return true;
            }
        }
        false
    }

    fn define_module(&mut self, local_name: &str, module_path: String) {
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.remove(local_name);
        remove_function_binding(scope, local_name);
        scope.instances.remove(local_name);
        if plan_fixes {
            scope.imported_callables.remove(local_name);
        }
        scope.opaque_locals.remove(local_name);
        scope.modules.insert(local_name.to_string(), module_path);
    }

    fn define_imported_name_and_module(&mut self, local_name: &str, fullname: String) {
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.insert(local_name.to_string(), fullname.clone());
        remove_function_binding(scope, local_name);
        scope.modules.insert(local_name.to_string(), fullname);
        scope.instances.remove(local_name);
        if plan_fixes {
            scope.imported_callables.insert(local_name.to_string());
        }
        scope.opaque_locals.remove(local_name);
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
            self.define_imported_name_and_module(local, fullname);
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
        let plan_fixes = self.plan_fixes;
        let scope = self.current_scope();
        scope.names.insert(local_name.to_string(), class_fullname);
        remove_function_binding(scope, local_name);
        scope.instances.insert(local_name.to_string());
        scope.modules.remove(local_name);
        if plan_fixes {
            scope.imported_callables.remove(local_name);
        }
        scope.opaque_locals.remove(local_name);
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

    fn binding_is_imported_callable(&self, name: &str) -> bool {
        if !self.plan_fixes {
            return false;
        }
        for scope in self.scopes.iter().rev() {
            if scope.names.contains_key(name) {
                return scope.imported_callables.contains(name);
            }
        }
        false
    }

    #[cfg_attr(coverage, coverage(off))]
    fn opaque_attribute_receiver_is_safe_for_fix(&self, name: &str) -> bool {
        !self.is_opaque_local(name)
            || self
                .resolve_annotation(name)
                .is_some_and(annotation_is_builtin_receiver_type)
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
    // class-method call. The public rewrite behavior is covered by integration
    // tests; the individual guard arms are pinned by direct unit tests.
    #[cfg_attr(coverage, coverage(off))]
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
            self.callee_matches_resolved_attr_or_inherited_owner(
                callee_fullname,
                &resolved,
                attr.as_str(),
            ) && !self.binding_is_instance(base)
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

    fn callee_matches_resolved_attr_or_inherited_owner(
        &self,
        callee_fullname: &str,
        resolved_class: &str,
        attr: &str,
    ) -> bool {
        let Some(owner) = callee_fullname.strip_suffix(&format!(".{attr}")) else {
            return false;
        };
        owner == resolved_class || self.index.class_inherits_from(resolved_class, owner)
    }

    #[cfg_attr(coverage, coverage(off))]
    fn is_explicit_dunder_receiver_call(
        &self,
        func: &Expr,
        callee_fullname: &str,
        first_param: Option<&str>,
    ) -> bool {
        if first_param != Some("self") {
            return false;
        }
        if !callee_fullname.ends_with(".__init__")
            && !callee_fullname.ends_with(".__new__")
            && !callee_fullname.ends_with(".__call__")
        {
            return false;
        }
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = func else {
            return false;
        };
        if let Expr::Name(base) = &**value {
            let base = base.id.as_str();
            let Some(resolved) = self.resolve_local(base) else {
                return false;
            };
            self.callee_matches_resolved_attr_or_inherited_owner(
                callee_fullname,
                &resolved,
                attr.as_str(),
            ) && !self.binding_is_instance(base)
        } else {
            let Some(chain) = Self::dotted_path(value) else {
                return false;
            };
            self.resolve_module(chain.split('.').next().unwrap_or(""))
                .is_some()
        }
    }

    fn is_bound_instance_method_call(&self, func: &Expr, first_param: Option<&str>) -> bool {
        if first_param != Some("self") {
            return false;
        }
        let Expr::Attribute(ast::ExprAttribute { value, .. }) = func else {
            return false;
        };
        if Self::class_from_literal_expr(value).is_some() {
            return true;
        }
        if self
            .class_from_constructor(value)
            .is_some_and(|class_fullname| class_fullname != "builtins.super")
        {
            return true;
        }
        if let Expr::Name(base) = &**value {
            let base_name = base.id.as_str();
            return self.binding_is_instance(base_name)
                || self.class_from_name_annotation(base_name).is_some();
        }
        false
    }

    // Covered by integration tests that exercise constructor receivers through
    // real calls. Excluded from the coverage gate because llvm-cov reports an
    // unexecuted per-test-binary instantiation even when those paths are hit.
    #[cfg_attr(coverage, coverage(off))]
    fn class_from_constructor_func(&self, func: &Expr) -> Option<String> {
        match func {
            Expr::Name(name) => {
                let local = name.id.as_str();
                if self.is_opaque_local(local) {
                    return None;
                }
                self.resolve_local(local)
                    .or_else(|| {
                        let candidate = format!("{}.{}", self.module_name, local);
                        self.index.is_class(&candidate).then_some(candidate)
                    })
                    .or_else(|| {
                        let candidate = format!("builtins.{local}");
                        self.index.is_class(&candidate).then_some(candidate)
                    })
            }
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let attr_name = attr.id.as_str();
                let candidate = if let Expr::Name(base) = &**value {
                    let base_name = base.id.as_str();
                    if self.is_opaque_local(base_name) {
                        return None;
                    }
                    if let Some(local) = self.resolve_local(base_name) {
                        format!("{local}.{attr_name}")
                    } else if let Some(module_path) = self.resolve_module(base_name) {
                        format!("{module_path}.{attr_name}")
                    } else {
                        format!("{}.{}.{}", self.module_name, base_name, attr_name)
                    }
                } else {
                    let chain = Self::dotted_path(value)?;
                    let (head, rest) = chain.split_once('.').unwrap_or(("", chain.as_str()));
                    let module_path = self.resolve_module(head)?;
                    format!("{module_path}.{rest}.{attr_name}")
                };
                self.index.is_class(&candidate).then_some(candidate)
            }
            _ => None,
        }
    }

    fn class_from_constructor(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Call(ast::ExprCall { func, .. }) => self.class_from_constructor_func(func),
            _ => None,
        }
    }

    const fn class_from_literal_expr(expr: &Expr) -> Option<&'static str> {
        match expr {
            Expr::StringLiteral(_) => Some("builtins.str"),
            Expr::BytesLiteral(_) => Some("builtins.bytes"),
            Expr::NumberLiteral(ast::ExprNumberLiteral { value, .. }) => match value {
                Number::Int(_) => Some("builtins.int"),
                Number::Float(_) => Some("builtins.float"),
                Number::Complex { .. } => Some("builtins.complex"),
            },
            Expr::BooleanLiteral(_) => Some("builtins.bool"),
            Expr::List(_) => Some("builtins.list"),
            Expr::Tuple(_) => Some("builtins.tuple"),
            Expr::Dict(_) => Some("builtins.dict"),
            Expr::Set(_) => Some("builtins.set"),
            _ => None,
        }
    }

    fn class_from_obvious_instance(&self, expr: &Expr) -> Option<String> {
        self.class_from_constructor(expr)
            .or_else(|| Self::class_from_literal_expr(expr).map(str::to_string))
    }

    fn value_is_bound_callable_attribute_alias(&self, expr: &Expr) -> bool {
        let Expr::Attribute(_) = expr else {
            return false;
        };
        let Some(chain) = Self::dotted_path(expr) else {
            return true;
        };
        let head = chain.split('.').next().unwrap_or("");
        self.resolve_module(head).is_none()
    }

    fn resolve_instance_method(&self, class_fullname: &str, attr_name: &str) -> String {
        let candidate = format!("{class_fullname}.{attr_name}");
        self.index
            .resolve_method(class_fullname, attr_name)
            .unwrap_or(candidate)
    }

    // Covered end-to-end by resolver/fix integration tests. Excluded from the
    // coverage gate because llvm-cov reports duplicate branch holes for this
    // dispatcher across the unit, integration, and CLI test binaries.
    #[cfg_attr(coverage, coverage(off))]
    fn check_call(&mut self, call: &ast::ExprCall) {
        // A `# noqa` on the call's line suppresses the diagnostic and any
        // auto-fix — and lets us skip the ty fallback for that call entirely
        // (issue #185). The diagnostic position uses the same `call.start()`
        // line, so the comment lands where the `path:line:col` output points.
        if self
            .noqa
            .suppresses(call.start().to_usize(), Diagnostic::CODE)
        {
            return;
        }
        let local_function = if self.local_function_scope_count == 0 {
            None
        } else {
            self.resolve_local_function_call(&call.func)
        };
        let callee_fullname = if let Some(local_function) = &local_function {
            local_function.fullname.clone()
        } else {
            let Some(callee_fullname) = self.resolve_callee(&call.func) else {
                // Built-in resolver couldn't resolve: defer to a pipelined ty
                // query (handled once per file after the walk).
                self.record_ty_pending(call);
                return;
            };
            callee_fullname
        };
        // Functions whose first argument must stay positional at runtime
        // (e.g. @singledispatch dispatches on args[0].__class__): skip
        // without deferring to ty.
        if self.index.is_excluded(&callee_fullname) {
            return;
        }
        let indexed_signatures;
        let local_signatures;
        let signatures: &[Signature] = if let Some(local_function) = &local_function {
            local_signatures = [local_function.signature.clone()];
            &local_signatures
        } else if let Some(signatures) = self.index.get(&callee_fullname) {
            indexed_signatures = signatures;
            indexed_signatures.as_ref()
        } else {
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
        let is_constructor =
            callee_fullname.ends_with(".__init__") || callee_fullname.ends_with(".__new__");
        let constructor_positional_requirement =
            if !is_constructor || self.index.is_synthesized(&callee_fullname) {
                0
            } else {
                self.class_from_constructor_func(&call.func)
                    .map_or(0, |class| {
                        self.index.constructor_positional_allowance(&class)
                    })
            };
        if self.config.debug {
            eprintln!("DEBUG: strict_kwargs: {callee_fullname}");
        }
        let ignored = callee_is_ignored(self.config, &callee_fullname);
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
        let receiver_is_implicit = self.is_bound_instance_method_call(&call.func, first_param_name);
        let receiver_is_explicit_for_fix = receiver_is_explicit
            || self.is_explicit_dunder_receiver_call(
                &call.func,
                &callee_fullname,
                first_param_name,
            );
        let effective_storage;
        let effective: &[Signature] = if receiver_is_explicit || receiver_is_implicit {
            effective_storage = signatures
                .iter()
                .map(without_leading_self)
                .collect::<Vec<_>>();
            &effective_storage
        } else {
            signatures
        };
        // A competing constructor boundary may require more leading
        // positional arguments than the selected constructor can accept.
        // Preserve only the required positions that exist in at least one
        // selected signature; the requirement is not an arity exemption.
        let constructor_positional_allowance = constructor_positional_requirement.min(
            effective
                .iter()
                .map(|signature| {
                    signature
                        .parameters
                        .iter()
                        .skip(1)
                        .filter(|parameter| {
                            matches!(
                                parameter.kind,
                                ParameterKind::PositionalOnly | ParameterKind::PositionalOrKeyword
                            )
                        })
                        .count()
                })
                .max()
                .unwrap_or(0),
        );
        let effective_count = if receiver_is_explicit {
            positional_count.saturating_sub(1)
        } else {
            positional_count
        };
        // Overload-safe: only flag when the call exceeds the positional limit
        // of *every* candidate signature (the most permissive overload wins),
        // so ``.pyi`` stub overloads never produce false positives.
        if effective_count <= constructor_positional_allowance
            || effective.iter().any(|signature| {
                !call_exceeds_positional_limit(
                    signature,
                    &callee_fullname,
                    ignored,
                    effective_count,
                )
            })
        {
            return;
        }
        let max_positional = effective
            .iter()
            .filter_map(|signature| {
                signature.max_positional_at_call_site(&callee_fullname, ignored)
            })
            .max()
            .unwrap_or(0)
            .max(constructor_positional_allowance);
        let (line, column) = self.diagnostic_position(call.start());
        self.diagnostics.push(Diagnostic {
            path: self.path.clone(),
            line,
            column,
            callee: format_callee_display(&callee_fullname),
            positional_count: effective_count,
            max_positional,
        });
        self.plan_builtin_fix_for_violation(
            call,
            &callee_fullname,
            signatures,
            max_positional,
            positional_count,
            receiver_is_explicit,
            receiver_is_explicit_for_fix,
        );
    }

    // Fix planning is covered by the `fix` integration suite. Keep this small
    // dispatch gate out of the coverage check because branch coverage reports
    // duplicate holes for the check-only/fix-mode split across test binaries.
    #[cfg_attr(coverage, coverage(off))]
    #[allow(
        clippy::too_many_arguments,
        reason = "threads resolved call state from the diagnostic path into fixer planning"
    )]
    fn plan_builtin_fix_for_violation(
        &mut self,
        call: &ast::ExprCall,
        callee_fullname: &str,
        signatures: &[Signature],
        max_positional: usize,
        positional_count: usize,
        receiver_is_explicit: bool,
        receiver_is_explicit_for_fix: bool,
    ) {
        if !self.plan_fixes {
            return;
        }
        // Auto-fix is applied by default when the parameter-name mapping is
        // proven unambiguous and it is not synthesized from class fields.
        // Synthesized constructors remain an explicit opt-in.
        if self.index.is_synthesized(callee_fullname) && !self.fix_opt_ins.synthesized_constructors
        {
            self.declined_fix_reasons
                .push(DeclinedFixReason::SynthesizedConstructor);
            return;
        }
        if self.call_uses_opaque_receiver_boundary(&call.func) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if self.call_may_dispatch_to_override_with_different_parameter_names(
            &call.func,
            callee_fullname,
        ) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if self.self_call_uses_inherited_method_boundary(&call.func, callee_fullname) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if self.constructor_call_uses_inherited_boundary(&call.func, callee_fullname) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if callable_name_is_private_keyword_boundary(callee_fullname) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if self.call_uses_imported_callable_boundary(&call.func) {
            self.declined_fix_reasons
                .push(DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        if let [signature] = signatures {
            // `receiver.method(...)` omits the bound receiver at the call
            // site; a plain `name(...)` call passes every parameter explicitly.
            let is_attribute_call = matches!(&*call.func, Expr::Attribute(_));
            match call_fix_insertions(
                call,
                self.tokens,
                callee_fullname,
                signature,
                max_positional,
                positional_count,
                is_attribute_call,
                receiver_is_explicit_for_fix,
            ) {
                Ok(insertions) => {
                    self.fixes.extend(insertions);
                    self.fixed_calls += 1;
                }
                Err(reason) => self.declined_fix_reasons.push(reason),
            }
        } else {
            // Multi-arm overloads need extra proof: different arms can bind
            // the same positional slot to different parameter names.
            // During `fix` only, ask ty for the hover at this exact call site;
            // if ty has selected one concrete arm, that selected arm provides
            // the only parameter-name mapping we may rewrite with. A hover
            // that still shows multiple arms, or no callable arm, is declined.
            let rewrite_start = max_positional + usize::from(receiver_is_explicit);
            if !self.record_ty_overload_fix_pending(
                call,
                callee_fullname,
                signatures,
                rewrite_start,
                positional_count,
            ) {
                self.declined_fix_reasons
                    .push(DeclinedFixReason::UnresolvedOverload);
            }
        }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn call_may_dispatch_to_override_with_different_parameter_names(
        &self,
        func: &Expr,
        callee_fullname: &str,
    ) -> bool {
        let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = func else {
            return false;
        };
        let Some((class_fullname, method)) = callee_fullname.rsplit_once('.') else {
            return false;
        };
        if receiver_is_class_object(value, class_fullname) {
            return false;
        }
        method == attr.as_str() && self.index.has_overriding_method(class_fullname, method)
    }

    fn call_uses_imported_callable_boundary(&self, func: &Expr) -> bool {
        matches!(func, Expr::Name(name) if self.binding_is_imported_callable(name.id.as_str()))
    }

    fn call_uses_opaque_receiver_boundary(&self, func: &Expr) -> bool {
        let Expr::Attribute(ast::ExprAttribute { value, .. }) = func else {
            return false;
        };
        let Expr::Name(name) = &**value else {
            return false;
        };
        !self.opaque_attribute_receiver_is_safe_for_fix(name.id.as_str())
    }

    #[cfg_attr(coverage, coverage(off))]
    fn self_call_uses_inherited_method_boundary(&self, func: &Expr, callee_fullname: &str) -> bool {
        let Expr::Attribute(ast::ExprAttribute { value, .. }) = func else {
            return false;
        };
        if !matches!(&**value, Expr::Name(name) if name.id.as_str() == "self") {
            return false;
        }
        let Some((owner, _)) = callee_fullname.rsplit_once('.') else {
            return false;
        };
        self.class_stack
            .last()
            .is_some_and(|current| owner != current)
    }

    fn constructor_call_uses_inherited_boundary(&self, func: &Expr, callee_fullname: &str) -> bool {
        let Some(owner) = callee_fullname
            .strip_suffix(".__init__")
            .or_else(|| callee_fullname.strip_suffix(".__new__"))
        else {
            return false;
        };
        let Some(constructed_class) = self.class_from_constructor_func(func) else {
            return false;
        };
        owner != constructed_class && self.index.class_inherits_from(&constructed_class, owner)
    }

    fn pending_ty_for_call(&self, call: &ast::ExprCall) -> Option<PendingTy> {
        // Position at the callee identifier: the attribute for ``x.m()``,
        // otherwise the name itself.
        let mut rewrite_args_are_statically_precise = true;
        let callee_offset = match &*call.func {
            Expr::Attribute(attr) => {
                if let Some(chain) = Self::dotted_path(&attr.value) {
                    let head = chain.split('.').next().unwrap_or(chain.as_str());
                    if !self.opaque_attribute_receiver_is_safe_for_fix(head)
                        || self.binding_is_imported_callable(head)
                    {
                        rewrite_args_are_statically_precise = false;
                    }
                }
                attr.attr.range().start()
            }
            Expr::Name(name) => {
                if self.is_opaque_local(name.id.as_str())
                    || self.binding_is_imported_callable(name.id.as_str())
                {
                    rewrite_args_are_statically_precise = false;
                }
                name.range().start()
            }
            _ => return None,
        };
        Some(PendingTy {
            callee_offset: callee_offset.to_usize(),
            call_start: call.start().to_usize(),
            positional_count: positional_argument_count(&call.arguments),
            rewrite_args_are_statically_precise,
        })
    }

    /// Defer a call the built-in resolver missed to a pipelined ty query.
    #[cfg_attr(coverage, coverage(off))]
    fn record_ty_pending(&mut self, call: &ast::ExprCall) {
        let Some(pending) = self.pending_ty_for_call(call) else {
            return;
        };
        if self.ty_pending_seen.insert(pending) {
            let group = self.hover_group_for_call(call);
            self.ty_pending.push(pending);
            self.hover_group_of_pending.push(group);
        }
    }

    /// Enter a hover-binding scope (a function, lambda, or class body being
    /// walked). Paired with [`Self::exit_hover_scope`].
    fn enter_hover_scope(&mut self, is_class: bool) {
        let id = self.next_hover_scope_id;
        self.next_hover_scope_id += 1;
        self.hover_scope_stack.push(HoverScope {
            id,
            is_class,
            frame_start: self.hover_group_frames.len(),
            pending_start: self.hover_group_of_pending.len(),
        });
    }

    /// Leave the innermost hover-binding scope, discarding its frames.
    fn exit_hover_scope(&mut self) {
        // The module scope seeded in `new` is never exited and every exit
        // pairs with a prior enter, so the stack is non-empty here and the
        // popped scope's `frame_start` is valid.
        #[allow(
            clippy::expect_used,
            reason = "hover scope stack invariant: always non-empty"
        )]
        let scope = self
            .hover_scope_stack
            .pop()
            .expect("hover scope stack non-empty");
        for frame in self.hover_group_frames.drain(scope.frame_start..) {
            // Frames are pushed and popped in stack order, so the dropped
            // frame is always the most recent entry for its name in the
            // per-name index (which therefore exists).
            let _ = self
                .hover_frame_index
                .get_mut(&frame.name)
                .and_then(Vec::pop);
        }
    }

    /// The innermost hover scope. The stack is seeded with the module scope
    /// in `new` and every `exit_hover_scope` is balanced with a prior
    /// `enter_hover_scope`, so it is never empty.
    fn current_hover_scope(&self) -> &HoverScope {
        #[allow(
            clippy::expect_used,
            reason = "hover scope stack invariant: always non-empty"
        )]
        self.hover_scope_stack
            .last()
            .expect("hover scope stack non-empty")
    }

    /// Bind one hover frame per parameter of a function or lambda being
    /// entered. Every parameter introduces a fresh binding, shadowing any
    /// outer binding of the same name.
    fn bind_parameter_hover_frames(&mut self, parameters: &ast::Parameters, offset: usize) {
        let names: Vec<String> = parameters
            .posonlyargs
            .iter()
            .chain(parameters.args.iter())
            .chain(parameters.kwonlyargs.iter())
            .map(|param| param.parameter.name.to_string())
            .chain(parameters.vararg.iter().map(|param| param.name.to_string()))
            .chain(parameters.kwarg.iter().map(|param| param.name.to_string()))
            .collect();
        for name in names {
            self.note_hover_binding(&name, offset);
        }
    }

    /// The innermost *visible* hover frame for a bare name, if any. Frames
    /// bound in a class body are visible only while that class body is the
    /// current scope (Python name lookup inside methods skips class scopes);
    /// an invisible class frame does not hide an outer module/function frame.
    fn visible_hover_frame_index(&self, name: &str) -> Option<usize> {
        let current_scope_id = self.current_hover_scope().id;
        self.hover_frame_index
            .get(name)?
            .iter()
            .rev()
            .copied()
            .find(|&index| {
                let frame = &self.hover_group_frames[index];
                !frame.in_class_scope || frame.scope_id == current_scope_id
            })
    }

    /// The innermost visible hover-binding context for a bare name, if any.
    fn hover_ctx_for(&self, name: &str) -> Option<u32> {
        self.visible_hover_frame_index(name)
            .map(|index| self.hover_group_frames[index].ctx)
    }

    /// Record a binding of `name` in the current scope (an import, a
    /// `def`/`class` statement, a parameter, or a `Store`-context name).
    ///
    /// The first binding in a scope opens a fresh frame; a *second* binding
    /// in the same scope poisons it (the name's type may differ between
    /// call sites on either side of the rebinding). A binding that shadows
    /// an enclosing frame additionally retro-poisons that frame's entries
    /// recorded since the current scope was entered: Python scoping makes
    /// the name local for the whole scope, so those earlier attributions
    /// were wrong.
    fn note_hover_binding(&mut self, name: &str, offset: usize) {
        match self.visible_hover_frame_index(name) {
            Some(index)
                if self.hover_group_frames[index].scope_id == self.current_hover_scope().id =>
            {
                let ctx = self.hover_group_frames[index].ctx;
                self.poisoned_hover_ctxs.insert(ctx);
            }
            shadowed => {
                if let Some(index) = shadowed {
                    let ctx = self.hover_group_frames[index].ctx;
                    let start = self.current_hover_scope().pending_start;
                    let end = self.hover_group_of_pending.len();
                    if end > start {
                        self.hover_retro_poisons.push((ctx, start, end));
                    }
                }
                let ctx = self.next_hover_ctx;
                self.next_hover_ctx += 1;
                let scope = self.current_hover_scope();
                let (scope_id, in_class_scope) = (scope.id, scope.is_class);
                self.hover_frame_index
                    .entry(name.to_owned())
                    .or_default()
                    .push(self.hover_group_frames.len());
                self.hover_group_frames.push(HoverGroupFrame {
                    ctx,
                    name: name.to_owned(),
                    scope_id,
                    in_class_scope,
                    binding_offset: offset,
                });
            }
        }
    }

    /// Mark a receiver binding as unsafe for hover grouping (rebound,
    /// narrowed, or escaped where this checker cannot follow).
    fn poison_hover_ctx_for(&mut self, name: &str) {
        if let Some(ctx) = self.hover_ctx_for(name) {
            self.poisoned_hover_ctxs.insert(ctx);
        }
    }

    /// Poison the binding of any *bare* receiver name appearing in a
    /// narrowing-capable expression position (a comparison operand, a
    /// boolean-operator operand, a `not` operand, a conditional test).
    fn poison_hover_bare_receiver(&mut self, expr: &Expr) {
        if let Expr::Name(name) = expr {
            self.poison_hover_ctx_for(name.id.as_str());
        }
    }

    /// A bare receiver passed as a call argument can be narrowed by the
    /// callee (`isinstance(self, T)`, `TypeGuard` helpers) for the rest of the
    /// enclosing block, so its binding is no longer hover-stable.
    fn poison_hover_call_args(&mut self, arguments: &ast::Arguments) {
        for arg in &arguments.args {
            match arg {
                Expr::Starred(starred) => self.poison_hover_bare_receiver(&starred.value),
                _ => self.poison_hover_bare_receiver(arg),
            }
        }
        for keyword in &arguments.keywords {
            self.poison_hover_bare_receiver(&keyword.value);
        }
    }

    /// Any mention of `self.attr` that is *not* the callee of a call —
    /// an assignment/`del` target, a truthiness test, a value read — can
    /// rebind or narrow the attribute, so its hover group is dropped.
    fn note_hover_attribute(&mut self, attr: &ast::ExprAttribute, addr: usize) {
        let Expr::Name(name) = &*attr.value else {
            return;
        };
        let Some(ctx) = self.hover_ctx_for(name.id.as_str()) else {
            return;
        };
        if !self.callee_exprs.contains(&addr) {
            self.poisoned_hover_keys
                .insert((ctx, attr.attr.to_string()));
        }
    }

    /// Statement-level hover-binding/poison scan, run exactly once when a
    /// statement is dispatched. `import`/`def`/`class` statements *bind*
    /// their name in the current scope (opening a frame, or poisoning a
    /// same-scope rebinding); `except ... as`, `global`/`nonlocal`, and
    /// augmented assignment poison (their binding lifetime or value is not
    /// hover-stable); `if`/`while`/`assert` tests and `match` statements
    /// poison the names they can narrow.
    fn scan_stmt_for_hover_poison(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::If(if_stmt) => self.poison_hover_bare_receiver(&if_stmt.test),
            Stmt::While(while_stmt) => self.poison_hover_bare_receiver(&while_stmt.test),
            Stmt::Assert(assert_stmt) => self.poison_hover_bare_receiver(&assert_stmt.test),
            Stmt::Match(match_stmt) => {
                // The subject (and every name mentioned inside it) is
                // narrowed per arm; capture patterns bind new names.
                self.poison_hover_names_in_expr(&match_stmt.subject);
                for case in &match_stmt.cases {
                    self.poison_hover_pattern_bindings(&case.pattern);
                }
            }
            Stmt::Try(try_stmt) => {
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(name) = &handler.name {
                        self.poison_hover_ctx_for(name.as_str());
                    }
                }
            }
            Stmt::Global(global_stmt) => {
                for name in &global_stmt.names {
                    self.poison_hover_ctx_for(name.as_str());
                }
            }
            Stmt::Nonlocal(nonlocal_stmt) => {
                for name in &nonlocal_stmt.names {
                    self.poison_hover_ctx_for(name.as_str());
                }
            }
            Stmt::Import(import_stmt) => {
                let offset = stmt.range().start().to_usize();
                for alias in &import_stmt.names {
                    let name = bound_import_name(alias).to_owned();
                    self.note_hover_binding(&name, offset);
                }
            }
            Stmt::ImportFrom(import_stmt) => {
                let offset = stmt.range().start().to_usize();
                for alias in &import_stmt.names {
                    let name = bound_import_name(alias).to_owned();
                    self.note_hover_binding(&name, offset);
                }
            }
            Stmt::FunctionDef(function_def) => {
                let name = function_def.name.to_string();
                self.note_hover_binding(&name, stmt.range().start().to_usize());
            }
            Stmt::ClassDef(class_def) => {
                let name = class_def.name.to_string();
                self.note_hover_binding(&name, stmt.range().start().to_usize());
            }
            Stmt::AugAssign(aug_assign) => {
                // `x += ...` keeps the name bound but changes its value (and
                // possibly its inferred type), so the binding is unstable.
                self.poison_hover_bare_receiver(&aug_assign.target);
            }
            _ => {}
        }
    }

    /// Poison every bare name mentioned anywhere inside `expr` (used for
    /// `match` subjects, which ty narrows per arm).
    fn poison_hover_names_in_expr(&mut self, expr: &Expr) {
        let mut collector = HoverNameCollector::default();
        collector.visit_expr(expr);
        for name in collector.names {
            self.poison_hover_ctx_for(&name);
        }
    }

    /// Poison every name a `match` case pattern can bind (capture names,
    /// star/mapping rests), recursively.
    fn poison_hover_pattern_bindings(&mut self, pattern: &ast::Pattern) {
        match pattern {
            ast::Pattern::MatchValue(_) | ast::Pattern::MatchSingleton(_) => {}
            ast::Pattern::MatchSequence(sequence) => {
                for inner in &sequence.patterns {
                    self.poison_hover_pattern_bindings(inner);
                }
            }
            ast::Pattern::MatchMapping(mapping) => {
                for inner in &mapping.patterns {
                    self.poison_hover_pattern_bindings(inner);
                }
                if let Some(rest) = &mapping.rest {
                    self.poison_hover_ctx_for(rest.as_str());
                }
            }
            ast::Pattern::MatchClass(class_pattern) => {
                for inner in &class_pattern.arguments.patterns {
                    self.poison_hover_pattern_bindings(inner);
                }
                for keyword in &class_pattern.arguments.keywords {
                    self.poison_hover_pattern_bindings(&keyword.pattern);
                }
            }
            ast::Pattern::MatchStar(star) => {
                if let Some(name) = &star.name {
                    self.poison_hover_ctx_for(name.as_str());
                }
            }
            ast::Pattern::MatchAs(as_pattern) => {
                if let Some(inner) = &as_pattern.pattern {
                    self.poison_hover_pattern_bindings(inner);
                }
                if let Some(name) = &as_pattern.name {
                    self.poison_hover_ctx_for(name.as_str());
                }
            }
            ast::Pattern::MatchOr(or_pattern) => {
                for inner in &or_pattern.patterns {
                    self.poison_hover_pattern_bindings(inner);
                }
            }
        }
    }

    /// The hover group for a deferred call, if its callee is an attribute on
    /// a bare in-scope binding (`recv.m(...)`) or a bare in-scope name
    /// (`f(...)`).
    ///
    /// Excluded from the coverage gate for the same instantiation-accounting
    /// reason as `visit_stmt`: the `hover_groups_*` unit tests exercise every
    /// arm directly, but `llvm-cov report` counts a phantom missed line for
    /// one of this function's duplicated test-binary instantiations.
    #[cfg_attr(coverage, coverage(off))]
    fn hover_group_for_call(&mut self, call: &ast::ExprCall) -> Option<u32> {
        let (name, attr) = match &*call.func {
            // `recv.m(...)` on a bare in-scope receiver binding.
            Expr::Attribute(attr) => match &*attr.value {
                Expr::Name(name) => (name, attr.attr.to_string()),
                _ => return None,
            },
            // Bare `f(...)` on an in-scope binding. The empty attribute
            // cannot collide with a real one (`x.(...)` does not parse).
            Expr::Name(name) => (name, String::new()),
            _ => return None,
        };
        let frame_index = self.visible_hover_frame_index(name.id.as_str())?;
        let frame = &self.hover_group_frames[frame_index];
        // A call site textually before the binding refers to an earlier (or
        // no) binding, so it never joins this frame's groups.
        if name.range().start().to_usize() < frame.binding_offset {
            return None;
        }
        let ctx = frame.ctx;
        // Group ids are per-file and bounded by the file's call count, so
        // the conversion cannot overflow; saturate rather than branch.
        let next_id = u32::try_from(self.hover_groups.len()).unwrap_or(u32::MAX);
        Some(
            *self
                .hover_groups
                .entry(HoverGroupKey {
                    ctx,
                    attr,
                    shape: call_shape_fingerprint(&call.arguments),
                })
                .or_insert(next_id),
        )
    }

    /// The hover group of each `ty_pending` entry, with groups whose binding
    /// or attribute was poisoned anywhere in the file dropped (poison may be
    /// discovered after a group's earlier call sites were recorded).
    fn take_pending_hover_groups(&mut self) -> Vec<Option<u32>> {
        let dropped: FxHashSet<u32> = self
            .hover_groups
            .iter()
            .filter(|(key, _)| {
                self.poisoned_hover_ctxs.contains(&key.ctx)
                    || self
                        .poisoned_hover_keys
                        .contains(&(key.ctx, key.attr.clone()))
            })
            .map(|(_, &group)| group)
            .collect();
        let mut groups: Vec<Option<u32>> = std::mem::take(&mut self.hover_group_of_pending)
            .into_iter()
            .map(|group| group.filter(|g| !dropped.contains(g)))
            .collect();
        // Strip entries a late shadowing binding retro-poisoned: only the
        // indicated index range, and only entries whose group belongs to the
        // shadowed binding context.
        if !self.hover_retro_poisons.is_empty() {
            let ctx_of_group: FxHashMap<u32, u32> = self
                .hover_groups
                .iter()
                .map(|(key, &group)| (group, key.ctx))
                .collect();
            for &(ctx, start, end) in &self.hover_retro_poisons {
                for slot in &mut groups[start..end] {
                    if slot.is_some_and(|group| ctx_of_group.get(&group) == Some(&ctx)) {
                        *slot = None;
                    }
                }
            }
        }
        groups
    }

    /// Queue an already-diagnosed overload violation for fix-only ty hover
    /// selection. This never affects `check`: no extra diagnostic is emitted.
    #[cfg_attr(coverage, coverage(off))]
    fn record_ty_overload_fix_pending(
        &mut self,
        call: &ast::ExprCall,
        callee_fullname: &str,
        candidate_signatures: &[Signature],
        rewrite_start: usize,
        positional_count: usize,
    ) -> bool {
        if let Some(pending) = self.pending_ty_for_call(call) {
            let rewrite_args_are_statically_precise = (rewrite_start..positional_count).all(|i| {
                call.arguments
                    .args
                    .get(i)
                    .is_some_and(|arg| self.arg_is_precise_for_overload_fix(arg))
            });
            self.ty_overload_fix_pending.push(PendingTyOverloadFix {
                pending,
                callee_fullname: callee_fullname.to_string(),
                candidate_signatures: candidate_signatures.to_vec(),
                rewrite_args_are_statically_precise,
            });
            true
        } else {
            false
        }
    }

    #[cfg_attr(coverage, coverage(off))]
    fn arg_is_precise_for_overload_fix(&self, arg: &Expr) -> bool {
        if is_precise_overload_literal(arg) {
            return true;
        }
        let Expr::Name(name) = arg else {
            return false;
        };
        self.resolve_annotation(name.id.as_str())
            .is_some_and(annotation_is_precise_overload_type)
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
        let (class, method) = base.rsplit_once('.').unwrap_or(("", base));
        if let Some(resolved) = self.index.resolve_method(class, method) {
            return Some(resolved);
        }
        for ctor in ["__init__", "__new__"] {
            let candidate = format!("{base}.{ctor}");
            if self.index.get(&candidate).is_some() {
                return Some(candidate);
            }
        }
        for ctor in ["__init__", "__new__"] {
            if let Some(resolved) = self.index.resolve_method(base, ctor) {
                return Some(resolved);
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
        if self.is_opaque_local(head) {
            return None;
        }
        let module_path = self.resolve_module(head)?;
        let candidate = format!("{module_path}.{rest}.{attr_name}");
        Some(self.callable_fullname(&candidate).unwrap_or(candidate))
    }

    #[cfg_attr(coverage, coverage(off))]
    fn resolve_local_function_call(&self, func: &Expr) -> Option<LocalFunction> {
        let Expr::Name(name) = func else {
            return None;
        };
        let local = name.id.as_str();
        for scope in self.scopes.iter().rev() {
            if let Some(function) = scope.functions.get(local) {
                return Some(function.clone());
            }
            if scope.names.contains_key(local) || scope.opaque_locals.contains(local) {
                return None;
            }
        }
        None
    }

    fn current_lexical_scope(&self) -> &str {
        self.function_stack
            .last()
            .or_else(|| self.class_stack.last())
            .map_or(self.module_name.as_str(), String::as_str)
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
                if let Some(class_fullname) = self.class_from_constructor(value) {
                    if class_fullname == "builtins.super" {
                        return None;
                    }
                    return Some(self.resolve_instance_method(&class_fullname, attr_name));
                }
                if let Some(class_fullname) = Self::class_from_literal_expr(value) {
                    return Some(self.resolve_instance_method(class_fullname, attr_name));
                }
                if let Expr::Name(base) = &**value {
                    let base_name = base.id.as_str();
                    if self.is_opaque_local(base_name) {
                        return self
                            .class_from_name_annotation(base_name)
                            .map(|class_fullname| {
                                self.resolve_instance_method(&class_fullname, attr_name)
                            });
                    }
                    // Local bindings (incl. a locally redefined class) take
                    // precedence over a stale ``import`` module binding.
                    if let Some(class_fullname) = self.class_from_name_annotation(base_name) {
                        return Some(self.resolve_instance_method(&class_fullname, attr_name));
                    }
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
                let class_fullname = self.class_from_constructor_func(&constructor.func)?;
                let dunder_call = format!("{class_fullname}.__call__");
                self.index
                    .resolve_method(&class_fullname, "__call__")
                    .or_else(|| {
                        self.index
                            .get(&dunder_call)
                            .is_some()
                            .then_some(dunder_call)
                    })
            }
            _ => None,
        }
    }

    fn visit_method_def(&mut self, method_def: &'a StmtFunctionDef) {
        let StmtFunctionDef {
            name,
            parameters,
            body,
            decorator_list,
            ..
        } = method_def;
        for decorator in decorator_list {
            self.visit_expr(&decorator.expression);
        }
        let class_fullname = self.class_stack.last().cloned().unwrap_or_default();
        let method_fullname = format!("{class_fullname}.{name}");
        self.push_scope();
        self.function_stack.push(method_fullname);
        let binds_instance_self = !has_staticmethod_or_classmethod_decorator(decorator_list);
        self.bind_method_parameters(parameters, &class_fullname, binds_instance_self);
        self.enter_hover_scope(false);
        self.bind_parameter_hover_frames(parameters, parameters.range().start().to_usize());

        let class_body_depth = self.class_body_depth;
        self.class_body_depth = 0;
        for method_stmt in body {
            self.visit_body_stmt(method_stmt);
        }
        self.class_body_depth = class_body_depth;
        self.exit_hover_scope();
        self.function_stack.pop();
        self.pop_scope();
    }

    fn visit_if_branch_stmt(&mut self, stmt: &'a Stmt, traversal: IfBranchTraversal) {
        match traversal {
            IfBranchTraversal::Module => self.visit_stmt(stmt),
            IfBranchTraversal::LocalBody => self.visit_body_stmt(stmt),
            IfBranchTraversal::ClassBody => self.visit_class_body_stmt(stmt),
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

    /// Walk a statement that appears in a function body or local control-flow
    /// branch. Statements that carry custom `visit_stmt` logic
    /// (`Assign`, `AnnAssign`, `FunctionDef`, `ClassDef`) are dispatched
    /// through `visit_stmt` so instance tracking and definition registration
    /// fire correctly. `If` uses the custom branch traversal so the
    /// double-elif-test fix still fires without registering function-local
    /// imports. Everything else (e.g. `Import` / `ImportFrom`) goes through
    /// `walk_stmt` directly; function-local imports are intentionally not
    /// registered.
    fn visit_body_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            // Delegated statements reach `visit_stmt`, which runs the
            // hover-binding scan itself; scanning here too would record a
            // `def`/`class` binding twice and wrongly poison it as a
            // same-scope rebinding.
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                self.visit_stmt(stmt);
            }
            Stmt::If(if_stmt) => {
                self.scan_stmt_for_hover_poison(stmt);
                self.visit_if_stmt(if_stmt, IfBranchTraversal::LocalBody);
            }
            _ => {
                self.scan_stmt_for_hover_poison(stmt);
                walk_stmt(self, stmt);
            }
        }
    }

    /// Walk a statement that appears in a class body or class-level branch.
    /// Function definitions in this context are methods, including those under
    /// class-level control flow, so their leading `self` parameter can bind to
    /// the containing class.
    fn visit_class_body_stmt(&mut self, stmt: &'a Stmt) {
        // Delegated statements reach `visit_stmt`, which runs the
        // hover-binding scan itself (see `visit_body_stmt`).
        if !matches!(
            stmt,
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::FunctionDef(_) | Stmt::ClassDef(_)
        ) {
            self.scan_stmt_for_hover_poison(stmt);
        }
        match stmt {
            Stmt::If(if_stmt) => self.visit_if_stmt(if_stmt, IfBranchTraversal::ClassBody),
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                for inner in body {
                    self.visit_class_body_stmt(inner);
                }
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(type_) = &handler.type_ {
                        self.visit_expr(type_);
                    }
                    for inner in &handler.body {
                        self.visit_class_body_stmt(inner);
                    }
                }
                for inner in orelse {
                    self.visit_class_body_stmt(inner);
                }
                for inner in finalbody {
                    self.visit_class_body_stmt(inner);
                }
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) | Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                self.visit_stmt(stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

impl<'a> Visitor<'a> for CallChecker<'a> {
    // Trait-walker glue delegates decision logic to covered helpers, but
    // llvm-cov reports branch/line holes for duplicated test-binary
    // instantiations of this large dispatch function.
    #[cfg_attr(coverage, coverage(off))]
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        self.scan_stmt_for_hover_poison(stmt);
        match stmt {
            Stmt::FunctionDef(function_def) => {
                if self.class_body_depth > 0 {
                    self.visit_method_def(function_def);
                    return;
                }
                let StmtFunctionDef {
                    name,
                    parameters,
                    body,
                    decorator_list,
                    ..
                } = function_def;
                // Decorator expressions are evaluated in the enclosing
                // scope, so visit them before defining/scoping the function
                // (issue #51: decorator-factory calls were never checked).
                for decorator in decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                let fullname = format!("{}.{}", self.current_lexical_scope(), name);
                if self.function_stack.is_empty() {
                    self.define(name, fullname.clone());
                } else {
                    self.define_function(
                        name,
                        fullname.clone(),
                        signature_from_parameters(parameters),
                    );
                }
                self.function_stack.push(fullname);
                self.push_scope();
                // Register every parameter as opaque so that calls through
                // a Callable-typed (or otherwise unresolvable) parameter
                // don't fall back to a module-level function with the same
                // name (issue #71).
                self.bind_function_parameters(parameters);
                self.enter_hover_scope(false);
                self.bind_parameter_hover_frames(parameters, parameters.range().start().to_usize());
                for inner in body {
                    self.visit_body_stmt(inner);
                }
                self.exit_hover_scope();
                self.pop_scope();
                self.function_stack.pop();
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
                let class_fullname = format!("{}.{}", self.current_lexical_scope(), name);
                self.define(name, class_fullname.clone());
                self.class_stack.push(class_fullname);
                self.push_scope();
                self.enter_hover_scope(true);
                self.class_body_depth += 1;
                for inner in body {
                    self.visit_class_body_stmt(inner);
                }
                self.class_body_depth -= 1;
                self.exit_hover_scope();
                self.pop_scope();
                self.class_stack.pop();
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                let class_fullname = self.class_from_obvious_instance(value);
                let is_callable_attribute_alias =
                    self.value_is_bound_callable_attribute_alias(value);
                walk_stmt(self, stmt);
                for target in targets {
                    if let Expr::Name(name) = target {
                        if let Some(class_fullname) = &class_fullname {
                            self.record_instance(name.id.as_str(), class_fullname.clone());
                        } else if is_callable_attribute_alias {
                            self.mark_opaque_local(name.id.as_str());
                        } else {
                            self.clear_instance_binding(name.id.as_str());
                        }
                    }
                }
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value: Some(value),
                ..
            }) => {
                let class_fullname = self.class_from_obvious_instance(value);
                let is_callable_attribute_alias =
                    self.value_is_bound_callable_attribute_alias(value);
                walk_stmt(self, stmt);
                if let Expr::Name(name) = &**target {
                    self.define_annotation(name.id.as_str(), annotation);
                    if let Some(class_fullname) = class_fullname {
                        self.record_instance(name.id.as_str(), class_fullname);
                    } else if is_callable_attribute_alias {
                        self.mark_opaque_local(name.id.as_str());
                    } else {
                        self.clear_instance_binding(name.id.as_str());
                    }
                }
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value: None,
                ..
            }) => {
                walk_stmt(self, stmt);
                if let Expr::Name(name) = &**target {
                    self.mark_opaque_local(name.id.as_str());
                    self.define_annotation(name.id.as_str(), annotation);
                }
            }
            Stmt::If(if_stmt) => self.visit_if_stmt(if_stmt, IfBranchTraversal::Module),
            Stmt::Import(import) => self.record_plain_import(import),
            Stmt::ImportFrom(import) => self.record_from_import(import),
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Call(call) => {
                if positional_argument_count(&call.arguments) > 0 {
                    self.check_call(call);
                }
                // Mark the callee before walking into it so the `Attribute`
                // arm can tell `self.m(...)` apart from a bare `self.m`
                // mention, and poison receivers escaping as call arguments.
                self.callee_exprs.insert(expr_addr(&call.func));
                self.poison_hover_call_args(&call.arguments);
            }
            Expr::Lambda(lambda) => {
                self.enter_hover_scope(false);
                if let Some(parameters) = lambda.parameters.as_deref() {
                    self.bind_parameter_hover_frames(parameters, lambda.range().start().to_usize());
                }
                walk_expr(self, expr);
                self.exit_hover_scope();
                return;
            }
            Expr::Attribute(attr) => {
                self.note_hover_attribute(attr, expr_addr(expr));
            }
            Expr::Name(name) if name.ctx.is_store() => {
                // A plain assignment target opens (or, on a same-scope
                // rebinding, poisons) a hover frame; `for`/`with`/walrus
                // targets bind the same way.
                let name_string = name.id.to_string();
                self.note_hover_binding(&name_string, name.range().start().to_usize());
            }
            Expr::Name(name) if !name.ctx.is_load() => {
                // `del x` (and invalid contexts): the binding disappears.
                self.poison_hover_ctx_for(name.id.as_str());
            }
            Expr::Compare(compare) => {
                self.poison_hover_bare_receiver(&compare.left);
                for comparator in &compare.comparators {
                    self.poison_hover_bare_receiver(comparator);
                }
            }
            Expr::BoolOp(bool_op) => {
                for value in &bool_op.values {
                    self.poison_hover_bare_receiver(value);
                }
            }
            Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                self.poison_hover_bare_receiver(&unary.operand);
            }
            Expr::If(ternary) => {
                self.poison_hover_bare_receiver(&ternary.test);
            }
            _ => {}
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
) -> Result<Vec<Insertion>, DeclinedFixReason> {
    // Star-unpacking at the call site (`f(*xs)` / `f(**kw)`): the positional
    // count is unknown, so a positional->keyword mapping is unsound.
    if call.arguments.args.iter().any(Expr::is_starred_expr) {
        return Err(DeclinedFixReason::UnsafeCallSiteUnpacking);
    }
    if call.arguments.keywords.iter().any(|kw| kw.arg.is_none()) {
        return Err(DeclinedFixReason::UnsafeCallSiteUnpacking);
    }
    // Descriptor protocol calls are rare and their receiver/value mapping is
    // subtle; skip rather than risk a wrong rewrite.
    if callee_fullname.ends_with(".__get__") || callee_fullname.ends_with(".__set__") {
        return Err(DeclinedFixReason::UnsupportedSignatureShape);
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
        if is_dunder_receiver && is_attribute_call {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        }
        let receiver_is_implicit =
            is_dunder_receiver || (is_attribute_call && first_param_is_receiver_name);
        (usize::from(receiver_is_implicit), max_positional)
    };

    let mut insertions = Vec::new();
    for arg_index in start..positional_count {
        let Some(arg) = call.arguments.args.get(arg_index) else {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        };
        // A bare generator (`f(x for x in y)`) or walrus (`f(x := 1)`) would
        // need extra parentheses once prefixed; decline rather than wrap.
        if arg.is_generator_expr() || arg.is_named_expr() {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        }
        let Some(param) = signature.parameters.get(arg_index + skip) else {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        };
        let Some(name) = param.name.as_deref() else {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        };
        if call
            .arguments
            .keywords
            .iter()
            .any(|kw| kw.arg.as_ref().is_some_and(|arg| arg.as_str() == name))
        {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        }
        if !parameter_name_is_safe_keyword_target(name) {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        }
        // Only these kinds accept a keyword argument; a positional-only
        // parameter or `*args`/`**kwargs` slot cannot be rewritten.
        if !matches!(
            param.kind,
            ParameterKind::PositionalOrKeyword | ParameterKind::KeywordOnly
        ) {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
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
    if insertions.is_empty() {
        Err(DeclinedFixReason::UnsupportedSignatureShape)
    } else {
        Ok(insertions)
    }
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
#[cfg(test)]
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
#[cfg(test)]
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

// ty-fallback data for `DefFileIndex`; the impl below is coverage-excluded
// with the surrounding ty fallback orchestration, while behaviour is verified
// by deterministic unit tests.
#[derive(Clone)]
struct DefFunction {
    class: Option<String>,
    name: String,
    name_start: usize,
    name_end: usize,
    signature: Signature,
}

#[derive(Clone)]
struct DefClass {
    name: String,
    name_start: usize,
    name_end: usize,
    init_signatures: Vec<Signature>,
    new_signatures: Vec<Signature>,
}

#[derive(Clone)]
struct DefFileIndex {
    funcs: Vec<DefFunction>,
    classes: Vec<DefClass>,
}

#[cfg_attr(coverage, coverage(off))]
impl DefFileIndex {
    fn from_stmts(stmts: &[Stmt]) -> Self {
        let mut funcs: Vec<FnEntry> = Vec::new();
        let mut classes: Vec<&StmtClassDef> = Vec::new();
        collect_defs(stmts, None, &mut funcs, &mut classes);

        let funcs = funcs
            .into_iter()
            .map(|(class, function)| {
                let range = function.name.range();
                DefFunction {
                    class,
                    name: function.name.to_string(),
                    name_start: range.start().to_usize(),
                    name_end: range.end().to_usize(),
                    signature: signature_from_parameters(&function.parameters),
                }
            })
            .collect();
        let classes = classes
            .into_iter()
            .map(|class| {
                let range = class.name.range();
                DefClass {
                    name: class.name.to_string(),
                    name_start: range.start().to_usize(),
                    name_end: range.end().to_usize(),
                    init_signatures: class_constructor_signatures(class, "__init__"),
                    new_signatures: class_constructor_signatures(class, "__new__"),
                }
            })
            .collect();
        Self { funcs, classes }
    }

    fn from_source(source: &str) -> Option<Self> {
        let parsed = parse_module_guarded(source).ok()?;
        Some(Self::from_stmts(parsed.suite()))
    }

    fn resolve_at(&self, offset: usize) -> Option<(String, Vec<Signature>)> {
        if let Some(target) = self
            .funcs
            .iter()
            .find(|function| offset >= function.name_start && offset < function.name_end)
        {
            let overloads = self
                .funcs
                .iter()
                .filter(|function| function.class == target.class && function.name == target.name)
                .map(|function| function.signature.clone())
                .collect();
            let fullname = match target.class.as_deref() {
                Some(class) if target.name == "__init__" || target.name == "__new__" => {
                    format!("ty.{class}.__init__")
                }
                Some(class) => format!("ty.{class}.{}", target.name),
                None => format!("ty.{}", target.name),
            };
            return Some((fullname, overloads));
        }

        if let Some(class) = self
            .classes
            .iter()
            .find(|class| offset >= class.name_start && offset < class.name_end)
        {
            if !class.init_signatures.is_empty() {
                return Some((
                    format!("ty.{}.__init__", class.name),
                    class.init_signatures.clone(),
                ));
            }
            if !class.new_signatures.is_empty() {
                return Some((
                    format!("ty.{}.__init__", class.name),
                    class.new_signatures.clone(),
                ));
            }
        }
        None
    }
}

#[cfg_attr(coverage, coverage(off))]
fn class_constructor_signatures(class: &StmtClassDef, ctor: &str) -> Vec<Signature> {
    class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) if function.name.as_str() == ctor => {
                Some(signature_from_parameters(&function.parameters))
            }
            _ => None,
        })
        .collect()
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
    callee_offset: usize,
    call: Option<&'a ast::ExprCall>,
}

#[cfg_attr(coverage, coverage(off))]
impl<'a> Visitor<'a> for CallAtStart<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.call.is_some() {
            return;
        }
        if let Expr::Call(call) = expr {
            if call.start().to_usize() == self.start
                && Self::callee_offset(call) == Some(self.callee_offset)
            {
                self.call = Some(call);
                return;
            }
        }
        walk_expr(self, expr);
    }
}

impl CallAtStart<'_> {
    #[cfg_attr(coverage, coverage(off))]
    fn callee_offset(call: &ast::ExprCall) -> Option<usize> {
        match &*call.func {
            Expr::Attribute(attr) => Some(attr.attr.range().start().to_usize()),
            Expr::Name(name) => Some(name.range().start().to_usize()),
            _ => None,
        }
    }
}

#[cfg_attr(coverage, coverage(off))]
fn call_at_start(suite: &[Stmt], start: usize, callee_offset: usize) -> Option<&ast::ExprCall> {
    let mut locator = CallAtStart {
        start,
        callee_offset,
        call: None,
    };
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
    if signature
        .parameters
        .first()
        .is_some_and(|first| first.name.as_deref() == Some("self"))
    {
        Signature {
            parameters: signature.parameters.iter().skip(1).cloned().collect(),
        }
    } else {
        signature.clone()
    }
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn violation_max_positional(
    fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
    ignored: bool,
) -> Option<usize> {
    if is_typing_special_form_constructor(fullname) {
        return None;
    }
    if signatures.is_empty()
        || signatures
            .iter()
            .any(|s| !call_exceeds_positional_limit(s, fullname, ignored, positional_count))
    {
        return None;
    }
    Some(
        signatures
            .iter()
            .filter_map(|s| s.max_positional_at_call_site(fullname, ignored))
            .max()
            .unwrap_or(0),
    )
}

#[cfg_attr(coverage, coverage(off))]
fn signature_mapping_fullname(fullname: &str, receiver_already_omitted: bool) -> &str {
    if receiver_already_omitted
        && (fullname.ends_with(".__call__")
            || fullname.ends_with(".__get__")
            || fullname.ends_with(".__set__")
            || fullname.ends_with(".__init__")
            || fullname.ends_with(".__new__"))
    {
        "strict_kwargs.call_site_signature"
    } else {
        fullname
    }
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn callee_is_ignored(config: &Config, fullname: &str) -> bool {
    // A constructor call resolves to `Class.__init__`/`__new__`; also honor an
    // `ignore_names` entry for the class itself (`builtins.str`).
    config.is_ignored(fullname)
        || fullname
            .strip_suffix(".__init__")
            .or_else(|| fullname.strip_suffix(".__new__"))
            .is_some_and(|class| config.is_ignored(class))
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn ty_fallback_callee_is_ignored(config: &Config, fullname: &str) -> bool {
    if callee_is_ignored(config, fullname) {
        return true;
    }
    let Some(rest) = fullname.strip_prefix("ty.") else {
        return false;
    };
    // The ty fallback deliberately synthesizes display-oriented names such as
    // `ty.str.split` for bound builtins. Map that shape back to the documented
    // ignore spelling (`builtins.str.split`) before deciding.
    callee_is_ignored(config, &format!("builtins.{rest}"))
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn ty_fallback_ignore_may_need_definition(config: &Config, fullname: &str) -> bool {
    let Some(rest) = fullname.strip_prefix("ty.") else {
        return false;
    };
    if rest.contains('.') {
        return false;
    }
    config
        .ignore_names
        .iter()
        .any(|name| name.rsplit('.').next() == Some(rest))
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct DefCacheKey {
    path: PathBuf,
    line: u32,
    character: u32,
}

type DefCache = FxHashMap<DefCacheKey, Option<(String, Vec<Signature>)>>;
type DefFileCache = FxHashMap<PathBuf, Option<DefFileIndex>>;
type ParamSignatureCache = FxHashMap<String, Option<Signature>>;

#[derive(Default)]
struct TyDefCaches {
    locations: DefCache,
    files: DefFileCache,
    param_signatures: ParamSignatureCache,
}

#[cfg_attr(coverage, coverage(off))]
fn signature_from_param_text_cached(
    params: &str,
    cache: &mut ParamSignatureCache,
) -> Option<Signature> {
    if let Some(cached) = cache.get(params) {
        return cached.clone();
    }
    let parsed = signature_from_param_text(params);
    cache.insert(params.to_string(), parsed.clone());
    parsed
}

#[cfg_attr(coverage, coverage(off))]
fn resolve_def_location_cached(
    current_path: &Path,
    current_source: &str,
    loc: &crate::ty_resolver::DefLocation,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    def_caches: &mut TyDefCaches,
) -> Option<(String, Vec<Signature>)> {
    let key = DefCacheKey {
        path: loc.path.clone(),
        line: loc.line,
        character: loc.character,
    };
    if let Some(cached) = def_caches.locations.get(&key) {
        return cached.clone();
    }

    let resolved = (|| {
        let (target_path, target) = if same_path(&loc.path, current_path) {
            (current_path, current_source)
        } else {
            if !file_cache.contains_key(&loc.path) {
                file_cache.insert(loc.path.clone(), std::fs::read_to_string(&loc.path).ok());
            }
            (
                loc.path.as_path(),
                file_cache.get(&loc.path).and_then(Option::as_deref)?,
            )
        };
        let off = lsp_to_byte_offset(target, loc.line, loc.character)?;
        if !def_caches.files.contains_key(target_path) {
            let def_index = indexed_files
                .get(target_path)
                .map(|indexed| DefFileIndex::from_stmts(indexed.parsed.suite()))
                .or_else(|| DefFileIndex::from_source(target));
            def_caches
                .files
                .insert(target_path.to_path_buf(), def_index);
        }
        def_caches
            .files
            .get(target_path)
            .and_then(Option::as_ref)?
            .resolve_at(off)
    })();
    def_caches.locations.insert(key, resolved.clone());
    resolved
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "ty definition resolution threads explicit per-run caches through fallback glue"
)]
fn resolve_ty_definition_for_pending(
    ty: &mut TyResolver,
    path: &Path,
    source: &str,
    lsp_index: &LspLineIndex,
    pending: &PendingTy,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    def_caches: &mut TyDefCaches,
) -> Option<(String, Vec<Signature>)> {
    let (line, ch) = lsp_index.position(source, pending.callee_offset);
    let id = ty.ask("textDocument/definition", path, line, ch)?;
    let response = ty.take(id)?;
    resolve_first_def_location(
        path,
        source,
        &response,
        indexed_files,
        file_cache,
        def_caches,
    )
}

/// Resolve a `textDocument/definition` response to the first usable
/// definition, trying ty's locations in order. ty's relative ordering of a
/// multi-location answer depends on the answering server's open/query
/// history, and the leading entry is often a local binding the definition
/// parser cannot use — trying each location keeps the result independent of
/// that ordering whenever exactly one location resolves, and recovers
/// definitions a first-entry-only read would lose.
// ty-fallback helper; excluded (see `collect_defs`).
#[cfg_attr(coverage, coverage(off))]
fn resolve_first_def_location(
    path: &Path,
    source: &str,
    response: &serde_json::Value,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    def_caches: &mut TyDefCaches,
) -> Option<(String, Vec<Signature>)> {
    locations_from_value(response).iter().find_map(|loc| {
        resolve_def_location_cached(path, source, loc, indexed_files, file_cache, def_caches)
    })
}

// ty-fallback helper; excluded (see `collect_defs`).
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "threads one more resolved call fact into the ty diagnostic helper"
)]
fn emit_if_violation(
    fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
    ignored: bool,
    source: &str,
    call_start: usize,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<usize> {
    emit_if_violation_with_signature_fullname(
        fullname,
        fullname,
        signatures,
        positional_count,
        ignored,
        source,
        call_start,
        path,
        diagnostics,
        None,
    )
}

#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "diagnostic and signature fullnames differ only for ty bound dunders"
)]
fn emit_if_violation_with_signature_fullname(
    diagnostic_fullname: &str,
    signature_fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
    ignored: bool,
    source: &str,
    call_start: usize,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
    line_starts: Option<&[usize]>,
) -> Option<usize> {
    let max_positional =
        violation_max_positional(signature_fullname, signatures, positional_count, ignored)?;
    let offset = u32::try_from(call_start).unwrap_or(u32::MAX);
    let offset = TextSize::new(offset);
    let (line, column) = line_starts.map_or_else(
        || line_column(source, offset),
        |starts| line_column_from_starts(source, starts, offset),
    );
    diagnostics.push(Diagnostic {
        path: path.to_path_buf(),
        line,
        column,
        callee: format_callee_display(diagnostic_fullname),
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

fn ty_hover_signature_is_safe_for_fix(
    name: &str,
    owner: Option<&str>,
    positional_count: usize,
) -> bool {
    positional_count != 1
        || (!name.contains('@') && !owner.is_some_and(|owner| owner.contains('@')))
}

fn parameter_name_is_safe_keyword_target(name: &str) -> bool {
    !name.starts_with("__") || name.ends_with("__")
}

fn callable_name_is_private_keyword_boundary(callee_fullname: &str) -> bool {
    callee_fullname
        .rsplit('.')
        .next()
        .is_some_and(|name| name.starts_with('_') && !name.starts_with("__"))
}

#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "threads the resolved ty call facts into the existing fixer \
              insertion helper"
)]
fn ty_call_fix_insertions(
    index: Option<&DefinitionIndex>,
    fix_ast: TyFixAst<'_>,
    pending: &PendingTy,
    callee_fullname: &str,
    signature: &Signature,
    max_positional: usize,
    positional_count: usize,
    receiver_is_explicit: bool,
    receiver_already_omitted: bool,
) -> Result<Vec<Insertion>, DeclinedFixReason> {
    if !signature_is_fully_named(signature) {
        return Err(DeclinedFixReason::UnsupportedSignatureShape);
    }
    if callee_fullname.ends_with(".__get__") || callee_fullname.ends_with(".__set__") {
        return Err(DeclinedFixReason::UnsupportedSignatureShape);
    }
    let Some(call) = call_at_start(fix_ast.suite, pending.call_start, pending.callee_offset) else {
        return Err(DeclinedFixReason::UnsupportedSignatureShape);
    };
    if let (
        Some(index),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }),
        Some((class_fullname, method)),
    ) = (index, &*call.func, callee_fullname.rsplit_once('.'))
    {
        if !receiver_is_class_object(value, class_fullname)
            && method == attr.as_str()
            && index.has_overriding_method_matching_class_name(class_fullname, method)
        {
            return Err(DeclinedFixReason::UnsupportedSignatureShape);
        }
    }
    if !receiver_is_explicit
        && !receiver_already_omitted
        && matches!(&*call.func, Expr::Attribute(_))
        && (callee_fullname.ends_with(".__init__")
            || callee_fullname.ends_with(".__new__")
            || callee_fullname.ends_with(".__call__"))
    {
        return Err(DeclinedFixReason::UnsupportedSignatureShape);
    }
    // Ty hovers are already call-site oriented for bound methods, so avoid
    // the built-in resolver's attribute-name receiver heuristic here. The one
    // exception is an unbound `def` hover with leading `self`/`cls`, where
    // `strip_unbound_receiver` proved the first positional is explicit.
    call_fix_insertions(
        call,
        fix_ast.tokens,
        signature_mapping_fullname(callee_fullname, receiver_already_omitted),
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
    index: Option<&DefinitionIndex>,
    fix_ast: Option<TyFixAst<'_>>,
    pending: &PendingTy,
    callee_fullname: &str,
    signature: &Signature,
    max_positional: usize,
    positional_count: usize,
    receiver_is_explicit: bool,
    receiver_already_omitted: bool,
) {
    let Some(fixes) = fixes.as_mut() else {
        return;
    };
    if !pending.rewrite_args_are_statically_precise {
        fixes
            .declined_fix_reasons
            .push(DeclinedFixReason::UnsupportedSignatureShape);
        return;
    }
    if callable_name_is_private_keyword_boundary(callee_fullname) {
        fixes
            .declined_fix_reasons
            .push(DeclinedFixReason::UnsupportedSignatureShape);
        return;
    }
    let Some(fix_ast) = fix_ast else {
        fixes
            .declined_fix_reasons
            .push(DeclinedFixReason::UnsupportedSignatureShape);
        return;
    };
    let insertions = match ty_call_fix_insertions(
        index,
        fix_ast,
        pending,
        callee_fullname,
        signature,
        max_positional,
        positional_count,
        receiver_is_explicit,
        receiver_already_omitted,
    ) {
        Ok(insertions) => insertions,
        Err(reason) => {
            fixes.declined_fix_reasons.push(reason);
            return;
        }
    };
    let original_len = fixes.insertions.len();
    for insertion in insertions {
        if !fixes.insertions.contains(&insertion) {
            fixes.insertions.push(insertion);
        }
    }
    if fixes.insertions.len() != original_len {
        *fixes.fixed_calls += 1;
    }
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
    _all_files: &[PathBuf],
    index: &DefinitionIndex,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    python_env: Option<&Path>,
    path: &Path,
    source: &str,
    pending: &[PendingTy],
    pending_groups: &[Option<u32>],
    config: &Config,
    ty_file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    ty_def_caches: &mut TyDefCaches,
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
        *ty = Some(start_ty_for_fallback(project_root, python_env)?);
    }
    if let Some(ty) = ty.as_mut() {
        resolve_pending_with_ty(
            ty,
            index,
            path,
            source,
            pending,
            pending_groups,
            config,
            indexed_files,
            ty_file_cache,
            ty_def_caches,
            diagnostics,
            fixes,
        );
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

/// Start ty for fallback queries. Query sites open the current file before
/// asking for hover/definition; the client advertises pull diagnostics, so
/// ty computes nothing for an opened file until a query demands it and
/// there is no per-file warm-up cost to wait out.
///
/// `ty`-subprocess orchestration like [`start_ty`]; excluded from the coverage
/// gate for the same reason.
#[cfg_attr(coverage, coverage(off))]
fn start_ty_for_fallback(
    project_root: &Path,
    python_env: Option<&Path>,
) -> Result<TyResolver, CheckError> {
    start_ty(project_root, python_env)
}

struct TyFixes<'a> {
    insertions: &'a mut Vec<Insertion>,
    fixed_calls: &'a mut usize,
    declined_fix_reasons: &'a mut Vec<DeclinedFixReason>,
}

#[derive(Clone, Copy)]
struct TyFixAst<'a> {
    suite: &'a [Stmt],
    tokens: &'a Tokens,
}

/// Try to rewrite built-in-diagnosed overload violations by asking ty for the
/// hover at the exact call site. The diagnostic is already recorded, so this
/// path is fix-only: it must never emit another diagnostic.
#[cfg_attr(coverage, coverage(off))]
#[allow(clippy::too_many_arguments)]
fn resolve_overload_fixes_with_ty(
    ty: &mut Option<TyResolver>,
    ty_start_attempted: &mut bool,
    project_root: &Path,
    _all_files: &[PathBuf],
    index: &DefinitionIndex,
    python_env: Option<&Path>,
    path: &Path,
    source: &str,
    pending: &[PendingTyOverloadFix],
    mut fixes: Option<TyFixes<'_>>,
) {
    if pending.is_empty() {
        return;
    }
    if !*ty_start_attempted {
        *ty_start_attempted = true;
        let Ok(started) = start_ty_for_fallback(project_root, python_env) else {
            record_declined_fixes(
                &mut fixes,
                DeclinedFixReason::UnresolvedOverload,
                pending.len(),
            );
            return;
        };
        *ty = Some(started);
    }
    let Some(ty) = ty.as_mut() else {
        record_declined_fixes(
            &mut fixes,
            DeclinedFixReason::UnresolvedOverload,
            pending.len(),
        );
        return;
    };
    if ty.ensure_open(path, source).is_none() {
        record_declined_fixes(
            &mut fixes,
            DeclinedFixReason::UnresolvedOverload,
            pending.len(),
        );
        return;
    }
    let parsed_for_fixes = fixes.as_ref().and_then(|_| parse_module(source).ok());
    let fix_ast = parsed_for_fixes.as_ref().map(|parsed| TyFixAst {
        suite: parsed.suite(),
        tokens: parsed.tokens(),
    });
    let lsp_index = LspLineIndex::new(source);

    for chunk in pending.chunks(TY_MAX_IN_FLIGHT) {
        let hover_ids: Vec<Option<i64>> = chunk
            .iter()
            .map(|p| {
                let (line, ch) = lsp_index.position(source, p.pending.callee_offset);
                ty.ask("textDocument/hover", path, line, ch)
            })
            .collect();

        for (item, hover_id) in chunk.iter().zip(hover_ids) {
            let raw = hover_id
                .and_then(|id| ty.take(id))
                .as_ref()
                .and_then(hover_text);
            let Some(raw) = raw else {
                record_declined_fix(&mut fixes, DeclinedFixReason::UnresolvedOverload);
                continue;
            };
            record_selected_overload_fix(&mut fixes, index, fix_ast, item, &raw);
        }
    }
}

#[cfg_attr(coverage, coverage(off))]
fn record_declined_fix(fixes: &mut Option<TyFixes<'_>>, reason: DeclinedFixReason) {
    if let Some(fixes) = fixes.as_mut() {
        fixes.declined_fix_reasons.push(reason);
    }
}

#[cfg_attr(coverage, coverage(off))]
fn record_declined_fixes(fixes: &mut Option<TyFixes<'_>>, reason: DeclinedFixReason, count: usize) {
    if let Some(fixes) = fixes.as_mut() {
        fixes
            .declined_fix_reasons
            .extend((0..count).map(|_| reason));
    }
}

#[cfg_attr(coverage, coverage(off))]
fn record_selected_overload_fix(
    fixes: &mut Option<TyFixes<'_>>,
    index: &DefinitionIndex,
    fix_ast: Option<TyFixAst<'_>>,
    item: &PendingTyOverloadFix,
    raw_hover: &str,
) {
    let p = &item.pending;

    if let Some(sig) = parse_hover_signature(raw_hover) {
        let Some(signature) = signature_from_param_text(&sig.params) else {
            record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
            return;
        };
        let (effective_signature, positional_count, receiver_is_explicit) =
            strip_unbound_receiver(signature.clone(), p.positional_count, sig.owner.is_none());
        let receiver_already_omitted = sig.owner.is_some();
        if !selected_overload_arm_is_unambiguous(&effective_signature, &item.candidate_signatures) {
            record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
            return;
        }
        let signature_fullname =
            signature_mapping_fullname(&item.callee_fullname, receiver_already_omitted);
        let Some(max_positional) = violation_max_positional(
            signature_fullname,
            std::slice::from_ref(&effective_signature),
            positional_count,
            false,
        ) else {
            record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
            return;
        };
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
        if !item.rewrite_args_are_statically_precise {
            record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
            return;
        }
        if !ty_hover_signature_is_safe_for_fix(&sig.name, sig.owner.as_deref(), p.positional_count)
        {
            record_declined_fix(fixes, DeclinedFixReason::UnsupportedSignatureShape);
            return;
        }
        record_ty_fix(
            fixes,
            Some(index),
            fix_ast,
            p,
            &item.callee_fullname,
            fix_signature,
            max_positional,
            fix_positional_count,
            receiver_is_explicit,
            receiver_already_omitted,
        );
        return;
    }

    let overloads: Vec<Signature> = parse_callable_type_overloads(raw_hover)
        .iter()
        .filter_map(|params| signature_from_param_text(params))
        .collect();
    let [signature] = overloads.as_slice() else {
        // A hover that still reports multiple overload arms has not selected
        // a unique callable. Leave the existing diagnostic declined.
        record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
        return;
    };
    if !selected_overload_arm_is_unambiguous(signature, &item.candidate_signatures) {
        record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
        return;
    }
    let Some(max_positional) = violation_max_positional(
        signature_mapping_fullname(&item.callee_fullname, true),
        std::slice::from_ref(signature),
        p.positional_count,
        false,
    ) else {
        record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
        return;
    };
    // The selected callable-type arm is already call-site oriented (as in the
    // normal ty fallback). It is safe only if it has complete parameter names.
    if !item.rewrite_args_are_statically_precise {
        record_declined_fix(fixes, DeclinedFixReason::UnresolvedOverload);
        return;
    }
    record_ty_fix(
        fixes,
        Some(index),
        fix_ast,
        p,
        &item.callee_fullname,
        signature,
        max_positional,
        p.positional_count,
        false,
        true,
    );
}

#[cfg_attr(coverage, coverage(off))]
const fn is_precise_overload_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StringLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
    )
}

#[cfg_attr(coverage, coverage(off))]
fn annotation_is_precise_overload_type(annotation: &str) -> bool {
    let annotation = annotation.trim();
    !annotation.is_empty()
        && !annotation.contains('|')
        && !matches!(annotation, "Any" | "typing.Any" | "object" | "Unknown")
}

fn annotation_is_builtin_receiver_type(annotation: &str) -> bool {
    let annotation = annotation.trim().trim_matches(['"', '\'']);
    let annotation = annotation.strip_prefix("builtins.").unwrap_or(annotation);
    matches!(
        annotation,
        "str"
            | "bytes"
            | "int"
            | "float"
            | "complex"
            | "bool"
            | "list"
            | "tuple"
            | "dict"
            | "set"
            | "frozenset"
    )
}

#[cfg_attr(coverage, coverage(off))]
fn selected_overload_arm_is_unambiguous(selected: &Signature, candidates: &[Signature]) -> bool {
    candidates
        .iter()
        .filter(|candidate| same_parameter_mapping(selected, candidate))
        .take(2)
        .count()
        == 1
}

#[cfg_attr(coverage, coverage(off))]
fn same_parameter_mapping(left: &Signature, right: &Signature) -> bool {
    left.parameters.len() == right.parameters.len()
        && left
            .parameters
            .iter()
            .zip(&right.parameters)
            .all(|(left, right)| left.kind == right.kind && left.name == right.name)
}

/// Resolve, in bounded pipelined batches per file, the calls the built-in
/// resolver missed: hover (precise, overload- and inheritance-resolved,
/// stdlib too), then goto-definition for the rest (constructors and callable
/// `__call__` definitions). Fails closed.
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
/// [`identifier_at`], `byte_offset_to_lsp`, [`lsp_to_byte_offset`],
/// [`locations_from_value`], [`DefFileIndex::resolve_at`] and
/// [`emit_if_violation_with_signature_fullname`].
#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::too_many_arguments,
    reason = "ty fallback orchestration shares per-file resolver state explicitly"
)]
fn resolve_pending_with_ty(
    ty: &mut TyResolver,
    index: &DefinitionIndex,
    path: &Path,
    source: &str,
    pending: &[PendingTy],
    pending_groups: &[Option<u32>],
    config: &Config,
    indexed_files: &FxHashMap<PathBuf, IndexedFile>,
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    def_caches: &mut TyDefCaches,
    diagnostics: &mut Vec<Diagnostic>,
    mut fixes: Option<TyFixes<'_>>,
) {
    if pending.is_empty() || ty.ensure_open(path, source).is_none() {
        return;
    }
    let source_line_starts = line_starts(source);
    let lsp_index = LspLineIndex::new(source);
    let parsed_for_fixes = fixes.as_ref().and_then(|_| parse_module(source).ok());
    let fix_ast = parsed_for_fixes.as_ref().map(|parsed| TyFixAst {
        suite: parsed.suite(),
        tokens: parsed.tokens(),
    });
    // Calls in the same hover group (same receiver binding, same attribute;
    // see `CallChecker::hover_group_for_call`) are proven to hover
    // identically, so the first member's raw answer is reused for the rest
    // — the bulk of a test suite's `self.assert*` calls collapse to one
    // round-trip per method. The request stream stays a pure function of
    // the (sorted) work list, so reproducibility is unaffected.
    let group_of = |i: usize| pending_groups.get(i).copied().flatten();
    let mut group_hover: FxHashMap<u32, Option<String>> = FxHashMap::default();
    let mut group_def: FxHashMap<u32, Option<serde_json::Value>> = FxHashMap::default();

    // Phase A: pipeline hover requests in bounded batches, then collect.
    let mut needs_def: Vec<usize> = Vec::new();
    for chunk_start in (0..pending.len()).step_by(TY_MAX_IN_FLIGHT) {
        let chunk_end = pending.len().min(chunk_start + TY_MAX_IN_FLIGHT);
        let hover_ids: Vec<(usize, Option<i64>)> = (chunk_start..chunk_end)
            .map(|i| {
                if group_of(i).is_some_and(|g| group_hover.contains_key(&g)) {
                    // Answered from the group cache; no request needed.
                    return (i, None);
                }
                let (line, ch) = lsp_index.position(source, pending[i].callee_offset);
                (i, ty.ask("textDocument/hover", path, line, ch))
            })
            .collect();

        for (i, hover_id) in hover_ids {
            let p = &pending[i];
            let group = group_of(i);
            let cached = group.and_then(|g| group_hover.get(&g).cloned());
            let raw = if let Some(cached) = cached {
                cached
            } else {
                let raw = hover_id
                    .and_then(|id| ty.take(id))
                    .as_ref()
                    .and_then(hover_text);
                // Only a usable callable signature is group-consistent. ty
                // answers a member sitting in code it deems unreachable — e.g.
                // a `sys.platform`-guarded branch live on one OS but dead on
                // another — with the bottom type (`Never`, or no hover at all)
                // rather than the receiver's real signature. Caching that
                // answer would suppress the group's *live* members, which do
                // resolve to a real signature. Leave such answers uncached so
                // each remaining member falls back to its own hover; the first
                // member that yields a real signature re-seeds the shared
                // answer for the rest.
                if let Some(g) = group {
                    if raw
                        .as_deref()
                        .is_some_and(|raw| parse_hover_signature(raw).is_some())
                    {
                        group_hover.insert(g, raw.clone());
                    }
                }
                raw
            };
            let Some(raw) = raw else {
                needs_def.push(i);
                continue;
            };

            // `def …`/`bound method …` display: a single, named signature.
            if let Some(sig) = parse_hover_signature(&raw) {
                let Some(signature) =
                    signature_from_param_text_cached(&sig.params, &mut def_caches.param_signatures)
                else {
                    continue;
                };
                let (effective_signature, positional_count, receiver_is_explicit) =
                    strip_unbound_receiver(
                        signature.clone(),
                        p.positional_count,
                        sig.owner.is_none(),
                    );
                let fullname = match &sig.owner {
                    Some(owner) => {
                        let owner = owner.split('[').next().unwrap_or(owner);
                        let owner = owner.rsplit('.').next().unwrap_or(owner);
                        format!("ty.{owner}.{}", sig.name)
                    }
                    None => format!("ty.{}", sig.name),
                };
                let receiver_already_omitted = sig.owner.is_some();
                let mut ignored = ty_fallback_callee_is_ignored(config, &fullname);
                if !ignored && ty_fallback_ignore_may_need_definition(config, &fullname) {
                    if let Some((def_fullname, _)) = resolve_ty_definition_for_pending(
                        ty,
                        path,
                        source,
                        &lsp_index,
                        p,
                        indexed_files,
                        file_cache,
                        def_caches,
                    ) {
                        ignored = ty_fallback_callee_is_ignored(config, &def_fullname);
                    }
                }
                let signature_fullname =
                    signature_mapping_fullname(&fullname, receiver_already_omitted);
                if let Some(max_positional) = emit_if_violation_with_signature_fullname(
                    &fullname,
                    signature_fullname,
                    std::slice::from_ref(&effective_signature),
                    positional_count,
                    ignored,
                    source,
                    p.call_start,
                    path,
                    diagnostics,
                    Some(&source_line_starts),
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
                    if ty_hover_signature_is_safe_for_fix(
                        &sig.name,
                        sig.owner.as_deref(),
                        p.positional_count,
                    ) {
                        record_ty_fix(
                            &mut fixes,
                            Some(index),
                            fix_ast,
                            p,
                            &fullname,
                            fix_signature,
                            max_positional,
                            fix_positional_count,
                            receiver_is_explicit,
                            receiver_already_omitted,
                        );
                    } else {
                        record_declined_fix(
                            &mut fixes,
                            DeclinedFixReason::UnsupportedSignatureShape,
                        );
                    }
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
                .filter_map(|params| {
                    signature_from_param_text_cached(params, &mut def_caches.param_signatures)
                })
                .collect();
            if overloads.is_empty() {
                needs_def.push(i);
                continue;
            }
            let name = identifier_at(source, p.callee_offset).unwrap_or_default();
            let fullname = format!("ty.{name}");
            let mut ignored = ty_fallback_callee_is_ignored(config, &fullname);
            if !ignored && ty_fallback_ignore_may_need_definition(config, &fullname) {
                if let Some((def_fullname, _)) = resolve_ty_definition_for_pending(
                    ty,
                    path,
                    source,
                    &lsp_index,
                    p,
                    indexed_files,
                    file_cache,
                    def_caches,
                ) {
                    ignored = ty_fallback_callee_is_ignored(config, &def_fullname);
                }
            }
            if let Some(max_positional) = emit_if_violation_with_signature_fullname(
                &fullname,
                &fullname,
                &overloads,
                p.positional_count,
                ignored,
                source,
                p.call_start,
                path,
                diagnostics,
                Some(&source_line_starts),
            ) {
                if let [signature] = overloads.as_slice() {
                    record_ty_fix(
                        &mut fixes,
                        Some(index),
                        fix_ast,
                        p,
                        &fullname,
                        signature,
                        max_positional,
                        p.positional_count,
                        false,
                        true,
                    );
                } else if fixes.is_some() {
                    record_declined_fix(&mut fixes, DeclinedFixReason::AmbiguousTyHover);
                }
            }
        }
    }
    // Phase B: pipeline goto-definition for hover misses (constructors) in
    // bounded batches too. Hover misses are group-consistent (the cached
    // hover answer routed every member here), so definition answers are
    // reused per group the same way.
    for chunk in needs_def.chunks(TY_MAX_IN_FLIGHT) {
        let def_ids: Vec<(usize, Option<i64>)> = chunk
            .iter()
            .map(|&i| {
                if group_of(i).is_some_and(|g| group_def.contains_key(&g)) {
                    // Answered from the group cache; no request needed.
                    return (i, None);
                }
                let (line, ch) = lsp_index.position(source, pending[i].callee_offset);
                (i, ty.ask("textDocument/definition", path, line, ch))
            })
            .collect();
        for (i, id) in def_ids {
            let group = group_of(i);
            let cached = group.and_then(|g| group_def.get(&g).cloned());
            let raw_def = if let Some(cached) = cached {
                cached
            } else {
                let raw = id.and_then(|id| ty.take(id));
                // Only a positive answer is group-consistent; see the matching
                // note in the hover phase. A definition miss at an unreachable
                // member must not suppress live members of the same group.
                if let (Some(g), Some(_)) = (group, raw.as_ref()) {
                    group_def.insert(g, raw.clone());
                }
                raw
            };
            // A `ty` goto-definition target is a dependency/stub. Each
            // location goes through the guarded parser so a deeply-nested
            // target is rejected gracefully rather than crashing the
            // analysis thread (issue #83 follow-up to #54). The two-stage
            // pre-filter keeps typical stubs cheap (byte count only); only
            // genuinely deep ones pay the tokeniser scan — and those would
            // have crashed the old unguarded call. A response whose every
            // location is too deep or unparsable is silently skipped, same
            // fail-closed behaviour as before.
            if let Some((fullname, sigs)) = raw_def.as_ref().and_then(|response| {
                resolve_first_def_location(
                    path,
                    source,
                    response,
                    indexed_files,
                    file_cache,
                    def_caches,
                )
            }) {
                let ignored = ty_fallback_callee_is_ignored(config, &fullname);
                let max_positional = emit_if_violation_with_signature_fullname(
                    &fullname,
                    &fullname,
                    &sigs,
                    pending[i].positional_count,
                    ignored,
                    source,
                    pending[i].call_start,
                    path,
                    diagnostics,
                    Some(&source_line_starts),
                );
                if let Some(max_positional) = max_positional {
                    let mut attempted_fix = false;
                    if fullname.ends_with(".__call__") {
                        if let [signature] = sigs.as_slice() {
                            attempted_fix = true;
                            record_ty_fix(
                                &mut fixes,
                                Some(index),
                                fix_ast,
                                &pending[i],
                                &fullname,
                                signature,
                                max_positional,
                                pending[i].positional_count,
                                false,
                                false,
                            );
                        }
                    }
                    if !attempted_fix {
                        record_declined_fix(&mut fixes, DeclinedFixReason::TyDefinitionOnly);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::{
        bound_import_name, call_shape_fingerprint, collect_python_files, decorator_tail,
        has_staticmethod_or_classmethod_decorator, is_ignored_path,
        is_typing_special_form_constructor, parameter_name_is_safe_keyword_target,
        plan_rewrite_insertions, process_scan_outcome_for_ty, receiver_is_class_object,
        record_ty_fix, signature_is_fully_named, strip_unbound_receiver,
        ty_hover_signature_is_safe_for_fix, without_leading_self, CallAtStart, DeclinedFixReason,
        FileScan, FileSelection, FixOptIns, IfBranchTraversal, InOrderReleaser, PendingTy,
        PendingTyWork, ScanOutcome, TyFixAst, TyFixes, TyShardAssigner,
    };
    use crate::config::Config;
    use crate::error::CheckError;
    use crate::fix::Insertion;
    use crate::signature::{Parameter, ParameterKind, Signature};
    use std::path::Path;

    fn ty_work(path: &str, pending_calls: usize) -> PendingTyWork {
        PendingTyWork {
            path: Path::new(path).to_path_buf(),
            source: String::new(),
            pending: (0..pending_calls)
                .map(|_| PendingTy {
                    callee_offset: 0,
                    call_start: 0,
                    positional_count: 1,
                    rewrite_args_are_statically_precise: false,
                })
                .collect(),
            pending_groups: vec![None; pending_calls],
        }
    }

    #[test]
    fn ty_shard_assigner_balances_pending_calls_deterministically() {
        // Greedy least-loaded assignment in sorted order: the heavy first
        // file fills shard 0, the next files go to the emptier shard (ties
        // break toward the lowest shard index).
        let work = [
            ty_work("a.py", 10),
            ty_work("b.py", 1),
            ty_work("c.py", 1),
            ty_work("d.py", 1),
        ];
        let mut assigner = TyShardAssigner::new(2);
        let owners: Vec<usize> = work
            .iter()
            .map(|w| assigner.assign(w.pending.len()))
            .collect();
        assert_eq!(owners, vec![0, 1, 1, 1]);
    }

    #[test]
    fn ty_shard_assigner_floors_empty_pending_at_one() {
        // A file with no pending calls still counts as load 1 so it cannot
        // make its shard look free forever.
        let mut assigner = TyShardAssigner::new(4);
        let owners: Vec<usize> = (0..5).map(|_| assigner.assign(0)).collect();
        assert_eq!(owners, vec![0, 1, 2, 3, 0]);
    }

    #[test]
    fn in_order_releaser_yields_contiguous_prefixes() {
        let mut releaser = InOrderReleaser::new();
        assert_eq!(releaser.push(2, "c"), Vec::<&str>::new());
        assert_eq!(releaser.push(0, "a"), vec!["a"]);
        assert_eq!(releaser.push(3, "d"), Vec::<&str>::new());
        assert_eq!(releaser.push(1, "b"), vec!["b", "c", "d"]);
    }

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
    fn file_selection_explicit_force_and_external_paths() {
        let root = tempfile::Builder::new()
            .prefix("strictkw")
            .tempdir()
            .expect("tempdir");
        let other = tempfile::Builder::new()
            .prefix("strictkw")
            .tempdir()
            .expect("tempdir");
        let generated = root.path().join("generated").join("api.py");
        let hidden = root.path().join(".generated.py");
        let external_generated = other.path().join("generated").join("api.py");
        let config = Config {
            extend_exclude: vec!["generated".to_string()],
            ..Config::default()
        };
        let selection = FileSelection::new(root.path(), &config).expect("selection");

        assert!(selection.is_excluded(&generated, false, false));
        assert!(!selection.is_excluded(&generated, false, true));
        assert!(selection.is_excluded(Path::new("generated/api.py"), false, false));
        assert!(selection.is_excluded(&hidden, false, false));
        assert!(!selection.is_excluded(&hidden, false, true));
        assert!(!selection.is_excluded(&external_generated, false, false));

        let relative_selection = FileSelection::new(
            Path::new("project"),
            &Config {
                extend_exclude: vec!["generated".to_string()],
                ..Config::default()
            },
        )
        .expect("relative selection");
        assert!(relative_selection.is_excluded(Path::new("generated/api.py"), false, false));

        let forced = FileSelection::new(
            root.path(),
            &Config {
                force_exclude: true,
                ..config
            },
        )
        .expect("forced selection");
        assert!(forced.is_excluded(&generated, false, true));
        assert!(forced.is_excluded(&hidden, false, true));
    }

    #[test]
    fn collect_python_files_filters_non_python_and_excluded_files() {
        let root = tempfile::Builder::new()
            .prefix("strictkw")
            .tempdir()
            .expect("tempdir");
        for path in ["src/real.py", "src/generated.py", "src/data.txt"] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(&file, "").expect("write");
        }

        let files = collect_python_files(
            root.path(),
            &[root.path().to_path_buf()],
            &Config {
                extend_exclude: vec!["src/generated.py".to_string()],
                ..Config::default()
            },
        )
        .expect("collect");

        assert_eq!(files, vec![root.path().join("src/real.py")]);
    }

    #[test]
    fn collect_python_files_reports_invalid_extend_exclude_pattern() {
        let root = tempfile::tempdir().expect("tempdir");
        let Err(error) = collect_python_files(
            root.path(),
            &[root.path().to_path_buf()],
            &Config {
                extend_exclude: vec!["[z-a]".to_string()],
                ..Config::default()
            },
        ) else {
            panic!("invalid glob must be rejected");
        };
        match error {
            CheckError::ConfigInvalid { path, message } => {
                assert!(path.ends_with("pyproject.toml"));
                assert!(
                    message.contains("invalid `extend_exclude` pattern"),
                    "message: {message}"
                );
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn file_selection_reports_invalid_extend_exclude_pattern() {
        let root = tempfile::tempdir().expect("tempdir");
        let Err(error) = FileSelection::new(
            root.path(),
            &Config {
                extend_exclude: vec!["[z-a]".to_string()],
                ..Config::default()
            },
        ) else {
            panic!("invalid glob must be rejected");
        };
        match error {
            CheckError::ConfigInvalid { path, message } => {
                assert!(path.ends_with("pyproject.toml"));
                assert!(
                    message.contains("invalid `extend_exclude` pattern"),
                    "message: {message}"
                );
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
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

    #[test]
    fn decorator_tail_covers_attribute_call_and_dynamic_shapes() {
        let parsed = parse_module(
            "\
class C:
    @decorators.staticmethod
    @classmethod()
    @(lambda fn: fn)
    def m(self): ...
",
        )
        .expect("parse decorators");
        let Some(super::Stmt::ClassDef(class_def)) = parsed.suite().first() else {
            panic!("expected class");
        };
        let Some(super::Stmt::FunctionDef(method_def)) = class_def.body.first() else {
            panic!("expected method");
        };

        let decorators = &method_def.decorator_list;
        assert_eq!(
            decorator_tail(&decorators[0].expression),
            Some("staticmethod")
        );
        assert_eq!(
            decorator_tail(&decorators[1].expression),
            Some("classmethod")
        );
        assert_eq!(decorator_tail(&decorators[2].expression), None);
        assert!(has_staticmethod_or_classmethod_decorator(decorators));
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
    fn receiver_class_object_detection_uses_receiver_tail() {
        with_call_func("C.m(1)\n", |func| {
            let Expr::Attribute(attr) = func else {
                panic!("expected attribute call");
            };
            assert!(receiver_is_class_object(&attr.value, "pkg.C"));
            assert!(!receiver_is_class_object(&attr.value, "pkg.D"));
        });
        with_call_func("pkg.C.m(1)\n", |func| {
            let Expr::Attribute(attr) = func else {
                panic!("expected attribute call");
            };
            assert!(receiver_is_class_object(&attr.value, "pkg.C"));
        });
        with_call_func("factory().m(1)\n", |func| {
            let Expr::Attribute(attr) = func else {
                panic!("expected attribute call");
            };
            assert!(!receiver_is_class_object(&attr.value, "pkg.C"));
        });
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
        assert!(parameter_name_is_safe_keyword_target("a"));
        assert!(parameter_name_is_safe_keyword_target("__dunder__"));
        assert!(!parameter_name_is_safe_keyword_target("__fp"));
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
        assert!(ty_hover_signature_is_safe_for_fix("__init__", None, 1));
        assert!(ty_hover_signature_is_safe_for_fix(
            "append",
            Some("list[int]"),
            1
        ));
        assert!(ty_hover_signature_is_safe_for_fix("Self@__init__", None, 2));
        assert!(!ty_hover_signature_is_safe_for_fix(
            "Self@__init__",
            None,
            1
        ));
        assert!(!ty_hover_signature_is_safe_for_fix(
            "method",
            Some("Self@C"),
            1
        ));
    }

    #[test]
    fn ty_fix_recording_decline_branches_are_explicit() {
        let pending = PendingTy {
            callee_offset: 0,
            call_start: 0,
            positional_count: 1,
            rewrite_args_are_statically_precise: true,
        };
        let named = sig(&["a"]);

        // `check_paths` passes no fix context. The ty path still considers
        // the violation, but rewrite recording must be a no-op.
        let mut no_fix_context = None;
        record_ty_fix(
            &mut no_fix_context,
            None,
            None,
            &pending,
            "ty.f",
            &named,
            0,
            1,
            false,
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
        let mut declined_fix_reasons = Vec::new();
        let mut fixes = Some(TyFixes {
            insertions: &mut insertions,
            fixed_calls: &mut fixed_calls,
            declined_fix_reasons: &mut declined_fix_reasons,
        });
        let parsed = ruff_python_parser::parse_module("f(1)\n").expect("parse");
        let fix_ast = TyFixAst {
            suite: parsed.suite(),
            tokens: parsed.tokens(),
        };
        record_ty_fix(
            &mut fixes,
            None,
            Some(fix_ast),
            &pending,
            "ty.f",
            &unnamed,
            0,
            1,
            false,
            false,
        );

        let private = Signature {
            parameters: vec![Parameter {
                name: Some("__fp".to_string()),
                kind: ParameterKind::PositionalOrKeyword,
            }],
        };
        record_ty_fix(
            &mut fixes,
            None,
            Some(fix_ast),
            &pending,
            "ty.f",
            &private,
            0,
            1,
            false,
            false,
        );
        assert!(insertions.is_empty());
        assert_eq!(fixed_calls, 0);
        assert_eq!(
            declined_fix_reasons,
            vec![
                DeclinedFixReason::UnsupportedSignatureShape,
                DeclinedFixReason::UnsupportedSignatureShape
            ]
        );
    }

    #[test]
    fn plan_rewrite_insertions_reports_invalid_rewrite() {
        let err = plan_rewrite_insertions(
            Path::new("bad.py"),
            "f(1)\n",
            &[Insertion {
                at: 3,
                text: "a=".to_string(),
            }],
        )
        .expect_err("rewrite should fail to parse");

        match err {
            super::CheckError::FixProducedInvalidSyntax { path } => {
                assert_eq!(path, Path::new("bad.py"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ----- ty goto-definition resolution internals --------------------

    use super::{
        collect_defs, format_callee_display, identifier_at, resolve_def_at,
        signature_from_param_text, DefFileIndex,
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
        let legacy = resolve_def_at(parsed.suite(), offset).map(|(name, sigs)| (name, sigs.len()));
        let cached = DefFileIndex::from_stmts(parsed.suite())
            .resolve_at(offset)
            .map(|(name, sigs)| (name, sigs.len()));
        assert_eq!(cached, legacy);
        cached
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
            false,
            "x",
            0,
            path,
            &mut d,
        );
        assert!(d.is_empty());

        // No signatures: nothing to check.
        let mut d = Vec::new();
        emit_if_violation("ty.f", &[], 2, false, "x", 0, path, &mut d);
        assert!(d.is_empty());

        // Within the limit (some overload permits it): no diagnostic.
        let mut d = Vec::new();
        emit_if_violation(
            "ty.f",
            std::slice::from_ref(&one),
            0,
            false,
            "f()\n",
            0,
            path,
            &mut d,
        );
        assert!(d.is_empty());

        // Exceeds the limit: one diagnostic with the rendered fields.
        let mut d = Vec::new();
        emit_if_violation("ty.f", &[one], 2, false, "f(1, 2)\n", 0, path, &mut d);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].line, 1);
        assert_eq!(d[0].column, 1);
        assert_eq!(d[0].callee, "\"f\"");
        assert_eq!(d[0].positional_count, 2);
        assert_eq!(d[0].max_positional, 0);

        // Ignored callables are suppressed even when the positional count
        // would otherwise exceed the limit.
        let mut d = Vec::new();
        emit_if_violation(
            "ty.f",
            &[sig(&["a"])],
            2,
            true,
            "f(1, 2)\n",
            0,
            path,
            &mut d,
        );
        assert!(d.is_empty());
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
    use crate::index::DefinitionIndex;
    use ruff_python_ast::visitor::Visitor;
    use ruff_python_ast::{Expr, Stmt};
    use std::path::PathBuf;

    fn run_checker_with_index(source: &str, index: &DefinitionIndex) -> (usize, usize) {
        let config = Config::default();
        let parsed = parse_module(source).expect("parse source");
        let mut checker = CallChecker::new(
            PathBuf::from("main.py"),
            "main".to_string(),
            false,
            source,
            parsed.tokens(),
            index,
            &config,
            FixOptIns::default(),
            false,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
        (checker.diagnostics.len(), checker.ty_pending.len())
    }

    /// The hover group of every deferred call in `source`, in deferral order.
    fn pending_hover_groups(source: &str) -> Vec<Option<u32>> {
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let parsed = parse_module(source).expect("parse source");
        let mut checker = CallChecker::new(
            PathBuf::from("main.py"),
            "main".to_string(),
            false,
            source,
            parsed.tokens(),
            &index,
            &config,
            FixOptIns::default(),
            false,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
        checker.take_pending_hover_groups()
    }

    #[test]
    fn hover_groups_share_same_binding_attribute_and_shape() {
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        self.f(2)
        self.f(1, 2)
        self.g(1)
    def b(self):
        self.f(3)
",
        );
        let [first, second, wider, other_attr, other_method] = groups.as_slice() else {
            panic!("expected five deferred calls, got {groups:?}");
        };
        // Same binding + attribute + shape: one group.
        assert!(first.is_some());
        assert_eq!(first, second);
        // A different arity is a different shape.
        assert!(wider.is_some());
        assert_ne!(first, wider);
        // A different attribute never shares a group.
        assert!(other_attr.is_some());
        assert_ne!(first, other_attr);
        // A different method is a different `self` binding.
        assert!(other_method.is_some());
        assert_ne!(first, other_method);
    }

    #[test]
    fn hover_groups_cover_cls_and_skip_other_receivers() {
        let groups = pending_hover_groups(
            "\
class C:
    @classmethod
    def a(cls):
        cls.f(1)
        cls.f(2)
        other.f(1)
        f(1)
",
        );
        let [first, second, foreign_receiver, bare_name] = groups.as_slice() else {
            panic!("expected four deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
        // Attribute calls on anything but a bound `self`/`cls` parameter,
        // and bare-name calls, are never grouped.
        assert_eq!(*foreign_receiver, None);
        assert_eq!(*bare_name, None);
    }

    #[test]
    fn hover_groups_distinguish_argument_kinds() {
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(x)
        self.f(\"s\")
",
        );
        let [name_arg, string_arg] = groups.as_slice() else {
            panic!("expected two deferred calls, got {groups:?}");
        };
        assert!(name_arg.is_some());
        assert!(string_arg.is_some());
        assert_ne!(name_arg, string_arg);
    }

    #[test]
    fn hover_groups_survive_unrelated_locals_and_keywords() {
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1, key=2)
        x = 1
        self.f(1, key=2)
",
        );
        let [first, second] = groups.as_slice() else {
            panic!("expected two deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[test]
    fn hover_groups_dropped_when_receiver_is_rebound() {
        // Rebinding `self` (any `Store`/`Del` context) poisons the binding —
        // even when the rebinding happens *after* the deferred calls.
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        self.f(2)
        self = q
",
        );
        assert_eq!(groups, vec![None, None]);
    }

    #[test]
    fn hover_groups_dropped_when_receiver_escapes_into_a_call() {
        // `isinstance(self, T)` and friends can narrow `self`; any bare
        // receiver passed as an argument (positional, starred, or keyword)
        // poisons the binding.
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        check(self)
    def b(self):
        self.f(1)
        check(*self)
    def c(self):
        self.f(1)
        check(target=self)
",
        );
        // The `check(...)` calls with a positional argument are deferred
        // too (unresolved bare names), without a group of their own.
        assert!(groups.len() >= 3);
        assert!(groups.iter().all(Option::is_none), "got {groups:?}");
    }

    #[test]
    fn hover_groups_dropped_only_for_the_mentioned_attribute() {
        // A non-callee mention of `self.f` (here: a truthiness test that can
        // narrow the attribute) drops `self.f` groups but leaves `self.g`.
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.g(1)
        self.g(2)
        if self.f:
            self.f(1)
            self.f(2)
",
        );
        let [g_first, g_second, f_first, f_second] = groups.as_slice() else {
            panic!("expected four deferred calls, got {groups:?}");
        };
        assert!(g_first.is_some());
        assert_eq!(g_first, g_second);
        assert_eq!(*f_first, None);
        assert_eq!(*f_second, None);
    }

    #[test]
    fn hover_groups_dropped_when_attribute_is_assigned() {
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        self.f = q
",
        );
        assert_eq!(groups, vec![None]);
    }

    #[test]
    fn hover_groups_dropped_by_bare_receiver_narrowing_positions() {
        // Comparison operand, boolean-op operand, `not`, conditional tests
        // (`if`/`while`/ternary), and `assert` can all narrow a bare name.
        for narrowing in [
            "x = self == other",
            "x = self or other",
            "x = not self",
            "x = 1 if self else 2",
            "if self:\n            pass",
            "while self:\n            pass",
            "assert self",
        ] {
            let source = format!(
                "\
class C:
    def a(self):
        self.f(1)
        {narrowing}
",
            );
            assert_eq!(
                pending_hover_groups(&source),
                vec![None],
                "expected {narrowing:?} to poison the binding"
            );
        }
        // A non-`not` unary on the receiver does not narrow it.
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        x = -self
",
        );
        assert_eq!(groups.len(), 1);
        assert!(groups[0].is_some());
    }

    #[test]
    fn hover_groups_dropped_by_statement_level_rebindings() {
        for rebinding in [
            "match self:\n            case _:\n                pass",
            "try:\n            pass\n        except Exception as self:\n            pass",
            "global self",
            "import self",
            "from x import self",
            "import x as self",
            "from x import y as self",
            "def self():\n            pass",
            "class self:\n            pass",
        ] {
            let source = format!(
                "\
class C:
    def a(self):
        self.f(1)
        {rebinding}
",
            );
            assert_eq!(
                pending_hover_groups(&source),
                vec![None],
                "expected {rebinding:?} to poison the binding"
            );
        }
        // `nonlocal` needs an enclosing function binding to parse; the inner
        // function may rebind the method's `self` through it.
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        def inner():
            nonlocal self
",
        );
        assert_eq!(groups, vec![None]);
    }

    #[test]
    fn hover_groups_track_nested_function_bindings() {
        let groups = pending_hover_groups(
            "\
class C:
    def a(self):
        self.f(1)
        def closes_over(value):
            self.f(2)
        def shadows(self):
            self.f(3)
        g = lambda self: self.f(4)
        h = lambda: self.f(5)
        self.f(6)
",
        );
        let [outer, closure, shadowed, lambda_shadowed, lambda_closure, outer_again] =
            groups.as_slice()
        else {
            panic!("expected six deferred calls, got {groups:?}");
        };
        // A nested function without its own `self` closes over the method's
        // binding and shares its group; one *with* a `self` parameter (def
        // or lambda) introduces a fresh binding.
        assert!(outer.is_some());
        assert_eq!(outer, closure);
        assert_eq!(outer, outer_again);
        assert!(shadowed.is_some());
        assert_ne!(outer, shadowed);
        assert!(lambda_shadowed.is_some());
        assert_ne!(outer, lambda_shadowed);
        assert_ne!(shadowed, lambda_shadowed);
        assert_eq!(outer, lambda_closure);
    }

    #[test]
    fn hover_groups_cover_vararg_and_kwarg_bindings() {
        let groups = pending_hover_groups(
            "\
def odd(*self, **cls):
    self.f(1)
    self.f(2)
    cls.g(1)
",
        );
        let [first, second, kwarg_bound] = groups.as_slice() else {
            panic!("expected three deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
        assert!(kwarg_bound.is_some());
        assert_ne!(first, kwarg_bound);
    }

    #[test]
    fn hover_groups_cover_any_parameter_receiver() {
        // Every parameter is a binding now, not just `self`/`cls`.
        let groups = pending_hover_groups(
            "\
def emitters(out):
    out.emit(1)
    out.emit(2)
",
        );
        let [first, second] = groups.as_slice() else {
            panic!("expected two deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[test]
    fn hover_groups_cover_module_imports_after_the_import() {
        // A module-level import opens a file-wide binding; only call sites
        // *after* it join the group (an earlier site refers to whatever was
        // bound before, if anything).
        let groups = pending_hover_groups(
            "\
helper.do(1)
import helper
helper.do(1)
def inside():
    helper.do(1)
",
        );
        let [before, after, in_function] = groups.as_slice() else {
            panic!("expected three deferred calls, got {groups:?}");
        };
        assert_eq!(*before, None);
        assert!(after.is_some());
        assert_eq!(after, in_function);
    }

    #[test]
    fn hover_groups_poisoned_by_a_second_same_scope_binding() {
        // `import` twice, then both `x = ...` forms: any same-scope
        // rebinding makes the name's type unstable across call sites.
        for source in [
            "import helper\nhelper.do(1)\nimport helper\n",
            "import helper\nhelper.do(1)\nhelper = q\n",
            "from m import helper\nhelper.do(1)\nfrom n import helper\n",
        ] {
            assert_eq!(
                pending_hover_groups(source),
                vec![None],
                "expected a rebinding in {source:?} to poison the group"
            );
        }
    }

    #[test]
    fn hover_groups_cover_bare_name_calls() {
        let groups = pending_hover_groups(
            "\
from decimal import Decimal
Decimal(1)
Decimal(2)
Decimal(1, 2)
undefined(1)
",
        );
        let [first, second, wider, unbound] = groups.as_slice() else {
            panic!("expected four deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
        // A different arity is a different shape.
        assert!(wider.is_some());
        assert_ne!(first, wider);
        // A name with no visible binding has no group.
        assert_eq!(*unbound, None);
    }

    #[test]
    fn hover_groups_cover_single_assignment_locals() {
        // `with open(...) as f` / `f = open(...)` open a local binding; a
        // second assignment in the same scope poisons it.
        let groups = pending_hover_groups(
            "\
def stable(p):
    with open(p) as f:
        f.write(1)
        f.write(2)

def rebound(p):
    g = open(p)
    g.write(1)
    g = open(p)
",
        );
        // The `open(p)` calls are deferred too (unbound bare name, no
        // group); the interesting entries are the `.write(...)` calls.
        let [open_1, first, second, open_2, rebound, open_3] = groups.as_slice() else {
            panic!("expected six deferred calls, got {groups:?}");
        };
        assert_eq!(*open_1, None);
        assert_eq!(*open_2, None);
        assert_eq!(*open_3, None);
        assert!(first.is_some());
        assert_eq!(first, second);
        assert_eq!(*rebound, None);
    }

    #[test]
    fn hover_groups_late_local_binding_retro_poisons_outer_attribution() {
        // Inside `test`, `helper` is local for the whole function body
        // (Python scoping), so the call recorded against the module binding
        // before the local assignment was misattributed and is stripped;
        // the call after the assignment groups under the local binding, and
        // module-level sites elsewhere keep theirs.
        let groups = pending_hover_groups(
            "\
import helper
import other
helper.do(1)
def test():
    helper.do(1)
    other.do(1)
    helper = make()
    helper.do(1)
helper.do(1)
",
        );
        let [module_before, shadowed, other_kept, local_after, module_after] = groups.as_slice()
        else {
            panic!("expected five deferred calls, got {groups:?}");
        };
        assert!(module_before.is_some());
        assert_eq!(*shadowed, None);
        // An in-range entry for a *different* binding is untouched.
        assert!(other_kept.is_some());
        assert!(local_after.is_some());
        assert_ne!(module_before, local_after);
        assert_eq!(module_before, module_after);
    }

    #[test]
    fn hover_groups_class_body_bindings_do_not_shadow_methods() {
        // Python name lookup inside methods skips class scopes: the class
        // attribute `helper` shadows the module binding only inside the
        // class body itself, never in the method.
        let groups = pending_hover_groups(
            "\
import helper
class C:
    helper = q
    helper.do(1)
    def m(self):
        helper.do(1)
helper.do(1)
",
        );
        let [class_site, method_site, module_site] = groups.as_slice() else {
            panic!("expected three deferred calls, got {groups:?}");
        };
        // The class-body site is attributed to the class binding, whose
        // groups survive only while no rebinding poisons them.
        assert!(class_site.is_some());
        assert!(method_site.is_some());
        assert_ne!(class_site, method_site);
        assert_eq!(method_site, module_site);
    }

    #[test]
    fn hover_groups_cover_module_def_and_class_bindings() {
        let groups = pending_hover_groups(
            "\
def check(value):
    pass
class Thing:
    pass
def caller(x):
    check(x, 1)
    check(x, 1)
    Thing(x, 1)
",
        );
        let [first, second, class_call] = groups.as_slice() else {
            panic!("expected three deferred calls, got {groups:?}");
        };
        assert!(first.is_some());
        assert_eq!(first, second);
        assert!(class_call.is_some());
        assert_ne!(first, class_call);
    }

    #[test]
    fn hover_groups_poisoned_by_augmented_assignment() {
        let groups = pending_hover_groups(
            "\
import helper
helper.do(1)
helper += q
",
        );
        assert_eq!(groups, vec![None]);
    }

    #[test]
    fn hover_groups_poisoned_by_del() {
        let groups = pending_hover_groups(
            "\
import helper
helper.do(1)
del helper
",
        );
        assert_eq!(groups, vec![None]);
    }

    #[test]
    fn hover_groups_match_poisons_subject_names_and_capture_bindings() {
        // The subject (any name mentioned in it) is narrowed per arm, and
        // every capture form binds: sequence elements, `*rest`, mapping
        // values and `**rest`, class positional/keyword patterns, `as`
        // names, and or-pattern alternatives. `MatchValue`/`MatchSingleton`
        // patterns bind nothing.
        let groups = pending_hover_groups(
            "\
import subj, seq, star, mapping, rest, klass, kw, alias, ored
subj.do(1)
seq.do(1)
star.do(1)
mapping.do(1)
rest.do(1)
klass.do(1)
kw.do(1)
alias.do(1)
ored.do(1)
match subj.value:
    case 1:
        pass
    case None:
        pass
    case [seq, *star]:
        pass
    case {1: mapping, **rest}:
        pass
    case {2: mapping}:
        pass
    case [*_]:
        pass
    case C(klass, named=kw):
        pass
    case (x) as alias:
        pass
    case ored | 2:
        pass
",
        );
        let [subj, seq, star, mapping, rest, klass, kw, alias, ored] = groups.as_slice() else {
            panic!("expected nine deferred calls, got {groups:?}");
        };
        for (name, group) in [
            ("subj", subj),
            ("seq", seq),
            ("star", star),
            ("mapping", mapping),
            ("rest", rest),
            ("klass", klass),
            ("kw", kw),
            ("alias", alias),
            ("ored", ored),
        ] {
            assert_eq!(*group, None, "expected match to poison {name}");
        }
    }

    #[test]
    fn call_shape_fingerprint_tags_arguments_and_sorts_keywords() {
        fn shape_of(call_source: &str) -> String {
            let parsed = parse_module(call_source).expect("parse call");
            let [ruff_python_ast::Stmt::Expr(stmt_expr)] = parsed.suite().as_slice() else {
                panic!("expected a single expression statement");
            };
            let Expr::Call(call) = &*stmt_expr.value else {
                panic!("expected a call expression");
            };
            call_shape_fingerprint(&call.arguments)
        }

        assert_eq!(
            shape_of("f(x, x.y, 's', 1, True, None, g(), [1], (1,), {1: 2}, {1})"),
            "nas0bcCltde"
        );
        assert_eq!(
            shape_of("f(-x, x + y, *xs, x[0], x == y, lambda: 1)"),
            "up*i?L"
        );
        // `...` and f-strings reuse the constant/string tags; comprehensions
        // reuse their container tags; anything unrecognised is `x`.
        assert_eq!(
            shape_of("f(..., f'{x}', [i for i in x], {i: 1 for i in x}, {i for i in x})"),
            "cslde"
        );
        assert_eq!(shape_of("f(x if y else z, x and y)"), "x?");
        // Keyword names are sorted; `**` unpacking is its own marker.
        assert_eq!(shape_of("f(1, z=1, a=2, **kw)"), "0,**,a,z");
    }

    #[test]
    fn bound_import_name_covers_asname_and_dotted_paths() {
        let parsed = parse_module("import a.b.c\nimport a.b as x\nfrom m import y\n")
            .expect("parse imports");
        let [Stmt::Import(plain), Stmt::Import(aliased), Stmt::ImportFrom(from_import)] =
            parsed.suite().as_slice()
        else {
            panic!("expected three import statements");
        };
        assert_eq!(bound_import_name(&plain.names[0]), "a");
        assert_eq!(bound_import_name(&aliased.names[0]), "x");
        assert_eq!(bound_import_name(&from_import.names[0]), "y");
    }

    fn with_empty_checker(plan_fixes: bool, check: impl FnOnce(&mut CallChecker)) {
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let parsed = parse_module("").expect("parse empty");
        let mut checker = CallChecker::new(
            PathBuf::from("test.py"),
            "test".to_string(),
            false,
            "",
            parsed.tokens(),
            &index,
            &config,
            FixOptIns::default(),
            plan_fixes,
        );
        check(&mut checker);
    }

    #[test]
    fn class_from_annotation_covers_invalid_builtin_and_dotted_shapes() {
        with_empty_checker(false, |checker| {
            for annotation in [
                "",
                "A | B",
                "Any",
                "typing.Any",
                "object",
                "builtins.object",
                "Unknown",
            ] {
                assert_eq!(checker.class_from_annotation(annotation), None);
            }

            assert_eq!(
                checker.class_from_annotation("list[int]").as_deref(),
                Some("builtins.list")
            );
            assert_eq!(
                checker.class_from_annotation("builtins.str").as_deref(),
                Some("builtins.str")
            );

            checker.define("Alias", "pkg.Real".to_string());
            assert_eq!(
                checker.class_from_annotation("Alias.Inner").as_deref(),
                Some("pkg.Real.Inner")
            );

            checker.define_module("mod", "pkg.mod".to_string());
            assert_eq!(
                checker.class_from_annotation("mod.Type").as_deref(),
                Some("pkg.mod.Type")
            );
            assert_eq!(
                checker.class_from_annotation("external.Type").as_deref(),
                Some("external.Type")
            );

            checker.define("Local", "pkg.Local".to_string());
            assert_eq!(
                checker.class_from_annotation("'Local'").as_deref(),
                Some("pkg.Local")
            );
            assert_eq!(
                checker.class_from_annotation("Missing").as_deref(),
                Some("test.Missing")
            );
        });
    }

    #[test]
    fn opaque_receiver_fix_boundary_covers_call_shapes_and_annotations() {
        with_empty_checker(false, |checker| {
            with_call_func("f(1)\n", |func| {
                assert!(!checker.call_uses_opaque_receiver_boundary(func));
            });
            with_call_func("factory().m(1)\n", |func| {
                assert!(!checker.call_uses_opaque_receiver_boundary(func));
            });
            with_call_func("receiver.m(1)\n", |func| {
                assert!(!checker.call_uses_opaque_receiver_boundary(func));
            });

            checker.mark_param_opaque("receiver");
            with_call_func("receiver.m(1)\n", |func| {
                assert!(checker.call_uses_opaque_receiver_boundary(func));
            });

            checker
                .current_scope()
                .annotations
                .insert("receiver".to_string(), "list".to_string());
            with_call_func("receiver.m(1)\n", |func| {
                assert!(!checker.call_uses_opaque_receiver_boundary(func));
            });

            checker
                .current_scope()
                .annotations
                .insert("receiver".to_string(), "Renderer".to_string());
            with_call_func("receiver.m(1)\n", |func| {
                assert!(checker.call_uses_opaque_receiver_boundary(func));
            });
        });
    }

    #[test]
    fn constructor_result_call_resolves_dunder_call_without_ty_fallback() {
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.__call__".to_string(), sig(&["self", "a", "b"]));

        let source = r"
class C:
    def __call__(self, a, b): ...

C()(1, 2)
";

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);
        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn unresolved_call_expression_and_subscript_callees_do_not_flag() {
        let index = DefinitionIndex::for_test();

        let source = r"
def make():
    return 1

registry = {}
make()(1, 2)
registry['k'](1, 2)
";

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);
        assert_eq!(diagnostics, 0);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn if_branch_dispatcher_visits_every_traversal_mode() {
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let parsed = parse_module("pass\n").expect("parse pass");
        let stmt = parsed.suite().first().expect("statement");
        let mut checker = CallChecker::new(
            PathBuf::from("test.py"),
            "test".to_string(),
            false,
            "pass\n",
            parsed.tokens(),
            &index,
            &config,
            FixOptIns::default(),
            false,
        );

        checker.visit_if_branch_stmt(stmt, IfBranchTraversal::Module);
        checker.visit_if_branch_stmt(stmt, IfBranchTraversal::LocalBody);
        checker.visit_if_branch_stmt(stmt, IfBranchTraversal::ClassBody);
    }

    #[test]
    fn ty_pending_scan_without_retained_source_is_reported() {
        let mut diagnostics = Vec::new();
        let mut skip_warnings = Vec::new();
        let mut ty_work = Vec::new();
        let error = process_scan_outcome_for_ty(
            0,
            PathBuf::from("test.py"),
            ScanOutcome::Scanned(FileScan {
                source: None,
                diagnostics: Vec::new(),
                pending: vec![PendingTy {
                    callee_offset: 0,
                    call_start: 0,
                    positional_count: 1,
                    rewrite_args_are_statically_precise: true,
                }],
                pending_groups: vec![None],
                overload_fix_pending: Vec::new(),
                fixes: Vec::new(),
                fixed_calls: 0,
                declined_fix_reasons: Vec::new(),
            }),
            &mut diagnostics,
            &mut skip_warnings,
            &mut ty_work,
        )
        .expect_err("missing retained source should be reported");

        assert!(error
            .to_string()
            .contains("scan with ty pending did not retain source"));
        assert!(diagnostics.is_empty());
        assert!(skip_warnings.is_empty());
        assert!(ty_work.is_empty());
    }

    #[test]
    fn skipped_scan_outcome_is_recorded_without_ty_work() {
        let mut diagnostics = Vec::new();
        let mut skip_warnings = Vec::new();
        let mut ty_work = Vec::new();

        process_scan_outcome_for_ty(
            7,
            PathBuf::from("skipped.py"),
            ScanOutcome::Skipped("unsupported encoding".to_string()),
            &mut diagnostics,
            &mut skip_warnings,
            &mut ty_work,
        )
        .expect("skipped scan records a warning");

        assert!(diagnostics.is_empty());
        assert_eq!(
            skip_warnings,
            vec![(
                7,
                PathBuf::from("skipped.py"),
                "unsupported encoding".to_string()
            )]
        );
        assert!(ty_work.is_empty());
    }

    #[test]
    fn ty_scan_source_retention_is_only_required_for_pending_queries() {
        let mut diagnostics = Vec::new();
        let mut skip_warnings = Vec::new();
        let mut ty_work = Vec::new();

        process_scan_outcome_for_ty(
            0,
            PathBuf::from("empty.py"),
            ScanOutcome::Scanned(FileScan {
                source: None,
                diagnostics: Vec::new(),
                pending: Vec::new(),
                pending_groups: Vec::new(),
                overload_fix_pending: Vec::new(),
                fixes: Vec::new(),
                fixed_calls: 0,
                declined_fix_reasons: Vec::new(),
            }),
            &mut diagnostics,
            &mut skip_warnings,
            &mut ty_work,
        )
        .expect("empty pending scan does not need retained source");

        process_scan_outcome_for_ty(
            1,
            PathBuf::from("pending.py"),
            ScanOutcome::Scanned(FileScan {
                source: Some("f(1)\n".to_string()),
                diagnostics: Vec::new(),
                pending: vec![PendingTy {
                    callee_offset: 0,
                    call_start: 0,
                    positional_count: 1,
                    rewrite_args_are_statically_precise: true,
                }],
                pending_groups: vec![None],
                overload_fix_pending: Vec::new(),
                fixes: Vec::new(),
                fixed_calls: 0,
                declined_fix_reasons: Vec::new(),
            }),
            &mut diagnostics,
            &mut skip_warnings,
            &mut ty_work,
        )
        .expect("pending scan with retained source is valid");

        assert!(diagnostics.is_empty());
        assert!(skip_warnings.is_empty());
        assert_eq!(ty_work.len(), 1);
        assert_eq!(ty_work[0].path, PathBuf::from("pending.py"));
        assert_eq!(ty_work[0].source, "f(1)\n");
        assert_eq!(ty_work[0].pending.len(), 1);
    }

    #[test]
    fn imported_callable_tracking_is_only_maintained_for_fix_planning() {
        with_empty_checker(false, |checker| {
            checker.define_imported_name_and_module("f", "pkg.f".to_string());
            assert!(!checker.binding_is_imported_callable("f"));
            checker.mark_opaque_local("f");
            checker.clear_instance_binding("f");
        });
        with_empty_checker(true, |checker| {
            checker.define_imported_name_and_module("f", "pkg.f".to_string());
            assert!(checker.binding_is_imported_callable("f"));
            checker.mark_opaque_local("f");
            assert!(!checker.binding_is_imported_callable("f"));
        });
        with_empty_checker(true, |checker| {
            checker.define_imported_name_and_module("f", "pkg.f".to_string());
            assert!(checker.binding_is_imported_callable("f"));
            checker.clear_instance_binding("f");
            assert!(!checker.binding_is_imported_callable("f"));
        });
    }

    #[test]
    fn imported_callable_boundary_matches_only_imported_name_calls() {
        with_empty_checker(true, |checker| {
            checker.define_imported_name_and_module("f", "pkg.f".to_string());
            with_call_func("f(0)\n", |func| {
                assert!(checker.call_uses_imported_callable_boundary(func));
            });
            with_call_func("g(0)\n", |func| {
                assert!(!checker.call_uses_imported_callable_boundary(func));
            });
            with_call_func("obj.f(0)\n", |func| {
                assert!(!checker.call_uses_imported_callable_boundary(func));
            });
        });
    }

    #[test]
    fn method_self_binding_resolves_inherited_self_calls_without_ty_fallback() {
        let source = "\
class Base:
    def method(self, a: int) -> None: ...

class Child(Base):
    def check(self) -> None:
        self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.Base.method".to_string(), sig(&["self", "a"]));
        index.insert_class_bases("main.Child".to_string(), vec!["main.Base".to_string()]);

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn class_branch_method_self_binding_resolves_inherited_self_calls_without_ty_fallback() {
        let source = "\
class Base:
    def method(self, a: int) -> None: ...

class Child(Base):
    if True:
        def check(self) -> None:
            self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.Base.method".to_string(), sig(&["self", "a"]));
        index.insert_class_bases("main.Child".to_string(), vec!["main.Base".to_string()]);

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn nested_class_branch_method_self_binding_resolves_inherited_self_calls_without_ty_fallback() {
        let source = "\
class Base:
    def method(self, a: int) -> None: ...

class Child(Base):
    try:
        if True:
            def check(self) -> None:
                self.method(1)
    except Exception:
        pass
    except:
        pass
    else:
        pass
    finally:
        pass
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.Base.method".to_string(), sig(&["self", "a"]));
        index.insert_class_bases("main.Child".to_string(), vec!["main.Base".to_string()]);

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn class_branch_staticmethod_does_not_bind_literal_self_as_instance() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

    if True:
        @staticmethod
        def static(self) -> None:
            self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 0);
        assert_eq!(ty_pending, 1);
    }

    #[test]
    fn method_self_binding_preserves_self_annotation() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

    def check(self: 'C') -> None:
        self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn annotated_parameter_receiver_resolves_without_ty_fallback() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

def check(value: C) -> None:
    value.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));
        index.insert("main.C.__init__".to_string(), sig(&["self"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn annotated_assignment_receiver_resolves_without_ty_fallback() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

value: C
value.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));
        index.insert("main.C.__init__".to_string(), sig(&["self"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn method_self_binding_uses_nested_class_fullname() {
        let source = "\
class Outer:
    class Base:
        def method(self, a: int) -> None: ...

    class Child(Base):
        def check(self) -> None:
            self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.Outer.Base.method".to_string(), sig(&["self", "a"]));
        index.insert_class_bases(
            "main.Outer.Child".to_string(),
            vec!["main.Outer.Base".to_string()],
        );

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 1);
        assert_eq!(ty_pending, 0);
    }

    #[test]
    fn method_self_binding_requires_literal_self_name() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

    def check(this) -> None:
        this.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 0);
        assert_eq!(ty_pending, 1);
    }

    #[test]
    fn staticmethod_and_classmethod_do_not_bind_literal_self_as_instance() {
        let source = "\
class C:
    def method(self, a: int) -> None: ...

    @staticmethod
    def static(self) -> None:
        self.method(1)

    @classmethod
    def class_method(self) -> None:
        self.method(1)
";
        let mut index = DefinitionIndex::for_test();
        index.insert("main.C.method".to_string(), sig(&["self", "a"]));

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 0);
        assert_eq!(ty_pending, 2);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn is_bound_callable_attribute_alias(
        assignment_src: &str,
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
            FixOptIns::default(),
            false,
        );
        setup(&mut checker);

        let assignment = parse_module(assignment_src).expect("parse assignment");
        let Some(super::Stmt::Assign(stmt)) = assignment.suite().first() else {
            panic!("expected assignment");
        };
        checker.value_is_bound_callable_attribute_alias(&stmt.value)
    }

    #[test]
    fn callable_attribute_alias_distinguishes_modules_from_bound_receivers() {
        assert!(!is_bound_callable_attribute_alias(
            "alias = value\n",
            |_| {}
        ));
        assert!(!is_bound_callable_attribute_alias(
            "alias = mod.func\n",
            |c| c.define_module("mod", "mod".to_string())
        ));
        assert!(is_bound_callable_attribute_alias(
            "alias = self.lang.word_filter\n",
            |_| {}
        ));
        assert!(is_bound_callable_attribute_alias(
            "alias = factory().method\n",
            |_| {}
        ));
    }

    #[test]
    fn annotated_bound_attribute_alias_marks_target_opaque() {
        let source = "alias: object = self.lang.word_filter\n";
        let index = DefinitionIndex::for_test();

        let (diagnostics, ty_pending) = run_checker_with_index(source, &index);

        assert_eq!(diagnostics, 0);
        assert_eq!(ty_pending, 0);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn with_call_func(call_src: &str, check: impl FnOnce(&Expr)) {
        let parsed = parse_module(call_src).expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };
        check(&call.func);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn with_call(call_src: &str, check: impl FnOnce(&super::ast::ExprCall)) {
        let parsed = parse_module(call_src).expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };
        check(call);
    }

    #[test]
    fn pending_ty_tracks_static_rewrite_precision() {
        with_empty_checker(true, |checker| {
            with_call("f(0)\n", |call| {
                assert!(
                    checker
                        .pending_ty_for_call(call)
                        .expect("name call")
                        .rewrite_args_are_statically_precise
                );
            });
            with_call("obj.f(0)\n", |call| {
                assert!(
                    checker
                        .pending_ty_for_call(call)
                        .expect("attribute call")
                        .rewrite_args_are_statically_precise
                );
            });
            with_call("factory().f(0)\n", |call| {
                assert!(
                    checker
                        .pending_ty_for_call(call)
                        .expect("dynamic receiver")
                        .rewrite_args_are_statically_precise
                );
            });
        });
        with_empty_checker(true, |checker| {
            checker.mark_opaque_local("f");
            checker.mark_opaque_local("obj");
            with_call("f(0)\n", |call| {
                assert!(
                    !checker
                        .pending_ty_for_call(call)
                        .expect("opaque name")
                        .rewrite_args_are_statically_precise
                );
            });
            with_call("obj.f(0)\n", |call| {
                assert!(
                    !checker
                        .pending_ty_for_call(call)
                        .expect("opaque receiver")
                        .rewrite_args_are_statically_precise
                );
            });
        });
        with_empty_checker(true, |checker| {
            checker.define_imported_name_and_module("f", "pkg.f".to_string());
            checker.define_imported_name_and_module("obj", "pkg.obj".to_string());
            with_call("f(0)\n", |call| {
                assert!(
                    !checker
                        .pending_ty_for_call(call)
                        .expect("imported name")
                        .rewrite_args_are_statically_precise
                );
            });
            with_call("obj.f(0)\n", |call| {
                assert!(
                    !checker
                        .pending_ty_for_call(call)
                        .expect("imported receiver")
                        .rewrite_args_are_statically_precise
                );
            });
        });
    }

    #[test]
    fn static_precision_guards_cover_malformed_callee_names() {
        let mut index = DefinitionIndex::for_test();
        index.insert_class_bases("pkg.Child".to_string(), vec!["pkg.Base".to_string()]);
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
            FixOptIns::default(),
            true,
        );
        checker.define("Child", "pkg.Child".to_string());
        checker.define("Unrelated", "pkg.Unrelated".to_string());

        with_call_func("obj.method(0)\n", |func| {
            assert!(!checker
                .call_may_dispatch_to_override_with_different_parameter_names(func, "method"));
            assert!(!checker.constructor_call_uses_inherited_boundary(func, "pkg.Base.method"));
        });
        with_call_func("self.method(0)\n", |func| {
            assert!(!checker.self_call_uses_inherited_method_boundary(func, "method"));
        });
        with_call_func("factory(0)\n", |func| {
            assert!(!checker.constructor_call_uses_inherited_boundary(func, "pkg.Base.__init__"));
        });
        with_call_func("Child(0)\n", |func| {
            assert!(checker.constructor_call_uses_inherited_boundary(func, "pkg.Base.__init__"));
            assert!(checker.constructor_call_uses_inherited_boundary(func, "pkg.Base.__new__"));
            assert!(!checker.constructor_call_uses_inherited_boundary(func, "pkg.Child.__init__"));
        });
        with_call_func("Unrelated(0)\n", |func| {
            assert!(!checker.constructor_call_uses_inherited_boundary(func, "pkg.Base.__init__"));
        });
    }

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
            FixOptIns::default(),
            true,
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

    fn is_explicit_dunder(
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
            FixOptIns::default(),
            true,
        );
        setup(&mut checker);

        let call_parsed = parse_module(call_src).expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = call_parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };
        checker.is_explicit_dunder_receiver_call(&call.func, callee, first_param)
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
    fn explicit_dunder_guard_accepts_dotted_module_class_call() {
        // `mod.Class.__init__(self, 0)`: the built-in fixer must preserve the
        // explicit receiver while mapping later positional args to parameters.
        assert!(is_explicit_dunder(
            "mod.Class.__init__(self, 0)\n",
            "mod.Class.__init__",
            Some("self"),
            |c| {
                c.define_module("mod", "mod".to_string());
            }
        ));
    }

    #[test]
    fn explicit_dunder_guard_rejects_call_expression_base() {
        // `factory().__init__(self, 0)`: not a dotted module/class path, so the
        // receiver cannot be proven to be an explicit dunder receiver.
        assert!(!is_explicit_dunder(
            "factory().__init__(self, 0)\n",
            "pkg.K.__init__",
            Some("self"),
            |_| {}
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
            FixOptIns::default(),
            true,
        );
        assert!(!checker.binding_is_instance("never_bound"));
    }

    #[test]
    fn callable_fullname_rejects_unqualified_unknown_name() {
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
            FixOptIns::default(),
            true,
        );

        assert_eq!(checker.callable_fullname("plain"), None);
    }

    #[test]
    fn constructor_func_rejects_non_name_or_attribute_callee() {
        let index = DefinitionIndex::for_test();
        let config = Config::default();
        let checker_parsed = parse_module("").expect("parse empty");
        let checker = CallChecker::new(
            PathBuf::from("test.py"),
            "test".to_string(),
            false,
            "",
            checker_parsed.tokens(),
            &index,
            &config,
            FixOptIns::default(),
            true,
        );
        let call_parsed = parse_module("(lambda: object)()\n").expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = call_parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };

        assert_eq!(checker.class_from_constructor_func(&call.func), None);
    }

    #[test]
    fn call_at_start_callee_offset_rejects_non_identifier_callee() {
        let parsed = parse_module("(lambda x: x)(1)\n").expect("parse call");
        let Some(super::Stmt::Expr(stmt)) = parsed.suite().first() else {
            panic!("expected an expression statement");
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            panic!("expected a call expression");
        };
        assert_eq!(CallAtStart::callee_offset(call), None);
    }
}
