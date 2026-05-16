//! Load ``[tool.strict_kwargs]`` and legacy ``mypy_strict_kwargs`` configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use configparser::ini::Ini;
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ignore_names: Vec<String>,
    #[serde(default)]
    pub debug: bool,
}

impl Config {
    pub fn load(project_root: &Path) -> Self {
        let mut config = Self::default();

        let setup_cfg = project_root.join("setup.cfg");
        if setup_cfg.is_file() {
            config = config.merge(Self::from_ini_path(&setup_cfg));
        }

        let mypy_ini = project_root.join("mypy.ini");
        if mypy_ini.is_file() {
            config = config.merge(Self::from_ini_path(&mypy_ini));
        }

        let dot_mypy_ini = project_root.join(".mypy.ini");
        if dot_mypy_ini.is_file() {
            config = config.merge(Self::from_ini_path(&dot_mypy_ini));
        }

        let pyproject = project_root.join("pyproject.toml");
        if pyproject.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&pyproject) {
                config = config.merge(Self::from_pyproject_str(&contents));
            }
        }

        config
    }

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

    fn from_ini_path(path: &Path) -> Self {
        let Some(path_str) = path.to_str() else {
            return Self::default();
        };
        let mut ini = Ini::new();
        if ini.load(path_str).is_err() {
            return Self::default();
        }
        let mut config = Self::default();
        let map = ini.get_map_ref();
        for section_name in ["mypy_strict_kwargs", "strict_kwargs"] {
            if let Some(section) = map.get(section_name) {
                config = config.merge(Self::from_ini_section(section));
            }
        }
        config
    }

    fn from_ini_section(section: &HashMap<String, Option<String>>) -> Self {
        let mut config = Self::default();
        if let Some(Some(ignore_names)) = section.get("ignore_names") {
            config.ignore_names = parse_ignore_names(ignore_names);
        }
        if let Some(Some(debug)) = section.get("debug") {
            config.debug = debug.eq_ignore_ascii_case("true") || debug == "1";
        }
        config
    }

    fn merge(mut self, other: Self) -> Self {
        if !other.ignore_names.is_empty() {
            self.ignore_names = other.ignore_names;
        }
        if other.debug {
            self.debug = true;
        }
        self
    }

    pub fn is_ignored(&self, fullname: &str) -> bool {
        self.ignore_names.iter().any(|name| name == fullname)
    }
}

fn parse_ignore_names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Discover project root by walking up from ``start`` looking for config files.
pub fn find_project_root(start: &Path) -> PathBuf {
    let mut current = if start.is_file() {
        start.parent().unwrap_or(start).to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if is_project_root(&current) {
            return current;
        }
        if !current.pop() {
            return start.to_path_buf();
        }
    }
}

fn is_project_root(path: &Path) -> bool {
    ["pyproject.toml", "mypy.ini", ".mypy.ini", "setup.cfg"]
        .iter()
        .any(|name| path.join(name).is_file())
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

    #[test]
    fn parses_mypy_strict_kwargs_ini_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("mypy.ini"),
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
debug = true
"#,
        )
        .unwrap();
        let config = Config::load(temp.path());
        assert_eq!(
            config.ignore_names,
            vec!["main.func".to_string(), "builtins.str".to_string()]
        );
        assert!(config.debug);
    }
}
