//! `archdots-core` — pure logic library for the archdots dotfile manager.
//!
//! This crate contains no UI or terminal I/O. Every public item is unit-testable
//! in isolation using [`tempfile::TempDir`] for filesystem operations.

#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(missing_docs)]

pub mod detector;
pub mod error;
pub mod journal;
pub mod linker;
pub mod lock;
pub mod profile;
pub mod snapshot;

pub use error::CoreError;

/// Convenience alias for results returned by core operations.
pub type Result<T> = std::result::Result<T, CoreError>;
