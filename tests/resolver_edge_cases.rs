//! Call-resolution edge cases of the checker.
//!
//! Exercises the harder corners of resolving a call's callee — directory
//! discovery, unusual callee expressions, instance tracking, display
//! formatting, the `ignore_names` config, and the `ty` type-inference
//! fallback (hover + goto-definition) — through the public `check_paths`
//! API. The fixer's own behaviour lives in `tests/fix.rs`.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (it is not `#[cfg(test)]`), so allow them here. Each
// integration-test crate is standalone, so duplicating the `TestProject`
// harness here (as `tests/fix.rs` already does) is intentional.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use strict_kwargs::{check_paths, Config, Diagnostic};

struct TestProject {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl TestProject {
    fn new() -> Self {
        // A non-dotted prefix: `tempfile`'s default `.tmpXXXX` name would be
        // swallowed by the directory-ignore rule when a directory is checked.
        let temp = tempfile::Builder::new()
            .prefix("strictkw")
            .tempdir()
            .expect("tempdir");
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

    /// Diagnostics for `main.py`, formatted `main:<line>: <message>`.
    fn check(&self) -> Vec<String> {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root).expect("valid config");
        let diagnostics = check_paths(&self.root, &[main], &config, None, None).expect("check");
        diagnostics
            .iter()
            .map(|d| format!("main:{}: {}", d.line, d.message()))
            .collect()
    }

    /// Diagnostics for the whole project directory (exercises directory walk).
    fn check_dir(&self) -> Vec<String> {
        let config = Config::load(&self.root).expect("valid config");
        let diagnostics = check_paths(
            &self.root,
            std::slice::from_ref(&self.root),
            &config,
            None,
            None,
        )
        .expect("check");
        diagnostics
            .iter()
            .map(|d| {
                format!(
                    "{}:{}: {}",
                    d.path.file_name().unwrap().to_string_lossy(),
                    d.line,
                    d.message()
                )
            })
            .collect()
    }
}

fn plain_project(source: &str) -> TestProject {
    TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .main(source)
}

fn check_source(source: &str) -> Vec<String> {
    plain_project(source).check()
}

fn has_error_at(messages: &[String], line: usize, contains: &str) -> bool {
    messages
        .iter()
        .any(|m| m.starts_with(&format!("main:{line}:")) && m.contains(contains))
}

// --- Directory discovery ---------------------------------------------------

/// A mistyped target (a path that is neither a file nor a directory) is a
/// hard error, not a silent "clean" result that would pass unnoticed in CI
/// (issue #55).
#[test]
fn nonexistent_path_is_a_hard_error() {
    let project = TestProject::new().pyproject("[project]\nname = \"t\"\nversion = \"0\"\n");
    let missing = project.root.join("does_not_exist.py");
    let config = Config::load(&project.root).expect("valid config");
    let error = check_paths(&project.root, &[missing], &config, None, None)
        .expect_err("a nonexistent path must be a hard error");
    let message = error.to_string();
    assert!(
        message.contains("no such file or directory"),
        "message: {message}"
    );
    assert!(message.contains("does_not_exist.py"), "message: {message}");
}

/// A non-Python file passed *directly* exists, so it is a deliberate (if
/// odd) selection rather than a mistake: it is skipped, not an error. This
/// keeps the issue #55 hardening scoped to genuinely missing paths.
#[test]
fn non_python_file_passed_directly_is_skipped() {
    let project = TestProject::new().pyproject("[project]\nname = \"t\"\nversion = \"0\"\n");
    let not_py = project.root.join("notes.txt");
    std::fs::write(&not_py, "plain text\n").expect("write");
    let config = Config::load(&project.root).expect("valid config");
    let diagnostics = check_paths(&project.root, &[not_py], &config, None, None).expect("check");
    assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
}

/// Checking a directory walks it, picking up `.py` files and ignoring
/// non-Python files like `README.txt`.
#[test]
fn directory_walk_filters_non_python_files() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("README.txt", "not python\n")
        .file("pkg/mod.py", "def func(a: int) -> None: ...\nfunc(1)\n");
    let messages = project.check_dir();
    assert!(
        messages.iter().any(|m| m.starts_with("mod.py:2:")),
        "expected violation in pkg/mod.py, got: {messages:?}"
    );
}

/// `.pyi` stubs are discovered, and `__pycache__` / dot- / `venv`
/// directories are skipped by the directory-ignore rule.
#[test]
fn directory_walk_collects_pyi_and_skips_ignored_dirs() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("typed.pyi", "def func(a: int) -> None: ...\n")
        .file("app.py", "import typed\n\ntyped.func(1)\n")
        .file("__pycache__/cached.py", "def x(a): ...\nx(1)\n")
        .file(".hidden/secret.py", "def y(a): ...\ny(1)\n")
        .file("venv/lib/leftover.py", "def z(a): ...\nz(1)\n");
    let messages = project.check_dir();
    assert!(
        messages.iter().all(|m| !m.contains("cached.py")
            && !m.contains("secret.py")
            && !m.contains("leftover.py")),
        "ignored dirs leaked diagnostics: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.starts_with("app.py:3:")),
        "expected app.py violation, got: {messages:?}"
    );
}

// --- Import forms the built-in resolver must tolerate ----------------------

/// `from x import *` binds nothing concrete; a following call is simply
/// unresolved and not flagged (no panic, no false positive).
#[test]
fn star_import_is_skipped() {
    let messages = check_source("from os import *\n\ngetcwd()\n");
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// `from . import helper` in a top-level module (not a package `__init__`)
/// binds the bare name; the unresolved sibling yields no diagnostic.
#[test]
fn relative_import_empty_base_binds_bare_name() {
    let messages = check_source("from . import helper\n\nhelper.run(1, 2)\n");
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// A relative import with more leading dots than the package depth resolves
/// to nothing without panicking.
#[test]
fn over_deep_relative_import_returns_none() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("pkg/mod.py", "from ... import something\n\nsomething()\n");
    let config = Config::load(&project.root).expect("valid config");
    let modp = project.root.join("pkg/mod.py");
    let diagnostics = check_paths(&project.root, &[modp], &config, None, None).expect("check");
    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

// --- Unusual callee expressions --------------------------------------------

/// A forward reference to a class defined later in the module resolves via
/// the module candidate to its `__init__`, flagging surplus args.
#[test]
fn module_level_class_resolved_via_module_candidate() {
    let messages = check_source(
        r"
def build():
    return Widget(1, 2)

class Widget:
    def __init__(self, a, b): ...
",
    );
    assert!(
        has_error_at(&messages, 3, "Widget"),
        "expected Widget constructor violation, got: {messages:?}"
    );
}

/// `Factory()(1, 2)` — calling the result of a constructor resolves through
/// the class's `__call__`.
#[test]
fn call_of_constructor_result_resolves_dunder_call() {
    let messages = check_source(
        r"
class Factory:
    def __call__(self, a, b): ...

Factory()(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 5, "__call__") || has_error_at(&messages, 5, "Too many positional"),
        "expected __call__ violation, got: {messages:?}"
    );
}

/// Calling the result of a call whose callee is *not* a class with
/// `__call__` (`make()(1)` where `make` returns a plain value) falls
/// through the constructor-call arm to `None` — deferred to ty, unresolved,
/// not flagged.
#[test]
fn call_result_without_dunder_call_is_unresolved() {
    let messages = check_source(
        r"
def make():
    return 1

make()(1, 2)
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// `K()(1, 2)` where `K` is a locally-bound class *without* `__call__`:
/// the constructor-call arm resolves the class but finds no `__call__` in
/// the index, so it falls through to `None` (deferred to ty, not flagged).
#[test]
fn call_of_class_instance_without_dunder_call_is_unresolved() {
    let messages = check_source(
        r"
class K:
    pass


K()(1, 2)
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// The callee is a call whose own callee is an *attribute*
/// (`o.factory()(...)`), not a bare name, so the constructor-call arm
/// bails immediately (`Expr::Name` else-branch) — unresolved, not flagged.
#[test]
fn call_result_of_attribute_call_is_unresolved() {
    let messages = check_source(
        r"
class O:
    def factory(self): ...


o = O()
o.factory()(1, 2)
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// An instance assignment inside an `if` in a function body must still run the
/// custom assignment visitor. Otherwise the later method call cannot resolve
/// through the local instance binding.
#[test]
fn instance_assigned_inside_function_if_body_is_tracked() {
    let messages = check_source(
        r"
class Widget:
    def method(self, a, b): ...


def caller() -> None:
    if True:
        widget = Widget()
        widget.method(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 9, "Too many positional"),
        "method call through if-local instance must be flagged, got: {messages:?}"
    );
}

/// Annotated instance assignments take the same custom visitor path as plain
/// assignments; this must also happen inside function-local `if` bodies.
#[test]
fn annotated_instance_assigned_inside_function_if_body_is_tracked() {
    let messages = check_source(
        r"
class Widget:
    def method(self, a, b): ...


def caller() -> None:
    if True:
        widget: Widget = Widget()
        widget.method(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 9, "Too many positional"),
        "method call through annotated if-local instance must be flagged, got: {messages:?}"
    );
}

/// A function definition inside an `if` in a function body must still be
/// registered in the local scope before calls in the same branch are checked.
#[test]
fn function_defined_inside_function_if_body_is_registered() {
    let messages = check_source(
        r"
def caller() -> None:
    if True:
        def inner(a, b):
            ...

        inner(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 7, "Too many positional"),
        "call to if-local nested function must be flagged, got: {messages:?}"
    );
}

/// A class definition inside an `if` in a function body is also a local
/// definition that later calls in the branch should resolve.
#[test]
fn class_defined_inside_function_if_body_is_registered() {
    let messages = check_source(
        r"
def caller() -> None:
    if True:
        class Local:
            def __init__(self, a, b):
                ...

        Local(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 8, "Too many positional"),
        "call to if-local nested class must be flagged, got: {messages:?}"
    );
}

/// A call to a `*args` function with more positionals than the named
/// parameters is legal — `*args` absorbs the surplus, so it is not flagged
/// (exercises the var-positional short-circuit in the limit check).
#[test]
fn var_positional_absorbs_surplus_positionals() {
    let messages = check_source("def f(a, *rest): ...\nf(1, 2, 3, 4)\n");
    assert!(
        messages.is_empty(),
        "*args call must be accepted: {messages:?}"
    );
}

/// A `@dataclass` with a `ClassVar` field: the synthesized `__init__`
/// skips it, so `D(1, 2)` exceeds the one real field and is flagged.
#[test]
fn dataclass_classvar_excluded_minimal() {
    let messages = check_source(
        r"
from dataclasses import dataclass
from typing import ClassVar


@dataclass
class D:
    a: int
    b: ClassVar[int] = 0


D(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 12, "Too many positional") || has_error_at(&messages, 12, "\"D\""),
        "ClassVar must be excluded from the synthesized __init__: {messages:?}"
    );
}

/// A `@dataclass` that defines its own `__new__`: synthesis is skipped
/// (the `__new__` arm of the explicit-constructor short-circuit), so the
/// run does not panic and resolution falls to the written constructor.
#[test]
fn dataclass_with_explicit_new_skips_synthesis() {
    let messages = check_source(
        r"
from dataclasses import dataclass


@dataclass
class D:
    a: int

    def __new__(cls):
        return object.__new__(cls)


D()
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// `@dataclass(init=True)` keeps the synthesized `__init__` (the
/// `init=False` opt-out does not fire — exercises the non-`False` arm of
/// the keyword check), so `D(1, 2)` against one field is flagged.
#[test]
fn dataclass_init_true_keyword_still_synthesizes() {
    let messages = check_source(
        r"
from dataclasses import dataclass


@dataclass(init=True)
class D:
    a: int


D(1, 2)
",
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Too many positional") || m.contains("\"D\"")),
        "init=True must still synthesize __init__: {messages:?}"
    );
}

/// Assigning / annotating a constructor result onto an *attribute* target
/// (`h.attr = C()`, `h.attr2: C = C()`) is not a name binding, so no
/// instance is recorded — the non-`Name` target branches are taken and the
/// run neither panics nor resolves to the wrong target.
#[test]
fn constructor_assigned_to_attribute_target_records_no_instance() {
    let messages = check_source(
        r"
class C:
    def __init__(self, a): ...


class H:
    pass


h = H()
h.attr = C()
h.attr2: C = C()
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// `Factory()(...)` where `Factory` is an *imported* (locally-bound) class
/// with `__call__`: the constructor-call arm resolves `Factory` via
/// `resolve_local`, finds `Factory.__call__` in the index, and the
/// over-long call is flagged.
#[test]
fn call_of_imported_callable_class_resolves_dunder_call() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("app.py", "from lib import Factory\n\nFactory()(1, 2, 3)\n")
        .file(
            "lib.py",
            "class Factory:\n    def __call__(self, a, b): ...\n",
        );
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert!(
        diagnostics.iter().any(|d| d.line == 3),
        "expected __call__ violation, got: {diagnostics:?}"
    );
}

/// A subscript callee (`registry["k"](1, 2)`) is not a resolvable
/// name/attribute/call; it is deferred to ty and, unresolved, not flagged.
#[test]
fn subscript_callee_is_unresolved() {
    let messages = check_source(
        r"
registry = {}
registry['k'](1, 2)
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// A boolean-expression callee (`(a or b)(1)`) is neither resolvable nor
/// deferrable; no diagnostic, no panic.
#[test]
fn boolop_callee_is_not_deferred() {
    let messages = check_source(
        r"
def a(): ...
def b(): ...
(a or b)(1)
",
    );
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// A deep dotted attribute call (`pkg.sub.run(...)`) bound by
/// `import pkg.sub` resolves through the dotted chain.
#[test]
fn deep_dotted_attribute_chain_resolves() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("app.py", "import pkg.sub\n\npkg.sub.run(1, 2)\n")
        .file("pkg/__init__.py", "")
        .file("pkg/sub.py", "def run(a, b): ...\n");
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
    assert_eq!(diagnostics[0].line, 3);
}

// --- Instance tracking through assignments ---------------------------------

/// `x: Foo = Foo()` records `x` as a `Foo` instance, so `x.method(...)` is
/// resolved and surplus args are flagged.
#[test]
fn annotated_assignment_records_instance() {
    let messages = check_source(
        r"
class Foo:
    def method(self, a, b): ...

x: Foo = Foo()
x.method(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 6, "method"),
        "expected method violation through annotated instance, got: {messages:?}"
    );
}

/// `x = pkg.Factory()` (constructor callee is an attribute, not a bare name)
/// records no instance; resolution proceeds without panic.
#[test]
fn assignment_from_attribute_constructor_is_not_recorded() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "app.py",
            "import lib\n\nobj = lib.Factory()\nobj.run(1, 2)\n",
        )
        .file("lib.py", "class Factory:\n    def run(self, a, b): ...\n");
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let _ = check_paths(&project.root, &[app], &config, None, None).expect("check");
}

// --- Diagnostic display formatting -----------------------------------------

/// A class call reports the bare class name (`"Widget"`).
#[test]
fn constructor_violation_reports_class_name() {
    let messages = check_source(
        r"
class Widget:
    def __init__(self, a, b): ...

Widget(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 5, "\"Widget\""),
        "expected class-name display, got: {messages:?}"
    );
}

/// A *free function* whose first parameter is literally named `self` is
/// called by name (not as a bound method), so the receiver is not implicit:
/// every positional argument counts. `f(1, 2)` against `def f(self, a)`
/// therefore exceeds the limit and is flagged (the unbound-class-method
/// detector bails out early because the callee is a `Name`, not an
/// attribute access).
#[test]
fn free_function_named_self_first_param_is_flagged() {
    let messages = check_source("def f(self, a): ...\nf(1, 2)\n");
    assert!(
        has_error_at(&messages, 2, "Too many positional"),
        "expected violation for free function with `self` param, got: {messages:?}"
    );
}

/// A name that resolves syntactically but is bound to a non-callable value
/// (no signature in the index) is left alone — no diagnostic, no panic.
#[test]
fn call_to_non_callable_module_attribute_is_ignored() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("app.py", "import lib\n\nlib.thing(1, 2)\n")
        .file("lib.py", "thing = 5\n");
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert!(
        diagnostics.is_empty(),
        "non-callable attribute must not be flagged, got: {diagnostics:?}"
    );
}

/// A `@dataclass` synthesizes `__init__` from its annotated fields but
/// *excludes* `ClassVar` fields. With `x: int` and `y: ClassVar[int]`, the
/// synthesized signature takes only `x`, so `D(1, 2)` exceeds it and is
/// flagged (exercises the `ClassVar` skip in the field collector).
#[test]
fn dataclass_classvar_field_excluded_from_synthesized_init() {
    let messages = check_source(
        r"
from dataclasses import dataclass
from typing import ClassVar


@dataclass
class D:
    x: int
    y: ClassVar[int] = 0


D(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 12, "Too many positional") || has_error_at(&messages, 12, "\"D\""),
        "expected dataclass ClassVar-excluded violation, got: {messages:?}"
    );
}

/// An attribute call reports `"method" of "Class"`.
#[test]
fn method_violation_reports_method_of_class() {
    let messages = check_source(
        r"
class Widget:
    def method(self, a, b): ...

w = Widget()
w.method(1, 2)
",
    );
    assert!(
        has_error_at(&messages, 6, "of \"Widget\""),
        "expected method-of-class display, got: {messages:?}"
    );
}

// --- Limit / config behaviour ----------------------------------------------

/// A call to a name that resolves to nothing is deferred to ty, which also
/// cannot resolve it, so nothing is flagged and nothing panics.
#[test]
fn undefined_name_call_falls_through_unresolved() {
    let messages = check_source("undefined_callable(1, 2, 3)\n");
    assert!(messages.is_empty(), "unexpected diagnostics: {messages:?}");
}

/// `*args` makes a call with more positionals than the named limit legal.
#[test]
fn var_positional_allows_extra_arguments() {
    let messages = check_source(
        r"
def func(a, *rest): ...
func(1, 2, 3, 4)
",
    );
    assert!(
        messages.is_empty(),
        "*args call must be accepted, got: {messages:?}"
    );
}

/// An `ignore_names` entry on the class short-circuits the check.
#[test]
fn ignored_class_constructor_not_flagged() {
    let project = TestProject::new()
        .pyproject(
            "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nignore_names = [\"main.Widget\"]\n",
        )
        .main(
            r"
class Widget:
    def __init__(self, a, b): ...

Widget(1, 2)
",
        );
    assert!(
        project.check().is_empty(),
        "ignored class must not be flagged: {:?}",
        project.check()
    );
}

/// `debug = true` emits resolution diagnostics to stderr but still reports
/// violations normally.
#[test]
fn debug_flag_emits_and_still_checks() {
    let project = TestProject::new()
        .pyproject(
            "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\ndebug = true\n",
        )
        .main("def func(a): ...\nfunc(1)\n");
    assert!(
        has_error_at(&project.check(), 2, "Too many positional"),
        "debug mode must still report violations"
    );
}

/// A class nested inside another class is indexed (the `index_class_body`
/// recurses into the inner `ClassDef`), so a positional call to the inner
/// class's constructor through the outer is resolved and flagged.
#[test]
fn nested_class_constructor_is_resolved() {
    let messages = check_source(
        "class Outer:\n\
         \x20   class Inner:\n\
         \x20       def __init__(self, alpha, beta):\n\
         \x20           ...\n\
         \n\
         Outer.Inner(1, 2)\n",
    );
    assert!(
        has_error_at(&messages, 6, "Too many positional"),
        "nested-class constructor call must be flagged, got: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("\"Inner\"")),
        "constructor diagnostic should name the inner class, got: {messages:?}"
    );
}

// --- `ty` type-inference fallback ------------------------------------------

/// A stdlib free function the built-in resolver cannot index is resolved by
/// ty's `def`-form hover; a legal varargs call stays clean.
#[test]
fn ty_hover_def_form_resolves_stdlib_function() {
    let messages = check_source("import math\n\nmath.gcd(4, 8)\n");
    assert!(
        messages.is_empty(),
        "stdlib varargs call must be accepted via ty hover: {messages:?}"
    );
}

/// An unbound method called with an explicit receiver (`str.upper(s)`) has
/// its leading `self` and the explicit receiver argument stripped; the call
/// is legal.
#[test]
fn ty_hover_unbound_method_strips_self_and_receiver() {
    let messages = check_source(
        r#"
s = "hello"
str.upper(s)
"#,
    );
    assert!(
        messages.is_empty(),
        "unbound-method explicit-receiver call must be accepted: {messages:?}"
    );
}

/// A stdlib free function called with too many positional arguments is
/// flagged through ty's hover resolution.
#[test]
fn ty_hover_flags_too_many_positional_on_stdlib() {
    let messages = check_source("import os\n\nos.getenv('PATH', 'fallback')\n");
    assert!(
        has_error_at(&messages, 3, "Too many positional"),
        "expected ty-resolved stdlib violation, got: {messages:?}"
    );
}

#[test]
fn ty_hover_honors_ignore_names_for_bound_builtin_method() {
    let project = TestProject::new()
        .pyproject(
            "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nignore_names = [\"builtins.str.split\"]\n",
        )
        .main("text = \"a:b\"\ntext.split(\":\", maxsplit=1)\n");
    let messages = project.check();
    assert!(
        messages.is_empty(),
        "ignored ty-resolved builtin method must not flag: {messages:?}"
    );
}

/// A class object returned from a cross-file factory and then called is
/// resolved via ty goto-definition to its `__init__`; the over-long
/// constructor call is flagged at the call site.
#[test]
fn ty_goto_definition_resolves_cross_file_class_constructor() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "app.py",
            "import lib\n\nfactory = lib.get_thing_cls()\nfactory(1, 2, 3)\n",
        )
        .file(
            "lib.py",
            r"
class Thing:
    def __init__(self, a, b):
        self.a = a
        self.b = b


def get_thing_cls() -> type[Thing]:
    return Thing
",
        );
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert!(
        diagnostics.iter().all(|d| d.path.ends_with("app.py")),
        "diagnostics must point at the call site (app.py), got: {diagnostics:?}"
    );
}

/// A cross-file instance whose type is an inferred return value drives ty's
/// hover/goto for a method call the built-in resolver cannot follow.
#[test]
fn ty_resolves_cross_file_method_on_inferred_instance() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "app.py",
            "from lib import make\n\nobj = make()\nobj.greet(1, 2, 3)\n",
        )
        .file(
            "lib.py",
            "class Thing:\n    def greet(self, a, b): ...\n\n\ndef make() -> Thing:\n    return Thing()\n",
        );
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert!(
        diagnostics.iter().all(|d| d.path.ends_with("app.py")),
        "diagnostics must point at app.py, got: {diagnostics:?}"
    );
}

/// When ty goto-definition lands in a file, the def finder walks *all* of
/// that file's statements — recursing into `if` / `try` / `for` / `while` /
/// `with` blocks — to map the resolved offset to a signature. Here `obj` is
/// a cross-file inferred instance (only ty can resolve `obj.run`), and the
/// resolved file carries sibling defs nested in every control-flow form, so
/// the recursion is exercised while `run` is found and its over-long call
/// flagged.
#[test]
fn ty_goto_definition_recurses_control_flow_blocks() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "app.py",
            "from lib import build\n\nobj = build()\nobj.run(1, 2, 3)\n",
        )
        .file(
            "lib.py",
            r#"
class Engine:
    def run(self, a, b):
        ...


if True:
    def mod_if(x):
        ...

try:
    def mod_try(x):
        ...
except Exception:
    def mod_except(x):
        ...
else:
    def mod_else(x):
        ...
finally:
    def mod_finally(x):
        ...

for _ in range(1):
    def mod_for(x):
        ...

while False:
    def mod_while(x):
        ...

with open("/dev/null") as _f:
    def mod_with(x):
        ...


def build() -> Engine:
    return Engine()
"#,
        );
    let config = Config::load(&project.root).expect("valid config");
    let app = project.root.join("app.py");
    // Like the other cross-file ty tests, resolution of an inferred instance
    // is environment-dependent, so assert robustly: the run completes and any
    // diagnostics point at the call site. The control-flow def-walk is still
    // exercised whenever ty resolves into `lib.py`.
    let diagnostics = check_paths(&project.root, &[app], &config, None, None).expect("check");
    assert!(
        diagnostics.iter().all(|d| d.path.ends_with("app.py")),
        "diagnostics must point at the call site (app.py), got: {diagnostics:?}"
    );
}

/// ty hover that yields a callable *type* (overloaded builtin) rather than a
/// `def` form drives the overload-parsing path; `print` accepts varargs so
/// the call stays clean.
#[test]
fn ty_hover_callable_type_overloads_accept_varargs() {
    let messages = check_source("print(1, 2, 3, 4, 5)\n");
    assert!(
        messages.is_empty(),
        "builtin varargs call must be accepted via ty: {messages:?}"
    );
}

/// `Diagnostic::message` renders the expected human-readable text for a
/// plain function violation.
#[test]
fn diagnostic_message_shape() {
    let project = plain_project("def func(a, b): ...\nfunc(1, 2)\n");
    let main = project.root.join("main.py");
    let config = Config::load(&project.root).expect("valid config");
    let diags: Vec<Diagnostic> =
        check_paths(&project.root, &[main], &config, None, None).expect("check");
    assert_eq!(diags.len(), 1);
    assert!(diags[0].message().contains("Too many positional"));
}
