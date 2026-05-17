//! Diagnostic reported for a call site.

use std::path::PathBuf;

/// A reported violation: a call site with too many positional arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
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
