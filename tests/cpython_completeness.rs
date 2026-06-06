//! Differential completeness test against a pinned `CPython` checkout (issue
//! #192).
//!
//! Scans the whole pinned `CPython` tree through the single shared `ty` session
//! and asserts the diagnostics, restricted to a set of stable real-library
//! packages, **exactly equal** a committed golden baseline: no fewer (a dropped
//! real violation — the issue #191 single-session failure mode) and no more (a
//! spurious one).
//!
//! Exact equality is possible because the `ty` fallback is deterministic: it
//! warms up a full type-check before querying (issue #198), and `ty server` is
//! run in the project root so resolution does not depend on the caller's
//! working directory. The result is therefore identical run-to-run. (The
//! baseline is generated on Linux, the CI gate platform, where ty resolves the
//! stdlib against the runtime interpreter; macOS resolves some stdlib symbols
//! against typeshed instead, so the exact set differs there.)
//!
//! The golden baseline lives in `tests/data/cpython_completeness_golden.tsv`;
//! regenerate it with `scripts/regenerate-cpython-golden.sh` when the pinned
//! ref, the pinned ty version, or the resolver output changes.
//!
//! The checkout is supplied out-of-band through the
//! `STRICT_KWARGS_CPYTHON_CHECKOUT` environment variable (CI clones the pinned
//! ref and points it here on every push and pull request). When the variable
//! is unset — the default for a local `cargo test` — the test skips itself so
//! it never forces a multi-minute network checkout on contributors.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (it is not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use strict_kwargs::{check_paths, Config};

/// Environment variable pointing at a pinned `CPython` checkout. Mirrors the
/// benchmark's `STRICT_KWARGS_BENCH_CPYTHON_CHECKOUT`.
const CHECKOUT_ENV: &str = "STRICT_KWARGS_CPYTHON_CHECKOUT";

/// Environment variable pointing at the pinned Python interpreter ty must
/// resolve against (`--python`). Pinning the interpreter — not just the ty
/// version — keeps resolution identical and bounds memory: otherwise ty
/// resolves the stdlib against whatever Python the host has, which changes the
/// result and, against a heavy environment (e.g. a CI runner's preinstalled
/// Python), balloons memory type-checking the whole tree. Set by CI and the
/// regeneration script.
const PYTHON_ENV: &str = "STRICT_KWARGS_CPYTHON_PYTHON";

/// The committed golden baseline, embedded at compile time so the test never
/// has to locate it relative to the working directory.
const GOLDEN: &str = include_str!("data/cpython_completeness_golden.tsv");

/// Packages the exact comparison is restricted to. The whole tree is scanned
/// (so the at-scale single-session path is exercised), but only diagnostics in
/// these stable real-library packages are compared, keeping the committed
/// baseline a reviewable size. Must match `PACKAGES` in
/// `scripts/regenerate-cpython-golden.sh`.
const PACKAGES: &[&str] = &[
    "Lib/asyncio/",
    "Lib/email/",
    "Lib/http/",
    "Lib/importlib/",
    "Lib/multiprocessing/",
    "Lib/unittest/",
];

/// Cap on how many differing entries are listed in a failure message, so a
/// regression is debuggable without dumping thousands of lines into the CI log.
const DIFF_SAMPLE: usize = 50;

/// One golden / actual diagnostic, normalized for comparison: the
/// checkout-relative path (forward slashes), 1-based line and column, and the
/// fully-qualified callee. Mirrors the `(path, line, column, callee)` tuple
/// the issue specifies.
type Entry = (String, usize, usize, String);

fn checkout_dir() -> Option<PathBuf> {
    let raw = std::env::var_os(CHECKOUT_ENV)?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    path.is_dir().then_some(path)
}

fn in_packages(rel: &str) -> bool {
    PACKAGES.iter().any(|pkg| rel.starts_with(pkg))
}

/// The pinned `--python` interpreter, if [`PYTHON_ENV`] is set (CI / regen).
fn gate_python() -> Option<PathBuf> {
    let raw = std::env::var_os(PYTHON_ENV)?;
    (!raw.is_empty()).then(|| PathBuf::from(raw))
}

/// Parse the golden baseline, skipping the `#` comment header.
fn golden_entries() -> BTreeSet<Entry> {
    GOLDEN
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.splitn(4, '\t');
            let path = fields.next().expect("golden path field").to_owned();
            let line_no = fields
                .next()
                .expect("golden line field")
                .parse()
                .expect("golden line is a number");
            let column = fields
                .next()
                .expect("golden column field")
                .parse()
                .expect("golden column is a number");
            let callee = fields.next().expect("golden callee field").to_owned();
            (path, line_no, column, callee)
        })
        .collect()
}

/// Scan the whole `checkout` and return the diagnostics in the chosen packages,
/// normalized to the golden's shape (checkout-relative, forward-slash paths).
fn scan_packages(checkout: &Path) -> BTreeSet<Entry> {
    let config = Config::load(checkout).expect("load config for CPython checkout");
    let python = gate_python();
    let diagnostics = check_paths(
        checkout,
        &[checkout.to_path_buf()],
        &config,
        python.as_deref(),
        None,
    )
    .expect("check the CPython checkout");
    diagnostics
        .into_iter()
        .filter_map(|d| {
            let rel = d
                .path
                .strip_prefix(checkout)
                .unwrap_or(&d.path)
                .to_string_lossy()
                .replace('\\', "/");
            in_packages(&rel).then_some((rel, d.line, d.column, d.callee))
        })
        .collect()
}

fn sample(entries: &BTreeSet<Entry>) -> String {
    entries
        .iter()
        .take(DIFF_SAMPLE)
        .map(|(path, line, column, callee)| format!("  {path}:{line}:{column}: {callee}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cpython_diagnostics_exactly_match_the_golden_baseline() {
    let Some(checkout) = checkout_dir() else {
        eprintln!(
            "skipping CPython completeness test: set {CHECKOUT_ENV} to a pinned \
             checkout to run it (CI does this on every push and pull request)"
        );
        return;
    };

    let golden = golden_entries();
    assert!(
        !golden.is_empty(),
        "golden baseline is empty — regenerate it with \
         scripts/regenerate-cpython-golden.sh"
    );

    let actual = scan_packages(&checkout);

    let missing: BTreeSet<Entry> = golden.difference(&actual).cloned().collect();
    let extra: BTreeSet<Entry> = actual.difference(&golden).cloned().collect();

    assert!(
        missing.is_empty() && extra.is_empty(),
        "ty-resolved diagnostics for the pinned CPython checkout no longer match \
         the golden baseline ({} golden, {} actual).\n\n\
         Missing ({} — dropped real violations, the issue #191 failure mode); \
         first {}:\n{}\n\n\
         Unexpected ({} — newly reported); first {}:\n{}\n\n\
         The ty fallback is deterministic, so this is a real change, not jitter. \
         If it is intended (e.g. a ty version bump), regenerate the baseline with \
         scripts/regenerate-cpython-golden.sh.",
        golden.len(),
        actual.len(),
        missing.len(),
        missing.len().min(DIFF_SAMPLE),
        sample(&missing),
        extra.len(),
        extra.len().min(DIFF_SAMPLE),
        sample(&extra),
    );
}
