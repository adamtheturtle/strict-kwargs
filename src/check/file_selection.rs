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
                    entry.depth() == 0 || !walk_entry_is_excluded(&selection, entry)
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

/// Collect Python files for an in-place fix without escaping requested trees.
///
/// Ordinary checks follow directory symlinks because reading a linked source is
/// useful. Mutation is different: a directory argument defines the boundary
/// within which writes are allowed. Canonicalizing both sides prevents a
/// symlink nested below that argument from redirecting a write elsewhere. A
/// directly requested file or symlinked directory is still an explicit opt-in.
#[cfg_attr(coverage, coverage(off))]
pub(super) fn collect_python_files_for_fix(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
) -> Result<Vec<PathBuf>, CheckError> {
    let files = collect_python_files(project_root, paths, config)?;
    let directory_roots = paths
        .iter()
        .filter(|path| path.is_dir())
        .map(|path| canonicalize_for_fix(path))
        .collect::<Result<Vec<_>, _>>()?;
    let explicit_files = paths
        .iter()
        .filter(|path| path.is_file())
        .map(|path| canonicalize_for_fix(path))
        .collect::<Result<FxHashSet<_>, _>>()?;

    files
        .into_iter()
        .map(|path| canonicalize_for_fix(&path).map(|canonical| (path, canonical)))
        .filter_map(|result| match result {
            Ok((path, canonical))
                if explicit_files.contains(&canonical)
                    || directory_roots
                        .iter()
                        .any(|root| canonical.starts_with(root)) =>
            {
                Some(Ok(path))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[cfg_attr(coverage, coverage(off))]
fn canonicalize_for_fix(path: &Path) -> Result<PathBuf, CheckError> {
    std::fs::canonicalize(path).map_err(|error| {
        CheckError::Io(std::io::Error::new(
            error.kind(),
            format!("IO error for operation on {}: {error}", path.display()),
        ))
    })
}

/// Collect a whole project while also capturing the broader file inventory
/// used by cache invalidation.
///
/// Selection follows symlinked directories and prunes configured exclusions.
/// The inventory must include the same followed targets so cache fingerprints
/// invalidate when a normally edited file behind a directory symlink changes
/// (#527). A no-follow project walk still inventories non-symlink entries;
/// each followed symlink directory is walked separately for both selection and
/// inventory. Other path shapes keep the established collector and fingerprint
/// walk.
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

        let excluded_symlink_target = entry.file_type().is_symlink()
            && symlink_target_directory_is_prunable(&selection, path);
        if entry.file_type().is_symlink()
            && is_directory
            && !excluded_as_directory
            && !excluded_symlink_target
        {
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
            .filter_entry(|entry| entry.depth() == 0 || !walk_entry_is_excluded(&selection, entry));
        for entry in walk {
            let entry = entry.map_err(walk_error)?;
            if entry.file_type().is_file() && is_python_file(entry.path()) {
                let path = entry.path().to_path_buf();
                inventory.push(FingerprintFile::from_path(path.clone()));
                files.push(path);
            }
        }
    }

    deduplicate_files(&mut files)?;
    inventory.sort_by(|left, right| left.path().cmp(right.path()));
    inventory.dedup_by(|left, right| left.path() == right.path());
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

#[cfg_attr(coverage, coverage(off))]
fn walk_entry_is_excluded(selection: &FileSelection, entry: &walkdir::DirEntry) -> bool {
    let path = entry.path();
    let is_directory = entry.file_type().is_dir() || path.is_dir();
    if selection.is_excluded(path, is_directory, false) {
        return true;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    is_directory && symlink_target_directory_is_prunable(selection, path)
}

#[cfg_attr(coverage, coverage(off))]
fn symlink_target_directory_is_prunable(selection: &FileSelection, path: &Path) -> bool {
    std::fs::canonicalize(path).ok().is_some_and(|target| {
        selection.is_extend_excluded(&target, true)
            || target.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with('.') || name == "venv" || name == "__pycache__"
            })
    })
}

/// The selected paths that the caller named explicitly.
///
/// Selection deduplicates by canonical filesystem target and keeps one display
/// path per target, which is not always the spelling the caller passed. Naming
/// the explicit files by their selected spelling keeps a parse failure on an
/// explicitly named file fatal, rather than skipped as if it were only walked
/// into.
#[cfg_attr(coverage, coverage(off))]
pub(super) fn explicit_python_files(paths: &[PathBuf], selected: &[PathBuf]) -> FxHashSet<PathBuf> {
    let explicit: FxHashSet<PathBuf> = paths
        .iter()
        .filter(|path| path.is_file() && is_python_file(path))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    if explicit.is_empty() {
        return FxHashSet::default();
    }
    selected
        .iter()
        .filter(|path| {
            std::fs::canonicalize(path).is_ok_and(|canonical| explicit.contains(&canonical))
        })
        .cloned()
        .collect()
}

pub(super) struct FileSelection {
    project_root: PathBuf,
    canonical_project_root: PathBuf,
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
            canonical_project_root: std::fs::canonicalize(project_root)
                .unwrap_or_else(|_| project_root.to_path_buf()),
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
        self.is_extend_excluded(path, is_dir)
    }

    fn is_extend_excluded(&self, path: &Path, is_dir: bool) -> bool {
        let normalized;
        let path = if self.project_root.is_absolute()
            && path.is_absolute()
            && !path.starts_with(&self.project_root)
        {
            let Ok(relative) = path.strip_prefix(&self.canonical_project_root) else {
                return false;
            };
            normalized = self.project_root.join(relative);
            normalized.as_path()
        } else {
            path
        };
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

#[cfg(test)]
mod file_selection_coverage {
    use super::{
        collect_python_files, collect_python_files_for_fix,
        collect_python_files_with_project_inventory,
    };
    use crate::config::Config;

    #[test]
    fn collect_python_files_for_fix_keeps_files_within_directory_scope() {
        let root = tempfile::tempdir().expect("tempdir");
        let pkg = root.path().join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");
        std::fs::write(pkg.join("in_scope.py"), "").expect("write in scope");
        std::fs::write(root.path().join("out_of_scope.py"), "").expect("write out of scope");

        let files = collect_python_files_for_fix(
            root.path(),
            std::slice::from_ref(&pkg),
            &Config::default(),
        )
        .expect("collect for fix");

        assert_eq!(files, vec![pkg.join("in_scope.py")]);
    }

    #[test]
    fn collect_python_files_for_fix_keeps_explicit_file_outside_directory_args() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir");
        std::fs::write(root.path().join("pkg/in_scope.py"), "").expect("write in scope");
        let explicit = root.path().join("explicit.py");
        std::fs::write(&explicit, "").expect("write explicit");

        let files = collect_python_files_for_fix(
            root.path(),
            &[root.path().join("pkg"), explicit.clone()],
            &Config::default(),
        )
        .expect("collect for fix");

        let mut expected = vec![root.path().join("pkg/in_scope.py"), explicit];
        expected.sort();
        let mut actual = files;
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn collect_python_files_for_fix_preserves_display_path_from_collection() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("main.py"), "").expect("write");
        let collected = collect_python_files(
            root.path(),
            &[root.path().join("./main.py")],
            &Config::default(),
        )
        .expect("collect");
        let fixed = collect_python_files_for_fix(
            root.path(),
            &[root.path().to_path_buf()],
            &Config::default(),
        )
        .expect("collect for fix");
        assert_eq!(fixed, collected);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_ignored_target_directory_is_pruned() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("project tempdir");
        let external = tempfile::tempdir().expect("external tempdir");
        let ignored = external.path().join("venv");
        let included = external.path().join("source");
        std::fs::create_dir_all(&ignored).expect("create ignored target");
        std::fs::create_dir_all(&included).expect("create included target");
        std::fs::write(ignored.join("dependency.py"), "").expect("write ignored Python file");
        std::fs::write(included.join("main.py"), "").expect("write included Python file");
        symlink(&ignored, root.path().join("linked")).expect("symlink ignored directory");
        symlink(&included, root.path().join("included")).expect("symlink included directory");

        let paths = [root.path().to_path_buf()];
        let files = collect_python_files(root.path(), &paths, &Config::default())
            .expect("collect ordinary files");
        assert_eq!(files, [root.path().join("included/main.py")]);

        let (files, _) =
            collect_python_files_with_project_inventory(root.path(), &paths, &Config::default())
                .expect("collect project inventory");
        assert_eq!(files, [root.path().join("included/main.py")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_extend_excluded_target_directory_is_pruned() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("project tempdir");
        let generated = root.path().join("generated");
        std::fs::create_dir_all(&generated).expect("create excluded target");
        std::fs::write(generated.join("output.py"), "").expect("write excluded Python file");
        symlink(&generated, root.path().join("linked")).expect("symlink excluded directory");
        let config = Config {
            extend_exclude: vec!["generated".to_string()],
            ..Config::default()
        };
        let paths = [root.path().to_path_buf()];

        assert!(collect_python_files(root.path(), &paths, &config)
            .expect("collect ordinary files")
            .is_empty());
        assert!(
            collect_python_files_with_project_inventory(root.path(), &paths, &config)
                .expect("collect project inventory")
                .0
                .is_empty()
        );
    }
}
