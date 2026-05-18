//! Integration tests for `strict-kwargs fix` (issue #7).

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort
// with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

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
        let config = Config::load(&self.root).expect("valid config");
        let outcome =
            fix_paths(&self.root, std::slice::from_ref(&main), &config, None).expect("fix");
        outcome
            .files
            .into_iter()
            .find(|f| f.path == main)
            .map_or_else(
                || std::fs::read_to_string(&main).expect("read"),
                |f| f.fixed,
            )
    }

    /// Run the fixer over `main.py`, returning the raw result so a test can
    /// assert on the fail-safe error (issue #41).
    fn fix_main_result(&self) -> Result<strict_kwargs::FixOutcome, strict_kwargs::CheckError> {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root).expect("valid config");
        fix_paths(&self.root, std::slice::from_ref(&main), &config, None)
    }

    /// Diagnostics for `main.py`, formatted like the other test harness.
    fn check_main(&self) -> Vec<String> {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root).expect("valid config");
        let diagnostics = check_paths(&self.root, &[main], &config, None, None).expect("check");
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

/// Locate the `site-packages` directory inside a freshly created venv
/// (Unix `lib/pythonX.Y/site-packages` or Windows `Lib/site-packages`).
fn venv_site_packages(venv: &Path) -> Option<PathBuf> {
    let win = venv.join("Lib").join("site-packages");
    if win.is_dir() {
        return Some(win);
    }
    for entry in std::fs::read_dir(venv.join("lib")).ok()?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("python") {
            let sp = entry.path().join("site-packages");
            if sp.is_dir() {
                return Some(sp);
            }
        }
    }
    None
}

/// Create a real (pip-less, fast, offline) venv at `dir`. Returns `None` if
/// no `python` is available so the test can skip rather than fail.
fn make_venv(dir: &Path) -> Option<PathBuf> {
    for py in ["python3", "python"] {
        let ok = std::process::Command::new(py)
            .args(["-m", "venv", "--without-pip"])
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Some(dir.to_path_buf());
        }
    }
    None
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
fn class_construction_prefers_init_over_instance_call() {
    assert_fixed(
        "class C:\n    def __init__(self, x: int) -> None: ...\n    def __call__(self, document: object) -> None: ...\nC(1)\n",
        "class C:\n    def __init__(self, x: int) -> None: ...\n    def __call__(self, document: object) -> None: ...\nC(x=1)\n",
    );
}

#[test]
fn list_subclass_construction_does_not_use_instance_call_signature() {
    assert_unchanged("class C(list):\n    def __call__(self, document):\n        pass\n\nC([])\n");
}

#[test]
fn instance_call_without_dunder_call_does_not_use_constructor_signature() {
    assert_unchanged("class C:\n    def __init__(self, x: int) -> None: ...\n\nc = C(x=1)\nc(2)\n");
}

#[test]
fn rewrites_method_excluding_self() {
    assert_fixed(
        "class C:\n    def m(self, a: int, b: int) -> None: ...\nc = C()\nc.m(1, 2)\n",
        "class C:\n    def m(self, a: int, b: int) -> None: ...\nc = C()\nc.m(a=1, b=2)\n",
    );
}

#[test]
fn standalone_function_named_self_is_not_skipped() {
    // Regression (PR #24 review): a standalone function may name its first
    // parameter `self`. It is called by name with `self` passed explicitly,
    // so the receiver must NOT be skipped: `f(1, 2)` -> `f(self=1, a=2)`,
    // not the wrong `f(a=1, b=2)`.
    assert_fixed(
        "def f(self, a, *, b=10) -> None: ...\nf(1, 2)\n",
        "def f(self, a, *, b=10) -> None: ...\nf(self=1, a=2)\n",
    );
}

#[test]
fn standalone_function_named_cls_is_not_skipped() {
    assert_fixed(
        "def make(cls, *, opt) -> None: ...\nmake(1)\n",
        "def make(cls, *, opt) -> None: ...\nmake(cls=1)\n",
    );
}

#[test]
fn standalone_function_named_self_round_trips() {
    assert_round_trips("def f(self, a, *, b=10) -> None: ...\nf(1, 2)\n");
}

#[test]
fn bound_method_self_still_skipped() {
    // The receiver IS implicit for an attribute-style bound call, so the
    // mapping must still start after `self`.
    assert_fixed(
        "class C:\n    def m(self, a: int, *, b: int = 1) -> None: ...\n\
         c = C()\nc.m(1)\n",
        "class C:\n    def m(self, a: int, *, b: int = 1) -> None: ...\n\
         c = C()\nc.m(a=1)\n",
    );
}

#[test]
fn unbound_class_method_keeps_explicit_receiver_positional() {
    // Issue #27: `K.m(K(), 1)` passes the receiver explicitly. It binds to
    // `self` (never keyword-passable) and stays positional; only the real
    // argument `a` is rewritten.
    assert_fixed(
        "class K:\n    def m(self, a: int) -> int:\n        return a\nK.m(K(), 1)\n",
        "class K:\n    def m(self, a: int) -> int:\n        return a\nK.m(K(), a=1)\n",
    );
}

#[test]
fn unbound_class_method_fix_round_trips() {
    assert_round_trips(
        "class K:\n    def m(self, a: int, b: int) -> int:\n        return a\nK.m(K(), 1, 2)\n",
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
fn fixes_builtins() {
    // Builtins are in scope: a single-signature builtin is rewritten using
    // its typeshed parameter names.
    assert_fixed("enumerate([1])\n", "enumerate(iterable=[1])\n");
}

#[test]
fn fixes_ty_resolved_stdlib_single_signature() {
    // Issue #94: stdlib calls that only ty resolves can be rewritten when
    // ty's hover gives one concrete, fully named signature.
    assert_fixed(
        "import math\n\nmath.isclose(1.0, 2.0, 0.1)\n",
        "import math\n\nmath.isclose(a=1.0, b=2.0, rel_tol=0.1)\n",
    );
}

#[test]
fn fixes_ty_resolved_inferred_receiver() {
    // `p` is inferred from `Path.cwd()`, not from a direct constructor call
    // the built-in resolver tracks. ty's bound-method hover maps the call-site
    // argument directly to `suffix`.
    assert_fixed(
        "from pathlib import Path\n\np = Path.cwd()\np.with_suffix(\".txt\")\n",
        "from pathlib import Path\n\np = Path.cwd()\np.with_suffix(suffix=\".txt\")\n",
    );
}

#[test]
fn fixes_ty_resolved_third_party_env_package() {
    // A package that exists only in an external venv is resolved by ty via the
    // forwarded `--python` environment, then safely rewritten from its single
    // named hover signature.
    let env_temp = tempfile::tempdir().expect("tempdir");
    let Some(venv) = make_venv(&env_temp.path().join("ext-env")) else {
        eprintln!("skipping: `python -m venv` unavailable");
        return;
    };
    let Some(site) = venv_site_packages(&venv) else {
        eprintln!("skipping: venv has no site-packages");
        return;
    };
    let pkg = site.join("extdep");
    std::fs::create_dir_all(&pkg).expect("mkdir pkg");
    std::fs::write(pkg.join("py.typed"), "").expect("py.typed");
    std::fs::write(
        pkg.join("__init__.py"),
        "def configure(host: str, port: int) -> tuple[str, int]:\n    return (host, port)\n",
    )
    .expect("pkg init");

    let proj = project("import extdep\n\nextdep.configure(\"localhost\", 8080)\n");
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(
        &proj.root,
        std::slice::from_ref(&main),
        &config,
        Some(venv.as_path()),
    )
    .expect("fix");
    assert_eq!(outcome.declined, 0);
    assert_eq!(outcome.files.len(), 1);
    assert_eq!(
        outcome.files[0].fixed,
        "import extdep\n\nextdep.configure(host=\"localhost\", port=8080)\n"
    );
}

#[test]
fn does_not_fix_overloaded_builtin() {
    // `str` is overloaded in typeshed: still flagged by the checker, but the
    // overload safety rule (not a builtins carve-out) keeps the fixer away.
    assert_unchanged("str(123)\n");
}

#[test]
fn does_not_fix_ty_resolved_ambiguous_stdlib_hover() {
    // `os.getenv` is still detected through ty, but its hover is not a single
    // concrete signature that the fixer can safely map, so it remains
    // declined and a following check will still report it.
    let proj = project("import os\n\nos.getenv(\"PATH\", \"fallback\")\n");
    let outcome = proj.fix_main_result().expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 1);
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
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(&proj.root, &[main], &config, None).expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 0);
}

#[test]
fn does_not_fix_violation_with_trailing_star_args() {
    // Flagged (3 explicit positionals > 0 allowed) but the `*rest` makes a
    // keyword rewrite unsound, so the fixer declines.
    let source = "def f(a, b): ...\nrest = []\nf(1, 2, 3, *rest)\n";
    assert_unchanged(source);
    assert!(
        project(source)
            .check_main()
            .iter()
            .any(|m| m.contains("Too many positional")),
        "starred-arg violation should still be flagged"
    );
}

#[test]
fn does_not_fix_when_surplus_maps_onto_var_keyword() {
    // `def f(a, **kw)` called `f(1, 2)`: the surplus `2` maps onto `**kw`,
    // which cannot take a keyword name, so the fixer declines.
    let source = "def f(a, **kw): ...\nf(1, 2)\n";
    assert_unchanged(source);
    assert!(
        project(source)
            .check_main()
            .iter()
            .any(|m| m.contains("Too many positional")),
        "**kwargs-surplus violation should still be flagged"
    );
}

#[test]
fn does_not_fix_generator_argument() {
    // A bare generator argument cannot be safely prefixed with `name=`, so
    // the fixer declines — but the checker still flags the call.
    let source = "def func(items): ...\nfunc(x for x in range(3))\n";
    assert_unchanged(source);
    let proj = project(source);
    assert!(
        proj.check_main()
            .iter()
            .any(|m| m.contains("Too many positional")),
        "generator call should still be flagged"
    );
}

#[test]
fn does_not_fix_walrus_argument() {
    // `func(y := 1)` — a walrus argument likewise cannot be prefixed.
    assert_unchanged("def func(a): ...\nfunc(y := 1)\n");
}

#[test]
fn rewrites_redundantly_parenthesized_argument() {
    // Issue #41: the Ruff parser drops redundant parentheses, so the arg's
    // AST span starts *inside* them. The `name=` prefix must land before the
    // parentheses (`f(a=(1), ...)`), not inside them (`f((a=1), ...)` — a
    // `SyntaxError`).
    assert_fixed(
        "def f(a, b): ...\nf((1), (2))\n",
        "def f(a, b): ...\nf(a=(1), b=(2))\n",
    );
    assert_fixed(
        "def f(a, b): ...\nf((1), 2)\n",
        "def f(a, b): ...\nf(a=(1), b=2)\n",
    );
    // Doubly parenthesized: the prefix goes before the *outermost* `(`.
    assert_fixed(
        "def f(a, b): ...\nf(((1)), 2)\n",
        "def f(a, b): ...\nf(a=((1)), b=2)\n",
    );
}

#[test]
fn redundantly_parenthesized_argument_round_trips() {
    // The rewrite must itself be clean and re-checkable (no corruption,
    // idempotent) — the core symptom reported in issue #41.
    assert_round_trips("def f(a, b): ...\nf((1), (2))\n");
}

#[test]
fn parenthesized_tuple_argument_is_not_unwrapped() {
    // A genuine parenthesized tuple is the tuple's own delimiter, not a
    // redundant wrapper, so it is preserved verbatim.
    assert_fixed(
        "def f(a, b): ...\nf((1, 2), 3)\n",
        "def f(a, b): ...\nf(a=(1, 2), b=3)\n",
    );
}

#[test]
fn fail_safe_rejects_a_rewrite_that_would_not_parse() {
    // Issue #41 (independent ask): if a rewrite would produce invalid
    // Python, the file must be left untouched and an error reported rather
    // than silently corrupted. `add(1, a=2)` would rewrite to
    // `add(a=1, a=2)` — a duplicate-keyword `SyntaxError`.
    let proj = project("def add(a, b): ...\nadd(1, a=2)\n");
    let err = proj
        .fix_main_result()
        .expect_err("rewrite must be rejected, not applied");
    let message = err.to_string();
    assert!(
        message.contains("would not parse") && message.contains("left unchanged"),
        "unexpected error message: {message}"
    );
    // `fix_paths` never writes; the source on disk is exactly as authored.
    let on_disk = std::fs::read_to_string(proj.root.join("main.py")).expect("read");
    assert_eq!(on_disk, "def add(a, b): ...\nadd(1, a=2)\n");
}

#[test]
fn all_positional_only_call_is_legal_and_unchanged() {
    let source = "def func(a, b, /): ...\nfunc(1, 2)\n";
    assert_unchanged(source);
    assert!(
        project(source).check_main().is_empty(),
        "wholly positional-only call must be accepted"
    );
}

#[test]
fn rewrites_surplus_into_keyword_only_parameter() {
    // `def func(a, *, b)` called `func(1, 2)`: the surplus maps onto the
    // keyword-only `b`, so both are rewritten.
    assert_fixed(
        "def func(a, *, b): ...\nfunc(1, 2)\n",
        "def func(a, *, b): ...\nfunc(a=1, b=2)\n",
    );
}

#[test]
fn does_not_fix_descriptor_set_call() {
    // Descriptor-protocol calls (`d.__set__(obj, value, ...)`) are never
    // rewritten even when flagged.
    assert_unchanged(
        "class Desc:\n    def __set__(self, obj, value, extra): ...\n\n\
         d = Desc()\nd.__set__(obj, value, extra)\n",
    );
}

#[test]
fn synthesized_dataclass_constructor_not_rewritten() {
    // Issue #29: synthesized constructors are reported by the checker but
    // still conservatively declined by the fixer.
    let proj = project(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(1, 2)\n",
    );
    assert!(
        proj.check_main().iter().any(|m| m.contains(r#"for "D""#)),
        "expected the dataclass call to be flagged: {:?}",
        proj.check_main()
    );
    assert_unchanged(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(1, 2)\n",
    );
}

#[test]
fn inherited_synthesized_dataclass_constructor_not_rewritten() {
    let source = "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    base: int\n\n@dataclass\nclass Child(Base):\n    child: int\n\nChild(1, 2)\n";
    let proj = project(source);
    assert!(
        proj.check_main()
            .iter()
            .any(|m| m.contains(r#"for "Child""#)),
        "expected the inherited dataclass call to be flagged: {:?}",
        proj.check_main()
    );
    assert_unchanged(source);
}

#[test]
fn synthesized_namedtuple_constructor_not_rewritten() {
    assert_unchanged(
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(1, 2)\n",
    );
}

#[test]
fn declined_count_equals_violations_left_for_check() {
    // The fixer rewrites the plain call but conservatively declines the
    // synthesized dataclass constructor (issue #29). `declined` must equal
    // the violations a following `check` still reports, so `fix` then
    // `check` is predictable rather than silently inconsistent (issue #42).
    let proj = project(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\ndef f(a, b): ...\n\nf(1, 2)\nD(1, 2)\n",
    );
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(&proj.root, std::slice::from_ref(&main), &config, None).expect("fix");
    assert_eq!(outcome.declined, 1);
    assert_eq!(outcome.files.len(), 1);
    assert_eq!(outcome.files[0].count, 1);
    // Applying the fix leaves exactly `declined` violations behind.
    std::fs::write(&main, &outcome.files[0].fixed).expect("write fixed");
    assert_eq!(proj.check_main().len(), outcome.declined);
}

#[test]
fn rewrites_decorator_factory_call() {
    // Issue #51: once `check` sees the decorator-position call, `fix`
    // rewrites it exactly as it would the same call in statement position.
    assert_fixed(
        "def retry(times: int, delay: float):\n    def w(fn): return fn\n    return w\n\n@retry(3, 0.5)\ndef a(): ...\n",
        "def retry(times: int, delay: float):\n    def w(fn): return fn\n    return w\n\n@retry(times=3, delay=0.5)\ndef a(): ...\n",
    );
}

#[test]
fn rewrites_method_decorator_factory_call() {
    assert_fixed(
        "def tag(a: int, b: int):\n    def w(fn): return fn\n    return w\n\nclass C:\n    @tag(1, 2)\n    def m(self): ...\n",
        "def tag(a: int, b: int):\n    def w(fn): return fn\n    return w\n\nclass C:\n    @tag(a=1, b=2)\n    def m(self): ...\n",
    );
}

#[test]
fn singledispatch_call_not_rewritten() {
    // @singledispatch dispatches on args[0].__class__; converting the first
    // positional arg to keyword form breaks runtime dispatch.
    assert_unchanged(
        "from functools import singledispatch\n\n@singledispatch\ndef process(node):\n    ...\n\nprocess(42)\n",
    );
}

#[test]
fn singledispatch_multi_arg_call_not_rewritten() {
    // Multiple positional args to a @singledispatch function must not be
    // rewritten: dispatch on the first positional arg would break (issue #81).
    assert_unchanged(
        "from functools import singledispatch\n\n@singledispatch\ndef fn(a, b):\n    return (a, b)\n\nfn(1, 2)\n",
    );
}

#[test]
fn does_not_double_insert_for_elif_in_function_body() {
    // Issue #80: walk_stmt in ruff 0.15.8 visits each `elif` test expression
    // twice (direct visit_expr + walk_elif_else_clause). The double insertion
    // at the same byte offset produced `name=name=arg`, which does not parse.
    assert_fixed(
        "def f(x: int) -> bool: ...\n\ndef caller():\n    if f(1):\n        pass\n    elif f(2):\n        pass\n",
        "def f(x: int) -> bool: ...\n\ndef caller():\n    if f(x=1):\n        pass\n    elif f(x=2):\n        pass\n",
    );
}

#[test]
fn does_not_double_insert_for_elif_nested_in_if_body() {
    // Nested if/elif chains inside an if branch must also be protected.
    assert_fixed(
        "def f(x: int) -> bool: ...\n\nif True:\n    if f(1):\n        pass\n    elif f(2):\n        pass\n",
        "def f(x: int) -> bool: ...\n\nif True:\n    if f(x=1):\n        pass\n    elif f(x=2):\n        pass\n",
    );
}
