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
        // A freshly-copied executable can fail to exec with ETXTBSY ("Text
        // file busy", raw OS error 26): in this multithreaded test binary
        // another thread's `fork` may still hold a write fd to the copy when
        // we `exec` it. It is transient — retry a few times before failing.
        for _ in 0..19 {
            let result = Command::new(&exe)
                .args(args)
                .current_dir(&self.root)
                .env("PATH", path_dir)
                .output();
            match result {
                Err(ref error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => return other.expect("spawn strict-kwargs"),
            }
        }
        Command::new(&exe)
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path_dir)
            .output()
            .expect("spawn strict-kwargs (after ETXTBSY retries)")
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

/// `f(f(f(…f(1)…)))` nested `depth` deep, plus the `f` it calls.
fn deeply_nested_source(depth: usize) -> String {
    format!(
        "def f(a):\n    return a\n\n{}1{}\n",
        "f(".repeat(depth),
        ")".repeat(depth)
    )
}

#[test]
fn check_deeply_nested_file_fails_gracefully_not_with_sigabrt() {
    // Issue #54: this used to overflow the stack and abort the process with
    // SIGABRT (exit 134), taking a whole directory/pre-commit run down.
    //
    // Issue #83: the original guard only moved the cliff: depths around 3000
    // were graceful, while the reported 6000/10000-depth inputs still aborted.
    // Pin those exact repro depths.
    for depth in [6000, 10000] {
        let project = Project::new().write("deep.py", &deeply_nested_source(depth));
        let output = project.run(&["deep.py"]);
        assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
        let err = stderr(&output);
        assert!(err.contains("nesting too deep"), "stderr: {err}");
        assert!(err.contains(&depth.to_string()), "stderr: {err}");
    }
}

#[test]
fn fix_deeply_nested_file_fails_gracefully_not_with_sigabrt() {
    let project = Project::new().write("deep.py", &deeply_nested_source(6000));
    let output = project.run(&["fix", "deep.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("nesting too deep"));
}

/// Issue #83: a deeply-nested *dependency* (a module imported by a checked
/// file, lazily resolved via the index) must also be skipped gracefully.
/// Before the fix, lazy stub resolution called unguarded `parse_module` which
/// could crash the analysis thread on a too-deep module; now it goes through
/// `parse_module_guarded` so the stub is silently skipped (fail-closed).
#[test]
fn deeply_nested_dependency_is_skipped_not_sigabrt() {
    // `caller.py` imports from `deep_dep.py` (depth 6000 > MAX_NESTING_DEPTH).
    // The checker must not crash when lazily resolving the deep dependency.
    let project = Project::new()
        .write("deep_dep.py", &deeply_nested_source(6000))
        .write("caller.py", "import deep_dep\ndeep_dep.f(1)\n");
    let output = project.run(&["caller.py"]);
    // Any graceful exit (0, 1, or 2) is acceptable; exit 134 (SIGABRT) is not.
    assert_ne!(
        code(&output),
        134,
        "crashed with SIGABRT; stderr: {}",
        stderr(&output)
    );
}

#[test]
fn check_nesting_at_the_limit_is_handled_on_the_large_stack() {
    // Exactly at the bound: accepted and analysed. This depth would overflow
    // the default thread stack on many platforms/build profiles; it succeeds
    // only because the analysis runs on the large dedicated stack, so this
    // pins that the bound is *deterministic* rather than crash-on-some-hosts.
    let project = Project::new().write("deep.py", &deeply_nested_source(1000));
    let output = project.run(&["deep.py"]);
    assert_eq!(code(&output), 1, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("deep.py"));
}

/// Issue #53 part 1: one non-UTF-8 file must not abort the whole run (exit 2)
/// nor mask genuine violations in every other file. It is skipped with a
/// warning; the sibling's real violation is still reported.
#[test]
fn non_utf8_file_is_skipped_with_warning_and_does_not_mask_others() {
    let project = Project::new();
    std::fs::create_dir_all(project.root.join("pkg")).expect("mkdir");
    std::fs::write(
        project.root.join("pkg/ok.py"),
        "def f(a: int, b: int) -> None: ...\nf(1, 2)\n",
    )
    .expect("write ok.py");
    // One stray non-UTF-8 byte, no PEP 263 declaration.
    std::fs::write(project.root.join("pkg/legacy.py"), b"x = \"\xe9\"\n").expect("write legacy.py");

    let output = project.run(&["pkg"]);
    let err = stderr(&output);
    // Exit 1 (a real violation), not 2 (aborted).
    assert_eq!(code(&output), 1, "stderr: {err}");
    // ok.py's violation is still reported despite the stray sibling.
    assert!(err.contains("Too many positional"), "stderr: {err}");
    assert!(err.contains("ok.py"), "stderr: {err}");
    // legacy.py is reported as a skipped-file warning, not a fatal error.
    assert!(err.contains("warning: skipping"), "stderr: {err}");
    assert!(err.contains("legacy.py"), "stderr: {err}");
    assert!(
        !err.contains("stream did not contain valid UTF-8"),
        "stderr: {err}"
    );
}

/// Issue #82: a non-UTF-8 file with no PEP 263 declaration must not abort a
/// directory run or mask violations in sibling files even when the sibling's
/// function has no type annotations (built-in-resolver-only path) and
/// `--project-root` is passed explicitly.
#[test]
fn issue_82_undecodable_file_does_not_abort_directory_run() {
    let project = Project::new();
    std::fs::create_dir_all(project.root.join("dir")).expect("mkdir");
    std::fs::write(
        project.root.join("dir/ok.py"),
        "def f(a, b): return a\nf(1, 2)\n",
    )
    .expect("write ok.py");
    // Raw non-UTF-8 byte, no PEP 263 declaration — the exact case from #82.
    std::fs::write(project.root.join("dir/legacy.py"), b"x = \"\xe9\"\n").expect("write legacy.py");

    let output = project.run(&["--project-root", ".", "dir"]);
    let err = stderr(&output);
    // Exit 1 (a real violation), not 2 (aborted).
    assert_eq!(code(&output), 1, "stderr: {err}");
    // ok.py:2's violation is still reported despite the stray sibling.
    assert!(err.contains("ok.py"), "stderr: {err}");
    // legacy.py is reported as a skipped-file warning, not a fatal error.
    assert!(err.contains("warning: skipping"), "stderr: {err}");
    assert!(err.contains("legacy.py"), "stderr: {err}");
    assert!(
        !err.contains("stream did not contain valid UTF-8"),
        "stderr: {err}"
    );
}

/// A run whose *only* input is undecodable is a warning, not a failure: the
/// run proceeds, finds nothing, and exits 0 (issue #53).
#[test]
fn lone_non_utf8_file_warns_and_exits_zero() {
    let project = Project::new();
    std::fs::write(project.root.join("bin.py"), b"\x00\x01\xfe\xff").expect("write");
    let output = project.run(&["bin.py"]);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("warning: skipping"));
}

/// Issue #53 part 2: a PEP 263 `coding:` declaration is honored, so a
/// legacy-encoded but valid file is checked (and its violation reported)
/// rather than rejected as an internal error.
#[test]
fn pep263_latin1_declaration_is_honored() {
    let project = Project::new();
    // `# -*- coding: latin-1 -*-`, a lone latin-1 (0xE9) byte in a comment,
    // then a genuine violation. Without PEP 263 this is invalid UTF-8 (exit 2).
    std::fs::write(
        project.root.join("legacy.py"),
        b"# -*- coding: latin-1 -*-\n# byte: \xe9\ndef f(a: int, b: int) -> None: ...\nf(1, 2)\n",
    )
    .expect("write");
    let output = project.run(&["legacy.py"]);
    let err = stderr(&output);
    assert_eq!(code(&output), 1, "stderr: {err}");
    assert!(err.contains("Too many positional"), "stderr: {err}");
    assert!(!err.contains("valid UTF-8"), "stderr: {err}");
    assert!(!err.contains("warning: skipping"), "stderr: {err}");
}

/// `fix` is robust to the same stray file: the undecodable one is skipped
/// with a warning while the fixable sibling is still rewritten (issue #53).
#[test]
fn fix_skips_non_utf8_file_and_still_fixes_others() {
    let project = Project::new();
    std::fs::create_dir_all(project.root.join("pkg")).expect("mkdir");
    std::fs::write(
        project.root.join("pkg/ok.py"),
        "def f(a: int, b: int) -> None: ...\nf(1, 2)\n",
    )
    .expect("write ok.py");
    std::fs::write(project.root.join("pkg/legacy.py"), b"x = \"\xe9\"\n").expect("write legacy.py");

    let output = project.run(&["fix", "pkg"]);
    let err = stderr(&output);
    assert_eq!(code(&output), 0, "stderr: {err}");
    assert!(err.contains("warning: skipping"), "stderr: {err}");
    assert!(err.contains("legacy.py"), "stderr: {err}");
    // The fixable sibling was still rewritten.
    assert_eq!(
        project.read("pkg/ok.py"),
        "def f(a: int, b: int) -> None: ...\nf(a=1, b=2)\n"
    );
    // The stray file is left exactly as it was (untouched bytes).
    assert_eq!(
        std::fs::read(project.root.join("pkg/legacy.py")).expect("read"),
        b"x = \"\xe9\"\n"
    );
}

#[test]
fn check_nonexistent_path_is_fatal_exit_two() {
    // A mistyped target must not report "clean" (exit 0) in CI; like ruff,
    // it is a hard error (issue #55).
    let project = Project::new();
    let output = project.run(&["typo_does_not_exist.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(err.contains("no such file or directory"), "stderr: {err}");
    assert!(err.contains("typo_does_not_exist.py"), "stderr: {err}");
}

#[test]
fn check_nonexistent_dir_is_fatal_exit_two() {
    // A mistyped directory target must not report "clean" (exit 0); like a
    // mistyped file it is a hard error (issue #84).
    let project = Project::new();
    let output = project.run(&["no_such_dir/"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(err.contains("no such file or directory"), "stderr: {err}");
    assert!(err.contains("no_such_dir"), "stderr: {err}");
}

#[test]
fn fix_nonexistent_path_is_fatal_exit_two() {
    // A mistyped target passed to `fix` must not exit 0 silently (issue #84).
    let project = Project::new();
    let output = project.run(&["fix", "typo_does_not_exist.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(err.contains("no such file or directory"), "stderr: {err}");
    assert!(err.contains("typo_does_not_exist.py"), "stderr: {err}");
}

#[test]
fn fix_nonexistent_dir_is_fatal_exit_two() {
    // A mistyped directory target passed to `fix` must not exit 0 silently
    // (issue #84).
    let project = Project::new();
    let output = project.run(&["fix", "no_such_dir/"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(err.contains("no such file or directory"), "stderr: {err}");
    assert!(err.contains("no_such_dir"), "stderr: {err}");
}

#[test]
fn check_invalid_config_is_fatal_exit_two() {
    // `ignore_names` is a string, not a list: running with defaults would
    // silently not apply the user's config. Reported as a hard error
    // instead (issue #55).
    let project = Project::new()
        .write(
            "pyproject.toml",
            "[tool.strict_kwargs]\nignore_names = \"not-a-list\"\n",
        )
        .write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    let output = project.run(&["main.py"]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.starts_with("strict-kwargs: "), "stderr: {err}");
    assert!(err.contains("pyproject.toml"), "stderr: {err}");
    assert!(
        err.contains("invalid `[tool.strict_kwargs]` table"),
        "stderr: {err}"
    );
}

#[test]
fn check_invalid_python_warns_but_continues() {
    // A nonexistent `--python` no longer silently disables the explicit
    // environment: it is reported, then the run falls back to ty's own
    // discovery (issue #55). The file is clean, so the run still exits 0.
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    let output = project.run(&["--python", "/no/such/python", "main.py"]);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("--python /no/such/python does not exist"),
        "stderr: {err}"
    );
    assert!(
        err.contains("ty's own environment discovery"),
        "stderr: {err}"
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
    assert!(
        err.contains("declined synthesized constructor: 1"),
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
    assert!(
        err.contains("declined synthesized constructor: 2"),
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
    assert!(
        !patch.contains("declined synthesized constructor"),
        "patch: {patch}"
    );
    let err = stderr(&output);
    assert!(
        err.contains("1 violation detected but not rewritten") && err.contains("see it"),
        "stderr: {err}"
    );
    assert!(
        err.contains("declined synthesized constructor: 1"),
        "stderr: {err}"
    );
}

#[test]
fn fix_synthesized_constructors_writes_synthesized_constructor() {
    let project = Project::new().write("main.py", &format!("{DATACLASS}D(1, 2)\n"));
    let output = project.run(&["fix", "--fix-synthesized-constructors", "main.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    assert!(
        err.contains("fix opt-in enabled: synthesized constructors"),
        "stderr: {err}"
    );
    assert!(err.contains("fixed 1 call in"), "stderr: {err}");
    assert!(project.read("main.py").contains("D(x=1, y=2)"));
}

#[test]
fn fix_diff_synthesized_constructors_previews_synthesized_constructor() {
    let project = Project::new().write("main.py", &format!("{DATACLASS}D(1, 2)\n"));
    let output = project.run(&["fix", "--diff", "--fix-synthesized-constructors", "main.py"]);
    assert_eq!(code(&output), 0);
    let patch = stdout(&output);
    assert!(patch.contains("+D(x=1, y=2)"), "patch: {patch}");
    assert!(project.read("main.py").contains("D(1, 2)"));
}

#[test]
fn fix_unambiguous_overloads_reports_opt_in() {
    let project = Project::new().write("main.py", "def f(a: int) -> None: ...\nf(a=1)\n");
    let output = project.run(&["fix", "--fix-unambiguous-overloads", "main.py"]);
    assert_eq!(code(&output), 0);
    let err = stderr(&output);
    assert!(
        err.contains("fix opt-in enabled: unambiguous overloads"),
        "stderr: {err}"
    );
    assert!(err.contains("no fixes to apply"), "stderr: {err}");
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
fn fix_elif_test_call_is_rewritten_exactly_once() {
    // Regression: the ruff visitor walked `elif` test expressions twice,
    // causing the fixer to insert `name=` twice and produce `name=name=value`
    // which fails to parse. Verify the rewrite is applied exactly once, and
    // that calls in `elif` bodies and `else` bodies are also rewritten (the
    // `else` clause has no test, exercising the `None` arm of the
    // `if let Some(clause_test)` branch in the `Stmt::If` handler).
    let source = "\
def f(a: int) -> None: ...

x = True
if x:
    pass
elif f(1):
    f(2)
else:
    f(3)
";
    let project = Project::new().write("main.py", source);
    let output = project.run(&["fix", "main.py"]);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(
        project.read("main.py"),
        "\
def f(a: int) -> None: ...

x = True
if x:
    pass
elif f(a=1):
    f(a=2)
else:
    f(a=3)
"
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
