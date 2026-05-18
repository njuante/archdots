//! Detects existing dotfiles and known application config paths in `$HOME`.
//!
//! The list of known paths is embedded at compile time from
//! `data/known_dotfiles.toml` via [`include_str!`].  Constructing a
//! [`Detector`] parses this catalog once; [`Detector::scan`] then walks every
//! entry and checks the filesystem, returning a flat [`Vec<DetectedDotfile>`].
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use archdots_core::detector::Detector;
//!
//! let d = Detector::new(Path::new("/home/alice")).unwrap();
//! for hit in d.scan().into_iter().filter(|e| e.exists) {
//!     println!("{} ({:?}): {}", hit.name, hit.category, hit.path.display());
//! }
//! ```

#![allow(clippy::module_name_repetitions)]

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::DetectorError;

// ── embedded catalog (private) ────────────────────────────────────────────────

const CATALOG_TOML: &str = include_str!("../data/known_dotfiles.toml");

#[derive(Deserialize)]
struct Catalog {
    entry: Vec<KnownEntry>,
}

#[derive(Deserialize)]
struct KnownEntry {
    name: String,
    paths: Vec<String>,
    category: Category,
    #[serde(default)]
    #[allow(dead_code)] // consumed by archdots check (Fase 3), stored for forward compat
    optional_deps: Vec<String>,
}

// ── public types ──────────────────────────────────────────────────────────────

/// Application category used to group dotfiles.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Tiling / floating window manager.
    Wm,
    /// Status bar.
    Bar,
    /// Application launcher.
    Launcher,
    /// Terminal emulator.
    Terminal,
    /// Login or interactive shell.
    Shell,
    /// Text editor.
    Editor,
    /// Shell prompt theme.
    Prompt,
    /// X11 / Wayland compositor.
    Compositor,
    /// Desktop notification daemon.
    Notification,
    /// Everything else.
    Misc,
}

/// One detected dotfile entry returned by [`Detector::scan`].
///
/// A single catalog app can produce multiple [`DetectedDotfile`] entries — one
/// per path listed under that app.  The `exists` field is `false` for paths
/// that were not found on the current system.
#[derive(Debug, Clone)]
pub struct DetectedDotfile {
    /// Application name as listed in the catalog (e.g. `"neovim"`, `"bspwm"`).
    pub name: String,
    /// Coarse category for grouping.
    pub category: Category,
    /// Absolute path that was checked (home-dir prefix already applied).
    pub path: PathBuf,
    /// `true` when the path exists on the filesystem.
    pub exists: bool,
    /// `true` when the path is a symbolic link (`exists` is also `true`).
    pub is_symlink: bool,
    /// Symlink target, populated only when `is_symlink` is `true`.
    pub target_if_link: Option<PathBuf>,
}

/// Scans `$HOME` for known dotfile paths.
///
/// The embedded catalog is parsed once in [`Detector::new`]; after that,
/// [`Detector::scan`] is cheap enough to call repeatedly.
pub struct Detector {
    home: PathBuf,
    entries: Vec<KnownEntry>,
}

impl Detector {
    /// Construct a new detector rooted at `home`.
    ///
    /// Parses the embedded catalog; returns an error only when the catalog is
    /// malformed (should never happen in a released build).
    ///
    /// # Errors
    ///
    /// * [`DetectorError::ParseCatalog`] — the embedded TOML is syntactically
    ///   or structurally invalid.
    pub fn new(home: &Path) -> Result<Self, DetectorError> {
        let catalog: Catalog = toml::from_str(CATALOG_TOML)?;
        Ok(Self {
            home: home.to_path_buf(),
            entries: catalog.entry,
        })
    }

    /// Scan the home directory and return one [`DetectedDotfile`] per
    /// (catalog entry × path) pair.
    ///
    /// Entries whose paths do not exist are included with `exists = false` so
    /// callers have the complete picture of what is known vs. what is installed.
    /// Filesystem errors during probing (e.g. permission denied) are treated
    /// as "not found" rather than propagated.
    #[must_use]
    pub fn scan(&self) -> Vec<DetectedDotfile> {
        let mut results = Vec::new();

        for entry in &self.entries {
            for rel in &entry.paths {
                let abs = self.home.join(rel);
                let (exists, is_symlink, target_if_link) = probe(&abs);
                results.push(DetectedDotfile {
                    name: entry.name.clone(),
                    category: entry.category,
                    path: abs,
                    exists,
                    is_symlink,
                    target_if_link,
                });
            }
        }

        results
    }
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Probe a path without following symlinks.
///
/// Returns `(exists, is_symlink, target)`.  Any I/O error is silently treated
/// as "not found".
fn probe(path: &Path) -> (bool, bool, Option<PathBuf>) {
    match path.symlink_metadata() {
        Ok(meta) => {
            let is_symlink = meta.file_type().is_symlink();
            let target = if is_symlink {
                std::fs::read_link(path).ok()
            } else {
                None
            };
            (true, is_symlink, target)
        }
        Err(_) => (false, false, None),
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{probe, Catalog, Category, Detector, CATALOG_TOML};

    // catalog

    #[test]
    fn catalog_deserializes() {
        let c: Catalog = toml::from_str(CATALOG_TOML).expect("catalog must be valid TOML");
        assert!(!c.entry.is_empty(), "catalog must have at least one entry");
    }

    #[test]
    fn catalog_has_all_categories() {
        let c: Catalog = toml::from_str(CATALOG_TOML).unwrap();
        let cats: std::collections::HashSet<String> = c
            .entry
            .iter()
            .map(|e| format!("{:?}", e.category))
            .collect();
        for expected in [
            "Wm",
            "Bar",
            "Launcher",
            "Terminal",
            "Shell",
            "Editor",
            "Prompt",
            "Compositor",
            "Notification",
            "Misc",
        ] {
            assert!(cats.contains(expected), "missing category {expected}");
        }
    }

    #[test]
    fn catalog_no_empty_names() {
        let c: Catalog = toml::from_str(CATALOG_TOML).unwrap();
        for entry in &c.entry {
            assert!(!entry.name.is_empty(), "entry must have a non-empty name");
            assert!(
                !entry.paths.is_empty(),
                "entry '{}' must have at least one path",
                entry.name
            );
        }
    }

    #[test]
    fn catalog_no_absolute_paths() {
        let c: Catalog = toml::from_str(CATALOG_TOML).unwrap();
        for entry in &c.entry {
            for path in &entry.paths {
                assert!(
                    !path.starts_with('/'),
                    "path '{}' in entry '{}' must be relative",
                    path,
                    entry.name
                );
            }
        }
    }

    // probe

    #[test]
    fn probe_missing_path() {
        let (exists, is_sym, target) =
            probe(std::path::Path::new("/tmp/__archdots_no_such_file__"));
        assert!(!exists);
        assert!(!is_sym);
        assert!(target.is_none());
    }

    #[test]
    fn probe_regular_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let (exists, is_sym, target) = probe(tmp.path());
        assert!(exists);
        assert!(!is_sym);
        assert!(target.is_none());
    }

    #[test]
    fn probe_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::fs::write(&real, b"").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let (exists, is_sym, target) = probe(&link);
        assert!(exists);
        assert!(is_sym);
        assert_eq!(target.unwrap(), real);
    }

    // Detector::new

    #[test]
    fn detector_new_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        Detector::new(dir.path()).expect("Detector::new must succeed with embedded catalog");
    }

    #[test]
    fn detector_scan_count_matches_total_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let d = Detector::new(dir.path()).unwrap();
        let c: Catalog = toml::from_str(CATALOG_TOML).unwrap();
        let total_paths: usize = c.entry.iter().map(|e| e.paths.len()).sum();
        assert_eq!(d.scan().len(), total_paths);
    }

    // Category is Hash + Eq
    #[test]
    fn category_hashable() {
        let mut set = std::collections::HashSet::new();
        set.insert(Category::Wm);
        set.insert(Category::Wm);
        assert_eq!(set.len(), 1);
    }
}
