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

use std::io::{BufWriter, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args as ClapArgs, Parser, Subcommand};
use owo_colors::OwoColorize as _;
use strict_kwargs::{
    check_paths, find_project_root, fix_paths_with_opt_ins, unified_diff, CheckError, Config,
    Diagnostic, FileFix, FixOptIns, OutputFormat,
};

const CACHE_DIR_ENV_VAR: &str = "STRICT_KWARGS_CACHE_DIR";

#[derive(Debug, Parser)]
#[command(
    name = "strict-kwargs",
    version,
    about = "Enforce using keyword arguments where possible (fast, independent of mypy/ty)",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run strict-kwargs over the given files or directories.
    Check(CheckArgs),
}

#[derive(Debug, ClapArgs)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "clap stores independent boolean flags directly"
)]
struct CheckArgs {
    /// List of files or directories to check.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Project root containing ``pyproject.toml`` (auto-discovered by default).
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Apply fixes to resolve violations.
    #[arg(long)]
    fix: bool,

    /// Preview fixes as a unified diff instead of writing files.
    #[arg(long)]
    diff: bool,

    /// Include fixes that may change runtime behavior.
    #[arg(long)]
    unsafe_fixes: bool,

    /// Diagnostic output format.
    #[arg(long, value_enum)]
    output_format: Option<OutputFormat>,

    /// Directory for the persistent on-disk diagnostic cache.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Python environment for the `ty` inference fallback.
    #[arg(long, value_name = "PATH")]
    python: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error}");
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
        "warning: --python {} does not exist; ignoring it and falling back to \
         ty's own environment discovery",
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

fn resolve_configured_cache_dir(project_root: &std::path::Path, cache_dir: &PathBuf) -> PathBuf {
    if cache_dir.is_absolute() {
        cache_dir.clone()
    } else {
        project_root.join(cache_dir)
    }
}

fn effective_cache_dir(
    cli_cache_dir: Option<PathBuf>,
    config: &Config,
    project_root: &std::path::Path,
) -> Option<PathBuf> {
    cli_cache_dir
        .or_else(|| {
            config
                .cache_dir
                .as_ref()
                .map(|dir| resolve_configured_cache_dir(project_root, dir))
        })
        .or_else(|| std::env::var_os(CACHE_DIR_ENV_VAR).map(PathBuf::from))
}

fn run() -> Result<ExitCode, CheckError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Check(args) => run_check(args),
    }
}

fn run_check(args: CheckArgs) -> Result<ExitCode, CheckError> {
    if args.fix || args.diff {
        return run_check_fix(args);
    }
    let project_root = project_root_for(args.project_root, &args.paths);
    let config = Config::load(&project_root)?;
    let output_format = args.output_format.unwrap_or(config.output_format);
    let python_env = resolve_python_env(args.python);
    let cache_dir = effective_cache_dir(args.cache_dir, &config, &project_root);
    let diagnostics = check_paths(
        &project_root,
        &args.paths,
        &config,
        python_env.as_deref(),
        cache_dir.as_deref(),
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
    code: &'static str,
    filename: String,
    location: JsonLocation,
    message: String,
    callee: &'a str,
}

#[derive(serde::Serialize)]
struct JsonLocation {
    row: usize,
    column: usize,
}

impl<'a> From<&'a Diagnostic> for JsonDiagnostic<'a> {
    fn from(diagnostic: &'a Diagnostic) -> Self {
        Self {
            code: Diagnostic::CODE,
            filename: diagnostic.path.display().to_string(),
            location: JsonLocation {
                row: diagnostic.line,
                column: diagnostic.column,
            },
            message: diagnostic.message(),
            callee: &diagnostic.callee,
        }
    }
}

fn report_check_diagnostics(
    diagnostics: &[Diagnostic],
    output_format: OutputFormat,
) -> Result<(), CheckError> {
    match output_format {
        OutputFormat::Full => {
            let color = stdout_color();
            let stdout = std::io::stdout();
            let mut stdout = BufWriter::new(stdout.lock());
            for diagnostic in diagnostics {
                writeln!(stdout, "{}", display_diagnostic(diagnostic, color))?;
            }
            if diagnostics.is_empty() {
                writeln!(stdout, "{}", success_message(color))?;
            } else {
                writeln!(stdout, "{}", found_summary(diagnostics.len(), color))?;
            }
            stdout.flush()?;
        }
        OutputFormat::Json => {
            let diagnostics = diagnostics
                .iter()
                .map(JsonDiagnostic::from)
                .collect::<Vec<_>>();
            let json = json_diagnostics(&diagnostics);
            println!("{json}");
        }
        OutputFormat::Github => {
            for diagnostic in diagnostics {
                println!("{}", diagnostic.github_annotation());
            }
        }
    }
    Ok(())
}

#[cfg_attr(coverage, coverage(off))]
#[allow(
    clippy::expect_used,
    reason = "serializing this fixed struct shape to a JSON string cannot fail"
)]
fn json_diagnostics(diagnostics: &[JsonDiagnostic<'_>]) -> String {
    serde_json::to_string_pretty(diagnostics)
        .expect("serializing strict-kwargs diagnostics to JSON should be infallible")
}

#[cfg_attr(coverage, coverage(off))]
fn stdout_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

#[cfg_attr(coverage, coverage(off))]
fn stderr_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn display_diagnostic(diagnostic: &Diagnostic, color: bool) -> String {
    if !color {
        return diagnostic.display_path();
    }
    let location = format!(
        "{}:{}:{}",
        diagnostic.path.display(),
        diagnostic.line,
        diagnostic.column
    );
    format!(
        "{}: {} {}",
        location.bold(),
        Diagnostic::CODE.red().bold(),
        diagnostic.message()
    )
}

fn success_message(color: bool) -> String {
    if color {
        format!("{}", "All checks passed!".green())
    } else {
        "All checks passed!".to_owned()
    }
}

fn found_summary(count: usize, color: bool) -> String {
    let summary = format!("Found {count} error{}.", if count == 1 { "" } else { "s" });
    styled_summary(summary, color, SummaryStyle::Error)
}

fn styled_summary(summary: String, color: bool, style: SummaryStyle) -> String {
    if !color {
        return summary;
    }
    match style {
        SummaryStyle::Success => format!("{}", summary.green().bold()),
        SummaryStyle::Warning => format!("{}", summary.yellow().bold()),
        SummaryStyle::Error => format!("{}", summary.red().bold()),
    }
}

#[derive(Clone, Copy)]
enum SummaryStyle {
    Success,
    Warning,
    Error,
}

/// Return `true` when diff output should be colorized.
///
/// Colors are enabled only for an interactive terminal that has not opted out
/// via the `NO_COLOR` convention (<https://no-color.org/>).
#[cfg_attr(coverage, coverage(off))]
fn diff_color() -> bool {
    stdout_color()
}

const fn fix_opt_ins_from_args(args: &CheckArgs) -> FixOptIns {
    FixOptIns {
        synthesized_constructors: args.unsafe_fixes,
    }
}

fn fix_total(fixes: &[FileFix]) -> usize {
    fixes.iter().map(|fix| fix.count).sum::<usize>()
}

fn report_diff_summary(fixes: &[FileFix], remaining: usize) {
    let color = stderr_color();
    for line in diff_summary_lines(fixes, remaining, color) {
        eprintln!("{line}");
    }
}

fn diff_summary_lines(fixes: &[FileFix], remaining: usize, color: bool) -> Vec<String> {
    let total = fix_total(fixes);
    if total == 0 && remaining == 0 {
        return vec![success_message(color)];
    }
    let mut lines = Vec::new();
    if total > 0 {
        let summary = format!(
            "Would fix {total} error{}.",
            if total == 1 { "" } else { "s" }
        );
        lines.push(styled_summary(summary, color, SummaryStyle::Warning));
    }
    if remaining > 0 {
        let summary = format!(
            "{remaining} error{} would remain.",
            if remaining == 1 { "" } else { "s" }
        );
        lines.push(styled_summary(summary, color, SummaryStyle::Error));
    }
    lines
}

fn report_fix_summary(fixed: usize, remaining: usize) -> Result<(), CheckError> {
    let color = stdout_color();
    let stdout = std::io::stdout();
    let mut stdout = BufWriter::new(stdout.lock());
    writeln!(stdout, "{}", fix_summary(fixed, remaining, color))?;
    stdout.flush()?;
    Ok(())
}

fn fix_summary(fixed: usize, remaining: usize, color: bool) -> String {
    let found = fixed + remaining;
    if found == 0 {
        success_message(color)
    } else {
        let summary = format!(
            "Found {found} error{} ({fixed} fixed, {remaining} remaining).",
            if found == 1 { "" } else { "s" },
        );
        if remaining == 0 {
            styled_summary(summary, color, SummaryStyle::Success)
        } else {
            styled_summary(summary, color, SummaryStyle::Error)
        }
    }
}

fn fix_exit_code(remaining: usize) -> ExitCode {
    if remaining == 0 {
        ExitCode::from(0)
    } else {
        ExitCode::from(1)
    }
}

fn run_check_fix(args: CheckArgs) -> Result<ExitCode, CheckError> {
    let args_fix_opt_ins = fix_opt_ins_from_args(&args);
    let project_root = project_root_for(args.project_root, &args.paths);
    let config = Config::load(&project_root)?;
    let fix_opt_ins = FixOptIns {
        synthesized_constructors: config.fix_synthesized_constructors
            || args_fix_opt_ins.synthesized_constructors,
    };
    let python_env = resolve_python_env(args.python);
    let outcome = fix_paths_with_opt_ins(
        &project_root,
        &args.paths,
        &config,
        python_env.as_deref(),
        fix_opt_ins,
    )?;
    let fixes = &outcome.files;
    let rewritten = fix_total(fixes);
    let remaining = outcome.declined;

    if args.diff {
        let color = diff_color();
        for fix in fixes {
            print!(
                "{}",
                unified_diff(&fix.path, &fix.original, &fix.fixed, color)
            );
        }
        report_diff_summary(fixes, remaining);
        return Ok(ExitCode::from(0));
    }

    for fix in fixes {
        fix.write_preserving_encoding()?;
    }
    report_fix_summary(rewritten, remaining)?;
    Ok(fix_exit_code(remaining))
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

    #[test]
    fn colored_diagnostic_contains_ansi_escape_sequences() {
        let diagnostic = Diagnostic {
            path: PathBuf::from("main.py"),
            line: 2,
            column: 1,
            callee: "\"f\"".to_owned(),
            positional_count: 1,
            max_positional: 0,
        };
        let rendered = display_diagnostic(&diagnostic, true);
        assert!(rendered.contains("\u{1b}["));
        assert!(rendered.contains("KW001"));
        assert!(rendered.contains("Too many positional"));
    }

    #[test]
    fn plain_diagnostic_matches_library_display() {
        let diagnostic = Diagnostic {
            path: PathBuf::from("main.py"),
            line: 2,
            column: 1,
            callee: "\"f\"".to_owned(),
            positional_count: 1,
            max_positional: 0,
        };
        assert_eq!(
            display_diagnostic(&diagnostic, false),
            diagnostic.display_path()
        );
    }

    #[test]
    fn success_and_found_summaries_render_plain_and_colored() {
        assert_eq!(success_message(false), "All checks passed!");
        assert!(success_message(true).contains("\u{1b}["));
        assert_eq!(found_summary(1, false), "Found 1 error.");
        assert_eq!(found_summary(2, false), "Found 2 errors.");
        let colored = found_summary(2, true);
        assert!(colored.contains("\u{1b}["));
        assert!(colored.contains("Found 2 errors."));
    }

    #[test]
    fn diff_summary_lines_cover_empty_fixable_and_remaining_cases() {
        let fix = FileFix {
            path: PathBuf::from("main.py"),
            original: "f(1)\n".to_owned(),
            fixed: "f(a=1)\n".to_owned(),
            count: 1,
        };
        assert_eq!(diff_summary_lines(&[], 0, false), ["All checks passed!"]);
        assert_eq!(diff_summary_lines(&[], 1, false), ["1 error would remain."]);
        assert_eq!(
            diff_summary_lines(std::slice::from_ref(&fix), 0, false),
            ["Would fix 1 error."]
        );
        assert_eq!(
            diff_summary_lines(&[fix.clone(), fix], 2, false),
            ["Would fix 2 errors.", "2 errors would remain."]
        );
        let colored = diff_summary_lines(&[], 0, true);
        assert!(colored[0].contains("\u{1b}["));
        let colored = diff_summary_lines(
            &[FileFix {
                path: PathBuf::from("main.py"),
                original: String::new(),
                fixed: String::new(),
                count: 1,
            }],
            1,
            true,
        );
        assert_eq!(colored.len(), 2);
        assert!(colored.iter().all(|line| line.contains("\u{1b}[")));
    }

    #[test]
    fn fix_summary_covers_plain_and_colored_outcomes() {
        assert_eq!(fix_summary(0, 0, false), "All checks passed!");
        assert_eq!(
            fix_summary(1, 0, false),
            "Found 1 error (1 fixed, 0 remaining)."
        );
        assert_eq!(
            fix_summary(1, 2, false),
            "Found 3 errors (1 fixed, 2 remaining)."
        );
        for summary in [
            fix_summary(0, 0, true),
            fix_summary(1, 0, true),
            fix_summary(1, 2, true),
        ] {
            assert!(summary.contains("\u{1b}["));
        }
    }
}
