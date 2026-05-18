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
use std::thread;
use std::time::Duration;

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

    /// Run a *copy* of the built binary out of `bin_subdir`, with `PATH` set
    /// to `path_dir`, so the only `ty` that can be resolved is one `path_dir`
    /// provides.
    ///
    /// Running a copy (rather than `BIN` under `target/`) is load-bearing:
    /// `ty_command` looks for a `ty` next to the *running executable* before
    /// it consults `PATH`, so controlling `PATH` alone would not hide a `ty`
    /// that happened to sit beside `strict-kwargs` in `target/debug`. The
    /// copy lives in a directory containing nothing but itself, so that
    /// sibling-discovery step finds no `ty` and `PATH` becomes the sole
    /// source — exactly the invariant these tests depend on.
    fn run_isolated(&self, args: &[&str], bin_subdir: &str, path_dir: &Path) -> Output {
        let bin_dir = self.root.join(bin_subdir);
        std::fs::create_dir_all(&bin_dir).expect("mkdir");
        let exe = bin_dir.join(if cfg!(windows) {
            "strict-kwargs.exe"
        } else {
            "strict-kwargs"
        });
        std::fs::copy(BIN, &exe).expect("copy strict-kwargs");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                .expect("chmod strict-kwargs copy");
        }
        for attempt in 0..20 {
            match Command::new(&exe)
                .args(args)
                .current_dir(&self.root)
                .env("PATH", path_dir)
                .output()
            {
                Ok(output) => return output,
                Err(e) if e.raw_os_error() == Some(26) && attempt < 19 => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    break;
                }
            }
        }
        Command::new(&exe)
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path_dir)
            .output()
            .expect("spawn strict-kwargs")
    }

    /// Run with no discoverable `ty` at all, so the required-backend probe
    /// must fail regardless of whether the host has `ty` installed: `PATH`
    /// is an empty directory and the binary copy has no sibling `ty` (see
    /// [`Self::run_isolated`]).
    fn run_without_ty(&self, args: &[&str]) -> Output {
        let empty = self.root.join("__no_ty_path__");
        std::fs::create_dir_all(&empty).expect("mkdir");
        self.run_isolated(args, "__no_ty_bin__", &empty)
    }

    /// Run with `PATH` pointing at a fake `ty` that passes the up-front
    /// `ty version` probe but whose `ty server` exits immediately, so the
    /// lazy server start fails (`CheckError::TyServerFailed`). The shim is
    /// the *only* discoverable `ty`: the strict-kwargs copy sits in its own
    /// directory with no sibling `ty` (see [`Self::run_isolated`]). Unix-only
    /// (shell shim); the coverage gate runs on Linux.
    #[cfg(unix)]
    fn run_with_broken_ty_server(&self, args: &[&str]) -> Output {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.root.join("__fake_ty__");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let shim = bin.join("ty");
        std::fs::write(
            &shim,
            "#!/bin/sh\ncase \"$1\" in\nversion) echo 'ty 0.0.0'; exit 0;;\n*) exit 1;;\nesac\n",
        )
        .expect("write shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.run_isolated(args, "__sk_bin__", &bin)
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
fn check_without_ty_is_fatal_exit_two() {
    // `ty` is a hard requirement: a missing backend aborts (exit 2) rather
    // than silently resolving fewer calls. The probe is up front and
    // content-independent, so even this fully built-in-resolvable file fails.
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    let output = project.run_without_ty(&["main.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(
        err.contains("`ty`") && err.contains("required"),
        "stderr: {err}"
    );
    assert!(err.contains("PATH"), "stderr: {err}");
}

#[test]
fn fix_without_ty_is_fatal_exit_two() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(1)\n");
    let source = project.read("main.py");
    let output = project.run_without_ty(&["fix", "main.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("required"),
        "stderr: {}",
        stderr(&output)
    );
    // The required-backend check is up front, so nothing was rewritten.
    assert_eq!(project.read("main.py"), source);
}

// An inherited-method call the built-in resolver cannot resolve, so it is
// deferred to `ty` — exercising the per-file driver where a failed lazy
// server start becomes the fatal `TyServerFailed`.
#[cfg(unix)]
const TY_DEFERRED: &str =
    "class A:\n    def m(self, a: int) -> None: ...\n\nclass B(A):\n    pass\n\nB().m(1)\n";

#[cfg(unix)]
#[test]
fn check_with_unstartable_ty_server_is_fatal_exit_two() {
    // `ty` is present (the `version` probe passes) but `ty server` will not
    // start, so the run aborts rather than silently degrading.
    let project = Project::new().write("main.py", TY_DEFERRED);
    let output = project.run_with_broken_ty_server(&["main.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(
        err.contains("ty server") && err.contains("required"),
        "stderr: {err}"
    );
}

#[cfg(unix)]
#[test]
fn fix_with_unstartable_ty_server_is_fatal_exit_two() {
    let project = Project::new().write("main.py", TY_DEFERRED);
    let source = project.read("main.py");
    let output = project.run_with_broken_ty_server(&["fix", "main.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("ty server"),
        "stderr: {}",
        stderr(&output)
    );
    assert_eq!(project.read("main.py"), source);
}

#[test]
fn help_flag_succeeds() {
    // Exercises clap's generated help path (process exits 0 before `run`).
    let output = Command::new(BIN).arg("--help").output().expect("spawn");
    assert_eq!(code(&output), 0);
    assert!(stdout(&output).contains("strict-kwargs"));
    assert!(Path::new(BIN).exists());
}
