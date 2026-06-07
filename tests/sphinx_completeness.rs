//! Opt-in differential completeness test against a pinned `Sphinx` checkout.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. This is an integration-test crate, so clippy's
// `allow-*-in-tests` does not apply here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use strict_kwargs::{check_paths, Config, Diagnostic};

const SPHINX_REPO: &str = "https://github.com/sphinx-doc/sphinx.git";
const SPHINX_REF: &str = "cc7c6f435ad37bb12264f8118c8461b230e6830c";
const TY_VERSION: &str = "0.0.44";
const EXPECTED_RELATIVE_PATH: &str = "tests/golden/sphinx-completeness.tsv";
const ALLOWED_EXTRA_RELATIVE_PATH: &str = "tests/golden/sphinx-completeness-allowed-extra.tsv";
const DIFF_DISPLAY_LIMIT: usize = 100;
const REGENERATE_ENV: &str = "STRICT_KWARGS_REGENERATE_SPHINX_GOLDEN";
const CHECKOUT_ENV: &str = "STRICT_KWARGS_SPHINX_CHECKOUT";
const PYTHON_ENV: &str = "STRICT_KWARGS_SPHINX_PYTHON_ENV";
const RUNS_ENV: &str = "STRICT_KWARGS_SPHINX_RUNS";

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct DiagnosticKey {
    path: String,
    line: usize,
    column: usize,
    callee: String,
}

struct SphinxCheckout {
    _temp: Option<tempfile::TempDir>,
    root: PathBuf,
}

#[test]
#[ignore = "heavy opt-in test: clones/checks pinned Sphinx and starts ty server"]
fn pinned_sphinx_diagnostics_match_golden_oracle() {
    assert_ty_version();
    let checkout = pinned_sphinx_checkout();
    let expected = read_diagnostic_set(expected_path());
    let allowed_extra = read_diagnostic_set(allowed_extra_path());
    assert!(
        !expected.is_empty(),
        "{EXPECTED_RELATIVE_PATH} must contain at least one diagnostic"
    );

    let runs = run_count();
    let actual = collect_observations(&checkout.root, runs);
    let actual_keys = actual.keys().cloned().collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual_keys).cloned().collect();
    let unstable_expected = expected
        .iter()
        .filter(|key| {
            let observed_runs = actual.get(*key).copied().unwrap_or_default();
            observed_runs > 0 && observed_runs < runs
        })
        .cloned()
        .collect();
    let unexpected = actual_keys
        .difference(&expected)
        .filter(|key| !allowed_extra.contains(*key))
        .cloned()
        .collect();

    let diff = OracleDiff {
        missing,
        unstable_expected,
        unexpected,
    };
    assert!(
        diff.is_empty(),
        "pinned Sphinx diagnostics differ from the golden oracle:\n{}",
        diff.display()
    );
}

#[test]
#[ignore = "writes tests/golden/sphinx-completeness.tsv when explicitly enabled"]
fn regenerate_pinned_sphinx_golden_baseline() {
    if std::env::var_os(REGENERATE_ENV).is_none() {
        eprintln!(
            "set {REGENERATE_ENV}=1 to regenerate {EXPECTED_RELATIVE_PATH} and \
             {ALLOWED_EXTRA_RELATIVE_PATH}"
        );
        return;
    }

    assert_ty_version();
    let checkout = pinned_sphinx_checkout();
    let runs = run_count();
    let observed = collect_observations(&checkout.root, runs);
    let baseline = collect_stable(&observed, runs);
    let allowed_extra = collect_unstable(&observed, runs);
    assert!(
        !baseline.is_empty(),
        "regenerated Sphinx baseline must not be empty"
    );

    let expected_path = expected_path();
    std::fs::write(&expected_path, format_expected(&baseline))
        .expect("write Sphinx golden baseline");
    eprintln!(
        "wrote {} stable diagnostics to {}",
        baseline.len(),
        expected_path.display()
    );

    let allowed_extra_path = allowed_extra_path();
    std::fs::write(
        &allowed_extra_path,
        format_allowed_extra(&allowed_extra, runs),
    )
    .expect("write Sphinx allowed-extra baseline");
    eprintln!(
        "wrote {} unstable allowed extras to {}",
        allowed_extra.len(),
        allowed_extra_path.display()
    );
}

fn pinned_sphinx_checkout() -> SphinxCheckout {
    if let Some(root) = std::env::var_os(CHECKOUT_ENV).map(PathBuf::from) {
        assert_pinned_ref(&root);
        return SphinxCheckout { _temp: None, root };
    }

    let temp = tempfile::Builder::new()
        .prefix("strictkw-sphinx-")
        .tempdir()
        .expect("create Sphinx tempdir");
    let root = temp.path().join("sphinx");
    std::fs::create_dir(&root).expect("create Sphinx checkout directory");
    git(&root, &["init", "--quiet"]);
    git(&root, &["remote", "add", "origin", SPHINX_REPO]);
    git(&root, &["fetch", "--depth=1", "origin", SPHINX_REF]);
    git(&root, &["checkout", "--detach", "--quiet", "FETCH_HEAD"]);
    assert_pinned_ref(&root);
    SphinxCheckout {
        _temp: Some(temp),
        root,
    }
}

fn assert_pinned_ref(root: &Path) {
    let output = git_output(root, &["rev-parse", "HEAD"]);
    let actual = String::from_utf8(output.stdout).expect("git rev-parse output is utf8");
    assert_eq!(
        actual.trim(),
        SPHINX_REF,
        "{CHECKOUT_ENV} must point at sphinx-doc/sphinx {SPHINX_REF}"
    );
}

fn git(root: &Path, args: &[&str]) {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "git {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git")
}

fn assert_ty_version() {
    let output = Command::new("ty")
        .arg("version")
        .output()
        .expect("run ty version");
    assert!(
        output.status.success(),
        "ty version failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("ty version output is utf8");
    assert!(
        stdout.split_whitespace().any(|part| part == TY_VERSION),
        "Sphinx completeness oracle requires ty {TY_VERSION}; got: {}",
        stdout.trim()
    );
}

fn run_count() -> usize {
    std::env::var(RUNS_ENV)
        .ok()
        .map_or(1, |raw| {
            raw.parse::<usize>()
                .expect("STRICT_KWARGS_SPHINX_RUNS is a usize")
        })
        .max(1)
}

fn collect_observations(root: &Path, runs: usize) -> BTreeMap<DiagnosticKey, usize> {
    let mut observed = BTreeMap::new();
    for _ in 0..runs {
        for key in collect_diagnostics(root) {
            *observed.entry(key).or_default() += 1;
        }
    }
    observed
}

fn collect_stable(
    observed: &BTreeMap<DiagnosticKey, usize>,
    runs: usize,
) -> BTreeSet<DiagnosticKey> {
    observed
        .iter()
        .filter(|(_, count)| **count == runs)
        .map(|(key, _)| key.clone())
        .collect()
}

fn collect_unstable(
    observed: &BTreeMap<DiagnosticKey, usize>,
    runs: usize,
) -> BTreeSet<DiagnosticKey> {
    observed
        .iter()
        .filter(|(_, count)| **count < runs)
        .map(|(key, _)| key.clone())
        .collect()
}

fn collect_diagnostics(root: &Path) -> BTreeSet<DiagnosticKey> {
    let config = Config::load(root).expect("load Sphinx config");
    let paths = [root.to_path_buf()];
    let python_env = std::env::var_os(PYTHON_ENV).map(PathBuf::from);
    check_paths(root, &paths, &config, python_env.as_deref(), None)
        .expect("check pinned Sphinx")
        .iter()
        .map(|diagnostic| DiagnosticKey::from_diagnostic(root, diagnostic))
        .collect()
}

fn read_diagnostic_set(path: PathBuf) -> BTreeSet<DiagnosticKey> {
    let raw = std::fs::read_to_string(path).expect("read Sphinx diagnostic baseline");
    raw.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(DiagnosticKey::parse)
        .collect()
}

fn expected_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(EXPECTED_RELATIVE_PATH)
}

fn allowed_extra_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(ALLOWED_EXTRA_RELATIVE_PATH)
}

fn format_expected(baseline: &BTreeSet<DiagnosticKey>) -> String {
    format_diagnostic_set(
        &format!(
            "# Pinned Sphinx stable diagnostic baseline.\n\
             # Diagnostics in this file must appear on every observed run.\n\
             # Repository: {SPHINX_REPO}\n\
             # Ref: {SPHINX_REF}\n\
             # ty: {TY_VERSION}\n\
             # Format: relative-path<TAB>line<TAB>column<TAB>callee\n"
        ),
        baseline,
    )
}

fn format_allowed_extra(allowed_extra: &BTreeSet<DiagnosticKey>, runs: usize) -> String {
    format_diagnostic_set(
        &format!(
            "# Pinned Sphinx allowed-extra diagnostic baseline.\n\
             # Entries in this file appeared in at least one, but not all, of {runs} \
             observed regeneration runs.\n\
             # They document currently unstable resolver output and are allowed as extras.\n\
             # Repository: {SPHINX_REPO}\n\
             # Ref: {SPHINX_REF}\n\
             # ty: {TY_VERSION}\n\
             # Format: relative-path<TAB>line<TAB>column<TAB>callee\n"
        ),
        allowed_extra,
    )
}

fn format_diagnostic_set(header: &str, baseline: &BTreeSet<DiagnosticKey>) -> String {
    let mut out = header.to_owned();
    for key in baseline {
        writeln!(
            out,
            "{}\t{}\t{}\t{}",
            key.path, key.line, key.column, key.callee
        )
        .expect("write baseline line");
    }
    out
}

struct OracleDiff {
    missing: BTreeSet<DiagnosticKey>,
    unstable_expected: BTreeSet<DiagnosticKey>,
    unexpected: BTreeSet<DiagnosticKey>,
}

impl OracleDiff {
    fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.unstable_expected.is_empty() && self.unexpected.is_empty()
    }

    fn display(&self) -> String {
        let mut out = String::new();
        write_diff_section(
            &mut out,
            "Missing golden diagnostics",
            &self.missing,
            "These expected diagnostics were absent from every run.",
        );
        write_diff_section(
            &mut out,
            "Unstable golden diagnostics",
            &self.unstable_expected,
            "These expected diagnostics appeared in some, but not all, runs.",
        );
        write_diff_section(
            &mut out,
            "Unexpected extra diagnostics",
            &self.unexpected,
            "Add legitimate new diagnostics to the stable baseline, or document unstable ones in \
             the allowed-extra file.",
        );
        out
    }
}

fn write_diff_section(
    out: &mut String,
    title: &str,
    diagnostics: &BTreeSet<DiagnosticKey>,
    help: &str,
) {
    if diagnostics.is_empty() {
        return;
    }
    writeln!(out, "{title}: {}", diagnostics.len()).expect("write diff section title");
    writeln!(out, "{help}").expect("write diff section help");
    for diagnostic in diagnostics.iter().take(DIFF_DISPLAY_LIMIT) {
        writeln!(out, "{}", diagnostic.display()).expect("write diff entry");
    }
    if diagnostics.len() > DIFF_DISPLAY_LIMIT {
        writeln!(out, "... {} more", diagnostics.len() - DIFF_DISPLAY_LIMIT)
            .expect("write diff truncation");
    }
}

impl DiagnosticKey {
    fn from_diagnostic(root: &Path, diagnostic: &Diagnostic) -> Self {
        let relative = diagnostic
            .path
            .strip_prefix(root)
            .unwrap_or(&diagnostic.path);
        Self {
            path: relative.to_string_lossy().replace('\\', "/"),
            line: diagnostic.line,
            column: diagnostic.column,
            callee: canonical_callee(&diagnostic.callee),
        }
    }

    fn parse(line: &str) -> Self {
        let mut parts = line.splitn(4, '\t');
        Self {
            path: parts.next().expect("baseline path").to_owned(),
            line: parts
                .next()
                .expect("baseline line")
                .parse()
                .expect("baseline line is a usize"),
            column: parts
                .next()
                .expect("baseline column")
                .parse()
                .expect("baseline column is a usize"),
            callee: canonical_callee(parts.next().expect("baseline callee")),
        }
    }

    fn display(&self) -> String {
        format!(
            "{}:{}:{}\t{}",
            self.path, self.line, self.column, self.callee
        )
    }
}

fn canonical_callee(raw: &str) -> String {
    let trimmed = raw.trim();
    let quoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.find('"').map(|end| &rest[..end]))
        .unwrap_or(trimmed);
    let without_generic = quoted.split_once('[').map_or(quoted, |(callee, _)| callee);
    let without_owner_marker = without_generic
        .rsplit_once('@')
        .map_or(without_generic, |(_, callee)| callee);
    format!("\"{without_owner_marker}\"")
}

#[cfg(test)]
mod tests {
    use super::{canonical_callee, collect_stable, collect_unstable, DiagnosticKey, OracleDiff};
    use std::collections::{BTreeMap, BTreeSet};

    fn key(path: &str, line: usize) -> DiagnosticKey {
        DiagnosticKey {
            path: path.to_owned(),
            line,
            column: 1,
            callee: "\"f\"".to_owned(),
        }
    }

    #[test]
    fn observation_partition_splits_stable_and_unstable_diagnostics() {
        let stable = key("stable.py", 1);
        let unstable = key("unstable.py", 2);
        let observed = BTreeMap::from([(stable.clone(), 3), (unstable.clone(), 1)]);

        assert_eq!(collect_stable(&observed, 3), BTreeSet::from([stable]));
        assert_eq!(collect_unstable(&observed, 3), BTreeSet::from([unstable]));
    }

    #[test]
    fn oracle_diff_reports_missing_unstable_and_unexpected_entries() {
        let diff = OracleDiff {
            missing: BTreeSet::from([key("missing.py", 1)]),
            unstable_expected: BTreeSet::from([key("flaky.py", 2)]),
            unexpected: BTreeSet::from([key("extra.py", 3)]),
        };

        let display = diff.display();
        assert!(display.contains("Missing golden diagnostics: 1"));
        assert!(display.contains("Unstable golden diagnostics: 1"));
        assert!(display.contains("Unexpected extra diagnostics: 1"));
    }

    #[test]
    fn canonical_callee_ignores_ty_display_owner_and_generic_drift() {
        assert_eq!(
            canonical_callee("\"find_files\" of \"BuildEnvironment\""),
            "\"find_files\""
        );
        assert_eq!(canonical_callee("\"setdefault[_T]\""), "\"setdefault\"");
        assert_eq!(
            canonical_callee("\"get\" of \"Self@extract_original_messages\""),
            "\"get\""
        );
        assert_eq!(
            canonical_callee("\"Self@preserve_original_messages\""),
            "\"preserve_original_messages\""
        );
    }
}
