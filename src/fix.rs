//! Auto-fix: rewrite positional call arguments to keyword arguments.
//!
//! Violation detection is shared with the checker (see [`crate::check`]); this
//! module only models the resulting source edits and renders them, either
//! applied in place or as a unified diff.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use owo_colors::OwoColorize as _;

/// Non-default fix categories a caller may opt into explicitly.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct FixOptIns {
    /// Rewrite dataclass and `NamedTuple` constructors whose signatures were
    /// synthesized from class fields.
    pub synthesized_constructors: bool,
    /// Rewrite overloaded calls when `ty` selects one precise overload arm.
    pub unambiguous_overloads: bool,
}

/// Why a detected violation was deliberately left untouched by the fixer.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum DeclinedFixReason {
    /// A constructor signature was synthesized from dataclass / namedtuple
    /// fields; safe mode keeps those calls unchanged.
    SynthesizedConstructor,
    /// An overloaded call could not be narrowed to one safe parameter-name
    /// mapping at the call site.
    UnresolvedOverload,
    /// `ty` reported more than one callable hover signature.
    AmbiguousTyHover,
    /// `ty` could only resolve the call via goto-definition, not a concrete
    /// call-site hover signature suitable for rewriting.
    TyDefinitionOnly,
    /// An overloaded call was narrowed to one rewriteable arm; default `fix`
    /// keeps overload-derived parameter mappings opt-in.
    UnambiguousOverload,
    /// The call uses `*args` or `**kwargs`, so local argument positions are not
    /// enough to build a sound keyword rewrite.
    UnsafeCallSiteUnpacking,
    /// The resolved signature or argument shape cannot be represented safely
    /// as a keyword rewrite.
    UnsupportedSignatureShape,
}

impl DeclinedFixReason {
    pub(crate) const ORDERED: [Self; 7] = [
        Self::SynthesizedConstructor,
        Self::UnresolvedOverload,
        Self::AmbiguousTyHover,
        Self::TyDefinitionOnly,
        Self::UnambiguousOverload,
        Self::UnsafeCallSiteUnpacking,
        Self::UnsupportedSignatureShape,
    ];

    /// Practical label shown in CLI output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SynthesizedConstructor => "synthesized constructor",
            Self::UnresolvedOverload => "unresolved overload",
            Self::AmbiguousTyHover => "ambiguous ty hover",
            Self::TyDefinitionOnly => "ty/goto-definition-only resolution",
            Self::UnambiguousOverload => "unambiguous overload",
            Self::UnsafeCallSiteUnpacking => "unsafe call-site unpacking",
            Self::UnsupportedSignatureShape => "unsupported signature shape",
        }
    }
}

/// Count for one declined fix category.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeclinedFixReasonCount {
    /// Declined rewrite category.
    pub reason: DeclinedFixReason,
    /// Number of violations declined for this category.
    pub count: usize,
}

pub fn declined_fix_reason_counts(reasons: &[DeclinedFixReason]) -> Vec<DeclinedFixReasonCount> {
    DeclinedFixReason::ORDERED
        .into_iter()
        .filter_map(|reason| {
            let count = reasons.iter().filter(|&&r| r == reason).count();
            (count > 0).then_some(DeclinedFixReasonCount { reason, count })
        })
        .collect()
}

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

/// What a fix run produced: the files it would rewrite plus the number of
/// violations it detected but deliberately left untouched.
///
/// `declined` is every violation the checker would report (built-in *and*
/// `ty`-resolved) minus the ones rewritten: overloaded callees, synthesized
/// constructors, ambiguous `ty` displays, and call-site unpacking that makes
/// a rewrite unsafe. Surfacing it makes `fix` then `check` predictable — a
/// non-zero count is exactly what a subsequent `strict-kwargs` run (with the
/// same `--python`) will still report (issue #42).
#[derive(Debug, Clone)]
pub struct FixOutcome {
    /// Files the fixer would rewrite (empty when there is nothing to write).
    pub files: Vec<FileFix>,
    /// Violations detected but not rewritten.
    pub declined: usize,
    /// Violations detected but not rewritten, grouped by practical reason.
    pub declined_reasons: Vec<DeclinedFixReasonCount>,
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
///
/// When `color` is `true`, removal lines are red, addition lines are green, and
/// hunk headers are bold — suitable for a terminal that supports ANSI codes.
/// Pass `false` when stdout is not a TTY or when `NO_COLOR` is set.
#[must_use]
pub fn unified_diff(path: &Path, original: &str, fixed: &str, color: bool) -> String {
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
    let mut lines: Vec<String> = if color {
        vec![
            format!("{}", format!("--- a/{display}").bold()),
            format!("{}", format!("+++ b/{display}").bold()),
        ]
    } else {
        vec![format!("--- a/{display}"), format!("+++ b/{display}")]
    };
    for (first, last) in groups {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT).min(line_count - 1);
        let len = end - start + 1;
        let hunk = format!("@@ -{0},{len} +{0},{len} @@", start + 1);
        lines.push(if color {
            format!("{}", hunk.bold())
        } else {
            hunk
        });
        for i in start..=end {
            if before[i] == after[i] {
                lines.push(format!(" {}", before[i]));
            } else {
                let removal = format!("-{}", before[i]);
                let addition = format!("+{}", after[i]);
                lines.push(if color {
                    format!("{}", removal.red())
                } else {
                    removal
                });
                lines.push(if color {
                    format!("{}", addition.green())
                } else {
                    addition
                });
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
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
    fn declined_fix_reason_counts_are_ordered_and_labeled() {
        let reasons = [
            DeclinedFixReason::UnsupportedSignatureShape,
            DeclinedFixReason::UnresolvedOverload,
            DeclinedFixReason::UnsafeCallSiteUnpacking,
            DeclinedFixReason::UnresolvedOverload,
            DeclinedFixReason::TyDefinitionOnly,
            DeclinedFixReason::AmbiguousTyHover,
            DeclinedFixReason::SynthesizedConstructor,
        ];

        assert_eq!(
            declined_fix_reason_counts(&reasons),
            vec![
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::SynthesizedConstructor,
                    count: 1,
                },
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::UnresolvedOverload,
                    count: 2,
                },
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::AmbiguousTyHover,
                    count: 1,
                },
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::TyDefinitionOnly,
                    count: 1,
                },
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::UnsafeCallSiteUnpacking,
                    count: 1,
                },
                DeclinedFixReasonCount {
                    reason: DeclinedFixReason::UnsupportedSignatureShape,
                    count: 1,
                },
            ]
        );
        assert_eq!(
            reasons.map(DeclinedFixReason::label),
            [
                "unsupported signature shape",
                "unresolved overload",
                "unsafe call-site unpacking",
                "unresolved overload",
                "ty/goto-definition-only resolution",
                "ambiguous ty hover",
                "synthesized constructor",
            ]
        );
    }

    #[test]
    fn unified_diff_empty_when_unchanged() {
        let path = Path::new("m.py");
        assert!(unified_diff(path, "a\nb\n", "a\nb\n", false).is_empty());
    }

    #[test]
    fn unified_diff_single_hunk_with_context_clamped() {
        let original = "l1\nl2\nf(a)\nl4\nl5\n";
        let fixed = "l1\nl2\nf(x=a)\nl4\nl5\n";
        let diff = unified_diff(Path::new("pkg/m.py"), original, fixed, false);
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
        let diff = unified_diff(Path::new("m.py"), original, fixed, false);
        assert_eq!(diff.matches("@@").count(), 2); // one hunk header (`@@ ... @@`)
        assert!(diff.contains("-A\n+A1\n"));
        assert!(diff.contains("-B\n+B1\n"));
    }

    #[test]
    fn unified_diff_color_contains_ansi_codes() {
        let original = "f(a)\n";
        let fixed = "f(x=a)\n";
        let diff = unified_diff(Path::new("m.py"), original, fixed, true);
        // ANSI escape sequences are present in colored output.
        assert!(
            diff.contains("\x1b["),
            "expected ANSI codes in colored diff"
        );
        // Structural markers still present (possibly wrapped in color codes).
        assert!(diff.contains("---"));
        assert!(diff.contains("+++"));
        assert!(diff.contains("@@"));
        assert!(diff.contains("f(a)"));
        assert!(diff.contains("f(x=a)"));
    }

    #[test]
    fn unified_diff_splits_distant_changes_into_two_hunks() {
        let mut before = String::from("X\n");
        for _ in 0..20 {
            before.push_str("ctx\n");
        }
        before.push_str("Y\n");
        let after = before.replace("X\n", "X1\n").replace("Y\n", "Y1\n");
        let diff = unified_diff(Path::new("m.py"), &before, &after, false);
        // Two separate hunks => two `@@ ... @@` headers.
        assert_eq!(diff.matches("@@ -").count(), 2);
        assert!(diff.contains("-X\n+X1\n"));
        assert!(diff.contains("-Y\n+Y1\n"));
    }
}
