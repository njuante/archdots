//! Export a profile into a publishable directory.
//!
//! `Exporter` is read-only on the user's `$XDG_DATA_HOME` / `$XDG_STATE_HOME`
//! and never invokes pacman / aur / network. The pipeline is `plan` → `scan` →
//! `write`. The caller (binary) decides whether to gate `write` on
//! confirmation.

#![allow(clippy::module_name_repetitions)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ExportError;
use crate::profile::Profile;

mod glob;

// ── public types ──────────────────────────────────────────────────────────────

/// Drives the export pipeline for a single profile.
///
/// Holds borrows of the profile and relevant paths; lifetime ends with the
/// call. No XDG state is written. No lock is acquired.
pub struct Exporter<'a> {
    profile: &'a Profile,
    profile_dir: &'a Path,
    home: &'a Path,
    /// Parsed entries from the embedded `sensitive_paths.toml`.
    /// Each entry is `(rule_id, kind, pattern_string)`.
    sensitive_paths: Vec<(String, SensitivePathKind, String)>,
}

/// A fully-planned (but not yet written) export: every profile entry
/// classified and, after `scan`, annotated with secret findings.
#[derive(Debug, Clone, Serialize)]
pub struct ExportPlan {
    /// Name from the profile metadata.
    pub profile_name: String,
    /// One item per `FileEntry` in the profile, in declaration order.
    pub items: Vec<PlannedExportItem>,
    /// The options that were in effect when this plan was built.
    pub options: ExportOptions,
}

/// Classification and metadata for one [`crate::profile::FileEntry`] in an
/// [`ExportPlan`].
#[derive(Debug, Clone, Serialize)]
pub struct PlannedExportItem {
    /// Corresponds to [`crate::profile::FileEntry::id`].
    pub entry_id: String,
    /// Absolute source path (pre-`canonicalize`; the symlink path if applicable).
    pub source: PathBuf,
    /// Absolute canonical source path (`canonicalize(source)`).
    /// `None` when the source is a broken symlink or does not exist.
    pub source_canonical: Option<PathBuf>,
    /// Absolute resolved target path.
    pub target: PathBuf,
    /// Path inside the export output (`dotfiles/<rel-to-home>`).
    /// `None` for items that are not `Include`.
    pub rel_in_repo: Option<PathBuf>,
    /// How this item was classified at plan time.
    pub classification: ItemClassification,
    /// Secret findings from the scanner. Empty until [`Exporter::scan`] runs.
    pub findings: Vec<SecretFinding>,
    /// File size in bytes of the effective (canonical) source. `None` when
    /// the source is missing or the size was not read (e.g. early exclusion).
    pub size_bytes: Option<u64>,
    /// `Some(true)` = passed the text sniff; `Some(false)` = binary;
    /// `None` = not sniffed (excluded before the binary check).
    pub is_text: Option<bool>,
}

/// Classification of a single export item.
///
/// All variants are struct-form (including zero-field ones) so that serde
/// external tagging always emits an object — never a bare string. This is a
/// stable JSON contract from v0.5.0; see the design §8.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemClassification {
    /// File is eligible to be copied into the export.
    Include {},
    /// Source did not exist at plan time.
    MissingSource {},
    /// Target resolved outside `$HOME`; the export refuses to ship it.
    OutsideHome {},
    /// Path (target or canonicalized source) matched the sensitive-path denylist.
    ExcludeSensitivePath {
        /// Rule identifier from `sensitive_paths.toml`.
        rule_id: String,
        /// Which match kind triggered the rule.
        kind: SensitivePathKind,
    },
    /// File size exceeds `max_bytes`.
    ExcludeBySize {
        /// Actual file size in bytes.
        size_bytes: u64,
        /// The configured limit that was exceeded.
        limit_bytes: u64,
    },
    /// File's first 8 KiB did not pass the UTF-8 / mostly-printable sniff.
    ExcludeBinary {},
    /// Source is a symlink whose target is missing or not readable.
    ExcludeBrokenSymlink {},
    /// Source resolved to a directory (recursive include is out of scope for
    /// v0.5; see design edge case #16).
    ExcludeDirectory {},
}

/// Which kind of pattern in the sensitive-path denylist triggered a match.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SensitivePathKind {
    /// Path starts with the pattern (e.g. `.ssh/`).
    Prefix,
    /// Path equals the pattern exactly (e.g. `.netrc`).
    Exact,
    /// Path ends with the pattern (e.g. `.pem`).
    Suffix,
}

/// Options controlling what the export pipeline includes or skips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOptions {
    /// Output format (`full` or `profile-only`).
    pub format: ExportFormat,
    /// Raw globs from `--allow-path`; paths matching any of these are exempt
    /// from the sensitive-path denylist.
    pub allow_paths: Vec<String>,
    /// Raw globs from `--allow-binary`; paths matching any are exempt from the
    /// binary-content filter.
    pub allow_binary: Vec<String>,
    /// Per-rule (and optionally per-path) secret-scanner allowances from
    /// `--allow-secret`.
    pub allow_secret_rules: Vec<SecretAllowance>,
    /// Maximum file size in bytes. Files larger than this are excluded.
    pub max_bytes: u64,
    /// When `true`, the content-scan abort for `High` findings is suppressed.
    /// Requires a typed interactive confirmation; not implied by `--yes`.
    pub include_secrets: bool,
    /// Whether to emit `install.sh` in the output directory.
    pub include_install_script: bool,
    /// Whether to emit `README.md` in the output directory.
    pub include_readme: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Full,
            allow_paths: Vec::new(),
            allow_binary: Vec::new(),
            allow_secret_rules: Vec::new(),
            max_bytes: 1_048_576,
            include_secrets: false,
            include_install_script: true,
            include_readme: true,
        }
    }
}

/// An owned snapshot of [`ExportOptions`] captured at plan time.
pub type ExportOptionsSnapshot = ExportOptions;

/// A single per-rule (optionally per-path) allowance for the secret scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretAllowance {
    /// Rule identifier to suppress (e.g. `"jwt"`).
    pub rule_id: String,
    /// Optional path glob; when `None`, the rule is suppressed everywhere.
    pub path_glob: Option<String>,
}

/// Export output format.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// Full publishable export: `README.md`, `dotfiles/`, `archdots-profile.toml`,
    /// `install.sh`, `.gitignore`.
    #[default]
    Full,
    /// Metadata-only export: `README.md` + `archdots-profile.toml`. No dotfile
    /// copies. Sources in the profile are left as-is.
    ProfileOnly,
}

/// Summary statistics produced by a completed [`Exporter::write`] call.
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    /// The path the export was written to.
    pub output_dir: PathBuf,
    /// Number of items classified as [`ItemClassification::Include`].
    pub items_included: usize,
    /// Number of items excluded by the sensitive-path filter.
    pub items_excluded_by_path: usize,
    /// Number of items excluded because they exceeded `max_bytes`.
    pub items_excluded_by_size: usize,
    /// Number of items excluded as binary.
    pub items_excluded_binary: usize,
    /// Number of items with a missing source.
    pub items_missing: usize,
    /// Number of `High`-severity secret findings.
    pub findings_high: usize,
    /// Number of `Medium`-severity secret findings.
    pub findings_medium: usize,
    /// Number of findings that were overridden by `--allow-secret` or
    /// `--include-secrets`.
    pub findings_overridden: usize,
    /// Total bytes written to `output_dir` (excluding the staging rename).
    pub bytes_written: u64,
}

/// One secret-scanner hit on a planned export item.
///
/// Populated by [`Exporter::scan`] (Sesión 3); empty after `plan` alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFinding {
    /// Rule identifier that triggered (e.g. `"aws-access-key-id"`).
    pub rule_id: String,
    /// Severity as declared in `secret_patterns.toml`.
    pub severity: SecretSeverity,
    /// 1-based line number of the match.
    pub line: u32,
    /// 1-based column number of the match.
    pub column: u32,
    /// Redacted preview: first 3 chars + `"..."` + last 3 chars + ` (N chars)`.
    pub preview: String,
}

/// Severity of a secret-scanner finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSeverity {
    /// High-severity; aborts the export unless overridden.
    High,
    /// Medium-severity; warns and proceeds after confirmation.
    Medium,
}

// ── private catalog types ─────────────────────────────────────────────────────

const SENSITIVE_PATHS_TOML: &str = include_str!("../../data/sensitive_paths.toml");

#[derive(Deserialize)]
struct SensitivePathCatalog {
    #[serde(default)]
    prefix: Vec<PrefixRule>,
    #[serde(default)]
    exact: Vec<ExactRule>,
    #[serde(default)]
    suffix: Vec<SuffixRule>,
}

#[derive(Deserialize)]
struct PrefixRule {
    id: String,
    path: String,
}

#[derive(Deserialize)]
struct ExactRule {
    id: String,
    path: String,
}

#[derive(Deserialize)]
struct SuffixRule {
    id: String,
    ext: String,
}

// ── impl ──────────────────────────────────────────────────────────────────────

impl<'a> Exporter<'a> {
    /// Construct an `Exporter` for `profile`.
    ///
    /// Parses the embedded sensitive-path catalog.
    ///
    /// # Parameters
    ///
    /// - `profile` — the profile to export.
    /// - `profile_dir` — directory containing the profile's dotfiles (sources).
    /// - `home` — the user's home directory; used for target resolution and
    ///   relative-path calculations.
    ///
    /// # Panics
    ///
    /// Panics if the embedded `sensitive_paths.toml` is malformed. This
    /// indicates a programming error in the data file and should never occur in
    /// a release build.
    #[must_use]
    pub fn new(profile: &'a Profile, profile_dir: &'a Path, home: &'a Path) -> Self {
        let catalog: SensitivePathCatalog =
            toml::from_str(SENSITIVE_PATHS_TOML).expect("embedded sensitive_paths.toml is valid");

        let mut sensitive_paths: Vec<(String, SensitivePathKind, String)> =
            Vec::with_capacity(catalog.prefix.len() + catalog.exact.len() + catalog.suffix.len());

        for r in catalog.prefix {
            sensitive_paths.push((r.id, SensitivePathKind::Prefix, r.path));
        }
        for r in catalog.exact {
            sensitive_paths.push((r.id, SensitivePathKind::Exact, r.path));
        }
        for r in catalog.suffix {
            sensitive_paths.push((r.id, SensitivePathKind::Suffix, r.ext));
        }

        Self {
            profile,
            profile_dir,
            home,
            sensitive_paths,
        }
    }

    /// Build a plan without reading file contents.
    ///
    /// Stats each source (`lstat` + `canonicalize`), classifies it against the
    /// embedded sensitive-path and size / binary filters, and records the final
    /// repo-relative path. Never writes anything to disk.
    ///
    /// # Errors
    ///
    /// - [`ExportError::Profile`] when target path resolution fails.
    /// - [`ExportError::Io`] when stat-ing a source fails for an *unexpected*
    ///   reason, or when the source is missing / a broken symlink and
    ///   `entry.required == true`.
    pub fn plan(&self, opts: &ExportOptions) -> Result<ExportPlan, ExportError> {
        let ctx = crate::profile::ResolveCtx::with_home(self.home);
        let mut items = Vec::with_capacity(self.profile.files.len());

        for entry in &self.profile.files {
            items.push(self.plan_entry(entry, &ctx, opts)?);
        }

        Ok(ExportPlan {
            profile_name: self.profile.profile.name.clone(),
            items,
            options: opts.clone(),
        })
    }

    // ── private helpers ───────────────────────────────────────────────────────

    #[allow(clippy::too_many_lines)]
    fn plan_entry(
        &self,
        entry: &crate::profile::FileEntry,
        ctx: &crate::profile::ResolveCtx<'_>,
        opts: &ExportOptions,
    ) -> Result<PlannedExportItem, ExportError> {
        let source = Profile::resolve_source(entry, self.profile_dir)?;
        let target = Profile::resolve_target(entry, ctx)?;
        let entry_id = entry.id.clone();

        // 1. Target must be inside $HOME.
        let rel_target = match target.strip_prefix(self.home) {
            Ok(r) => r.to_path_buf(),
            Err(_) => {
                return Ok(PlannedExportItem {
                    entry_id,
                    source,
                    source_canonical: None,
                    target,
                    rel_in_repo: None,
                    classification: ItemClassification::OutsideHome {},
                    findings: Vec::new(),
                    size_bytes: None,
                    is_text: None,
                });
            }
        };

        // 2. lstat the source.
        let lstat = match std::fs::symlink_metadata(&source) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if entry.required {
                    return Err(ExportError::Io {
                        path: source,
                        source: e,
                    });
                }
                return Ok(PlannedExportItem {
                    entry_id,
                    source,
                    source_canonical: None,
                    target,
                    rel_in_repo: None,
                    classification: ItemClassification::MissingSource {},
                    findings: Vec::new(),
                    size_bytes: None,
                    is_text: None,
                });
            }
            Err(e) => {
                return Err(ExportError::Io {
                    path: source,
                    source: e,
                })
            }
            Ok(m) => m,
        };

        // 3. Resolve symlinks / detect directories.
        let (source_canonical, effective_size) = if lstat.file_type().is_symlink() {
            match std::fs::canonicalize(&source) {
                Err(e) => {
                    // Broken symlink.
                    if entry.required {
                        return Err(ExportError::Io {
                            path: source,
                            source: e,
                        });
                    }
                    return Ok(PlannedExportItem {
                        entry_id,
                        source,
                        source_canonical: None,
                        target,
                        rel_in_repo: None,
                        classification: ItemClassification::ExcludeBrokenSymlink {},
                        findings: Vec::new(),
                        size_bytes: None,
                        is_text: None,
                    });
                }
                Ok(canon) => {
                    if canon.is_dir() {
                        return Ok(PlannedExportItem {
                            entry_id,
                            source,
                            source_canonical: Some(canon),
                            target,
                            rel_in_repo: None,
                            classification: ItemClassification::ExcludeDirectory {},
                            findings: Vec::new(),
                            size_bytes: None,
                            is_text: None,
                        });
                    }
                    let size = canon
                        .metadata()
                        .map_err(|e| ExportError::Io {
                            path: canon.clone(),
                            source: e,
                        })?
                        .len();
                    (canon, size)
                }
            }
        } else if lstat.is_dir() {
            let canon = std::fs::canonicalize(&source).map_err(|e| ExportError::Io {
                path: source.clone(),
                source: e,
            })?;
            return Ok(PlannedExportItem {
                entry_id,
                source,
                source_canonical: Some(canon),
                target,
                rel_in_repo: None,
                classification: ItemClassification::ExcludeDirectory {},
                findings: Vec::new(),
                size_bytes: None,
                is_text: None,
            });
        } else {
            let canon = std::fs::canonicalize(&source).map_err(|e| ExportError::Io {
                path: source.clone(),
                source: e,
            })?;
            let size = lstat.len();
            (canon, size)
        };

        // 4. Sensitive-path check (target and canonical source).
        let rel_target_str = rel_target.to_string_lossy();
        let sensitive_hit = self.check_sensitive_path(&rel_target_str).or_else(|| {
            source_canonical
                .strip_prefix(self.home)
                .ok()
                .and_then(|rel| self.check_sensitive_path(&rel.to_string_lossy()))
        });

        if let Some((rule_id, kind)) = sensitive_hit {
            let overridden = opts
                .allow_paths
                .iter()
                .any(|pat| glob::glob_matches(pat, &rel_target_str));
            if !overridden {
                return Ok(PlannedExportItem {
                    entry_id,
                    source,
                    source_canonical: Some(source_canonical),
                    target,
                    rel_in_repo: None,
                    classification: ItemClassification::ExcludeSensitivePath { rule_id, kind },
                    findings: Vec::new(),
                    size_bytes: Some(effective_size),
                    is_text: None,
                });
            }
        }

        // 5. Size check.
        if effective_size > opts.max_bytes {
            return Ok(PlannedExportItem {
                entry_id,
                source,
                source_canonical: Some(source_canonical),
                target,
                rel_in_repo: None,
                classification: ItemClassification::ExcludeBySize {
                    size_bytes: effective_size,
                    limit_bytes: opts.max_bytes,
                },
                findings: Vec::new(),
                size_bytes: Some(effective_size),
                is_text: None,
            });
        }

        // 6. Binary sniff (first 8 KiB of the canonical source).
        let sniff = read_first_bytes(&source_canonical, 8 * 1024).map_err(|e| ExportError::Io {
            path: source_canonical.clone(),
            source: e,
        })?;
        let is_text = is_text_sniff(&sniff);

        if !is_text {
            let overridden = opts
                .allow_binary
                .iter()
                .chain(opts.allow_paths.iter())
                .any(|pat| glob::glob_matches(pat, &rel_target_str));
            if !overridden {
                return Ok(PlannedExportItem {
                    entry_id,
                    source,
                    source_canonical: Some(source_canonical),
                    target,
                    rel_in_repo: None,
                    classification: ItemClassification::ExcludeBinary {},
                    findings: Vec::new(),
                    size_bytes: Some(effective_size),
                    is_text: Some(false),
                });
            }
        }

        // 7. Include.
        let rel_in_repo = PathBuf::from("dotfiles").join(&rel_target);
        Ok(PlannedExportItem {
            entry_id,
            source,
            source_canonical: Some(source_canonical),
            target,
            rel_in_repo: Some(rel_in_repo),
            classification: ItemClassification::Include {},
            findings: Vec::new(),
            size_bytes: Some(effective_size),
            is_text: Some(true),
        })
    }

    /// Returns the first sensitive-path denylist match for `rel_path`
    /// (a path relative to `$HOME`), or `None` if no rule matches.
    fn check_sensitive_path(&self, rel_path: &str) -> Option<(String, SensitivePathKind)> {
        for (id, kind, pattern) in &self.sensitive_paths {
            let hit = match kind {
                SensitivePathKind::Prefix => rel_path.starts_with(pattern.as_str()),
                SensitivePathKind::Exact => rel_path == pattern.as_str(),
                SensitivePathKind::Suffix => rel_path.ends_with(pattern.as_str()),
            };
            if hit {
                return Some((id.clone(), *kind));
            }
        }
        None
    }
}

// ── private fs helpers ────────────────────────────────────────────────────────

fn read_first_bytes(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(max.min(4096));
    f.take(max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Returns `true` when `bytes` look like text (UTF-8 or a mostly-printable
/// non-UTF-8 encoding such as ISO-8859-1).
///
/// Binary heuristic: any null byte → binary. Otherwise try UTF-8; if that
/// fails, require ≥ 90 % of bytes to be printable or common whitespace.
fn is_text_sniff(bytes: &[u8]) -> bool {
    if bytes.contains(&0x00) {
        return false;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return true;
    }
    let printable = bytes
        .iter()
        .filter(|&&b| b >= 0x20 || matches!(b, b'\n' | b'\r' | b'\t'))
        .count();
    printable >= bytes.len() * 9 / 10
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{ExportOptions, Exporter, ItemClassification, SensitivePathKind};
    use crate::profile::{Dependencies, FileEntry, Hooks, LinkMode, Profile, ProfileMeta};

    // ── test helpers ──────────────────────────────────────────────────────────

    fn make_profile(name: &str, files: Vec<FileEntry>) -> Profile {
        Profile {
            schema_version: 1,
            profile: ProfileMeta {
                name: name.to_string(),
                description: None,
                author: None,
                created_at: None,
                tags: vec![],
            },
            wm: None,
            files,
            dependencies: Dependencies::default(),
            hooks: Hooks::default(),
        }
    }

    fn make_entry(id: &str, source: &str, target: &str) -> FileEntry {
        FileEntry {
            id: id.to_string(),
            source: PathBuf::from(source),
            target: target.to_string(),
            mode: LinkMode::Symlink,
            exec: false,
            required: true,
        }
    }

    fn make_entry_optional(id: &str, source: &str, target: &str) -> FileEntry {
        let mut e = make_entry(id, source, target);
        e.required = false;
        e
    }

    fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    // ── pure sensitive-path filter tests (no FS) ──────────────────────────────

    #[test]
    fn sensitive_prefix_ssh_matches() {
        let profile = make_profile("t", vec![]);
        let home = PathBuf::from("/tmp");
        let exp = Exporter::new(&profile, &home, &home);
        let hit = exp.check_sensitive_path(".ssh/id_ed25519");
        assert!(
            matches!(hit, Some((_, SensitivePathKind::Prefix))),
            "expected Prefix hit for .ssh/id_ed25519"
        );
    }

    #[test]
    fn sensitive_exact_netrc_matches() {
        let profile = make_profile("t", vec![]);
        let home = PathBuf::from("/tmp");
        let exp = Exporter::new(&profile, &home, &home);
        let hit = exp.check_sensitive_path(".netrc");
        assert!(
            matches!(hit, Some((_, SensitivePathKind::Exact))),
            "expected Exact hit for .netrc"
        );
    }

    #[test]
    fn sensitive_suffix_pem_matches() {
        let profile = make_profile("t", vec![]);
        let home = PathBuf::from("/tmp");
        let exp = Exporter::new(&profile, &home, &home);
        let hit = exp.check_sensitive_path("certs/server.pem");
        assert!(
            matches!(hit, Some((_, SensitivePathKind::Suffix))),
            "expected Suffix hit for certs/server.pem"
        );
    }

    #[test]
    fn sensitive_normal_path_no_match() {
        let profile = make_profile("t", vec![]);
        let home = PathBuf::from("/tmp");
        let exp = Exporter::new(&profile, &home, &home);
        assert!(
            exp.check_sensitive_path(".config/hypr/hyprland.conf")
                .is_none(),
            "expected no hit for a normal config path"
        );
    }

    // ── plan integration tests (tempdir) ─────────────────────────────────────

    #[test]
    fn plan_normal_file_rel_in_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        write_file(&profile_dir, "bar", b"hello\n");

        let target = format!("{}/.config/foo/bar", home.display());
        let entry = FileEntry {
            id: "bar".to_string(),
            source: PathBuf::from("bar"),
            target: target.clone(),
            mode: LinkMode::Copy,
            exec: false,
            required: true,
        };
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert_eq!(plan.items.len(), 1);
        let item = &plan.items[0];
        assert!(
            matches!(item.classification, ItemClassification::Include {}),
            "expected Include, got {:?}",
            item.classification
        );
        assert_eq!(
            item.rel_in_repo.as_deref(),
            Some(Path::new("dotfiles/.config/foo/bar"))
        );
    }

    #[test]
    fn plan_sensitive_path_ssh_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(
            &profile_dir,
            "id_ed25519",
            b"-----BEGIN OPENSSH PRIVATE KEY-----\n",
        );

        let target = format!("{}/.ssh/id_ed25519", home.display());
        let entry = make_entry("key", "id_ed25519", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                &plan.items[0].classification,
                ItemClassification::ExcludeSensitivePath {
                    kind: SensitivePathKind::Prefix,
                    ..
                }
            ),
            "expected ExcludeSensitivePath(Prefix) for .ssh/"
        );
    }

    #[test]
    fn plan_size_limit_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Create a 2 MiB sparse file.
        let large = profile_dir.join("big");
        {
            let f = std::fs::File::create(&large).unwrap();
            f.set_len(2 * 1024 * 1024).unwrap();
        }

        let target = format!("{}/.config/big", home.display());
        let entry = make_entry("big", "big", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::ExcludeBySize { .. }
            ),
            "expected ExcludeBySize for 2 MiB file"
        );
    }

    #[test]
    fn plan_binary_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // File with null bytes → binary.
        write_file(&profile_dir, "font.ttf", &[0u8, 1, 2, 3, 0, 255]);

        let target = format!("{}/.local/share/fonts/font.ttf", home.display());
        let entry = make_entry("font", "font.ttf", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::ExcludeBinary {}
            ),
            "expected ExcludeBinary for file with null bytes"
        );
        assert_eq!(plan.items[0].is_text, Some(false));
    }

    #[test]
    fn plan_target_outside_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(
            &profile_dir,
            "xorg.conf",
            b"Section \"Device\"\nEndSection\n",
        );

        // Absolute target outside home.
        let entry = make_entry("xorg", "xorg.conf", "/etc/X11/xorg.conf.d/10-custom.conf");
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::OutsideHome {}
            ),
            "expected OutsideHome for /etc target"
        );
    }

    #[test]
    fn plan_broken_symlink_required_is_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Create a dangling symlink.
        let link = profile_dir.join("broken");
        std::os::unix::fs::symlink("/nonexistent/path/nowhere", &link).unwrap();

        let target = format!("{}/.config/broken", home.display());
        let entry = make_entry("broken", "broken", &target); // required = true
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);

        let result = exp.plan(&ExportOptions::default());
        assert!(
            matches!(result, Err(crate::error::ExportError::Io { .. })),
            "required broken symlink must surface as Err(Io)"
        );
    }

    #[test]
    fn plan_broken_symlink_optional_classified() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let link = profile_dir.join("broken");
        std::os::unix::fs::symlink("/nonexistent/path/nowhere", &link).unwrap();

        let target = format!("{}/.config/broken", home.display());
        let entry = make_entry_optional("broken", "broken", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);

        let plan = exp.plan(&ExportOptions::default()).unwrap();
        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::ExcludeBrokenSymlink {}
            ),
            "optional broken symlink must be ExcludeBrokenSymlink"
        );
    }

    #[test]
    fn plan_source_directory_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Source is a directory.
        std::fs::create_dir(profile_dir.join("mydir")).unwrap();

        let target = format!("{}/.config/mydir", home.display());
        let entry = make_entry("mydir", "mydir", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);

        let plan = exp.plan(&ExportOptions::default()).unwrap();
        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::ExcludeDirectory {}
            ),
            "directory source must be ExcludeDirectory"
        );
    }

    #[test]
    fn plan_allow_path_overrides_sensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "config", b"Host example.com\n  User alice\n");

        let target = format!("{}/.ssh/config", home.display());
        let entry = make_entry("sshcfg", "config", &target);
        let profile = make_profile("test", vec![entry]);

        let opts = ExportOptions {
            allow_paths: vec![".ssh/config".to_string()],
            ..ExportOptions::default()
        };

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&opts).unwrap();

        assert!(
            matches!(plan.items[0].classification, ItemClassification::Include {}),
            "--allow-path must override the denylist; got {:?}",
            plan.items[0].classification
        );
    }

    #[test]
    fn plan_valid_symlink_classified_include() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Create the real file and a symlink to it.
        let real = write_file(&tmp.path().join("dotfiles"), "zshrc", b"# zsh config\n");
        let link = profile_dir.join("zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        let item = &plan.items[0];
        assert!(
            matches!(item.classification, ItemClassification::Include {}),
            "valid symlink must be Include"
        );
        assert_eq!(
            item.source_canonical.as_deref(),
            Some(real.as_path()),
            "source_canonical must be the resolved real path"
        );
    }

    #[test]
    fn plan_symlink_to_sensitive_source_excluded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Real file is inside home/.ssh/
        let real = write_file(&home, ".ssh/id_ed25519", b"private key bytes\n");
        // Profile source is a symlink pointing into .ssh/ — innocuous name
        let link = profile_dir.join("innocent");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Target is also innocent (not under .ssh/)
        let target = format!("{}/.config/innocent", home.display());
        let entry = make_entry("innocent", "innocent", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                &plan.items[0].classification,
                ItemClassification::ExcludeSensitivePath {
                    kind: SensitivePathKind::Prefix,
                    ..
                }
            ),
            "symlink to sensitive source must be ExcludeSensitivePath"
        );
    }

    #[test]
    fn plan_missing_source_required_is_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Source file does NOT exist.
        let target = format!("{}/.config/missing", home.display());
        let entry = make_entry("miss", "nonexistent.txt", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);

        let result = exp.plan(&ExportOptions::default());
        assert!(
            matches!(result, Err(crate::error::ExportError::Io { .. })),
            "required missing source must be Err(Io)"
        );
    }

    #[test]
    fn plan_missing_source_optional_classified() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let target = format!("{}/.config/missing", home.display());
        let entry = make_entry_optional("miss", "nonexistent.txt", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);

        let plan = exp.plan(&ExportOptions::default()).unwrap();
        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::MissingSource {}
            ),
            "optional missing source must be MissingSource"
        );
    }

    #[test]
    fn plan_allow_binary_overrides_binary_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Binary file (null bytes).
        write_file(&profile_dir, "firmware.bin", &[0u8, 1, 2, 3, 0, 255]);

        let target = format!("{}/.config/firmware.bin", home.display());
        let entry = make_entry("fw", "firmware.bin", &target);
        let profile = make_profile("test", vec![entry]);

        let opts = ExportOptions {
            allow_binary: vec!["**/*.bin".to_string()],
            ..ExportOptions::default()
        };

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&opts).unwrap();

        assert!(
            matches!(plan.items[0].classification, ItemClassification::Include {}),
            "--allow-binary must override ExcludeBinary; got {:?}",
            plan.items[0].classification
        );
    }

    #[test]
    fn plan_pem_suffix_excluded_by_denylist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "server.pem", b"-----BEGIN CERTIFICATE-----\n");

        let target = format!("{}/.config/server.pem", home.display());
        let entry = make_entry("cert", "server.pem", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                &plan.items[0].classification,
                ItemClassification::ExcludeSensitivePath {
                    kind: SensitivePathKind::Suffix,
                    ..
                }
            ),
            "expected ExcludeSensitivePath(Suffix) for .pem file; got {:?}",
            plan.items[0].classification
        );
    }
}
