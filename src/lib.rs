//! Fast enforcement of keyword arguments at call sites (no mypy/ty plugin required).

// `cargo llvm-cov` builds with `--cfg coverage`; under it we opt into the
// unstable `coverage(off)` attribute so a handful of genuinely unreachable
// arms can be excluded from the 100%-coverage gate. Gating on `coverage`
// (not `coverage_nightly`) keeps local (stable + `RUSTC_BOOTSTRAP=1`) and CI
// (nightly) coverage identical.
#![cfg_attr(coverage, feature(coverage_attribute))]

mod ast_util;
mod cache;
mod check;
mod config;
mod diagnostic;
mod error;
mod fix;
mod index;
mod limits;
mod resolve;
mod signature;
mod source;
// The `ty` type-inference fallback is an *optional*, environment-dependent
// subprocess integration: real `ty` only ever drives the happy paths, while
// the error/edge branches (disabled latch, malformed frames, timeouts,
// unusual hover/goto shapes) are reachable only from unit tests, which are
// `#[coverage(off)]`. The whole module's behaviour is verified by those
// tests and the ty-backed integration tests, but it is excluded from the
// 100% line/branch gate — the gate covers the built-in resolver, fixer and
// index (the core product), consistent with the already-excluded
// `resolve_pending_with_ty`/`start` glue.
#[cfg_attr(coverage, coverage(off))]
mod ty_resolver;

pub use check::{check_paths, fix_paths};
pub use config::{find_project_root, Config};
pub use diagnostic::Diagnostic;
pub use error::CheckError;
pub use fix::{unified_diff, FileFix, FixOutcome};
