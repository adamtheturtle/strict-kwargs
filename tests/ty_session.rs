//! Regression test for the `ty` fallback's session model.
//!
//! A `ty server` is an incremental analysis engine whose resolution
//! completeness grows with the work it has already done. Resolving a
//! whole project through one shared session is therefore a *correctness*
//! requirement, not just a performance choice: splitting the files across
//! several independent sessions silently drops violations that only a warm
//! whole-project session resolves. That divergence does not show up on small
//! fixtures (every session is equally cold) — it only appears at real-codebase
//! scale, which is exactly why it can slip through byte-for-byte comparisons on
//! toy projects. This test pins the invariant directly instead: a whole-project
//! check must start exactly one `ty` session.

use strict_kwargs::{check_paths, ty_sessions_started, Config};

/// Checking a multi-file project that defers calls to `ty` must use a single
/// shared `ty` session, regardless of how many files there are.
///
/// This is its own test binary so the process-global session counter reflects
/// only this check (no other ty-backed test runs alongside it here).
#[test]
fn whole_project_check_uses_a_single_ty_session() {
    let dir = tempfile::Builder::new()
        .prefix("strictkw-ty-session")
        .tempdir()
        .expect("tempdir");
    let root = dir.path();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0\"\n",
    )
    .expect("write pyproject");

    // Enough files that a per-worker-session implementation would fan them out
    // across several `ty server` instances (the parallel pool is bounded but
    // still > 1 on any multi-core host). Each file calls a method on an instance
    // whose type only `ty` can infer (from `make`'s return annotation): the
    // built-in resolver cannot resolve `obj.greet`, so it defers the call to the
    // ty fallback and a session is started.
    for i in 0..50 {
        std::fs::write(
            root.join(format!("mod{i}.py")),
            "class Thing:\n    def greet(self, a, b): ...\n\n\n\
             def make() -> Thing:\n    return Thing()\n\n\n\
             obj = make()\nobj.greet(1, 2)\n",
        )
        .expect("write module");
    }

    let before = ty_sessions_started();
    let config = Config::load(root).expect("valid config");
    check_paths(root, &[root.to_path_buf()], &config, None, None)
        .expect("check succeeds with ty present");
    let started = ty_sessions_started() - before;

    // Sanity: the fixture actually exercised the ty fallback (these inferred
    // method calls are deferred to ty). Without this the equality below could
    // pass vacuously if nothing reached ty.
    assert!(started >= 1, "expected the ty fallback to be exercised");
    assert_eq!(
        started, 1,
        "the ty fallback must resolve the whole project through one shared \
         session; started {started} sessions instead, which drops violations \
         that only a warm whole-project session resolves"
    );
}
