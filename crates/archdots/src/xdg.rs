//! XDG Base Directory helpers.
//!
//! Implements the [XDG Base Directory Specification] using only standard
//! environment variables — no external crate needed on Linux.
//!
//! [XDG Base Directory Specification]: https://specifications.freedesktop.org/basedir-spec/latest/

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Returns `$HOME` as an absolute path.
pub fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .context("$HOME is not set")
        .map(PathBuf::from)
}

/// Returns the XDG config base directory.
///
/// Uses `$XDG_CONFIG_HOME` when it is set **and absolute**; falls back to
/// `$HOME/.config` otherwise (per the XDG spec).
pub fn config_home() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
        let p = PathBuf::from(&val);
        if p.is_absolute() {
            return Ok(p);
        }
        tracing::warn!(
            value = %val,
            "$XDG_CONFIG_HOME is not absolute — ignoring and falling back to $HOME/.config"
        );
    }
    home_dir().map(|h| h.join(".config"))
}

/// Returns the archdots profiles directory:
/// `<config_home>/archdots/profiles`.
pub fn profiles_dir() -> Result<PathBuf> {
    config_home().map(|c| c.join("archdots").join("profiles"))
}

/// Returns the XDG data base directory.
///
/// Uses `$XDG_DATA_HOME` when set and absolute; falls back to
/// `$HOME/.local/share`.
pub fn data_home() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("XDG_DATA_HOME") {
        let p = PathBuf::from(&val);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    home_dir().map(|h| h.join(".local").join("share"))
}

/// Returns the XDG state base directory.
///
/// Uses `$XDG_STATE_HOME` when set and absolute; falls back to
/// `$HOME/.local/state`.
pub fn state_home() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("XDG_STATE_HOME") {
        let p = PathBuf::from(&val);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    home_dir().map(|h| h.join(".local").join("state"))
}
