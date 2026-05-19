//! CLI for ``strict-kwargs``.

// `cargo llvm-cov` builds with `--cfg coverage`; under it both the inline
// `#[cfg(test)] mod tests` and `diff_color` (non-test) are marked
// `#[coverage(off)]`.  Because `coverage_attribute` is now used outside
// `#[cfg(test)]` the gate must be just `coverage` (not `all(coverage, test)`)
// so the feature is declared in both the test and the non-test binary
// coverage builds. `coverage` (not `coverage_nightly`) keeps local
// (stable + `RUSTC_BOOTSTRAP=1`) and CI (nightly) identical.
// See `lib.rs` for the library-crate rationale.
#![cfg_attr(coverage, feature(coverage_attribute))]

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args as ClapArgs, Parser, Subcommand};
use strict_kwargs::{
    check_paths, find_project_root, fix_paths_with_opt_ins, unified_diff, CheckError, Config,
    DeclinedFixReasonCount, Diagnostic, FileFix, FixOptIns, OutputFormat,
};

#[derive(Debug, Parser)]
#[command(
    name = "strict-kwargs",
    about = "Enforce using keyword arguments where possible (fast, independent of mypy/ty)",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    check: CheckArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Rewrite positional call arguments to keyword arguments in place.
    Fix(FixArgs),
}

#[derive(Debug, ClapArgs)]
struct CheckArgs {
    /// Paths to Python files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Project root containing ``pyproject.toml`` (auto-discovered by default).
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Python environment for the `ty` inference fallback: a Python
    /// interpreter, a virtualenv directory, or a ``sys.prefix`` directory
    /// (mirrors ``ty check --python``). Forwarded to ``ty server`` so it can
    /// resolve third-party imports against environments the built-in
    /// resolver does not discover (Conda, a venv outside the project,
    /// system site-packages). Unset: ty's own auto-discovery is unchanged.
    #[arg(long, value_name = "PATH")]
    python: Option<PathBuf>,

    /// Directory for the persistent on-disk diagnostic cache.  When set,
    /// resolved results are stored here and reused on future runs where
    /// the file and its environment are unchanged.  Omit to disable the
    /// cache (every run is cold, the previous behaviour).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Diagnostic output format for check results.
    #[arg(long, value_enum)]
    output_format: Option<OutputFormat>,
}

#[derive(Debug, ClapArgs)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap stores independent boolean flags directly"
)]
struct FixArgs {
    /// Paths to Python files or directories to fix.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Project root containing ``pyproject.toml`` (auto-discovered by default).
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Print the unified diff of what would change instead of writing it.
    #[arg(long)]
    diff: bool,

    /// Rewrite dataclass and `NamedTuple` constructor calls whose signatures
    /// were synthesized from class fields.
    #[arg(long)]
    fix_synthesized_constructors: bool,

    /// Python environment for the `ty` inference fallback (see
    /// ``strict-kwargs --help``). The rewrite stays conservative and never
    /// edits a `ty`-resolved call, but passing this lets ``fix`` *detect*
    /// the same violations ``check`` would, so the "not rewritten" count it
    /// reports — and a following ``strict-kwargs --python`` run — agree.
    #[arg(long, value_name = "PATH")]
    python: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("strict-kwargs: {error}");
            ExitCode::from(2)
        }
    }
}

/// Resolve the `--python` value before it reaches the `ty` fallback.
///
/// An invalid (nonexistent) `--python` used to be forwarded to `ty`
/// verbatim and silently ignored there, so the explicit environment was
/// disabled with no signal — detection silently degraded (issue #55). Now a
/// nonexistent path is reported on stderr and dropped, so the run falls
/// back to `ty`'s own environment discovery (the same as if `--python`
/// were unset) rather than silently degrading detection.
fn resolve_python_env(python: Option<PathBuf>) -> Option<PathBuf> {
    let path = python?;
    if path.exists() {
        return Some(path);
    }
    eprintln!(
        "strict-kwargs: --python {} does not exist; ignoring it and falling \
         back to ty's own environment discovery",
        path.display()
    );
    None
}

fn project_root_for(explicit: Option<PathBuf>, paths: &[PathBuf]) -> PathBuf {
    explicit.unwrap_or_else(|| {
        let start = paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        find_project_root(&start)
    })
}

fn run() -> Result<ExitCode, CheckError> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Fix(args)) => run_fix(args),
        None => run_check(cli.check),
    }
}

fn run_check(args: CheckArgs) -> Result<ExitCode, CheckError> {
    let project_root = project_root_for(args.project_root, &args.paths);
    let config = Config::load(&project_root)?;
    let output_format = args.output_format.unwrap_or(config.output_format);
    let python_env = resolve_python_env(args.python);
    let diagnostics = check_paths(
        &project_root,
        &args.paths,
        &config,
        python_env.as_deref(),
        args.cache_dir.as_deref(),
    )?;
    report_check_diagnostics(&diagnostics, output_format)?;
    if diagnostics.is_empty() {
        Ok(ExitCode::from(0))
    } else {
        Ok(ExitCode::from(1))
    }
}

#[derive(serde::Serialize)]
struct JsonDiagnostic<'a> {
    path: String,
    line: usize,
    column: usize,
    callee: &'a str,
    positional_count: usize,
    max_positional_count: usize,
}

impl<'a> From<&'a Diagnostic> for JsonDiagnostic<'a> {
    fn from(diagnostic: &'a Diagnostic) -> Self {
        Self {
            path: diagnostic.path.display().to_string(),
            line: diagnostic.line,
            column: diagnostic.column,
            callee: &diagnostic.callee,
            positional_count: diagnostic.positional_count,
            max_positional_count: diagnostic.max_positional,
        }
    }
}

fn report_check_diagnostics(
    diagnostics: &[Diagnostic],
    output_format: OutputFormat,
) -> Result<(), CheckError> {
    match output_format {
        OutputFormat::Full => {
            for diagnostic in diagnostics {
                eprintln!("{}", diagnostic.display_path());
            }
        }
        OutputFormat::Json => {
            let diagnostics = diagnostics
                .iter()
                .map(JsonDiagnostic::from)
                .collect::<Vec<_>>();
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            serde_json::to_writer_pretty(&mut stdout, &diagnostics)
                .map_err(|error| CheckError::Io(std::io::Error::other(error)))?;
            writeln!(stdout)?;
        }
        OutputFormat::Github => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            for diagnostic in diagnostics {
                writeln!(stdout, "{}", diagnostic.github_annotation())?;
            }
        }
    }
    Ok(())
}

/// Report violations `fix` detected but deliberately did not rewrite, so a
/// following `strict-kwargs` run is no surprise (issue #42). Always to stderr
/// — stdout is reserved for the `--diff` patch.
fn report_declined(declined_reasons: &[DeclinedFixReasonCount]) {
    let declined = declined_reasons
        .iter()
        .map(|item| item.count)
        .sum::<usize>();
    if declined == 0 {
        return;
    }
    eprintln!(
        "strict-kwargs: {declined} violation{} detected but not rewritten; \
         run `strict-kwargs` to see {}",
        if declined == 1 { "" } else { "s" },
        if declined == 1 { "it" } else { "them" }
    );
    for item in declined_reasons {
        eprintln!(
            "strict-kwargs: declined {}: {}",
            item.reason.label(),
            item.count
        );
    }
}

/// Return `true` when diff output should be colorized.
///
/// Colors are enabled only for an interactive terminal that has not opted out
/// via the `NO_COLOR` convention (<https://no-color.org/>).
#[cfg_attr(coverage, coverage(off))]
fn diff_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

const fn fix_opt_ins_from_args(args: &FixArgs) -> FixOptIns {
    FixOptIns {
        synthesized_constructors: args.fix_synthesized_constructors,
    }
}

fn report_enabled_fix_opt_ins(opt_ins: FixOptIns) {
    if opt_ins.synthesized_constructors {
        eprintln!(
            "strict-kwargs: fix opt-in enabled: synthesized constructors may change runtime behavior"
        );
    }
}

fn report_diff_summary(fixes: &[FileFix]) {
    let total = fixes.iter().map(|fix| fix.count).sum::<usize>();
    eprintln!(
        "strict-kwargs: would fix {total} call{} in {} file{}",
        if total == 1 { "" } else { "s" },
        fixes.len(),
        if fixes.len() == 1 { "" } else { "s" }
    );
}

fn run_fix(args: FixArgs) -> Result<ExitCode, CheckError> {
    let args_fix_opt_ins = fix_opt_ins_from_args(&args);
    let project_root = project_root_for(args.project_root, &args.paths);
    let config = Config::load(&project_root)?;
    let fix_opt_ins = FixOptIns {
        synthesized_constructors: config.fix_synthesized_constructors
            || args_fix_opt_ins.synthesized_constructors,
    };
    let python_env = resolve_python_env(args.python);
    report_enabled_fix_opt_ins(fix_opt_ins);
    let outcome = fix_paths_with_opt_ins(
        &project_root,
        &args.paths,
        &config,
        python_env.as_deref(),
        fix_opt_ins,
    )?;
    let fixes = &outcome.files;
    if fixes.is_empty() {
        eprintln!("strict-kwargs: no fixes to apply");
        report_declined(&outcome.declined_reasons);
        return Ok(ExitCode::from(0));
    }

    if args.diff {
        let color = diff_color();
        for fix in fixes {
            print!(
                "{}",
                unified_diff(&fix.path, &fix.original, &fix.fixed, color)
            );
        }
        report_diff_summary(fixes);
        report_declined(&outcome.declined_reasons);
        return Ok(ExitCode::from(0));
    }

    let mut total = 0usize;
    for fix in fixes {
        std::fs::write(&fix.path, &fix.fixed)?;
        total += fix.count;
        eprintln!(
            "strict-kwargs: fixed {} call{} in {}",
            fix.count,
            if fix.count == 1 { "" } else { "s" },
            fix.path.display()
        );
    }
    eprintln!(
        "strict-kwargs: fixed {total} call{} in {} file{}",
        if total == 1 { "" } else { "s" },
        fixes.len(),
        if fixes.len() == 1 { "" } else { "s" }
    );
    report_declined(&outcome.declined_reasons);
    Ok(ExitCode::from(0))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn project_root_uses_explicit_when_given() {
        let explicit = PathBuf::from("/some/explicit/root");
        assert_eq!(
            project_root_for(Some(explicit.clone()), &[PathBuf::from("x.py")]),
            explicit
        );
    }

    #[test]
    fn project_root_discovers_from_first_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\n").expect("write");
        let nested = dir.path().join("pkg");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let file = nested.join("m.py");
        std::fs::write(&file, "").expect("write");
        assert_eq!(project_root_for(None, &[file]), dir.path());
    }

    #[test]
    fn project_root_falls_back_to_dot_when_no_paths() {
        // `paths.first()` is `None` (unreachable from the CLI because clap
        // defaults `paths` to `.`, but covered here for completeness).
        let root = project_root_for(None, &[]);
        assert_eq!(root, find_project_root(&PathBuf::from(".")));
    }

    #[test]
    fn python_env_unset_stays_unset() {
        assert_eq!(resolve_python_env(None), None);
    }

    #[test]
    fn python_env_existing_path_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        assert_eq!(resolve_python_env(Some(path.clone())), Some(path));
    }

    #[test]
    fn python_env_nonexistent_path_is_dropped() {
        // Nonexistent `--python`: dropped (so the run falls back to ty's own
        // discovery) rather than silently forwarded and ignored (issue #55).
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no_such_python");
        assert_eq!(resolve_python_env(Some(missing)), None);
    }
}
