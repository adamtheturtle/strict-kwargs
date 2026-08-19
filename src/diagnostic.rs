//! Diagnostic reported for a call site.

use std::path::PathBuf;

/// What a [`Diagnostic`] reports, and the rule-specific detail behind it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticKind {
    /// `KW001`: a call site with too many positional arguments.
    TooManyPositional {
        /// Fully-qualified name of the called function.
        callee: String,
        /// Number of positional arguments passed.
        positional_count: usize,
        /// Maximum positional arguments the callee allows.
        max_positional: usize,
    },
    /// `KW002`: a `# noqa: KW001` directive that suppressed nothing.
    UnusedNoqa,
}

/// A reported finding at a source position.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// File containing the finding.
    pub path: PathBuf,
    /// 1-based line of the finding.
    pub line: usize,
    /// 1-based column of the finding.
    pub column: usize,
    /// Which rule fired, with its rule-specific detail.
    pub kind: DiagnosticKind,
}

impl Diagnostic {
    /// Rule code for too many positional arguments.
    pub const CODE: &'static str = "KW001";

    /// Rule code for an unused `# noqa: KW001` directive.
    pub const UNUSED_NOQA_CODE: &'static str = "KW002";

    /// A `KW001` diagnostic for a call that passes too many positionals.
    #[must_use]
    pub const fn too_many_positional(
        path: PathBuf,
        line: usize,
        column: usize,
        callee: String,
        positional_count: usize,
        max_positional: usize,
    ) -> Self {
        Self {
            path,
            line,
            column,
            kind: DiagnosticKind::TooManyPositional {
                callee,
                positional_count,
                max_positional,
            },
        }
    }

    /// A `KW002` diagnostic for a `# noqa: KW001` that suppressed nothing.
    #[must_use]
    pub const fn unused_noqa(path: PathBuf, line: usize, column: usize) -> Self {
        Self {
            path,
            line,
            column,
            kind: DiagnosticKind::UnusedNoqa,
        }
    }

    /// Rule code shown in CLI output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self.kind {
            DiagnosticKind::TooManyPositional { .. } => Self::CODE,
            DiagnosticKind::UnusedNoqa => Self::UNUSED_NOQA_CODE,
        }
    }

    /// Fully-qualified name of the called function, for the rules that name
    /// one.
    #[must_use]
    pub fn callee(&self) -> Option<&str> {
        match &self.kind {
            DiagnosticKind::TooManyPositional { callee, .. } => Some(callee),
            DiagnosticKind::UnusedNoqa => None,
        }
    }

    /// Human-readable description of the finding.
    #[must_use]
    pub fn message(&self) -> String {
        match &self.kind {
            DiagnosticKind::TooManyPositional {
                callee,
                positional_count,
                max_positional,
            } => format!(
                "Too many positional arguments for {callee} \
                 (got {positional_count}, maximum {max_positional})"
            ),
            DiagnosticKind::UnusedNoqa => {
                format!("Unused `noqa` directive (unused: `{}`)", Self::CODE)
            }
        }
    }

    /// `path:line:column: <code> <message>` line for terminal output.
    #[must_use]
    pub fn display_path(&self) -> String {
        format!(
            "{}:{}:{}: {} {}",
            self.path.display(),
            self.line,
            self.column,
            self.code(),
            self.message()
        )
    }

    /// GitHub Actions annotation line for CI-native output.
    #[must_use]
    pub fn github_annotation(&self) -> String {
        format!(
            "::error file={},line={},col={},title={}::{}",
            escape_github_property(&self.path.display().to_string()),
            self.line,
            self.column,
            self.code(),
            escape_github_data(&self.message())
        )
    }
}

fn escape_github_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_github_property(value: &str) -> String {
    escape_github_data(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn sample() -> Diagnostic {
        Diagnostic::too_many_positional(
            PathBuf::from("pkg/mod.py"),
            7,
            3,
            "pkg.mod.func".to_string(),
            4,
            2,
        )
    }

    #[test]
    fn message_and_display_path_render() {
        let diagnostic = sample();
        assert_eq!(diagnostic.code(), "KW001");
        assert_eq!(diagnostic.callee(), Some("pkg.mod.func"));
        assert_eq!(
            diagnostic.message(),
            "Too many positional arguments for pkg.mod.func (got 4, maximum 2)"
        );
        assert_eq!(
            diagnostic.display_path(),
            "pkg/mod.py:7:3: KW001 \
             Too many positional arguments for pkg.mod.func (got 4, maximum 2)"
        );
        assert_eq!(
            diagnostic.github_annotation(),
            "::error file=pkg/mod.py,line=7,col=3,title=KW001::\
             Too many positional arguments for pkg.mod.func (got 4, maximum 2)"
        );
    }

    #[test]
    fn github_annotation_escapes_workflow_command_syntax() {
        let diagnostic = Diagnostic::too_many_positional(
            PathBuf::from("pkg/a,b%:mod.py"),
            7,
            3,
            "pkg.mod.f%\n".to_string(),
            4,
            2,
        );
        assert_eq!(
            diagnostic.github_annotation(),
            "::error file=pkg/a%2Cb%25%3Amod.py,line=7,col=3,title=KW001::\
             Too many positional arguments for pkg.mod.f%25%0A (got 4, maximum 2)"
        );
    }

    #[test]
    fn unused_noqa_renders_its_own_code_and_message() {
        let diagnostic = Diagnostic::unused_noqa(PathBuf::from("pkg/mod.py"), 7, 15);
        assert_eq!(diagnostic.code(), "KW002");
        assert_eq!(diagnostic.callee(), None);
        assert_eq!(
            diagnostic.message(),
            "Unused `noqa` directive (unused: `KW001`)"
        );
        assert_eq!(
            diagnostic.display_path(),
            "pkg/mod.py:7:15: KW002 Unused `noqa` directive (unused: `KW001`)"
        );
        assert_eq!(
            diagnostic.github_annotation(),
            "::error file=pkg/mod.py,line=7,col=15,title=KW002::\
             Unused `noqa` directive (unused: `KW001`)"
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
