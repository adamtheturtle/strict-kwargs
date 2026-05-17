//! Continuous benchmarks for the resolver hot paths (issue #30).
//!
//! Run locally with `cargo bench`; CI runs the same binary under `CodSpeed`
//! (`.github/workflows/ci.yml`) so every PR gets an instruction-count delta
//! against `main`.
//!
//! The `CodSpeed` job intentionally does **not** install `ty`: `CodSpeed`
//! counts instructions of *this* process, and `ty` work happens in a
//! subprocess that would not be measured anyway. Every fixture is therefore
//! designed to be
//! fully resolvable by the built-in resolver (project code + embedded
//! typeshed), so `ty_pending` stays empty and the measured numbers are the
//! deterministic parse / index / walk / resolve cost — exactly the hot paths
//! every future resolver change touches.

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
    let config = Config::load(root);
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

/// The auto-fixer shares the index/parse/walk path with `check` but runs the
/// positional → keyword rewrite instead of the ty deferral, so it is tracked
/// separately.
#[divan::bench]
fn fix_first_party_closure() -> usize {
    let root = first_party_project();
    let config = Config::load(root);
    let paths = [root.to_path_buf()];
    fix_paths(root, &paths, &config)
        .expect("fix_paths over a benchmark fixture must succeed")
        .len()
}
