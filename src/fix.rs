//! Auto-fix: rewrite positional call arguments to keyword arguments.
//!
//! Violation detection is shared with the checker (see [`crate::check`]); this
//! module only models the resulting source edits and renders them, either
//! applied in place or as a unified diff.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

/// A single source insertion: `text` is spliced in at byte offset `at`.
///
/// The fixer only ever *inserts* (`name=` before an argument), so it never
/// changes the file's line count — a property the diff renderer relies on.
#[derive(Debug, Clone)]
pub struct Insertion {
    pub at: usize,
    pub text: String,
}

/// A file the fixer would rewrite.
#[derive(Debug, Clone)]
pub struct FileFix {
    /// Path of the rewritten file.
    pub path: PathBuf,
    /// Original source.
    pub original: String,
    /// Source after applying every fix.
    pub fixed: String,
    /// Number of call sites rewritten.
    pub count: usize,
}

/// Apply `insertions` to `source`, returning the rewritten text.
///
/// Edits are applied from the highest offset down so earlier offsets stay
/// valid as the string grows.
pub fn apply_insertions(source: &str, insertions: &[Insertion]) -> String {
    let mut ordered: Vec<&Insertion> = insertions.iter().collect();
    ordered.sort_by_key(|insertion| Reverse(insertion.at));
    let mut out = source.to_string();
    for insertion in ordered {
        out.insert_str(insertion.at, &insertion.text);
    }
    out
}

/// Render a unified diff between `original` and `fixed`.
///
/// The fixer never adds or removes newlines, so the two share a line count and
/// every change is an in-place line modification — that lets us pair lines by
/// index instead of running a full diff algorithm.
#[must_use]
pub fn unified_diff(path: &Path, original: &str, fixed: &str) -> String {
    const CONTEXT: usize = 3;

    let before: Vec<&str> = original.split('\n').collect();
    let after: Vec<&str> = fixed.split('\n').collect();
    let line_count = before.len().min(after.len());
    let changed: Vec<usize> = (0..line_count).filter(|&i| before[i] != after[i]).collect();
    if changed.is_empty() {
        return String::new();
    }

    // Group changed lines into hunks, merging groups whose context windows
    // would touch or overlap.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &line in &changed {
        match groups.last_mut() {
            Some(last) if line <= last.1 + 2 * CONTEXT + 1 => last.1 = line,
            _ => groups.push((line, line)),
        }
    }

    let display = path.display();
    let mut lines: Vec<String> = vec![format!("--- a/{display}"), format!("+++ b/{display}")];
    for (first, last) in groups {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT).min(line_count - 1);
        let len = end - start + 1;
        lines.push(format!("@@ -{0},{len} +{0},{len} @@", start + 1));
        for i in start..=end {
            if before[i] == after[i] {
                lines.push(format!(" {}", before[i]));
            } else {
                lines.push(format!("-{}", before[i]));
                lines.push(format!("+{}", after[i]));
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_insertions_splices_high_to_low() {
        let out = apply_insertions(
            "f(a, b)",
            &[
                Insertion {
                    at: 2,
                    text: "x=".to_string(),
                },
                Insertion {
                    at: 5,
                    text: "y=".to_string(),
                },
            ],
        );
        assert_eq!(out, "f(x=a, y=b)");
    }

    #[test]
    fn unified_diff_empty_when_unchanged() {
        let path = Path::new("m.py");
        assert!(unified_diff(path, "a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn unified_diff_single_hunk_with_context_clamped() {
        let original = "l1\nl2\nf(a)\nl4\nl5\n";
        let fixed = "l1\nl2\nf(x=a)\nl4\nl5\n";
        let diff = unified_diff(Path::new("pkg/m.py"), original, fixed);
        assert_eq!(
            diff,
            "--- a/pkg/m.py\n\
             +++ b/pkg/m.py\n\
             @@ -1,6 +1,6 @@\n\
             \u{20}l1\n\
             \u{20}l2\n\
             -f(a)\n\
             +f(x=a)\n\
             \u{20}l4\n\
             \u{20}l5\n\
             \u{20}\n"
        );
        // Context window clamps at the start (`saturating_sub`) and end
        // (`min(line_count - 1)`).
        assert!(diff.starts_with("--- a/pkg/m.py\n+++ b/pkg/m.py\n@@ -1,6"));
    }

    #[test]
    fn unified_diff_merges_near_changes_into_one_hunk() {
        // Two changed lines 4 apart: within `2*CONTEXT+1`, so one hunk.
        let original = "c0\nc1\nA\nc3\nc4\nB\nc6\nc7\n";
        let fixed = "c0\nc1\nA1\nc3\nc4\nB1\nc6\nc7\n";
        let diff = unified_diff(Path::new("m.py"), original, fixed);
        assert_eq!(diff.matches("@@").count(), 2); // one hunk header (`@@ ... @@`)
        assert!(diff.contains("-A\n+A1\n"));
        assert!(diff.contains("-B\n+B1\n"));
    }

    #[test]
    fn unified_diff_splits_distant_changes_into_two_hunks() {
        let mut before = String::from("X\n");
        for _ in 0..20 {
            before.push_str("ctx\n");
        }
        before.push_str("Y\n");
        let after = before.replace("X\n", "X1\n").replace("Y\n", "Y1\n");
        let diff = unified_diff(Path::new("m.py"), &before, &after);
        // Two separate hunks => two `@@ ... @@` headers.
        assert_eq!(diff.matches("@@ -").count(), 2);
        assert!(diff.contains("-X\n+X1\n"));
        assert!(diff.contains("-Y\n+Y1\n"));
    }
}
