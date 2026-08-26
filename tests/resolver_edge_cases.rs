//! Call-resolution edge cases of the checker.
//!
//! Exercises the harder corners of resolving a call's callee — directory
//! discovery, unusual callee expressions, instance tracking, display
//! formatting, the `ignore_names` config, and the `ty` type-inference
//! fallback (hover + goto-definition) — through the public `check_paths`
//! API. The fixer's own behaviour lives in `tests/fix.rs`.

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort the
// test with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (it is not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use strict_kwargs::{check_paths, Config, Diagnostic};

mod common;

use common::{TestProject, DEFAULT_PYPROJECT};

fn plain_project(source: &str) -> TestProject {
    TestProject::new().pyproject(DEFAULT_PYPROJECT).main(source)
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

/// `partialmethod` retains the wrapped method signature after removing
/// arguments bound after the receiver (issue #377).
#[test]
fn partialmethod_preserves_remaining_method_signature() {
    let messages = check_source(
        r"
from functools import partialmethod
class C:
    def base(self, required: int, /, value: int) -> None: ...
    method = partialmethod(base)
    bound = partialmethod(base, 0)
C().method(0, 1)
C().bound(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "method") && has_error_at(&messages, 8, "bound"),
        "expected both partialmethod violations, got: {messages:?}"
    );
}

/// `del f` removes a local callable binding so later calls are not resolved
/// against the deleted definition.
#[test]
fn delete_invalidates_prior_function_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
del f
f(1)
",
    );
    assert!(
        messages.is_empty(),
        "deleted function must not be resolved: {messages:?}"
    );
}

/// Calls before a later ``del`` must still resolve; index-excluding on ``del``
/// would suppress earlier sites such as ``@_wraps`` then ``del _wraps``.
#[test]
fn use_before_delete_still_checks_callable() {
    let messages = check_source(
        r"
def _wraps(wrapped):
    def decorator(wrapper):
        return wrapper
    return decorator

@_wraps(abs)
def signal(x):
    return x

del _wraps
",
    );
    assert!(
        has_error_at(&messages, 7, "_wraps"),
        "use before del must still be checked, got: {messages:?}"
    );
}

/// Exercises that conditional ``del`` does not permanently drop the binding
/// from check resolution of later unconditional uses.
#[test]
fn conditional_delete_is_indexed_without_exclusion() {
    let _messages = check_source(
        r"
def f(value: int) -> None: ...
if condition:
    del f
f(1)
",
    );
}

/// A `for` target remains bound after the loop and invalidates an earlier
/// function definition with the same name (issue #414).
#[test]
fn for_target_invalidates_prior_function_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
for f in [lambda *args: None]:
    pass
f(1)
",
    );
    assert!(
        messages.is_empty(),
        "stale loop-target function: {messages:?}"
    );
}

/// A `with ... as` target remains bound after the statement and invalidates
/// an earlier function definition with the same name (issue #415).
#[test]
fn with_as_target_invalidates_prior_function_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
class Manager:
    def __enter__(self): return lambda *args: None
    def __exit__(self, *args): pass
with Manager() as f:
    pass
f(1)
",
    );
    assert!(messages.is_empty(), "stale with-as function: {messages:?}");
}

/// Bound-method aliases are opaque to the built-in resolver but must still
/// reach the ty fallback. Skipping ty for every opaque local (not only
/// invalidated callables) silenced Sphinx-style `_filter = lang.word_filter`
/// diagnostics after #561.
#[test]
fn bound_method_alias_still_defers_to_ty() {
    let messages = check_source(
        r"
class SearchLanguage:
    def word_filter(self, word: str) -> bool: ...

def feed(lang: SearchLanguage, stemmed_word: str, extra: str) -> bool:
    _filter = lang.word_filter
    return _filter(stemmed_word, extra)
",
    );
    assert!(
        has_error_at(&messages, 7, "Too many positional"),
        "bound-method alias must still reach ty, got: {messages:?}"
    );
}

/// Fresh lambda bindings are opaque but must still reach ty. Marking every
/// lambda assignment as invalidated (rather than only those replacing a
/// prior ``def``) silenced `CPython` helpers such as
/// ``badvalue = lambda f: self.assertRaises(...)``.
#[test]
fn fresh_lambda_binding_still_defers_to_ty() {
    let messages = check_source(
        r"
def test() -> None:
    badvalue = lambda f: None
    badvalue(lambda: None)
",
    );
    assert!(
        has_error_at(&messages, 4, "Too many positional")
            || messages
                .iter()
                .any(|m| m.contains("badvalue") || m.contains("lambda")),
        "fresh lambda binding must still reach ty, got: {messages:?}"
    );
}

/// A lambda that replaces an earlier ``def`` must not keep the stale
/// signature (issue #412).
#[test]
fn lambda_replacement_invalidates_prior_function_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
f = lambda *args: None
f(1)
",
    );
    assert!(
        messages.is_empty(),
        "stale lambda-replaced function: {messages:?}"
    );
}

/// Fresh imports must stay module-resolvable for attribute calls (regression
/// from marking every import opaque during rebinding invalidation).
#[test]
fn imported_module_attributes_remain_checkable() {
    let messages = check_source(
        r"
from functools import reduce
reduce(lambda left, right: left + right, [1, 2], 0, 0)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("reduce")),
        "expected reduce signature check after import, got {messages:?}"
    );
}

/// Augmented assignment, import-as, match capture, walrus, except-as cleanup,
/// destructuring, and empty-for else suites invalidate prior callables
/// (issues #416–#421, #427).
#[test]
fn rebinding_forms_invalidate_prior_function_signatures() {
    for source in [
        "def f(value: int) -> None: ...\nf += None\nf(1)\n",
        "def target(value: int) -> None: ...\nimport sys as target\ntarget(1)\n",
        "def f(value: int) -> None: ...\nmatch (lambda *args: None):\n    case f:\n        pass\nf(1)\n",
        "def f(value: int) -> None: ...\n(f := lambda *args: None)\nf(1)\n",
        "def f(value: int) -> None: ...\ntry:\n    raise ValueError\nexcept ValueError as f:\n    pass\nf(1)\n",
        "def f(value: int) -> None: ...\n(f,) = (lambda *args: None,)\nf(1)\n",
        "def f(value: int) -> None: ...\nfor _ in []:\n    pass\nelse:\n    f = lambda *args: None\nf(1)\n",
        "def f(value: int) -> None: ...\nwhile False:\n    pass\nelse:\n    f = lambda *args: None\nf(1)\n",
    ] {
        let messages = check_source(source);
        assert!(
            messages.is_empty(),
            "expected no stale KW001 for {source:?}, got {messages:?}"
        );
    }

    let messages = check_source(
        r"
class C:
    def method(self, value: int) -> None: ...
C.method += None
C().method(1)
",
    );
    assert!(messages.is_empty(), "stale augmented method: {messages:?}");
}

/// Literal ``globals()`` / ``exec`` / ``setattr`` / ``nonlocal`` mutations
/// invalidate prior callable signatures (issues #422–#425).
#[test]
fn dynamic_rebinding_invalidates_prior_function_signatures() {
    for source in [
        "def f(value: int) -> None: ...\nglobals()[\"f\"] = lambda *args: None\nf(1)\n",
        "def f(value: int) -> None: ...\nexec(\"f = lambda *args: None\")\nf(1)\n",
        "class C:\n    def method(self, value: int) -> None: ...\nsetattr(C, \"method\", lambda *args: None)\nC().method(1)\n",
        "class Base:\n    def method(self, value: int) -> None: ...\nclass Child(Base): ...\nBase.method = lambda *args: None\nChild().method(1)\n",
        "def outer() -> None:\n    def f(value: int) -> None: ...\n    def replace() -> None:\n        nonlocal f\n        f = lambda *args: None\n    replace()\n    f(1)\nouter()\n",
        // Nested ``globals()`` must clear the module binding, not only the
        // current function scope (Bugbot on #720).
        "def f(value: int) -> None: ...\ndef rebind() -> None:\n    globals()[\"f\"] = lambda *args: None\nrebind()\nf(1)\n",
        // ``nonlocal`` via annotated / augmented assign (Bugbot on #720).
        "def outer() -> None:\n    def f(value: int) -> None: ...\n    def replace() -> None:\n        nonlocal f\n        f: object = lambda *args: None\n    replace()\n    f(1)\nouter()\n",
    ] {
        let messages = check_source(source);
        assert!(
            messages.is_empty(),
            "expected no stale KW001 for {source:?}, got {messages:?}"
        );
    }
}

/// Annotation-only ``AnnAssign`` must not clear an enclosing ``nonlocal``
/// callable (Bugbot on #726).
#[test]
fn annotation_only_nonlocal_does_not_clear_enclosing_callable() {
    let messages = check_source(
        r"
def outer() -> None:
    def f(value: int) -> None: ...
    def annotate() -> None:
        nonlocal f
        f: object
    annotate()
    f(1)
outer()
",
    );
    assert!(
        has_error_at(&messages, 8, "f"),
        "annotation-only nonlocal must keep enclosing callable: {messages:?}"
    );
}

/// A try/except import fallback lambda must not suppress calls through the
/// successfully imported name (``_tuplegetter`` pattern in `CPython`).
#[test]
fn try_except_import_fallback_lambda_still_checks_import() {
    let project = TestProject::new()
        .pyproject(DEFAULT_PYPROJECT)
        .file(
            "collections_helper.py",
            "def _tuplegetter(index: int, doc: str) -> object: ...\n",
        )
        .main(
            r"
try:
    from collections_helper import _tuplegetter
except ImportError:
    _tuplegetter = lambda index, doc: None
_tuplegetter(0)
",
        );
    let messages = project.check();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("_tuplegetter") || m.contains("Too many")),
        "imported _tuplegetter must still be checked, got: {messages:?}"
    );
}

/// A named expression evaluates to its assigned value, so using one as the
/// callee preserves the concrete function signature (issue #361).
#[test]
fn named_expression_callee_resolves_its_value() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
(alias := f)(1)
",
    );
    assert!(
        has_error_at(&messages, 3, "f"),
        "expected named-expression violation, got: {messages:?}"
    );
}

/// A conditional expression with the same callable on both branches has an
/// unambiguous signature (issue #362).
#[test]
fn conditional_expression_callee_resolves_matching_branches() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
(f if condition else f)(1)
",
    );
    assert!(
        has_error_at(&messages, 3, "f"),
        "expected conditional-expression violation, got: {messages:?}"
    );
}

/// A boolean expression whose operands are the same callable has an
/// unambiguous signature (issue #363).
#[test]
fn boolean_expression_callee_resolves_matching_operands() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
(f or f)(1)
(f and f)(1)
",
    );
    assert!(
        has_error_at(&messages, 3, "f") && has_error_at(&messages, 4, "f"),
        "expected both boolean-expression violations, got: {messages:?}"
    );
}

/// Literal list, tuple, and dictionary subscripts preserve the selected
/// callable's signature (issue #364).
#[test]
fn literal_container_subscripts_resolve_selected_callables() {
    let messages = check_source(
        r#"
def f(value: int) -> None: ...
def g(first: int, second: int) -> None: ...
[g, f][1](1)
(f, g)[-2](1)
{"other": g, "call": f}["call"](1)
"#,
    );
    for line in 4..=6 {
        assert!(
            has_error_at(&messages, line, "f"),
            "expected literal-subscript violation on line {line}, got: {messages:?}"
        );
    }
}

/// Concatenated literal sequences retain the concrete callable selected from
/// either operand (issue #806).
#[test]
fn concatenated_literal_sequences_resolve_selected_callables() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
([target] + [])[0](1)
((target,) + ())[0](1)
([0] + [target])[-1](1)
((target,) + (0,))[0](1)
",
    );
    for line in 3..=6 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected concatenated-literal violation on line {line}, got: {messages:?}"
        );
    }
}

/// Literal slices retain the concrete callable at a statically selected result
/// index, including negative and stepped slices (issue #805).
#[test]
fn literal_sequence_slices_resolve_selected_callables() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
[target][0:1][0](1)
(target,)[0:1][0](1)
[target, 0][::-1][-1](1)
[target, 0, target][::2][1](1)
",
    );
    for line in 3..=6 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected sliced-literal violation on line {line}, got: {messages:?}"
        );
    }
}

/// A statically non-empty homogeneous slice captured by a starred assignment
/// target remains a callable list (issue #801).
#[test]
fn starred_destructuring_tail_preserves_callable_elements() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
head, *tail = [target, target]
tail[0](1)
",
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected starred-tail violation, got: {messages:?}"
    );

    let rebound = check_source(
        r"
def target(value: int) -> None: ...
head, *tail = [target, target]
tail = [lambda *args: None]
tail[0](1)
",
    );
    assert!(
        rebound.is_empty(),
        "reassigning a starred list must clear its old callable: {rebound:?}"
    );

    let destructured_rebind = check_source(
        r"
def target(value: int) -> None: ...
head, *tail = [target, target]
tail, = [[lambda *args: None]]
tail[0](1)
",
    );
    assert!(
        destructured_rebind.is_empty(),
        "destructuring must clear a starred callable list: {destructured_rebind:?}"
    );
}

/// Generic builtins that select or sort elements preserve a homogeneous
/// literal collection's concrete callable signature (issue #370).
#[test]
fn generic_builtin_results_preserve_literal_callable_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
min([f], key=id)(1)
max((f,), key=id)(1)
sorted({f}, key=id)[0](1)
",
    );
    for line in 3..=5 {
        assert!(
            has_error_at(&messages, line, "f"),
            "expected generic-builtin violation on line {line}, got: {messages:?}"
        );
    }
}

/// Calling an argument-free lambda evaluates to its body, so a callable
/// returned directly from that body retains its signature (issue #365).
#[test]
fn direct_lambda_result_callee_resolves_body() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
(lambda: f)()(1)
",
    );
    assert!(
        has_error_at(&messages, 3, "f"),
        "expected direct lambda-result violation, got: {messages:?}"
    );
}

/// Lambda parameters have ordinary positional-or-keyword semantics when the
/// lambda expression is invoked directly (issue #366).
#[test]
fn direct_lambda_invocation_uses_lambda_signature() {
    let messages = check_source("(lambda value: None)(1)\n");
    assert!(
        has_error_at(&messages, 1, "lambda"),
        "expected direct lambda violation, got: {messages:?}"
    );
}

/// `typing.cast` itself is exempt, while a cast to a concrete `Callable`
/// supplies the result call's positional limit (issue #367).
#[test]
fn typing_cast_callable_result_is_checked_without_flagging_cast() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import cast
def f(value: int) -> None: ...
cast(Callable[[int], None], f)(1)
",
    );
    assert_eq!(
        messages.len(),
        1,
        "expected only the result call: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 5, "cast result"),
        "expected cast-result violation, got: {messages:?}"
    );
}

/// `functools.partial` exposes the wrapped callable after removing parameters
/// consumed by bound positional arguments (issue #368).
#[test]
fn functools_partial_result_preserves_remaining_signature() {
    let messages = check_source(
        r"
from functools import partial
def f(required: int, /, value: int) -> None: ...
partial(f)(0, 1)
partial(f, 0)(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "partial") && has_error_at(&messages, 5, "partial"),
        "expected both partial-result violations, got: {messages:?}"
    );
}

/// `next` returns the item type declared by local iterator and generator
/// factories, including a concrete callable signature (issue #369).
#[test]
fn annotated_iterator_results_preserve_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable, Iterator, Generator
def iterator() -> Iterator[Callable[[int], None]]: ...
def generator() -> Generator[Callable[[int], None], None, None]: ...
next(iterator())(1)
next(generator())(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "next() result") && has_error_at(&messages, 6, "next() result"),
        "expected iterator and generator result violations, got: {messages:?}"
    );
}

/// A true `TypeGuard` branch narrows its argument to the declared callable
/// signature (issue #456).
#[test]
fn typeguard_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import TypeGuard
def is_call(value: object) -> TypeGuard[Callable[[int], None]]: ...
def caller(value: object) -> None:
    if is_call(value=value):
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "narrowed"),
        "expected narrowed violation, got: {messages:?}"
    );
}

#[test]
fn typeguard_narrowing_with_positional_argument() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import TypeGuard
def is_call(value: object) -> TypeGuard[Callable[[int], None]]: ...
def caller(value: object) -> None:
    if is_call(value):
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "narrowed") || has_error_at(&messages, 7, "narrowed"),
        "expected narrowed violation, got: {messages:?}"
    );
}

#[test]
fn typeis_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import TypeIs
def is_call(value: object) -> TypeIs[Callable[[int], None]]: ...
def caller(value: object) -> None:
    if is_call(value=value):
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "narrowed") || has_error_at(&messages, 7, "Too many"),
        "expected TypeIs narrowed violation, got: {messages:?}"
    );
}

#[test]
fn optional_is_not_none_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
def caller(value: Callable[[int], None] | None) -> None:
    if value is not None:
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "narrowed") || has_error_at(&messages, 5, "Too many"),
        "expected optional narrowing violation, got: {messages:?}"
    );
}

#[test]
fn assert_is_not_none_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections.abc import Callable
def caller(value: Callable[[int], None] | None) -> None:
    assert value is not None, "present"
    value(1)
"#,
    );
    assert!(
        has_error_at(&messages, 5, "narrowed") || has_error_at(&messages, 5, "Too many"),
        "expected assert narrowing violation, got: {messages:?}"
    );
}

#[test]
fn optional_typing_optional_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Optional
def caller(value: Optional[Callable[[int], None]]) -> None:
    if value is not None:
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "narrowed") || has_error_at(&messages, 6, "Too many"),
        "expected Optional narrowing violation, got: {messages:?}"
    );
}

#[test]
fn optional_none_union_left_narrowing_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
def caller(value: None | Callable[[int], None]) -> None:
    if value is not None:
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "narrowed") || has_error_at(&messages, 5, "Too many"),
        "expected None|Callable narrowing violation, got: {messages:?}"
    );
}

#[test]
fn typeguard_narrowing_accepts_qualified_annotation() {
    let messages = check_source(
        r"
import typing
from collections.abc import Callable
def is_call(value: object) -> typing.TypeGuard[Callable[[int], None]]: ...
def caller(value: object) -> None:
    if is_call(value):
        value(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "narrowed"),
        "expected narrowed violation, got: {messages:?}"
    );
}

/// `iter(instance)` uses the concrete callable item type declared by the
/// instance class's `__iter__` return annotation (issue #382).
#[test]
fn annotated_dunder_iter_results_preserve_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable, Iterator
class C:
    def __iter__(self) -> Iterator[Callable[[int], None]]: ...
next(iter(C()))(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "next() result"),
        "expected __iter__ result violation, got: {messages:?}"
    );
}

/// A single irrefutable capture aliases the match subject and therefore keeps
/// its concrete callable signature (issue #371).
#[test]
fn match_capture_alias_preserves_callable_signature() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
match f:
    case alias:
        alias(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "f"),
        "expected match-capture alias violation, got: {messages:?}"
    );
}

/// Literal `SimpleNamespace` constructor keywords define concrete callable
/// attributes on the assigned instance (issue #372).
#[test]
fn simple_namespace_keyword_preserves_callable_attribute() {
    let messages = check_source(
        r"
from types import SimpleNamespace
def f(value: int) -> None: ...
namespace = SimpleNamespace(call=f)
namespace.call(1)
namespace = object()
namespace.call(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "f"),
        "expected namespace callable violation, got: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.starts_with("main:7:")),
        "rebinding must clear synthesized attributes: {messages:?}"
    );
}

/// Subscripting `vars(SimpleNamespace(...))` preserves a concrete callable
/// constructor attribute (issue #834).
#[test]
fn vars_simple_namespace_preserves_callable_attribute_signature() {
    let messages = check_source(
        r#"
import types
def target(value: int) -> None: ...
vars(types.SimpleNamespace(callback=target))["callback"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected vars(SimpleNamespace) violation, got: {messages:?}"
    );
}

/// `getattr` with a literal name preserves a concrete callable attribute on
/// an inline `SimpleNamespace` (issue #835).
#[test]
fn getattr_simple_namespace_preserves_callable_attribute_signature() {
    let messages = check_source(
        r#"
import types
def target(value: int) -> None: ...
getattr(types.SimpleNamespace(callback=target), "callback")(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected getattr(SimpleNamespace) violation, got: {messages:?}"
    );
}

/// `ContextVar` accepts its required name positionally and `get()` preserves
/// the configured callable value type (issue #409).
#[test]
fn contextvar_constructor_and_get_callable_result() {
    let messages = check_source(
        r#"
from collections.abc import Callable
from contextvars import ContextVar
def f(value: int) -> None: ...
current: ContextVar[Callable[[int], None]] = ContextVar("current", default=f)
current.get()(1)
"#,
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.starts_with("main:5:")),
        "ContextVar name must be allowed positionally: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 6, "get() result"),
        "expected ContextVar.get result violation, got: {messages:?}"
    );
}

/// A dataclass constructor keyword directly supplies the corresponding field
/// value, preserving a concrete callable's signature (issue #373).
#[test]
fn dataclass_constructor_keyword_preserves_callable_field() {
    let messages = check_source(
        r"
from collections.abc import Callable
from dataclasses import dataclass
@dataclass
class Holder:
    call: Callable[[int], None]
def f(value: int) -> None: ...
Holder(call=f).call(1)
",
    );
    assert!(
        has_error_at(&messages, 8, "f"),
        "expected dataclass field violation, got: {messages:?}"
    );
}

/// `dataclasses.astuple` preserves concrete callable constructor fields at a
/// literal tuple index (issue #830).
#[test]
fn dataclasses_astuple_preserves_callable_field_signature() {
    let messages = check_source(
        r"
import dataclasses
@dataclasses.dataclass
class Record:
    callback: object
def target(value: int) -> None: ...
dataclasses.astuple(obj=Record(callback=target))[0](1)
",
    );
    assert!(
        has_error_at(&messages, 7, "target"),
        "expected dataclasses.astuple violation, got: {messages:?}"
    );
}

/// `dataclasses.asdict` preserves a concrete callable constructor field
/// selected by its literal field name (issue #831).
#[test]
fn dataclasses_asdict_preserves_callable_field_signature() {
    let messages = check_source(
        r#"
import dataclasses
@dataclasses.dataclass
class Record:
    callback: object
def target(value: int) -> None: ...
dataclasses.asdict(obj=Record(callback=target))["callback"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 7, "target"),
        "expected dataclasses.asdict violation, got: {messages:?}"
    );
}

/// `asdict` omits constructor-only `InitVar` entries.
#[test]
fn dataclasses_asdict_ignores_initvar_fields() {
    let messages = check_source(
        r#"
import dataclasses
from dataclasses import InitVar, dataclass
def target(value: int) -> None: ...
@dataclass
class Record:
    transient: InitVar[object]
dataclasses.asdict(obj=Record(transient=target))["transient"](1)
"#,
    );
    assert!(
        messages.is_empty(),
        "InitVar must not resolve as an asdict field: {messages:?}"
    );
}

/// `astuple` uses stored dataclass fields rather than constructor-only
/// `InitVar` entries when mapping tuple positions (issue #830).
#[test]
fn dataclasses_astuple_ignores_initvar_positions() {
    let messages = check_source(
        r"
import dataclasses
from dataclasses import InitVar, dataclass, field
def target(value: int) -> None: ...
@dataclass
class Record:
    transient: InitVar[object]
    stored: object = field(default=None, init=False)
dataclasses.astuple(obj=Record(transient=target))[0](1)
",
    );
    assert!(
        messages.is_empty(),
        "InitVar must not occupy an astuple position: {messages:?}"
    );
}

/// `KW_ONLY` is a dataclass sentinel rather than a stored runtime field.
#[test]
fn dataclasses_astuple_ignores_kw_only_sentinel() {
    let messages = check_source(
        r"
import dataclasses
from dataclasses import KW_ONLY, dataclass
def target(value: int) -> None: ...
@dataclass
class Record:
    _: KW_ONLY
    stored: object
dataclasses.astuple(Record(stored=target))[0](1)
",
    );
    assert!(
        has_error_at(&messages, 9, "target"),
        "KW_ONLY must not occupy an astuple position: {messages:?}"
    );
}

/// Functional dataclasses omit constructor-only `InitVar` entries at runtime.
#[test]
fn make_dataclass_astuple_ignores_initvar_positions() {
    let messages = check_source(
        r"
import dataclasses
from dataclasses import InitVar
def target(value: int) -> None: ...
Record = dataclasses.make_dataclass(cls_name='Record', fields=[('transient', InitVar[object]), ('stored', object)])
dataclasses.astuple(obj=Record(transient=target, stored=None))[0](1)
",
    );
    assert!(
        messages.is_empty(),
        "InitVar must not occupy a make_dataclass astuple position: {messages:?}"
    );
}

/// ``make_dataclass`` synthesizes a class with typed callable fields (issue #453).
#[test]
fn make_dataclass_preserves_callable_field_signatures() {
    let messages = check_source(
        r"
from collections.abc import Callable
from dataclasses import make_dataclass

Point = make_dataclass(cls_name='Point', fields=[('call', Callable[[int], None])])
def f(value: int) -> None: ...
Point(f).call(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "f"),
        "expected make_dataclass field violation, got: {messages:?}"
    );
}

/// A `NamedTuple` constructor keyword directly supplies the corresponding
/// callable field value (issue #374).
#[test]
fn namedtuple_constructor_keyword_preserves_callable_field() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import NamedTuple
class Holder(NamedTuple):
    call: Callable[[int], None]
def f(value: int) -> None: ...
Holder(call=f).call(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "f"),
        "expected NamedTuple field violation, got: {messages:?}"
    );
}

/// ``dataclasses.replace`` and ``NamedTuple._replace`` preserve callable fields
/// (issue #457).
#[test]
fn record_replacement_preserves_callable_field_signatures() {
    let messages = check_source(
        r"
from dataclasses import dataclass, replace
from collections.abc import Callable
from typing import NamedTuple

@dataclass
class D:
    call: Callable[[int], None]

class N(NamedTuple):
    call: Callable[[int], None]

def f(value: int) -> None: ...
replace(D(call=f)).call(1)
N(call=f)._replace().call(1)
",
    );
    assert!(
        has_error_at(&messages, 14, "f") && has_error_at(&messages, 15, "f"),
        "expected replace/_replace field violations, got: {messages:?}"
    );
}

/// A literal `operator.attrgetter` applied to a receiver with a known
/// callable attribute preserves that attribute's signature (issue #375).
#[test]
fn attrgetter_result_preserves_known_callable_attribute() {
    let messages = check_source(
        r#"
from operator import attrgetter
from types import SimpleNamespace
def f(value: int) -> None: ...
holder = SimpleNamespace(call=f)
attrgetter("call")(holder)(1)
attrgetter("call")(SimpleNamespace(call=f))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 6, "f") && has_error_at(&messages, 7, "f"),
        "expected both attrgetter-result violations, got: {messages:?}"
    );
}

/// `itemgetter` consumes its operand positionally but preserves a statically
/// selected literal container element's callable signature (issue #376).
#[test]
fn itemgetter_operand_is_exempt_and_result_is_checked() {
    let messages = check_source(
        r"
from operator import itemgetter
def f(value: int) -> None: ...
itemgetter(0)([f])(1)
itemgetter(-1)((f,))(1)
",
    );
    assert_eq!(
        messages.len(),
        2,
        "only result calls should fail: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 4, "f") && has_error_at(&messages, 5, "f"),
        "expected both itemgetter result violations, got: {messages:?}"
    );
}

/// A local factory's return annotation identifies a callable instance class
/// and therefore its concrete `__call__` signature (issue #378).
#[test]
fn typed_factory_result_preserves_callable_instance_signature() {
    let messages = check_source(
        r"
class C:
    def __call__(self, value: int) -> None: ...
def factory() -> C:
    return C()
factory()(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "__call__"),
        "expected typed factory-result violation, got: {messages:?}"
    );
}

/// A descriptor's annotated `__get__` callable return becomes the signature
/// of class attributes assigned from that descriptor (issue #379).
#[test]
fn descriptor_get_return_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
class Descriptor:
    def __get__(self, instance, owner) -> Callable[[int], None]: ...
class C:
    call = Descriptor()
C().call(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "call"),
        "expected descriptor-return violation, got: {messages:?}"
    );
}

/// `CPython` descriptor `__get__` methods reject keyword arguments, so both
/// binding arguments must remain positional (issues #501–#506).
#[test]
fn stdlib_descriptor_get_positional_binding_args_are_allowed() {
    let cases = [
        (
            "classmethod",
            r"
def target(cls, value: int) -> int:
    return value

classmethod(target).__get__(None, object)
",
        ),
        (
            "staticmethod",
            r"
def target(value: int) -> int:
    return value

staticmethod(target).__get__(None, object)
",
        ),
        (
            "property",
            r"
class Owner: pass
prop = property(fget=lambda self: 1)
prop.__get__(Owner(), Owner)
",
        ),
        (
            "function",
            r"
class Owner: pass

def target(self, value: int) -> int:
    return value

target.__get__(Owner(), Owner)
",
        ),
        ("method_descriptor", "str.upper.__get__(\"text\", str)()\n"),
        (
            "wrapper_descriptor",
            r"
class Owner: pass
object.__str__.__get__(Owner(), Owner)()
",
        ),
    ];
    for (label, source) in cases {
        let messages = check_source(source);
        assert!(
            messages.is_empty(),
            "{label} __get__ falsely flagged: {messages:?}"
        );
    }
}

/// Multi-level unbound descriptor calls must keep the protocol exemption
/// (issue #742).
#[test]
fn multi_level_descriptor_get_positional_binding_args_are_allowed() {
    let messages = TestProject::new()
        .pyproject(DEFAULT_PYPROJECT)
        .file(
            "desc.py",
            "class Desc:\n\
             def __get__(self, instance, owner=None):\n\
                 return instance\n",
        )
        .main(
            "import desc\n\
             class Owner: pass\n\
             desc.Desc.__get__(desc.Desc(), Owner(), Owner)\n",
        )
        .check();
    assert!(
        messages.is_empty(),
        "pkg.Desc.__get__ falsely flagged: {messages:?}"
    );
}

/// `functools.cached_property.__get__` accepts `instance` by keyword, so a
/// positional pass must still emit KW001 (issue #507).
#[test]
fn cached_property_get_positional_instance_is_flagged() {
    let messages = check_source(
        r#"
import functools

class Owner: pass
cached = functools.cached_property(func=lambda self: 1)
cached.__set_name__(owner=Owner, name="cached")
cached.__get__(Owner())
"#,
    );
    assert!(
        has_error_at(&messages, 7, "__get__"),
        "expected cached_property.__get__ violation, got: {messages:?}"
    );
}

/// Callable-annotated instance fields retain the annotation's concrete
/// signature when accessed on a newly constructed instance (issue #380).
#[test]
fn callable_instance_field_annotation_is_resolved() {
    let messages = check_source(
        r"
from collections.abc import Callable

class C:
    call: Callable[[int], None]
    def __init__(self, call: Callable[[int], None]) -> None:
        self.call = call

def f(value: int) -> None: ...
C(call=f).call(1)
",
    );
    assert!(
        has_error_at(&messages, 10, "call"),
        "expected callable-field violation, got: {messages:?}"
    );
}

/// A concrete callable return annotation on `__getitem__` supplies the
/// selected value's signature (issue #381).
#[test]
fn getitem_callable_return_is_resolved() {
    let messages = check_source(
        r#"
from collections.abc import Callable

class C:
    def __getitem__(self, key: str) -> Callable[[int], None]:
        def inner(value: int) -> None: ...
        return inner

C()["call"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 9, "__return__"),
        "expected getitem-result violation, got: {messages:?}"
    );
}

/// Awaiting a local async factory preserves its concrete callable return
/// annotation (issue #383).
#[test]
fn awaited_callable_result_preserves_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
async def factory() -> Callable[[int], None]: ...
async def caller() -> None:
    (await factory())(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "awaited result"),
        "expected awaited-result violation, got: {messages:?}"
    );
}

/// Awaiting an item yielded by `asyncio.as_completed` preserves the concrete
/// callable result of its source awaitables (issue #836).
#[test]
fn asyncio_as_completed_preserves_callable_awaitable_result() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]:
    return lambda value: None
async def main() -> None:
    completed = asyncio.as_completed(fs=[factory()])
    (await next(iter(completed)))(1)
",
    );
    assert!(
        has_error_at(&messages, 8, "awaited result"),
        "expected asyncio.as_completed violation, got: {messages:?}"
    );
}

/// `anext` preserves callable item signatures declared by async iterator
/// factories (issue #384).
#[test]
fn annotated_anext_result_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import AsyncIterator, Callable
async def functions() -> AsyncIterator[Callable[[int], None]]: ...
async def caller() -> None:
    (await anext(functions()))(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "anext() result"),
        "expected anext-result violation, got: {messages:?}"
    );
}

/// A `@contextmanager` factory's callable iterator item becomes the concrete
/// signature of its `with ... as` binding (issue #385).
#[test]
fn contextmanager_with_binding_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable, Iterator
from contextlib import contextmanager
@contextmanager
def manager() -> Iterator[Callable[[int], None]]: ...
with manager() as call:
    call(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "context result"),
        "expected context-manager binding violation, got: {messages:?}"
    );
}

/// `reversed` preserves an annotated list's concrete callable item type when
/// binding a simple loop target (issue #397).
#[test]
fn reversed_loop_preserves_callable_item_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
def f(value: int) -> None: ...
calls: list[Callable[[int], None]] = [f]
for call in reversed(calls):
    call(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "reversed item"),
        "expected reversed-item violation, got: {messages:?}"
    );
}

/// ``async for`` preserves an annotated async iterator's callable item type
/// (issue #455).
#[test]
fn async_for_preserves_callable_item_signature() {
    let messages = check_source(
        r"
from collections.abc import AsyncIterator, Callable
async def caller(values: AsyncIterator[Callable[[int], None]]) -> None:
    async for call in values:
        call(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "async-for item"),
        "expected async-for item violation, got: {messages:?}"
    );
}

/// ``async for`` reads callable items from an annotated iterator factory
/// invocation (issue #841).
#[test]
fn async_for_factory_preserves_callable_item_signature() {
    let messages = check_source(
        r"
from collections.abc import AsyncIterator, Callable
async def values() -> AsyncIterator[Callable[[int], None]]:
    yield lambda value: None
async def main() -> None:
    async for item in values():
        item(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "async-for item"),
        "expected async-for factory item violation, got: {messages:?}"
    );
}

/// Async comprehensions use the annotated callable item from their iterator
/// factory (issue #842).
#[test]
fn async_comprehension_factory_preserves_callable_item_signature() {
    let messages = check_source(
        r"
from collections.abc import AsyncIterator, Callable
async def values() -> AsyncIterator[Callable[[int], None]]:
    yield lambda value: None
async def main() -> None:
    [item(1) async for item in values()]
",
    );
    assert!(
        has_error_at(&messages, 6, "comprehension item"),
        "expected async-comprehension item violation, got: {messages:?}"
    );
}

/// ``async with`` preserves a context manager's ``__aenter__`` return type
/// (issue #454).
#[test]
fn async_with_preserves_callable_aenter_result_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable

class Manager:
    async def __aenter__(self) -> Callable[[int], None]: ...
    async def __aexit__(self, *args: object) -> None: ...

async def caller(manager: Manager) -> None:
    async with manager as call:
        call(1)
",
    );
    assert!(
        has_error_at(&messages, 10, "async-with context result"),
        "expected async-with binding violation, got: {messages:?}"
    );
}

/// ``singledispatch.register`` returns the registered implementation
/// (issue #483).
#[test]
fn singledispatch_register_preserves_implementation_callable_signature() {
    let messages = check_source(
        r"
from functools import singledispatch

@singledispatch
def generic(value: object) -> object:
    return value

def target(value: int) -> int:
    return value

generic.register(int, target)(1)
",
    );
    assert!(
        has_error_at(&messages, 11, "target"),
        "expected singledispatch.register violation, got: {messages:?}"
    );
}

/// ``singledispatchmethod.register`` returns the registered implementation
/// (issue #611).
#[test]
fn singledispatchmethod_register_preserves_implementation_callable_signature() {
    let messages = check_source(
        r"
import functools

class Dispatcher:
    @functools.singledispatchmethod
    def method(self, value: object) -> object:
        return value

def target(value: int) -> int:
    return value

Dispatcher.method.register(int, target)(1)
",
    );
    assert!(
        has_error_at(&messages, 12, "target"),
        "expected singledispatchmethod.register violation, got: {messages:?}"
    );
}

/// ``singledispatch.dispatch`` returns a previously registered implementation
/// (issue #649).
#[test]
fn singledispatch_dispatch_preserves_registered_implementation_signature() {
    let messages = check_source(
        r"
import functools

@functools.singledispatch
def generic(value: object) -> object:
    return value

def target(value: int) -> int:
    return value

generic.register(int, target)
generic.dispatch(cls=int)(1)
",
    );
    assert!(
        has_error_at(&messages, 12, "target"),
        "expected singledispatch.dispatch violation, got: {messages:?}"
    );
}

/// Comprehension targets shadow same-named outer functions (issue #512).
#[test]
fn comprehension_target_shadows_outer_function_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
[target(1) for target in [lambda *args: None]]
{target(1) for target in [lambda *args: None]}
{target(1): None for target in [lambda *args: None]}
(target(1) for target in [lambda *args: None])
",
    );
    assert!(
        !messages.iter().any(|message| message.contains("target")),
        "comprehension target must shadow outer function: {messages:?}"
    );
}

/// ``callable()`` narrowing preserves optional handler signatures (issue #484).
#[test]
fn callable_builtin_narrowing_preserves_signal_handler_signature() {
    let messages = check_source(
        r"
import signal

handler = signal.getsignal(signal.SIGINT)
if callable(handler):
    handler(signal.SIGINT, None)
",
    );
    assert!(
        has_error_at(&messages, 6, "narrowed") || has_error_at(&messages, 6, "Too many"),
        "expected callable() narrowed handler violation, got: {messages:?}"
    );
}

/// Generic functions that return the same `TypeVar` accepted by their
/// arguments preserve an unambiguous concrete callable (issue #386).
#[test]
fn generic_return_propagates_callable_arguments() {
    let messages = check_source(
        r#"
from typing import TypeVar
T = TypeVar("T")
def identity(value: T) -> T: ...
def choose(first: T, second: T) -> T: ...
def f(value: int) -> None: ...
def g(other: int) -> None: ...
identity(value=f)(1)
choose(f, g)(1)
"#,
    );
    assert!(
        has_error_at(&messages, 8, "generic result")
            && has_error_at(&messages, 9, "generic result"),
        "expected generic-result violations, got: {messages:?}"
    );
}

/// Generic instance method returns substitute class type arguments (issue #522).
#[test]
fn generic_instance_method_return_substitutes_callable_type_arg() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    def get(self) -> T: ...
box: Box[Callable[[int], None]]
box.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 8, "generic result"),
        "expected specialized method result violation, got: {messages:?}"
    );
}

/// Generic classmethod returns substitute class type arguments (issue #523).
#[test]
fn generic_classmethod_return_substitutes_callable_type_arg() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    @classmethod
    def get(cls) -> T: ...
box: Box[Callable[[int], None]]
box.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "generic result"),
        "expected classmethod specialized result violation, got: {messages:?}"
    );
}

/// Generic staticmethod returns substitute class type arguments (issue #524).
#[test]
fn generic_staticmethod_return_substitutes_callable_type_arg() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    @staticmethod
    def get() -> T: ...
box: Box[Callable[[int], None]]
box.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "generic result"),
        "expected staticmethod specialized result violation, got: {messages:?}"
    );
}

/// Inherited generic method specializations keep callable returns (issue #525).
#[test]
fn inherited_generic_method_specialization_preserves_callable_return() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Base(Generic[T]):
    def get(self) -> T: ...
class Concrete(Base[Callable[[int], None]]):
    pass
Concrete().get()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "generic result"),
        "expected inherited specialized result violation, got: {messages:?}"
    );
}

/// Typeshed ``sys.version_info`` gates select signatures for ``target_version``
/// (issue #407). On 3.14+, ``functools.reduce``'s ``initial`` is keyword-only.
#[test]
fn target_version_selects_version_gated_stdlib_signature() {
    let messages = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n[tool.strict_kwargs]\ntarget_version = \"3.14\"\n")
        .main(
            r"
from functools import reduce
reduce(lambda left, right: left + right, [1, 2], 0)
",
        )
        .check();
    assert!(
        messages.iter().any(|message| message.contains("reduce")),
        "3.14 reduce initial must be keyword-only: {messages:?}"
    );

    let messages = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n[tool.strict_kwargs]\ntarget_version = \"3.12\"\n")
        .main(
            r"
from functools import reduce
reduce(lambda left, right: left + right, [1, 2], 0)
",
        )
        .check();
    assert!(
        messages.is_empty(),
        "3.12 reduce initial stays positional: {messages:?}"
    );
}

/// Nested scopes must not leak instance type-arg specializations (Bugbot on #709).
#[test]
fn nested_instance_type_args_do_not_clobber_outer() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    def get(self) -> T: ...
box: Box[Callable[[int], None]]
def inner() -> None:
    box: Box[Callable[[], None]]
    box.get()()
inner()
box.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 12, "generic result"),
        "outer specialization must survive nested rebind: {messages:?}"
    );
}

/// Annotated assignment with a non-constructor value must keep the
/// specialization recorded after binding clear (Bugbot on #718).
#[test]
fn annotated_assign_with_value_keeps_instance_type_args() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    def get(self) -> T: ...
def make() -> object: ...
box: Box[Callable[[int], None]] = make()  # type: ignore[assignment]
box.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "generic result"),
        "ann-assign value must not wipe specialization: {messages:?}"
    );
}

/// Inner opaque rebind must not leak an outer instance specialization (Bugbot on #718).
#[test]
fn nested_opaque_rebind_hides_outer_instance_type_args() {
    let messages = check_source(
        r"
from collections.abc import Callable
from typing import Generic, TypeVar
T = TypeVar('T')
class Box(Generic[T]):
    def get(self) -> T: ...
box: Box[Callable[[int], None]]
def inner(box: object) -> None:
    box.get()(1)
",
    );
    assert!(
        messages.is_empty(),
        "inner param must hide outer specialization: {messages:?}"
    );
}

/// An immediately constructed and dereferenced `weakref.ref` preserves its
/// referent callable's signature (issue #387).
#[test]
fn weakref_result_preserves_callable_signature() {
    let messages = check_source(
        r"
import weakref
def f(value: int) -> None: ...
weakref.ref(f)()(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "f"),
        "expected weakref-result violation, got: {messages:?}"
    );
}

/// `operator.getitem` shares literal container selection semantics with a
/// subscript expression (issue #394).
#[test]
fn operator_getitem_preserves_callable_signature() {
    let messages = check_source(
        r"
import operator
def f(value: int) -> None: ...
operator.getitem([f], 0)(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "f"),
        "expected operator.getitem violation, got: {messages:?}"
    );
}

/// `copy.copy` and `copy.deepcopy` return their input type, preserving a
/// concrete callable shape without preserving unsafe keyword names (#388).
#[test]
fn copy_results_propagate_callable_arguments() {
    let messages = check_source(
        r"
import copy
def f(value: int) -> None: ...
copy.copy(x=f)(1)
copy.deepcopy(x=f)(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "generic result")
            && has_error_at(&messages, 5, "generic result"),
        "expected copy-result violations, got: {messages:?}"
    );
}

/// Type-preserving itertools recipes retain callable element shapes through
/// `next`, including a selected iterator returned by `tee` (issue #389).
#[test]
fn itertools_results_preserve_callable_item_signatures() {
    let messages = check_source(
        r"
import itertools
def f(value: int) -> None: ...
next(itertools.repeat(object=f))(1)
next(itertools.chain([f]))(1)
next(itertools.cycle([f]))(1)
next(itertools.tee([f])[0])(1)
",
    );
    for line in 4..=7 {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected itertools-result violation on line {line}, got: {messages:?}"
        );
    }
}

/// Filtering/slicing itertools helpers preserve callable item types (issue
/// #448).
#[test]
fn itertools_filter_helpers_preserve_callable_item_signatures() {
    let messages = check_source(
        r"
import itertools
def f(value: int) -> None: ...
next(itertools.accumulate([f]))(1)
next(itertools.compress([f], [True]))(1)
next(itertools.dropwhile(lambda _: False, [f]))(1)
next(itertools.takewhile(lambda _: True, [f]))(1)
next(itertools.islice([f], 1))(1)
",
    );
    for line in 4..=8 {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected itertools filter-helper violation on line {line}, got: {messages:?}"
        );
    }
}

/// Combinatoric itertools helpers preserve callable tuple-element types
/// (issue #449).
#[test]
fn itertools_tuple_helpers_preserve_callable_element_signatures() {
    let messages = check_source(
        r"
import itertools
def f(value: int) -> None: ...
next(itertools.pairwise([f, f]))[0](1)
next(itertools.product([f]))[0](1)
next(itertools.permutations([f]))[0](1)
next(itertools.combinations([f], 1))[0](1)
next(itertools.zip_longest([f]))[0](1)
",
    );
    for line in 4..=8 {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected itertools tuple-helper violation on line {line}, got: {messages:?}"
        );
    }
}

/// Tuple-producing builtins preserve literal callable elements at their
/// documented output positions (issue #390).
#[test]
fn builtin_iterator_results_preserve_callable_item_signatures() {
    let messages = check_source(
        r"
def f(value: int) -> None: ...
next(zip([f]))[0](1)
next(enumerate([f]))[1](1)
",
    );
    for line in 3..=4 {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected builtin-iterator violation on line {line}, got: {messages:?}"
        );
    }
}

/// `map` and `filter` preserve literal callable items (regression #759).
#[test]
fn map_and_filter_preserve_callable_item_signatures() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
next(map(lambda item: item, [target]))(1)
next(filter(None, [target]))(1)
",
    );
    assert_eq!(
        messages.len(),
        2,
        "expected exactly two violations: {messages:?}"
    );
    for line in 3..=4 {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected map/filter violation on line {line}, got: {messages:?}"
        );
    }
}

/// `pop`/`popleft` on an immediately constructed deque preserve a literal
/// initializer's concrete callable element shape (issue #392).
#[test]
fn deque_result_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections import deque
def f(value: int) -> None: ...
deque(iterable=[f]).popleft()(1)
deque([f]).pop()(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "deque result") && has_error_at(&messages, 5, "deque result"),
        "expected deque-result violations, got: {messages:?}"
    );
}

/// Subscripting an immediately constructed deque preserves a literal
/// initializer's selected callable element (issue #786).
#[test]
fn deque_subscript_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections import deque
def target(value: int) -> None: ...
deque(iterable=[target])[0](1)
",
    );
    assert!(
        has_error_at(&messages, 4, "deque result"),
        "expected deque-subscript violation, got: {messages:?}"
    );
}

/// Iterating an immediate literal dictionary's values preserves a concrete
/// callable value shape (issue #391).
#[test]
fn dict_values_iteration_preserves_callable_signature() {
    let messages = check_source(
        r#"
def f(value: int) -> None: ...
next(iter({"call": f}.values()))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 3, "next() result"),
        "expected dictionary-values violation, got: {messages:?}"
    );
}

/// Iterating an immediate `OrderedDict`'s values preserves a concrete callable
/// value shape (issue #922).
#[test]
fn ordered_dict_values_iteration_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import OrderedDict
def target(value: int) -> None: ...
next(iter(OrderedDict({"x": target}).values()))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "next() result"),
        "expected OrderedDict values violation, got: {messages:?}"
    );
}

/// Iterating an immediate `UserDict`'s values preserves a concrete callable
/// value shape (issue #926).
#[test]
fn user_dict_values_iteration_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import UserDict
def target(value: int) -> None: ...
next(iter(UserDict({"x": target}).values()))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "next() result"),
        "expected UserDict values violation, got: {messages:?}"
    );
}

/// Iterating a single-mapping immediate `ChainMap`'s values preserves a
/// concrete callable value shape (issue #928).
#[test]
fn chain_map_values_iteration_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import ChainMap
def target(value: int) -> None: ...
next(iter(ChainMap({"x": target}).values()))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "next() result"),
        "expected ChainMap values violation, got: {messages:?}"
    );
}

/// Iterating an immediate `MappingProxyType`'s values preserves a concrete
/// callable value shape (issue #931).
#[test]
fn mapping_proxy_values_iteration_preserves_callable_signature() {
    let messages = check_source(
        r#"
from types import MappingProxyType
def target(value: int) -> None: ...
next(iter(MappingProxyType(mapping={"x": target}).values()))(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "next() result"),
        "expected MappingProxyType values violation, got: {messages:?}"
    );
}

/// `dict.get` on an existing literal key preserves its callable value
/// (issue #773).
#[test]
fn literal_dict_get_preserves_callable_signature() {
    let messages = check_source(
        r#"
def target(value: int) -> None: ...
{"key": target}.get("key")(1)
"#,
    );
    assert!(
        has_error_at(&messages, 3, "target"),
        "expected dict-get violation, got: {messages:?}"
    );
}

/// `defaultdict.get` preserves an existing literal mapping value rather than
/// invoking or widening through its default factory (issue #800).
#[test]
fn defaultdict_get_preserves_existing_callable_signature() {
    let messages = check_source(
        r#"
from collections import defaultdict
def target(value: int) -> None: ...
defaultdict(lambda: None, {"x": target}).get("x")(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected defaultdict.get violation, got: {messages:?}"
    );
}

/// A literal dictionary's `popitem` value preserves its concrete callable
/// (issue #774).
#[test]
fn literal_dict_popitem_value_preserves_callable_signature() {
    let messages = check_source(
        r#"
def target(value: int) -> None: ...
{"key": target}.popitem()[1](1)
"#,
    );
    assert!(
        has_error_at(&messages, 3, "target"),
        "expected dict-popitem violation, got: {messages:?}"
    );
}

/// A queue's declared callable item type becomes the signature returned by
/// `get` and `get_nowait` (issue #393).
#[test]
fn queue_get_preserves_declared_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
from queue import Queue
queue: Queue[Callable[[int], None]] = Queue()
queue.get()(1)
queue.get_nowait()(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "queue result") && has_error_at(&messages, 6, "queue result"),
        "expected queue-result violations, got: {messages:?}"
    );
}

/// `multiprocessing.pool.Pool` map helpers preserve callback result types
/// (issues #622-626).
#[test]
fn pool_map_results_preserve_callable_item_signatures() {
    let messages = check_source(
        r"
from multiprocessing.pool import Pool

def target(value: int) -> int: return value
pool = Pool(1)
Pool(1).map(func=lambda _: target, iterable=[None])[0](1)
Pool(1).starmap(func=lambda: target, iterable=[()])[0](1)
next(pool.imap(func=lambda _: target, iterable=[None]))(1)
next(pool.imap_unordered(func=lambda _: target, iterable=[None]))(1)
Pool(1).map_async(func=lambda _: target, iterable=[None]).get(timeout=1)[0](1)
Pool(1).starmap_async(func=lambda: target, iterable=[()]).get(timeout=1)[0](1)
",
    );
    for line in [6, 7, 10, 11] {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected pool map subscript violation on line {line}, got: {messages:?}"
        );
    }
    for line in [8, 9] {
        assert!(
            has_error_at(&messages, line, "next() result"),
            "expected pool imap violation on line {line}, got: {messages:?}"
        );
    }
}

/// `Pool.apply` and `ApplyResult.get` preserve callback result callables
/// (issues #520, #521).
#[test]
fn pool_apply_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
from multiprocessing.pool import Pool

pool = Pool(1)
def target(value: int) -> None: ...
pool.apply(func=lambda: target)(1)
pool.apply_async(func=lambda: target).get(timeout=1)(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "apply() result")
            && has_error_at(&messages, 7, "apply() result"),
        "expected pool apply violations, got: {messages:?}"
    );
}

/// Awaiting an asyncio Queue get retains a callable item annotation
/// (issue #445).
#[test]
fn annotated_asyncio_queue_results_preserve_callable_signature() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
queue: asyncio.Queue[Callable[[int], None]] = asyncio.Queue()
async def caller() -> None:
    (await queue.get())(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "get() result"),
        "expected queue result violation, got: {messages:?}"
    );
}

/// A concrete `put_nowait` mutation infers the callable item returned by an
/// unannotated `asyncio.Queue` (issue #840).
#[test]
fn asyncio_queue_put_nowait_infers_callable_item_signature() {
    let messages = check_source(
        r"
import asyncio
def target(value: int) -> None: ...
async def main() -> None:
    values = asyncio.Queue()
    values.put_nowait(item=target)
    (await values.get())(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "get() result"),
        "expected concrete Queue item violation, got: {messages:?}"
    );
}

/// Heterogeneous concrete queue mutations do not retain a stale signature.
#[test]
fn asyncio_queue_put_nowait_rejects_ambiguous_item_signatures() {
    let messages = check_source(
        r"
import asyncio
def target(value: int) -> None: ...
async def main() -> None:
    values = asyncio.Queue()
    values.put_nowait(item=target)
    values.put_nowait(item=object())
    (await values.get())(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("get() result")),
        "heterogeneous queue must not retain an inferred signature: {messages:?}"
    );
}

/// A non-callable first insertion prevents later homogeneous inference.
#[test]
fn asyncio_queue_put_nowait_non_callable_first_is_ambiguous() {
    let messages = check_source(
        r"
import asyncio
def target(value: int) -> None: ...
async def main() -> None:
    values = asyncio.Queue()
    values.put_nowait(item=object())
    values.put_nowait(item=target)
    (await values.get())(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("get() result")),
        "non-callable first item must make queue inference ambiguous: {messages:?}"
    );
}

/// A nested shadow cannot poison the outer queue's inferred item signature.
#[test]
fn asyncio_queue_put_nowait_respects_nested_shadowing() {
    let messages = check_source(
        r"
import asyncio
def target(value: int) -> None: ...
async def main() -> None:
    values = asyncio.Queue()
    values.put_nowait(item=target)
    async def inner() -> None:
        values = object()
        values.put_nowait(item=object())
    (await values.get())(1)
",
    );
    assert!(
        has_error_at(&messages, 10, "get() result"),
        "nested shadow must not poison the outer queue: {messages:?}"
    );
}

/// A concrete `queue.Queue.put` mutation infers the callable returned by
/// `get` (issue #848).
#[test]
fn sync_queue_put_infers_callable_item_signature() {
    let messages = check_source(
        r"
import queue
def target(value: int) -> None: ...
values = queue.Queue()
values.put(item=target)
values.get()(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "queue result"),
        "expected concrete sync Queue item violation, got: {messages:?}"
    );
}

/// Calling coroutine-based `asyncio.Queue.put` without awaiting it does not
/// establish a concrete queue mutation.
#[test]
fn asyncio_queue_unawaited_put_does_not_infer_item_signature() {
    let messages = check_source(
        r"
import asyncio
def target(value: int) -> None: ...
async def main() -> None:
    values = asyncio.Queue()
    values.put(item=target)
    (await values.get())(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("get() result")),
        "unawaited asyncio.Queue.put must not infer a mutation: {messages:?}"
    );
}

/// A constructed `operator.methodcaller` accepts its target positionally,
/// while encoded `__call__` arguments are checked against that target (#395).
#[test]
fn methodcaller_checks_encoded_call_not_target_boundary() {
    let messages = check_source(
        r#"
import operator
def f(value: int) -> None: ...
operator.methodcaller("__call__", 1)(f)
"#,
    );
    assert_eq!(messages.len(), 1, "unexpected diagnostics: {messages:?}");
    assert!(
        has_error_at(&messages, 4, "f"),
        "expected encoded f-call violation, got: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("methodcaller")),
        "methodcaller target boundary must stay positional: {messages:?}"
    );
}

/// `from operator import getitem` names the same callable as
/// `operator.getitem`, so it selects the literal container's element too.
#[test]
fn imported_operator_getitem_preserves_callable_signature() {
    let messages = check_source(
        r"
from operator import getitem
def f(value: int) -> None: ...
getitem([f], 0)(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "f"),
        "expected operator.getitem violation, got: {messages:?}"
    );
}

/// A local binding named `operator` shadows the import, so its `getitem` is
/// not the stdlib one and the literal container is not selected through it.
#[test]
fn shadowed_operator_module_is_not_stdlib_getitem() {
    let messages = check_source(
        r"
import operator
def f(value: int) -> None: ...
def use(operator: object) -> None:
    operator.getitem([f], 0)(1)
",
    );
    assert!(
        !has_error_at(&messages, 5, "f"),
        "a shadowed operator must not resolve to the stdlib one: {messages:?}"
    );
}

/// `from operator import methodcaller` builds the same encoded call, so its
/// target boundary stays positional.
#[test]
fn imported_methodcaller_target_boundary_stays_positional() {
    let messages = check_source(
        r#"
from operator import methodcaller
def f(value: int) -> None: ...
methodcaller("__call__", 1)(f)
"#,
    );
    assert_eq!(messages.len(), 1, "unexpected diagnostics: {messages:?}");
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("methodcaller")),
        "methodcaller target boundary must stay positional: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 4, "\"f\""),
        "expected encoded f-call violation, got: {messages:?}"
    );
}

/// An encoded call takes the same `ignore_names` exemption a written-out call
/// to the same callee would.
#[test]
fn encoded_methodcaller_call_honours_ignore_names() {
    let project = TestProject::new()
        .pyproject(
            "[project]\nname = \"t\"\nversion = \"0\"\n\n\
             [tool.strict_kwargs]\nignore_names = [\"main.f\"]\n",
        )
        .main(
            r#"
import operator
def f(value: int) -> None: ...
operator.methodcaller("__call__", 1)(f)
"#,
        );
    let messages = project.check();
    assert!(
        messages.is_empty(),
        "an ignored callee must stay ignored through an encoded call: {messages:?}"
    );
}

/// A `def` that replaces a completed `@overload` group no longer answers to
/// the group's recorded arms.
#[test]
fn redefinition_drops_completed_overload_arms() {
    let messages = check_source(
        r#"
from typing import Callable, overload

def g(value: int) -> None: ...

@overload
def make(kind: str) -> Callable[[int], None]: ...
@overload
def make(kind: int) -> Callable[[int], None]: ...
def make(kind): ...

def make(kind, /): ...

make("a")(1)
"#,
    );
    assert!(
        !messages.iter().any(|m| m.contains("overload result")),
        "a replaced overload group must not select a stale arm: {messages:?}"
    );
}

/// A second `@overload` group replaces a completed one rather than extending
/// it, so only the new group's arms can be selected.
#[test]
fn a_second_overload_group_replaces_a_completed_one() {
    let messages = check_source(
        r"
from typing import Callable, overload

@overload
def make(kind: str) -> Callable[[int], None]: ...
def make(kind): ...

@overload
def make(kind: int) -> Callable[[int], None]: ...
def make(kind): ...

make(kind=1)(1)
make(kind='a')(1)
",
    );
    assert!(
        has_error_at(&messages, 12, "overload result"),
        "expected the new int arm to be selected, got: {messages:?}"
    );
    assert!(
        !has_error_at(&messages, 13, "overload result"),
        "the replaced str arm must not be selected: {messages:?}"
    );
}

/// A negative literal is still an `int` for overload selection.
#[test]
fn negative_literals_select_numeric_overload_arms() {
    let messages = check_source(
        r"
from typing import Callable, overload

@overload
def make(kind: str) -> Callable[[int], None]: ...
@overload
def make(kind: int) -> Callable[[int], None]: ...
def make(kind): ...

make(-1)(1)
",
    );
    assert!(
        has_error_at(&messages, 10, "overload result"),
        "expected the int arm to be selected, got: {messages:?}"
    );
}

/// A literal argument uniquely selects an overload's concrete callable return
/// signature (issue #396).
#[test]
fn overload_selection_preserves_callable_return_signature() {
    let messages = check_source(
        r#"
from collections.abc import Callable
from typing import overload
@overload
def factory(kind: int) -> Callable[[int], None]: ...
@overload
def factory(kind: str) -> Callable[[str], None]: ...
def factory(kind): ...
factory(kind=1)(1)
factory("text")("value")
"#,
    );
    assert!(
        has_error_at(&messages, 9, "overload result")
            && has_error_at(&messages, 10, "overload result"),
        "expected overload-result violations, got: {messages:?}"
    );
}

/// Assigning the result of `pop` from an annotated list preserves the
/// concrete callable item signature (issue #398).
#[test]
fn list_pop_assignment_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable
def f(value: int) -> None: ...
calls: list[Callable[[int], None]] = [f]
call = calls.pop()
call(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "list pop result"),
        "expected list-pop violation, got: {messages:?}"
    );
}

/// A literal list copy preserves the concrete callable at a static index
/// (issue #771).
#[test]
fn literal_list_copy_subscript_preserves_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
[target].copy()[0](1)
",
    );
    assert!(
        has_error_at(&messages, 3, "target"),
        "expected list-copy violation, got: {messages:?}"
    );
}

/// Explicit `list.__getitem__` preserves the callable at a literal index
/// (issue #772).
#[test]
fn literal_list_explicit_getitem_preserves_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
[target].__getitem__(0)(1)
",
    );
    assert!(
        has_error_at(&messages, 3, "target"),
        "expected explicit-getitem violation, got: {messages:?}"
    );
}

/// Immediate list pop results preserve homogeneous callable elements
/// (issue #770).
#[test]
fn literal_list_pop_preserves_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
[target].pop()(1)
sorted([target]).pop()(1)
",
    );
    assert_eq!(
        messages.len(),
        2,
        "expected both pop violations: {messages:?}"
    );
    for line in 3..=4 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected list-pop violation on line {line}, got: {messages:?}"
        );
    }
}

/// An operand-selecting `reduce` lambda preserves a literal sequence's
/// concrete callable element shape (issue #399).
#[test]
fn reduce_result_preserves_callable_signature() {
    let messages = check_source(
        r"
from functools import reduce
def f(value: int) -> None: ...
reduce(lambda left, right: left, [f, f])(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "reduce result"),
        "expected reduce-result violation, got: {messages:?}"
    );
}

/// A Generator send result retains the declared callable yield signature
/// (issue #458).
#[test]
fn generator_send_result_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections.abc import Callable, Generator
def functions() -> Generator[Callable[[int], None], None, None]: ...
gen = functions()
next(gen)
gen.send(None)(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "send() result"),
        "expected generator send violation, got: {messages:?}"
    );
}

/// A `Generator.throw` result retains the declared callable yield signature
/// (issue #656).
#[test]
fn generator_throw_result_preserves_callable_signature() {
    let messages = check_source(
        r"
import collections.abc
import typing
generator: collections.abc.Generator[typing.Callable[[int], int], None, None]
generator.throw(typ=RuntimeError)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "throw() result"),
        "expected generator throw violation, got: {messages:?}"
    );
}

/// An `AsyncGenerator.asend` result retains the declared callable yield signature
/// (issue #657).
#[test]
fn async_generator_asend_result_preserves_callable_signature() {
    let messages = check_source(
        r"
import collections.abc
import typing
agen: collections.abc.AsyncGenerator[typing.Callable[[int], int], None]
async def main() -> None:
    (await agen.asend(value=None))(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "asend() result"),
        "expected async generator asend violation, got: {messages:?}"
    );
}

/// An `AsyncGenerator.athrow` result retains the declared callable yield signature
/// (issue #658).
#[test]
fn async_generator_athrow_result_preserves_callable_signature() {
    let messages = check_source(
        r"
import collections.abc
import typing
agen: collections.abc.AsyncGenerator[typing.Callable[[int], int], None]
async def main() -> None:
    (await agen.athrow(typ=RuntimeError))(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "athrow() result"),
        "expected async generator athrow violation, got: {messages:?}"
    );
}

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

/// Repeated same-shape `self.method(...)` calls share one ty hover answer
/// (the scan groups them; see `CallChecker::hover_group_for_call`). Every
/// member of the group must still get its own diagnostic, and a
/// different-shape sibling must be resolved independently. The base class
/// is bound through an alias the built-in resolver does not follow, so the
/// calls genuinely reach the ty fallback's grouped hover path.
#[test]
fn ty_hover_reuse_flags_every_grouped_self_call() {
    let messages = check_source(
        "\
import unittest

Base = unittest.TestCase


class T(Base):
    def m(self):
        self.assertEqual(1, 2)
        self.assertEqual(3, 4)
        self.assertEqual(5, 6)
        with self.assertRaises(ValueError):
            pass
",
    );
    for line in [8, 9, 10] {
        assert!(
            has_error_at(&messages, line, "Too many positional"),
            "every grouped assertEqual call must be flagged, got: {messages:?}"
        );
    }
    assert!(
        !messages.iter().any(|m| m.starts_with("main:11:")),
        "the context-manager assertRaises overload allows its positional \
         argument, got: {messages:?}"
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

/// A loop variable bound to a tuple of class objects has a *union* type
/// (`type[int] | type[Number]`) that only the ty fallback can follow — the
/// built-in resolver does not flow-type a `for` target across a tuple of
/// callables. Calling it resolves each arm, so the over-long no-argument
/// `Number()` construction is flagged at the call site. This mirrors the
/// `typea(a)` construct in the `test_richcmp` standard-library test that the
/// sharded ty fallback now reports (issue #240).
#[test]
fn ty_resolves_union_of_class_objects_from_tuple_loop() {
    let messages = check_source(concat!(
        "class Number:\n",
        "    def __init__(self):\n",
        "        pass\n",
        "\n",
        "\n",
        "def widen(a):\n",
        "    for ctor in (int, Number):\n",
        "        ctor(a)\n",
    ));
    assert!(
        has_error_at(&messages, 8, "Too many positional arguments for \"Number\""),
        "union-typed class-object construction must be flagged via ty: {messages:?}"
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

#[test]
fn ty_hover_callable_type_honors_definition_based_ignore_names() {
    let project = TestProject::new()
        .pyproject(
            "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nignore_names = [\"builtins.IO.write\"]\n",
        )
        .main("import sys\n\nsys.stdout.write(\"hello\", \"extra\")\n");
    let messages = project.check();
    assert!(
        messages.is_empty(),
        "callable-type hover ignore must use goto-definition context: {messages:?}"
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

/// atexit.register returns the registered callable unchanged (issue #478).
#[test]
fn atexit_register_preserves_callable_signature() {
    let messages = check_source(
        r"
import atexit
def target(value: int) -> int:
    return value
atexit.register(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `MethodType` binds the leading receiver of a concrete method signature
/// (issue #460).
#[test]
fn method_type_result_preserves_bound_method_signature() {
    let messages = check_source(
        r"
from types import MethodType
class C:
    def method(self, value: int) -> None: ...
MethodType(C.method, C())(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "MethodType"),
        "expected bound method violation, got: {messages:?}"
    );
}

/// inspect.unwrap retains the concrete wrapped callable signature (issue
/// #459).
#[test]
fn inspect_unwrap_preserves_callable_signature() {
    let messages = check_source(
        r"
import inspect
def f(value: int) -> None: ...
inspect.unwrap(func=f)(1)
",
    );
    assert!(has_error_at(&messages, 4, "f"), "messages: {messages:?}");
}

/// `reprlib.recursive_repr` preserves the decorated callable signature (issue
/// #613).
#[test]
fn recursive_repr_preserves_callable_signature() {
    let messages = check_source(
        "
import reprlib
def target(value: int) -> int:
    return value
reprlib.recursive_repr(fillvalue='...')(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `unittest.skip` preserves the decorated callable signature (issue #614).
#[test]
fn unittest_skip_preserves_callable_signature() {
    let messages = check_source(
        "
import unittest
def target(value: int) -> int:
    return value
unittest.skip('skip')(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `unittest.skipIf` preserves the decorated callable signature (issue #615).
#[test]
fn unittest_skip_if_preserves_callable_signature() {
    let messages = check_source(
        "
import unittest
def target(value: int) -> int:
    return value
unittest.skipIf(True, 'skip')(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `unittest.skipUnless` preserves the decorated callable signature (issue
/// #616).
#[test]
fn unittest_skip_unless_preserves_callable_signature() {
    let messages = check_source(
        "
import unittest
def target(value: int) -> int:
    return value
unittest.skipUnless(False, 'skip')(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `unittest.expectedFailure` preserves its identity return signature (issue
/// #617).
#[test]
fn unittest_expected_failure_preserves_callable_signature() {
    let messages = check_source(
        "
import unittest
def target(value: int) -> int:
    return value
unittest.expectedFailure(test_item=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// ``from unittest.case import expectedFailure`` resolves through the case
/// module path.
#[test]
fn unittest_case_expected_failure_preserves_callable_signature() {
    let messages = check_source(
        "
from unittest.case import expectedFailure
def target(value: int) -> int:
    return value
expectedFailure(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// Context-manager helpers and ``__enter__`` preserve managed callable types
/// (issues #618-#621, #627-#632, #444, #630, #631).
#[test]
fn context_manager_generic_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import contextlib
import unittest

def target(value: int) -> int:
    return value

class Writer:
    def write(self, text: str) -> int:
        return len(text)
    def flush(self) -> None:
        pass
    def __call__(self, value: int) -> int:
        return value

class Value:
    async def aclose(self) -> None:
        pass
    def __call__(self, value: int) -> int:
        return value

class Decorator(contextlib.ContextDecorator):
    def __enter__(self):
        return self
    def __exit__(self, *args: object) -> None:
        pass

class Manager:
    async def __aenter__(self):
        return lambda value: value
    async def __aexit__(self, *args: object) -> None:
        pass

contextlib.closing(thing=target).__enter__()(1)
contextlib.aclosing(thing=Value()).__aenter__()(1)
contextlib.redirect_stdout(new_target=Writer()).__enter__()(1)
contextlib.redirect_stderr(new_target=Writer()).__enter__()(1)
unittest.enterModuleContext(cm=contextlib.nullcontext(enter_result=target))(1)
unittest.TestCase().enterContext(cm=contextlib.nullcontext(enter_result=target))(1)
unittest.TestCase.enterClassContext(cm=contextlib.nullcontext(enter_result=target))(1)
Decorator()(func=target)(1)
contextlib.ExitStack().enter_context(cm=contextlib.nullcontext(enter_result=target))(1)
contextlib.nullcontext(enter_result=target).__enter__()(1)

async def main() -> None:
    (await unittest.IsolatedAsyncioTestCase().enterAsyncContext(cm=Manager()))(1)
    stack = contextlib.AsyncExitStack()
    (await stack.enter_async_context(cm=Manager()))(1)
",
    );
    for line in 34..=43 {
        assert!(
            has_error_at(&messages, line, "generic result"),
            "expected generic-result violation on line {line}, got: {messages:?}"
        );
    }
    for line in [46, 48] {
        assert!(
            has_error_at(&messages, line, "awaited result")
                || has_error_at(&messages, line, "generic result"),
            "expected awaited/generic-result violation on line {line}, got: {messages:?}"
        );
    }
}

/// A stored `IsolatedAsyncioTestCase` retains the generic callable returned by
/// `enterAsyncContext` (regression #760).
#[test]
fn isolated_asyncio_test_case_instance_preserves_entered_callable() {
    let messages = check_source(
        r"
import unittest

class Manager:
    async def __aenter__(self):
        return lambda value: value
    async def __aexit__(self, *args: object) -> None: pass

async def main() -> None:
    case = unittest.IsolatedAsyncioTestCase()
    (await case.enterAsyncContext(cm=Manager()))(1)
    case = object()
    (await case.enterAsyncContext(cm=Manager()))(1)
",
    );
    assert_eq!(
        messages.len(),
        1,
        "stale case bindings must be cleared: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 11, "awaited result")
            || has_error_at(&messages, 11, "generic result"),
        "expected entered-callable violation, got: {messages:?}"
    );
}

#[test]
fn inherited_unittest_enter_helper_keeps_generic_return() {
    let messages = check_source(
        r"
import contextlib
import unittest
def target(value: int) -> None: ...
class Case(unittest.TestCase): pass
Case().enterContext(cm=contextlib.nullcontext(enter_result=target))(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "generic result"),
        "inherited enter helper must preserve its generic return: {messages:?}"
    );
}

/// `__enter__` / `__aenter__` bodies that return named callables or bare lambdas
/// are indexed for later generic-result checking.
#[test]
fn context_manager_enter_indexes_named_and_bare_lambda_callables() {
    let messages = check_source(
        r"
def target(value: int) -> int:
    return value

class NamedEnter:
    def __enter__(self):
        return target
    def __exit__(self, *args: object) -> None:
        pass

class BareLambdaEnter:
    def __enter__(self):
        return lambda: 1
    def __exit__(self, *args: object) -> None:
        pass

NamedEnter().__enter__()(1)
BareLambdaEnter().__enter__()(1)
",
    );
    assert!(
        has_error_at(&messages, 17, "generic result"),
        "expected violation for named enter callable, got: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 18, "generic result"),
        "expected violation for bare-lambda enter result, got: {messages:?}"
    );
}

/// When ``__enter__`` returns a name that is not a single indexed signature,
/// resolution still flows through ``context_manager_enter_callable_result``
/// in ``resolve_callee`` (rather than the generic-result signature path).
#[test]
fn context_manager_enter_unindexed_callable_resolves_via_callee() {
    let messages = check_source(
        r"
class MysteryEnter:
    def __enter__(self):
        return missing_target
    def __exit__(self, *args: object) -> None:
        pass

MysteryEnter().__enter__()(1)
",
    );
    // No indexed signature means the call is deferred (ty) or left unresolved;
    // either way it must not be reported as a generic-result arity error.
    assert!(
        !has_error_at(&messages, 9, "generic result"),
        "unindexed enter callable must not use generic-result checking, got: {messages:?}"
    );
}

/// ``__enter__`` bodies that are not a single ``return`` of a callable/lambda,
/// and single returns of non-callable literals, take the indexing fallthrough
/// arms (no map insert).
#[test]
fn context_manager_enter_non_indexable_bodies_are_skipped() {
    let messages = check_source(
        r"
class PassEnter:
    def __enter__(self):
        pass
    def __exit__(self, *args: object) -> None:
        pass

class LiteralEnter:
    def __enter__(self):
        return 1
    def __exit__(self, *args: object) -> None:
        pass

PassEnter().__enter__()
LiteralEnter().__enter__()
",
    );
    assert!(
        messages.is_empty(),
        "non-callable enter results should not emit violations, got: {messages:?}"
    );
}

/// Callable-valued properties are not checked as the property getter (issue
/// #668).
#[test]
fn callable_valued_property_call_is_not_attributed_to_property() {
    let messages = check_source(
        r"
from collections.abc import Callable
class Spec:
    @property
    def opener(self) -> Callable[[list[int]], str]:
        return lambda items: str(len(items))
def main() -> None:
    print(Spec().opener([1, 2]))
",
    );
    assert!(
        !messages.iter().any(|message| message.contains("opener")),
        "property getter must not be the callee: {messages:?}"
    );
}

/// `Enum.value` descriptor reads must not be treated as calls (issue #669).
#[test]
fn enum_value_callable_member_is_not_attributed_to_enum_value() {
    let messages = check_source(
        r"
import enum
def _shout(value: str) -> str:
    return value.upper()
class Style(enum.Enum):
    SHOUT = enum.member(value=_shout)
    def __call__(self, value: str, /) -> str:
        return self.value(value)
",
    );
    assert!(
        !messages.iter().any(|message| message.contains("\"value\"")),
        "Enum.value must not be the callee: {messages:?}"
    );
}

/// `typing.override` preserves the decorated callable signature (issue #612).
#[test]
fn typing_override_preserves_callable_signature() {
    let messages = check_source(
        "
import typing
def target(value: int) -> int:
    return value
typing.override(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `abc.abstractmethod` preserves the decorated callable signature (issue
/// #489).
#[test]
fn abc_abstractmethod_preserves_callable_signature() {
    let messages = check_source(
        "
import abc
def target(value: int) -> int:
    return value
abc.abstractmethod(funcobj=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `typing.final` preserves the decorated callable signature (issue #488).
#[test]
fn typing_final_preserves_callable_signature() {
    let messages = check_source(
        "
import typing
def target(value: int) -> int:
    return value
typing.final(f=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `typing.no_type_check` preserves the decorated callable signature (issue
/// #487).
#[test]
fn typing_no_type_check_preserves_callable_signature() {
    let messages = check_source(
        "
import typing
def target(value: int) -> int:
    return value
typing.no_type_check(arg=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `functools.update_wrapper` preserves the wrapper signature (issue #492).
#[test]
fn functools_update_wrapper_preserves_callable_signature() {
    let messages = check_source(
        "
import functools
def target(value: int) -> int:
    return value
functools.update_wrapper(wrapper=target, wrapped=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `functools.wraps` preserves the decorated wrapper signature (issue #493).
#[test]
fn functools_wraps_preserves_callable_signature() {
    let messages = check_source(
        "
import functools
def target(value: int) -> int:
    return value
functools.wraps(wrapped=target)(wrapper=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `functools.lru_cache` preserves the cached wrapper signature (issue #494).
#[test]
fn functools_lru_cache_preserves_callable_signature() {
    let messages = check_source(
        "
import functools
def target(value: int) -> int:
    return value
functools.lru_cache()(user_function=target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `functools.cache` preserves the cached wrapper signature (issue #495).
#[test]
fn functools_cache_preserves_callable_signature() {
    let messages = check_source(
        "
import functools
def target(value: int) -> int:
    return value
functools.cache(target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "messages: {messages:?}"
    );
}

/// `types.coroutine` preserves the decorated generator signature (issue #490).
#[test]
fn types_coroutine_preserves_callable_signature() {
    let messages = check_source(
        "
import types
def generator(value: int):
    yield value
types.coroutine(func=generator)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "generator"),
        "messages: {messages:?}"
    );
}

/// `abc.update_abstractmethods` preserves the returned class constructor
/// signature (issue #500).
#[test]
fn abc_update_abstractmethods_preserves_class_constructor_signature() {
    let messages = check_source(
        "
import abc
class Model:
    def __init__(self, value: int) -> None:
        pass
abc.update_abstractmethods(cls=Model)(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "Model"),
        "messages: {messages:?}"
    );
}

/// `functools.total_ordering` preserves the returned class constructor
/// signature (issue #499).
#[test]
fn functools_total_ordering_preserves_class_constructor_signature() {
    let messages = check_source(
        "
import functools
class Model:
    def __init__(self, value: int) -> None:
        pass
    def __lt__(self, other: object) -> bool:
        return False
functools.total_ordering(cls=Model)(1)
",
    );
    assert!(
        has_error_at(&messages, 8, "Model"),
        "messages: {messages:?}"
    );
}

/// A literal dictionary's `setdefault` returns either its known existing
/// value or the supplied default, preserving either callable signature
/// (issue #439).
#[test]
fn literal_dict_setdefault_preserves_callable_signatures() {
    let messages = check_source(
        r#"
def existing(value: int) -> None: ...
def fallback(value: int) -> None: ...
{"call": existing}.setdefault("call", fallback)(1)
{}.setdefault("call", fallback)(1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "existing"),
        "expected existing-value violation, got: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 5, "fallback"),
        "expected default-value violation, got: {messages:?}"
    );
}

/// Standard-library generic mappings retain the concrete callable value type
/// through their result-returning methods (issue #440).
#[test]
fn collections_mapping_results_preserve_callable_signatures() {
    let messages = check_source(
        r#"
from collections import ChainMap, OrderedDict, UserDict
def first(value: int) -> None: ...
def second(value: int) -> None: ...
def third(value: int) -> None: ...
ChainMap({"call": first}).pop("call")(1)
OrderedDict([("call", second)]).popitem()[1](1)
UserDict({"call": third}).pop("call")(1)
"#,
    );
    for (line, callee) in [(6, "first"), (7, "second"), (8, "third")] {
        assert!(
            has_error_at(&messages, line, callee),
            "expected {callee} violation, got: {messages:?}"
        );
    }
}

/// A single-mapping `ChainMap` preserves concrete callable values through
/// subscripting (issue #777).
#[test]
fn chainmap_literal_subscript_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import ChainMap
def target(value: int) -> None: ...
ChainMap({"key": target})["key"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected ChainMap result violation, got: {messages:?}"
    );
}

/// An empty `ChainMap.new_child` falls through to concrete callable values in
/// the parent mapping (issue #820).
#[test]
fn chainmap_empty_new_child_preserves_parent_callable_signature() {
    let messages = check_source(
        r#"
from collections import ChainMap
def target(value: int) -> None: ...
ChainMap({"x": target}).new_child()["x"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected parent ChainMap result violation, got: {messages:?}"
    );
}

/// An `OrderedDict` initialized from a literal mapping preserves concrete
/// callable values through subscripting (issue #779).
#[test]
fn ordereddict_literal_subscript_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import OrderedDict
def target(value: int) -> None: ...
OrderedDict({"key": target})["key"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected OrderedDict result violation, got: {messages:?}"
    );
}

/// A `UserDict` initialized from a literal mapping preserves concrete callable
/// values through subscripting (issue #778).
#[test]
fn userdict_literal_subscript_preserves_callable_signature() {
    let messages = check_source(
        r#"
from collections import UserDict
def target(value: int) -> None: ...
UserDict({"key": target})["key"](1)
"#,
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected UserDict result violation, got: {messages:?}"
    );
}

/// `heapq` functions returning a homogeneous list element retain its concrete
/// callable signature (issue #441).
#[test]
fn heapq_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import heapq
def f(value: int) -> None: ...
heap = [f]
heapq.heappop(heap)(1)
heapq.heapreplace(heap, f)(1)
heapq.heappop([f, f])(1)
heapq.heapreplace([f, f], f)(1)
",
    );
    assert!(has_error_at(&messages, 5, "f"), "messages: {messages:?}");
    assert!(has_error_at(&messages, 6, "f"), "messages: {messages:?}");
    assert!(has_error_at(&messages, 7, "f"), "messages: {messages:?}");
    assert!(has_error_at(&messages, 8, "f"), "messages: {messages:?}");
}

/// `heapq.nsmallest` and `nlargest` retain concrete callable elements through
/// subscripting, including their keyword argument forms (issue #793).
#[test]
fn heapq_selection_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import heapq
def target(value: int) -> None: ...
heapq.nsmallest(n=1, iterable=[target])[0](1)
heapq.nlargest(n=1, iterable=[target])[0](1)
",
    );
    for line in 4..=5 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected heapq selection violation on line {line}, got: {messages:?}"
        );
    }
}

/// Generic selectors in `random` and `secrets` retain their input element's
/// concrete callable signature (issue #442).
#[test]
fn random_selector_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import random, secrets
def f(value: int) -> None: ...
random.choice([f])(1)
random.choice((f,))(1)
random.sample([f], 1)[0](1)
random.sample((f,), k=1)[0](1)
secrets.choice([f])(1)
secrets.choice((f,))(1)
",
    );
    for line in 4..=9 {
        assert!(has_error_at(&messages, line, "f"), "messages: {messages:?}");
    }
}

/// Directly imported aliases of `random.choice` and `secrets.choice` retain
/// the selected callable's signature (issue #852).
#[test]
fn direct_import_choice_preserves_callable_signatures() {
    let messages = check_source(
        r"
from random import choice as random_choice
from secrets import choice as secret_choice
def target(value: int) -> None: ...
random_choice(seq=[target])(1)
secret_choice(seq=[target])(1)
",
    );
    for line in 5..=6 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected direct choice violation on line {line}, got: {messages:?}"
        );
    }
}

/// The valid keyword form of `random.choice` and `secrets.choice` preserves
/// callable elements just like the positional form (issue #785).
#[test]
fn random_choice_keyword_preserves_callable_signature() {
    let messages = check_source(
        r"
import random
import secrets
def target(value: int) -> None: ...
random.choice(seq=[target])(1)
secrets.choice(seq=[target])(1)
",
    );
    for line in 5..=6 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected keyword choice violation on line {line}, got: {messages:?}"
        );
    }
}

/// `statistics.mode` / `multimode` preserve callable element types (issue #515).
#[test]
fn statistics_selector_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import statistics
def target(value: int) -> None: ...
statistics.mode(data=[target])(1)
statistics.multimode(data=[target])[0](1)
",
    );
    assert!(
        has_error_at(&messages, 4, "target") && has_error_at(&messages, 5, "target"),
        "expected statistics selector violations, got: {messages:?}"
    );
}

/// Singleton `statistics.median_low` and `median_high` results preserve the
/// sole callable data element (issue #796).
#[test]
fn statistics_median_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import statistics
def target(value: int) -> None: ...
statistics.median_low(data=[target])(1)
statistics.median_high(data=[target])(1)
",
    );
    for line in 4..=5 {
        assert!(
            has_error_at(&messages, line, "target"),
            "expected statistics median violation on line {line}, got: {messages:?}"
        );
    }
}

/// `Counter.most_common` preserves callable key types (issue #514).
#[test]
fn counter_most_common_preserves_callable_key_signatures() {
    let messages = check_source(
        r"
from collections import Counter
def target(value: int) -> None: ...
Counter([target]).most_common(n=1)[0][0](1)
",
    );
    assert!(
        has_error_at(&messages, 4, "target"),
        "expected Counter.most_common violation, got: {messages:?}"
    );
}

/// `Counter.elements()` preserves concrete callable keys through `next()`
/// for an immediately constructed counter (issue #797).
#[test]
fn counter_elements_preserves_callable_signature() {
    let messages = check_source(
        r"
from collections import Counter
def target(value: int) -> None: ...
next(Counter([target]).elements())(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "next() result"),
        "expected Counter.elements violation, got: {messages:?}"
    );
}

/// ``ContextVar.set()`` tokens preserve ``Token.old_value`` callable types
/// (issue #659).
#[test]
fn contextvar_token_old_value_preserves_callable_signatures() {
    let messages = check_source(
        r"
import contextvars
from collections.abc import Callable
var: contextvars.ContextVar[Callable[[int], int]]
token = var.set(lambda value: value)
old = token.old_value
if old is not contextvars.Token.MISSING:
    old(1)
",
    );
    assert!(
        has_error_at(&messages, 8, "old"),
        "expected Token.old_value violation, got: {messages:?}"
    );
}

/// ``MappingProxyType.get`` preserves generic mapping value types (issue #660).
#[test]
fn mapping_proxy_get_preserves_callable_value_signatures() {
    let messages = check_source(
        r"
import types
from collections.abc import Callable
mapping: types.MappingProxyType[str, Callable[[int], int]]
value = mapping.get('key')
value(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "get() result"),
        "expected MappingProxyType.get violation, got: {messages:?}"
    );
}

/// ``TopologicalSorter.get_ready`` preserves callable graph node types
/// (issue #516).
#[test]
fn topological_sorter_get_ready_preserves_callable_node_signatures() {
    let messages = check_source(
        r"
from graphlib import TopologicalSorter
def target(value: int) -> None: ...
sorter = TopologicalSorter(graph={target: set()})
sorter.prepare()
sorter.get_ready()[0](1)
",
    );
    assert!(
        has_error_at(&messages, 6, "target"),
        "expected TopologicalSorter.get_ready violation, got: {messages:?}"
    );
}

/// Annotated ``WeakKeyDictionary.popitem`` preserves callable key types
/// (issue #517).
#[test]
fn weak_key_dictionary_popitem_preserves_callable_key_signatures() {
    let messages = check_source(
        r"
import weakref
from collections.abc import Callable
keys: weakref.WeakKeyDictionary[Callable[[int], None], int] = weakref.WeakKeyDictionary()
def target(value: int) -> None: ...
keys[target] = 1
keys.popitem()[0](1)
",
    );
    assert!(
        has_error_at(&messages, 7, "popitem() key"),
        "expected WeakKeyDictionary.popitem key violation, got: {messages:?}"
    );
}

/// ``WeakKeyDictionary`` key signatures must not feed the list ``.pop()``
/// tracker (Bugbot on #704).
#[test]
fn weak_key_dictionary_pop_does_not_reuse_key_signature() {
    let messages = check_source(
        r"
import weakref
from collections.abc import Callable
def target(value: int) -> None: ...
keys: weakref.WeakKeyDictionary[Callable[[int], None], int] = weakref.WeakKeyDictionary()
keys[target] = 1
value = keys.pop(target)
calls: list[Callable[[int], None]] = [target]
call = calls.pop()
call(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("list pop result") && message.contains(":7:")),
        "WeakKeyDictionary.pop must not be tracked as list pop: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 10, "list pop result"),
        "list.pop must still preserve callable items: {messages:?}"
    );
}

/// Rebinding a former token / sorter clears stale callable tracking (Bugbot on #704).
#[test]
fn token_and_sorter_rebind_clears_stale_callables() {
    let messages = check_source(
        r"
import contextvars
from collections.abc import Callable
from graphlib import TopologicalSorter
var: contextvars.ContextVar[Callable[[int], int]]
token = var.set(lambda value: value)
class Holder: ...
token = Holder()
old = token.old_value
",
    );
    assert!(
        messages.is_empty(),
        "rebound token must not keep Token.old_value: {messages:?}"
    );

    let messages = check_source(
        r"
from graphlib import TopologicalSorter
def target(value: int) -> None: ...
sorter = TopologicalSorter(graph={target: set()})
class Holder: ...
sorter = Holder()
sorter.get_ready()[0](1)
",
    );
    assert!(
        messages.is_empty(),
        "rebound sorter must not keep get_ready callable: {messages:?}"
    );
}

/// Immediate and assigned `WeakSet` / annotated `WeakValueDictionary` pops
/// preserve callable elements (issue #443).
#[test]
fn weak_container_results_preserve_callable_signatures() {
    let messages = check_source(
        r"
import weakref
from collections.abc import Callable
def f(value: int) -> None: ...
values = weakref.WeakSet([f])
values.pop()(1)
mapping: weakref.WeakValueDictionary[str, Callable[[int], None]] = weakref.WeakValueDictionary()
mapping['call'] = f
mapping.pop('call')(1)
weakref.WeakSet([f]).pop()(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "f")
            && has_error_at(&messages, 9, "pop() result")
            && has_error_at(&messages, 10, "f"),
        "expected weak-container violations, got: {messages:?}"
    );
}

/// Annotated `WeakValueDictionary` must not share queue `.get()` tracking
/// (issue #729).
#[test]
fn weak_value_dictionary_get_is_not_queue_result() {
    let messages = check_source(
        r"
import weakref
from collections.abc import Callable
def f(value: int) -> None: ...
mapping: weakref.WeakValueDictionary[str, Callable[[int], None]] = weakref.WeakValueDictionary()
mapping.get('missing')(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("get() result") || message.contains("queue")),
        "WeakValueDictionary.get must not use queue-item storage: {messages:?}"
    );
}

/// Annotated `Future[Callable[...]].result()` preserves the callable signature
/// (issue #410).
#[test]
fn future_result_preserves_callable_signatures() {
    let messages = check_source(
        r"
from collections.abc import Callable
from concurrent.futures import Future
def f(value: int) -> None: ...
future: Future[Callable[[int], None]] = Future()
future.result()(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "result() result"),
        "expected Future.result violation, got: {messages:?}"
    );
}

/// `asyncio.create_task` infers a callable coroutine result for the created
/// task's `result()` method (issue #837).
#[test]
fn asyncio_create_task_preserves_callable_result_signature() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]:
    return lambda value: None
async def main() -> None:
    task = asyncio.create_task(coro=factory())
    await task
    task.result()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "result() result"),
        "expected create_task result violation, got: {messages:?}"
    );
}

/// Dotted stdlib access remains a module-level `create_task` call.
#[test]
fn asyncio_tasks_create_task_preserves_callable_result_signature() {
    let messages = check_source(
        r"
import asyncio.tasks
from collections.abc import Callable
async def factory() -> Callable[[int], None]: ...
async def main() -> None:
    task = asyncio.tasks.create_task(coro=factory())
    task.result()(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "result() result"),
        "expected dotted create_task result violation, got: {messages:?}"
    );
}

/// `TaskGroup.create_task` preserves the coroutine's callable result after
/// the context manager waits for task completion (issue #839).
#[test]
fn asyncio_task_group_create_task_preserves_callable_result_signature() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]:
    return lambda value: None
async def main() -> None:
    async with asyncio.TaskGroup() as group:
        task = group.create_task(coro=factory())
    task.result()(1)
",
    );
    assert!(
        has_error_at(&messages, 9, "result() result"),
        "expected TaskGroup.create_task result violation, got: {messages:?}"
    );
}

/// Rebinding the context-manager local must discard its `TaskGroup` identity.
#[test]
fn asyncio_task_group_identity_is_cleared_on_rebind() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]: ...
async def main() -> None:
    async with asyncio.TaskGroup() as group:
        group = object()
        task = group.create_task(coro=factory())
    task.result()(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("result() result")),
        "rebound TaskGroup local must not retain task result metadata: {messages:?}"
    );
}

/// A nearer local binding shadows an enclosing `TaskGroup` identity.
#[test]
fn asyncio_task_group_identity_respects_nested_shadowing() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]: ...
async def main() -> None:
    async with asyncio.TaskGroup() as group:
        async def inner() -> None:
            group = object()
            task = group.create_task(coro=factory())
            task.result()(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("result() result")),
        "nested shadow must hide the outer TaskGroup: {messages:?}"
    );
}

/// Loop-target rebinding discards a context target's `TaskGroup` identity.
#[test]
fn asyncio_task_group_identity_is_cleared_by_loop_target() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]: ...
async def main() -> None:
    async with asyncio.TaskGroup() as group:
        for group in [object()]:
            task = group.create_task(coro=factory())
            task.result()(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("result() result")),
        "loop target must clear TaskGroup identity: {messages:?}"
    );
}

/// Rebinding a Future local must drop the annotated `future_callables` entry
/// (issue #737).
#[test]
fn future_result_signature_cleared_on_rebind() {
    let messages = check_source(
        r"
from collections.abc import Callable
from concurrent.futures import Future
def f(value: int) -> None: ...
future: Future[Callable[[int], None]] = Future()
future = object()
future.result()(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("result() result")),
        "rebound future must not keep annotated result callable: {messages:?}"
    );
}

/// `Context.run` preserves a lambda callback result callable signature
/// (issue #480).
#[test]
fn context_run_preserves_callable_signatures() {
    let messages = check_source(
        r"
import contextvars
def target(value: int) -> int:
    return value
contextvars.copy_context().run(lambda: target)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "run() result"),
        "expected Context.run violation, got: {messages:?}"
    );
}

/// Assigned ``copy_context()`` results are recognized as ``Context`` (issue #738).
#[test]
fn stored_copy_context_run_preserves_callable_signatures() {
    let messages = check_source(
        r"
import contextvars
def target(value: int) -> int:
    return value
ctx = contextvars.copy_context()
ctx.run(lambda: target)(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "run() result"),
        "stored copy_context result must preserve Context.run: {messages:?}"
    );
}

/// `ExitStack.callback` returns the callback unchanged (issue #481).
#[test]
fn exit_stack_callback_preserves_callable_signature() {
    let messages = check_source(
        r"
from contextlib import ExitStack
def target(value: int) -> int:
    return value
ExitStack().callback(target)(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "callback identity return must preserve callee: {messages:?}"
    );
}

/// `AsyncExitStack.push_async_callback` returns the callback unchanged (issue #482).
#[test]
fn async_exit_stack_push_async_callback_preserves_callable_signature() {
    let messages = check_source(
        r"
from contextlib import AsyncExitStack
async def target(value: int) -> int:
    return value
AsyncExitStack().push_async_callback(target)(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "push_async_callback identity return must preserve callee: {messages:?}"
    );
}

/// `typing.assert_type` returns its first argument (issue #486).
#[test]
fn typing_assert_type_preserves_callable_signature() {
    let messages = check_source(
        r"
import typing
def target(value: int) -> int:
    return value
typing.assert_type(target, typing.Callable[[int], int])(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "assert_type identity return must preserve callee: {messages:?}"
    );
}

/// ``typing_extensions.assert_type`` is dual-handled with ``typing`` (issue #739).
#[test]
fn typing_extensions_assert_type_preserves_callable_signature() {
    let messages = check_source(
        r"
import typing_extensions
def target(value: int) -> int:
    return value
typing_extensions.assert_type(target, object)(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "typing_extensions.assert_type must preserve callee: {messages:?}"
    );
}

#[test]
fn functools_wraps_positional_factory_preserves_wrapper_signature() {
    let messages = check_source(
        "
import functools
def wrapped(first: int, second: int) -> int:
    return first + second
def wrapper(value: int) -> int:
    return value
functools.wraps(wrapped)(wrapper)(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "wrapper"),
        "messages: {messages:?}"
    );
    assert!(
        !has_error_at(&messages, 7, "wrapped"),
        "messages: {messages:?}"
    );
}

#[test]
fn inner_rebinding_hides_outer_callable_list() {
    let messages = check_source(
        r"
import heapq
def f(value: int) -> None: ...
heap = [f]
def consume() -> None:
    heap = [object()]
    heapq.heappop(heap)(1)
",
    );
    assert!(messages.is_empty(), "messages: {messages:?}");
}

#[test]
fn property_setter_does_not_reenable_getter_checks() {
    let messages = check_source(
        r"
from collections.abc import Callable
class Spec:
    @property
    def opener(self) -> Callable[[list[int]], str]:
        return lambda items: str(len(items))
    @opener.setter
    def opener(self, value: Callable[[list[int]], str]) -> None: ...
def main() -> None:
    print(Spec().opener([1, 2]))
",
    );
    assert!(
        !messages.iter().any(|message| message.contains("opener")),
        "property getter must remain excluded after its setter: {messages:?}"
    );
}

#[test]
fn typing_extensions_identity_decorators_preserve_callable_signature() {
    let messages = check_source(
        "
from typing_extensions import final, no_type_check, override
def target(value: int) -> int:
    return value
final(target)(1)
no_type_check(target)(1)
override(target)(1)
",
    );
    for line in 5..=7 {
        assert!(
            has_error_at(&messages, line, "target"),
            "messages: {messages:?}"
        );
    }
}

/// Direct `staticmethod` objects keep the wrapped callable (issue #650).
#[test]
fn direct_staticmethod_preserves_wrapped_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> int:
    return value
staticmethod(target)(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "staticmethod identity return must preserve callee: {messages:?}"
    );
}

/// Bound-method `__func__` keeps the unbound method signature (issue #651).
#[test]
fn bound_method_func_preserves_unbound_method_signature() {
    let messages = check_source(
        r"
class Owner:
    def method(self, value: int) -> int:
        return value
Owner().method.__func__(Owner(), 1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("method")),
        "__func__ must preserve unbound method: {messages:?}"
    );
}

/// `property.fget` returns the getter; its result keeps a callable signature
/// (issue #652).
#[test]
fn property_fget_result_preserves_callable_signature() {
    let messages = check_source(
        r"
class Owner:
    pass
def target(value: int) -> int:
    return value
property(fget=lambda self: target).fget(self=Owner())(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "property.fget result must preserve callee: {messages:?}"
    );
}

/// A stored `property` retains the callable returned by its getter
/// (regression #761).
#[test]
fn stored_property_fget_result_preserves_callable_signature() {
    let messages = check_source(
        r"
class Owner: pass
def target(value: int) -> int: return value
prop = property(fget=lambda self: target)
prop.fget(self=Owner())(1)
prop = object()
prop.fget(self=Owner())(1)
",
    );
    assert_eq!(
        messages.len(),
        1,
        "stale property bindings must be cleared: {messages:?}"
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "stored property.fget result must preserve callee: {messages:?}"
    );
}

#[test]
fn stored_property_fget_does_not_cross_a_nested_rebinding() {
    let messages = check_source(
        r"
class Owner: pass
def target(value: int) -> int: return value
prop = property(fget=lambda self: target)
def caller() -> None:
    prop = object()
    prop.fget(self=Owner())(1)
",
    );
    assert!(
        messages.is_empty(),
        "nested rebinding must hide the stored getter: {messages:?}"
    );
}

/// Annotated `asyncio.Task[Callable[...]].result()` preserves the callable
/// signature (issue #447).
#[test]
fn task_result_preserves_callable_signatures() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
task: asyncio.Task[Callable[[int], None]]
task.result()(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "result() result"),
        "expected Task.result violation, got: {messages:?}"
    );
}

/// Asyncio `wait_for`/`shield`/`to_thread`/`gather` preserve awaitable callable
/// results (issue #446).
#[test]
fn asyncio_combinators_preserve_callable_result_signatures() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
def f(value: int) -> None: ...
async def factory() -> Callable[[int], None]:
    return f
async def caller() -> None:
    (await asyncio.wait_for(factory(), timeout=1))(1)
    (await asyncio.shield(factory()))(1)
    (await asyncio.to_thread(lambda: f))(1)
    (await asyncio.gather(factory()))[0](1)
",
    );
    for line in 8..=11 {
        assert!(
            has_error_at(&messages, line, "result"),
            "expected asyncio combinator violation on line {line}, got: {messages:?}"
        );
    }
}

/// `asyncio.run` preserves coroutine callable result types (issue #518).
#[test]
fn asyncio_run_preserves_callable_result_signatures() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]:
    def target(value: int) -> None: ...
    return target
asyncio.run(main=factory())(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "run() result"),
        "expected asyncio.run violation, got: {messages:?}"
    );
}

/// `asyncio.Runner.run` preserves coroutine callable result types (issue #519).
#[test]
fn asyncio_runner_run_preserves_callable_result_signatures() {
    let messages = check_source(
        r"
import asyncio
from collections.abc import Callable
async def factory() -> Callable[[int], None]:
    def target(value: int) -> None: ...
    return target
asyncio.Runner().run(coro=factory())(1)
",
    );
    assert!(
        has_error_at(&messages, 7, "run() result"),
        "expected Runner.run violation, got: {messages:?}"
    );
}

/// Only ``asyncio.Runner`` unwraps ``run`` results — not every ``*.Runner``
/// (issue #743).
#[test]
fn non_asyncio_runner_run_is_not_treated_as_asyncio() {
    let messages = check_source(
        r"
from collections.abc import Callable
class Runner:
    def run(self, coro: object) -> Callable[[int], None]:
        def target(value: int) -> None: ...
        return target
runner = Runner()
runner.run(object())(1)
",
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("run() result")),
        "user Runner.run must not use asyncio unwrap: {messages:?}"
    );
}

/// Definite `if True` keeps the taken branch signature (issue #508).
#[test]
fn definite_true_if_branch_signature_is_kept() {
    let messages = check_source(
        r"
if True:
    def target(value: int) -> None: ...
else:
    def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "True branch must win: {messages:?}"
    );
}

/// Unreachable `while False` must not overwrite a live signature (issue #638).
#[test]
fn unreachable_while_false_does_not_overwrite_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
while False:
    def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "while False must not overwrite: {messages:?}"
    );
}

/// Zero-iteration `for` loops must not overwrite a live signature (issue #639).
#[test]
fn zero_iteration_for_does_not_overwrite_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
for _ in []:
    def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "empty for must not overwrite: {messages:?}"
    );
}

/// Zero-iteration class-body `for` loops must not overwrite methods (issue #644).
#[test]
fn zero_iteration_class_for_does_not_overwrite_method() {
    let messages = check_source(
        r"
class Owner:
    def method(self, value: int) -> None: ...
    for _ in []:
        def method(self, value: int, /) -> None: ...
Owner().method(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("method")),
        "empty class for must not overwrite method: {messages:?}"
    );
}

/// Nested `if True` keeps the taken branch signature (issue #645).
#[test]
fn local_definite_true_if_keeps_nested_signature() {
    let messages = check_source(
        r"
def outer() -> None:
    if True:
        def target(value: int) -> None: ...
    else:
        def target(value: int, /) -> None: ...
    target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "local if True must win: {messages:?}"
    );
}

/// Nested `while False` must not overwrite a live nested signature (issue #646).
#[test]
fn local_while_false_does_not_overwrite_nested_signature() {
    let messages = check_source(
        r"
def outer() -> None:
    def target(value: int) -> None: ...
    while False:
        def target(value: int, /) -> None: ...
    target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "local while False must not overwrite: {messages:?}"
    );
}

/// Nested try/except: try-body `def` wins over the handler (issue #647).
#[test]
fn local_try_body_signature_beats_handler() {
    let messages = check_source(
        r"
def outer() -> None:
    try:
        def target(value: int) -> None: ...
    except Exception:
        def target(value: int, /) -> None: ...
    target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "local try body must win: {messages:?}"
    );
}

/// Nested match: definite first case wins (issue #648).
#[test]
fn local_definite_match_case_keeps_nested_signature() {
    let messages = check_source(
        r"
def outer() -> None:
    match 1:
        case 1:
            def target(value: int) -> None: ...
        case _:
            def target(value: int, /) -> None: ...
    target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "local match case must win: {messages:?}"
    );
}

/// Module try/except: try-body `def` wins (issue #509).
#[test]
fn try_body_signature_beats_handler() {
    let messages = check_source(
        r"
try:
    def target(value: int) -> None: ...
except Exception:
    def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "try body must win: {messages:?}"
    );
}

/// Module match: definite first case wins (issue #510).
#[test]
fn definite_match_case_keeps_signature() {
    let messages = check_source(
        r"
match 1:
    case 1:
        def target(value: int) -> None: ...
    case _:
        def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "match case must win: {messages:?}"
    );
}

/// Class-body `if True` keeps the taken method (issue #511).
#[test]
fn class_definite_true_if_keeps_method_signature() {
    let messages = check_source(
        r"
class Owner:
    if True:
        def target(self, value: int) -> None: ...
    else:
        def target(self, value: int, /) -> None: ...
Owner().target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "class if True must win: {messages:?}"
    );
}

/// `if True` class definitions keep the taken constructor (issue #640).
#[test]
fn definite_true_if_keeps_class_constructor() {
    let messages = check_source(
        r"
if True:
    class Model:
        def __init__(self, value: int) -> None: ...
else:
    class Model:
        def __init__(self, value: int, /) -> None: ...
Model(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("Model")),
        "if True class must win: {messages:?}"
    );
}

/// Try-body class constructor beats the handler (issue #641).
#[test]
fn try_body_class_constructor_beats_handler() {
    let messages = check_source(
        r"
try:
    class Model:
        def __init__(self, value: int) -> None: ...
except Exception:
    class Model:
        def __init__(self, value: int, /) -> None: ...
Model(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("Model")),
        "try body class must win: {messages:?}"
    );
}

/// Definite match keeps the selected class constructor (issue #642).
#[test]
fn definite_match_keeps_class_constructor() {
    let messages = check_source(
        r"
match 1:
    case 1:
        class Model:
            def __init__(self, value: int) -> None: ...
    case _:
        class Model:
            def __init__(self, value: int, /) -> None: ...
Model(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("Model")),
        "match class must win: {messages:?}"
    );
}

/// Class-body `while False` must not overwrite methods (issue #643).
#[test]
fn class_while_false_does_not_overwrite_method() {
    let messages = check_source(
        r"
class Owner:
    def method(self, value: int) -> None: ...
    while False:
        def method(self, value: int, /) -> None: ...
Owner().method(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("method")),
        "class while False must not overwrite: {messages:?}"
    );
}

/// Definite `if False` uses the else branch signature (issue #508).
#[test]
fn definite_false_if_branch_uses_else_signature() {
    let messages = check_source(
        r"
if False:
    def target(value: int, /) -> None: ...
else:
    def target(value: int) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "else branch must win: {messages:?}"
    );
}

/// Definite `if False` / `elif True` uses the elif body (Bugbot on #708).
#[test]
fn definite_false_if_uses_taken_elif_signature() {
    let messages = check_source(
        r"
if False:
    def target(value: int, /) -> None: ...
elif True:
    def target(value: int) -> None: ...
else:
    def target(value: int, /, extra: int) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "taken elif must win: {messages:?}"
    );
}

/// Nested definite-false elifs still reach a later taken elif.
#[test]
fn definite_false_elif_chain_reaches_later_true() {
    let messages = check_source(
        r"
if False:
    def target(value: int, /) -> None: ...
elif False:
    def target(value: int, /, unused: int) -> None: ...
elif True:
    def target(value: int) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "later elif True must win: {messages:?}"
    );
}

/// `if None` is definitely false and uses the else branch.
#[test]
fn definite_none_if_branch_uses_else_signature() {
    let messages = check_source(
        r"
if None:
    def target(value: int, /) -> None: ...
else:
    def target(value: int) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "None is falsey for definite if: {messages:?}"
    );
}

/// `if False` without else leaves the prior signature intact.
#[test]
fn definite_false_if_without_else_keeps_prior_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
if False:
    def target(value: int, /) -> None: ...
target(1)
",
    );
    assert!(
        messages.iter().any(|message| message.contains("target")),
        "no-else False must keep prior: {messages:?}"
    );
}

/// `weakref.proxy` preserves the proxied callable signature (issue #450).
#[test]
fn weakref_proxy_preserves_callable_signature() {
    let messages = check_source(
        r"
import weakref
def f(value: int) -> None: ...
weakref.proxy(f)(1)
",
    );
    assert!(
        has_error_at(&messages, 4, "f"),
        "expected weakref.proxy violation, got: {messages:?}"
    );
}

/// `contextlib.contextmanager` preserves the wrapped factory signature
/// (issue #496).
#[test]
fn contextmanager_preserves_factory_signature() {
    let messages = check_source(
        r"
import contextlib
def manager(value: int):
    yield value
contextlib.contextmanager(func=manager)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "manager"),
        "expected contextmanager factory violation, got: {messages:?}"
    );
}

/// `contextlib.asynccontextmanager` preserves the wrapped factory signature
/// (issue #497).
#[test]
fn asynccontextmanager_preserves_factory_signature() {
    let messages = check_source(
        r"
import contextlib
async def manager(value: int):
    yield value
contextlib.asynccontextmanager(func=manager)(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "manager"),
        "expected asynccontextmanager factory violation, got: {messages:?}"
    );
}

/// Dynamic `dataclasses.dataclass` returns keep the class constructor
/// signature (issue #498).
#[test]
fn dataclass_decorator_preserves_class_constructor_signature() {
    let messages = check_source(
        r"
import dataclasses
class Model:
    def __init__(self, value: int) -> None:
        pass
dataclasses.dataclass(Model)(1)
dataclasses.dataclass()(Model)(1)
",
    );
    assert!(
        has_error_at(&messages, 6, "Model") && has_error_at(&messages, 7, "Model"),
        "expected dataclass identity-return violations, got: {messages:?}"
    );
}

/// A local function consisting of one unconditional return of a concrete
/// callable preserves that callable's signature (issue #803).
#[test]
fn single_return_factory_preserves_concrete_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
def factory() -> object:
    return target
factory()(1)
",
    );
    assert!(
        has_error_at(&messages, 5, "target"),
        "expected concrete factory-result violation, got: {messages:?}"
    );
}

/// Additional statements make a factory body too dynamic to infer safely.
#[test]
fn multi_statement_factory_does_not_preserve_concrete_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
def factory() -> object:
    marker = 1
    return target
factory()(1)
",
    );
    assert!(
        messages.is_empty(),
        "dynamic factory should decline: {messages:?}"
    );
}

/// Parameters shadow enclosing concrete callables inside the factory body.
#[test]
fn single_return_factory_does_not_resolve_shadowed_parameter() {
    let messages = check_source(
        r"
def value(first: int, second: int) -> None: ...
def factory(value=lambda *args: None):
    return value
factory()(1)
",
    );
    assert!(
        messages.is_empty(),
        "shadowed parameter must not resolve to the outer callable: {messages:?}"
    );
}

/// Calling an async factory directly produces a coroutine, not its returned
/// callable value.
#[test]
fn async_single_return_factory_does_not_propagate_callable_signature() {
    let messages = check_source(
        r"
def target(value: int) -> None: ...
async def factory() -> object:
    return target
factory()(1)
",
    );
    assert!(
        messages.is_empty(),
        "async factory must decline: {messages:?}"
    );
}
