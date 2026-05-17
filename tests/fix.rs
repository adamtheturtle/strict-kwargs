//! Integration tests for `strict-kwargs fix` (issue #7).

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort
// with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use strict_kwargs::{check_paths, fix_paths, Config, Diagnostic};

struct TestProject {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl TestProject {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        Self { _temp: temp, root }
    }

    fn file(self, path: &str, content: &str) -> Self {
        let file_path = self.root.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(file_path, content).expect("write file");
        self
    }

    fn main(self, content: &str) -> Self {
        self.file("main.py", content)
    }

    fn pyproject(self, content: &str) -> Self {
        self.file("pyproject.toml", content)
    }

    /// Run the fixer over `main.py` and return the rewritten source (or the
    /// original when nothing was fixed).
    fn fixed_main(&self) -> String {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root);
        let fixes = fix_paths(&self.root, std::slice::from_ref(&main), &config).expect("fix");
        fixes.into_iter().find(|f| f.path == main).map_or_else(
            || std::fs::read_to_string(&main).expect("read"),
            |f| f.fixed,
        )
    }

    /// Diagnostics for `main.py`, formatted like the other test harness.
    fn check_main(&self) -> Vec<String> {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root);
        let diagnostics = check_paths(&self.root, &[main], &config, None).expect("check");
        diagnostics.iter().map(Diagnostic::message).collect()
    }
}

fn project(source: &str) -> TestProject {
    TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .main(source)
}

fn assert_fixed(source: &str, expected: &str) {
    let proj = project(source);
    assert_eq!(proj.fixed_main(), expected);
}

/// The fixer's output must itself be clean (round-trip).
fn assert_round_trips(source: &str) {
    let proj = project(source);
    let fixed = proj.fixed_main();
    std::fs::write(proj.root.join("main.py"), &fixed).expect("write fixed");
    assert!(
        proj.check_main().is_empty(),
        "fixed source still has violations: {:?}\n{fixed}",
        proj.check_main()
    );
}

/// The fixer must leave `source` untouched.
fn assert_unchanged(source: &str) {
    assert_eq!(project(source).fixed_main(), source);
}

#[test]
fn rewrites_plain_function_call() {
    assert_fixed(
        "def add(a: int, b: int) -> int: ...\nadd(1, 2)\n",
        "def add(a: int, b: int) -> int: ...\nadd(a=1, b=2)\n",
    );
}

#[test]
fn rewrites_mixed_call() {
    assert_fixed(
        "def add(a: int, b: int) -> int: ...\nadd(1, b=2)\n",
        "def add(a: int, b: int) -> int: ...\nadd(a=1, b=2)\n",
    );
}

#[test]
fn preserves_internal_whitespace() {
    assert_fixed(
        "def add(a: int, b: int) -> int: ...\nadd(  1 ,  2  )\n",
        "def add(a: int, b: int) -> int: ...\nadd(  a=1 ,  b=2  )\n",
    );
}

#[test]
fn rewrites_constructor_excluding_self() {
    assert_fixed(
        "class C:\n    def __init__(self, x: int, y: int) -> None: ...\nC(1, 2)\n",
        "class C:\n    def __init__(self, x: int, y: int) -> None: ...\nC(x=1, y=2)\n",
    );
}

#[test]
fn rewrites_method_excluding_self() {
    assert_fixed(
        "class C:\n    def m(self, a: int, b: int) -> None: ...\nc = C()\nc.m(1, 2)\n",
        "class C:\n    def m(self, a: int, b: int) -> None: ...\nc = C()\nc.m(a=1, b=2)\n",
    );
}

#[test]
fn keeps_positional_only_positional() {
    // `a` is positional-only and stays; only `b` is rewritten.
    assert_fixed(
        "def f(a: int, /, b: int) -> None: ...\nf(1, 2)\n",
        "def f(a: int, /, b: int) -> None: ...\nf(1, b=2)\n",
    );
}

#[test]
fn does_not_fix_star_args() {
    assert_unchanged("def add(a: int, b: int) -> int: ...\nxs = [1, 2]\nadd(*xs)\n");
}

#[test]
fn does_not_fix_double_star_kwargs() {
    assert_unchanged("def f(a: int, b: int) -> None: ...\nkw = {}\nf(1, **kw)\n");
}

#[test]
fn does_not_fix_builtins() {
    // Flagged by the checker but intentionally out of scope for the fixer.
    assert_unchanged("str(123)\n");
}

#[test]
fn does_not_fix_before_var_positional() {
    // Absorbed by `*rest`: not a violation, nothing to fix.
    assert_unchanged("def f(a: int, *rest: int) -> None: ...\nf(1, 2, 3)\n");
}

#[test]
fn does_not_fix_overloaded_callee() {
    // Two signatures: a keyword rewrite could bind the wrong name.
    let source = "from typing import overload\n\
         @overload\n\
         def f(a: int) -> int: ...\n\
         @overload\n\
         def f(a: str) -> str: ...\n\
         def f(a):\n    return a\n\
         f(1)\n";
    assert_unchanged(source);
}

#[test]
fn round_trips_keyword_only_and_methods() {
    assert_round_trips(
        "def add(a: int, b: int) -> int: ...\n\
         class C:\n    def m(self, a: int, b: int) -> None: ...\n\
         add(1, 2)\n\
         c = C()\n\
         c.m(3, 4)\n",
    );
}

#[test]
fn fixes_only_surplus_positionals() {
    // `a` is positional-only (allowed); `b`/`c` are rewritten.
    assert_fixed(
        "def f(a: int, /, b: int, c: int) -> None: ...\nf(1, 2, 3)\n",
        "def f(a: int, /, b: int, c: int) -> None: ...\nf(1, b=2, c=3)\n",
    );
}

#[test]
fn unchanged_file_not_reported() {
    let proj = project("def f(a: int) -> None: ...\nf(a=1)\n");
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root);
    let fixes = fix_paths(&proj.root, &[main], &config).expect("fix");
    assert!(fixes.is_empty());
}
