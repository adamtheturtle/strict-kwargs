//! Import- and re-export-resolution behaviour of the definition index.
//!
//! Each test feeds a multi-file Python project through the public
//! `check_paths` API and asserts that an import / re-export construct
//! (relative imports, `import *`, dotted imports, package re-exports,
//! control-flow-nested defs, assignment aliases, …) was resolved — or
//! deliberately *not* resolved — to the right call target.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort
// with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use strict_kwargs::{check_paths, Config};

struct TestProject {
    _temp: tempfile::TempDir,
    root: PathBuf,
    paths: Vec<PathBuf>,
}

impl TestProject {
    fn new() -> Self {
        // Use a non-dotted prefix: `tempfile`'s default `.tmpXXXX` dirs have
        // a leading-dot component that the directory walker would ignore.
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
        Self {
            _temp: temp,
            root,
            paths: Vec::new(),
        }
    }

    /// Write a project file. Files named here are also the explicit set of
    /// paths handed to `check_paths` (so directory-ignore rules never
    /// interfere with multi-file package fixtures).
    fn file(mut self, path: &str, content: &str) -> Self {
        let file_path = self.root.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file_path, content).expect("write file");
        self.paths.push(file_path);
        self
    }

    /// Write a project file that is *not* passed to `check_paths` directly:
    /// it is only discovered transitively via imports (exercising the
    /// import-resolution queue in the index builder).
    fn dep(self, path: &str, content: &str) -> Self {
        let file_path = self.root.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file_path, content).expect("write file");
        self
    }

    /// Run `check_paths` over the explicitly-added files and return
    /// diagnostics formatted as ``<filename>:<line>: <message>``.
    fn check(&self) -> Vec<String> {
        let config = Config::load(&self.root).expect("valid config");
        let diagnostics = check_paths(&self.root, &self.paths, &config, None, None).expect("check");
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

fn has(messages: &[String], prefix: &str, contains: &str) -> bool {
    messages
        .iter()
        .any(|m| m.starts_with(prefix) && m.contains(contains))
}

/// An imported first-party module that exists but fails to parse is skipped
/// without aborting the run; sibling modules are still indexed and checked.
#[test]
fn imported_module_with_syntax_error_is_skipped() {
    let project = TestProject::new()
        .dep("broken.py", "def (((( this is not valid python\n")
        .file("app.py", "import broken\nimport good\n\ngood.helper(1)\n")
        .dep("good.py", "def helper(a: int) -> None: ...\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:4:", "Too many positional"),
        "good module should still be indexed; got: {messages:?}"
    );
}

/// `import x` nested inside a function body binds in the *function*
/// namespace, not the module's. The built-in resolver only tracks
/// module-level import bindings as call targets, so a function-local call
/// through such an import is conservatively left unresolved — no false
/// positive, no panic — while the submodule is still queued during indexing.
#[test]
fn import_inside_function_is_non_module_scope() {
    let project = TestProject::new()
        .dep("svc.py", "def run(a: int) -> None: ...\n")
        .file(
            "app.py",
            "def driver() -> None:\n    import svc\n    svc.run(1)\n",
        );
    let messages = project.check();
    assert!(
        messages.is_empty(),
        "function-local import must not resolve to a module-level target; got: {messages:?}"
    );
}

/// A module-level `for` / `while` / `with` does not open a new scope, so
/// imports inside them still produce module-level re-export aliases.
#[test]
fn control_flow_for_while_with_preserve_module_scope() {
    let project = TestProject::new()
        .dep("impl.py", "def alpha(a: int) -> None: ...\n")
        .dep("impl2.py", "def beta(a: int) -> None: ...\n")
        .dep("impl3.py", "def gamma(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
for _ in range(1):
    from impl import alpha

while False:
    from impl2 import beta

with open(__file__):
    from impl3 import gamma

alpha(1)
beta(1)
gamma(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "for-nested import alias; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:12:", "Too many positional"),
        "while-nested import alias; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:13:", "Too many positional"),
        "with-nested import alias; got: {messages:?}"
    );
}

/// A module-level `if` / `elif` / `else` also preserves module scope. The
/// custom `Stmt::If` visitor must still route body statements through
/// `visit_stmt` so imports in every branch are recorded.
#[test]
fn control_flow_if_elif_else_preserve_module_scope() {
    let project = TestProject::new()
        .dep("impl.py", "def alpha(a: int) -> None: ...\n")
        .dep("impl2.py", "def beta(a: int) -> None: ...\n")
        .dep("impl3.py", "def gamma(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
if True:
    from impl import alpha
elif False:
    from impl2 import beta
else:
    from impl3 import gamma

alpha(1)
beta(1)
gamma(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:9:", "Too many positional"),
        "if-nested import alias; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:10:", "Too many positional"),
        "elif-nested import alias; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "else-nested import alias; got: {messages:?}"
    );
}

/// A module-level `try` / `except` / `else` / `finally` likewise preserves
/// module scope (typeshed gates re-exports on `sys.version_info` this way),
/// so imports in every clause produce module-level re-export aliases.
#[test]
fn try_except_else_finally_preserve_module_scope() {
    let project = TestProject::new()
        .dep("ta.py", "def fa(a: int) -> None: ...\n")
        .dep("tb.py", "def fb(a: int) -> None: ...\n")
        .dep("tc.py", "def fc(a: int) -> None: ...\n")
        .dep("td.py", "def fd(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
try:
    from ta import fa
except ImportError:
    from tb import fb
else:
    from tc import fc
finally:
    from td import fd

fa(1)
fb(1)
fc(1)
fd(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "try-body import; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:12:", "Too many positional"),
        "except-handler import; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:13:", "Too many positional"),
        "else-clause import; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:14:", "Too many positional"),
        "finally-clause import; got: {messages:?}"
    );
}

/// Module-level function/class definitions nested inside `for` / `while` /
/// `with` are still indexed.
#[test]
fn defs_inside_for_while_with_are_indexed() {
    let project = TestProject::new().file(
        "app.py",
        r"
for _ in range(1):
    def made_in_for(a: int) -> None: ...

while False:
    def made_in_while(a: int) -> None: ...

with open(__file__):
    def made_in_with(a: int) -> None: ...

made_in_for(1)
made_in_while(1)
made_in_with(1)
",
    );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "for-nested def indexed; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:12:", "Too many positional"),
        "while-nested def indexed; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:13:", "Too many positional"),
        "with-nested def indexed; got: {messages:?}"
    );
}

/// Module-level definitions nested inside every clause of a `try` statement
/// are indexed.
#[test]
fn defs_inside_try_clauses_are_indexed() {
    let project = TestProject::new().file(
        "app.py",
        r"
try:
    def in_try(a: int) -> None: ...
except ImportError:
    def in_except(a: int) -> None: ...
else:
    def in_else(a: int) -> None: ...
finally:
    def in_finally(a: int) -> None: ...

in_try(1)
in_except(1)
in_else(1)
in_finally(1)
",
    );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "try-body def; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:12:", "Too many positional"),
        "except def; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:13:", "Too many positional"),
        "else def; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:14:", "Too many positional"),
        "finally def; got: {messages:?}"
    );
}

/// Module-level definitions inside `match` case bodies are indexed.
#[test]
fn defs_inside_match_cases_are_indexed() {
    let project = TestProject::new().file(
        "app.py",
        r"
import sys

match sys.argv:
    case []:
        def matched_empty(a: int) -> None: ...
    case _:
        def matched_other(a: int) -> None: ...

matched_empty(1)
matched_other(1)
",
    );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:10:", "Too many positional"),
        "first case-body def indexed; got: {messages:?}"
    );
    assert!(
        has(&messages, "app.py:11:", "Too many positional"),
        "second case-body def indexed; got: {messages:?}"
    );
}

/// `from ..helper import helper` (level 2) resolves by popping one package
/// level from the importing module's package.
#[test]
fn double_dot_relative_import_pops_a_package_level() {
    let project = TestProject::new()
        .dep("pkg/__init__.py", "")
        .dep("pkg/helper.py", "def helper(a: int) -> None: ...\n")
        .dep("pkg/sub/__init__.py", "")
        .file(
            "pkg/sub/app.py",
            "from ..helper import helper\n\nhelper(1)\n",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "`from ..helper import helper` should resolve; got: {messages:?}"
    );
}

/// A relative import that walks above the top-level package resolves to
/// nothing (no panic, no binding, no diagnostic).
#[test]
fn over_deep_relative_import_returns_none_and_is_skipped() {
    let project = TestProject::new()
        .dep("pkg/__init__.py", "")
        .file("pkg/app.py", "from ... import helper\n\nhelper(1)\n");
    let messages = project.check();
    assert!(
        !has(&messages, "app.py:3:", "Too many positional"),
        "over-deep relative import must resolve to nothing; got: {messages:?}"
    );
}

/// A top-level module (not a package) doing `from . import helper` resolves
/// to the sibling top-level module.
#[test]
fn dot_import_from_top_level_module_empty_base() {
    let project = TestProject::new()
        .dep("helper.py", "def helper(a: int) -> None: ...\n")
        .file("app.py", "from . import helper\n\nhelper.helper(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "`from . import helper` at top level should resolve helper module; got: {messages:?}"
    );
}

/// A top-level module doing `from .helper import fn` resolves to the sibling
/// top-level module `helper` (empty package, no dot separator inserted).
#[test]
fn dotted_relative_import_from_top_level_module_empty_package() {
    let project = TestProject::new()
        .dep("helper.py", "def fn(a: int) -> None: ...\n")
        .file("app.py", "from .helper import fn\n\nfn(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "`from .helper import fn` at top level should resolve; got: {messages:?}"
    );
}

/// A single statement with both an attribute target and a bare-`Name` target
/// (`obj.attr = aliased = real`): only the bare name becomes a re-export
/// alias; the attribute target is ignored. Resolved purely by the built-in
/// re-export index (ty-independent).
#[test]
fn assignment_with_mixed_name_and_attribute_targets() {
    let project = TestProject::new()
        .dep("impl.py", "def real(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
from impl import real

class Holder:
    attr = None

obj = Holder()
obj.attr = aliased = real

aliased(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:10:", "Too many positional"),
        "Name target should alias even alongside an attribute target; got: {messages:?}"
    );
}

/// An assignment whose RHS is an attribute access on a non-reference
/// (`factory().real`) creates no alias; a sibling pure-reference assignment
/// (`good = real`) still aliases (ty-independent).
#[test]
fn attribute_on_call_result_is_not_a_reference() {
    let project = TestProject::new()
        .dep("impl.py", "def real(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
from impl import real

def factory():
    return real

bad = factory().real
good = real

good(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:10:", "Too many positional"),
        "pure-reference alias `good = real` must still resolve; got: {messages:?}"
    );
}

/// `from base import *` at module level re-exports every name of `base`
/// under the importing module.
#[test]
fn star_import_reexports_all_names() {
    let project = TestProject::new()
        .dep("kit.py", "def widget(a: int) -> None: ...\n")
        .file("app.py", "from kit import *\n\nwidget(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "star import should re-export `widget`; got: {messages:?}"
    );
}

/// `from x import *` nested in a function body creates no module-level
/// re-export, so the name is not resolvable at module scope.
#[test]
fn star_import_inside_function_is_not_module_reexport() {
    let project = TestProject::new()
        .dep("kit2.py", "def widget(a: int) -> None: ...\n")
        .file(
            "app.py",
            "def loader() -> None:\n    from kit2 import *\n\nwidget(1)\n",
        );
    let messages = project.check();
    assert!(
        !has(&messages, "app.py:4:", "Too many positional"),
        "function-local star import must not create a module re-export; got: {messages:?}"
    );
}

/// A top-level module doing `from . import *` (empty base) creates no star
/// re-export and does not panic; ordinary first-party defs still resolve.
#[test]
fn star_import_with_empty_base_creates_no_reexport() {
    let project = TestProject::new().file(
        "app.py",
        r"
from . import *

def local(a: int) -> None: ...

local(1)
",
    );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:6:", "Too many positional"),
        "empty-base star import must be a harmless no-op; got: {messages:?}"
    );
}

/// `import a.b.c` queues every dotted prefix so the deep submodule is
/// resolved and `a.b.c.fn` is indexed.
#[test]
fn dotted_import_queues_every_prefix() {
    let project = TestProject::new()
        .dep("a/__init__.py", "")
        .dep("a/b/__init__.py", "")
        .dep("a/b/c.py", "def fn(x: int) -> None: ...\n")
        .file("app.py", "import a.b.c\n\na.b.c.fn(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "deep dotted import should index `a.b.c.fn`; got: {messages:?}"
    );
}

/// `import p.q as pq` binds the alias `pq -> p.q`, so `pq.fn(...)` resolves
/// to `p.q.fn`.
#[test]
fn dotted_import_with_asname_binds_alias() {
    let project = TestProject::new()
        .dep("p/__init__.py", "")
        .dep("p/q.py", "def fn(x: int) -> None: ...\n")
        .file("app.py", "import p.q as pq\n\npq.fn(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "`import p.q as pq` alias should resolve; got: {messages:?}"
    );
}

/// Chained re-exports through a package `__init__` (`from .impl import name`)
/// resolve through the re-export fixpoint.
#[test]
fn package_init_reexports_resolve_via_fixpoint() {
    let project = TestProject::new()
        .dep("pkg/__init__.py", "from .impl import thing\n")
        .dep("pkg/impl.py", "def thing(a: int) -> None: ...\n")
        .file("app.py", "from pkg import thing\n\nthing(1)\n");
    let messages = project.check();
    assert!(
        has(&messages, "app.py:3:", "Too many positional"),
        "re-exported `pkg.thing` should resolve via fixpoint; got: {messages:?}"
    );
}

/// A module-level annotated-assignment alias (`helper: Callable = impl.real`)
/// re-exports `impl.real` under the importing module.
#[test]
fn annotated_assignment_alias_is_reexported() {
    let project = TestProject::new()
        .dep("impl.py", "def real(a: int) -> None: ...\n")
        .file(
            "app.py",
            r"
import impl
from typing import Callable

helper: Callable = impl.real

helper(1)
",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:7:", "Too many positional"),
        "annotated-assignment alias should resolve to impl.real; got: {messages:?}"
    );
}

/// A plain module-level assignment alias to an imported name
/// (`helper = real`) resolves through the import binding.
#[test]
fn assignment_alias_via_import_binding() {
    let project = TestProject::new()
        .dep("impl.py", "def real(a: int) -> None: ...\n")
        .file(
            "app.py",
            "from impl import real\n\nhelper = real\n\nhelper(1)\n",
        );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:5:", "Too many positional"),
        "`helper = real` should alias the imported `real`; got: {messages:?}"
    );
}

/// More queued modules than `MODULE_BUDGET` (4000): the import-closure
/// loop exhausts its budget and stops resolving further modules without
/// panicking. The (unresolvable) names need no files — they are still
/// dequeued and counted, driving the budget to zero.
#[test]
fn import_closure_module_budget_is_enforced() {
    use std::fmt::Write as _;
    let mut app = String::new();
    for i in 0..4100 {
        writeln!(app, "import m{i}").expect("write");
    }
    app.push_str("\n\ndef local(a: int) -> None: ...\n\nlocal(1)\n");
    let project = TestProject::new().file("app.py", &app);
    let messages = project.check();
    // The run completes; the genuine first-party violation is still found.
    assert!(
        has(&messages, "app.py:", "Too many positional"),
        "budget-capped run must still flag the local call; got count: {}",
        messages.len()
    );
}

/// A `@dataclass` whose body has a `ClassVar` field and an
/// attribute-target annotation (`_sentinel.x: int`): the synthesized
/// `__init__` field collector skips the `ClassVar` (not a constructor
/// parameter) and the non-`Name` target, taking only `x`. `D(1, 2)`
/// therefore exceeds the synthesized signature and is flagged.
#[test]
fn dataclass_skips_classvar_and_attribute_target_fields() {
    let project = TestProject::new().file(
        "app.py",
        r"
from dataclasses import dataclass
from typing import ClassVar


class _Sentinel:
    x = 0


@dataclass
class D:
    x: int
    y: ClassVar[int] = 0
    _Sentinel.x: int = 1


D(1, 2)
",
    );
    let messages = project.check();
    assert!(
        has(&messages, "app.py:", "Too many positional") || has(&messages, "app.py:", "\"D\""),
        "dataclass synth must skip ClassVar/attribute fields; got: {messages:?}"
    );
}
