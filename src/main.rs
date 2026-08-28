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
    check_paths, find_project_root, fix_paths_with_opt_ins, is_python_environment, unified_diff,
    CheckError, Config, Diagnostic, FileFix, FixOptIns, OutputFormat,
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
#[command(group(
    clap::ArgGroup::new("fix_mode")
        .args(["fix", "diff"])
        .multiple(false)
))]
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
    #[arg(long, conflicts_with = "diff")]
    fix: bool,

    /// Preview fixes as a unified diff instead of writing files.
    #[arg(long)]
    diff: bool,

    /// Include fixes that may change runtime behavior.
    #[arg(long, requires = "fix_mode")]
    unsafe_fixes: bool,

    /// Diagnostic output format.
    #[arg(long, value_enum, conflicts_with_all = ["fix", "diff"])]
    output_format: Option<OutputFormat>,

    /// Directory for the persistent on-disk diagnostic cache.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["fix", "diff"])]
    cache_dir: Option<PathBuf>,

    /// Python environment for the `ty` inference fallback.
    #[arg(long, value_name = "PATH")]
    python: Option<PathBuf>,

    /// Report a `KW002` error for every `# noqa: KW001` that suppressed
    /// nothing.
    #[arg(long, conflicts_with_all = ["fix", "diff"])]
    error_on_unused_noqa: bool,
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
fn resolve_python_env(python: Option<PathBuf>) -> Result<Option<PathBuf>, CheckError> {
    let Some(path) = python else {
        return Ok(None);
    };
    if path.exists() {
        if is_python_environment(&path) {
            return Ok(Some(path));
        }
        return Err(CheckError::InvalidPythonEnvironment { path });
    }
    eprintln!(
        "warning: --python {} does not exist; ignoring it and falling back to \
         ty's own environment discovery",
        path.display()
    );
    Ok(None)
}

fn project_root_for(explicit: Option<PathBuf>, paths: &[PathBuf]) -> Result<PathBuf, CheckError> {
    if let Some(root) = explicit {
        if !root.is_dir() {
            return Err(CheckError::InvalidProjectRoot { path: root });
        }
        return Ok(root);
    }

    let start = paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let root = find_project_root(&start);
    let root_identity = project_root_identity(&root);
    for path in paths.iter().skip(1) {
        let other = find_project_root(path);
        if project_root_identity(&other) != root_identity {
            return Err(CheckError::MultipleProjectRoots {
                first: root,
                second: other,
            });
        }
    }
    Ok(root)
}

/// Compare roots by what they name, not by how they are spelled, so a relative
/// and an absolute path to one project are not read as two projects.
///
/// The fallback covers a root that cannot be canonicalized, such as one deleted
/// between discovery and this call; comparing the paths as written is then the
/// best available answer.
#[cfg_attr(coverage, coverage(off))]
fn project_root_identity(root: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn resolve_configured_cache_dir(project_root: &std::path::Path, cache_dir: &PathBuf) -> PathBuf {
    if cache_dir.is_absolute() {
        cache_dir.clone()
    } else {
        project_root.join(cache_dir)
    }
}

#[cfg_attr(coverage, coverage(off))]
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
        .or_else(|| {
            std::env::var_os(CACHE_DIR_ENV_VAR).and_then(|value| {
                // An empty value must not resolve to the working directory
                // (issue #513).
                (!value.is_empty()).then(|| PathBuf::from(value))
            })
        })
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
    let project_root = project_root_for(args.project_root, &args.paths)?;
    let mut config = Config::load(&project_root)?;
    // The flag turns the rule on; it never turns a configured one off.
    config.error_on_unused_noqa |= args.error_on_unused_noqa;
    let config = config;
    let output_format = args.output_format.unwrap_or(config.output_format);
    let python_env = resolve_python_env(args.python)?;
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
    /// Omitted for rules that name no callee (`KW002`).
    #[serde(skip_serializing_if = "Option::is_none")]
    callee: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct JsonLocation {
    row: usize,
    column: usize,
}

impl<'a> From<&'a Diagnostic> for JsonDiagnostic<'a> {
    fn from(diagnostic: &'a Diagnostic) -> Self {
        Self {
            code: diagnostic.code(),
            filename: diagnostic.path.display().to_string(),
            location: JsonLocation {
                row: diagnostic.line,
                column: diagnostic.column,
            },
            message: diagnostic.message(),
            callee: diagnostic.callee(),
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
            let stdout = std::io::stdout();
            let mut stdout = BufWriter::new(stdout.lock());
            writeln!(stdout, "{json}")?;
            stdout.flush()?;
        }
        OutputFormat::Github => {
            let stdout = std::io::stdout();
            let mut stdout = BufWriter::new(stdout.lock());
            for diagnostic in diagnostics {
                writeln!(stdout, "{}", diagnostic.github_annotation())?;
            }
            stdout.flush()?;
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
        diagnostic.code().red().bold(),
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

#[cfg_attr(coverage, coverage(off))]
fn diff_header_path(project_root: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if !path.is_absolute() {
        // On Windows, `/` is not absolute and has no file name — still fall back.
        return path
            .file_name()
            .map_or_else(|| PathBuf::from("file.py"), |_| path.to_path_buf());
    }
    if let Ok(relative) = path.strip_prefix(project_root) {
        return relative.to_path_buf();
    }
    if let (Ok(root), Ok(canonical_path)) = (project_root.canonicalize(), path.canonicalize()) {
        if let Ok(relative) = canonical_path.strip_prefix(root) {
            return relative.to_path_buf();
        }
    }
    path.file_name()
        .map_or_else(|| PathBuf::from("file.py"), PathBuf::from)
}

fn run_check_fix(args: CheckArgs) -> Result<ExitCode, CheckError> {
    let args_fix_opt_ins = fix_opt_ins_from_args(&args);
    let project_root = project_root_for(args.project_root, &args.paths)?;
    let config = Config::load(&project_root)?;
    let fix_opt_ins = FixOptIns {
        synthesized_constructors: config.fix_synthesized_constructors
            || args_fix_opt_ins.synthesized_constructors,
    };
    let python_env = resolve_python_env(args.python)?;
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
        let stdout = std::io::stdout();
        let mut stdout = BufWriter::new(stdout.lock());
        for fix in fixes {
            let path = diff_header_path(&project_root, &fix.path);
            write!(
                stdout,
                "{}",
                unified_diff(&path, &fix.original, &fix.fixed, color)
            )?;
        }
        stdout.flush()?;
        report_diff_summary(fixes, remaining);
        return Ok(fix_exit_code(remaining));
    }

    strict_kwargs::write_all_preserving_encoding(fixes)?;
    report_fix_summary(rewritten, remaining)?;
    Ok(fix_exit_code(remaining))
}

/// Restores the process-global working directory even if a test panics, and
/// serialises the tests that change it against each other.
///
/// Leaving the change in place on a failed assertion let other tests in this
/// binary resolve relative paths against the temporary directory, which made
/// the diff-header tests flake when run together (issue #1091).
#[cfg(test)]
struct CurrentDirGuard {
    previous: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl CurrentDirGuard {
    fn set(path: &std::path::Path) -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(path).expect("chdir");
        Self {
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).expect("restore cwd");
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn project_root_uses_explicit_when_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let explicit = dir.path().to_path_buf();
        assert_eq!(
            project_root_for(Some(explicit.clone()), &[PathBuf::from("x.py")])
                .expect("valid project root"),
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
        assert_eq!(
            project_root_for(None, &[file]).expect("discovered project root"),
            dir.path()
        );
    }

    #[test]
    fn project_root_rejects_paths_from_different_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("a");
        let second = dir.path().join("b");
        std::fs::create_dir_all(&first).expect("mkdir");
        std::fs::create_dir_all(&second).expect("mkdir");
        std::fs::write(first.join("pyproject.toml"), "[project]\n").expect("write");
        std::fs::write(second.join("pyproject.toml"), "[project]\n").expect("write");

        let error = project_root_for(None, &[first.join("m.py"), second.join("m.py")])
            .expect_err("mixed roots must be rejected");
        assert!(matches!(error, CheckError::MultipleProjectRoots { .. }));
    }

    #[test]
    fn project_root_accepts_sibling_paths_without_a_pyproject() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("pkg");
        std::fs::create_dir_all(&package).expect("mkdir");
        let first = package.join("a.py");
        let second = package.join("b.py");
        std::fs::write(&first, "").expect("write");
        std::fs::write(&second, "").expect("write");

        // Without a ``pyproject.toml`` both files share the directory they sit
        // in, so this is one project rather than two.
        assert_eq!(
            project_root_for(None, &[first, second]).expect("one project root"),
            package
        );
    }

    #[test]
    fn project_root_accepts_equivalent_spellings_of_one_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\n").expect("write");
        let package = dir.path().join("pkg");
        std::fs::create_dir_all(&package).expect("mkdir");
        let file = package.join("m.py");
        std::fs::write(&file, "").expect("write");
        let indirect = package.join("..").join("pkg").join("m.py");

        assert!(project_root_for(None, &[file, indirect]).is_ok());
    }

    #[test]
    fn project_root_falls_back_to_dot_when_no_paths() {
        // `paths.first()` is `None` (unreachable from the CLI because clap
        // defaults `paths` to `.`, but covered here for completeness).
        let root = project_root_for(None, &[]).expect("fallback project root");
        assert_eq!(root, find_project_root(&PathBuf::from(".")));
    }

    #[test]
    fn python_env_unset_stays_unset() {
        assert_eq!(resolve_python_env(None).expect("valid"), None);
    }

    #[test]
    fn python_env_existing_path_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyvenv.cfg"), "").expect("write");
        let path = dir.path().to_path_buf();
        assert_eq!(
            resolve_python_env(Some(path.clone())).expect("valid"),
            Some(path)
        );
    }

    #[test]
    fn python_env_nonexistent_path_is_dropped() {
        // Nonexistent `--python`: dropped (so the run falls back to ty's own
        // discovery) rather than silently forwarded and ignored (issue #55).
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no_such_python");
        assert_eq!(resolve_python_env(Some(missing)).expect("valid"), None);
    }

    #[test]
    fn colored_diagnostic_contains_ansi_escape_sequences() {
        let diagnostic = Diagnostic::too_many_positional(
            PathBuf::from("main.py"),
            2,
            1,
            "\"f\"".to_owned(),
            1,
            0,
        );
        let rendered = display_diagnostic(&diagnostic, true);
        assert!(rendered.contains("\u{1b}["));
        assert!(rendered.contains("KW001"));
        assert!(rendered.contains("Too many positional"));
    }

    #[test]
    fn plain_diagnostic_matches_library_display() {
        let diagnostic = Diagnostic::too_many_positional(
            PathBuf::from("main.py"),
            2,
            1,
            "\"f\"".to_owned(),
            1,
            0,
        );
        assert_eq!(
            display_diagnostic(&diagnostic, false),
            diagnostic.display_path()
        );
    }

    #[test]
    fn diff_header_path_keeps_relative_paths() {
        let root = PathBuf::from("/project");
        assert_eq!(
            diff_header_path(&root, Path::new("src/main.py")),
            PathBuf::from("src/main.py")
        );
    }

    #[test]
    fn diff_header_path_strips_project_root_from_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let absolute = root.join("main.py");
        assert_eq!(diff_header_path(&root, &absolute), PathBuf::from("main.py"));
    }

    #[test]
    fn diff_header_path_canonicalizes_before_stripping_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).expect("create project dir");
        let file = root.join("main.py");
        std::fs::write(&file, "").expect("write file");
        let _cwd = super::CurrentDirGuard::set(dir.path());
        let absolute = file.canonicalize().expect("canonicalize file");
        assert_eq!(
            diff_header_path(Path::new("proj"), &absolute),
            PathBuf::from("main.py")
        );
    }

    #[test]
    fn diff_header_path_falls_back_to_file_name_outside_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("create project dir");
        let outside = dir.path().join("elsewhere").join("other.py");
        std::fs::create_dir_all(outside.parent().unwrap()).expect("create parent");
        std::fs::write(&outside, "").expect("write file");
        assert_eq!(diff_header_path(&root, &outside), PathBuf::from("other.py"));
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

/// Exercises `diff_header_path` branches under llvm-cov. The main `tests`
/// module is `#[coverage(off)]`, so these live in a sibling module.
#[cfg(test)]
mod diff_header_path_coverage {
    use super::diff_header_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn diff_header_path_falls_back_to_file_py_without_file_name() {
        assert_eq!(
            diff_header_path(Path::new("/project"), Path::new("/")),
            PathBuf::from("file.py")
        );
    }

    #[test]
    fn diff_header_path_canonicalizes_before_stripping_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).expect("create project dir");
        let file = root.join("main.py");
        std::fs::write(&file, "").expect("write file");
        let _cwd = super::CurrentDirGuard::set(dir.path());
        let absolute = file.canonicalize().expect("canonicalize file");
        assert_eq!(
            diff_header_path(Path::new("proj"), &absolute),
            PathBuf::from("main.py")
        );
    }
}
