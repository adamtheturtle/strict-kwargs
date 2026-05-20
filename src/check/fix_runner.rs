use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::config::{Config, SourceRoots};
use crate::error::CheckError;
use crate::fix::{declined_fix_reason_counts, FileFix, FixOptIns, FixOutcome};
use crate::index::build_index;
use crate::ty_resolver::TyResolver;

use super::{
    collect_python_files, explicit_python_files, plan_rewrite_insertions, require_ty_present,
    resolve_file_with_ty, resolve_overload_fixes_with_ty, run_with_large_stack, scan_files_for_fix,
    ScanOutcome, TyFixes,
};

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
    let python_files = collect_python_files(project_root, paths, config)?;
    let explicit_files = explicit_python_files(paths);
    let source_roots = SourceRoots::from_config(project_root, config);
    let index = build_index(project_root, &python_files, &source_roots);

    // Phase 1 (parallel, see `check_paths`): run the built-in pass for each
    // file. Rewrites are planned serially below after the ty fallback has a
    // chance to add safe single-signature hover fixes.
    let scans = scan_files_for_fix(
        &python_files,
        &explicit_files,
        &source_roots,
        config,
        &index,
        fix_opt_ins,
    )?;

    let mut ty: Option<TyResolver> = None;
    let mut ty_start_attempted = false;
    let mut ty_file_cache: FxHashMap<PathBuf, Option<String>> = FxHashMap::default();
    // Every violation the checker would report, across all files (built-in
    // and ty-resolved). Used for the declined count; ty may also append safe
    // hover-derived insertions to the built-in rewrite plan.
    let mut diagnostics = Vec::new();
    let mut declined_fix_reasons = Vec::new();
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
        declined_fix_reasons.extend(scan.declined_fix_reasons);
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
            &index,
            python_env,
            &path,
            &scan.source,
            &scan.pending,
            config,
            &mut ty_file_cache,
            &mut diagnostics,
            Some(TyFixes {
                insertions: &mut insertions,
                fixed_calls: &mut fixed_calls,
                declined_fix_reasons: &mut declined_fix_reasons,
            }),
        )?;
        resolve_overload_fixes_with_ty(
            &mut ty,
            &mut ty_start_attempted,
            project_root,
            &index,
            python_env,
            &path,
            &scan.source,
            &scan.overload_fix_pending,
            Some(TyFixes {
                insertions: &mut insertions,
                fixed_calls: &mut fixed_calls,
                declined_fix_reasons: &mut declined_fix_reasons,
            }),
        );
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
    // defensive -- `fixed_total` can never exceed the diagnostic count.
    let declined = declined_fix_reasons.len();
    debug_assert_eq!(declined, diagnostics.len().saturating_sub(fixed_total));
    let declined_reasons = declined_fix_reason_counts(&declined_fix_reasons);
    Ok(FixOutcome {
        files: results,
        declined,
        declined_reasons,
    })
}
