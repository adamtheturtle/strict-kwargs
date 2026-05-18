//! Diagnostic reported for a call site.

use std::path::PathBuf;

/// A reported violation: a call site with too many positional arguments.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// File containing the offending call.
    pub path: PathBuf,
    /// 1-based line of the call.
    pub line: usize,
    /// 1-based column of the call.
    pub column: usize,
    /// Fully-qualified name of the called function.
    pub callee: String,
    /// Number of positional arguments passed.
    pub positional_count: usize,
    /// Maximum positional arguments the callee allows.
    pub max_positional: usize,
}

impl Diagnostic {
    /// Human-readable description of the violation.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "Too many positional arguments for {} (got {}, maximum {})",
            self.callee, self.positional_count, self.max_positional
        )
    }

    /// `path:line:column: error: <message>` line for terminal output.
    #[must_use]
    pub fn display_path(&self) -> String {
        format!(
            "{}:{}:{}: error: {}",
            self.path.display(),
            self.line,
            self.column,
            self.message()
        )
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn sample() -> Diagnostic {
        Diagnostic {
            path: PathBuf::from("pkg/mod.py"),
            line: 7,
            column: 3,
            callee: "pkg.mod.func".to_string(),
            positional_count: 4,
            max_positional: 2,
        }
    }

    #[test]
    fn message_and_display_path_render() {
        let diagnostic = sample();
        assert_eq!(
            diagnostic.message(),
            "Too many positional arguments for pkg.mod.func (got 4, maximum 2)"
        );
        assert_eq!(
            diagnostic.display_path(),
            "pkg/mod.py:7:3: error: \
             Too many positional arguments for pkg.mod.func (got 4, maximum 2)"
        );
    }

    #[test]
    fn derives_are_exercised() {
        let diagnostic = sample();
        let clone = diagnostic.clone();
        assert_eq!(diagnostic, clone);
        let mut other = sample();
        other.line = 8;
        assert_ne!(diagnostic, other);
        assert!(format!("{diagnostic:?}").contains("pkg.mod.func"));
    }
}
