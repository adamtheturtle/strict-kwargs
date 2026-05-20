//! Load ``[tool.strict_kwargs]`` from ``pyproject.toml``.

use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;

use crate::error::CheckError;

#[cfg_attr(coverage, coverage(off))]
mod output_format {
    use serde::Deserialize;

    /// Diagnostic output format for `strict-kwargs check`.
    #[derive(
        Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize, clap::ValueEnum,
    )]
    #[serde(rename_all = "kebab-case")]
    pub enum OutputFormat {
        /// Human-oriented `path:line:column: error: ...` lines on stderr.
        #[default]
        Full,
        /// A JSON array of structured diagnostics on stdout.
        Json,
        /// GitHub Actions workflow command annotations on stdout.
        Github,
    }
}

pub use output_format::OutputFormat;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolved `[tool.strict_kwargs]` configuration.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Config {
    /// Required `strict-kwargs` version specifier for this project.
    #[serde(default)]
    pub required_version: Option<String>,
    /// Fully-qualified callee names to skip (e.g. `package.module.func`).
    #[serde(default)]
    pub ignore_names: Vec<String>,
    /// Additional path patterns to exclude from project walks, relative to
    /// the project root.
    #[serde(default)]
    pub extend_exclude: Vec<String>,
    /// Apply configured and built-in path exclusions even to explicitly
    /// passed file paths.
    #[serde(default)]
    pub force_exclude: bool,
    /// Directory for the persistent on-disk diagnostic cache.
    ///
    /// This affects where diagnostics are stored, not the diagnostics
    /// themselves, so it is omitted from cache fingerprints.
    #[serde(default, skip_serializing)]
    pub cache_dir: Option<PathBuf>,
    /// Emit verbose resolution diagnostics to stderr.
    #[serde(default)]
    pub debug: bool,
    /// Rewrite dataclass and `NamedTuple` constructor calls whose signatures
    /// were synthesized from class fields.
    #[serde(default)]
    pub fix_synthesized_constructors: bool,
    /// Diagnostic output format for `strict-kwargs check`.
    ///
    /// This affects how diagnostics are reported, not which diagnostics are
    /// found, so it is omitted from cache fingerprints.
    #[serde(default, skip_serializing)]
    pub output_format: OutputFormat,
}

impl Config {
    /// Load configuration from `pyproject.toml` under `project_root`.
    ///
    /// A missing `pyproject.toml`, or one without a `[tool.strict_kwargs]`
    /// table, yields [`Config::default`] — those are not errors. But a
    /// `pyproject.toml` that exists yet cannot be read or parsed, or whose
    /// `[tool.strict_kwargs]` has the wrong shape or value types, is a hard
    /// error rather than a silent fall back to defaults: that would hide a
    /// misconfigured `ignore_names` in exactly the automated contexts this
    /// tool targets (issue #55).
    ///
    /// # Errors
    ///
    /// Returns [`CheckError::ConfigInvalid`] if `pyproject.toml` exists but
    /// cannot be read, is not valid TOML, or its `[tool.strict_kwargs]`
    /// table is malformed or wrongly typed.
    pub fn load(project_root: &Path) -> Result<Self, CheckError> {
        let pyproject = project_root.join("pyproject.toml");
        if !pyproject.is_file() {
            return Ok(Self::default());
        }
        let contents =
            std::fs::read_to_string(&pyproject).map_err(|error| CheckError::ConfigInvalid {
                path: pyproject.clone(),
                message: format!("could not be read: {error}"),
            })?;
        Self::from_pyproject_str(&contents).map_err(|message| CheckError::ConfigInvalid {
            path: pyproject,
            message,
        })
    }

    /// Parse configuration from the contents of a `pyproject.toml`.
    ///
    /// An absent `[tool]`/`[tool.strict_kwargs]` table yields
    /// [`Config::default`] (a project need not configure this tool).
    ///
    /// # Errors
    ///
    /// Returns a human-readable message (phrased to follow the file path,
    /// e.g. `"is not valid TOML: …"`) if `contents` is not valid TOML, if
    /// `[tool.strict_kwargs]` is present but not a table, or if its value
    /// types do not match the schema (e.g. `ignore_names` not a list).
    pub fn from_pyproject_str(contents: &str) -> Result<Self, String> {
        let document = contents
            .parse::<toml::Table>()
            .map_err(|error| format!("is not valid TOML: {error}"))?;
        // An absent `[tool]` (or a `tool` that is not a table) just means
        // this project does not configure strict-kwargs — not an error.
        let Some(tool) = document.get("tool").and_then(toml::Value::as_table) else {
            return Ok(Self::default());
        };
        let Some(strict) = tool.get("strict_kwargs") else {
            return Ok(Self::default());
        };
        if !strict.is_table() {
            return Err(format!(
                "`[tool.strict_kwargs]` must be a table, found {}",
                strict.type_str()
            ));
        }
        let config: Self = strict
            .clone()
            .try_into()
            .map_err(|error| format!("has an invalid `[tool.strict_kwargs]` table: {error}"))?;
        if let Some(required_version) = &config.required_version {
            validate_required_version(required_version, CURRENT_VERSION)?;
        }
        Ok(config)
    }

    /// Whether `fullname` is in the configured ignore list.
    #[must_use]
    pub fn is_ignored(&self, fullname: &str) -> bool {
        self.ignore_names.iter().any(|name| name == fullname)
    }
}

fn validate_required_version(required_version: &str, current_version: &str) -> Result<(), String> {
    let required_version = required_version.trim();
    if required_version.is_empty() {
        return Err(
            "`required_version` must be an exact version or a `>=` version specifier".to_owned(),
        );
    }
    let current = parse_version(current_version, "current strict-kwargs version")?;
    if let Some(minimum) = required_version.strip_prefix(">=") {
        let minimum = parse_version(minimum.trim(), "`required_version` minimum")?;
        if minimum_required_version_is_satisfied(&current, &minimum) {
            return Ok(());
        }
        return Err(format!(
            "`required_version = \"{required_version}\"` is not satisfied by strict-kwargs \
             {current_version}; install a compatible strict-kwargs version or update the setting"
        ));
    }
    if required_version.starts_with(['<', '>', '=', '~', '^']) {
        return Err(format!(
            "`required_version = \"{required_version}\"` uses unsupported syntax; supported \
             forms are exact versions and `>=`"
        ));
    }
    let exact = parse_version(required_version, "`required_version`")?;
    if current == exact {
        return Ok(());
    }
    Err(format!(
        "`required_version = \"{required_version}\"` is not satisfied by strict-kwargs \
         {current_version}; install strict-kwargs {required_version} or update the setting"
    ))
}

fn minimum_required_version_is_satisfied(current: &Version, minimum: &Version) -> bool {
    if current >= minimum {
        return true;
    }
    minimum.pre.is_empty()
        && current.major == minimum.major
        && current.minor == minimum.minor
        && current.patch == minimum.patch
        && current
            .pre
            .as_str()
            .split('.')
            .next()
            .is_some_and(|identifier| identifier == "post")
}

fn parse_version(version: &str, label: &str) -> Result<Version, String> {
    Version::parse(version).map_err(|error| {
        format!("{label} must be a valid version like `2026.5.19-post.3`: {error}")
    })
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
      required_version = "2026.5.19-post.3"
      ignore_names = ["main.func", "builtins.str"]
      extend_exclude = ["generated", "vendor"]
      force_exclude = true
      cache_dir = ".strict-kwargs-cache"
      debug = true
      fix_synthesized_constructors = true
      output_format = "json"
      "#,
        )
        .expect("valid config");
        assert_eq!(
            config.ignore_names,
            vec!["main.func".to_string(), "builtins.str".to_string()]
        );
        assert_eq!(
            config.extend_exclude,
            vec!["generated".to_string(), "vendor".to_string()]
        );
        assert!(config.force_exclude);
        assert_eq!(
            config.required_version,
            Some("2026.5.19-post.3".to_string())
        );
        assert_eq!(
            config.cache_dir,
            Some(PathBuf::from(".strict-kwargs-cache"))
        );
        assert!(config.debug);
        assert!(config.fix_synthesized_constructors);
        assert_eq!(config.output_format, OutputFormat::Json);
    }

    #[test]
    fn absent_table_is_default_not_an_error() {
        // No `[tool]` table at all.
        assert!(Config::from_pyproject_str("[project]\nname = \"x\"\n")
            .expect("absent table is not an error")
            .ignore_names
            .is_empty());
        // `tool` present but not a table (so it cannot hold our subtable).
        assert!(Config::from_pyproject_str("tool = 5\n")
            .expect("non-table `tool` is not our concern")
            .ignore_names
            .is_empty());
        // `[tool]` present but no `strict_kwargs` key.
        let config =
            Config::from_pyproject_str("[tool.other]\nk = 1\n").expect("absent subtable is fine");
        assert!(config.ignore_names.is_empty());
        assert!(config.extend_exclude.is_empty());
        assert!(!config.force_exclude);
        assert!(config.required_version.is_none());
        assert_eq!(config.cache_dir, None);
        assert!(!config.debug);
        assert!(!config.fix_synthesized_constructors);
        assert_eq!(config.output_format, OutputFormat::Full);
    }

    #[test]
    fn unparsable_toml_is_an_error() {
        let message = Config::from_pyproject_str("this is not toml = =")
            .expect_err("broken TOML must be reported");
        assert!(message.contains("not valid TOML"), "message: {message}");
    }

    #[test]
    fn strict_kwargs_not_a_table_is_an_error() {
        // `strict_kwargs` present but the wrong shape (string, not a table).
        let message = Config::from_pyproject_str("[tool]\nstrict_kwargs = \"oops\"\n")
            .expect_err("wrong-shaped table must be reported");
        assert!(message.contains("must be a table"), "message: {message}");
        assert!(message.contains("string"), "message: {message}");
    }

    #[test]
    fn wrong_value_type_is_an_error() {
        // The table exists but `ignore_names` is a string, not a list — the
        // exact silent-misconfiguration case from issue #55.
        let message =
            Config::from_pyproject_str("[tool.strict_kwargs]\nignore_names = \"not-a-list\"\n")
                .expect_err("wrong value type must be reported");
        assert!(
            message.contains("invalid `[tool.strict_kwargs]` table"),
            "message: {message}"
        );
    }

    #[test]
    fn wrong_fix_synthesized_constructors_type_is_an_error() {
        let message = Config::from_pyproject_str(
            "[tool.strict_kwargs]\nfix_synthesized_constructors = \"yes\"\n",
        )
        .expect_err("wrong value type must be reported");
        assert!(
            message.contains("invalid `[tool.strict_kwargs]` table"),
            "message: {message}"
        );
    }

    #[test]
    fn wrong_file_selection_types_are_errors() {
        for contents in [
            "[tool.strict_kwargs]\nextend_exclude = \"generated\"\n",
            "[tool.strict_kwargs]\nforce_exclude = \"yes\"\n",
        ] {
            let message =
                Config::from_pyproject_str(contents).expect_err("wrong value type must error");
            assert!(
                message.contains("invalid `[tool.strict_kwargs]` table"),
                "message: {message}"
            );
        }
    }

    #[test]
    fn wrong_output_format_value_is_an_error() {
        let message = Config::from_pyproject_str("[tool.strict_kwargs]\noutput_format = \"xml\"\n")
            .expect_err("wrong value must be reported");
        assert!(
            message.contains("invalid `[tool.strict_kwargs]` table"),
            "message: {message}"
        );
    }

    #[test]
    fn wrong_cache_dir_type_is_an_error() {
        let message = Config::from_pyproject_str("[tool.strict_kwargs]\ncache_dir = [\"dir\"]\n")
            .expect_err("wrong value type must be reported");
        assert!(
            message.contains("invalid `[tool.strict_kwargs]` table"),
            "message: {message}"
        );
    }

    #[test]
    fn explicit_full_output_format_is_valid() {
        let config = Config::from_pyproject_str("[tool.strict_kwargs]\noutput_format = \"full\"\n")
            .expect("full output format is valid");
        assert_eq!(config.output_format, OutputFormat::Full);
    }

    #[test]
    fn output_format_is_not_serialized_for_cache_fingerprints() {
        let full = Config {
            output_format: OutputFormat::Full,
            ..Config::default()
        };
        let json = Config {
            output_format: OutputFormat::Json,
            ..Config::default()
        };

        assert_eq!(
            serde_json::to_string(&full).expect("serialize config"),
            serde_json::to_string(&json).expect("serialize config")
        );
    }

    #[test]
    fn exact_required_version_can_match_current_version() {
        let config = Config::from_pyproject_str(&format!(
            "[tool.strict_kwargs]\nrequired_version = \"{}\"\n",
            env!("CARGO_PKG_VERSION")
        ))
        .expect("current exact version is valid");
        assert_eq!(
            config.required_version,
            Some(env!("CARGO_PKG_VERSION").to_string())
        );
    }

    #[test]
    fn minimum_required_version_can_match_current_version() {
        let config = Config::from_pyproject_str(&format!(
            "[tool.strict_kwargs]\nrequired_version = \">={}\"\n",
            env!("CARGO_PKG_VERSION")
        ))
        .expect("current minimum version is valid");
        assert_eq!(
            config.required_version,
            Some(format!(">={}", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn minimum_required_version_can_be_older_than_current_version() {
        validate_required_version(">=2026.5.19-post.2", "2026.5.19-post.3")
            .expect("older minimum is satisfied");
    }

    #[test]
    fn bare_minimum_required_version_accepts_matching_post_release() {
        validate_required_version(">=2026.5.19", "2026.5.19-post.3")
            .expect("post release satisfies its calendar-version minimum");
    }

    #[test]
    fn bare_minimum_required_version_rejects_non_post_prerelease() {
        let message = validate_required_version(">=2026.5.19", "2026.5.19-alpha.1")
            .expect_err("non-post prerelease must stay below the bare release");
        assert!(message.contains("required_version"), "message: {message}");
        assert!(message.contains("not satisfied"), "message: {message}");
    }

    #[test]
    fn bare_minimum_required_version_rejects_post_release_before_required_base() {
        for required_version in [">=2027.5.19", ">=2026.6.19", ">=2026.5.20"] {
            let message = validate_required_version(required_version, "2026.5.19-post.3")
                .expect_err("post release must not satisfy a newer base version");
            assert!(message.contains("required_version"), "message: {message}");
            assert!(message.contains("not satisfied"), "message: {message}");
        }
    }

    #[test]
    fn required_version_rejects_too_old_binary() {
        let message = validate_required_version(">=2026.5.19-post.4", "2026.5.19-post.3")
            .expect_err("too-old binary must be rejected");
        assert!(message.contains("required_version"), "message: {message}");
        assert!(message.contains("not satisfied"), "message: {message}");
        assert!(message.contains("2026.5.19-post.3"), "message: {message}");
    }

    #[test]
    fn required_version_rejects_empty_specifier() {
        let message = validate_required_version("   ", "2026.5.19-post.3")
            .expect_err("empty specifier must be rejected");
        assert!(message.contains("required_version"), "message: {message}");
        assert!(message.contains("exact version"), "message: {message}");
        assert!(message.contains(">="), "message: {message}");
    }

    #[test]
    fn required_version_rejects_wrong_exact_version() {
        let message = validate_required_version("2026.5.19-post.4", "2026.5.19-post.3")
            .expect_err("wrong exact version must be rejected");
        assert!(message.contains("required_version"), "message: {message}");
        assert!(message.contains("not satisfied"), "message: {message}");
        assert!(message.contains("2026.5.19-post.4"), "message: {message}");
    }

    #[test]
    fn required_version_rejects_unsupported_syntax() {
        for specifier in [
            "<2026.5.19",
            ">2026.5.19",
            "=2026.5.19",
            "~=2026.5.19",
            "^2026.5.19",
        ] {
            let message = validate_required_version(specifier, "2026.5.19-post.3")
                .expect_err("unsupported syntax must be rejected");
            assert!(message.contains("unsupported syntax"), "message: {message}");
            assert!(message.contains("exact versions"), "message: {message}");
            assert!(message.contains(">="), "message: {message}");
        }
    }

    #[test]
    fn required_version_rejects_invalid_version() {
        let message = validate_required_version(">=definitely-not-a-version", "2026.5.19-post.3")
            .expect_err("invalid version must be rejected");
        assert!(
            message.contains("must be a valid version"),
            "message: {message}"
        );
    }

    #[test]
    fn required_version_rejects_invalid_exact_version() {
        let message = validate_required_version("definitely-not-a-version", "2026.5.19-post.3")
            .expect_err("invalid exact version must be rejected");
        assert!(
            message.contains("must be a valid version"),
            "message: {message}"
        );
    }

    #[test]
    fn required_version_rejects_invalid_current_version() {
        let message = validate_required_version(">=2026.5.19-post.3", "not-a-version")
            .expect_err("invalid current version must be reported");
        assert!(message.contains("current strict-kwargs version"));
        assert!(
            message.contains("must be a valid version"),
            "message: {message}"
        );
    }

    #[test]
    fn wrong_required_version_type_is_an_error() {
        let message = Config::from_pyproject_str("[tool.strict_kwargs]\nrequired_version = 7\n")
            .expect_err("wrong value type must be reported");
        assert!(
            message.contains("invalid `[tool.strict_kwargs]` table"),
            "message: {message}"
        );
    }

    #[test]
    fn is_ignored_matches_exact_names() {
        let config = Config::from_pyproject_str("[tool.strict_kwargs]\nignore_names = [\"a.b\"]\n")
            .expect("valid config");
        assert!(config.is_ignored("a.b"));
        assert!(!config.is_ignored("a.c"));
    }

    #[test]
    fn load_missing_pyproject_is_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config::load(dir.path()).expect("missing file is not an error");
        assert!(config.ignore_names.is_empty());
        assert!(!config.debug);
        assert!(!config.fix_synthesized_constructors);
    }

    #[test]
    fn load_reads_pyproject_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\nignore_names = [\"pkg.f\"]\n",
        )
        .expect("write");
        let config = Config::load(dir.path()).expect("valid config");
        assert_eq!(config.ignore_names, vec!["pkg.f".to_string()]);
    }

    #[test]
    fn load_reads_cache_dir_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\ncache_dir = \".strict-kwargs-cache\"\n",
        )
        .expect("write");
        let config = Config::load(dir.path()).expect("valid config");
        assert_eq!(
            config.cache_dir,
            Some(PathBuf::from(".strict-kwargs-cache"))
        );
    }

    #[test]
    fn load_reads_fix_synthesized_constructors_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\nfix_synthesized_constructors = true\n",
        )
        .expect("write");
        let config = Config::load(dir.path()).expect("valid config");
        assert!(config.fix_synthesized_constructors);
    }

    #[test]
    fn load_reads_output_format_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\noutput_format = \"github\"\n",
        )
        .expect("write");
        let config = Config::load(dir.path()).expect("valid config");
        assert_eq!(config.output_format, OutputFormat::Github);
    }

    #[test]
    fn load_unreadable_pyproject_is_an_error() {
        // `pyproject.toml` exists (so `is_file()` is true) but is not valid
        // UTF-8, so `read_to_string` fails: a hard error (the file is there
        // but we cannot honour it), not a silent default (issue #55).
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyproject.toml"), [0xff, 0xfe, 0x00]).expect("write");
        let error = Config::load(dir.path()).expect_err("unreadable file must be reported");
        match error {
            CheckError::ConfigInvalid { message, .. } => {
                assert!(message.contains("could not be read"), "message: {message}");
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn load_malformed_pyproject_is_an_error() {
        // Exercises `load`'s `from_pyproject_str` error mapping (path is
        // attached to the message).
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.strict_kwargs]\nignore_names = 7\n",
        )
        .expect("write");
        let error = Config::load(dir.path()).expect_err("bad config must be reported");
        match error {
            CheckError::ConfigInvalid { path, message } => {
                assert!(path.ends_with("pyproject.toml"));
                assert!(
                    message.contains("invalid `[tool.strict_kwargs]` table"),
                    "message: {message}"
                );
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
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
