//! Continuous benchmarks for the resolver hot paths (issue #30).
//!
//! Run locally with `cargo bench`; CI runs the same binary under `CodSpeed`
//! (`.github/workflows/ci.yml`) so every PR gets an instruction-count delta
//! against `main`.
//!
//! `ty` is a hard requirement, so the `CodSpeed` job installs it (otherwise
//! `check_paths`/`fix_paths` would error out). Its presence is probed once
//! per process and memoized (`require_ty_present`), so the benchmarks are
//! not perturbed by repeated `ty version` spawns. Every fixture is designed
//! to be fully resolvable by the built-in resolver (project code + embedded
//! typeshed), so `ty_pending` stays empty and the `ty server` subprocess is
//! never started — the measured numbers are the deterministic parse / index
//! / walk / resolve cost, exactly the hot paths every future resolver change
//! touches.

// `expect`/`unwrap`/`panic` are idiomatic in a benchmark harness: a broken
// fixture *should* abort with a clear message. A `[[bench]]` target is not
// `#[cfg(test)]`, so clippy's `allow-*-in-tests` does not apply here; mirror
// `tests/integration.rs` and allow them explicitly.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

use strict_kwargs::{check_paths, fix_paths, unified_diff, Config};

fn main() {
    divan::main();
}

/// Absolute path to a vendored fixture project under `benches/fixtures/`.
fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
        .join(name)
}

/// Run the full check pipeline over `root` and return the diagnostic count
/// (returned so divan black-boxes it and the work is not optimised away).
fn check(root: &Path) -> usize {
    let config = Config::load(root).expect("valid benchmark-fixture config");
    let paths = [root.to_path_buf()];
    check_paths(root, &paths, &config, None, None)
        .expect("check_paths over a benchmark fixture must succeed")
        .len()
}

/// Number of modules in the generated first-party closure. Large enough to
/// dominate the run with cross-module import/re-export following, small
/// enough that one-time generation stays cheap.
const FIRST_PARTY_MODULES: usize = 60;

/// A deterministic first-party project: `pkg/mod_000 … mod_059`, each
/// importing the next two modules so a single entry point pulls the whole
/// closure, plus an `app.py` that calls across modules positionally.
///
/// Generated once and reused (the inputs are read-only), so generation never
/// counts toward the measured benchmark.
fn first_party_project() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp = tempfile::tempdir().expect("create first-party fixture tempdir");
            let root = temp.path();
            std::fs::write(
                root.join("pyproject.toml"),
                "[project]\nname = \"first-party-fixture\"\nversion = \"0\"\n",
            )
            .expect("write pyproject.toml");

            let pkg = root.join("pkg");
            std::fs::create_dir_all(&pkg).expect("create pkg/");
            std::fs::write(pkg.join("__init__.py"), "").expect("write pkg/__init__.py");

            for index in 0..FIRST_PARTY_MODULES {
                let mut module = format!("\"\"\"Generated module {index}.\"\"\"\n\n");
                for offset in 1..=2 {
                    let target = index + offset;
                    if target < FIRST_PARTY_MODULES {
                        writeln!(
                            module,
                            "from pkg.mod_{target:03} import func_{target}"
                        )
                        .expect("format import");
                    }
                }
                writeln!(
                    module,
                    "\n\ndef func_{index}(alpha: int, beta: int, gamma: int = 0) -> int:\n    return alpha + beta + gamma\n\n\ndef use_{index}() -> int:\n    return func_{index}(1, 2, 3)"
                )
                .expect("format defs");
                let next = index + 1;
                if next < FIRST_PARTY_MODULES {
                    writeln!(
                        module,
                        "\n\nchained_{index} = func_{next}(4, 5, 6)"
                    )
                    .expect("format cross-module call");
                }
                std::fs::write(pkg.join(format!("mod_{index:03}.py")), module)
                    .expect("write generated module");
            }

            let mut app = String::from("\"\"\"Entry point importing the closure head.\"\"\"\n\n");
            for index in 0..FIRST_PARTY_MODULES {
                writeln!(app, "from pkg.mod_{index:03} import func_{index}")
                    .expect("format app import");
            }
            app.push('\n');
            for index in 0..FIRST_PARTY_MODULES {
                writeln!(app, "result_{index} = func_{index}(7, 8, 9)")
                    .expect("format app call");
            }
            std::fs::write(root.join("app.py"), app).expect("write app.py");

            temp
        })
        .path()
}

/// Width of the re-export hub: a `__init__` that aggregates this many leaf
/// modules through chained `from … import *` hops, the structural shape of a
/// real third-party package's public surface (numpy/scipy-style). Large
/// enough that backward re-export resolution dominates; small enough that
/// one-time generation stays cheap.
const REEXPORT_LEAVES: usize = 40;
/// Functions defined per leaf module (each a positional call target).
const REEXPORT_FUNCS_PER_LEAF: usize = 8;

/// Number of packages in the whole-project fixture (issue #69). Each package
/// contributes `WHOLE_PROJECT_MODS_PER_PKG` independent modules; the flat fan-
/// out means every file in a package can be processed without waiting for any
/// sibling, maximising rayon utilisation in Phase 1.
const WHOLE_PROJECT_PKGS: usize = 4;
/// Independent modules per package. Total file count is
/// `WHOLE_PROJECT_PKGS × WHOLE_PROJECT_MODS_PER_PKG + 1` (the `app.py` entry
/// point). Chosen so the whole-project wall-clock is large enough to expose
/// variance without making CI noticeably slower.
const WHOLE_PROJECT_MODS_PER_PKG: usize = 25;
/// Call-site variants per module (plain, keyword-only, default-bearing). More
/// patterns means the checker's visitor code is exercised more thoroughly.
const WHOLE_PROJECT_FUNCS_PER_MOD: usize = 5;
/// Packages in the larger generated whole-project benchmark. This is intended
/// to provide a PR-visible `CodSpeed` signal at a scale closer to real projects
/// without pulling in network checkout / install overhead from Sphinx or `CPython`.
const LARGE_PROJECT_PKGS: usize = 10;
/// Independent modules per package in the large generated benchmark.
const LARGE_PROJECT_MODS_PER_PKG: usize = 50;
/// Functions and matching positional call sites per generated module.
const LARGE_PROJECT_FUNCS_PER_MOD: usize = 5;
/// Modules in the `ty` fallback benchmark. Kept moderate because every sample
/// starts a `ty server` and sends LSP hover requests.
const TY_FALLBACK_MODULES: usize = 8;
/// return-typed factory calls per module in the `ty` fallback benchmark.
const TY_FALLBACK_CALLS_PER_MODULE: usize = 12;
/// Environment variable pointing at an optional pinned `CPython` checkout.
const CPYTHON_BENCH_CHECKOUT_ENV: &str = "STRICT_KWARGS_BENCH_CPYTHON_CHECKOUT";

fn cpython_benchmark_checkout() -> Option<PathBuf> {
    let raw = std::env::var_os(CPYTHON_BENCH_CHECKOUT_ENV)?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    path.exists().then_some(path)
}

/// Generate a deterministic multi-package project under `root`.
fn write_project_fixture(
    root: &Path,
    project_name: &str,
    packages: usize,
    modules_per_package: usize,
    functions_per_module: usize,
) {
    std::fs::write(
        root.join("pyproject.toml"),
        format!("[project]\nname = \"{project_name}\"\nversion = \"0\"\n"),
    )
    .expect("write pyproject.toml");

    for pkg_idx in 0..packages {
        let pkg_name = format!("pkg_{pkg_idx}");
        let pkg_dir = root.join(&pkg_name);
        std::fs::create_dir_all(&pkg_dir).expect("create package dir");
        std::fs::write(pkg_dir.join("__init__.py"), "").expect("write package __init__.py");

        for mod_idx in 0..modules_per_package {
            let mut src = format!("\"\"\"Generated module p{pkg_idx}m{mod_idx}.\"\"\"\n\n");
            for func_idx in 0..functions_per_module {
                // Vary the signature shape so the checker visits different
                // ParameterKind combinations.
                match func_idx % 3 {
                    0 => writeln!(
                        src,
                        "def f_{pkg_idx}_{mod_idx}_{func_idx}(alpha: int, beta: int, gamma: int = 0) -> int:\n    return alpha + beta + gamma\n"
                    ),
                    1 => writeln!(
                        src,
                        "def f_{pkg_idx}_{mod_idx}_{func_idx}(x: str, y: str) -> str:\n    return x + y\n"
                    ),
                    _ => writeln!(
                        src,
                        "def f_{pkg_idx}_{mod_idx}_{func_idx}(n: int) -> int:\n    return n\n"
                    ),
                }
                .expect("format function def");
            }
            std::fs::write(pkg_dir.join(format!("mod_{mod_idx:03}.py")), src)
                .expect("write generated module");
        }
    }

    // app.py: import every function and call it positionally (so every call
    // site is a potential violation the checker must evaluate).
    let mut app = String::from("\"\"\"Entry point that exercises every package.\"\"\"\n\n");
    for pkg_idx in 0..packages {
        for mod_idx in 0..modules_per_package {
            for func_idx in 0..functions_per_module {
                let pkg = format!("pkg_{pkg_idx}");
                let mod_ = format!("mod_{mod_idx:03}");
                let func = format!("f_{pkg_idx}_{mod_idx}_{func_idx}");
                writeln!(app, "from {pkg}.{mod_} import {func}").expect("format import");
            }
        }
    }
    app.push('\n');
    for pkg_idx in 0..packages {
        for mod_idx in 0..modules_per_package {
            for func_idx in 0..functions_per_module {
                let func = format!("f_{pkg_idx}_{mod_idx}_{func_idx}");
                match func_idx % 3 {
                    0 => writeln!(app, "{func}(1, 2, 3)"),
                    1 => writeln!(app, "{func}(\"a\", \"b\")"),
                    _ => writeln!(app, "{func}(42)"),
                }
                .expect("format call");
            }
        }
    }
    std::fs::write(root.join("app.py"), app).expect("write app.py");
}

/// A deterministic first-party project whose package root re-exports a wide,
/// chained `import *` web — `pkg/__init__` ← `pkg.api` ← `pkg.agg` ←
/// `pkg.leaf_000 … leaf_039`, with `app.py` calling every leaf function
/// *through the package root* positionally.
///
/// This is the regression fixture for issue #39: the old eager
/// `expand_reexports` materialized the full alias cross-product over this
/// shape and did not complete on a real heavy closure; the lazy
/// demand-driven resolver chases each queried name backward through the
/// `dst`-keyed edge index instead. Every name is fully resolvable by the
/// built-in resolver (no `ty`, no third-party deps), so the measured number
/// is the deterministic lazy-resolution hot path #39 introduced. Generated
/// once and reused (read-only), so generation never counts toward the bench.
fn reexport_hub_project() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp = tempfile::tempdir().expect("create reexport fixture tempdir");
            let root = temp.path();
            std::fs::write(
                root.join("pyproject.toml"),
                "[project]\nname = \"reexport-fixture\"\nversion = \"0\"\n",
            )
            .expect("write pyproject.toml");

            let pkg = root.join("pkg");
            std::fs::create_dir_all(&pkg).expect("create pkg/");
            // Chained star re-export hops: root ← api ← agg ← every leaf.
            std::fs::write(pkg.join("__init__.py"), "from pkg.api import *\n")
                .expect("write pkg/__init__.py");
            std::fs::write(pkg.join("api.py"), "from pkg.agg import *\n")
                .expect("write pkg/api.py");
            let mut agg = String::new();
            for leaf in 0..REEXPORT_LEAVES {
                writeln!(agg, "from pkg.leaf_{leaf:03} import *").expect("format agg import");
            }
            std::fs::write(pkg.join("agg.py"), agg).expect("write pkg/agg.py");

            for leaf in 0..REEXPORT_LEAVES {
                let mut module = format!("\"\"\"Generated leaf {leaf}.\"\"\"\n\n");
                for func in 0..REEXPORT_FUNCS_PER_LEAF {
                    writeln!(
                        module,
                        "def leaf_{leaf:03}_f{func}(alpha: int, beta: int, gamma: int = 0) -> int:\n    return alpha + beta + gamma\n"
                    )
                    .expect("format leaf def");
                }
                std::fs::write(pkg.join(format!("leaf_{leaf:03}.py")), module)
                    .expect("write leaf module");
            }

            let mut app = String::from("\"\"\"Calls every re-exported leaf via the package root.\"\"\"\n\n");
            for leaf in 0..REEXPORT_LEAVES {
                for func in 0..REEXPORT_FUNCS_PER_LEAF {
                    writeln!(app, "from pkg import leaf_{leaf:03}_f{func}")
                        .expect("format app import");
                }
            }
            app.push('\n');
            for leaf in 0..REEXPORT_LEAVES {
                for func in 0..REEXPORT_FUNCS_PER_LEAF {
                    writeln!(app, "leaf_{leaf:03}_f{func}(1, 2, 3)")
                        .expect("format app call");
                }
            }
            std::fs::write(root.join("app.py"), app).expect("write app.py");

            temp
        })
        .path()
}

/// A deterministic whole-project directory (issue #69): `WHOLE_PROJECT_PKGS`
/// packages of `WHOLE_PROJECT_MODS_PER_PKG` independent modules each, plus a
/// top-level `app.py` entry point that imports and calls every function.
///
/// The modules within each package share no imports — every file in a package
/// is an independent unit of work — so rayon can process the entire package in
/// parallel during Phase 1 (the built-in pass). The `app.py` calls span all
/// packages, so the index walk exercises cross-package resolution too.
///
/// Every call is resolvable by the built-in resolver (project code only, no
/// third-party deps), so the `ty server` subprocess is never started and the
/// measured numbers are the deterministic Phase-1 (parallel built-in pass)
/// hot path — exactly what issue #46 / #70 targeted and what issue #69 asks to
/// track over time. Generated once and reused (read-only).
fn whole_project_dir() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp = tempfile::tempdir().expect("create whole-project fixture tempdir");
            write_project_fixture(
                temp.path(),
                "whole-project-fixture",
                WHOLE_PROJECT_PKGS,
                WHOLE_PROJECT_MODS_PER_PKG,
                WHOLE_PROJECT_FUNCS_PER_MOD,
            );
            temp
        })
        .path()
}

/// Larger generated whole-project directory: 500 modules plus `app.py`, with
/// 2,500 imported positional call sites. This keeps the benchmark hermetic like
/// the smaller fixtures while making end-to-end directory-run scaling visible
/// in `CodSpeed`.
fn large_project_dir() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp = tempfile::tempdir().expect("create large-project fixture tempdir");
            write_project_fixture(
                temp.path(),
                "large-project-fixture",
                LARGE_PROJECT_PKGS,
                LARGE_PROJECT_MODS_PER_PKG,
                LARGE_PROJECT_FUNCS_PER_MOD,
            );
            temp
        })
        .path()
}

/// A deterministic project whose calls intentionally go through the `ty`
/// fallback. Calls through `make_worker().configure(...)` are not direct
/// constructor receivers the built-in resolver tracks, but `ty` can infer the
/// return type and provide a bound-method hover for safe fixes.
fn ty_fallback_project_dir() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp = tempfile::tempdir().expect("create ty-fallback fixture tempdir");
            let root = temp.path();
            std::fs::write(
                root.join("pyproject.toml"),
                "[project]\nname = \"ty-fallback-fixture\"\nversion = \"0\"\n",
            )
            .expect("write pyproject.toml");

            let pkg = root.join("pkg");
            std::fs::create_dir_all(&pkg).expect("create package dir");
            std::fs::write(pkg.join("__init__.py"), "").expect("write package __init__.py");

            for module in 0..TY_FALLBACK_MODULES {
                let mut src = String::from(
                    "class Worker:\n    def configure(self, host: str, port: int) -> None: ...\n\n\
                     def make_worker() -> Worker:\n    return Worker()\n\n",
                );
                for call in 0..TY_FALLBACK_CALLS_PER_MODULE {
                    writeln!(
                        src,
                        "make_worker().configure(\"host-{module}-{call}\", {call})"
                    )
                    .expect("format return-typed method call");
                }
                std::fs::write(pkg.join(format!("mod_{module:03}.py")), src)
                    .expect("write ty fallback module");
            }

            temp
        })
        .path()
}

#[divan::bench]
fn leaf() -> usize {
    check(&fixture_dir("leaf"))
}

#[divan::bench]
fn stdlib_closure() -> usize {
    check(&fixture_dir("stdlib_closure"))
}

#[divan::bench]
fn special_forms_overloads() -> usize {
    check(&fixture_dir("special_forms"))
}

#[divan::bench]
fn first_party_closure() -> usize {
    check(first_party_project())
}

/// Heavy re-export closure (issue #39): wide, chained `import *` web fully
/// resolved by the lazy demand-driven backward resolver. The eager
/// `expand_reexports` this replaced did not complete on a real closure of
/// this shape.
#[divan::bench]
fn reexport_closure() -> usize {
    check(reexport_hub_project())
}

/// Whole-project directory run (issue #69): tracks the post-parallelisation
/// Phase-1 baseline across the full parse / index / walk / resolve pipeline
/// over a multi-package project. A regression in directory-run time (or a
/// return of high variance) shows up here rather than being noticed manually.
#[divan::bench]
fn whole_project() -> usize {
    check(whole_project_dir())
}

/// Larger end-to-end directory run. This is the generated-project analogue of
/// the scheduled Sphinx/CPython dry runs: big enough to expose scaling changes,
/// but deterministic and cheap enough to report through `CodSpeed` on PRs.
#[divan::bench]
fn large_project() -> usize {
    check(large_project_dir())
}

/// Packages in the ty-exercising whole-project fixture. Kept smaller than
/// `WHOLE_PROJECT_PKGS` so Phase 1 bulk stays comparable to ty round-trip
/// cost; the fixture is designed to show the pipelining overlap (issue #67).
const WHOLE_PROJECT_TY_PKGS: usize = 3;
/// Modules per package (first-party, fully resolved by the built-in pass).
const WHOLE_PROJECT_TY_MODS_PER_PKG: usize = 20;
/// Functions defined per first-party module.
const WHOLE_PROJECT_TY_FUNCS_PER_MOD: usize = 5;
/// Base/derived class pairs in the inherited-method module. These used to
/// defer to `ty`; issue #135 keeps them in the built-in resolver.
const WHOLE_PROJECT_TY_INHERITED: usize = 10;

/// A deterministic whole-project directory that mixes first-party Phase 1
/// work with inherited-method calls (issue #67 / #135). The first-party
/// packages are identical in shape to [`whole_project_dir`]; the extra
/// `ty_pkg` package adds a `base.py` / `derived.py` / `caller.py` triple
/// whose inherited calls are now resolved by the built-in inheritance lookup.
fn whole_project_ty_dir() -> &'static Path {
    static PROJECT: OnceLock<TempDir> = OnceLock::new();
    PROJECT
        .get_or_init(|| {
            let temp =
                tempfile::tempdir().expect("create whole-project-ty fixture tempdir");
            let root = temp.path();
            std::fs::write(
                root.join("pyproject.toml"),
                "[project]\nname = \"whole-project-ty-fixture\"\nversion = \"0\"\n",
            )
            .expect("write pyproject.toml");

            // First-party packages: independent modules, fully resolvable by
            // the built-in pass (same structure as `whole_project_dir`).
            for pkg_idx in 0..WHOLE_PROJECT_TY_PKGS {
                let pkg_name = format!("pkg_{pkg_idx}");
                let pkg_dir = root.join(&pkg_name);
                std::fs::create_dir_all(&pkg_dir).expect("create package dir");
                std::fs::write(pkg_dir.join("__init__.py"), "")
                    .expect("write package __init__.py");
                for mod_idx in 0..WHOLE_PROJECT_TY_MODS_PER_PKG {
                    let mut src =
                        format!("\"\"\"Generated module p{pkg_idx}m{mod_idx}.\"\"\"\n\n");
                    for func_idx in 0..WHOLE_PROJECT_TY_FUNCS_PER_MOD {
                        match func_idx % 3 {
                            0 => writeln!(
                                src,
                                "def f_{pkg_idx}_{mod_idx}_{func_idx}(alpha: int, beta: int, gamma: int = 0) -> int:\n    return alpha + beta + gamma\n"
                            ),
                            1 => writeln!(
                                src,
                                "def f_{pkg_idx}_{mod_idx}_{func_idx}(x: str, y: str) -> str:\n    return x + y\n"
                            ),
                            _ => writeln!(
                                src,
                                "def f_{pkg_idx}_{mod_idx}_{func_idx}(n: int) -> int:\n    return n\n"
                            ),
                        }
                        .expect("format function def");
                    }
                    std::fs::write(
                        pkg_dir.join(format!("mod_{mod_idx:03}.py")),
                        src,
                    )
                    .expect("write generated module");
                }
            }

            // `ty_pkg`: base classes, derived classes (inheriting without
            // overriding), and a caller that invokes inherited methods
            // positionally.
            let ty_pkg = root.join("ty_pkg");
            std::fs::create_dir_all(&ty_pkg).expect("create ty_pkg/");
            std::fs::write(ty_pkg.join("__init__.py"), "")
                .expect("write ty_pkg/__init__.py");

            let mut base_src =
                String::from("\"\"\"Base classes with typed methods.\"\"\"\n\n");
            let mut derived_src = String::from(
                "\"\"\"Derived classes inheriting without overriding.\"\"\"\n\nfrom ty_pkg.base import ",
            );
            for i in 0..WHOLE_PROJECT_TY_INHERITED {
                if i > 0 {
                    derived_src.push_str(", ");
                }
                write!(derived_src, "Base{i}").expect("format derived import");
            }
            derived_src.push('\n');
            let mut caller_src = String::from(
                "\"\"\"Calls inherited methods positionally.\"\"\"\n\nfrom ty_pkg.derived import ",
            );
            for i in 0..WHOLE_PROJECT_TY_INHERITED {
                if i > 0 {
                    caller_src.push_str(", ");
                }
                write!(caller_src, "Derived{i}").expect("format caller import");
            }
            caller_src.push('\n');

            for i in 0..WHOLE_PROJECT_TY_INHERITED {
                writeln!(
                    base_src,
                    "class Base{i}:\n    def method{i}(self, alpha: int, beta: int) -> int:\n        return alpha + beta\n"
                )
                .expect("format base class");
                writeln!(derived_src, "\nclass Derived{i}(Base{i}):\n    pass\n")
                    .expect("format derived class");
                writeln!(caller_src, "Derived{i}().method{i}(1, 2)")
                    .expect("format caller call");
            }
            std::fs::write(ty_pkg.join("base.py"), base_src)
                .expect("write ty_pkg/base.py");
            std::fs::write(ty_pkg.join("derived.py"), derived_src)
                .expect("write ty_pkg/derived.py");
            std::fs::write(ty_pkg.join("caller.py"), caller_src)
                .expect("write ty_pkg/caller.py");

            // app.py: calls every first-party function positionally.
            let mut app =
                String::from("\"\"\"Entry point that exercises every first-party package.\"\"\"\n\n");
            for pkg_idx in 0..WHOLE_PROJECT_TY_PKGS {
                for mod_idx in 0..WHOLE_PROJECT_TY_MODS_PER_PKG {
                    for func_idx in 0..WHOLE_PROJECT_TY_FUNCS_PER_MOD {
                        let pkg = format!("pkg_{pkg_idx}");
                        let mod_ = format!("mod_{mod_idx:03}");
                        let func = format!("f_{pkg_idx}_{mod_idx}_{func_idx}");
                        writeln!(app, "from {pkg}.{mod_} import {func}")
                            .expect("format import");
                    }
                }
            }
            app.push('\n');
            for pkg_idx in 0..WHOLE_PROJECT_TY_PKGS {
                for mod_idx in 0..WHOLE_PROJECT_TY_MODS_PER_PKG {
                    for func_idx in 0..WHOLE_PROJECT_TY_FUNCS_PER_MOD {
                        let func = format!("f_{pkg_idx}_{mod_idx}_{func_idx}");
                        match func_idx % 3 {
                            0 => writeln!(app, "{func}(1, 2, 3)"),
                            1 => writeln!(app, "{func}(\"a\", \"b\")"),
                            _ => writeln!(app, "{func}(42)"),
                        }
                        .expect("format call");
                    }
                }
            }
            std::fs::write(root.join("app.py"), app).expect("write app.py");

            temp
        })
        .path()
}

/// Whole-project run with a mix of first-party files and inherited-method
/// calls (issue #67 / #135).
#[divan::bench]
fn whole_project_ty() -> usize {
    check(whole_project_ty_dir())
}

/// The auto-fixer shares the index/parse/walk path with `check` and now runs
/// the same detection, but adds the positional → keyword rewrite; it is
/// tracked separately. The fixture is fully resolvable by the built-in
/// resolver, so the ty fallback never starts (lazy) and the numbers stay
/// deterministic.
#[divan::bench]
fn fix_first_party_closure() -> usize {
    let root = first_party_project();
    let config = Config::load(root).expect("valid benchmark-fixture config");
    let paths = [root.to_path_buf()];
    fix_paths(root, &paths, &config, None)
        .expect("fix_paths over a benchmark fixture must succeed")
        .files
        .len()
}

/// The scheduled full-checkout workflow runs `strict-kwargs fix --diff`.
/// This keeps the same hermetic large generated project as [`large_project`],
/// but includes fix planning, fixed-source validation, and unified diff
/// rendering. It still cannot model `CPython`'s exact source distribution, but
/// it is a much closer benchmark for the dry-run command shape than `check`.
#[divan::bench]
fn fix_large_project_diff() -> usize {
    let root = large_project_dir();
    let config = Config::load(root).expect("valid benchmark-fixture config");
    let paths = [root.to_path_buf()];
    let outcome = fix_paths(root, &paths, &config, None)
        .expect("fix_paths over a benchmark fixture must succeed");
    outcome
        .files
        .iter()
        .map(|fix| unified_diff(&fix.path, &fix.original, &fix.fixed, false).len())
        .sum()
}

/// `fix --diff` over a project where many calls require `ty` call-site hovers.
/// This is the closest `CodSpeed` signal to a real checkout whose wall time is
/// dominated by the inference fallback rather than by the built-in resolver.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn fix_ty_fallback_diff() -> usize {
    let root = ty_fallback_project_dir();
    let config = Config::load(root).expect("valid benchmark-fixture config");
    let paths = [root.to_path_buf()];
    let outcome = fix_paths(root, &paths, &config, None)
        .expect("fix_paths over a ty-fallback benchmark fixture must succeed");
    outcome
        .files
        .iter()
        .map(|fix| unified_diff(&fix.path, &fix.original, &fix.fixed, false).len())
        .sum()
}

/// Opt-in real-checkout benchmark for `CPython`. This is ignored unless
/// [`CPYTHON_BENCH_CHECKOUT_ENV`] points at an existing checkout, because
/// cloning and scanning `CPython` is too expensive for ordinary local
/// `cargo bench` and every-PR `CodSpeed` runs.
#[divan::bench(
    ignore = cpython_benchmark_checkout().is_none(),
    sample_count = 1,
    sample_size = 1
)]
fn cpython_fix_diff() -> usize {
    let root = cpython_benchmark_checkout()
        .expect("CPython benchmark checkout env should exist when benchmark is enabled");
    let config = Config::load(&root).expect("valid CPython benchmark config");
    let paths = [root.clone()];
    let outcome = fix_paths(&root, &paths, &config, None)
        .expect("fix_paths over the CPython benchmark checkout must succeed");
    outcome
        .files
        .iter()
        .map(|fix| unified_diff(&fix.path, &fix.original, &fix.fixed, false).len())
        .sum()
}

// ---------------------------------------------------------------------------
// Cache benchmarks (issue #68)
//
// Each pair measures the same fixture cold (no cache, same as the existing
// bench) vs warm (all entries already in cache, so Phase 1 + Phase 2 are
// fully skipped — only fingerprint hashing + cache reads remain).
// ---------------------------------------------------------------------------

/// Run `check_paths` with a cache directory.
fn check_cached(root: &Path, cache_dir: &Path) -> usize {
    let config = Config::load(root).expect("valid benchmark-fixture config");
    let paths = [root.to_path_buf()];
    check_paths(root, &paths, &config, None, Some(cache_dir))
        .expect("check_paths must succeed")
        .len()
}

/// First-party closure — cold run with cache infrastructure enabled (measures
/// global-fingerprint + key-computation overhead on top of the normal scan).
#[divan::bench]
fn first_party_closure_cache_cold(bencher: divan::Bencher) {
    let root = first_party_project();
    bencher.bench(|| {
        let cache = tempfile::tempdir().expect("tempdir");
        check_cached(root, cache.path())
    });
}

/// First-party closure — warm run (cache fully populated before measurement).
/// Measures only fingerprint hashing + cache-file reads; Phase 1 and Phase 2
/// are completely skipped.
#[divan::bench]
fn first_party_closure_cache_warm(bencher: divan::Bencher) {
    let root = first_party_project();
    let cache = tempfile::tempdir().expect("tempdir");
    // Prime: one cold run to populate every cache entry.
    check_cached(root, cache.path());
    bencher.bench(|| check_cached(root, cache.path()));
}

/// Whole-project directory — cold run with cache infrastructure enabled.
#[divan::bench]
fn whole_project_cache_cold(bencher: divan::Bencher) {
    let root = whole_project_dir();
    bencher.bench(|| {
        let cache = tempfile::tempdir().expect("tempdir");
        check_cached(root, cache.path())
    });
}

/// Whole-project directory — warm run (cache fully populated before measurement).
#[divan::bench]
fn whole_project_cache_warm(bencher: divan::Bencher) {
    let root = whole_project_dir();
    let cache = tempfile::tempdir().expect("tempdir");
    // Prime: one cold run to populate every cache entry.
    check_cached(root, cache.path());
    bencher.bench(|| check_cached(root, cache.path()));
}
