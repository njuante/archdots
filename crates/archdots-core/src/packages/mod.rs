//! Package management queries via `pacman`.
//!
//! The central type is [`PackageDB`], which wraps `pacman -Q`, `pacman -Qm`,
//! and `pacman -F` with lazy caching. Tests inject a [`CommandRunner`] via
//! [`PackageDB::with_runner`] instead of spawning real processes.

mod db;
mod providers;
mod runner;

pub use crate::error::PackageError;
pub use db::{AurHelper, PackageDB, Pkg, PkgSource, ProviderHit};
pub use runner::{CommandOutput, CommandRunner, SystemRunner};
