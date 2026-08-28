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
fn pep484_double_underscore_is_positional_only() {
    // A leading double underscore is the pre-PEP-570 spelling of
    // positional-only, which the typing spec still requires checkers to
    // honor. There is no keyword form to rewrite to, so a call passing it
    // positionally is correct as written (issue #1247).
    assert_ok(
        r"
def legacy(__value: str, *, upper: bool = False) -> str: ...
legacy('x')
",
    );
}

#[test]
fn dunder_parameter_name_is_still_keyword_passable() {
    // A trailing double underscore takes the name out of the convention.
    assert_error(
        r"
def func(__value__: int) -> None: ...
func(1)
",
        3,
        "Too many positional",
    );
}

#[test]
fn pep484_positional_only_does_not_cover_later_parameters() {
    // `__first` is positional-only; `second` still has a keyword form.
    assert_error(
        r"
def func(__first: int, second: int) -> None: ...
func(1, 2)
",
        3,
        "Too many positional",
    );
}

#[test]
fn pep484_double_underscore_method_parameter_is_positional_only() {
    assert_ok(
        r"
class C:
    def method(self, __value: int) -> None: ...
    def __init__(self, __value: int) -> None: ...
C(1).method(1)
",
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
fn inherited_stdlib_constructor_retains_signature_owner() {
    assert_error(
        r"
from logging import StreamHandler
class LocalHandler(StreamHandler):
    pass
LocalHandler(None)
",
        5,
        "\"StreamHandler\"",
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
    assert_eq!(diagnostics.len(), 0);
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
fn configured_src_search_order_prefers_earlier_root() {
    // Issue #633: configured `src` roots must keep user order for imports.
    let temp = tempfile::Builder::new()
        .prefix("strictkw")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nsrc = [\"asrc\", \"zsrc\"]\n",
    )
    .expect("write pyproject");
    for (name, content) in [
        ("asrc/orderdep.py", "def target(value, /) -> None: ...\n"),
        ("zsrc/orderdep.py", "def target(value) -> None: ...\n"),
        ("main.py", "from orderdep import target\n\ntarget(1)\n"),
    ] {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dirs");
        }
        std::fs::write(path, content).expect("write");
    }

    let config = Config::load(&root).expect("valid config");
    let diagnostics =
        check_paths(&root, &[root.join("main.py")], &config, None, None).expect("check");

    assert!(
        diagnostics.is_empty(),
        "asrc's positional-only signature should win; got: {diagnostics:?}"
    );
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
fn constructor_respects_local_new_positional_only_boundary() {
    assert_ok(
        r"
class C:
    def __new__(cls, value, /):
        return super().__new__(cls)

    def __init__(self, value):
        self.value = value

C(1)
",
    );
}

#[test]
fn constructor_with_keywordable_new_still_flags() {
    assert_error(
        r"
class C:
    def __new__(cls, value): ...
    def __init__(self, value): ...
C(1)
",
        5,
        "Too many positional",
    );
}

#[test]
fn constructor_flags_only_surplus_after_local_new_boundary() {
    assert_error(
        r"
class C:
    def __new__(cls, value, /, other): ...
    def __init__(self, value, other): ...
C(1, 2)
",
        5,
        "maximum 1",
    );
}

#[test]
fn constructor_allowance_does_not_hide_arguments_beyond_init_arity() {
    assert_error(
        r"
class C:
    def __new__(cls, first, second, /): ...
    def __init__(self, first): ...
C(1, 2)
",
        5,
        "maximum 1",
    );
}

#[test]
fn constructor_respects_metaclass_call_positional_only_boundary() {
    assert_ok(
        r"
class Meta(type):
    def __call__(cls, value, /):
        return super().__call__(value=value)

class C(metaclass=Meta):
    def __init__(self, value):
        self.value = value

C(1)
",
    );
}

#[test]
fn metaclass_allowance_does_not_hide_arguments_beyond_init_arity() {
    assert_error(
        r"
class Meta(type):
    def __call__(cls, first, second, /): ...
class C(metaclass=Meta):
    def __init__(self, first): ...
C(1, 2)
",
        6,
        "maximum 1",
    );
}

#[test]
fn constructor_flags_only_surplus_after_metaclass_call_boundary() {
    assert_error(
        r"
class Meta(type):
    def __call__(cls, value, /, other): ...
class C(metaclass=Meta):
    def __init__(self, value, other): ...
C(1, 2)
",
        6,
        "maximum 1",
    );
}

#[test]
fn callable_instance_constructor_boundary_does_not_affect_dunder_call() {
    assert_error(
        r"
class C:
    def __init__(self, seed, /): ...
    def __call__(self, value): ...
c = C(0)
c(1)
",
        6,
        "Too many positional",
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
            (".venv/lib/python3.12/site-packages/mypkg/py.typed", ""),
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
            (".venv/lib/python3.12/site-packages/mypkg/py.typed", ""),
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
            (".venv/lib/python3.12/site-packages/mypkg/py.typed", ""),
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
            (".venv/lib/python3.12/site-packages/mypkg/py.typed", ""),
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

#[test]
fn adjacent_pyi_stub_wins_over_py_implementation() {
    let messages = check_with_aux(
        &[(
            "app.py",
            "from dep import false_positive, source_only\n\nsource_only(1)\nfalse_positive(1)\n",
        )],
        &[
            (
                "dep.py",
                "def source_only(*args: object) -> None: ...\ndef false_positive(value: int) -> None: ...\n",
            ),
            (
                "dep.pyi",
                "def source_only(value: int) -> None: ...\ndef false_positive(value: int, /) -> None: ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].contains("source_only"),
        "stub should require kwargs for source_only, got: {messages:?}"
    );
}

#[test]
fn package_wins_over_same_name_module_file() {
    let messages = check_with_aux(
        &[("app.py", "import collision\n\ncollision.target(1)\n")],
        &[
            ("collision.py", "def target(value: int, /) -> None: ...\n"),
            (
                "collision/__init__.py",
                "def target(value: int) -> None: ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].starts_with("app.py:3:"));
}

#[test]
fn star_reexport_honors_dunder_all() {
    let messages = check_with_aux(
        &[("app.py", "from facade import hidden\n\nhidden(1)\n")],
        &[
            (
                "source.py",
                "__all__ = [\"public\"]\n\
                 def public(value: int, /) -> None: ...\n\
                 def hidden(value: int) -> None: ...\n",
            ),
            ("facade.py", "from source import *\n"),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn star_reexport_honors_annotated_dunder_all() {
    let messages = check_with_aux(
        &[("app.py", "from facade import hidden\n\nhidden(1)\n")],
        &[
            (
                "source.py",
                "__all__: list[str] = [\"public\"]\n\
                 def public(value: int, /) -> None: ...\n\
                 def hidden(value: int) -> None: ...\n",
            ),
            ("facade.py", "from source import *\n"),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn star_reexport_omits_leading_underscore_names() {
    let messages = check_with_aux(
        &[("app.py", "from facade import _private\n\n_private(1)\n")],
        &[
            (
                "source.py",
                "def public(value: int, /) -> None: ...\n\
                 def _private(value: int) -> None: ...\n",
            ),
            ("facade.py", "from source import *\n"),
        ],
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn reexport_alias_invalidated_by_later_reassignment() {
    let messages = check_with_aux(
        &[("app.py", "from source import alias\n\nalias(1)\n")],
        &[(
            "source.py",
            "def target(value: int) -> None: ...\n\
                 alias = target\n\
                 alias = lambda *args: None\n",
        )],
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

#[test]
fn super_init_into_a_positional_only_base_is_not_flagged() {
    // Issue #1248: ty hovers `super().__init__` in a `list` subclass as
    // `def __init__(iterable: Iterable[int], /) -> None` — the receiver is
    // already bound away, and `iterable` is positional-only. `list.__init__`
    // takes no keyword arguments, so the rewrite the diagnostic asked for
    // raises `TypeError`.
    assert_ok(
        r"
class Payload(list[int]):
    def __init__(self, data: list[int], *, tag: str) -> None:
        super().__init__(data)
        self.tag = tag
",
    );
}

#[test]
fn super_init_diagnostic_does_not_name_tys_binder_display() {
    // ty hovers this as `bound method Self@__init__.__init__(value: int)`.
    // `Self@__init__` is ty's internal binder display, and it is not the
    // callee's class either — `super().__init__` dispatches to a base — so
    // the method alone is reported (issue #1253).
    let messages = check_source(
        r"
class Base:
    def __init__(self, value: int) -> None: ...

class Child(Base):
    def __init__(self, value: int) -> None:
        super().__init__(value)
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].contains(r#"for "__init__""#),
        "got: {messages:?}"
    );
    assert!(!messages[0].contains('@'), "got: {messages:?}");
}

#[test]
fn super_init_into_a_named_base_is_still_flagged() {
    // The counterpart: the base's `__init__` names its parameter, so
    // `super().__init__(value=value)` is a valid rewrite and the call is
    // still reported.
    assert_error(
        r"
class Base:
    def __init__(self, value: int) -> None: ...

class Child(Base):
    def __init__(self, value: int) -> None:
        super().__init__(value)
",
        7,
        "Too many positional",
    );
}

#[test]
fn a_local_rebinding_shadows_an_enclosing_callable() {
    // Python makes a name local to the whole function once it is bound
    // there, whatever an enclosing scope bound it to. The invalidation only
    // consulted the innermost scope, so the enclosing `def` stayed visible
    // and its parameter names reached the call
    // (issues #1121, #1123, #1124).
    for body in [
        "    target, _ = items\n    target(1, 2)\n", // destructuring
        "    if (target := items[0]) is not None:\n        target(1, 2)\n", // walrus
        "    for target in items:\n        target(1, 2)\n", // loop target
    ] {
        let messages = check_source(&format!(
            "def target(alpha, beta): ...\ndef caller(items):\n{body}"
        ));
        assert!(messages.is_empty(), "{body}: got: {messages:?}");
    }
}

#[test]
fn functional_dataclass_omits_init_false_fields() {
    // `field(init=False)` keeps the attribute but drops it from `__init__`,
    // as the class form already models. Putting it on the constructor made
    // the opt-in fix emit `Point(x=1, y=2)`, which raises `TypeError` at
    // runtime (issue #1089).
    let messages = check_source(
        "from dataclasses import field, make_dataclass\nPoint = make_dataclass(\"Point\", [(\"x\", int), (\"y\", int, field(init=False))])\nPoint(1)\n",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("(got 1, maximum 0)")),
        "the sole init field is still a constructor parameter: {messages:?}"
    );
}

#[test]
fn functional_dataclass_keeps_ordinary_fields() {
    // The counterpart: a field with no spec, or one that does not disable
    // `init`, stays on the constructor.
    let messages = check_source(
        "from dataclasses import field, make_dataclass\nPoint = make_dataclass(\"Point\", [(\"x\", int), (\"y\", int, field(default=0))])\nPoint(1, 2)\n",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("(got 2, maximum 0)")),
        "both fields should remain constructor parameters: {messages:?}"
    );
}

#[test]
fn class_body_destructuring_replaces_a_method() {
    // `method, _ = (lambda ...), None` rebinds the method exactly as the
    // plain assignment does, but only the plain form dropped the indexed
    // `def` (issue #1122).
    let messages = check_source(
        "class C:\n    def method(self, alpha, beta): ...\n    method, _ = (lambda only: None), None\nC().method(1, 2)\n",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn class_body_destructuring_keeps_an_unreplaced_method() {
    // The counterpart: a destructuring that does not replace the method
    // leaves it resolvable.
    let messages = check_source(
        "class C:\n    def method(self, alpha, beta): ...\n    other, _ = (lambda only: None), None\nC().method(1, 2)\n",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
}

#[test]
fn an_inner_rebinding_survives_an_enclosing_invalidation() {
    // A `del` at module scope must not suppress a fresh inner binding of the
    // same name; the lambda's own signature applies (issue #1087).
    let messages = check_source(
        "def target(alpha, beta): ...\ndel target\ndef caller():\n    target = lambda only: None\n    target(1, 2)\n",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
}

#[test]
fn a_lambda_replacing_a_def_in_one_scope_stays_blocked() {
    // The counterpart the invalidation exists for: a lambda replacing an
    // earlier `def` in the same scope must not be checked against either
    // signature (issue #412).
    let messages =
        check_source("def target(alpha, beta): ...\ntarget = lambda only: None\ntarget(1, 2)\n");
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn a_loop_iterable_is_checked_before_the_target_binds() {
    // Python evaluates the iterable first, so a call there still refers to
    // the previous callable and must be checked against it (issue #1105).
    let messages = check_source(
        "def target(alpha, beta): ...\ndef caller():\n    for target in [target(1, 2)]:\n        pass\n",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains(r#"for "target""#), "got: {messages:?}");
}

#[test]
fn a_destructured_loop_target_shadows_an_enclosing_callable() {
    // Tuple targets rebind too, so the enclosing `def` must not be used
    // after them (issue #1097).
    let messages = check_source(
        "def target(alpha, beta): ...\ndef caller(items):\n    for target, _ in items:\n        target(1, 2)\n",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn class_body_rebindings_invalidate_an_earlier_callable() {
    // A class body routes its statements separately, and `With`/`For` were
    // not delegated there, so a rebinding kept the earlier callable
    // (issues #1104, #1106).
    let with_body = "import contextlib\ndef target(alpha, beta): ...\n@contextlib.contextmanager\ndef provide():\n    yield lambda only: None\nclass C:\n    with provide() as target:\n        target(1, 2)\n";
    assert!(
        check_source(with_body).is_empty(),
        "with: got: {:?}",
        check_source(with_body)
    );
    let for_body =
        "def target(alpha, beta): ...\nclass C:\n    for target in []:\n        target(1, 2)\n";
    assert!(
        check_source(for_body).is_empty(),
        "for: got: {:?}",
        check_source(for_body)
    );
}

#[test]
fn an_enclosing_callable_still_resolves_without_a_rebinding() {
    // The counterpart: with no local rebinding the enclosing `def` is used.
    let messages = check_source("def target(alpha, beta): ...\ndef caller():\n    target(1, 2)\n");
    assert_eq!(messages.len(), 1, "got: {messages:?}");
}

#[test]
fn module_level_function_named_self_is_not_an_unbound_call() {
    // `pkg.utils.process` is a plain function whose first parameter happens
    // to be named `self`. Treating the dotted path as a class method dropped
    // that argument from the count and left it unnamed by the fixer
    // (issue #1193). All three spellings must agree.
    for call in [
        "import pkg.utils\n\npkg.utils.process(1, 2)\n",
        "from pkg import utils\n\nutils.process(1, 2)\n",
        "from pkg.utils import process\n\nprocess(1, 2)\n",
    ] {
        let messages = check_with_aux(
            &[("app.py", call)],
            &[
                ("pkg/__init__.py", ""),
                ("pkg/utils.py", "def process(self, data): ...\n"),
            ],
        );
        assert_eq!(messages.len(), 1, "got: {messages:?}");
        assert!(
            messages[0].contains("(got 2, maximum 0)"),
            "both arguments are ordinary: {messages:?}"
        );
    }
}

#[test]
fn dotted_module_class_method_is_still_an_unbound_call() {
    // The counterpart: `pkg.utils.Thing.method(obj, 1)` really is unbound, so
    // the explicit receiver still fills `self` and is not counted.
    let messages = check_with_aux(
        &[(
            "app.py",
            "import pkg.utils\n\nobj = object()\npkg.utils.Thing.method(obj, 1)\n",
        )],
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/utils.py",
                "class Thing:\n    def method(self, alpha): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].contains("(got 1, maximum 0)"),
        "the explicit receiver must not be counted: {messages:?}"
    );
}

#[test]
fn reexported_class_is_still_an_unbound_call() {
    // `classes` only records defining names, so a class reached through a
    // package `__init__` re-export is not there under the name the call
    // site uses. Without following the alias the owner check failed and the
    // explicit receiver was counted (Bugbot on #1193).
    let messages = check_with_aux(
        &[(
            "app.py",
            "import lib\n\nobj = object()\nlib.D.method(obj, 1)\n",
        )],
        &[
            (
                "lib/__init__.py",
                "from .impl import D\n\n__all__ = [\"D\"]\n",
            ),
            (
                "lib/impl.py",
                "class D:\n    def method(self, alpha): ...\n",
            ),
        ],
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(
        messages[0].contains("(got 1, maximum 0)"),
        "the explicit receiver must not be counted: {messages:?}"
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
fn explicit_python_env_resolves_module_level_dependency() {
    // A venv outside the project root (not `$VIRTUAL_ENV`, not
    // `<root>/.venv`) is invisible to automatic discovery. Passing the
    // `--python` value makes the built-in resolver and ty use that same
    // third-party environment.
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
        "def configure(host, port):\n    return (host, port)\n\n\
         class Base:\n    def __init__(self, value): ...\n\n\
         class Child(Base):\n    pass\n",
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
        "import extdep\n\n\
         extdep.configure(\"localhost\", 8080)\n\
         extdep.configure(host=\"localhost\", port=8080)\n\
         extdep.Child(1)\n",
    )
    .expect("main");
    let config = Config::load(root).expect("valid config");

    // Unset: `extdep` is unresolvable -> no diagnostics (no regression).
    let none = check_paths(root, std::slice::from_ref(&main), &config, None, None).expect("check");
    assert!(
        none.is_empty(),
        "expected no diagnostics without --python, got: {none:?}"
    );

    // Forwarded: the positional function and inherited constructor calls are
    // both flagged, while the keyword function call is fine. The constructor
    // diagnostic retains the inherited signature owner.
    let got = check_paths(root, &[main], &config, Some(venv.as_path()), None).expect("check");
    let msgs: Vec<String> = got
        .iter()
        .map(|d| format!("{}: {}", d.line, d.message()))
        .collect();
    assert_eq!(got.len(), 2, "got: {msgs:?}");
    assert_eq!(got[0].line, 3, "got: {msgs:?}");
    assert!(msgs[0].contains("\"configure\""), "got: {msgs:?}");
    assert_eq!(got[1].line, 5, "got: {msgs:?}");
    assert!(msgs[1].contains("\"Base\""), "got: {msgs:?}");
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
fn functional_namedtuple_constructor_and_callable_field_are_modeled() {
    let messages = check_source(
        r#"
from collections.abc import Callable
from typing import NamedTuple
Point = NamedTuple("Point", [("call", Callable[[int], None])])
def f(value: int) -> None: ...
Point(f).call(1)
"#,
    );
    // Only the constructor call is reported: `Point(call=f)` is a valid
    // rewrite. The field's type is a bare `Callable[...]`, which names no
    // parameter, so `.call(1)` has no keyword form to move to (issue #1246).
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains(r#"for "Point""#), "got: {messages:?}");
}

#[test]
fn make_dataclass_constructor_and_callable_field_are_modeled() {
    let messages = check_source(
        r#"
from collections.abc import Callable
from dataclasses import make_dataclass
Point = make_dataclass(cls_name="Point", fields=[("call", Callable[[int], None])])
def f(value: int) -> None: ...
Point(f).call(1)
"#,
    );
    // Only the constructor call is reported: `Point(call=f)` is a valid
    // rewrite. The field's type is a bare `Callable[...]`, which names no
    // parameter, so `.call(1)` has no keyword form to move to (issue #1246).
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains(r#"for "Point""#), "got: {messages:?}");
}

#[test]
fn bare_callable_field_call_is_not_reported() {
    // A `Callable[...]` annotation carries no parameter names, so a call
    // through the field has no keyword spelling. Reporting it asked for a
    // rewrite that raises at runtime (issue #1246).
    let messages = check_source(
        r"
from collections.abc import Callable
from dataclasses import dataclass
@dataclass(frozen=True)
class Cfg:
    opener: Callable[[list[int]], str]
def main(cfg: Cfg, items: list[int]) -> None:
    print(cfg.opener(items))
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn bare_callable_field_still_checks_arity() {
    // Dropping the keyword demand keeps the arity the annotation does state:
    // one parameter, so two positional arguments remain wrong.
    let messages = check_source(
        r"
from collections.abc import Callable
from dataclasses import dataclass
@dataclass(frozen=True)
class Cfg:
    opener: Callable[[int], str]
def main(cfg: Cfg) -> None:
    print(cfg.opener(1, 2))
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains("maximum 1"), "got: {messages:?}");
}

#[test]
fn callable_field_traced_to_a_named_function_is_still_reported() {
    // The field's value traces to `f`, which has named parameters, so a
    // keyword form exists and the call is reported against it (issue #373).
    let messages = check_source(
        r"
from collections.abc import Callable
from dataclasses import dataclass
def f(value: int) -> None: ...
@dataclass
class Holder:
    call: Callable[[int], None]
Holder(call=f).call(1)
",
    );
    assert_eq!(messages.len(), 1, "got: {messages:?}");
    assert!(messages[0].contains(r#"for "f""#), "got: {messages:?}");
}

#[test]
fn record_replacements_preserve_callable_field_signatures() {
    let messages = check_source(
        r"
from dataclasses import dataclass, replace
from collections.abc import Callable
from typing import NamedTuple
@dataclass
class D: call: Callable[[int], None]
class N(NamedTuple): call: Callable[[int], None]
def f(value: int) -> None: ...
replace(D(call=f)).call(1)
N(call=f)._replace().call(1)
",
    );
    assert_eq!(messages.len(), 2, "got: {messages:?}");
    assert!(messages
        .iter()
        .any(|message| message.starts_with("main:9:")));
    assert!(messages
        .iter()
        .any(|message| message.starts_with("main:10:")));
}

#[test]
fn collections_namedtuple_keyword_field_preserves_callable_signature() {
    assert_error(
        r#"
from collections import namedtuple
Point = namedtuple("Point", ["call"])
def f(value: int) -> None: ...
Point(call=f).call(1)
"#,
        5,
        r#"for "f" (got 1, maximum 0)"#,
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
fn arbitrary_decorator_definition_signature_is_not_trusted() {
    assert_ok(
        r"
def positional_only(decorated):
    def unrelated():
        return None
    def wrapper(value, /):
        return decorated(value)
    return wrapper

@positional_only
def consume(value):
    return value

consume(1)
",
    );
}

#[test]
fn runtime_decorator_signature_replaces_overloads() {
    assert_ok(
        r"
from typing import overload

def positional_only(decorated):
    def wrapper(value, /):
        return decorated(value)
    return wrapper

@overload
def consume(value: int): ...
@overload
def consume(value: str): ...
@positional_only
def consume(value): ...

consume(1)
",
    );
}

#[test]
fn imported_arbitrary_decorator_definition_signature_is_not_trusted() {
    let project = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file(
            "decorated.py",
            r"
def positional_only(decorated):
    def wrapper(value, /):
        return decorated(value)
    return wrapper

@positional_only
def consume(value):
    return value
",
        )
        .main("from decorated import consume\n\nconsume(1)\n");
    let messages = project.check();
    assert!(messages.is_empty(), "expected no errors, got: {messages:?}");
}

#[test]
fn nested_arbitrary_decorator_definition_signature_is_not_trusted() {
    assert_ok(
        r"
def outer():
    def positional_only(decorated):
        def wrapper(value, /):
            return decorated(value)
        return wrapper

    @positional_only
    def consume(value):
        return value

    consume(1)
",
    );
}

#[test]
fn class_local_decorator_shadows_module_level() {
    assert_ok(
        r"
def positional_only(decorated):
    def wrapper(x, y, z, /):
        return decorated(x, y, z)
    return wrapper

class C:
    @staticmethod
    def positional_only(decorated):
        def wrapper(value, /):
            return decorated(value)
        return wrapper

    @positional_only
    def consume(value):
        return value

C.consume(1)
",
    );
}

#[test]
fn class_local_decorator_shadows_module_level_with_bindings() {
    assert_ok(
        r"
from dataclasses import dataclass

def positional_only(decorated):
    def wrapper(x, y, z, /):
        return decorated(x, y, z)
    return wrapper

@dataclass
class C:
    @staticmethod
    def positional_only(decorated):
        def wrapper(value, /):
            return decorated(value)
        return wrapper

    @positional_only
    def consume(self, value):
        return value

C().consume(1)
",
    );
}

#[test]
fn nested_function_uses_module_decorator_on_fast_path() {
    assert_ok(
        r"
def positional_only(decorated):
    def wrapper(value, /):
        return decorated(value)
    return wrapper

def outer():
    @positional_only
    def consume(value):
        return value

    consume(1)

outer()
",
    );
}

#[test]
fn runtime_decorated_method_body_still_indexes_nested_defs() {
    assert_error(
        r"
from dataclasses import dataclass

def positional_only(decorated):
    def wrapper(value, /):
        return decorated(value)
    return wrapper

@dataclass
class C:
    @positional_only
    def method(self):
        def nested(*, only_kw):
            return only_kw

        nested(1)

C().method()
",
        16,
        "Too many positional",
    );
}

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

/// Moving per-file cache entries directly into output must preserve the same
/// path ordering as the cold whole-project run.
#[test]
fn cache_all_hit_fast_path_preserves_cross_file_order() {
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
    for name in ["z.py", "a.py"] {
        std::fs::write(
            root.join(name),
            "def f(a, b): ...\n\
             f(1, 2)\n",
        )
        .expect("write source");
    }

    let config = Config::load(&root).expect("config");
    let cold = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    let warm = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check");

    assert_eq!(warm, cold);
    assert_eq!(
        warm.iter()
            .map(|diagnostic| diagnostic.path.as_path())
            .collect::<Vec<_>>(),
        [root.join("a.py").as_path(), root.join("z.py").as_path()]
    );
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

#[test]
fn cache_reuses_unaffected_results_after_layout_only_edit() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache_layout_edit")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let unchanged = root.join("a.py");
    let edited = root.join("b.py");
    std::fs::write(&unchanged, "def f(a, b): ...\nf(1, 2)\n").expect("write unchanged");
    std::fs::write(&edited, "def g(a, b): ...\ng(1, 2)\n").expect("write edited");

    let config = Config::load(&root).expect("config");
    let cold = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    assert_eq!(cold.len(), 2);

    std::fs::write(&edited, "\n\ndef g( a,b ) : ...\ng( 1,2 )\n").expect("rewrite edited");
    std::fs::File::options()
        .write(true)
        .open(&edited)
        .expect("open edited")
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1)),
        )
        .expect("advance edited mtime");

    let after = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("check after layout edit");
    assert_eq!(after.len(), 2);
    let unchanged_diagnostic = after
        .iter()
        .find(|diagnostic| diagnostic.path == unchanged)
        .expect("unchanged diagnostic");
    let edited_diagnostic = after
        .iter()
        .find(|diagnostic| diagnostic.path == edited)
        .expect("edited diagnostic");
    assert_eq!(unchanged_diagnostic.line, 2);
    assert_eq!(
        edited_diagnostic.line, 4,
        "the edited file must be rescanned so shifted locations are refreshed"
    );
}

#[test]
fn cache_invalidates_cross_file_results_when_signature_changes() {
    let temp = tempfile::Builder::new()
        .prefix("strictkw_cache_signature_edit")
        .tempdir()
        .expect("tempdir");
    let root = temp.path().to_path_buf();
    let cache_dir = root.join(".cache");
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let definitions = root.join("definitions.py");
    std::fs::write(&definitions, "def f(a, b): ...\n").expect("write definitions");
    std::fs::write(
        root.join("calls.py"),
        "from definitions import f\nf(1, 2)\n",
    )
    .expect("write calls");

    let config = Config::load(&root).expect("config");
    let before = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    assert_eq!(before.len(), 1);

    std::fs::write(&definitions, "def f(a, b, /): ...\n").expect("rewrite definitions");
    std::fs::File::options()
        .write(true)
        .open(&definitions)
        .expect("open definitions")
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1)),
        )
        .expect("advance definitions mtime");

    let after = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("check after signature edit");
    assert!(
        after.is_empty(),
        "signature changes must invalidate dependent cached diagnostics: {after:?}"
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

/// A skipped neighbour must not prevent successfully checked files from using
/// the shared manifest on the next run.
#[test]
fn cache_keeps_successful_entries_when_another_file_is_skipped() {
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
    let valid_file = root.join("valid.py");
    std::fs::write(&valid_file, "def f(a, b): ...\nf(1, 2)\nf(3, 4)\n").expect("write valid");
    std::fs::write(root.join("binary.py"), [0x80u8, 0x90, 0xa0, 0xff]).expect("write binary");

    let config = Config::load(&root).expect("config");
    let cold = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    assert_eq!(cold.len(), 2);

    // A warm run with unchanged sources must reuse the cached diagnostics even
    // when a neighbour was skipped on the cold run.
    let warm = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check");
    assert_eq!(warm, cold);
}

/// A skipped cache miss is re-read on every warm run. If it becomes valid
/// without changing its mtime, the skip-only fast path must fall back to the
/// full pipeline rather than returning only its neighbours' cached results.
#[test]
fn cache_notices_when_skipped_file_becomes_valid_with_same_mtime() {
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
    let cached_file = root.join("cached.py");
    let skipped_file = root.join("binary.py");
    std::fs::write(&cached_file, "def f(a, b): ...\nf(1, 2)\n").expect("write cached");
    std::fs::write(&skipped_file, [0x80u8, 0x90, 0xa0, 0xff]).expect("write binary");

    let config = Config::load(&root).expect("config");
    let cold = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("cold check");
    assert_eq!(cold.len(), 1);

    let modified = std::fs::metadata(&skipped_file)
        .expect("skipped metadata")
        .modified()
        .expect("skipped mtime");
    std::fs::write(&skipped_file, "def g(a, b): ...\ng(1, 2)\n").expect("rewrite skipped");
    std::fs::File::options()
        .write(true)
        .open(&skipped_file)
        .expect("open rewritten file")
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore skipped mtime");

    let warm = check_paths(
        &root,
        std::slice::from_ref(&root),
        &config,
        None,
        Some(&cache_dir),
    )
    .expect("warm check");
    assert_eq!(warm.len(), 2, "rewritten skipped file must be analysed");
    assert!(
        warm.iter()
            .any(|diagnostic| diagnostic.path == skipped_file),
        "expected a diagnostic from the newly valid file: {warm:?}"
    );
}

/// Project-wide collection errors propagate through `check_paths` when a
/// cache directory is configured (covers the `?` on inventory collection).
#[test]
fn check_with_cache_dir_propagates_invalid_extend_exclude() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nextend_exclude = [\"[z-a]\"]\n",
    )
    .expect("write pyproject");
    std::fs::write(root.join("main.py"), "def f(a: int) -> None: ...\n").expect("write main");
    let config = Config::load(root).expect("config");
    let cache_dir = root.join(".cache");
    let result = check_paths(root, &[root.to_path_buf()], &config, None, Some(&cache_dir));
    assert!(
        result.is_err(),
        "invalid extend_exclude must fail before checking when cache is enabled"
    );
}

/// Partial-cache skip preflight propagates I/O errors for explicit unreadable
/// misses (covers the `?` on `skipped_cache_miss_warnings`).
///
/// The preflight only runs when some paths are cache hits and others are
/// misses (`files_to_scan.len() < python_files.len()`). Leave the miss contents
/// unchanged and only deny reads so project validation can keep the warm hit
/// while preflight's `?` fires on the miss.
#[cfg(unix)]
#[test]
fn partial_cache_preflight_propagates_io_error_on_explicit_unreadable_file() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");
    let cached = root.join("cached.py");
    let unreadable = root.join("unreadable.py");
    std::fs::write(&cached, "def f(a: int) -> None: ...\n").expect("write cached");
    std::fs::write(&unreadable, "def g(a: int) -> None: ...\n").expect("write unreadable");

    let config = Config::load(root).expect("config");
    let cache_dir = root.join(".cache");
    check_paths(root, &[root.to_path_buf()], &config, None, Some(&cache_dir))
        .expect("cold check caches both files");

    // Deny reads on the miss only. Do not rewrite contents: a content change
    // plus an unreadable fingerprint used to clear every cache entry and skip
    // the partial-cache preflight path (Bugbot on #707).
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let error = check_paths(root, &[root.to_path_buf()], &config, None, Some(&cache_dir))
        .expect_err("unreadable cache miss must fail during partial-cache preflight");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600)).expect("restore");

    assert!(matches!(error, CheckError::Io(_)));
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

// Unused `# noqa: KW001` reporting (`KW002`), off unless opted into.

const UNUSED_NOQA_PYPROJECT: &str = "[project]\nname = \"t\"\nversion = \"0\"\n\n\
     [tool.strict_kwargs]\nerror_on_unused_noqa = true\n";

/// Diagnostics for `main.py` with `error_on_unused_noqa` enabled, formatted
/// `main:<line>:<column>: <code> <message>`.
fn check_source_with_unused_noqa(source: &str) -> Vec<String> {
    let project = TestProject::new()
        .pyproject(UNUSED_NOQA_PYPROJECT)
        .main(source);
    let main = project.main_path();
    check_paths(
        &project.root,
        std::slice::from_ref(&main),
        &project.config(),
        None,
        None,
    )
    .expect("check")
    .iter()
    .map(|d| format!("main:{}:{}: {} {}", d.line, d.column, d.code(), d.message()))
    .collect()
}

#[test]
fn unused_coded_noqa_is_reported_at_the_directive() {
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(a=1)  # noqa: KW001
",
    );
    assert_eq!(
        messages,
        ["main:3:12: KW002 Unused `noqa` directive (unused: `KW001`)"]
    );
}

#[test]
fn used_coded_noqa_is_not_reported() {
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(1)  # noqa: KW001
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn unused_noqa_is_not_reported_unless_enabled() {
    let messages = check_source(
        r"
def func(a: int) -> None: ...
func(a=1)  # noqa: KW001
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn unused_blanket_or_foreign_noqa_is_left_alone() {
    // A bare `# noqa` or one naming only another tool's codes may well be
    // suppressing a finding this tool cannot see, so neither is reported.
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(a=1)  # noqa
func(a=2)  # noqa: E501
func(a=3)  # noqa:
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn one_used_directive_does_not_excuse_another() {
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(1)  # noqa: KW001
func(a=2)  # noqa: KW001
",
    );
    assert_eq!(
        messages,
        ["main:4:12: KW002 Unused `noqa` directive (unused: `KW001`)"]
    );
}

#[test]
fn a_directive_shared_with_another_tool_still_counts_as_used() {
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(1)  # noqa: E501, KW001
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn multiline_call_directive_on_the_first_line_counts_as_used() {
    let messages = check_source_with_unused_noqa(
        "
def func(a: int, b: int) -> None: ...
func(  # noqa: KW001
    1,
    2,
)
",
    );
    assert!(messages.is_empty(), "got: {messages:?}");
}

#[test]
fn unused_noqa_and_a_real_violation_are_both_reported() {
    let messages = check_source_with_unused_noqa(
        r"
def func(a: int) -> None: ...
func(1)
func(a=2)  # noqa: KW001
",
    );
    assert_eq!(
        messages,
        [
            "main:3:1: KW001 Too many positional arguments for \"func\" (got 1, maximum 0)",
            "main:4:12: KW002 Unused `noqa` directive (unused: `KW001`)",
        ]
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

/// Hyperfine executes its prepare string in a fresh shell, so that string
/// needs its own errexit setting rather than relying on the parent script
/// (issue #1161).
#[test]
fn edit_cache_benchmark_prepare_enables_errexit() {
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/compare-real-project-edit-cache.sh"),
    )
    .expect("read edit-cache benchmark script");
    let prepare = script
        .split_once("printf -v prepare_command")
        .expect("prepare command construction")
        .1;
    let before_hyperfine = prepare
        .split_once("hyperfine")
        .expect("hyperfine invocation")
        .0;
    assert!(
        before_hyperfine.contains("'set -e; "),
        "hyperfine prepare shell must fail fast"
    );
}

#[test]
fn cached_property_is_treated_as_a_property() {
    // `functools.cached_property` is a non-data descriptor, so `self.cached`
    // yields the getter's return value rather than the getter. Checking the
    // call against the zero-parameter getter reported a call the plain
    // `@property` spelling already left alone (issue #1254).
    assert_ok(
        r"
import functools
from collections.abc import Callable

class Spec:
    @property
    def plain(self) -> Callable[[list[int]], str]: ...
    @functools.cached_property
    def cached(self) -> Callable[[list[int]], str]: ...
    def use(self, items: list[int]) -> str:
        return self.plain(items) + self.cached(items)
",
    );
}

#[test]
fn bare_cached_property_import_is_treated_as_a_property() {
    // The `from functools import cached_property` spelling too.
    assert_ok(
        r"
from functools import cached_property
from collections.abc import Callable

class Spec:
    @cached_property
    def cached(self) -> Callable[[int], None]: ...
    def use(self) -> None:
        self.cached(1)
",
    );
}

#[test]
fn narrowed_optional_bare_callable_is_not_reported() {
    // A `Callable[...] | None` narrowed to its callable arm names no
    // parameter, so the call has no keyword spelling (issue #1255).
    assert_ok(
        r"
from collections.abc import Callable
def main(transform: Callable[[str], str] | None, text: str) -> str:
    if transform is not None:
        return transform(text)
    return text
",
    );
}

#[test]
fn bare_callable_annotation_still_checks_arity() {
    // The annotation states an arity even though it names no parameter, so a
    // surplus argument is still reported — this is how the resolver's
    // signature propagation stays observable (issue #1252).
    assert_error(
        r"
from collections.abc import Callable, Iterator
def iterator() -> Iterator[Callable[[int], None]]: ...
next(iterator())(1, 2)
",
        4,
        "maximum 1",
    );
}

#[test]
fn callable_with_a_concrete_target_is_still_reported() {
    // Names erased from a *concrete* definition keep their real kinds, so a
    // callable propagated out of a literal container is still reported and
    // still fixable. Only annotation-derived signatures go quiet.
    assert_error(
        r"
def target(value: int) -> None: ...
next(iter([target]))(1)
",
        3,
        "Too many positional",
    );
}
