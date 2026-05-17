//! Fast enforcement of keyword arguments at call sites (no mypy/ty plugin required).

// `cargo llvm-cov` builds with `--cfg coverage`; under it we opt into the
// unstable `coverage(off)` attribute so a handful of genuinely unreachable
// arms can be excluded from the 100%-coverage gate. Gating on `coverage`
// (not `coverage_nightly`) keeps local (stable + `RUSTC_BOOTSTRAP=1`) and CI
// (nightly) coverage identical.
#![cfg_attr(coverage, feature(coverage_attribute))]

mod ast_util;
mod check;
mod config;
mod diagnostic;
mod error;
mod fix;
mod index;
mod resolve;
mod signature;
mod ty_resolver;

pub use check::{check_paths, fix_paths};
pub use config::{find_project_root, Config};
pub use diagnostic::Diagnostic;
pub use error::CheckError;
pub use fix::{unified_diff, FileFix};
