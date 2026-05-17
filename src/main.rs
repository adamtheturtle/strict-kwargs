//! CLI for ``strict-kwargs``.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use strict_kwargs::{check_paths, find_project_root, CheckError, Config};

#[derive(Debug, Parser)]
#[command(
    name = "strict-kwargs",
    about = "Enforce using keyword arguments where possible (fast, independent of mypy/ty)"
)]
struct Args {
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

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("strict-kwargs: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, CheckError> {
    let args = Args::parse();
    let start = args
        .paths
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let project_root = args
        .project_root
        .unwrap_or_else(|| find_project_root(&start));
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
