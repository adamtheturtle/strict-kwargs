//! Fast enforcement of keyword arguments at call sites (no mypy/ty plugin required).

mod ast_util;
mod check;
mod config;
mod diagnostic;
mod error;
mod index;
mod signature;

pub use check::check_paths;
pub use config::{find_project_root, Config};
pub use diagnostic::Diagnostic;
pub use error::CheckError;
