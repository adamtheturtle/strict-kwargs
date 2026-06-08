//! Opt-in differential completeness test against a pinned `Sphinx` checkout.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. This is an integration-test crate, so clippy's
// `allow-*-in-tests` does not apply here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use insta::assert_snapshot;
use strict_kwargs::{check_paths, Config, Diagnostic};

const SPHINX_REPO: &str = "https://github.com/sphinx-doc/sphinx.git";
const SPHINX_REF: &str = "cc7c6f435ad37bb12264f8118c8461b230e6830c";
const TY_VERSION: &str = "0.0.44";
const SNAPSHOT_RELATIVE_PATH: &str =
    "tests/snapshots/sphinx_completeness__pinned_sphinx_diagnostics.snap";
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

    let runs = run_count();
    let actual = collect_observations(&checkout.root, runs);
    let actual_keys = collect_stable(&actual, runs);
    assert!(
        !actual_keys.is_empty(),
        "pinned Sphinx diagnostics snapshot must not be empty"
    );

    if std::env::var_os(REGENERATE_ENV).is_some() {
        assert_snapshot!("pinned_sphinx_diagnostics", format_snapshot(&actual_keys));
        return;
    }

    let expected = read_snapshot_diagnostics(snapshot_path());
    let missing = expected
        .difference(&actual_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        missing.is_empty(),
        "pinned Sphinx diagnostics are missing required snapshot entries:\n{}",
        format_diagnostic_set("", &missing)
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

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT_RELATIVE_PATH)
}

fn read_snapshot_diagnostics(path: PathBuf) -> BTreeSet<DiagnosticKey> {
    let raw = std::fs::read_to_string(path).expect("read Sphinx diagnostic snapshot");
    raw.lines()
        .skip_while(|line| *line != "---")
        .skip(1)
        .skip_while(|line| *line != "---")
        .skip(1)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(DiagnosticKey::parse)
        .collect()
}

fn format_snapshot(baseline: &BTreeSet<DiagnosticKey>) -> String {
    format_diagnostic_set(
        &format!(
            "# Pinned Sphinx diagnostic snapshot.\n\
             # Repository: {SPHINX_REPO}\n\
             # Ref: {SPHINX_REF}\n\
             # ty: {TY_VERSION}\n\
             # Format: relative-path<TAB>line<TAB>column<TAB>callee\n"
        ),
        baseline,
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
            path: parts.next().expect("snapshot path").to_owned(),
            line: parts
                .next()
                .expect("snapshot line")
                .parse()
                .expect("snapshot line is a usize"),
            column: parts
                .next()
                .expect("snapshot column")
                .parse()
                .expect("snapshot column is a usize"),
            callee: canonical_callee(parts.next().expect("snapshot callee")),
        }
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
    use super::{canonical_callee, collect_stable, DiagnosticKey};
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
    fn collect_stable_keeps_diagnostics_seen_on_every_run() {
        let stable = key("stable.py", 1);
        let unstable = key("unstable.py", 2);
        let observed = BTreeMap::from([(stable.clone(), 3), (unstable, 1)]);

        assert_eq!(collect_stable(&observed, 3), BTreeSet::from([stable]));
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
