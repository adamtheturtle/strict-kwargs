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
}
