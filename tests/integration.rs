//! Integration tests ported from ``mypy-strict-kwargs``'s ``test_plugin.yaml``.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (it is not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
#[cfg(unix)]
use strict_kwargs::CheckError;
use strict_kwargs::{check_paths, Config};

mod common;

use common::{TestProject, DEFAULT_PYPROJECT};

fn check_source(source: &str) -> Vec<String> {
    TestProject::new()
        .pyproject(DEFAULT_PYPROJECT)
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

#[cfg(unix)]
#[test]
fn unreadable_python_file_reports_io_error() {
    use std::os::unix::fs::PermissionsExt;

    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .main("def f():\n    pass\n");
    let main = project.root.join("main.py");
    std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let config = Config::load(&project.root).expect("valid config");

    let error = check_paths(
        &project.root,
        std::slice::from_ref(&main),
        &config,
        None,
        None,
    )
    .expect_err("unreadable source should fail");
    std::fs::set_permissions(&main, std::fs::Permissions::from_mode(0o600)).expect("restore chmod");

    assert!(matches!(error, CheckError::Io(_)));
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
fn unbound_imported_module_method_receiver_not_flagged() {
    let messages = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "lib.py",
            r"
class K:
    def n(self) -> int:
        return 0
",
        )
        .main(
            r"
import lib

lib.K.n(lib.K())
",
        )
        .check();
    assert!(messages.is_empty(), "got: {messages:?}");
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
    let config = Config::load(&root).expect("valid config");
    let diagnostics = check_paths(&root, &[dir], &config, None, None).expect("check");
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
        // A dot-prefixed *file* (not a directory) in real source: pruning
        // only skips directories, so this still reaches — and must stay
        // rejected by — `is_ignored_path`, keeping it the authoritative
        // filter the optimization defers to.
        "src/.generated.py",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(&file, violation).expect("write");
    }

    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, std::slice::from_ref(&root), &config, None, None).expect("check");
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

#[test]
fn directory_walk_applies_extend_exclude() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        r#"
[project]
name = "t"
version = "0"

[tool.strict_kwargs]
extend_exclude = ["generated", "vendor"]
"#,
    )
    .expect("write pyproject");
    let violation = "\ndef func(a: int) -> None: ...\nfunc(1)\n";
    for path in ["src/real.py", "generated/api.py", "pkg/vendor/dep.py"] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(&file, violation).expect("write");
    }

    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, std::slice::from_ref(&root), &config, None, None).expect("check");
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

    assert_eq!(files, vec!["src/real.py".to_string()]);
}

#[test]
fn explicit_paths_ignore_extend_exclude_unless_forced() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let pyproject = root.join("pyproject.toml");
    let file = root.join("generated").join("api.py");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
    std::fs::write(&file, "\ndef func(a: int) -> None: ...\nfunc(1)\n").expect("write source");

    std::fs::write(
        &pyproject,
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nextend_exclude = [\"generated\"]\n",
    )
    .expect("write pyproject");
    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, std::slice::from_ref(&file), &config, None, None).expect("check");
    assert_eq!(diagnostics.len(), 1);

    std::fs::write(
        &pyproject,
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nextend_exclude = [\"generated\"]\nforce_exclude = true\n",
    )
    .expect("write pyproject");
    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, std::slice::from_ref(&file), &config, None, None).expect("check");
    assert!(diagnostics.is_empty());
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
    let config = Config::load(&root).expect("valid config");
    let diagnostics = check_paths(&root, &paths, &config, None, None).expect("check");
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
fn annotated_dotted_receiver_resolves_without_ty_fallback() {
    let messages = check_multi(&[
        (
            "main.py",
            "import lib\n\n\ndef use(renderer: lib.Renderer) -> None:\n    renderer.render(1)\n",
        ),
        (
            "lib.py",
            "class Renderer:\n    def render(self, value): ...\n",
        ),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn configured_src_layout_resolves_first_party_imports() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nsrc = [\"src\"]\n",
    )
    .expect("write pyproject");
    for (name, content) in [
        (
            "src/pkg/lib.py",
            "def helper(a: int, b: int) -> int:\n    return a + b\n",
        ),
        (
            "src/app.py",
            "from pkg.lib import helper\n\nhelper(1, 2)\nhelper(a=1, b=2)\n",
        ),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, content).expect("write");
    }

    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, &[root.join("src/app.py")], &config, None, None).expect("check");

    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
    assert_eq!(diagnostics[0].line, 3);
    assert!(diagnostics[0].message().contains("Too many positional"));
}

#[test]
fn configured_namespace_package_without_init_resolves_under_src_root() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nsrc = [\"src\"]\nnamespace_packages = [\"src/acme/plugins\"]\n",
    )
    .expect("write pyproject");
    for (name, content) in [
        (
            "src/acme/plugins/service.py",
            "def run(a: int, b: int) -> None: ...\n",
        ),
        (
            "src/app.py",
            "import acme.plugins.service as service\n\nservice.run(1, 2)\n",
        ),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, content).expect("write");
    }

    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, &[root.join("src/app.py")], &config, None, None).expect("check");

    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
    assert_eq!(diagnostics[0].line, 3);
    assert!(diagnostics[0].message().contains("Too many positional"));
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
            "from typing import overload\n\n@overload\ndef f(a: int, /) -> None: ...\n@overload\ndef f(a: int, b: int, /) -> None: ...\ndef f(a: int, b: int | None = None) -> None: ...\n",
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
            "from typing import overload\n\n@overload\ndef f(a: int) -> None: ...\n@overload\ndef f(a: int, b: int) -> None: ...\ndef f(a: int, b: int | None = None) -> None: ...\n",
        ),
        ("app.py", "from lib import f\n\nf(1, 2)\n"),
    ]);
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn sequential_function_redefinition_uses_last_binding() {
    let messages = check_source(
        r"
def f(value, /):
    return value

def f(value):
    return value

f(1)
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:8:"), "got: {messages:?}");
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
    let config = Config::load(&root).expect("valid config");
    check_paths(&root, &paths, &config, None, None)
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

// `ty` is a hard requirement (it is verified up front by
// `check_paths`/`fix_paths`), so the whole suite - not just these
// `ty_`-prefixed tests - needs `ty` on `PATH`. There is therefore no
// per-test availability guard: without `ty` every test fails, which is the
// intended, deterministic behaviour. CI installs `ty` (see the workflows).

#[test]
fn builtin_resolves_inherited_method() {
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
fn builtin_resolves_unbound_inherited_method() {
    assert_error(
        r"
class Base:
    def method(self, a: int) -> None: ...

class Child(Base):
    pass

Child.method(Child(), 1)
",
        8,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_imported_inherited_method() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "base.py",
            "class A:\n    def method(self, a: int) -> None: ...\n",
        )
        .main(
            r"
from base import A

class B(A):
    pass

B().method(1)
",
        );
    assert_error_at(&project, 7, "Too many positional");
}

#[test]
fn builtin_resolves_inherited_constructor() {
    assert_error(
        r"
class Base:
    def __init__(self, a: int) -> None: ...

class Child(Base):
    pass

Child(1)
",
        8,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_inherited_dunder_call() {
    assert_error(
        r"
class Base:
    def __call__(self, a: int) -> None: ...

class Child(Base):
    pass

Child()(1)
",
        8,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_forward_constructor_receiver() {
    assert_error(
        r"
def run() -> None:
    Child().method(1)

class Base:
    def method(self, a: int) -> None: ...

class Child(Base):
    pass
",
        3,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_module_attribute_constructor_receiver() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "pkg/models.py",
            "class Base:\n    def method(self, a: int) -> None: ...\n\nclass Child(Base):\n    pass\n",
        )
        .file("pkg/__init__.py", "")
        .main(
            r"
import pkg.models

pkg.models.Child().method(1)
",
        );
    assert_error_at(&project, 4, "Too many positional");
}

#[test]
fn builtin_resolves_imported_module_constructor_receiver() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "models.py",
            "class Base:\n    def method(self, a: int) -> None: ...\n\nclass Child(Base):\n    pass\n",
        )
        .main(
            r"
import models

models.Child().method(1)
",
        );
    assert_error_at(&project, 4, "Too many positional");
}

#[test]
fn builtin_resolves_local_attribute_constructor_receiver() {
    assert_error(
        r"
class Outer:
    class Base:
        def method(self, a: int) -> None: ...

    class Child(Base):
        pass

Outer.Child().method(1)
",
        9,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_forward_attribute_constructor_receiver() {
    assert_error(
        r"
def run() -> None:
    Outer.Child().method(1)

class Outer:
    class Base:
        def method(self, a: int) -> None: ...

    class Child(Base):
        pass
",
        3,
        "Too many positional",
    );
}

#[test]
fn builtin_resolves_builtin_constructor_receiver() {
    assert_ok("list().append(1)\n");
}

#[test]
fn builtin_resolves_scalar_literal_receivers() {
    assert_ok(
        r#"
(True).bit_length()
(1.0).hex()
(1j).__format__("")
"#,
    );
}

#[test]
fn builtin_ignores_unresolved_deep_constructor_receiver() {
    assert_ok("missing.ns.Child().method(1)\n");
}

#[test]
fn builtin_ignores_dynamic_deep_constructor_receiver() {
    assert_ok(
        r"
def factory():
    return object

factory().Child.Leaf().method(1)
",
    );
}

#[test]
fn dynamic_class_base_is_ignored() {
    assert_ok(
        r"
def factory():
    return object

class Child(factory()):
    pass
",
    );
}

#[test]
fn ty_resolves_return_typed_and_annotated() {
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
fn builtin_stdlib_via_literal_assignment_receiver() {
    let messages = check_source(
        r"
xs: list[int] = []
xs.append(1, 2)
xs.append(1)
",
    );
    // append's `object` is positional-only in typeshed: append(1) is fine,
    // append(1, 2) exceeds it. The literal assignment lets the built-in
    // resolver handle this without ty inference.
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:3:"));
    assert!(messages[0].contains("\"append\""));
}

#[test]
fn ty_stdlib_keyword_ok() {
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
    let config = Config::load(root).expect("valid config");

    // Unset: `extdep` is unresolvable -> no diagnostics (no regression).
    let none = check_paths(root, std::slice::from_ref(&main), &config, None, None).expect("check");
    assert!(
        none.is_empty(),
        "expected no diagnostics without --python, got: {none:?}"
    );

    // Forwarded: ty resolves `extdep.configure` against the external venv
    // and flags the positional call (line 3); the keyword call (line 4) is
    // fine.
    let got = check_paths(root, &[main], &config, Some(venv.as_path()), None).expect("check");
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
    let config = Config::load(root).expect("valid config");
    let bogus = root.join("does-not-exist-env");
    let got = check_paths(root, &[main], &config, Some(bogus.as_path()), None).expect("check");
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

#[test]
fn decorator_factory_call_flagged() {
    // Issue #51: a call in decorator position is a call like any other and
    // its surplus positional arguments must be flagged.
    assert_error(
        r"
def retry(times: int, delay: float):
    def w(fn): return fn
    return w


@retry(3, 0.5)
def a(): ...
",
        7,
        "retry",
    );
}

#[test]
fn attribute_chain_decorator_factory_flagged() {
    // The decorator expression is an attribute-chain call (`obj.deco(...)`),
    // resolved through the recorded instance like any other method call.
    assert_error(
        r"
class R:
    def deco(self, a: int, b: int):
        def w(fn): return fn
        return w


r = R()


@r.deco(1, 2)
def c(): ...
",
        11,
        "deco",
    );
}

#[test]
fn method_decorator_factory_flagged() {
    // The blind spot also covered methods inside a class body, whose own
    // decorator list was previously skipped.
    assert_error(
        r"
def tag(a: int, b: int):
    def w(fn): return fn
    return w


class C:
    @tag(1, 2)
    def m(self): ...
",
        8,
        "tag",
    );
}

#[test]
fn class_decorator_factory_flagged() {
    assert_error(
        r#"
def register(name: str, order: int):
    def w(cls): return cls
    return w


@register("widgets", 1)
class W: ...
"#,
        7,
        "register",
    );
}

#[test]
fn keyword_decorator_factory_ok() {
    // The compliant form (already keyword) must not be flagged.
    assert_ok(
        r"
def retry(times: int, delay: float):
    def w(fn): return fn
    return w


@retry(times=3, delay=0.5)
def d(): ...
",
    );
}

// --- @singledispatch / @singledispatchmethod ---

#[test]
fn singledispatch_positional_not_flagged() {
    // Calls to @singledispatch functions must not be flagged: the dispatch
    // mechanism reads args[0].__class__, so the first argument must stay
    // positional. Bare-name import form.
    assert_ok(
        r"
from functools import singledispatch

@singledispatch
def process(node):
    ...

process(42)
",
    );
}

#[test]
fn singledispatch_qualified_not_flagged() {
    // Qualified attribute form: `functools.singledispatch`.
    assert_ok(
        r"
import functools

@functools.singledispatch
def process(node):
    ...

process(42)
",
    );
}

#[test]
fn user_defined_singledispatch_does_not_disable_checking() {
    assert_error(
        r"
def singledispatch(function):
    return function

@singledispatch
def f(value):
    return value

f(1)
",
        9,
        "Too many positional",
    );
}

#[test]
fn aliased_functools_singledispatch_not_flagged() {
    assert_ok(
        r"
from functools import singledispatch as dispatch

@dispatch
def process(node):
    ...

process(42)
",
    );
}

#[test]
fn singledispatchmethod_not_flagged() {
    // @singledispatchmethod on a class method must not be flagged.
    assert_ok(
        r"
from functools import singledispatchmethod

class C:
    @singledispatchmethod
    def process(self, node):
        ...

c = C()
c.process(42)
",
    );
}

// --- issue #81: @singledispatch call sites with multiple positional arguments ---

#[test]
fn singledispatch_multi_arg_call_not_flagged() {
    // Call sites to @singledispatch functions with multiple positional args
    // must not be flagged.
    assert_ok(
        r"
from functools import singledispatch

@singledispatch
def fn(a, b):
    return (a, b)

fn(1, 2)
",
    );
}

#[test]
fn singledispatch_imported_multi_arg_call_not_flagged() {
    // Cross-module: @singledispatch function defined in a sibling module that
    // is resolved lazily (not eagerly indexed). The re-check after `get`
    // returns None is required to catch this case (issue #81).
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "dispatch.py",
            r"
from functools import singledispatch

@singledispatch
def fn(a, b):
    return (a, b)
",
        )
        .main(
            r"
from dispatch import fn

fn(1, 2)
",
        );
    let messages = project.check();
    assert!(messages.is_empty(), "expected no errors, got: {messages:?}");
}

// --- issue #71: false positives from Callable parameters / unbound locals ---

/// A call through a `Callable`-typed parameter must not be attributed to a
/// module-level or nested function with the same name (issue #71).
#[test]
fn callable_parameter_not_flagged() {
    assert_ok(
        r"
from typing import Callable


def make_transform(
    *,
    convert: Callable[[int], str],
) -> str:
    value = 42
    return convert(value)
",
    );
}

/// Same check for a positional (non-keyword-only) Callable parameter.
#[test]
fn callable_positional_parameter_not_flagged() {
    assert_ok(
        r"
from typing import Callable


def apply(fn: Callable[[int], str], x: int) -> str:
    return fn(x)
",
    );
}

/// A Callable-typed parameter whose name matches a real nested function in the
/// same module must not be attributed to that nested function (issue #71).
#[test]
fn callable_parameter_shadowing_nested_function_not_flagged() {
    assert_ok(
        r"
from typing import Callable


def _make() -> None:
    def transform(x: int) -> int:
        return x


def apply(transform: Callable[[int], int], x: int) -> int:
    # `transform` here is the parameter, not the nested helper above.
    return transform(x)
",
    );
}

/// A Callable-typed parameter on a *method* must not produce a false positive
/// when there is a same-named function in the module (issue #71).
#[test]
fn callable_method_parameter_not_flagged() {
    assert_ok(
        r"
from typing import Callable


def helper(x: int) -> int:
    return x


class Processor:
    def run(self, helper: Callable[[int], int]) -> int:
        return helper(42)
",
    );
}

/// Class method with *args and **kwargs: those parameters must be marked
/// opaque so calls through them are never attributed to a same-named
/// function (issue #71). Also exercises the vararg/kwarg branches in the
/// class-method parameter registration code.
#[test]
fn callable_method_vararg_kwarg_parameters_not_flagged() {
    assert_ok(
        r"
from typing import Any, Callable


def process(*args: Any) -> None: ...


class Handler:
    def dispatch(
        self,
        process: Callable[..., None],
        *args: Any,
        **kwargs: Any,
    ) -> None:
        process(*args, **kwargs)
",
    );
}

/// A real positional-argument violation through a local *function def* (not a
/// parameter) must still be caught after the opaque-parameter fix (issue #71).
#[test]
fn nested_function_positional_violation_still_caught() {
    assert_error(
        r"
def outer() -> None:
    def inner(x: int) -> None: ...

    inner(1)
",
        5,
        "Too many positional",
    );
}

#[test]
fn same_named_nested_helpers_use_their_lexical_scope() {
    let messages = check_source(
        r"
def first() -> None:
    def check(value: int) -> None: ...

    check(1)


def second() -> None:
    def check(value: int, /) -> None: ...

    check(1)
",
    );

    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].starts_with("main:5:") && messages[0].contains("Too many positional"),
        "expected only the first helper call to be flagged, got: {messages:?}"
    );
}

#[test]
fn branch_local_helper_redefinition_does_not_create_overload() {
    let messages = check_source(
        r"
def caller(flag: bool) -> None:
    if flag:
        def check(value: int) -> None: ...

        check(1)
    else:
        def check(value: int, /) -> None: ...

        check(1)
",
    );

    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].starts_with("main:6:") && messages[0].contains("Too many positional"),
        "expected only the first branch helper call to be flagged, got: {messages:?}"
    );
}

#[test]
fn nested_helper_does_not_leak_to_sibling_scope() {
    assert_ok(
        r"
def owner() -> None:
    def check(value: int) -> None: ...


def sibling(check) -> None:
    check(1)
",
    );
}

#[test]
fn method_local_helper_is_not_indexed_as_class_attribute() {
    assert_ok(
        r"
class Owner:
    def method(self) -> None:
        def check(value: int) -> None: ...


Owner.check(1)
",
    );
}

// ---------------------------------------------------------------------------
// Persistent cache (issue #68)
// ---------------------------------------------------------------------------

/// Warm run with an unchanged file returns byte-identical diagnostics.
#[test]
fn cache_warm_run_returns_same_diagnostics() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");

    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let file = root.join("main.py");
    std::fs::write(&file, "def f(a, b, c): ...\nf(1, 2, 3)\n").expect("write main");

    let config = Config::load(&root).expect("config");
    // First (cold) run — populates the cache.
    let cold = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    // Second (warm) run — should hit the cache.
    let warm = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check");

    assert_eq!(
        cold, warm,
        "warm run must return byte-identical diagnostics to the cold run"
    );
}

/// Warm all-hit runs with multiple cached diagnostics still sort their output
/// deterministically after bypassing index construction.
#[test]
fn cache_all_hit_fast_path_sorts_multiple_diagnostics() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");

    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let file = root.join("main.py");
    std::fs::write(
        &file,
        "def f(a, b): ...\n\
         def g(a, b): ...\n\
         g(1, 2)\n\
         f(1, 2)\n",
    )
    .expect("write main");

    let config = Config::load(&root).expect("config");
    let cold = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    let warm = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check");

    assert_eq!(warm, cold);
    assert_eq!(warm.len(), 2);
}

/// Modifying a checked file invalidates the cache entry.
#[test]
fn cache_invalidated_on_file_change() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");

    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let file = root.join("main.py");
    // First: file with a violation.
    std::fs::write(&file, "def f(a, b, c): ...\nf(1, 2, 3)\n").expect("write main v1");

    let config = Config::load(&root).expect("config");
    let with_violation = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check v1");
    assert!(!with_violation.is_empty(), "expected a violation");

    // Second: rewrite the file to fix the violation.
    std::fs::write(&file, "def f(a, b, c): ...\nf(a=1, b=2, c=3)\n").expect("write main v2");
    let without_violation = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check v2");
    assert!(
        without_violation.is_empty(),
        "cache must be invalidated after file change; got: {without_violation:?}"
    );
}

/// Issue #253: project-local environment dependencies participate in the
/// global fingerprint even though the first-party walk prunes `.venv`.
#[test]
fn cache_invalidated_when_project_venv_dependency_changes() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache_venv")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let file = root.join("main.py");
    std::fs::write(&file, "from dep import f\n\nf(1)\n").expect("write main");
    let package = root.join(".venv/lib/python3.12/site-packages").join("dep");
    std::fs::create_dir_all(&package).expect("mkdir package");
    std::fs::write(package.join("__init__.py"), "def f(a: int) -> None: ...\n")
        .expect("write dependency");
    std::fs::write(package.join("py.typed"), "").expect("write py.typed");

    let config = Config::load(&root).expect("config");
    let before = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    assert_eq!(before.len(), 1, "expected dependency-based violation");

    // A newly installed stub changes the resolved signature. Its path is
    // nested below the pruned `.venv`, so this was previously a stale hit.
    std::fs::write(
        package.join("__init__.pyi"),
        "def f(a: int, /) -> None: ...\n",
    )
    .expect("write dependency stub");
    let after = check_paths(
        &root,
        std::slice::from_ref(&file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check after dependency change");
    assert!(
        after.is_empty(),
        "environment change must invalidate cached diagnostics: {after:?}"
    );
}

/// Undecodable-encoding files are never written to the cache — a skipped file
/// must not produce a stale "no violations" cache hit on the next run.
#[test]
fn cache_does_not_cache_skipped_file() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");

    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    // A file with invalid UTF-8 and no PEP 263 declaration — scan_file returns
    // ScanOutcome::Skipped, which means it must never be stored in the cache.
    let binary_file = root.join("binary.py");
    std::fs::write(&binary_file, [0x80u8, 0x90, 0xa0, 0xff]).expect("write binary");

    let config = Config::load(&root).expect("config");
    // Cold run — binary.py is skipped; nothing should be cached for it.
    check_paths(
        &root,
        std::slice::from_ref(&binary_file),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");

    // The cache directory must be empty: a skipped file must not produce an
    // entry (which would be an empty-diagnostics hit on the next run, masking
    // the skip warning).
    let entries: Vec<_> = std::fs::read_dir(&cache_dir)
        .expect("read cache dir")
        .collect();
    assert!(
        entries.is_empty(),
        "skipped file must not produce a cache entry; got {entries:?}"
    );
}

/// If the cache-dir path already exists as a regular file, opening the cache
/// fails and `check_paths` propagates the I/O error.
#[test]
fn cache_dir_pointing_to_file_is_an_error() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();

    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    // A regular file where the cache directory would be created.
    let cache_as_file = root.join("not_a_dir");
    std::fs::write(&cache_as_file, b"block dir creation").expect("write file at cache path");

    let config = Config::load(&root).expect("config");
    let result = check_paths(&root, &[], &config, None, Some(&cache_as_file));
    assert!(
        result.is_err(),
        "expected an error when cache-dir is a regular file"
    );
}

// `# noqa` suppression for KW001 (issue #185).

#[test]
fn noqa_bare_suppresses_violation() {
    assert_ok(
        r"
def func(a: int) -> None: ...
func(1)  # noqa
",
    );
}

#[test]
fn noqa_code_suppresses_violation() {
    assert_ok(
        r"
def func(a: int) -> None: ...
func(1)  # noqa: KW001
",
    );
}

#[test]
fn noqa_for_other_code_still_reports() {
    assert_error(
        r"
def func(a: int) -> None: ...
func(1)  # noqa: E501
",
        3,
        "Too many positional",
    );
}

#[test]
fn noqa_trailing_comment_after_real_comment_text() {
    // A trailing comment on the violating call line carrying the directive.
    assert_ok(
        r"
def func(a: int) -> None: ...
func(1)  # keep positional  # noqa: KW001
",
    );
}

#[test]
fn noqa_only_suppresses_its_own_line() {
    let messages = check_source(
        r"
def func(a: int) -> None: ...
func(1)
func(2)  # noqa: KW001
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("main:3:"), "got: {messages:?}");
}

#[test]
fn noqa_on_call_first_line_suppresses_multiline_call() {
    // The diagnostic points at the call's first line, so a directive there
    // suppresses it even when the arguments span multiple lines.
    assert_ok(
        "
def func(a: int, b: int) -> None: ...
func(  # noqa: KW001
    1,
    2,
)
",
    );
}

#[test]
fn noqa_in_string_does_not_suppress() {
    assert_error(
        r##"
def func(a: int) -> None: ...
x = "# noqa: KW001"
func(1)
"##,
        4,
        "Too many positional",
    );
}
