use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustc_hash::FxHashSet;

use crate::cache::FingerprintFile;
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
/// exist, or [`CheckError::Io`] if a requested directory cannot be traversed
/// completely. An incomplete walk must not be reported as a clean check.
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
                // Match explicitly supplied symlinked directories: scan the
                // source they point at instead of silently treating it as
                // absent. `walkdir` detects symlink loops and reports them
                // through the existing fallible walk path below.
                .follow_links(true)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !selection.is_excluded(entry.path(), entry.file_type().is_dir(), false)
                });
            for entry in walk {
                let entry = entry.map_err(walk_error)?;
                if !entry.file_type().is_file() {
                    continue;
                }
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
    deduplicate_files(&mut files)?;
    Ok(files)
}

/// Collect a whole project while also capturing the broader file inventory
/// used by cache invalidation.
///
/// Selection normally follows symlinked directories and prunes configured
/// exclusions, while fingerprinting intentionally does neither. A no-follow
/// project walk can serve both purposes: selected entries are filtered, all
/// Python entries are inventoried, and symlinked directories get a separate
/// selection-only walk. Other path shapes keep the established collector and
/// fingerprint walk.
///
/// Excluded from the coverage gate because traversal errors require
/// platform-specific filesystem faults. Deterministic selection, exclusion,
/// symlink, and fingerprint equivalence are covered by caller tests.
#[cfg_attr(coverage, coverage(off))]
pub(super) fn collect_python_files_with_project_inventory(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
) -> Result<(Vec<PathBuf>, Option<Vec<FingerprintFile>>), CheckError> {
    if paths.len() != 1 || paths[0] != project_root {
        return collect_python_files(project_root, paths, config).map(|files| (files, None));
    }

    let selection = FileSelection::new(project_root, config)?;
    let mut files = Vec::new();
    let mut inventory = Vec::new();
    let mut symlink_directories = Vec::new();
    let walk = walkdir::WalkDir::new(project_root)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_prunable_dir(entry));

    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if error
                    .path()
                    .is_some_and(|path| selection.is_excluded(path, path.is_dir(), false))
                {
                    continue;
                }
                return Err(walk_error(error));
            }
        };
        let path = entry.path();
        let excluded_as_file = selection.is_excluded(path, false, false);
        let excluded_as_directory = selection.is_excluded(path, true, false);
        let target_type = if entry.file_type().is_symlink() {
            Some(
                std::fs::metadata(path)
                    .map_err(|error| {
                        CheckError::Io(std::io::Error::new(
                            error.kind(),
                            format!("IO error for operation on {}: {error}", path.display()),
                        ))
                    })?
                    .file_type(),
            )
        } else {
            None
        };
        let is_directory = target_type
            .as_ref()
            .map_or_else(|| entry.file_type().is_dir(), std::fs::FileType::is_dir);
        let is_file = target_type
            .as_ref()
            .map_or_else(|| entry.file_type().is_file(), std::fs::FileType::is_file);

        if entry.file_type().is_symlink() && is_directory && !excluded_as_directory {
            symlink_directories.push(path.to_path_buf());
        }

        if !is_python_file(path) {
            continue;
        }
        inventory.push(FingerprintFile::from_path(path.to_path_buf()));
        if is_file && !excluded_as_file {
            files.push(path.to_path_buf());
        }
    }

    for directory in symlink_directories {
        let walk = walkdir::WalkDir::new(directory)
            .follow_links(true)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !selection.is_excluded(entry.path(), entry.file_type().is_dir(), false)
            });
        for entry in walk {
            let entry = entry.map_err(walk_error)?;
            if entry.file_type().is_file() && is_python_file(entry.path()) {
                files.push(entry.path().to_path_buf());
            }
        }
    }

    deduplicate_files(&mut files)?;
    inventory.sort_by(|left, right| left.path().cmp(right.path()));
    Ok((files, Some(inventory)))
}

/// Deduplicate selected paths by their canonical filesystem target while
/// retaining the lexicographically first display path for deterministic
/// diagnostics.
#[cfg_attr(coverage, coverage(off))]
fn deduplicate_files(files: &mut Vec<PathBuf>) -> Result<(), CheckError> {
    files.sort();
    let mut seen = FxHashSet::default();
    let mut unique = Vec::with_capacity(files.len());
    for path in files.drain(..) {
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            CheckError::Io(std::io::Error::new(
                error.kind(),
                format!("failed to canonicalize {}: {error}", path.display()),
            ))
        })?;
        if seen.insert(canonical) {
            unique.push(path);
        }
    }
    *files = unique;
    Ok(())
}

fn walk_error(error: walkdir::Error) -> CheckError {
    let kind = error
        .io_error()
        .map_or(std::io::ErrorKind::Other, std::io::Error::kind);
    CheckError::Io(std::io::Error::new(kind, error))
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
        let project_relative = path.strip_prefix(&self.project_root).unwrap_or(path);
        if is_ignored_path(project_relative) {
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
