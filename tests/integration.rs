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

#[test]
fn ignore_name_ini() {
    let project = TestProject::new()
        .file(
            "mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
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
fn debug_ini() {
    let project = TestProject::new()
        .file(
            "mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
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

#[test]
fn ignore_name_ini_no_spaces() {
    let project = TestProject::new()
        .file(
            "mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func,builtins.str
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
fn no_plugin_section_ini() {
    let project = TestProject::new()
        .file(
            "mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs
"#,
        )
        .main(
            r#"
def func(a: int) -> None: ...
func(1)
"#,
        );
    assert_error_at(&project, 3, "Too many positional");
}

#[test]
fn empty_ignore_names_ini() {
    let project = TestProject::new()
        .file(
            "mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names =
"#,
        )
        .main(
            r#"
def func(a: int) -> None: ...
func(1)
"#,
        );
    assert_error_at(&project, 3, "Too many positional");
}

#[test]
fn ignore_name_dot_mypy_ini() {
    let project = TestProject::new()
        .file(
            ".mypy.ini",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
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
fn ignore_name_setup_cfg() {
    let project = TestProject::new()
        .file(
            "setup.cfg",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
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
fn debug_setup_cfg() {
    let project = TestProject::new()
        .file(
            "setup.cfg",
            r#"
[mypy]
plugins = mypy_strict_kwargs

[mypy_strict_kwargs]
ignore_names = main.func, builtins.str
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
