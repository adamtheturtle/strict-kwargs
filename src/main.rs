//! CLI for ``strict-kwargs``.

// See `lib.rs` for why this is gated on `coverage` rather than
// `coverage_nightly`.
#![cfg_attr(coverage, feature(coverage_attribute))]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args as ClapArgs, Parser, Subcommand};
use strict_kwargs::{check_paths, find_project_root, fix_paths, unified_diff, CheckError, Config};

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
}

#[derive(Debug, ClapArgs)]
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
    let config = Config::load(&project_root);
    let diagnostics = check_paths(&project_root, &args.paths, &config, args.python.as_deref())?;
    let mut failed = false;
    for diagnostic in &diagnostics {
        eprintln!("{}", diagnostic.display_path());
        failed = true;
    }
    if failed {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::from(0))
    }
}

fn run_fix(args: FixArgs) -> Result<ExitCode, CheckError> {
    let project_root = project_root_for(args.project_root, &args.paths);
    let config = Config::load(&project_root);
    let fixes = fix_paths(&project_root, &args.paths, &config)?;
    if fixes.is_empty() {
        eprintln!("strict-kwargs: no fixes to apply");
        return Ok(ExitCode::from(0));
    }

    if args.diff {
        for fix in &fixes {
            print!("{}", unified_diff(&fix.path, &fix.original, &fix.fixed));
        }
        return Ok(ExitCode::from(0));
    }

    let mut total = 0usize;
    for fix in &fixes {
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
    Ok(ExitCode::from(0))
}

#[cfg(test)]
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
}
