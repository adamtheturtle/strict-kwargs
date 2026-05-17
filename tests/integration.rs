//! Integration tests ported from ``mypy-strict-kwargs``'s ``test_plugin.yaml``.

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
        let diagnostics = check_paths(&self.root, &[main], &config).expect("check");
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
        r#"
def func(a: int) -> None: ...
func(1)
"#,
        3,
        "Too many positional",
    );
}

#[test]
fn positional_optional() {
    assert_error(
        r#"
def func(a: int = 1) -> None: ...
func(1)
func()
"#,
        3,
        "Too many positional",
    );
}

#[test]
fn keyword_only() {
    assert_ok(
        r#"
def func(*, a: int) -> None: ...
func(a=1)
"#,
    );
}

#[test]
fn keyword_only_optional() {
    assert_ok(
        r#"
def func(*, a: int = 1) -> None: ...
func(a=1)
func()
"#,
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
        r#"
def func(a: int, *args: str) -> None: ...
func(1)
"#,
    );
}

#[test]
fn positional_optional_followed_by_var_positional() {
    assert_ok(
        r#"
def func(a: int = 1, *args: str) -> None: ...
func(1)
func()
"#,
    );
}

#[test]
fn positional_followed_by_var_keyword() {
    assert_error(
        r#"
def func(a: int, **kwargs: str) -> None: ...
func(1)
"#,
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
        r#"
class C:
    def __init__(self) -> None: ...
    def method(self, a: int) -> None: ...
c = C()
c.method(1)
"#,
        6,
        "Too many positional",
    );
}

#[test]
fn callable_class_as_decorator() {
    assert_ok(
        r#"
from typing import Any

class C:
    def __call__(self, func: Any) -> None: ...

@C()
def func() -> None: ...
"#,
    );
}

#[test]
fn callable_class_extra_params() {
    let messages = check_source(
        r#"
from typing import Any

class C:
    def __call__(self, func: Any, a: int) -> None: ...

c = C()
c(lambda: None, 1)
c(func=lambda: None, a=1)
c(lambda: None, a=1)
"#,
    );
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("Too many positional"));
}

#[test]
fn descriptor() {
    assert_ok(
        r#"
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
"#,
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
            r#"
def func(a: int) -> None: ...
func(1)

def not_ignored(a: int) -> None: ...
not_ignored(1)

str(1)
"#,
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
            r#"
def func(a: int) -> None: ...
func(1)

def not_ignored(a: int) -> None: ...
not_ignored(1)

str(1)
"#,
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
    let diagnostics = check_paths(&root, &[dir], &config).expect("check");
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
    let diagnostics = check_paths(&root, &paths, &config).expect("check");
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
        r#"
class C:
    def __init__(self, a: int) -> None: ...
C(1)
"#,
        4,
        "Too many positional",
    );
}

#[test]
fn project_constructor_keyword_ok() {
    assert_ok(
        r#"
class C:
    def __init__(self, a: int) -> None: ...
C(a=1)
"#,
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
    check_paths(&root, &paths, &config)
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

/// ty-backed tests need the `ty` binary; skip cleanly when it is absent so
/// the suite is not environment-dependent.
fn ty_available() -> bool {
    std::process::Command::new("ty")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn ty_resolves_inherited_method() {
    if !ty_available() {
        eprintln!("skipping: `ty` not installed");
        return;
    }
    assert_error(
        r#"
class A:
    def method(self, a: int) -> None: ...

class B(A):
    pass

B().method(1)
"#,
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
        r#"
class A:
    def method(self, a: int) -> None: ...

def make() -> A:
    return A()

def takes(x: A) -> None:
    x.method(1)

make().method(1)
A().method(a=1)
"#,
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
        r#"
class A:
    def method(self, a: int) -> None: ...

class B(A):
    pass

B().method(a=1)
"#,
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
        r#"
from typing import overload

@overload
def f(a: int) -> int: ...
@overload
def f(a: int, b: int) -> str: ...
def f(a, b=0): return a

f(1, 2)
"#,
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
        r#"
xs: list[int] = []
xs.append(1, 2)
xs.append(1)
"#,
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
