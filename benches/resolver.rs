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

use strict_kwargs::{check_paths, fix_paths, Config};

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
    check_paths(root, &paths, &config, None)
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
