//! Integration tests for `strict-kwargs fix` (issue #7).

// `expect`/`unwrap` are idiomatic in tests: a failed fixture *should* abort
// with a clear message. Clippy's `allow-*-in-tests` does not apply to an
// integration-test crate (not `#[cfg(test)]`), so allow them here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use strict_kwargs::{fix_paths, Config, DeclinedFixReason, FixOptIns};

mod common;

use common::{TestProject, DEFAULT_PYPROJECT};

fn project(source: &str) -> TestProject {
    TestProject::new().pyproject(DEFAULT_PYPROJECT).main(source)
}

fn assert_fixed(source: &str, expected: &str) {
    let proj = project(source);
    assert_eq!(proj.fixed_main(), expected);
}

fn assert_fixed_with_opt_ins(source: &str, expected: &str, fix_opt_ins: FixOptIns) {
    let proj = project(source);
    assert_eq!(proj.fixed_main_with_opt_ins(fix_opt_ins), expected);
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

fn assert_synthesized_constructor_fixed(source: &str, expected: &str) {
    assert_fixed_with_opt_ins(
        source,
        expected,
        FixOptIns {
            synthesized_constructors: true,
        },
    );
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
fn does_not_fix_self_method_call_when_subclass_override_renames_parameter() {
    // A base-class `self.m(...)` call may dispatch to a subclass override.
    // Rewriting with the base parameter name can break overrides that chose a
    // different keyword name.
    assert_unchanged(
        "class Base:\n    def m(self, value): ...\n    def run(self, x):\n        self.m(x)\n\n\
         class Child(Base):\n    def m(self, renamed): ...\n",
    );
}

#[test]
fn does_not_fix_self_call_to_inherited_method() {
    // Inherited methods can come from descriptor/wrapper implementations whose
    // runtime keyword behavior is narrower than the visible signature.
    assert_unchanged(
        "class Base:\n    def m(self, fullname): ...\n\n\
         class Child(Base):\n    def run(self, fullname):\n        self.m(fullname)\n",
    );
}

#[test]
fn does_not_fix_inherited_constructor_call() {
    // A subclass can inherit a constructor from a library base whose runtime
    // keyword behavior is narrower than the indexed/hovered signature.
    assert_unchanged(
        "class Base:\n    def __init__(self, document): ...\n\n\
         class Child(Base):\n    pass\n\n\
         Child('doc')\n",
    );
}

#[test]
fn does_not_fix_polymorphic_method_call_when_subclass_override_renames_parameter() {
    // The same hazard exists when a base-typed local or parameter receives a
    // subclass instance at runtime.
    assert_unchanged(
        "class Base:\n    def m(self, value): ...\n\n\
         class Child(Base):\n    def m(self, renamed): ...\n\n\
         def run(base: Base, x):\n    base.m(x)\n",
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
fn explicit_dunder_constructor_keeps_receiver_positional() {
    assert_fixed(
        "class Base:\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None): ...\n\n\
         class Child(Base):\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None):\n        Base.__init__(self, outfp, mangle_from_, maxheaderlen, policy=policy)\n",
        "class Base:\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None): ...\n\n\
         class Child(Base):\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None):\n        Base.__init__(self, outfp=outfp, mangle_from_=mangle_from_, maxheaderlen=maxheaderlen, policy=policy)\n",
    );
}

#[test]
fn explicit_dunder_constructor_via_module_keeps_receiver_positional() {
    let proj = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("pkg/__init__.py", "")
        .file(
            "pkg/base.py",
            "class Base:\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None): ...\n",
        )
        .main(
            "from pkg import base\n\n\
             class Child(base.Base):\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None):\n        base.Base.__init__(self, outfp, mangle_from_, maxheaderlen, policy=policy)\n",
        );

    assert_eq!(
        proj.fixed_main(),
        "from pkg import base\n\n\
         class Child(base.Base):\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None):\n        base.Base.__init__(self, outfp=outfp, mangle_from_=mangle_from_, maxheaderlen=maxheaderlen, policy=policy)\n"
    );
}

#[test]
fn inherited_explicit_dunder_constructor_keeps_receiver_positional() {
    assert_fixed(
        "class Base:\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None): ...\n\n\
         class Child(Base):\n    pass\n\n\
         Child.__init__(Child(), outfp, mangle_from_, maxheaderlen, policy=policy)\n",
        "class Base:\n    def __init__(self, outfp, mangle_from_=None, maxheaderlen=None, *, policy=None): ...\n\n\
         class Child(Base):\n    pass\n\n\
         Child.__init__(Child(), outfp=outfp, mangle_from_=mangle_from_, maxheaderlen=maxheaderlen, policy=policy)\n",
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
fn does_not_fix_private_callable_boundary() {
    // Private helpers are commonly monkeypatched with simpler callables whose
    // parameter names do not match the implementation.
    assert_unchanged("def _helper(outdir, filename): ...\n\n_helper('out', 'file')\n");
}

#[test]
fn does_not_fix_imported_callable_boundary() {
    // Directly imported callables are also commonly monkeypatched at the
    // importing module boundary.
    let proj = TestProject::new()
        .pyproject("[project]\nname = \"t\"\nversion = \"0\"\n")
        .file("other.py", "def build_main(argv): ...\n")
        .main("from other import build_main\n\nbuild_main(['x'])\n");
    assert_eq!(
        proj.fixed_main(),
        "from other import build_main\n\nbuild_main(['x'])\n"
    );
}

#[test]
fn does_not_corrupt_typeshed_annotated_callable_instance_overloads() {
    // `unittest.mock.patch` is a module variable annotated as a `_patcher`
    // instance in typeshed. Resolve it through `_patcher.__call__` rather than
    // relying on ty hover, which can expose the already-bound tail signature
    // and rewrite the target string as `spec=...`.
    assert_unchanged(
        "from unittest.mock import patch\n\n@patch('pkg.mod.obj')\ndef test(obj):\n    pass\n",
    );
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
fn fixes_ty_resolved_bound_dunder_without_double_receiver_skip() {
    // `super().__init__` is resolved by ty as a bound method whose hover
    // signature is already call-site oriented. A `.__init__` suffix must not
    // make the fixer skip the first real argument again.
    assert_fixed(
        "class Base:\n    def __init__(self, srcdir: str, confdir: str, confoverrides: dict | None = None) -> None: ...\n\n\
         class Child(Base):\n    def __init__(self, srcdir: str, confdir: str, confoverrides: dict | None = None) -> None:\n        super().__init__(srcdir, confdir, confoverrides=confoverrides)\n",
        "class Base:\n    def __init__(self, srcdir: str, confdir: str, confoverrides: dict | None = None) -> None: ...\n\n\
         class Child(Base):\n    def __init__(self, srcdir: str, confdir: str, confoverrides: dict | None = None) -> None:\n        super().__init__(srcdir=srcdir, confdir=confdir, confoverrides=confoverrides)\n",
    );
}

#[test]
fn ty_fix_recording_is_idempotent_for_nested_method_call() {
    assert_fixed(
        "def encode(netloc: str) -> str:\n    return netloc.encode('idna').decode('ascii')\n",
        "def encode(netloc: str) -> str:\n    return netloc.encode(encoding='idna').decode(encoding='ascii')\n",
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
fn does_not_fix_ty_self_hover_that_may_hide_positional_only() {
    // ty can display third-party `super().__init__` hovers as `Self@__init__`
    // and lose runtime positional-only markers. Decline the fix rather than
    // rewriting to a keyword that may raise TypeError.
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
        "class Base:\n    def __init__(self, document, /): ...\n",
    )
    .expect("pkg init");

    let source = "from extdep import Base\n\n\
                  class Child(Base):\n    def __init__(self, document):\n        super().__init__(document)\n";
    let proj = project(source);
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(
        &proj.root,
        std::slice::from_ref(&main),
        &config,
        Some(venv.as_path()),
    )
    .expect("fix");
    assert!(outcome.files.is_empty(), "{outcome:?}");
}

#[test]
fn does_not_fix_call_through_opaque_callable_parameter() {
    // A callable parameter can receive a different implementation at runtime
    // whose parameter names do not match the annotation/protocol names.
    assert_unchanged(
        "from typing import Protocol\n\n\
         class Formatter(Protocol):\n    def __call__(self, date, format, *, locale): ...\n\n\
         def run(date, format, locale, formatter: Formatter):\n    return formatter(date, format, locale=locale)\n",
    );
}

#[test]
fn does_not_fix_method_call_through_opaque_parameter() {
    // Annotated object parameters are injection boundaries: tests and callers
    // can pass mocks/proxies whose method call assertions depend on the
    // original positional shape.
    assert_unchanged(
        "class Renderer:\n    def render(self, template_name, context): ...\n\n\
         def run(renderer: Renderer, context):\n    return renderer.render('module', context)\n",
    );
}

#[test]
fn does_not_fix_call_through_bound_method_alias() {
    // A bound method saved in a local can still dispatch to subclass
    // implementations with different parameter names.
    assert_unchanged(
        "class SearchLanguage:\n    def word_filter(self, word): ...\n\n\
         class SearchJapanese(SearchLanguage):\n    def word_filter(self, stemmed_word): ...\n\n\
         def feed(lang: SearchLanguage, stemmed_word):\n    _filter = lang.word_filter\n    return _filter(stemmed_word)\n",
    );
}

#[test]
fn fixes_protocol_callable_value_returned_by_factory() {
    // Follow-up to issue #115: when ty can resolve the callable value to a
    // Protocol `__call__`, use that call signature, not the factory's
    // parameter names.
    assert_fixed(
        "from typing import Protocol\n\n\
         class Formatter(Protocol):\n    def __call__(self, value: str) -> str: ...\n\n\
         def build_formatter(quote_char: str) -> Formatter: ...\n\n\
         formatter = build_formatter(quote_char='\"')\n\
         value = 'hello'\n\
         formatter(value)\n",
        "from typing import Protocol\n\n\
         class Formatter(Protocol):\n    def __call__(self, value: str) -> str: ...\n\n\
         def build_formatter(quote_char: str) -> Formatter: ...\n\n\
         formatter = build_formatter(quote_char='\"')\n\
         value = 'hello'\n\
         formatter(value=value)\n",
    );
}

#[test]
fn does_not_fix_annotated_callable_value_returned_by_factory() {
    // `Callable[[float], str]` does not carry a safe parameter name. The
    // factory's `prefix` parameter must not be used for calls through the
    // returned function value.
    assert_unchanged(
        "from collections.abc import Callable\n\n\
         def build_wrapper(prefix: str) -> Callable[[float], str]: ...\n\n\
         finite: Callable[[float], str] = build_wrapper(prefix='')\n\
         value = 1.0\n\
         finite(value)\n",
    );
}

#[test]
fn fixes_callable_instance_value_returned_by_factory() {
    assert_fixed(
        "class Formatter:\n    def __call__(self, value: str) -> str: ...\n\n\
         def build_formatter(quote_char: str) -> Formatter: ...\n\n\
         formatter = build_formatter(quote_char='\"')\n\
         value = 'hello'\n\
         formatter(value)\n",
        "class Formatter:\n    def __call__(self, value: str) -> str: ...\n\n\
         def build_formatter(quote_char: str) -> Formatter: ...\n\n\
         formatter = build_formatter(quote_char='\"')\n\
         value = 'hello'\n\
         formatter(value=value)\n",
    );
}

#[test]
fn does_not_fix_private_parameter_name() {
    // Private/stub-style double-underscore parameter names are not safe
    // keyword targets. The checker can still report the call, but `fix` must
    // not emit an unsafe keyword such as `load(__fp=fp)` (issue #114).
    let source = "def load(__fp):\n    return __fp\n\nfp = object()\nload(fp)\n";
    let proj = project(source);
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(&proj.root, std::slice::from_ref(&main), &config, None).expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 1);
    assert_eq!(std::fs::read_to_string(main).expect("read source"), source);
}

#[test]
fn does_not_fix_overloaded_builtin() {
    // `str` is overloaded in typeshed: still flagged by the checker, but the
    // overload safety rule (not a builtins carve-out) keeps the fixer away.
    assert_unchanged("str(123)\n");
}

#[test]
fn fixes_overloaded_stdlib_when_ty_selection_is_unambiguous() {
    // `os.getenv` has overload arms, but this arity and argument shape select
    // one fully named arm. The overload fix path may rewrite it with that
    // selected parameter mapping.
    assert_fixed(
        "import os\n\nos.getenv(\"PATH\", \"fallback\")\n",
        "import os\n\nos.getenv(key=\"PATH\", default=\"fallback\")\n",
    );
}

#[test]
fn does_not_fix_before_var_positional() {
    // Absorbed by `*rest`: not a violation, nothing to fix.
    assert_unchanged("def f(a: int, *rest: int) -> None: ...\nf(1, 2, 3)\n");
}

#[test]
fn does_not_fix_overloaded_callee() {
    // Two signatures with the same parameter-name mapping still remain a
    // multi-arm overload for the fixer: without a uniquely identifiable arm,
    // the conservative rule declines the rewrite.
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
fn fixes_overloaded_callee_when_ty_selects_one_differently_named_arm() {
    // Issue #95: overload arms can map the same positional slot to different
    // parameter names. Rewrite only when ty's call-site hover selects exactly
    // one indexed arm, so the chosen keyword name is not guessed.
    assert_fixed(
        "from typing import overload\n\
         @overload\n\
         def f(count: int) -> int: ...\n\
         @overload\n\
         def f(text: str) -> str: ...\n\
         def f(value):\n    return value\n\
         f(1)\n",
        "from typing import overload\n\
         @overload\n\
         def f(count: int) -> int: ...\n\
         @overload\n\
         def f(text: str) -> str: ...\n\
         def f(value):\n    return value\n\
         f(count=1)\n",
    );
}

#[test]
fn fixes_overloaded_callee_for_precisely_annotated_argument() {
    assert_fixed(
        "from typing import overload\n\
         @overload\n\
         def f(count: int) -> int: ...\n\
         @overload\n\
         def f(text: str) -> str: ...\n\
         def f(value):\n    return value\n\
         def g(x: int):\n    f(x)\n",
        "from typing import overload\n\
         @overload\n\
         def f(count: int) -> int: ...\n\
         @overload\n\
         def f(text: str) -> str: ...\n\
         def f(value):\n    return value\n\
         def g(x: int):\n    f(count=x)\n",
    );
}

#[test]
fn does_not_fix_overloaded_callee_when_ty_selection_is_not_unique() {
    // `x` could match either overload arm, whose first parameter names differ.
    // The union annotation is not precise enough for the fixer to trust any
    // single hover arm, so it keeps the diagnostic declined.
    let source = "from typing import overload\n\
         @overload\n\
         def f(count: int) -> int: ...\n\
         @overload\n\
         def f(text: str) -> str: ...\n\
         def f(value):\n    return value\n\
         def g(x: int | str):\n    f(x)\n";
    let proj = project(source);
    let outcome = proj.fix_main_result().expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 1);
    assert_eq!(outcome.declined_reasons.len(), 1);
    assert_eq!(
        outcome.declined_reasons[0].reason,
        DeclinedFixReason::UnresolvedOverload
    );
    assert_eq!(outcome.declined_reasons[0].count, 1);
}

#[test]
fn callable_expression_overload_is_declined_when_it_cannot_be_hovered() {
    let proj = project(
        "from typing import overload\n\nclass C:\n    @overload\n    def __call__(self, count: int) -> int: ...\n    @overload\n    def __call__(self, text: str) -> str: ...\n    def __call__(self, value):\n        return value\n\nC()(1)\n",
    );
    let outcome = proj.fix_main_result().expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 1);
    assert_eq!(outcome.declined_reasons.len(), 1);
    assert_eq!(
        outcome.declined_reasons[0].reason,
        DeclinedFixReason::UnresolvedOverload
    );
    assert_eq!(outcome.declined_reasons[0].count, 1);
}

#[test]
fn scans_annotated_method_vararg_and_kwarg_parameters() {
    assert_unchanged(
        "class C:\n    def m(self, *rest: int, **kw: int) -> None: ...\n\nc = C()\nc.m()\n",
    );
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
fn does_not_fix_when_rewrite_would_duplicate_existing_keyword() {
    // `add(1, a=2)` would rewrite to `add(a=1, a=2)`, so the fixer declines
    // the call before the parse fail-safe has to catch invalid syntax.
    let source = "def add(a, b): ...\nadd(1, a=2)\n";
    assert_unchanged(source);
    assert!(
        project(source)
            .check_main()
            .iter()
            .any(|m| m.contains("Too many positional")),
        "duplicate-keyword-risk violation should still be flagged"
    );
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
fn fix_synthesized_constructors_rewrites_dataclass_constructor() {
    assert_synthesized_constructor_fixed(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(1, 2)\n",
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(x=1, y=2)\n",
    );
}

#[test]
fn config_fix_synthesized_constructors_rewrites_dataclass_constructor() {
    let proj = project(
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(1, 2)\n",
    )
    .pyproject(
        "[project]\nname = \"t\"\nversion = \"0\"\n\n[tool.strict_kwargs]\nfix_synthesized_constructors = true\n",
    );
    let main = proj.root.join("main.py");
    let config = Config::load(&proj.root).expect("valid config");
    let outcome = fix_paths(&proj.root, std::slice::from_ref(&main), &config, None).expect("fix");
    assert_eq!(outcome.declined, 0);
    assert_eq!(outcome.files.len(), 1);
    assert_eq!(
        outcome.files[0].fixed,
        "from dataclasses import dataclass\n\n@dataclass\nclass D:\n    x: int\n    y: int\n\nD(x=1, y=2)\n"
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
fn fix_synthesized_constructors_rewrites_inherited_dataclass_constructor() {
    assert_synthesized_constructor_fixed(
        "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    base: int\n\n@dataclass\nclass Child(Base):\n    child: int\n\nChild(1, 2)\n",
        "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    base: int\n\n@dataclass\nclass Child(Base):\n    child: int\n\nChild(base=1, child=2)\n",
    );
}

#[test]
fn synthesized_namedtuple_constructor_not_rewritten() {
    assert_unchanged(
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(1, 2)\n",
    );
}

#[test]
fn fix_synthesized_constructors_rewrites_namedtuple_constructor() {
    assert_synthesized_constructor_fixed(
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(1, 2)\n",
        "from typing import NamedTuple\n\nclass NT(NamedTuple):\n    a: int\n    b: int\n\nNT(a=1, b=2)\n",
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
    assert_eq!(outcome.declined_reasons.len(), 1);
    assert_eq!(
        outcome.declined_reasons[0].reason,
        DeclinedFixReason::SynthesizedConstructor
    );
    assert_eq!(outcome.declined_reasons[0].count, 1);
    assert_eq!(outcome.files.len(), 1);
    assert_eq!(outcome.files[0].count, 1);
    // Applying the fix leaves exactly `declined` violations behind.
    std::fs::write(&main, &outcome.files[0].fixed).expect("write fixed");
    assert_eq!(proj.check_main().len(), outcome.declined);
}

#[test]
fn declined_reason_tracks_unsafe_call_site_unpacking() {
    let proj = project("def f(a, *, b): ...\n\nrest = (2,)\nf(1, *rest)\n");
    let outcome = proj.fix_main_result().expect("fix");
    assert!(outcome.files.is_empty());
    assert_eq!(outcome.declined, 1);
    assert_eq!(outcome.declined_reasons.len(), 1);
    assert_eq!(
        outcome.declined_reasons[0].reason,
        DeclinedFixReason::UnsafeCallSiteUnpacking
    );
    assert_eq!(outcome.declined_reasons[0].count, 1);
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
