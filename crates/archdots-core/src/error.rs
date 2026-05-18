//! Typed errors for archdots-core using `thiserror`.

use thiserror::Error;

/// Top-level error type for all core operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// I/O error wrapping [`std::io::Error`].
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
