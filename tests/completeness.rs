//! Opt-in differential completeness test against a pinned external repository.

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

const TY_VERSION: &str = "0.0.44";
const SPHINX_LINUX_SNAPSHOT_RELATIVE_PATH: &str =
    "tests/snapshots/completeness__pinned_repository_diagnostics.snap";
const SPHINX_MACOS_SNAPSHOT_RELATIVE_PATH: &str =
    "tests/snapshots/completeness__pinned_repository_diagnostics_macos.snap";
const CPYTHON_LINUX_SNAPSHOT_RELATIVE_PATH: &str =
    "tests/snapshots/completeness__cpython_repository_diagnostics.snap";
const CPYTHON_MACOS_SNAPSHOT_RELATIVE_PATH: &str =
    "tests/snapshots/completeness__cpython_repository_diagnostics_macos.snap";
const REGENERATE_ENV: &str = "STRICT_KWARGS_COMPLETENESS_REGENERATE_GOLDEN";
const CHECKOUT_ENV: &str = "STRICT_KWARGS_COMPLETENESS_CHECKOUT";
const PYTHON_ENV: &str = "STRICT_KWARGS_COMPLETENESS_PYTHON_ENV";
const RUNS_ENV: &str = "STRICT_KWARGS_COMPLETENESS_RUNS";
const REPOSITORY_NAME_ENV: &str = "STRICT_KWARGS_COMPLETENESS_REPOSITORY_NAME";
const REPOSITORY_REF_ENV: &str = "STRICT_KWARGS_COMPLETENESS_REPOSITORY_REF";
const REPOSITORY_URL_ENV: &str = "STRICT_KWARGS_COMPLETENESS_REPOSITORY_URL";

#[derive(Clone, Copy)]
struct RepositoryCase {
    id: &'static str,
    default_name: &'static str,
    default_url: &'static str,
    default_ref: &'static str,
    linux_snapshot_name: &'static str,
    linux_snapshot_relative_path: &'static str,
    macos_snapshot_name: &'static str,
    macos_snapshot_relative_path: &'static str,
    allow_legacy_env: bool,
}

const SPHINX: RepositoryCase = RepositoryCase {
    id: "sphinx",
    default_name: "sphinx",
    default_url: "https://github.com/sphinx-doc/sphinx.git",
    default_ref: "cc7c6f435ad37bb12264f8118c8461b230e6830c",
    linux_snapshot_name: "pinned_repository_diagnostics",
    linux_snapshot_relative_path: SPHINX_LINUX_SNAPSHOT_RELATIVE_PATH,
    macos_snapshot_name: "pinned_repository_diagnostics_macos",
    macos_snapshot_relative_path: SPHINX_MACOS_SNAPSHOT_RELATIVE_PATH,
    allow_legacy_env: true,
};

const CPYTHON: RepositoryCase = RepositoryCase {
    id: "cpython",
    default_name: "cpython",
    default_url: "https://github.com/python/cpython.git",
    default_ref: "8b31d08e62b9714cf8dd1d8b19afa5ecbad2414a",
    linux_snapshot_name: "cpython_repository_diagnostics",
    linux_snapshot_relative_path: CPYTHON_LINUX_SNAPSHOT_RELATIVE_PATH,
    macos_snapshot_name: "cpython_repository_diagnostics_macos",
    macos_snapshot_relative_path: CPYTHON_MACOS_SNAPSHOT_RELATIVE_PATH,
    allow_legacy_env: false,
};

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct DiagnosticKey {
    path: String,
    line: usize,
    column: usize,
    callee: String,
}

struct PinnedRepository {
    case: RepositoryCase,
    name: String,
    url: String,
    reference: String,
}

struct Checkout {
    _temp: Option<tempfile::TempDir>,
    root: PathBuf,
}

#[test]
#[ignore = "heavy opt-in test: clones/checks a pinned external repository and starts ty server"]
fn pinned_repository_diagnostics_match_golden_oracle() {
    run_repository_case(SPHINX);
}

#[test]
#[ignore = "heavy opt-in test: clones/checks CPython and starts ty server"]
fn cpython_repository_diagnostics_match_golden_oracle() {
    run_repository_case(CPYTHON);
}

fn run_repository_case(case: RepositoryCase) {
    assert_ty_version();
    let repository = pinned_repository(case);
    let checkout = pinned_checkout(&repository);

    let runs = run_count();
    let actual = collect_observations(&checkout.root, &repository, runs);
    let actual_keys = collect_stable(&actual, runs);
    assert!(
        !actual_keys.is_empty(),
        "{} diagnostics snapshot must not be empty",
        repository.name
    );

    if std::env::var_os(REGENERATE_ENV).is_some() {
        assert_snapshot!(
            platform_snapshot_name(repository.case),
            format_snapshot(&repository, &actual_keys)
        );
        return;
    }

    let expected = read_snapshot_diagnostics(golden_path(platform_snapshot_relative_path(
        repository.case,
    )));
    let missing = expected
        .difference(&actual_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        missing.is_empty(),
        "{} diagnostics are missing required snapshot entries:\n{}",
        repository.name,
        format_diagnostic_set("", &missing)
    );
}

const fn platform_snapshot_name(case: RepositoryCase) -> &'static str {
    if cfg!(target_os = "macos") {
        case.macos_snapshot_name
    } else {
        case.linux_snapshot_name
    }
}

const fn platform_snapshot_relative_path(case: RepositoryCase) -> &'static str {
    if cfg!(target_os = "macos") {
        case.macos_snapshot_relative_path
    } else {
        case.linux_snapshot_relative_path
    }
}

fn pinned_repository(case: RepositoryCase) -> PinnedRepository {
    PinnedRepository {
        case,
        name: repository_env(case, "REPOSITORY_NAME", REPOSITORY_NAME_ENV)
            .unwrap_or_else(|| case.default_name.to_owned()),
        url: repository_env(case, "REPOSITORY_URL", REPOSITORY_URL_ENV)
            .unwrap_or_else(|| case.default_url.to_owned()),
        reference: repository_env(case, "REPOSITORY_REF", REPOSITORY_REF_ENV)
            .unwrap_or_else(|| case.default_ref.to_owned()),
    }
}

fn repository_env(case: RepositoryCase, suffix: &str, legacy: &str) -> Option<String> {
    std::env::var(case_env_name(case, suffix)).ok().or_else(|| {
        case.allow_legacy_env
            .then(|| std::env::var(legacy).ok())
            .flatten()
    })
}

fn repository_env_os(
    case: RepositoryCase,
    suffix: &str,
    legacy: &str,
) -> Option<std::ffi::OsString> {
    std::env::var_os(case_env_name(case, suffix)).or_else(|| {
        case.allow_legacy_env
            .then(|| std::env::var_os(legacy))
            .flatten()
    })
}

fn case_env_name(case: RepositoryCase, suffix: &str) -> String {
    format!(
        "STRICT_KWARGS_COMPLETENESS_{}_{}",
        case.id.to_ascii_uppercase(),
        suffix
    )
}

fn pinned_checkout(repository: &PinnedRepository) -> Checkout {
    if let Some(root) =
        repository_env_os(repository.case, "CHECKOUT", CHECKOUT_ENV).map(PathBuf::from)
    {
        assert_pinned_ref(&root, repository);
        return Checkout { _temp: None, root };
    }

    let temp = tempfile::Builder::new()
        .prefix("strictkw-completeness-")
        .tempdir()
        .expect("create completeness test tempdir");
    let root = temp.path().join(&repository.name);
    std::fs::create_dir(&root).expect("create completeness checkout directory");
    git(&root, &["init", "--quiet"]);
    git(&root, &["remote", "add", "origin", repository.url.as_str()]);
    git(
        &root,
        &[
            "fetch",
            "--depth=1",
            "origin",
            repository.reference.as_str(),
        ],
    );
    git(&root, &["checkout", "--detach", "--quiet", "FETCH_HEAD"]);
    assert_pinned_ref(&root, repository);
    Checkout {
        _temp: Some(temp),
        root,
    }
}

fn assert_pinned_ref(root: &Path, repository: &PinnedRepository) {
    let output = git_output(root, &["rev-parse", "HEAD"]);
    let actual = String::from_utf8(output.stdout).expect("git rev-parse output is utf8");
    assert_eq!(
        actual.trim(),
        repository.reference,
        "checkout for {} must point at {} {}",
        repository.name,
        repository.url,
        repository.reference
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
        "completeness oracle requires ty {TY_VERSION}; got: {}",
        stdout.trim()
    );
}

fn run_count() -> usize {
    std::env::var(RUNS_ENV)
        .ok()
        .map_or(1, |raw| {
            raw.parse::<usize>()
                .expect("STRICT_KWARGS_COMPLETENESS_RUNS is a usize")
        })
        .max(1)
}

fn collect_observations(
    root: &Path,
    repository: &PinnedRepository,
    runs: usize,
) -> BTreeMap<DiagnosticKey, usize> {
    let mut observed = BTreeMap::new();
    for _ in 0..runs {
        for key in collect_diagnostics(root, repository) {
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

fn collect_diagnostics(root: &Path, repository: &PinnedRepository) -> BTreeSet<DiagnosticKey> {
    let config = Config::load(root).expect("load pinned repository config");
    let paths = [root.to_path_buf()];
    let python_env =
        repository_env_os(repository.case, "PYTHON_ENV", PYTHON_ENV).map(PathBuf::from);
    check_paths(root, &paths, &config, python_env.as_deref(), None)
        .expect("check pinned repository")
        .iter()
        .map(|diagnostic| DiagnosticKey::from_diagnostic(root, diagnostic))
        .collect()
}

fn golden_path(relative_path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path)
}

fn read_snapshot_diagnostics(path: PathBuf) -> BTreeSet<DiagnosticKey> {
    let raw = std::fs::read_to_string(path).expect("read completeness diagnostic snapshot");
    parse_diagnostic_lines(
        raw.lines()
            .skip_while(|line| *line != "---")
            .skip(1)
            .skip_while(|line| *line != "---")
            .skip(1),
    )
}

fn parse_diagnostic_lines<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeSet<DiagnosticKey> {
    lines
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(DiagnosticKey::parse)
        .collect()
}

fn format_snapshot(repository: &PinnedRepository, baseline: &BTreeSet<DiagnosticKey>) -> String {
    format_diagnostic_set(
        &format!(
            "# Pinned repository diagnostic snapshot.\n\
             # Repository: {}\n\
             # Ref: {}\n\
             # ty: {TY_VERSION}\n\
             # Format: relative-path<TAB>line<TAB>column<TAB>callee\n",
            repository.url, repository.reference
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
