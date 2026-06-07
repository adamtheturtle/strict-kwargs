//! Opt-in differential completeness test against a pinned `Sphinx` checkout.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. This is an integration-test crate, so clippy's
// `allow-*-in-tests` does not apply here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use strict_kwargs::{check_paths, Config, Diagnostic};

const SPHINX_REPO: &str = "https://github.com/sphinx-doc/sphinx.git";
const SPHINX_REF: &str = "cc7c6f435ad37bb12264f8118c8461b230e6830c";
const BASELINE_RELATIVE_PATH: &str = "tests/golden/sphinx-completeness.tsv";
const REGENERATE_ENV: &str = "STRICT_KWARGS_REGENERATE_SPHINX_GOLDEN";
const CHECKOUT_ENV: &str = "STRICT_KWARGS_SPHINX_CHECKOUT";
const PYTHON_ENV: &str = "STRICT_KWARGS_SPHINX_PYTHON_ENV";
const RUNS_ENV: &str = "STRICT_KWARGS_SPHINX_RUNS";
const BASELINE_LIMIT_ENV: &str = "STRICT_KWARGS_SPHINX_BASELINE_LIMIT";
const DEFAULT_BASELINE_LIMIT: usize = 5_000;

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
fn pinned_sphinx_diagnostics_include_golden_subset() {
    let checkout = pinned_sphinx_checkout();
    let expected = read_baseline();
    assert!(
        !expected.is_empty(),
        "{BASELINE_RELATIVE_PATH} must contain at least one diagnostic"
    );

    let actual = collect_union(&checkout.root, run_count());
    let missing = expected
        .difference(&actual)
        .take(20)
        .map(DiagnosticKey::display)
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "pinned Sphinx diagnostics are missing {} golden entries; first missing entries:\n{}",
        expected.difference(&actual).count(),
        missing.join("\n")
    );
}

#[test]
#[ignore = "writes tests/golden/sphinx-completeness.tsv when explicitly enabled"]
fn regenerate_pinned_sphinx_golden_baseline() {
    if std::env::var_os(REGENERATE_ENV).is_none() {
        eprintln!("set {REGENERATE_ENV}=1 to regenerate {BASELINE_RELATIVE_PATH}");
        return;
    }

    let checkout = pinned_sphinx_checkout();
    let baseline = limit_baseline(collect_intersection(&checkout.root, run_count()));
    assert!(
        !baseline.is_empty(),
        "regenerated Sphinx baseline must not be empty"
    );

    let path = baseline_path();
    std::fs::write(&path, format_baseline(&baseline)).expect("write Sphinx golden baseline");
    eprintln!("wrote {} diagnostics to {}", baseline.len(), path.display());
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

fn run_count() -> usize {
    std::env::var(RUNS_ENV)
        .ok()
        .map_or(1, |raw| {
            raw.parse::<usize>()
                .expect("STRICT_KWARGS_SPHINX_RUNS is a usize")
        })
        .max(1)
}

fn collect_union(root: &Path, runs: usize) -> BTreeSet<DiagnosticKey> {
    let mut union = BTreeSet::new();
    for _ in 0..runs {
        union.extend(collect_diagnostics(root));
    }
    union
}

fn collect_intersection(root: &Path, runs: usize) -> BTreeSet<DiagnosticKey> {
    let mut intersection = collect_diagnostics(root);
    for _ in 1..runs {
        let current = collect_diagnostics(root);
        intersection = intersection.intersection(&current).cloned().collect();
    }
    intersection
}

fn limit_baseline(baseline: BTreeSet<DiagnosticKey>) -> BTreeSet<DiagnosticKey> {
    baseline.into_iter().take(baseline_limit()).collect()
}

fn baseline_limit() -> usize {
    std::env::var(BASELINE_LIMIT_ENV)
        .ok()
        .map_or(DEFAULT_BASELINE_LIMIT, |raw| {
            raw.parse::<usize>()
                .expect("STRICT_KWARGS_SPHINX_BASELINE_LIMIT is a usize")
        })
        .max(1)
}

fn collect_diagnostics(root: &Path) -> BTreeSet<DiagnosticKey> {
    let config = Config::load(root).expect("load Sphinx config");
    let paths = [root.to_path_buf()];
    let python_env = std::env::var_os(PYTHON_ENV).map(PathBuf::from);
    check_paths(root, &paths, &config, python_env.as_deref(), None)
        .expect("check pinned Sphinx")
        .into_iter()
        .map(|diagnostic| DiagnosticKey::from_diagnostic(root, diagnostic))
        .collect()
}

fn read_baseline() -> BTreeSet<DiagnosticKey> {
    let raw = std::fs::read_to_string(baseline_path()).expect("read Sphinx golden baseline");
    raw.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(DiagnosticKey::parse)
        .collect()
}

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE_RELATIVE_PATH)
}

fn format_baseline(baseline: &BTreeSet<DiagnosticKey>) -> String {
    let mut out = format!(
        "# Pinned Sphinx diagnostic completeness baseline.\n\
         # Repository: {SPHINX_REPO}\n\
         # Ref: {SPHINX_REF}\n\
         # Conservative subset limit: {}\n\
         # Format: relative-path<TAB>line<TAB>column<TAB>callee\n",
        baseline_limit()
    );
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

impl DiagnosticKey {
    fn from_diagnostic(root: &Path, diagnostic: Diagnostic) -> Self {
        let relative = diagnostic
            .path
            .strip_prefix(root)
            .unwrap_or(&diagnostic.path);
        Self {
            path: relative.to_string_lossy().replace('\\', "/"),
            line: diagnostic.line,
            column: diagnostic.column,
            callee: diagnostic.callee,
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
            callee: parts.next().expect("baseline callee").to_owned(),
        }
    }

    fn display(&self) -> String {
        format!(
            "{}:{}:{}\t{}",
            self.path, self.line, self.column, self.callee
        )
    }
}
