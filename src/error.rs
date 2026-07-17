//! Error types for ``strict-kwargs``.

use std::path::PathBuf;

use ruff_python_parser::ParseError;

/// Fatal error while reading or parsing a checked file.
#[derive(Debug)]
pub enum CheckError {
    /// Filesystem error while reading a source file.
    Io(std::io::Error),
    /// The Python source could not be parsed.
    Parse(ParseError),
    /// The source nests `()`/`[]`/`{}` deeper than the supported limit
    /// (issue #54). Refused before the recursive parser is reached so a
    /// pathological or hostile file fails cleanly (exit 2) instead of
    /// overflowing the stack and aborting the whole run.
    TooDeeplyNested {
        /// The deepest bracket nesting found in the file.
        depth: usize,
        /// The maximum supported depth (`limits::MAX_NESTING_DEPTH`).
        limit: usize,
    },
    /// A fix would have written syntactically invalid Python; the file was
    /// left untouched rather than corrupted (issue #41).
    FixProducedInvalidSyntax {
        /// The file whose rewrite was rejected.
        path: PathBuf,
    },
    /// A path passed on the command line does not exist. A mistyped target
    /// must not let the run report "clean" (a false pass in CI); like
    /// `ruff`, it is a hard error instead of being silently skipped
    /// (issue #55).
    PathNotFound {
        /// The nonexistent path as given on the command line.
        path: PathBuf,
    },
    /// An explicitly supplied `--project-root` is not an existing directory.
    /// Rejecting it before configuration loading prevents a mistyped root
    /// from silently disabling project configuration (issue #255).
    InvalidProjectRoot {
        /// The invalid project root as given on the command line.
        path: PathBuf,
    },
    /// `pyproject.toml` (or its `[tool.strict_kwargs]` table) could not be
    /// read or parsed, or has the wrong shape/value types. Reported instead
    /// of silently running with defaults, which would hide a misconfigured
    /// `ignore_names` (issue #55).
    ConfigInvalid {
        /// The offending `pyproject.toml`.
        path: PathBuf,
        /// What is wrong with it, phrased to follow the path.
        message: String,
    },
    /// The `ty` type-inference backend is a hard requirement, but no `ty`
    /// executable was found on `PATH`. Failing instead of silently
    /// degrading keeps results deterministic across machines (a run never
    /// resolves fewer calls just because `ty` happens to be missing).
    TyNotFound,
    /// `ty` is on `PATH` but its language server (`ty server`) could not be
    /// started, so the inference backend is unavailable. Fatal for the same
    /// determinism reason as [`Self::TyNotFound`].
    TyServerFailed,
}

impl From<std::io::Error> for CheckError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ParseError> for CheckError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Parse(error) => write!(formatter, "{error}"),
            Self::TooDeeplyNested { depth, limit } => write!(
                formatter,
                "expression nesting too deep ({depth} levels, limit {limit}); \
                 refusing to parse to avoid a stack overflow — split the \
                 expression (this is almost always machine-generated or \
                 hostile input)"
            ),
            Self::FixProducedInvalidSyntax { path } => write!(
                formatter,
                "refusing to write {}: the rewrite would not parse (file left unchanged)",
                path.display()
            ),
            Self::TyNotFound => write!(
                formatter,
                "the `ty` type-inference backend is required but no `ty` \
                 executable was found on PATH; install it (e.g. `uv tool \
                 install ty`)"
            ),
            Self::TyServerFailed => write!(
                formatter,
                "`ty` was found but its language server (`ty server`) could \
                 not be started; the type-inference backend is required"
            ),
            Self::PathNotFound { path } => {
                write!(formatter, "no such file or directory: {}", path.display())
            }
            Self::InvalidProjectRoot { path } => write!(
                formatter,
                "--project-root must be an existing directory: {}",
                path.display()
            ),
            Self::ConfigInvalid { path, message } => {
                write!(formatter, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for CheckError {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use ruff_python_parser::parse_module;

    fn parse_error() -> ParseError {
        // `def f(:` is an unrecoverable syntax error.
        parse_module("def f(:\n").expect_err("expected a parse error")
    }

    #[test]
    fn io_error_converts_and_displays() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let error = CheckError::from(io);
        assert_eq!(error.to_string(), "missing");
        // The `Debug` derive identifies the variant without a `matches!`
        // (whose synthesized non-matching arm would be uncoverable).
        // Also exercises the `std::error::Error` impl.
        let _: &dyn std::error::Error = &error;
        assert!(format!("{error:?}").starts_with("Io("));
    }

    #[test]
    fn parse_error_converts_and_displays() {
        let error = CheckError::from(parse_error());
        assert!(!error.to_string().is_empty());
        assert!(format!("{error:?}").starts_with("Parse("));
    }

    #[test]
    fn too_deeply_nested_reports_depth_limit_and_reassures() {
        let error = CheckError::TooDeeplyNested {
            depth: 5000,
            limit: 1000,
        };
        let message = error.to_string();
        assert!(message.contains("5000"));
        assert!(message.contains("1000"));
        assert!(message.contains("stack overflow"));
        assert!(format!("{error:?}").starts_with("TooDeeplyNested"));
    }

    #[test]
    fn fix_invalid_syntax_displays_path_and_reassures() {
        let error = CheckError::FixProducedInvalidSyntax {
            path: PathBuf::from("pkg/m.py"),
        };
        let message = error.to_string();
        assert!(message.contains("pkg/m.py"));
        assert!(message.contains("would not parse"));
        assert!(message.contains("left unchanged"));
        assert!(format!("{error:?}").starts_with("FixProducedInvalidSyntax"));
    }

    #[test]
    fn ty_not_found_explains_the_requirement() {
        let error = CheckError::TyNotFound;
        let message = error.to_string();
        assert!(message.contains("required"));
        assert!(message.contains("PATH"));
        // Points at the documented install path.
        assert!(message.contains("uv tool install ty"));
        assert_eq!(format!("{error:?}"), "TyNotFound");
    }

    #[test]
    fn path_not_found_names_the_path() {
        let error = CheckError::PathNotFound {
            path: PathBuf::from("typo_does_not_exist.py"),
        };
        let message = error.to_string();
        assert!(message.contains("no such file or directory"));
        assert!(message.contains("typo_does_not_exist.py"));
        assert!(format!("{error:?}").starts_with("PathNotFound"));
    }

    #[test]
    fn invalid_project_root_names_the_path_and_requirement() {
        let error = CheckError::InvalidProjectRoot {
            path: PathBuf::from("not-a-project-root"),
        };
        let message = error.to_string();
        assert!(message.contains("--project-root"));
        assert!(message.contains("existing directory"));
        assert!(message.contains("not-a-project-root"));
        assert!(format!("{error:?}").starts_with("InvalidProjectRoot"));
    }

    #[test]
    fn config_invalid_shows_path_then_reason() {
        let error = CheckError::ConfigInvalid {
            path: PathBuf::from("pyproject.toml"),
            message: "`[tool.strict_kwargs]` must be a table, found a string".to_owned(),
        };
        let message = error.to_string();
        assert!(message.starts_with("pyproject.toml: "));
        assert!(message.contains("must be a table"));
        assert!(format!("{error:?}").starts_with("ConfigInvalid"));
    }

    #[test]
    fn ty_server_failed_explains_the_requirement() {
        let error = CheckError::TyServerFailed;
        let message = error.to_string();
        assert!(message.contains("ty server"));
        assert!(message.contains("required"));
        assert_eq!(format!("{error:?}"), "TyServerFailed");
    }
}
