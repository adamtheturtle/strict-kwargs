//! Diagnostic reported for a call site.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
  pub path: PathBuf,
  pub line: usize,
  pub column: usize,
  pub callee: String,
  pub positional_count: usize,
  pub max_positional: usize,
}

impl Diagnostic {
  pub fn message(&self) -> String {
    format!(
      "Too many positional arguments for {} (got {}, maximum {})",
      self.callee, self.positional_count, self.max_positional
    )
  }

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
