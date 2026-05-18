//! Load ``[tool.strict_kwargs]`` from ``pyproject.toml``.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Resolved `[tool.strict_kwargs]` configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Fully-qualified callee names to skip (e.g. `package.module.func`).
    #[serde(default)]
    pub ignore_names: Vec<String>,
    /// Emit verbose resolution diagnostics to stderr.
    #[serde(default)]
    pub debug: bool,
}

impl Config {
    /// Load configuration from `pyproject.toml` under `project_root`.
    ///
    /// Returns [`Config::default`] if the file is missing or unreadable.
    #[must_use]
    pub fn load(project_root: &Path) -> Self {
        let pyproject = project_root.join("pyproject.toml");
        if !pyproject.is_file() {
            return Self::default();
        }
        let Ok(contents) = std::fs::read_to_string(&pyproject) else {
            return Self::default();
        };
        Self::from_pyproject_str(&contents)
    }

    /// Parse configuration from the contents of a `pyproject.toml`.
    ///
    /// Returns [`Config::default`] if the table is absent or malformed.
    #[must_use]
    pub fn from_pyproject_str(contents: &str) -> Self {
        let Ok(document) = contents.parse::<toml::Table>() else {
            return Self::default();
        };
        let Some(tool) = document.get("tool").and_then(toml::Value::as_table) else {
            return Self::default();
        };
        let Some(strict) = tool.get("strict_kwargs") else {
            return Self::default();
        };
        strict.clone().try_into().unwrap_or_default()
    }

    /// Whether `fullname` is in the configured ignore list.
    #[must_use]
    pub fn is_ignored(&self, fullname: &str) -> bool {
        self.ignore_names.iter().any(|name| name == fullname)
    }
}

/// Discover project root by walking up from ``start`` looking for ``pyproject.toml``.
#[must_use]
pub fn find_project_root(start: &Path) -> PathBuf {
    let mut current = if start.is_file() {
        start.parent().unwrap_or(start).to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if current.join("pyproject.toml").is_file() {
            return current;
        }
        if !current.pop() {
            return start.to_path_buf();
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_kwargs_table() {
        let config = Config::from_pyproject_str(
            r#"
      [tool.strict_kwargs]
      ignore_names = ["main.func", "builtins.str"]
      debug = true
      "#,
        );
        assert_eq!(
            config.ignore_names,
            vec!["main.func".to_string(), "builtins.str".to_string()]
        );
        assert!(config.debug);
    }

    #[test]
    fn malformed_table_falls_back_to_default() {
        // Not valid TOML at all.
        assert!(Config::from_pyproject_str("this is not toml = =")
            .ignore_names
            .is_empty());
        // Valid TOML, but no `[tool]` table.
        assert!(Config::from_pyproject_str("[project]\nname = \"x\"\n")
            .ignore_names
            .is_empty());
        // `[tool]` present but no `strict_kwargs`.
        assert!(Config::from_pyproject_str("[tool.other]\nk = 1\n")
            .ignore_names
            .is_empty());
        // `strict_kwargs` present but the wrong shape (string, not a table).
        let config = Config::from_pyproject_str("[tool]\nstrict_kwargs = \"oops\"\n");
        assert!(config.ignore_names.is_empty());
        assert!(!config.debug);
    }

    #[test]
    fn is_ignored_matches_exact_names() {
        let config = Config::from_pyproject_str("[tool.strict_kwargs]\nignore_names = [\"a.b\"]\n");
        assert!(config.is_ignored("a.b"));
        assert!(!config.is_ignored("a.c"));
    }

    #[test]
    fn load_missing_pyproject_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config::load(dir.path());
        assert!(config.ignore_names.is_empty());
        assert!(!config.debug);
    }

    #[test]
    fn load_reads_pyproject_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\nignore_names = [\"pkg.f\"]\n",
        )
        .expect("write");
        let config = Config::load(dir.path());
        assert_eq!(config.ignore_names, vec!["pkg.f".to_string()]);
    }

    #[test]
    fn load_unreadable_pyproject_is_default() {
        // `pyproject.toml` exists (so `is_file()` is true) but is not valid
        // UTF-8, so `read_to_string` fails and we fall back to the default.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyproject.toml"), [0xff, 0xfe, 0x00]).expect("write");
        let config = Config::load(dir.path());
        assert!(config.ignore_names.is_empty());
    }

    #[test]
    fn find_project_root_walks_up_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("pyproject.toml"), "[project]\n").expect("write");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let file = nested.join("main.py");
        std::fs::write(&file, "").expect("write");

        // From a nested file: walk up to the directory holding pyproject.toml.
        assert_eq!(find_project_root(&file), root);
        // From a directory: same result.
        assert_eq!(find_project_root(&nested), root);
    }

    #[test]
    fn find_project_root_without_pyproject_returns_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let start = dir.path().join("no_pyproject");
        std::fs::create_dir_all(&start).expect("mkdir");
        assert_eq!(find_project_root(&start), start);
    }
}
