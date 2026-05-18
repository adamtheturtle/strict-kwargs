//! End-to-end tests for the `strict-kwargs` command-line interface.
//!
//! These drive the *compiled* binary so argument parsing, subcommand
//! dispatch, exit codes and stdout/stderr behaviour are verified the way a
//! user experiences them, rather than by calling the library directly.

// `expect`/`unwrap` are idiomatic in tests; this is an integration-test crate
// (not `#[cfg(test)]`), so the repo's `allow-*-in-tests` does not apply here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_strict-kwargs");

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = Self { _temp: temp, root };
        project.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\nversion = \"0\"\n",
        )
    }

    fn write(self, rel: &str, contents: &str) -> Self {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, contents).expect("write");
        self
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).expect("read")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("spawn strict-kwargs")
    }
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8 stderr")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

#[test]
fn check_clean_exits_zero() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    // Explicit path argument.
    let output = project.run(&["main.py"]);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert!(stderr(&output).is_empty());
}

#[test]
fn check_default_path_dot_reports_violation() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(1)\n");
    // No path argument => clap's default `.`; also exercises project-root
    // auto-discovery from the first path.
    let output = project.run(&[]);
    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(err.contains("Too many positional"), "stderr: {err}");
    assert!(err.contains("main.py"));
}

#[test]
fn check_explicit_project_root_flag() {
    let project = Project::new().write("pkg/m.py", "def f(a: int) -> None: ...\nf(1)\n");
    let root = project.root.to_string_lossy().into_owned();
    let output = project.run(&["--project-root", &root, "pkg/m.py"]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("Too many positional"));
}

#[test]
fn check_unparsable_file_is_fatal_exit_two() {
    let project = Project::new().write("broken.py", "def f(:\n");
    let output = project.run(&["broken.py"]);
    assert_eq!(code(&output), 2);
    assert!(
        stderr(&output).starts_with("strict-kwargs: "),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn fix_reports_when_nothing_to_fix() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 0);
    assert!(stderr(&output).contains("no fixes to apply"));
}

#[test]
fn fix_diff_prints_patch_without_writing() {
    let source = "def f(a: int) -> None: ...\nf(1)\n";
    let project = Project::new().write("main.py", source);
    let output = project.run(&["fix", "--diff", "main.py"]);
    assert_eq!(code(&output), 0);
    let patch = stdout(&output);
    assert!(patch.contains("--- a/"), "patch: {patch}");
    assert!(patch.contains("-f(1)"));
    assert!(patch.contains("+f(a=1)"));
    // `--diff` must not modify the file.
    assert_eq!(project.read("main.py"), source);
}

#[test]
fn fix_single_call_singular_messages() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(1)\n");
    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    // Singular: "1 call" and "1 file".
    assert!(err.contains("fixed 1 call in"), "stderr: {err}");
    assert!(err.contains("fixed 1 call in 1 file"), "stderr: {err}");
    assert_eq!(
        project.read("main.py"),
        "def f(a: int) -> None: ...\nf(a=1)\n"
    );
}

#[test]
fn fix_multiple_calls_and_files_plural_messages() {
    let project = Project::new()
        .write(
            "a.py",
            "def f(a: int, b: int) -> None: ...\nf(1, 2)\nf(3, 4)\n",
        )
        .write("b.py", "def g(a: int) -> None: ...\ng(9)\n");
    let output = project.run(&["fix", "a.py", "b.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    // Plural per-file ("calls") and plural summary ("calls"/"files").
    assert!(err.contains("fixed 2 calls in"), "stderr: {err}");
    assert!(err.contains("calls in 2 files"), "stderr: {err}");
    assert!(project.read("a.py").contains("f(a=1, b=2)"));
    assert!(project.read("b.py").contains("g(a=9)"));
}

const DATACLASS: &str =
    "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\n";

#[test]
fn fix_reports_declined_when_no_fixes() {
    // Only a synthesized-constructor violation: the fixer declines it
    // (issue #29), so there is nothing to write, but it must still announce
    // the violation it left for `check` (issue #42). Singular wording.
    let project = Project::new().write("main.py", &format!("{DATACLASS}D(1, 2)\n"));
    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    assert!(err.contains("no fixes to apply"), "stderr: {err}");
    assert!(
        err.contains("1 violation detected but not rewritten") && err.contains("see it"),
        "stderr: {err}"
    );
}

#[test]
fn fix_reports_declined_after_writing() {
    // One rewritable plain call plus two declined dataclass constructors:
    // `fix` writes the one and reports the two it left (issue #42). Plural
    // wording, and the declined note follows the write summary.
    let project = Project::new().write(
        "main.py",
        &format!("{DATACLASS}def f(a, b): ...\n\nf(1, 2)\nD(1, 2)\nD(3, 4)\n"),
    );
    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    assert!(err.contains("fixed 1 call in"), "stderr: {err}");
    assert!(
        err.contains("2 violations detected but not rewritten") && err.contains("see them"),
        "stderr: {err}"
    );
    assert!(project.read("main.py").contains("f(a=1, b=2)"));
}

#[test]
fn fix_diff_reports_declined() {
    // `--diff` writes nothing but still reports the declined violation on
    // stderr (the patch owns stdout).
    let project = Project::new().write(
        "main.py",
        &format!("{DATACLASS}def f(a, b): ...\n\nf(1, 2)\nD(1, 2)\n"),
    );
    let output = project.run(&["fix", "--diff", "main.py"]);
    assert_eq!(code(&output), 0);
    let patch = stdout(&output);
    assert!(patch.contains("+f(a=1, b=2)"), "patch: {patch}");
    let err = stderr(&output);
    assert!(
        err.contains("1 violation detected but not rewritten") && err.contains("see it"),
        "stderr: {err}"
    );
}

#[test]
fn fix_accepts_python_flag() {
    // `--python` is now accepted by `fix` (issue #42). This call is fully
    // resolvable by the built-in resolver, so the ty fallback never starts
    // and the flag value is irrelevant to the result — the point is that the
    // argument parses and the rewrite still happens.
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(1)\n");
    let output = project.run(&["fix", "--python", ".", "main.py"]);
    assert_eq!(code(&output), 0);
    assert!(
        stderr(&output).contains("fixed 1 call in"),
        "stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        project.read("main.py"),
        "def f(a: int) -> None: ...\nf(a=1)\n"
    );
}

#[test]
fn fix_unparsable_file_is_fatal_exit_two() {
    let project = Project::new().write("broken.py", "def f(:\n");
    let output = project.run(&["fix", "broken.py"]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).starts_with("strict-kwargs: "));
}

#[cfg(unix)]
#[test]
fn fix_write_failure_is_fatal_exit_two() {
    use std::os::unix::fs::PermissionsExt;

    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(1)\n");
    let target = project.root.join("main.py");
    // Read-only file: the fix is computed fine but `std::fs::write` fails,
    // exercising the `?` error path in `run_fix`.
    let mut perms = std::fs::metadata(&target).expect("metadata").permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&target, perms).expect("chmod");

    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 2);
    assert!(
        stderr(&output).starts_with("strict-kwargs: "),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn help_flag_succeeds() {
    // Exercises clap's generated help path (process exits 0 before `run`).
    let output = Command::new(BIN).arg("--help").output().expect("spawn");
    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("strict-kwargs"));
    assert!(Path::new(BIN).exists());
}
