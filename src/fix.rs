//! Auto-fix: rewrite positional call arguments to keyword arguments.
//!
//! Violation detection is shared with the checker (see [`crate::check`]); this
//! module only models the resulting source edits and renders them, either
//! applied in place or as a unified diff.

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use owo_colors::OwoColorize as _;
use serde::{Deserialize, Serialize};

const FIX_JOURNAL_PREFIX: &str = ".strict-kwargs-fix-journal-";

#[derive(Debug, Serialize, Deserialize)]
struct FixJournal {
    committed: bool,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalEntry {
    destination: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

/// Fix categories a caller may opt into explicitly.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct FixOptIns {
    /// Rewrite dataclass and `NamedTuple` constructors whose signatures were
    /// synthesized from class fields.
    pub synthesized_constructors: bool,
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
    /// The call uses `*args` or `**kwargs`, so local argument positions are not
    /// enough to build a sound keyword rewrite.
    UnsafeCallSiteUnpacking,
    /// The resolved signature or argument shape cannot be represented safely
    /// as a keyword rewrite.
    UnsupportedSignatureShape,
}

impl DeclinedFixReason {
    pub(crate) const ORDERED: [Self; 6] = [
        Self::SynthesizedConstructor,
        Self::UnresolvedOverload,
        Self::AmbiguousTyHover,
        Self::TyDefinitionOnly,
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
#[derive(Debug, Clone, Eq, PartialEq)]
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

impl FileFix {
    /// Write this fix while preserving the source file's declared encoding.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the original file cannot be read, the fixed
    /// text cannot be represented in its original encoding, the file changed
    /// after this fix was planned, or the rewritten bytes cannot be written.
    pub fn write_preserving_encoding(&self) -> std::io::Result<()> {
        let original_bytes = std::fs::read(&self.path)?;
        ensure_source_is_current(self, &original_bytes)?;
        let fixed_bytes = crate::source::encode_python_source(&original_bytes, &self.fixed)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
        std::fs::write(&self.path, fixed_bytes)
    }
}

/// Write all fixes as one recoverable operation.
///
/// Every source is read and encoded before the first destination is changed.
/// If a later write fails, previously written files are restored from their
/// original bytes before the error is returned.
///
/// # Errors
///
/// Returns an I/O error when a source cannot be read or encoded, or when a
/// destination cannot be written.
pub fn write_all_preserving_encoding(fixes: &[FileFix]) -> std::io::Result<()> {
    recover_fix_transactions(fixes)?;
    let prepared: Vec<(PathBuf, Vec<u8>, Vec<u8>)> = fixes
        .iter()
        .map(|fix| {
            let original = std::fs::read(&fix.path)?;
            ensure_source_is_current(fix, &original)?;
            let fixed_bytes = crate::source::encode_python_source(&original, &fix.fixed)
                .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
            Ok((fix.path.clone(), original, fixed_bytes))
        })
        .collect::<std::io::Result<_>>()?;

    if prepared.is_empty() {
        return Ok(());
    }
    commit_prepared_fixes(&prepared)
}

#[cfg_attr(coverage, coverage(off))]
fn fix_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg_attr(coverage, coverage(off))]
fn commit_prepared_fixes(prepared: &[(PathBuf, Vec<u8>, Vec<u8>)]) -> std::io::Result<()> {
    let transaction = format!("{}-{}", std::process::id(), unique_transaction_suffix());
    let mut entries: Vec<JournalEntry> = Vec::with_capacity(prepared.len());
    for (index, (destination, _, fixed)) in prepared.iter().enumerate() {
        let parent = fix_parent(destination);
        let staged = parent.join(format!(".strict-kwargs-fix-{transaction}-{index}.new"));
        let backup = parent.join(format!(".strict-kwargs-fix-{transaction}-{index}.old"));
        let stage_result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(fixed)?;
            file.set_permissions(std::fs::metadata(destination)?.permissions())?;
            file.sync_all()
        })();
        if let Err(error) = stage_result {
            let _ = std::fs::remove_file(&staged);
            for entry in &entries {
                let _ = std::fs::remove_file(&entry.staged);
            }
            return Err(error);
        }
        entries.push(JournalEntry {
            destination: destination.clone(),
            staged,
            backup,
        });
    }

    let first_parent = fix_parent(&prepared[0].0);
    let journal_path = first_parent.join(format!("{FIX_JOURNAL_PREFIX}{transaction}.json"));
    let mut journal = FixJournal {
        committed: false,
        entries,
    };
    if let Err(error) = write_journal(&journal_path, &journal) {
        for entry in &journal.entries {
            let _ = std::fs::remove_file(&entry.staged);
        }
        return Err(error);
    }

    let result = (|| {
        for entry in &journal.entries {
            std::fs::rename(&entry.destination, &entry.backup)?;
            std::fs::rename(&entry.staged, &entry.destination)?;
            sync_parent(&entry.destination)?;
        }
        journal.committed = true;
        write_journal(&journal_path, &journal)?;
        cleanup_committed_transaction(&journal, &journal_path)
    })();
    if result.is_err() {
        let _ = recover_journal(&journal_path);
    }
    result
}

#[cfg_attr(coverage, coverage(off))]
fn unique_transaction_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

#[cfg_attr(coverage, coverage(off))]
fn write_journal(path: &Path, journal: &FixJournal) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    sync_parent(path)
}

#[cfg_attr(coverage, coverage(off))]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(fix_parent(path))?.sync_all()
}

#[cfg_attr(coverage, coverage(off))]
fn recover_fix_transactions(fixes: &[FileFix]) -> std::io::Result<()> {
    let mut parents = fixes
        .iter()
        .map(|fix| fix_parent(&fix.path).to_path_buf())
        .collect::<Vec<_>>();
    parents.sort();
    parents.dedup();
    for parent in parents {
        for entry in std::fs::read_dir(parent)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(FIX_JOURNAL_PREFIX))
            {
                recover_journal(&path)?;
            }
        }
    }
    Ok(())
}

#[cfg_attr(coverage, coverage(off))]
fn recover_journal(path: &Path) -> std::io::Result<()> {
    let journal: FixJournal = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if journal.committed {
        return cleanup_committed_transaction(&journal, path);
    }
    for entry in journal.entries.iter().rev() {
        if entry.backup.exists() {
            if entry.destination.exists() {
                std::fs::remove_file(&entry.destination)?;
            }
            std::fs::rename(&entry.backup, &entry.destination)?;
            sync_parent(&entry.destination)?;
        }
        if entry.staged.exists() {
            std::fs::remove_file(&entry.staged)?;
        }
    }
    std::fs::remove_file(path)?;
    sync_parent(path)
}

#[cfg_attr(coverage, coverage(off))]
fn cleanup_committed_transaction(journal: &FixJournal, path: &Path) -> std::io::Result<()> {
    for entry in &journal.entries {
        if entry.backup.exists() {
            std::fs::remove_file(&entry.backup)?;
        }
        if entry.staged.exists() {
            std::fs::remove_file(&entry.staged)?;
        }
    }
    std::fs::remove_file(path)?;
    sync_parent(path)
}

#[cfg_attr(coverage, coverage(off))]
fn ensure_source_is_current(fix: &FileFix, bytes: &[u8]) -> std::io::Result<()> {
    match crate::source::decode_python_source(bytes) {
        crate::source::Source::Decoded(current) if current == fix.original => Ok(()),
        crate::source::Source::Decoded(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "refusing to overwrite {}: source changed after fixes were planned",
                fix.path.display()
            ),
        )),
        crate::source::Source::Undecodable(reason) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "refusing to overwrite {}: current source cannot be decoded: {reason}",
                fix.path.display()
            ),
        )),
    }
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

fn git_diff_path(prefix: &str, path: &Path) -> String {
    let mut bytes = prefix.as_bytes().to_vec();
    bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
    if bytes
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte) && !matches!(byte, b'"' | b'\\'))
    {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let mut quoted = String::from("\"");
    for byte in bytes {
        match byte {
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            b'\t' => quoted.push_str("\\t"),
            b'\\' => quoted.push_str("\\\\"),
            b'"' => quoted.push_str("\\\""),
            b' '..=b'~' => quoted.push(char::from(byte)),
            _ => {
                let _ = write!(quoted, "\\{byte:03o}");
            }
        }
    }
    quoted.push('"');
    quoted
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

    let old_path = git_diff_path("a/", path);
    let new_path = git_diff_path("b/", path);
    let mut lines: Vec<String> = if color {
        vec![
            format!("{}", format!("--- {old_path}").bold()),
            format!("{}", format!("+++ {new_path}").bold()),
        ]
    } else {
        vec![format!("--- {old_path}"), format!("+++ {new_path}")]
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
    fn write_preserving_encoding_rejects_unrepresentable_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy.py");
        std::fs::write(&path, b"# coding: ascii\nx = 1\n").expect("write source");
        let fix = FileFix {
            path,
            original: "# coding: ascii\nx = 1\n".to_owned(),
            fixed: "# coding: ascii\nx = '\u{e9}'\n".to_owned(),
            count: 1,
        };

        let error = fix
            .write_preserving_encoding()
            .expect_err("ascii cannot represent the rewritten text");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_preserving_encoding_updates_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("source.py");
        std::fs::write(&path, "before\n").expect("write source");
        let fix = FileFix {
            path: path.clone(),
            original: "before\n".to_owned(),
            fixed: "after\n".to_owned(),
            count: 1,
        };

        fix.write_preserving_encoding().expect("write fix");
        assert_eq!(
            std::fs::read_to_string(path).expect("read source"),
            "after\n"
        );
    }

    #[test]
    fn write_preserving_encoding_rejects_a_stale_fix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("source.py");
        std::fs::write(&path, "edited independently\n").expect("write source");
        let fix = FileFix {
            path: path.clone(),
            original: "analyzed source\n".to_owned(),
            fixed: "fixed analyzed source\n".to_owned(),
            count: 1,
        };

        let error = fix
            .write_preserving_encoding()
            .expect_err("stale fix must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("changed after fixes were planned"));
        assert_eq!(
            std::fs::read_to_string(path).expect("read source"),
            "edited independently\n"
        );
    }

    #[test]
    fn write_all_preflights_every_file_before_changing_any() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.py");
        let invalid_target = dir.path().join("directory");
        std::fs::write(&first, "before\n").expect("write first");
        std::fs::create_dir(&invalid_target).expect("mkdir");
        let fixes = vec![
            FileFix {
                path: first.clone(),
                original: "before\n".to_owned(),
                fixed: "after\n".to_owned(),
                count: 1,
            },
            FileFix {
                path: invalid_target,
                original: String::new(),
                fixed: String::new(),
                count: 1,
            },
        ];

        assert!(write_all_preserving_encoding(&fixes).is_err());
        assert_eq!(
            std::fs::read_to_string(first).expect("read first"),
            "before\n"
        );
    }

    #[test]
    fn write_all_rejects_unrepresentable_text_before_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy.py");
        std::fs::write(&path, b"# coding: ascii\nx = 1\n").expect("write source");
        let fixes = [FileFix {
            path: path.clone(),
            original: "# coding: ascii\nx = 1\n".to_owned(),
            fixed: "# coding: ascii\nx = '\u{e9}'\n".to_owned(),
            count: 1,
        }];

        let error = write_all_preserving_encoding(&fixes)
            .expect_err("ascii cannot represent the rewritten text");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(path).expect("read source"),
            b"# coding: ascii\nx = 1\n"
        );
    }

    #[test]
    fn write_all_rejects_a_stale_fix_before_writing_any_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.py");
        let stale = dir.path().join("stale.py");
        std::fs::write(&first, "first before\n").expect("write first");
        std::fs::write(&stale, "independent edit\n").expect("write stale");
        let fixes = [
            FileFix {
                path: first.clone(),
                original: "first before\n".to_owned(),
                fixed: "first after\n".to_owned(),
                count: 1,
            },
            FileFix {
                path: stale.clone(),
                original: "stale before\n".to_owned(),
                fixed: "stale after\n".to_owned(),
                count: 1,
            },
        ];

        let error = write_all_preserving_encoding(&fixes)
            .expect_err("stale fix must fail during preflight");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(first).expect("read first"),
            "first before\n"
        );
        assert_eq!(
            std::fs::read_to_string(stale).expect("read stale"),
            "independent edit\n"
        );
    }

    #[test]
    fn recovery_rolls_back_an_interrupted_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("source.py");
        let staged = dir.path().join("source.new");
        let backup = dir.path().join("source.old");
        let journal_path = dir.path().join(".strict-kwargs-fix-journal-test.json");
        std::fs::write(&destination, "fixed\n").expect("write partial replacement");
        std::fs::write(&backup, "before\n").expect("write backup");
        let journal = FixJournal {
            committed: false,
            entries: vec![JournalEntry {
                destination: destination.clone(),
                staged,
                backup: backup.clone(),
            }],
        };
        write_journal(&journal_path, &journal).expect("write journal");

        recover_journal(&journal_path).expect("recover transaction");

        assert_eq!(
            std::fs::read_to_string(destination).expect("read destination"),
            "before\n"
        );
        assert!(!backup.exists());
        assert!(!journal_path.exists());
    }

    #[test]
    fn write_all_updates_every_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.py");
        let second = dir.path().join("second.py");
        std::fs::write(&first, "first before\n").expect("write first");
        std::fs::write(&second, "second before\n").expect("write second");
        let fixes = vec![
            FileFix {
                path: first.clone(),
                original: "first before\n".to_owned(),
                fixed: "first after\n".to_owned(),
                count: 1,
            },
            FileFix {
                path: second.clone(),
                original: "second before\n".to_owned(),
                fixed: "second after\n".to_owned(),
                count: 1,
            },
        ];

        write_all_preserving_encoding(&fixes).expect("write every fix");
        assert_eq!(
            std::fs::read_to_string(first).expect("read first"),
            "first after\n"
        );
        assert_eq!(
            std::fs::read_to_string(second).expect("read second"),
            "second after\n"
        );
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
    fn unified_diff_quotes_unsafe_path_bytes() {
        for (path, escaped) in [
            ("line\nbreak.py", "line\\nbreak.py"),
            ("tab\tname.py", "tab\\tname.py"),
            ("back\\slash.py", "back\\\\slash.py"),
            ("\"quote.py", "\\\"quote.py"),
        ] {
            let diff = unified_diff(Path::new(path), "f(1)\n", "f(x=1)\n", false);
            let expected_old = format!("--- \"a/{escaped}\"");
            let expected_new = format!("+++ \"b/{escaped}\"");
            let mut lines = diff.lines();
            assert_eq!(lines.next(), Some(expected_old.as_str()));
            assert_eq!(lines.next(), Some(expected_new.as_str()));
        }
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

/// Exercises `git_diff_path` under llvm-cov. The main `tests` module is
/// `#[coverage(off)]`.
#[cfg(test)]
mod git_diff_path_coverage {
    use super::{git_diff_path, unified_diff};
    use std::path::Path;

    #[test]
    fn git_diff_path_keeps_safe_paths_unquoted() {
        assert_eq!(
            git_diff_path("a/", Path::new("pkg/main.py")),
            "a/pkg/main.py"
        );
    }

    #[test]
    fn git_diff_path_quotes_control_bytes_with_octal_escapes() {
        assert_eq!(
            git_diff_path("a/", Path::new("ctrl\x01name.py")),
            "\"a/ctrl\\001name.py\""
        );
    }

    #[test]
    fn git_diff_path_quotes_carriage_return() {
        let diff = unified_diff(Path::new("line\rname.py"), "f(1)\n", "f(x=1)\n", false);
        assert!(diff.starts_with("--- \"a/line\\rname.py\"\n"));
    }
}
