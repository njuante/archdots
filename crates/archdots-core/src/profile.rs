//! Profile schema, serialization, and path resolution.
//!
//! A *profile* is a named rice configuration stored as a `profile.toml` file
//! inside a profile directory, e.g.
//! `$XDG_DATA_HOME/archdots/profiles/minimal-bspwm/profile.toml`.
//!
//! # Storage model
//!
//! * [`FileEntry::source`] is always **relative to the profile directory**.
//!   Call [`Profile::resolve_source`] at apply time to obtain an absolute path.
//! * [`FileEntry::target`] is stored **unexpanded** (may contain `~` or
//!   `$VAR`/`${VAR}`).  Call [`Profile::resolve_target`] with a [`ResolveCtx`]
//!   to expand it.  This keeps the profile portable (shareable on GitHub) and
//!   testable without touching `$HOME`.

// ProfileMeta, ProfileError etc. follow Rust naming convention for module items.
#![allow(clippy::module_name_repetitions)]

use std::collections::HashSet;
use std::fmt;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ProfileError;

/// Schema version understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

// ── top-level struct ──────────────────────────────────────────────────────────

/// A complete rice profile, corresponding 1:1 to a `profile.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    /// Must equal [`CURRENT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Human-readable metadata for this profile.
    pub profile: ProfileMeta,
    /// Window manager hint (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wm: Option<WmSpec>,
    /// Files to link / copy / template.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
    /// Package dependencies.
    #[serde(default)]
    pub dependencies: Dependencies,
    /// Lifecycle hooks.
    #[serde(default)]
    pub hooks: Hooks,
}

// ── sub-types ─────────────────────────────────────────────────────────────────

/// Human-readable metadata stored under the `[profile]` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileMeta {
    /// Profile identifier. Must match `[a-z0-9_-]+`.
    pub name: String,
    /// Optional one-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional author name or handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Optional RFC 3339 creation timestamp (stored as plain string; no chrono dep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Arbitrary classification tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Window-manager specification under the `[wm]` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WmSpec {
    /// Which window manager this profile targets.
    pub kind: WmKind,
    /// Optional minimum version string (free-form, e.g. `">=0.9"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Supported window managers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
pub enum WmKind {
    /// bspwm
    Bspwm,
    /// Hyprland
    Hyprland,
    /// i3
    I3,
    /// sway
    Sway,
    /// WM-agnostic profile.
    None,
}

/// One file / directory managed by this profile (`[[files]]` array entry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    /// Unique identifier within this profile (used in logs and the journal).
    pub id: String,
    /// Path **relative to the profile directory**.  No leading `/`, no `..`
    /// that escapes the directory.
    pub source: PathBuf,
    /// Destination path, **unexpanded**.  May contain `~`, `$VAR`, `${VAR}`.
    pub target: String,
    /// How to place the file at the target location.
    #[serde(default)]
    pub mode: LinkMode,
    /// If `true`, `chmod +x` the target after linking/copying.
    #[serde(default)]
    pub exec: bool,
    /// If `true` (default), `apply` fails when this entry cannot be linked.
    #[serde(default = "default_true")]
    pub required: bool,
}

/// How a [`FileEntry`] is placed at its target location.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Create a symbolic link (default).
    #[default]
    Symlink,
    /// Copy the file instead of linking.
    Copy,
    /// Render the file as a template before linking/copying (post-MVP).
    Template,
}

/// Package dependencies for this profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependencies {
    /// Required packages from the official repos (`pacman -S`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pacman: Vec<String>,
    /// Required AUR packages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aur: Vec<String>,
    /// Nice-to-have packages from the official repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_pacman: Vec<String>,
}

/// Lifecycle hooks (paths relative to the profile directory).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hooks {
    /// Scripts to run before `apply`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_apply: Vec<PathBuf>,
    /// Scripts to run after a successful `apply`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_apply: Vec<PathBuf>,
}

/// Context injected when resolving unexpanded target paths.
///
/// Using a struct instead of direct `std::env::var` calls makes all path
/// resolution testable without touching the real environment.
pub struct ResolveCtx<'a> {
    /// Caller's home directory (substituted for leading `~`).
    pub home: &'a Path,
    /// Resolver for `$VAR` / `${VAR}` references.  Returns `None` when the
    /// variable is unknown.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

// ── ResolveCtx constructors ───────────────────────────────────────────────────

impl<'a> ResolveCtx<'a> {
    /// Build a context that resolves `~` to `home` and all `$VAR` / `${VAR}`
    /// references via the real process environment.
    #[must_use]
    pub fn with_home(home: &'a Path) -> Self {
        Self {
            home,
            env: &|var| std::env::var(var).ok(),
        }
    }
}

// ── main impl ─────────────────────────────────────────────────────────────────

impl Profile {
    /// Load and validate a profile from a TOML file.
    ///
    /// # Errors
    ///
    /// * [`ProfileError::Io`] — the file could not be read.
    /// * [`ProfileError::ParseToml`] — the file is not valid TOML or does not
    ///   match the expected schema.
    /// * Any error returned by [`Profile::validate`].
    pub fn load_from_file(path: &Path) -> Result<Self, ProfileError> {
        let content = std::fs::read_to_string(path).map_err(|e| ProfileError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let profile: Self = toml::from_str(&content)?;
        profile.validate()?;
        Ok(profile)
    }

    /// Serialize and atomically write this profile to `path`.
    ///
    /// Content is written to an unnamed temporary file in the same directory,
    /// then renamed over `path`.  Concurrent writers each get their own tmp
    /// file, so they cannot corrupt each other.
    ///
    /// # Errors
    ///
    /// * [`ProfileError::SerializeToml`] — serialization failed.
    /// * [`ProfileError::Io`] — the tmp file could not be created/written, or
    ///   the rename failed.
    pub fn save_to_file(&self, path: &Path) -> Result<(), ProfileError> {
        let content = toml::to_string_pretty(self)?;

        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| ProfileError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        tmp.write_all(content.as_bytes())
            .map_err(|e| ProfileError::Io {
                path: tmp.path().to_path_buf(),
                source: e,
            })?;
        tmp.persist(path).map_err(|e| ProfileError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
        Ok(())
    }

    /// Validate the profile contents.
    ///
    /// This is called automatically by [`Profile::load_from_file`].  It is
    /// also public so callers can validate a profile constructed in memory
    /// (e.g. `archdots init`) before persisting it.
    ///
    /// # Errors
    ///
    /// * [`ProfileError::UnsupportedSchema`] — wrong `schema_version`.
    /// * [`ProfileError::InvalidName`] — name does not match `[a-z0-9_-]+`.
    /// * [`ProfileError::DuplicateFileId`] — two entries share an `id`.
    /// * [`ProfileError::EmptyPath`] — an `id`, `source`, or `target` is empty.
    /// * [`ProfileError::SourceEscape`] — a `source` path escapes the profile
    ///   directory.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedSchema {
                found: self.schema_version,
                expected: CURRENT_SCHEMA_VERSION,
            });
        }

        let name = &self.profile.name;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
        {
            return Err(ProfileError::InvalidName(name.clone()));
        }

        let mut seen_ids: HashSet<&str> = HashSet::new();
        for entry in &self.files {
            if entry.id.is_empty() {
                return Err(ProfileError::EmptyPath {
                    id: String::from("<empty>"),
                    field: "id",
                });
            }
            if !seen_ids.insert(entry.id.as_str()) {
                return Err(ProfileError::DuplicateFileId(entry.id.clone()));
            }
            if entry.source.as_os_str().is_empty() {
                return Err(ProfileError::EmptyPath {
                    id: entry.id.clone(),
                    field: "source",
                });
            }
            if entry.target.is_empty() {
                return Err(ProfileError::EmptyPath {
                    id: entry.id.clone(),
                    field: "target",
                });
            }
            if path_escapes_root(&entry.source) {
                return Err(ProfileError::SourceEscape(entry.source.clone()));
            }
        }

        for hook in self
            .hooks
            .pre_apply
            .iter()
            .chain(self.hooks.post_apply.iter())
        {
            if path_escapes_root(hook) {
                return Err(ProfileError::SourceEscape(hook.clone()));
            }
        }

        Ok(())
    }

    /// Expand `entry.target` (which may contain `~` or `$VAR`/`${VAR}`) to an
    /// absolute [`PathBuf`] using the provided [`ResolveCtx`].
    ///
    /// # Errors
    ///
    /// * [`ProfileError::UnknownEnvVar`] — a `$VAR` reference has no value in
    ///   `ctx.env`.
    /// * [`ProfileError::NonAbsoluteTarget`] — the target is not absolute after
    ///   expansion.
    pub fn resolve_target(
        entry: &FileEntry,
        ctx: &ResolveCtx<'_>,
    ) -> Result<PathBuf, ProfileError> {
        let expanded_vars = expand_vars(&entry.target, ctx)?;

        let path = if expanded_vars == "~" {
            ctx.home.to_path_buf()
        } else if let Some(rest) = expanded_vars.strip_prefix("~/") {
            ctx.home.join(rest)
        } else {
            PathBuf::from(&expanded_vars)
        };

        if !path.is_absolute() {
            return Err(ProfileError::NonAbsoluteTarget(entry.target.clone()));
        }
        Ok(path)
    }

    /// Iterate over all file entries with their resolved `(source, target)` paths.
    ///
    /// Yields `(entry, absolute_source, absolute_target)` for each entry.
    /// Stops and returns the first error if any entry fails to resolve.
    ///
    /// # Errors
    ///
    /// * Any error returned by [`Profile::resolve_source`] or
    ///   [`Profile::resolve_target`].
    pub fn resolved_entries<'a>(
        &'a self,
        profile_dir: &'a Path,
        ctx: &'a ResolveCtx<'a>,
    ) -> impl Iterator<Item = Result<(&'a FileEntry, PathBuf, PathBuf), ProfileError>> + 'a {
        self.files.iter().map(move |entry| {
            let src = Self::resolve_source(entry, profile_dir)?;
            let tgt = Self::resolve_target(entry, ctx)?;
            Ok((entry, src, tgt))
        })
    }

    /// Resolve `entry.source` (relative to `profile_dir`) to an absolute path.
    ///
    /// The path is computed lexically; the target file does not need to exist.
    ///
    /// # Errors
    ///
    /// * [`ProfileError::SourceEscape`] — the source path escapes
    ///   `profile_dir` via `..` components or is absolute.
    pub fn resolve_source(entry: &FileEntry, profile_dir: &Path) -> Result<PathBuf, ProfileError> {
        if path_escapes_root(&entry.source) {
            return Err(ProfileError::SourceEscape(entry.source.clone()));
        }
        Ok(profile_dir.join(&entry.source))
    }
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Returns `true` when `path` contains `..` components that would escape the
/// root, or when the path is absolute.
fn path_escapes_root(path: &Path) -> bool {
    let mut depth: i32 = 0;
    for component in path.components() {
        match component {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::Normal(_) => depth += 1,
            // Absolute paths (RootDir / Prefix) are not valid relative sources.
            Component::RootDir | Component::Prefix(_) => return true,
            Component::CurDir => {}
        }
    }
    false
}

/// Expand `$VAR` and `${VAR}` references in `raw` using `ctx.env`.
fn expand_vars(raw: &str, ctx: &ResolveCtx<'_>) -> Result<String, ProfileError> {
    let chars: Vec<char> = raw.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(raw.len());
    let mut i = 0;

    while i < len {
        if chars[i] != '$' {
            result.push(chars[i]);
            i += 1;
            continue;
        }

        i += 1; // skip '$'

        let braced = i < len && chars[i] == '{';
        if braced {
            i += 1; // skip '{'
        }

        let start = i;
        while i < len {
            let c = chars[i];
            if braced && c == '}' {
                break;
            }
            if !braced && !c.is_alphanumeric() && c != '_' {
                break;
            }
            i += 1;
        }

        let var_name: String = chars[start..i].iter().collect();

        if braced && i < len && chars[i] == '}' {
            i += 1; // skip '}'
        }

        if var_name.is_empty() {
            // Bare `$` or `${}` — emit literally.
            result.push('$');
            if braced {
                result.push_str("{}");
            }
            continue;
        }

        let val = (ctx.env)(&var_name).ok_or_else(|| ProfileError::UnknownEnvVar {
            name: var_name.clone(),
            target: raw.to_string(),
        })?;
        result.push_str(&val);
    }

    Ok(result)
}

fn default_true() -> bool {
    true
}

impl fmt::Display for LinkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Symlink => "symlink",
            Self::Copy => "copy",
            Self::Template => "template",
        })
    }
}

impl fmt::Display for WmKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bspwm => "bspwm",
            Self::Hyprland => "hyprland",
            Self::I3 => "i3",
            Self::Sway => "sway",
            Self::None => "none",
        })
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{expand_vars, path_escapes_root, FileEntry, LinkMode, Profile, ResolveCtx};
    use crate::error::ProfileError;

    fn fake_ctx(home: &std::path::Path) -> ResolveCtx<'_> {
        ResolveCtx {
            home,
            env: &|_| None,
        }
    }

    fn dummy_entry(target: &str) -> FileEntry {
        FileEntry {
            id: String::from("x"),
            source: PathBuf::from("x"),
            target: target.to_string(),
            mode: LinkMode::Symlink,
            exec: false,
            required: true,
        }
    }

    // path_escapes_root

    #[test]
    fn escape_relative_normal() {
        assert!(!path_escapes_root(std::path::Path::new("subdir/file.txt")));
    }

    #[test]
    fn escape_deep_dotdot_valid() {
        // "a/../b" doesn't escape (depth never goes below 0)
        assert!(!path_escapes_root(std::path::Path::new("a/../b")));
    }

    #[test]
    fn escape_dotdot_escapes() {
        assert!(path_escapes_root(std::path::Path::new("../../etc")));
    }

    #[test]
    fn escape_absolute_rejected() {
        assert!(path_escapes_root(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn escape_single_dotdot() {
        assert!(path_escapes_root(std::path::Path::new("..")));
    }

    // expand_vars

    #[test]
    fn expand_no_vars() {
        let home = std::path::PathBuf::from("/home/u");
        let ctx = fake_ctx(&home);
        assert_eq!(
            expand_vars("/absolute/path", &ctx).unwrap(),
            "/absolute/path"
        );
    }

    #[test]
    fn expand_bare_dollar() {
        let home = std::path::PathBuf::from("/home/u");
        let ctx = fake_ctx(&home);
        // Bare $ with no following alnum stays literal.
        assert_eq!(expand_vars("$ ", &ctx).unwrap(), "$ ");
    }

    #[test]
    fn expand_known_var() {
        let home = std::path::PathBuf::from("/home/u");
        let ctx = ResolveCtx {
            home: &home,
            env: &|v| (v == "FOO").then(|| String::from("/bar")),
        };
        assert_eq!(expand_vars("$FOO/baz", &ctx).unwrap(), "/bar/baz");
    }

    #[test]
    fn expand_braced_var() {
        let home = std::path::PathBuf::from("/home/u");
        let ctx = ResolveCtx {
            home: &home,
            env: &|v| (v == "XDG_CONFIG_HOME").then(|| String::from("/cfg")),
        };
        assert_eq!(
            expand_vars("${XDG_CONFIG_HOME}/foo", &ctx).unwrap(),
            "/cfg/foo"
        );
    }

    #[test]
    fn expand_unknown_var_errors() {
        let home = std::path::PathBuf::from("/home/u");
        let ctx = fake_ctx(&home);
        let err = expand_vars("$NOPE/foo", &ctx).unwrap_err();
        assert!(matches!(err, ProfileError::UnknownEnvVar { .. }));
    }

    // resolve_target tilde

    #[test]
    fn resolve_tilde_slash() {
        let home = std::path::PathBuf::from("/home/alice");
        let ctx = fake_ctx(&home);
        let entry = dummy_entry("~/.config/foo");
        assert_eq!(
            Profile::resolve_target(&entry, &ctx).unwrap(),
            std::path::PathBuf::from("/home/alice/.config/foo")
        );
    }

    #[test]
    fn resolve_tilde_only() {
        let home = std::path::PathBuf::from("/home/alice");
        let ctx = fake_ctx(&home);
        let entry = dummy_entry("~");
        assert_eq!(
            Profile::resolve_target(&entry, &ctx).unwrap(),
            std::path::PathBuf::from("/home/alice")
        );
    }
}
