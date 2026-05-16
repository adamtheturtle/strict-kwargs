//! Integration tests ported from ``mypy-strict-kwargs``'s ``test_plugin.yaml``.

use strict_kwargs::{check_paths, Config};

fn check_source(source: &str) -> Vec<String> {
  let temp = tempfile::tempdir().expect("tempdir");
  let root = temp.path();
  std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"t\"\nversion = \"0\"\n").unwrap();
  let main = root.join("main.py");
  std::fs::write(&main, source).expect("write main.py");
  let config = Config::default();
  let diagnostics = check_paths(root, &[main], &config).expect("check");
  diagnostics
    .iter()
    .map(|d| format!("main:{}: {}", d.line, d.message()))
    .collect()
}

fn assert_error(source: &str, line: usize, contains: &str) {
  let messages = check_source(source);
  assert!(
    messages.iter().any(|m| m.starts_with(&format!("main:{line}:")) && m.contains(contains)),
    "expected error on line {line} containing {contains:?}, got: {messages:?}"
  );
}

fn assert_ok(source: &str) {
  let messages = check_source(source);
  assert!(messages.is_empty(), "expected no errors, got: {messages:?}");
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
fn var_positional() {
  assert_ok(
    r#"
def func(*args: str) -> None: ...
func("extra")
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
  assert!(messages[0].contains("__call__"));
}

#[test]
fn ignore_names() {
  let temp = tempfile::tempdir().expect("tempdir");
  let root = temp.path();
  std::fs::write(
    root.join("pyproject.toml"),
    r#"
[project]
name = "t"
version = "0"

[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
"#,
  )
  .unwrap();
  let main = root.join("main.py");
  std::fs::write(
    &main,
    r#"
def func(a: int) -> None: ...
func(1)

def not_ignored(a: int) -> None: ...
not_ignored(1)
"#,
  )
  .unwrap();
  let config = Config::load(root);
  let diagnostics = check_paths(root, &[main], &config).expect("check");
  assert_eq!(diagnostics.len(), 1);
  assert!(diagnostics[0].message().contains("not_ignored"));
}
