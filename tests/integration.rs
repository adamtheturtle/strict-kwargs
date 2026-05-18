//! Integration tests ported from ``mypy-strict-kwargs``'s ``test_plugin.yaml``.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (it is not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use strict_kwargs::{check_paths, Config};

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

    fn check(&self) -> Vec<String> {
        let main = self.root.join("main.py");
        let config = Config::load(&self.root);
        let diagnostics = check_paths(&self.root, &[main], &config, None).expect("check");
        diagnostics
            .iter()
            .map(|d| format!("main:{}: {}", d.line, d.message()))
            .collect()
    }
}

fn check_source(source: &str) -> Vec<String> {
    TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .main(source)
        .check()
}

fn assert_error(source: &str, line: usize, contains: &str) {
    let messages = check_source(source);
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with(&format!("main:{line}:")) && m.contains(contains)),
        "expected error on line {line} containing {contains:?}, got: {messages:?}"
    );
}

fn assert_ok(source: &str) {
    let messages = check_source(source);
    assert!(messages.is_empty(), "expected no errors, got: {messages:?}");
}

fn assert_error_at(project: &TestProject, line: usize, contains: &str) {
    let messages = project.check();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with(&format!("main:{line}:")) && m.contains(contains)),
        "expected error on line {line} containing {contains:?}, got: {messages:?}"
    );
}

#[test]
fn positional_only() {
    assert_ok(
        r#"
def func(a: int, /, b: str = "default") -> None: ...
func(1)
"#,
    );
}

#[test]
fn positional() {
    assert_error(
        r"
def func(a: int) -> None: ...
func(1)
",
        3,
        "Too many positional",
    );
}

#[test]
fn positional_optional() {
    assert_error(
        r"
def func(a: int = 1) -> None: ...
func(1)
func()
",
        3,
        "Too many positional",
    );
}

#[test]
fn keyword_only() {
    assert_ok(
        r"
def func(*, a: int) -> None: ...
func(a=1)
",
    );
}

#[test]
fn keyword_only_optional() {
    assert_ok(
        r"
def func(*, a: int = 1) -> None: ...
func(a=1)
func()
",
    );
}

#[test]
fn var_positional() {
    assert_ok(
        r#"
def func(*args: str) -> None: ...
func("extra")
"#,
    );
}

#[test]
fn var_keyword() {
    assert_ok(
        r#"
def func(**kwargs: str) -> None: ...
func(a="extra")
"#,
    );
}

#[test]
fn positional_followed_by_var_positional() {
    assert_ok(
        r"
def func(a: int, *args: str) -> None: ...
func(1)
",
    );
}

#[test]
fn positional_optional_followed_by_var_positional() {
    assert_ok(
        r"
def func(a: int = 1, *args: str) -> None: ...
func(1)
func()
",
    );
}

#[test]
fn positional_followed_by_var_keyword() {
    assert_error(
        r"
def func(a: int, **kwargs: str) -> None: ...
func(1)
",
        3,
        "Too many positional",
    );
}

#[test]
fn var_positional_followed_by_keyword() {
    assert_ok(
        r#"
def func(*args: str, a: int) -> None: ...
func("a", a=1)
"#,
    );
}

#[test]
fn method() {
    assert_error(
        r"
class C:
    def __init__(self) -> None: ...
    def method(self, a: int) -> None: ...
c = C()
c.method(1)
",
        6,
        "Too many positional",
    );
}

#[test]
fn unbound_first_party_method_receiver_not_flagged() {
    // Issue #27: `K.n(K())` is an unbound-method call resolved by the
    // built-in resolver (first-party class). The explicit receiver binds to
    // `self` and is never keyword-passable, so it must not be counted — the
    // first-party analogue of the issue #15 ty-path fix.
    assert_ok(
        r"
class K:
    def n(self) -> int:
        return 0

K.n(K())
",
    );
}

#[test]
fn unbound_first_party_method_flags_only_real_positional() {
    // `K.m(K(), 1)`: the receiver is excluded, but `a` is a genuine
    // keyword-able positional — reported as `got 1`, not `got 2`.
    let messages = check_source(
        r"
class K:
    def m(self, a: int) -> int:
        return a

K.m(K(), 1)
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:6:"), "got: {messages:?}");
    assert!(
        messages[0].contains("\"m\" of \"K\"") && messages[0].contains("got 1, maximum 0"),
        "got: {messages:?}"
    );
}

#[test]
fn bound_instance_method_still_flagged() {
    // The instance form `k.m(1)` is a normal bound call: the receiver is
    // implicit, so `1` is still over the limit (issue #27 must not regress
    // the existing instance-call behaviour).
    assert_error(
        r"
class K:
    def m(self, a: int) -> int:
        return a

k = K()
k.m(1)
",
        7,
        "got 1, maximum 0",
    );
}

#[test]
fn unbound_classmethod_via_class_still_flagged() {
    // `cls` is auto-bound even through the class, so `K.cm(1)` passes no
    // explicit receiver: `1` is a keyword-able positional and is flagged.
    assert_error(
        r"
class K:
    @classmethod
    def cm(cls, a: int) -> int:
        return a

K.cm(1)
",
        7,
        "got 1, maximum 0",
    );
}

#[test]
fn unbound_dunder_via_class_not_double_stripped() {
    // Bugbot (PR #34): a dunder-receiver callee is excluded from the issue
    // #27 strip — `max_positional_at_call_site` already drops its leading
    // receiver, so stripping `self` again would double-count `a`. The
    // explicit `K.__init__(K(), 1)` keeps the existing dunder handling
    // (positional-only `a` allowed) -> `got 2, maximum 1`, not the
    // double-stripped `got 1, maximum 0`.
    let messages = check_source(
        r"
class K:
    def __init__(self, a: int, /) -> None: ...

K.__init__(K(), 1)
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].contains("got 2, maximum 1"),
        "got: {messages:?}"
    );
}

#[test]
fn callable_class_as_decorator() {
    assert_ok(
        r"
from typing import Any

class C:
    def __call__(self, func: Any) -> None: ...

@C()
def func() -> None: ...
",
    );
}

#[test]
fn callable_class_extra_params() {
    // An *explicit* call through `__call__` gets no first-argument exemption:
    // `self` is bound by the receiver and every remaining parameter can be
    // passed by keyword, so any positional argument is flagged (issue #28).
    let messages = check_source(
        r"
from typing import Any

class C:
    def __call__(self, func: Any, a: int) -> None: ...

c = C()
c(lambda: None, 1)
c(func=lambda: None, a=1)
c(lambda: None, a=1)
",
    );
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|m| m.contains("Too many positional")));
}

/// Issue #28: a bound instance `__call__` strips `self` and grants no
/// first-positional exemption, so both the count and the flagging are exact.
#[test]
fn bound_dunder_call_strips_self_no_exemption() {
    let messages = check_source(
        r"
class C:
    def __call__(self, a: int, b: int) -> int:
        return a + b

C()(1, 2)
C()(1, b=2)
",
    );
    assert_eq!(messages.len(), 2);
    assert!(messages[0].contains("got 2, maximum 0"));
    assert!(messages[1].contains("got 1, maximum 0"));
}

#[test]
fn descriptor() {
    assert_ok(
        r"
class D:
    def __get__(self, o: object, ot: type | None = None) -> None:
        return

    def __set__(self, o: object, v: int) -> None:
        return

class C:
    a = D()

c = C()
c.a
c.a = 1
",
    );
}

#[test]
fn ignore_name() {
    let project = TestProject::new()
        .file(
            "pyproject.toml",
            r#"
[project]
name = "t"
version = "0"

[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
"#,
        )
        .main(
            r"
def func(a: int) -> None: ...
func(1)

def not_ignored(a: int) -> None: ...
not_ignored(1)

str(1)
",
        );
    assert_error_at(&project, 6, "not_ignored");
}

#[test]
fn debug() {
    let project = TestProject::new()
        .file(
            "pyproject.toml",
            r#"
[project]
name = "t"
version = "0"

[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
debug = true
"#,
        )
        .main(
            r"
def func(a: int) -> None: ...
func(1)

def not_ignored(a: int) -> None: ...
not_ignored(1)

str(1)
",
        );
    assert_error_at(&project, 6, "not_ignored");
}

/// Regression: passing a directory whose path contains a `.` (current-dir)
/// component — as happens with the documented ``strict-kwargs .`` — must
/// still discover files. ``tempfile::tempdir`` names dirs ``.tmpXXXX``, which
/// would itself be ignored, so use an explicit non-dotted prefix here.
#[test]
fn directory_with_curdir_component() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    std::fs::write(
        root.join("main.py"),
        "\ndef func(a: int) -> None: ...\nfunc(1)\n",
    )
    .expect("write main");

    let dir = root.join(".");
    let config = Config::load(&root);
    let diagnostics = check_paths(&root, &[dir], &config, None).expect("check");
    let messages: Vec<String> = diagnostics
        .iter()
        .map(|d| format!("{}: {}", d.line, d.message()))
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("3:") && m.contains("Too many positional")),
        "expected violation to be reported, got: {messages:?}"
    );
}

/// A directory walk must not look inside `.venv`, `.git`, `__pycache__`, or
/// other dot-directories: violations in real source are reported while
/// identical violations under those skipped trees are not. This pins the
/// result set that the directory-pruning optimization must leave unchanged
/// (it only stops the walk descending into trees every file of which is
/// excluded anyway).
#[test]
fn directory_walk_skips_venv_git_and_dunder_pycache() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let violation = "\ndef func(a: int) -> None: ...\nfunc(1)\n";
    for path in [
        "src/real.py",
        ".venv/lib/python3.12/site-packages/dep.py",
        ".git/hooks/hook.py",
        "venv/lib/legacy.py",
        "src/__pycache__/cached.py",
        ".hidden/secret.py",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(&file, violation).expect("write");
    }

    let config = Config::load(&root);
    let diagnostics =
        check_paths(&root, std::slice::from_ref(&root), &config, None).expect("check");
    let files: Vec<String> = diagnostics
        .iter()
        .map(|d| {
            d.path
                .strip_prefix(&root)
                .unwrap_or(&d.path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    assert_eq!(
        files,
        vec!["src/real.py".to_string()],
        "only real source should be checked; got {files:?}"
    );
}

/// Build a non-dotted project dir, write the given files, and check them all
/// (passing explicit file paths so directory-ignore rules don't interfere).
fn check_multi(files: &[(&str, &str)]) -> Vec<String> {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let mut paths = Vec::new();
    for (name, content) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dirs");
        }
        std::fs::write(&path, content).expect("write file");
        paths.push(path);
    }
    let config = Config::load(&root);
    let diagnostics = check_paths(&root, &paths, &config, None).expect("check");
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

#[test]
fn cross_module_from_import() {
    let messages = check_multi(&[
        (
            "lib.py",
            "def helper(a: int, b: int) -> int:\n    return a + b\n",
        ),
        (
            "app.py",
            "from lib import helper\n\nhelper(1, 2)\nhelper(a=1, b=2)\n",
        ),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn cross_module_from_import_aliased() {
    let messages = check_multi(&[
        ("lib.py", "def helper(a: int) -> None: ...\n"),
        ("app.py", "from lib import helper as h\n\nh(1)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn module_attribute_import() {
    let messages = check_multi(&[
        ("lib.py", "def helper(a: int) -> None: ...\n"),
        ("app.py", "import lib\n\nlib.helper(1)\nlib.helper(a=1)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn module_attribute_import_aliased() {
    let messages = check_multi(&[
        ("pkg/__init__.py", ""),
        ("pkg/lib.py", "def helper(a: int) -> None: ...\n"),
        ("app.py", "import pkg.lib as pl\n\npl.helper(1)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn relative_import() {
    let messages = check_multi(&[
        ("pkg/__init__.py", ""),
        ("pkg/lib.py", "def helper(a: int) -> None: ...\n"),
        ("pkg/app.py", "from .lib import helper\n\nhelper(1)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

/// Overloads (multiple signatures for one name, as in ``.pyi`` stubs) must be
/// treated permissively: a call OK under *any* overload is not flagged.
#[test]
fn overload_is_permissive() {
    let messages = check_multi(&[
        (
            "lib.py",
            "def f(a: int, /) -> None: ...\ndef f(a: int, b: int, /) -> None: ...\n",
        ),
        ("app.py", "from lib import f\n\nf(1, 2)\n"),
    ]);
    assert!(
        messages.is_empty(),
        "call valid under the 2-arg overload must not flag, got: {messages:?}"
    );
}

#[test]
fn overload_flags_when_all_exceed() {
    let messages = check_multi(&[
        (
            "lib.py",
            "def f(a: int) -> None: ...\ndef f(a: int, b: int) -> None: ...\n",
        ),
        ("app.py", "from lib import f\n\nf(1, 2)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn builtin_str_positional_flags() {
    assert_error(r#"str("a")"#, 1, "Too many positional");
    assert_error(r#"str("a")"#, 1, "\"str\"");
}

#[test]
fn builtin_str_keyword_ok() {
    assert_ok(r#"str(object="a")"#);
}

#[test]
fn builtin_positional_only_ok() {
    // typeshed marks these positional-only, so idiomatic calls don't flag.
    assert_ok(
        r#"
len([1])
int("1")
range(10)
isinstance(1, int)
sorted([3, 1])
print("hi", 1, 2)
"#,
    );
}

#[test]
fn typing_special_forms_not_flagged() {
    // `TypeVar`/`ParamSpec`/`TypeVarTuple`/`NewType`/`TypeAliasType` require a
    // positional string literal first argument; no type-checker-valid keyword
    // form exists, so the rule must never fire (issue #19).
    assert_ok(
        r#"
from typing import ParamSpec, TypeVar, TypeVarTuple, NewType

_P = ParamSpec("_P")
_T = TypeVar("_T")
_Ts = TypeVarTuple("_Ts")
Uid = NewType("UserId", int)
"#,
    );
    // `typing_extensions` backports resolve to the same special forms.
    assert_ok(
        r#"
from typing_extensions import ParamSpec, TypeAliasType

_P = ParamSpec("_P")
IntList = TypeAliasType("IntList", list[int])
"#,
    );
}

#[test]
fn builtin_shadowed_by_local_def() {
    // A local ``def str`` shadows the builtin; resolution must prefer it.
    assert_error(
        r#"
def str(object): ...
str("x")
"#,
        3,
        "Too many positional",
    );
}

#[test]
fn project_constructor_positional_flags() {
    // Constructor resolution: ``C(1)`` now maps to ``C.__init__``.
    assert_error(
        r"
class C:
    def __init__(self, a: int) -> None: ...
C(1)
",
        4,
        "Too many positional",
    );
}

#[test]
fn project_constructor_keyword_ok() {
    assert_ok(
        r"
class C:
    def __init__(self, a: int) -> None: ...
C(a=1)
",
    );
}

#[test]
fn builtin_ignore_name_suppresses() {
    let project = TestProject::new()
        .file(
            "pyproject.toml",
            "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nignore_names = [\"builtins.str\"]\n",
        )
        .main(r#"str("a")"#);
    assert!(
        project.check().is_empty(),
        "ignored builtin must not flag: {:?}",
        project.check()
    );
}

/// Write `aux` files to disk (sibling modules, fake venv) but only check the
/// `check` files, so resolver behavior can be exercised in isolation.
fn check_with_aux(check: &[(&str, &str)], aux: &[(&str, &str)]) -> Vec<String> {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let write = |name: &str, content: &str| {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).expect("dirs");
        std::fs::write(&path, content).expect("write");
        path
    };
    for (n, c) in aux {
        write(n, c);
    }
    let paths: Vec<_> = check.iter().map(|(n, c)| write(n, c)).collect();
    let config = Config::load(&root);
    check_paths(&root, &paths, &config, None)
        .expect("check")
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

#[test]
fn first_party_sibling_resolved_for_single_file() {
    // Only ``app.py`` is checked; ``lib.py`` is resolved via the first-party
    // root (ty-style), so the cross-module call is still enforced.
    let messages = check_with_aux(
        &[("app.py", "from lib import helper\n\nhelper(1, 2)\n")],
        &[(
            "lib.py",
            "def helper(a: int, b: int) -> int:\n    return a + b\n",
        )],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn third_party_inline_typed_package() {
    let messages = check_with_aux(
        &[(
            "app.py",
            "from mypkg import api\n\napi(1, 2)\napi(a=1, b=2)\n",
        )],
        &[
            (".venv/lib/python3.12/site-packages/mypkg/py.typed", ""),
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "def api(a, b):\n    return a\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn third_party_stub_package_pep561() {
    // A dedicated ``*-stubs`` distribution is preferred over inline source.
    let messages = check_with_aux(
        &[("app.py", "import mypkg\n\nmypkg.api(1)\n")],
        &[
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "def api(*args, **kwargs): ...\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg-stubs/__init__.pyi",
                "def api(a: int) -> None: ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn stdlib_typeshed_resolves() {
    // ``OrderedDict`` (collections) takes its arg positional-or-keyword in
    // typeshed via ``dict``; a keyword call must be accepted, proving the
    // stdlib module was resolved (not silently skipped).
    let messages = check_with_aux(
        &[(
            "app.py",
            "from collections import OrderedDict\n\nOrderedDict()\n",
        )],
        &[],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn reexport_from_submodule_in_init() {
    // ``pkg/__init__`` re-exports ``handler`` from a private submodule; a
    // call via the package must resolve through the re-export.
    let messages = check_with_aux(
        &[("app.py", "import mypkg\n\nmypkg.handler(1, 2)\n")],
        &[
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "from ._impl import handler\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg/_impl.py",
                "def handler(a, b): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_imported_name_resolves() {
    let messages = check_with_aux(
        &[("app.py", "from mypkg import handler\n\nhandler(1, 2)\n")],
        &[
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "from ._impl import handler\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg/_impl.py",
                "def handler(a, b): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_chained_through_packages() {
    // __init__ -> sub/__init__ -> _deep, a multi-hop re-export chain.
    let messages = check_with_aux(
        &[("app.py", "import mypkg\n\nmypkg.deep(1)\n")],
        &[
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "from .sub import deep\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg/sub/__init__.py",
                "from ._deep import deep\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg/sub/_deep.py",
                "def deep(a): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_star() {
    // ``from ._impl import *`` re-exports every public name.
    let messages = check_with_aux(
        &[("app.py", "import mypkg\n\nmypkg.handler(1, 2)\n")],
        &[
            (
                ".venv/lib/python3.12/site-packages/mypkg/__init__.py",
                "from ._impl import *\n",
            ),
            (
                ".venv/lib/python3.12/site-packages/mypkg/_impl.py",
                "def handler(a, b): ...\ndef other(x, /): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_first_party_package() {
    // Single-file check still resolves a sibling package's re-exported API.
    let messages = check_with_aux(
        &[("app.py", "from pkg import api\n\napi(1, 2)\n")],
        &[
            ("pkg/__init__.py", "from .core import api\n"),
            ("pkg/core.py", "def api(a, b):\n    return a\n"),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn function_scoped_import_is_not_a_module_reexport() {
    // A `from ._impl import helper` *inside a function* binds `helper` in
    // that function's scope, not the package's. It must not make
    // ``pkg.helper`` resolve, so the call below is unresolved (not flagged)
    // rather than a false "too many positional" against ``_impl.helper``.
    let messages = check_with_aux(
        &[("app.py", "import pkg\n\npkg.helper(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "def _setup():\n    from ._impl import helper\n    return helper\n",
            ),
            ("pkg/_impl.py", "def helper(a, b): ...\n"),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn reexport_assignment_alias_of_submodule_attr() {
    // ``pkg/__init__`` exposes its API via a plain assignment alias of a
    // submodule attribute (``helper = _impl.real``). The built-in resolver
    // must follow it (no ty required), so the cross-module call is enforced.
    let messages = check_with_aux(
        &[("app.py", "from pkg import helper\n\nhelper(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "from . import _impl\n\nhelper = _impl.real\n",
            ),
            ("pkg/_impl.py", "def real(a, b): ...\n"),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn reexport_assignment_alias_bare_name() {
    // ``alias = real`` where ``real`` is itself a ``from`` import: the alias
    // resolves through the import binding. Exercised via package attribute
    // access too (``pkg.alias(...)``).
    let messages = check_with_aux(
        &[("app.py", "import pkg\n\npkg.alias(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "from ._impl import real\n\nalias = real\n",
            ),
            ("pkg/_impl.py", "def real(a, b): ...\n"),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_assignment_alias_chained() {
    // ``helper = _impl.real`` then ``shortcut = helper``: the second alias
    // has no import binding for its head, so it falls back to the module
    // namespace and is filled by the re-export fixpoint.
    let messages = check_with_aux(
        &[("app.py", "from pkg import shortcut\n\nshortcut(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "from . import _impl\n\nhelper = _impl.real\nshortcut = helper\n",
            ),
            ("pkg/_impl.py", "def real(a, b): ...\n"),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn reexport_annotated_assignment_alias() {
    // An annotated alias (``handler: Callable = _impl.real``) is followed
    // just like a plain assignment.
    let messages = check_with_aux(
        &[("app.py", "from pkg import handler\n\nhandler(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "import typing\nfrom . import _impl\n\nhandler: typing.Callable = _impl.real\n",
            ),
            ("pkg/_impl.py", "def real(a, b): ...\n"),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn function_scoped_assignment_alias_is_not_a_module_reexport() {
    // ``helper = _impl.real`` *inside a function* binds in that function's
    // scope, not the package's, so ``pkg.helper`` must not resolve (no false
    // positive against ``_impl.real``).
    let messages = check_with_aux(
        &[("app.py", "import pkg\n\npkg.helper(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "from . import _impl\n\n\ndef _setup():\n    helper = _impl.real\n    return helper\n",
            ),
            ("pkg/_impl.py", "def real(a, b): ...\n"),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn assignment_from_call_is_not_an_alias() {
    // ``made = factory()`` is a value, not a re-export: it must not alias
    // ``pkg.made`` to ``factory`` (which would wrongly flag ``made(1, 2)``).
    let messages = check_with_aux(
        &[("app.py", "from pkg import made\n\nmade(1, 2)\n")],
        &[
            (
                "pkg/__init__.py",
                "from ._impl import factory\n\nmade = factory()\n",
            ),
            (
                "pkg/_impl.py",
                "def factory(a, b):\n    return lambda *x: None\n",
            ),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

/// ty-backed tests need the `ty` binary; skip cleanly when it is absent so
/// the suite is not environment-dependent.
fn ty_available() -> bool {
    std::process::Command::new("ty")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn ty_resolves_inherited_method() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_error(
        r"
class A:
    def method(self, a: int) -> None: ...

class B(A):
    pass

B().method(1)
",
        8,
        "Too many positional",
    );
}

#[test]
fn ty_resolves_return_typed_and_annotated() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    let messages = check_source(
        r"
class A:
    def method(self, a: int) -> None: ...

def make() -> A:
    return A()

def takes(x: A) -> None:
    x.method(1)

make().method(1)
A().method(a=1)
",
    );
    // x.method(1) (annotated) and make().method(1) (return-typed) flag;
    // the keyword call does not.
    assert_eq!(messages.len(), 2, "got: {messages:?}");
    assert!(messages.iter().any(|m| m.starts_with("main:9:")));
    assert!(messages.iter().any(|m| m.starts_with("main:11:")));
}

#[test]
fn ty_keyword_call_not_flagged() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_ok(
        r"
class A:
    def method(self, a: int) -> None: ...

class B(A):
    pass

B().method(a=1)
",
    );
}

#[test]
fn ty_overload_precision() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    // ty resolves the argument-matched overload; the call is flagged
    // because positional args should be keywords either way.
    assert_error(
        r"
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: int, b: int) -> str: ...
def f(a, b=0): return a

f(1, 2)
",
        10,
        "Too many positional",
    );
}

#[test]
fn ty_stdlib_via_inferred_receiver() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    let messages = check_source(
        r"
xs: list[int] = []
xs.append(1, 2)
xs.append(1)
",
    );
    // append's `object` is positional-only in typeshed: append(1) is fine,
    // append(1, 2) exceeds it. Proves stdlib resolves via ty inference.
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:3:"));
    assert!(messages[0].contains("\"append\""));
}

#[test]
fn ty_stdlib_keyword_ok() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_ok(
        r#"
s = "hello"
s.upper()
"#,
    );
}

#[test]
fn ty_unbound_method_receiver_not_flagged() {
    // Issue #15: `str.lower(key)` is an unbound-method call — `key` binds to
    // `self`. ty's hover keeps the unbound function's leading `self`
    // (`def lower(self: ...) -> ...`); pre-fix that explicit receiver was
    // counted against the limit (`got 1, maximum 0`). The receiver must not
    // count, including in a comprehension (the real-world repro).
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_ok(
        r#"
key = "Content-Type"
str.lower(key)
str.split("a b")
headers = {"Content-Type": "text/html"}
lowered = {str.lower(k) for k in headers}
"#,
    );
}

#[test]
fn ty_unbound_method_still_flags_real_extra_positional() {
    // The receiver is excluded, but a genuine keyword-able positional still
    // is: `str.encode("hello", "utf-8")` == `"hello".encode("utf-8")`, where
    // `"utf-8"` should be `encoding=`. Only that one argument is counted
    // (`got 1`), not the receiver (issue #15).
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    let messages = check_source(
        r#"
text = "hello"
str.encode(text, "utf-8")
"#,
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:3:"), "got: {messages:?}");
    assert!(
        messages[0].contains("\"encode\"") && messages[0].contains("got 1, maximum 0"),
        "got: {messages:?}"
    );
}

#[test]
fn ty_positional_only_inferred_receiver_not_flagged() {
    // Issue #14: `sys.stdout` infers to `TextIO`; ty's hover is the callable
    // *type* `(Overload[(s: …, /) -> int, …]) | Any`. `s` is positional-only,
    // so these calls cannot be rewritten and must not be flagged. (Pre-fix
    // this fell through to goto-definition on runtime stdlib source whose
    // signature drops the `/`, yielding a false positive.)
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_ok(
        r#"
import sys

sys.stdout.write("hello\n")
sys.stderr.write("oops\n")
"#,
    );
}

/// Locate the `site-packages` directory inside a freshly created venv
/// (Unix `lib/pythonX.Y/site-packages` or Windows `Lib/site-packages`).
fn venv_site_packages(venv: &std::path::Path) -> Option<PathBuf> {
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
fn make_venv(dir: &std::path::Path) -> Option<PathBuf> {
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
fn ty_forwards_external_python_env() {
    // Issue #12: a venv outside the project root (not `$VIRTUAL_ENV`, not
    // `<root>/.venv`) is invisible to the built-in resolver *and* to ty's
    // auto-discovery. Passing the `--python` value forwards it to
    // `ty server`, so the inference fallback resolves the third-party import
    // and the positional call is flagged. With `None` (the default) nothing
    // resolves and no diagnostic is produced — proving both that the flag is
    // what enables resolution and that the unset path is unchanged.
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    let env_temp = tempfile::tempdir().expect("tempdir");
    let Some(venv) = make_venv(&env_temp.path().join("ext-env")) else {
        eprintln!("skipping: `python -m venv` unavailable");
        return;
    };
    let Some(site) = venv_site_packages(&venv) else {
        eprintln!("skipping: venv has no site-packages");
        return;
    };
    // A typed third-party package that exists ONLY in the external venv.
    let pkg = site.join("extdep");
    std::fs::create_dir_all(&pkg).expect("mkdir pkg");
    std::fs::write(pkg.join("py.typed"), "").expect("py.typed");
    std::fs::write(
        pkg.join("__init__.py"),
        "def configure(host, port):\n    return (host, port)\n",
    )
    .expect("pkg init");

    let proj = tempfile::tempdir().expect("tempdir");
    let root = proj.path();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("pyproject");
    let main = root.join("main.py");
    std::fs::write(
        &main,
        "import extdep\n\nextdep.configure(\"localhost\", 8080)\nextdep.configure(host=\"localhost\", port=8080)\n",
    )
    .expect("main");
    let config = Config::load(root);

    // Unset: `extdep` is unresolvable -> no diagnostics (no regression).
    let none = check_paths(root, std::slice::from_ref(&main), &config, None).expect("check");
    assert!(
        none.is_empty(),
        "expected no diagnostics without --python, got: {none:?}"
    );

    // Forwarded: ty resolves `extdep.configure` against the external venv
    // and flags the positional call (line 3); the keyword call (line 4) is
    // fine.
    let got = check_paths(root, &[main], &config, Some(venv.as_path())).expect("check");
    let msgs: Vec<String> = got
        .iter()
        .map(|d| format!("{}: {}", d.line, d.message()))
        .collect();
    assert_eq!(got.len(), 1, "got: {msgs:?}");
    assert_eq!(got[0].line, 3, "got: {msgs:?}");
    assert!(msgs[0].contains("\"configure\""), "got: {msgs:?}");
}

#[test]
fn ty_invalid_python_env_fails_closed() {
    // A bad `--python` value must not produce wrong diagnostics: ty resolves
    // nothing against it, so the run degrades to the built-in resolver
    // exactly as if no env were configured. First-party code still resolves.
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    let proj = tempfile::tempdir().expect("tempdir");
    let root = proj.path();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("pyproject");
    let main = root.join("main.py");
    std::fs::write(
        &main,
        "def func(a, b):\n    return a\n\nfunc(1, 2)\nimport extdep\n\nextdep.configure(\"h\", 9)\n",
    )
    .expect("main");
    let config = Config::load(root);
    let bogus = root.join("does-not-exist-env");
    let got = check_paths(root, &[main], &config, Some(bogus.as_path())).expect("check");
    let msgs: Vec<String> = got
        .iter()
        .map(|d| format!("{}: {}", d.line, d.message()))
        .collect();
    // Only the first-party `func(1, 2)` is flagged; the unresolvable
    // `extdep` import yields nothing rather than a wrong diagnostic.
    assert_eq!(got.len(), 1, "got: {msgs:?}");
    assert_eq!(got[0].line, 4, "got: {msgs:?}");
}

#[test]
fn constructor_via_module_attribute() {
    // Bugbot: `import lib; lib.MyClass(1)` must resolve to
    // `lib.MyClass.__init__` (was silently skipped).
    let messages = check_with_aux(
        &[("app.py", "import lib\n\nlib.MyClass(1)\nlib.MyClass(a=1)\n")],
        &[(
            "lib.py",
            "class MyClass:\n    def __init__(self, a: int) -> None: ...\n",
        )],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn relative_import_in_package_init() {
    // Bugbot: `from .core import helper` inside `pkg/__init__.py` must
    // anchor on `pkg`, not strip to top level.
    let messages = check_with_aux(
        &[(
            "pkg/__init__.py",
            "from .core import helper\n\nhelper(1, 2)\nhelper(a=1, b=2)\n",
        )],
        &[(
            "pkg/core.py",
            "def helper(a: int, b: int) -> int:\n    return a\n",
        )],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("__init__.py:3:"));
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn local_redefinition_shadows_import() {
    // Bugbot: a locally redefined name must win over a stale `import`
    // module binding in attribute resolution.
    let messages = check_with_aux(
        &[(
            "app.py",
            "from lib import helper\n\nclass helper:\n    @staticmethod\n    def run(a: int) -> None: ...\n\nhelper.run(1)\nhelper.run(a=1)\n",
        )],
        &[("lib.py", "def helper(a: int) -> None: ...\n")],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:7:"));
}

// --- issue #29: synthesized constructors (@dataclass, NamedTuple) ---

#[test]
fn dataclass_positional_construction_flagged() {
    assert_error(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(1, 2)\n",
        8,
        r#"for "D" (got 2, maximum 0)"#,
    );
}

#[test]
fn dataclass_keyword_construction_ok() {
    assert_ok(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(x=1, y=2)\nD()\n",
    );
}

#[test]
fn namedtuple_positional_construction_flagged() {
    assert_error(
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(1, 2)\n",
        7,
        r#"for "NT" (got 2, maximum 0)"#,
    );
}

#[test]
fn namedtuple_keyword_construction_ok() {
    assert_ok(
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(a=1, b=2)\n",
    );
}

#[test]
fn dataclass_decorator_variants_flagged() {
    // Qualified, called, and argument forms all resolve to the same
    // synthesized `__init__`.
    assert_error(
        "import dataclasses\n\n@dataclasses.dataclass\nclass Q:\n    a: int\n\nQ(1)\n",
        7,
        r#"for "Q""#,
    );
    assert_error(
        "from dataclasses import dataclass\n\n@dataclass(frozen=True)\nclass F:\n    a: int\n\nF(1)\n",
        7,
        r#"for "F""#,
    );
}

#[test]
fn dataclass_init_false_not_synthesized() {
    // `@dataclass(init=False)` generates no `__init__`; nothing to flag.
    assert_ok(
        "from dataclasses import dataclass\n\n@dataclass(init=False)\nclass D:\n    a: int\n\nD()\n",
    );
}

#[test]
fn dataclass_classvar_and_field_init_false_excluded() {
    // `ClassVar` and `field(init=False)` are not `__init__` parameters, so
    // the lone real field still makes positional construction a violation.
    assert_error(
        "from dataclasses import dataclass, field\nfrom typing import ClassVar\n\n@dataclass\nclass D:\n    cv: ClassVar[int] = 0\n    real: int = 0\n    skip: int = field(init=False, default=3)\n\nD(1)\n",
        10,
        r#"for "D" (got 1, maximum 0)"#,
    );
}

#[test]
fn dataclass_explicit_init_wins_over_synthesis() {
    // A hand-written `__init__` is used as-is; the synthesized one must not
    // shadow or duplicate it.
    let messages = check_source(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    a: int\n    def __init__(self, only: int) -> None: ...\n\nD(1)\n",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:8:"));
}

#[test]
fn functional_namedtuple_form_out_of_scope() {
    // The functional `NamedTuple("N", [...])` form is not synthesized; no
    // false positive for the surrounding call.
    assert_ok(
        "from typing import NamedTuple\n\nNT = NamedTuple(\"NT\", [(\"a\", int), (\"b\", int)])\n",
    );
}
