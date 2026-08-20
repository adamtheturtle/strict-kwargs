use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::config::{Config, SourceRoots};
use crate::error::CheckError;
use crate::fix::{declined_fix_reason_counts, FileFix, FixOptIns, FixOutcome};
use crate::index::build_index_with_sources;
use crate::ty_resolver::TyResolver;

use super::{
    collect_python_files_for_fix, explicit_python_files, plan_rewrite_insertions,
    require_ty_present, resolve_file_with_ty, resolve_overload_fixes_with_ty, run_with_large_stack,
    scan_files_for_fix, ScanOutcome, TyDefCaches, TyFixes, TyShardAssigner, TY_SHARD_COUNT,
};

/// Minimum deferred-call count that justifies starting multiple ty servers.
/// Below this, server initialization costs more than parallel query handling;
/// this keeps Sphinx on one server while `CPython` uses the sharded path.
const PARALLEL_FIX_TY_THRESHOLD: usize = 20_000;

const fn should_parallelize_fix_ty(deferred_calls: usize) -> bool {
    deferred_calls >= PARALLEL_FIX_TY_THRESHOLD
}

#[derive(Default)]
struct FixTyState {
    ty: Option<TyResolver>,
    start_attempted: bool,
    file_cache: FxHashMap<PathBuf, Option<String>>,
    def_caches: TyDefCaches,
}

struct ResolvedFixFile {
    order: usize,
    warning: Option<(PathBuf, String)>,
    file: Option<FileFix>,
    diagnostics: usize,
    fixed: usize,
    declined_reasons: Vec<crate::fix::DeclinedFixReason>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one independently resolved fix file shares immutable project state"
)]
#[cfg_attr(coverage, coverage(off))]
fn resolve_fix_scan(
    order: usize,
    project_root: &Path,
    python_files: &[PathBuf],
    index: &crate::index::DefinitionIndex,
    indexed_files: &FxHashMap<PathBuf, crate::index::IndexedFile>,
    python_env: Option<&Path>,
    config: &Config,
    path: PathBuf,
    outcome: ScanOutcome,
    ty_state: &mut FixTyState,
) -> Result<ResolvedFixFile, CheckError> {
    let scan = match outcome {
        ScanOutcome::Skipped(reason) => {
            return Ok(ResolvedFixFile {
                order,
                warning: Some((path, reason)),
                file: None,
                diagnostics: 0,
                fixed: 0,
                declined_reasons: Vec::new(),
            });
        }
        ScanOutcome::Scanned(scan) => scan,
    };
    let Some(source) = scan.source else {
        return Err(CheckError::Io(std::io::Error::other(
            "internal error: fix scan did not retain source",
        )));
    };
    let mut diagnostics = scan.diagnostics;
    let mut declined_reasons = scan.declined_fix_reasons;
    let mut insertions = scan.fixes;
    let mut fixed = scan.fixed_calls;
    resolve_file_with_ty(
        &mut ty_state.ty,
        &mut ty_state.start_attempted,
        project_root,
        python_files,
        index,
        indexed_files,
        python_env,
        &path,
        &source,
        &scan.pending,
        &scan.pending_groups,
        config,
        &mut ty_state.file_cache,
        &mut ty_state.def_caches,
        &mut diagnostics,
        Some(TyFixes {
            insertions: &mut insertions,
            fixed_calls: &mut fixed,
            declined_fix_reasons: &mut declined_reasons,
        }),
    )?;
    resolve_overload_fixes_with_ty(
        &mut ty_state.ty,
        &mut ty_state.start_attempted,
        project_root,
        python_files,
        index,
        python_env,
        &path,
        &source,
        &scan.overload_fix_pending,
        Some(TyFixes {
            insertions: &mut insertions,
            fixed_calls: &mut fixed,
            declined_fix_reasons: &mut declined_reasons,
        }),
    );
    let file = plan_rewrite_insertions(&path, &source, &insertions)?.map(|fixed_source| FileFix {
        path,
        original: source,
        fixed: fixed_source,
        count: fixed,
    });
    debug_assert_eq!(
        declined_reasons.len(),
        diagnostics.len().saturating_sub(fixed)
    );
    Ok(ResolvedFixFile {
        order,
        warning: None,
        file,
        diagnostics: diagnostics.len(),
        fixed,
        declined_reasons,
    })
}

/// Rewrite positional call arguments to keyword arguments for every fixable
/// violation reachable from `paths`.
///
/// Mirrors [`super::check_paths`]: it runs the same detection -- built-in
/// resolver *and*, for the calls that misses, the (required) `ty` fallback
/// steered by `python_env` (the `--python` value). The *rewrite*, by design
/// (issue #7), stays conservative: a call is rewritten only when the parameter
/// mapping is unambiguous. By default, that means ordinary built-in,
/// single-signature mappings only. [`fix_paths_with_opt_ins`] can also include
/// synthesized constructors, `ty`-resolved calls, and overloads where `ty`
/// selects one precise arm.
/// Ambiguous callable displays and most goto-definition-only resolutions are
/// left alone (a wrong parameter name would corrupt source, cf. issue #41); a
/// single resolved `__call__` signature may still be fixed because it maps
/// directly to the callable value being invoked.
///
/// Running the `ty` fallback here also lets the returned
/// [`FixOutcome::declined`] account for *every* violation `check` would report,
/// so `fix` then `check` (with the same `--python`) is predictable rather than
/// silently inconsistent (issue #42). The fallback still starts lazily -- only
/// when the built-in resolver leaves a file with unresolved calls -- so the
/// all-first-party common case pays nothing.
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
    fix_paths_with_opt_ins(
        project_root,
        paths,
        config,
        python_env,
        FixOptIns::default(),
    )
}

/// Like [`fix_paths`], but includes the requested non-default fix categories.
///
/// # Errors
///
/// Returns the same errors as [`fix_paths`].
pub fn fix_paths_with_opt_ins(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
    fix_opt_ins: FixOptIns,
) -> Result<FixOutcome, CheckError> {
    let fix_opt_ins = FixOptIns {
        synthesized_constructors: config.fix_synthesized_constructors
            || fix_opt_ins.synthesized_constructors,
    };
    run_with_large_stack(move || {
        fix_paths_impl(project_root, paths, config, python_env, fix_opt_ins)
    })
}

// Fix orchestration is covered end-to-end by CLI/fix tests. Keep it out of the
// coverage gate because the remaining uncovered arm is the fail-safe
// propagation from `plan_rewrite_insertions`: parser-derived insertions should
// not be able to construct that invalid rewrite, and the validator is
// unit-tested directly.
#[cfg_attr(coverage, coverage(off))]
fn fix_paths_impl(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
    python_env: Option<&Path>,
    fix_opt_ins: FixOptIns,
) -> Result<FixOutcome, CheckError> {
    // `ty` is a hard requirement; verify it up front (see `check_paths`).
    require_ty_present()?;
    let python_files = collect_python_files_for_fix(project_root, paths, config)?;
    let explicit_files = explicit_python_files(paths, &python_files);
    let source_roots = SourceRoots::from_config(project_root, config);
    let (index, indexed_files) =
        build_index_with_sources(project_root, &python_files, &source_roots, python_env);

    // Phase 1 (parallel, see `check_paths`): run the built-in pass for each
    // file. Rewrites are planned serially below after the ty fallback has a
    // chance to add safe single-signature hover fixes.
    let scans = scan_files_for_fix(
        &python_files,
        &explicit_files,
        &source_roots,
        config,
        &index,
        &indexed_files,
        fix_opt_ins,
    )?;

    let deferred_calls = scans
        .iter()
        .map(|(_, outcome)| match outcome {
            ScanOutcome::Scanned(scan) => scan.pending.len() + scan.overload_fix_pending.len(),
            ScanOutcome::Skipped(_) => 0,
        })
        .sum::<usize>();

    let mut resolved_files = if should_parallelize_fix_ty(deferred_calls) {
        // Match the checker's deterministic, load-balanced ty ownership. Each
        // server sees only its files, while final results are restored to the
        // sorted input order before warnings and rewrites are emitted.
        let mut partitions: Vec<Vec<(usize, PathBuf, ScanOutcome)>> =
            (0..TY_SHARD_COUNT).map(|_| Vec::new()).collect();
        let mut assigner = TyShardAssigner::new(TY_SHARD_COUNT);
        for (order, (path, outcome)) in scans.into_iter().enumerate() {
            let weight = match &outcome {
                ScanOutcome::Scanned(scan) => scan.pending.len() + scan.overload_fix_pending.len(),
                ScanOutcome::Skipped(_) => 0,
            };
            let owner = assigner.assign(weight);
            partitions[owner].push((order, path, outcome));
        }

        std::thread::scope(|scope| -> Result<Vec<ResolvedFixFile>, CheckError> {
            let mut handles = Vec::with_capacity(TY_SHARD_COUNT);
            for partition in partitions {
                let handle = std::thread::Builder::new()
                    .stack_size(crate::limits::STACK_SIZE)
                    .spawn_scoped(scope, || -> Result<Vec<ResolvedFixFile>, CheckError> {
                        let mut ty_state = FixTyState::default();
                        partition
                            .into_iter()
                            .map(|(order, path, outcome)| {
                                resolve_fix_scan(
                                    order,
                                    project_root,
                                    &python_files,
                                    &index,
                                    &indexed_files,
                                    python_env,
                                    config,
                                    path,
                                    outcome,
                                    &mut ty_state,
                                )
                            })
                            .collect()
                    })
                    .map_err(CheckError::Io)?;
                handles.push(handle);
            }
            let mut resolved = Vec::with_capacity(python_files.len());
            for handle in handles {
                let shard = match handle.join() {
                    Ok(result) => result,
                    Err(payload) => std::panic::resume_unwind(payload),
                }?;
                resolved.extend(shard);
            }
            Ok(resolved)
        })?
    } else {
        // Starting several language servers costs more than it saves for
        // small projects, so retain the original one-server path there.
        let mut ty_state = FixTyState::default();
        scans
            .into_iter()
            .enumerate()
            .map(|(order, (path, outcome))| {
                resolve_fix_scan(
                    order,
                    project_root,
                    &python_files,
                    &index,
                    &indexed_files,
                    python_env,
                    config,
                    path,
                    outcome,
                    &mut ty_state,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    resolved_files.sort_by_key(|resolved| resolved.order);

    // Every violation the checker would report, across all files (built-in
    // and ty-resolved). Used for the declined count; ty may also append safe
    // hover-derived insertions to the built-in rewrite plan.
    let mut diagnostics = 0usize;
    let mut declined_fix_reasons = Vec::new();
    let mut fixed_total = 0usize;
    let mut results = Vec::new();
    for resolved in resolved_files {
        if let Some((path, reason)) = resolved.warning {
            eprintln!(
                "strict-kwargs: warning: skipping {} ({reason})",
                path.display()
            );
        }
        diagnostics += resolved.diagnostics;
        declined_fix_reasons.extend(resolved.declined_reasons);
        fixed_total += resolved.fixed;
        if let Some(file) = resolved.file {
            results.push(file);
        }
    }
    results.sort_by_key(|fix| fix.path.clone());
    // Each violation pushes exactly one diagnostic, then is rewritten or not;
    // the ty fallback only ever adds diagnostics. So the un-rewritten count
    // is the total detected minus the total rewritten. `saturating_sub` is
    // defensive -- `fixed_total` can never exceed the diagnostic count.
    let declined = declined_fix_reasons.len();
    debug_assert_eq!(declined, diagnostics.saturating_sub(fixed_total));
    let declined_reasons = declined_fix_reason_counts(&declined_fix_reasons);
    Ok(FixOutcome {
        files: results,
        declined,
        declined_reasons,
    })
}

#[cfg(test)]
mod tests {
    use super::{should_parallelize_fix_ty, PARALLEL_FIX_TY_THRESHOLD};

    #[test]
    fn parallel_fix_ty_threshold_is_inclusive() {
        assert!(!should_parallelize_fix_ty(PARALLEL_FIX_TY_THRESHOLD - 1));
        assert!(should_parallelize_fix_ty(PARALLEL_FIX_TY_THRESHOLD));
    }
}
