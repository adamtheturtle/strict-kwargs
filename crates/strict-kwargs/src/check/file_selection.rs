use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustc_hash::FxHashSet;

use crate::config::Config;
use crate::error::CheckError;

/// Collect the `.py`/`.pyi` files reachable from `paths`.
///
/// A path that is neither a file nor a directory does not exist: that is a
/// hard error ([`CheckError::PathNotFound`]), like `ruff`, rather than a
/// silent skip that would let a mistyped target report "clean" in CI
/// (issue #55). An *existing* file passed directly that is not Python is
/// still skipped - that is a deliberate selection, not a mistake.
///
/// # Errors
///
/// Returns [`CheckError::PathNotFound`] for the first path that does not
/// exist.
pub(super) fn collect_python_files(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
) -> Result<Vec<PathBuf>, CheckError> {
    let selection = FileSelection::new(project_root, config)?;
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_python_file(path) && !selection.is_excluded(path, false, true) {
                files.push(path.clone());
            }
        } else if path.is_dir() {
            // Prune excluded directories instead of descending into them and
            // discarding their files one by one: a real project's virtualenv
            // alone is tens of thousands of entries, so the unpruned walk
            // dominated whole-project runtime and run-to-run variance. The
            // walk root is never pruned so `strict-kwargs .` keeps working
            // even when `.` contains ignored path components.
            let walk = walkdir::WalkDir::new(path)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !selection.is_excluded(entry.path(), entry.file_type().is_dir(), false)
                });
            for entry in walk
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let entry_path = entry.path().to_path_buf();
                if is_python_file(&entry_path) {
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

pub(super) fn explicit_python_files(paths: &[PathBuf]) -> FxHashSet<PathBuf> {
    paths
        .iter()
        .filter(|path| path.is_file() && is_python_file(path))
        .cloned()
        .collect()
}

pub(super) struct FileSelection {
    project_root: PathBuf,
    extend_exclude: Gitignore,
    force_exclude: bool,
}

impl FileSelection {
    pub(super) fn new(project_root: &Path, config: &Config) -> Result<Self, CheckError> {
        let mut builder = GitignoreBuilder::new(project_root);
        for pattern in &config.extend_exclude {
            builder
                .add_line(None, pattern)
                .map_err(|error| CheckError::ConfigInvalid {
                    path: project_root.join("pyproject.toml"),
                    message: format!(
                        "has an invalid `extend_exclude` pattern `{pattern}`: {error}"
                    ),
                })?;
        }
        let extend_exclude = build_extend_exclude(&builder, project_root)?;
        Ok(Self {
            project_root: project_root.to_path_buf(),
            extend_exclude,
            force_exclude: config.force_exclude,
        })
    }

    pub(super) fn is_excluded(&self, path: &Path, is_dir: bool, explicit: bool) -> bool {
        if explicit && !self.force_exclude {
            return false;
        }
        if is_ignored_path(path) {
            return true;
        }
        if self.project_root.is_absolute()
            && path.is_absolute()
            && !path.starts_with(&self.project_root)
        {
            return false;
        }
        self.extend_exclude
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }
}

/// Build the already-validated gitignore matcher.
///
/// Excluded from the coverage gate because `GitignoreBuilder::add_line`
/// validates each glob eagerly; a later `build` failure is a defensive
/// third-party error path that is not practically triggerable through
/// `extend_exclude`.
#[cfg_attr(coverage, coverage(off))]
fn build_extend_exclude(
    builder: &GitignoreBuilder,
    project_root: &Path,
) -> Result<Gitignore, CheckError> {
    builder.build().map_err(|error| CheckError::ConfigInvalid {
        path: project_root.join("pyproject.toml"),
        message: format!("has invalid `extend_exclude` patterns: {error}"),
    })
}

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "py" || ext == "pyi")
}

/// Whether `entry` is a built-in ignored directory (`.git`, `.venv` and other
/// dot-directories, `venv`, `__pycache__`), so cache fingerprinting can avoid
/// descending into default-skipped trees.
#[cfg_attr(coverage, coverage(off))]
pub fn is_prunable_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "venv" || name == "__pycache__"
}

#[cfg_attr(coverage, coverage(off))]
pub(super) fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') || name == "venv" || name == "__pycache__"
        }
        _ => false,
    })
}
