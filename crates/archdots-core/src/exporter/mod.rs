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
pub mod scanner;
pub(crate) mod template;

pub use scanner::SecretScanner;

const README_TEMPLATE: &str = include_str!("../../data/readme_template.md");
const INSTALL_TEMPLATE: &str = include_str!("../../data/install_template.sh");
const GITIGNORE_TEMPLATE: &str = include_str!("../../data/gitignore_template");

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

impl ExportPlan {
    /// Count High-severity findings on `Include` items that are covered by
    /// `--allow-secret` rules or `--include-secrets` in the embedded options.
    #[must_use]
    pub fn count_overridden_findings(&self, home: &Path) -> usize {
        let opts = &self.options;
        if !opts.include_secrets && opts.allow_secret_rules.is_empty() {
            return 0;
        }
        let mut count = 0;
        for item in &self.items {
            if !matches!(item.classification, ItemClassification::Include {}) {
                continue;
            }
            let target_rel = item
                .target
                .strip_prefix(home)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            for finding in &item.findings {
                if finding.severity != SecretSeverity::High {
                    continue;
                }
                let allowed_by_rule = opts.allow_secret_rules.iter().any(|a| {
                    a.rule_id == finding.rule_id
                        && match &a.path_glob {
                            None => true,
                            Some(g) => glob::glob_matches(g, &target_rel),
                        }
                });
                if allowed_by_rule || opts.include_secrets {
                    count += 1;
                }
            }
        }
        count
    }
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

    // ── scan pipeline ─────────────────────────────────────────────────────────

    /// Read the contents of every `Include` item via `canonicalize(source)` and
    /// populate its `findings`. Idempotent: re-running clears and re-fills
    /// findings for all `Include` items.
    ///
    /// Symlinks: if `source_canonical` is set (all `Include` items have it),
    /// the bytes are read from the canonicalized path, which is the real file.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::ScannerInit`] if the embedded patterns fail to
    /// compile (programming bug). Returns [`ExportError::Io`] if a file that
    /// was readable at plan time is no longer readable at scan time.
    pub fn scan(&self, plan: &mut ExportPlan) -> Result<(), ExportError> {
        let sec = scanner::SecretScanner::new()?;
        for item in &mut plan.items {
            if !matches!(item.classification, ItemClassification::Include {}) {
                continue;
            }
            // Idempotent: clear before refilling.
            item.findings.clear();
            // Clone to avoid simultaneous borrow of item.
            let Some(canon) = item.source_canonical.clone() else {
                continue;
            };
            let bytes = std::fs::read(&canon).map_err(|e| ExportError::Io {
                path: canon.clone(),
                source: e,
            })?;
            item.findings = sec.scan(&bytes);
        }
        Ok(())
    }

    /// Returns `true` iff the plan is safe to write without additional
    /// interactive confirmation.
    ///
    /// Returns `false` when:
    /// - Any item has an [`ItemClassification::ExcludeSensitivePath`]
    ///   classification (the user has not acknowledged the sensitive file via
    ///   `--allow-path`).
    /// - Any `Include` item has a `High`-severity finding that is not covered
    ///   by an entry in `opts.allow_secret_rules`.
    ///
    /// `opts.include_secrets` overrides the `High`-finding gate but **not**
    /// the `ExcludeSensitivePath` gate.
    #[must_use]
    pub fn is_safe_to_write(&self, plan: &ExportPlan, opts: &ExportOptions) -> bool {
        // Gate 1: any un-acknowledged sensitive path → unsafe.
        for item in &plan.items {
            if matches!(
                item.classification,
                ItemClassification::ExcludeSensitivePath { .. }
            ) {
                return false;
            }
        }

        // Gate 2: any High finding not covered by an allowance.
        if !opts.include_secrets {
            for item in &plan.items {
                let target_rel = item
                    .target
                    .strip_prefix(self.home)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();

                for finding in &item.findings {
                    if finding.severity != SecretSeverity::High {
                        continue;
                    }
                    let allowed = opts.allow_secret_rules.iter().any(|a| {
                        a.rule_id == finding.rule_id
                            && match &a.path_glob {
                                None => true,
                                Some(g) => glob::glob_matches(g, &target_rel),
                            }
                    });
                    if !allowed {
                        return false;
                    }
                }
            }
        }

        true
    }

    // ── render_readme ─────────────────────────────────────────────────────────

    /// Render the README from profile + plan. Pure (no disk I/O).
    ///
    /// # Errors
    /// Returns [`ExportError::TemplateRender`] when the embedded template has
    /// an unresolved placeholder (programming bug; should not occur in release).
    pub fn render_readme(
        &self,
        plan: &ExportPlan,
        archdots_version: &str,
    ) -> Result<String, ExportError> {
        use template::{escape_md_block, escape_md_inline, TemplateContext};

        let mut ctx = TemplateContext::new();

        // ── profile metadata ──────────────────────────────────────────────
        ctx.set_str("profile_name", &self.profile.profile.name);
        ctx.set_str(
            "profile_description",
            escape_md_block(
                self.profile
                    .profile
                    .description
                    .as_deref()
                    .unwrap_or("A dotfiles profile generated with archdots."),
            ),
        );
        ctx.set_str(
            "author",
            escape_md_inline(self.profile.profile.author.as_deref().unwrap_or("\u{2014}")),
        );

        let tags_inline = if self.profile.profile.tags.is_empty() {
            "\u{2014}".to_string()
        } else {
            self.profile
                .profile
                .tags
                .iter()
                .map(|t| format!("`{}`", escape_md_inline(t)))
                .collect::<Vec<_>>()
                .join(" ")
        };
        ctx.set_str("tags_inline", tags_inline);
        ctx.set_str("wm_kind", wm_kind_str(self.profile.wm.as_ref()));
        ctx.set_str("generated_date", today_iso());
        ctx.set_str("archdots_version", archdots_version);

        // ── dependencies ──────────────────────────────────────────────────
        let pacman = &self.profile.dependencies.pacman;
        let aur = &self.profile.dependencies.aur;
        let optional = &self.profile.dependencies.optional_pacman;

        ctx.set_bool("has_pacman", !pacman.is_empty());
        ctx.set_bool("no_pacman", pacman.is_empty());
        ctx.set_str("pacman_deps_str", pacman.join(" "));

        ctx.set_bool("has_aur", !aur.is_empty());
        ctx.set_bool("no_aur", aur.is_empty());
        ctx.set_str("aur_deps_str", aur.join(" "));

        ctx.set_bool("has_optional", !optional.is_empty());
        ctx.set_bool("no_optional", optional.is_empty());
        ctx.set_str("optional_deps_str", optional.join(" "));

        // ── file tables ───────────────────────────────────────────────────
        // Build an id → entry map for unexpanded target and mode lookup.
        let entry_map: std::collections::HashMap<&str, &crate::profile::FileEntry> = self
            .profile
            .files
            .iter()
            .map(|e| (e.id.as_str(), e))
            .collect();

        let mut included_rows: Vec<String> = Vec::new();
        let mut excluded_rows: Vec<String> = Vec::new();

        for item in &plan.items {
            match &item.classification {
                ItemClassification::Include {} => {
                    if let (Some(rel), Some(entry)) = (
                        item.rel_in_repo.as_ref(),
                        entry_map.get(item.entry_id.as_str()),
                    ) {
                        let rel_str = rel.to_string_lossy();
                        let target_raw = escape_md_inline(&entry.target);
                        let mode = link_mode_str(entry);
                        included_rows.push(format!(
                            "| `{}` | `{}` | {} |",
                            escape_md_inline(&rel_str),
                            target_raw,
                            mode
                        ));
                    }
                }
                ItemClassification::OutsideHome {}
                | ItemClassification::MissingSource {}
                | ItemClassification::ExcludeSensitivePath { .. }
                | ItemClassification::ExcludeBySize { .. }
                | ItemClassification::ExcludeBinary {}
                | ItemClassification::ExcludeBrokenSymlink {}
                | ItemClassification::ExcludeDirectory {} => {
                    let target_str = escape_md_inline(&item.target.to_string_lossy());
                    let reason = exclusion_reason_str(&item.classification);
                    excluded_rows.push(format!("| `{target_str}` | {reason} |"));
                }
            }
        }

        ctx.set_list("files_included", included_rows);
        ctx.set_bool("has_excluded", !excluded_rows.is_empty());
        ctx.set_list("files_excluded", excluded_rows);

        template::render(README_TEMPLATE, &ctx)
    }

    // ── write ─────────────────────────────────────────────────────────────────

    /// Materialise the plan to `output_dir` using atomic staging + rename.
    ///
    /// # Errors
    /// - [`ExportError::Unsafe`] when `is_safe_to_write` returns false.
    /// - [`ExportError::OutputNotEmpty`] if directory is non-empty without `force`.
    /// - [`ExportError::InvalidOptions`] if `output_dir` overlaps source paths.
    /// - [`ExportError::Io`] on any filesystem error.
    pub fn write(
        &self,
        plan: &ExportPlan,
        output_dir: &Path,
        opts: &ExportOptions,
        force: bool,
    ) -> Result<ExportReport, ExportError> {
        // 0. Safety gate.
        if !self.is_safe_to_write(plan, opts) {
            return Err(ExportError::Unsafe);
        }

        // 1. Pre-flight checks.
        if output_dir.is_file() {
            return Err(ExportError::Io {
                path: output_dir.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "output path is a regular file, not a directory",
                ),
            });
        }

        let parent = output_dir.parent().ok_or_else(|| ExportError::Io {
            path: output_dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "output directory has no parent",
            ),
        })?;
        if !parent.exists() {
            return Err(ExportError::Io {
                path: parent.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "parent directory does not exist",
                ),
            });
        }

        if output_dir.is_dir() && !force && dir_is_nonempty(output_dir)? {
            return Err(ExportError::OutputNotEmpty(output_dir.to_path_buf()));
        }

        // Check for overlap between output_dir and profile source paths.
        if let Ok(canon_out) = output_dir.canonicalize() {
            for item in &plan.items {
                if let Some(canon_src) = &item.source_canonical {
                    if canon_src.starts_with(&canon_out) {
                        return Err(ExportError::InvalidOptions(
                            "output directory overlaps with profile source paths".to_string(),
                        ));
                    }
                }
            }
        }

        // 2. Create staging dir (sibling of output_dir).
        let staging = make_staging_dir(parent)?;

        // 3. Populate + finalize (cleanup staging on any error).
        let result = self.populate_staging(&staging, plan, opts);
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        let result = finalize_staging(&staging, output_dir);
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        // 4. Build report.
        Ok(build_report(output_dir, plan, self.home))
    }

    fn populate_staging(
        &self,
        staging: &Path,
        plan: &ExportPlan,
        opts: &ExportOptions,
    ) -> Result<(), ExportError> {
        let version = env!("CARGO_PKG_VERSION");

        // README.md
        if opts.include_readme {
            let readme = self.render_readme(plan, version)?;
            write_file_sync(staging, "README.md", readme.as_bytes())?;
        }

        // archdots-profile.toml
        let exported_profile = self.build_exported_profile(plan, opts);
        let toml_str = toml::to_string_pretty(&exported_profile).map_err(|e| {
            ExportError::TemplateRender(format!("failed to serialize profile: {e}"))
        })?;
        write_file_sync(staging, "archdots-profile.toml", toml_str.as_bytes())?;

        if matches!(opts.format, ExportFormat::Full) {
            // install.sh
            if opts.include_install_script {
                let install_sh = self.render_install_sh(plan)?;
                write_file_sync_exec(staging, "install.sh", install_sh.as_bytes())?;
            }

            // .gitignore
            write_file_sync(staging, ".gitignore", GITIGNORE_TEMPLATE.as_bytes())?;

            // dotfiles/<rel>
            self.copy_dotfiles(staging, plan)?;
        }

        // fsync the staging directory itself.
        fsync_dir(staging).map_err(|e| ExportError::Io {
            path: staging.to_path_buf(),
            source: e,
        })?;

        Ok(())
    }

    fn render_install_sh(&self, _plan: &ExportPlan) -> Result<String, ExportError> {
        use template::TemplateContext;
        let mut ctx = TemplateContext::new();
        let pacman = &self.profile.dependencies.pacman;
        let aur = &self.profile.dependencies.aur;
        ctx.set_bool("has_pacman", !pacman.is_empty());
        ctx.set_str("pacman_deps_str", pacman.join(" "));
        ctx.set_bool("has_aur", !aur.is_empty());
        ctx.set_str("aur_deps_str", aur.join(" "));
        template::render(INSTALL_TEMPLATE, &ctx)
    }

    fn build_exported_profile(
        &self,
        plan: &ExportPlan,
        opts: &ExportOptions,
    ) -> crate::profile::Profile {
        use crate::profile::{FileEntry, Profile};
        let mut profile = self.profile.clone();

        if matches!(opts.format, ExportFormat::Full) {
            // Keep only Include items; rewrite source to dotfiles/<rel>.
            let rel_map: std::collections::HashMap<&str, &std::path::PathBuf> = plan
                .items
                .iter()
                .filter(|i| matches!(i.classification, ItemClassification::Include {}))
                .filter_map(|i| i.rel_in_repo.as_ref().map(|r| (i.entry_id.as_str(), r)))
                .collect();

            let new_files: Vec<FileEntry> = profile
                .files
                .iter()
                .filter(|e| rel_map.contains_key(e.id.as_str()))
                .map(|e| {
                    let mut ne = e.clone();
                    if let Some(rel) = rel_map.get(e.id.as_str()) {
                        ne.source.clone_from(*rel);
                    }
                    ne
                })
                .collect();

            profile.files = new_files;
        }
        // ProfileOnly: keep all files with original sources.
        Profile {
            schema_version: profile.schema_version,
            profile: profile.profile,
            wm: profile.wm,
            files: profile.files,
            dependencies: profile.dependencies,
            hooks: profile.hooks,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn copy_dotfiles(&self, staging: &Path, plan: &ExportPlan) -> Result<(), ExportError> {
        use std::os::unix::fs::PermissionsExt as _;

        // Build id → exec flag map for the profile entries.
        let exec_map: std::collections::HashMap<&str, bool> = self
            .profile
            .files
            .iter()
            .map(|e| (e.id.as_str(), e.exec))
            .collect();

        for item in &plan.items {
            if !matches!(item.classification, ItemClassification::Include {}) {
                continue;
            }
            let (Some(rel), Some(canon)) =
                (item.rel_in_repo.as_ref(), item.source_canonical.as_ref())
            else {
                continue;
            };

            let dst = staging.join(rel);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p).map_err(|e| ExportError::Io {
                    path: p.to_path_buf(),
                    source: e,
                })?;
            }

            // Re-canonicalize at copy time to defend against TOCTOU.
            let real_src = std::fs::canonicalize(canon).map_err(|e| ExportError::Io {
                path: canon.clone(),
                source: e,
            })?;

            std::fs::copy(&real_src, &dst).map_err(|e| ExportError::Io {
                path: dst.clone(),
                source: e,
            })?;

            // Preserve executable bit from source; also apply exec flag.
            let src_mode = std::fs::metadata(&real_src)
                .map_err(|e| ExportError::Io {
                    path: real_src.clone(),
                    source: e,
                })?
                .permissions()
                .mode();
            let force_exec = exec_map
                .get(item.entry_id.as_str())
                .copied()
                .unwrap_or(false);
            if src_mode & 0o111 != 0 || force_exec {
                let mut perms = std::fs::metadata(&dst)
                    .map_err(|e| ExportError::Io {
                        path: dst.clone(),
                        source: e,
                    })?
                    .permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&dst, perms).map_err(|e| ExportError::Io {
                    path: dst.clone(),
                    source: e,
                })?;
            }

            // fsync the destination file.
            let f = std::fs::File::open(&dst).map_err(|e| ExportError::Io {
                path: dst.clone(),
                source: e,
            })?;
            f.sync_all().map_err(|e| ExportError::Io {
                path: dst.clone(),
                source: e,
            })?;
        }
        Ok(())
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

        // Compute hits from target and source separately: --allow-path must match
        // the path that triggered the hit.  If only the canonical source matched
        // (e.g. a profile symlink pointing into ~/.ssh/), then a glob matching only
        // the innocuous target name must NOT grant the override.
        let target_hit = self.check_sensitive_path(&rel_target_str);
        let source_rel_str: Option<String> = source_canonical
            .strip_prefix(self.home)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned());
        let source_hit = source_rel_str
            .as_deref()
            .and_then(|s| self.check_sensitive_path(s));

        let sensitive_hit = target_hit.clone().or_else(|| source_hit.clone());

        if let Some((rule_id, kind)) = sensitive_hit {
            let overridden = opts.allow_paths.iter().any(|pat| {
                (target_hit.is_some() && glob::glob_matches(pat, &rel_target_str))
                    || source_hit.is_some()
                        && source_rel_str
                            .as_deref()
                            .is_some_and(|s| glob::glob_matches(pat, s))
            });
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

// ── module-level helpers ──────────────────────────────────────────────────────

fn wm_kind_str(wm: Option<&crate::profile::WmSpec>) -> String {
    match wm.map(|w| &w.kind) {
        Some(crate::profile::WmKind::Bspwm) => "bspwm".to_string(),
        Some(crate::profile::WmKind::Hyprland) => "hyprland".to_string(),
        Some(crate::profile::WmKind::I3) => "i3".to_string(),
        Some(crate::profile::WmKind::Sway) => "sway".to_string(),
        None | Some(_) => "\u{2014}".to_string(),
    }
}

fn link_mode_str(entry: &crate::profile::FileEntry) -> &'static str {
    use crate::profile::LinkMode;
    match entry.mode {
        LinkMode::Symlink if entry.exec => "symlink +x",
        LinkMode::Symlink => "symlink",
        LinkMode::Copy if entry.exec => "copy +x",
        LinkMode::Copy => "copy",
        LinkMode::Template => "template",
    }
}

fn exclusion_reason_str(c: &ItemClassification) -> String {
    match c {
        ItemClassification::ExcludeSensitivePath { rule_id, kind } => {
            let k = match kind {
                SensitivePathKind::Prefix => "prefix",
                SensitivePathKind::Exact => "exact",
                SensitivePathKind::Suffix => "suffix",
            };
            format!("sensitive path ({k}: `{rule_id}`)")
        }
        ItemClassification::ExcludeBySize {
            size_bytes,
            limit_bytes,
        } => format!("file too large ({size_bytes} B > {limit_bytes} B limit)"),
        ItemClassification::ExcludeBinary {} => "binary file".to_string(),
        ItemClassification::ExcludeBrokenSymlink {} => "broken symlink".to_string(),
        ItemClassification::ExcludeDirectory {} => "source is a directory".to_string(),
        ItemClassification::MissingSource {} => "source file not found".to_string(),
        ItemClassification::OutsideHome {} => "target is outside \\$HOME".to_string(),
        _ => "excluded".to_string(),
    }
}

fn today_iso() -> String {
    use time::OffsetDateTime;
    let d = OffsetDateTime::now_utc().date();
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

fn dir_is_nonempty(dir: &Path) -> Result<bool, ExportError> {
    let mut rd = std::fs::read_dir(dir).map_err(|e| ExportError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    Ok(rd.next().is_some())
}

fn make_staging_dir(parent: &Path) -> Result<PathBuf, ExportError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let staging = parent.join(format!(".archdots-export.tmp.{pid}.{nonce:x}"));
    std::fs::create_dir(&staging).map_err(|e| ExportError::Io {
        path: staging.clone(),
        source: e,
    })?;
    Ok(staging)
}

fn write_file_sync(dir: &Path, name: &str, content: &[u8]) -> Result<(), ExportError> {
    use std::io::Write as _;
    let path = dir.join(name);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| ExportError::Io {
            path: p.to_path_buf(),
            source: e,
        })?;
    }
    let mut f = std::fs::File::create(&path).map_err(|e| ExportError::Io {
        path: path.clone(),
        source: e,
    })?;
    f.write_all(content).map_err(|e| ExportError::Io {
        path: path.clone(),
        source: e,
    })?;
    f.sync_all().map_err(|e| ExportError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(())
}

fn write_file_sync_exec(dir: &Path, name: &str, content: &[u8]) -> Result<(), ExportError> {
    use std::os::unix::fs::PermissionsExt as _;
    write_file_sync(dir, name, content)?;
    let path = dir.join(name);
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| ExportError::Io {
            path: path.clone(),
            source: e,
        })?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(&path, perms).map_err(|e| ExportError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(())
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = std::fs::File::open(dir)?;
    f.sync_all()
}

/// Atomic finalisation: rename staging → `output_dir` if `output_dir` doesn't
/// exist; otherwise merge file-by-file (`output_dir` existed with force=true).
fn finalize_staging(staging: &Path, output_dir: &Path) -> Result<(), ExportError> {
    if output_dir.exists() {
        // output_dir exists (force=true path): merge staging into it.
        merge_dir_into(staging, output_dir)?;
        let _ = std::fs::remove_dir_all(staging);
        // fsync output_dir after merge.
        fsync_dir(output_dir).map_err(|e| ExportError::Io {
            path: output_dir.to_path_buf(),
            source: e,
        })?;
    } else {
        std::fs::rename(staging, output_dir).map_err(|e| ExportError::Io {
            path: output_dir.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

fn merge_dir_into(src: &Path, dst: &Path) -> Result<(), ExportError> {
    for entry in std::fs::read_dir(src).map_err(|e| ExportError::Io {
        path: src.to_path_buf(),
        source: e,
    })? {
        let entry = entry.map_err(|e| ExportError::Io {
            path: src.to_path_buf(),
            source: e,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path).map_err(|e| ExportError::Io {
                path: dst_path.clone(),
                source: e,
            })?;
            merge_dir_into(&src_path, &dst_path)?;
        } else {
            std::fs::rename(&src_path, &dst_path).map_err(|e| ExportError::Io {
                path: dst_path.clone(),
                source: e,
            })?;
        }
    }
    Ok(())
}

fn build_report(output_dir: &Path, plan: &ExportPlan, home: &Path) -> ExportReport {
    let mut report = ExportReport {
        output_dir: output_dir.to_path_buf(),
        items_included: 0,
        items_excluded_by_path: 0,
        items_excluded_by_size: 0,
        items_excluded_binary: 0,
        items_missing: 0,
        findings_high: 0,
        findings_medium: 0,
        findings_overridden: plan.count_overridden_findings(home),
        bytes_written: 0,
    };
    for item in &plan.items {
        match &item.classification {
            ItemClassification::Include {} => {
                report.items_included += 1;
                report.bytes_written += item.size_bytes.unwrap_or(0);
                for f in &item.findings {
                    match f.severity {
                        SecretSeverity::High => report.findings_high += 1,
                        SecretSeverity::Medium => report.findings_medium += 1,
                    }
                }
            }
            ItemClassification::ExcludeSensitivePath { .. } => {
                report.items_excluded_by_path += 1;
            }
            ItemClassification::ExcludeBySize { .. } => {
                report.items_excluded_by_size += 1;
            }
            ItemClassification::ExcludeBinary {} => {
                report.items_excluded_binary += 1;
            }
            ItemClassification::MissingSource {} => {
                report.items_missing += 1;
            }
            _ => {}
        }
    }
    report
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

    use super::{
        ExportFormat, ExportOptions, Exporter, ItemClassification, PlannedExportItem,
        SecretAllowance, SecretFinding, SecretSeverity, SensitivePathKind,
    };
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

    // H-1 regression: --allow-path matching the target must NOT bypass a
    // sensitive hit that was triggered only by the canonical source.
    #[test]
    fn plan_allow_path_target_does_not_bypass_source_sensitive_hit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Real file lives under ~/.ssh/ → sensitive.
        let real = write_file(&home, ".ssh/id_ed25519", b"private key bytes\n");
        // Profile source is a symlink with an innocent name.
        let link = profile_dir.join("innocent");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Target is also innocent (not under .ssh/).
        let target = format!("{}/.config/innocent", home.display());
        let entry = make_entry("innocent", "innocent", &target);
        let profile = make_profile("test", vec![entry]);

        // --allow-path matches the target name, NOT the canonical source.
        // Before the fix this would incorrectly override the sensitive-path hit.
        let opts = ExportOptions {
            allow_paths: vec![".config/innocent".to_string()],
            ..ExportOptions::default()
        };

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&opts).unwrap();

        assert!(
            matches!(
                &plan.items[0].classification,
                ItemClassification::ExcludeSensitivePath { .. }
            ),
            "--allow-path on the target alone must not override a hit from the \
             canonical source; got {:?}",
            plan.items[0].classification
        );
    }

    // Companion positive test: --allow-path matching the canonical source path
    // correctly overrides the sensitive hit.
    #[test]
    fn plan_allow_path_source_path_overrides_source_sensitive_hit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let real = write_file(&home, ".ssh/id_ed25519", b"private key bytes\n");
        let link = profile_dir.join("innocent");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = format!("{}/.config/innocent", home.display());
        let entry = make_entry("innocent", "innocent", &target);
        let profile = make_profile("test", vec![entry]);

        // --allow-path matches the canonical source path that triggered the hit.
        let opts = ExportOptions {
            allow_paths: vec![".ssh/id_ed25519".to_string()],
            ..ExportOptions::default()
        };

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&opts).unwrap();

        assert!(
            matches!(plan.items[0].classification, ItemClassification::Include {}),
            "--allow-path on the canonical source path must override the sensitive \
             hit; got {:?}",
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

    // ── scan integration tests ────────────────────────────────────────────────

    /// AWS key content used across scan tests.
    const AWS_KEY_CONTENT: &str = "export KEY=value\nAKIAIOSFODNN7EXAMPLE\n# end\n";

    #[test]
    fn scan_include_item_gets_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "zshrc", AWS_KEY_CONTENT.as_bytes());

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(matches!(
            plan.items[0].classification,
            ItemClassification::Include {}
        ));

        exp.scan(&mut plan).unwrap();

        let item = &plan.items[0];
        assert!(!item.findings.is_empty(), "expected AWS key finding");
        assert!(
            item.findings
                .iter()
                .any(|f| f.rule_id == "aws-access-key-id"),
            "expected aws-access-key-id finding"
        );
    }

    #[test]
    fn scan_excluded_by_size_findings_remain_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // 2 MiB sparse file — will be ExcludeBySize; put AWS key in metadata
        let large = profile_dir.join("big.conf");
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&large).unwrap();
            f.write_all(AWS_KEY_CONTENT.as_bytes()).unwrap();
            f.set_len(2 * 1024 * 1024).unwrap();
        }

        let target = format!("{}/.config/big.conf", home.display());
        let entry = make_entry("big", "big.conf", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(
            matches!(
                plan.items[0].classification,
                ItemClassification::ExcludeBySize { .. }
            ),
            "expected ExcludeBySize"
        );

        exp.scan(&mut plan).unwrap();

        assert!(
            plan.items[0].findings.is_empty(),
            "excluded item must not have findings after scan"
        );
    }

    #[test]
    fn scan_symlink_reads_real_file_findings_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        // Real file with AWS key (outside home so no sensitive-path hit)
        let real = write_file(&real_dir, "zshrc", AWS_KEY_CONTENT.as_bytes());
        // Symlink inside profile_dir
        let link = profile_dir.join("zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(matches!(
            plan.items[0].classification,
            ItemClassification::Include {}
        ));

        exp.scan(&mut plan).unwrap();

        assert!(
            plan.items[0]
                .findings
                .iter()
                .any(|f| f.rule_id == "aws-access-key-id"),
            "symlink scan must find AWS key in real file"
        );
    }

    #[test]
    fn scan_idempotent_no_duplicate_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "zshrc", AWS_KEY_CONTENT.as_bytes());

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();

        exp.scan(&mut plan).unwrap();
        let count_after_first = plan.items[0].findings.len();
        assert!(count_after_first > 0, "first scan must produce findings");

        exp.scan(&mut plan).unwrap();
        let count_after_second = plan.items[0].findings.len();
        assert_eq!(
            count_after_first, count_after_second,
            "second scan must not duplicate findings"
        );
    }

    // ── is_safe_to_write tests ────────────────────────────────────────────────

    /// Build a minimal [`super::ExportPlan`] with one `Include` item and the
    /// given findings. The item target is `home/.config/f`.
    fn make_simple_plan(home: &Path, findings: Vec<SecretFinding>) -> super::ExportPlan {
        let item = PlannedExportItem {
            entry_id: "f".to_string(),
            source: home.join("f"),
            source_canonical: Some(home.join("f")),
            target: home.join(".config").join("f"),
            rel_in_repo: Some(PathBuf::from("dotfiles/.config/f")),
            classification: ItemClassification::Include {},
            findings,
            size_bytes: Some(10),
            is_text: Some(true),
        };
        super::ExportPlan {
            profile_name: "t".to_string(),
            items: vec![item],
            options: ExportOptions::default(),
        }
    }

    fn high_finding() -> SecretFinding {
        SecretFinding {
            rule_id: "aws-access-key-id".to_string(),
            severity: SecretSeverity::High,
            line: 1,
            column: 1,
            preview: "AKI...PLE (20 chars)".to_string(),
        }
    }

    fn medium_finding() -> SecretFinding {
        SecretFinding {
            rule_id: "jwt".to_string(),
            severity: SecretSeverity::Medium,
            line: 2,
            column: 1,
            preview: "eyJ...abc (80 chars)".to_string(),
        }
    }

    #[test]
    fn is_safe_false_for_high_finding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![high_finding()]);
        assert!(!exp.is_safe_to_write(&plan, &ExportOptions::default()));
    }

    #[test]
    fn is_safe_true_with_matching_allow_secret() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![high_finding()]);
        let opts = ExportOptions {
            allow_secret_rules: vec![SecretAllowance {
                rule_id: "aws-access-key-id".to_string(),
                path_glob: None,
            }],
            ..ExportOptions::default()
        };
        assert!(exp.is_safe_to_write(&plan, &opts));
    }

    #[test]
    fn is_safe_false_allow_secret_path_glob_no_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![high_finding()]);
        let opts = ExportOptions {
            allow_secret_rules: vec![SecretAllowance {
                rule_id: "aws-access-key-id".to_string(),
                path_glob: Some(".config/other-file".to_string()),
            }],
            ..ExportOptions::default()
        };
        assert!(
            !exp.is_safe_to_write(&plan, &opts),
            "non-matching path_glob must not allow the finding"
        );
    }

    #[test]
    fn is_safe_true_only_medium_findings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![medium_finding()]);
        assert!(
            exp.is_safe_to_write(&plan, &ExportOptions::default()),
            "medium findings must not block"
        );
    }

    #[test]
    fn is_safe_false_for_exclude_sensitive_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let profile_dir = tmp.path().join("p");
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(
            &profile_dir,
            "id_ed25519",
            b"-----BEGIN OPENSSH PRIVATE KEY-----\n",
        );
        let target = format!("{}/.ssh/id_ed25519", home.display());
        let entry = make_entry("key", "id_ed25519", &target);
        let profile = make_profile("t", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();

        assert!(matches!(
            &plan.items[0].classification,
            ItemClassification::ExcludeSensitivePath { .. }
        ));
        exp.scan(&mut plan).unwrap();

        assert!(
            !exp.is_safe_to_write(&plan, &ExportOptions::default()),
            "ExcludeSensitivePath must make plan unsafe"
        );
    }

    #[test]
    fn is_safe_true_include_secrets_overrides_high_finding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![high_finding()]);
        let opts = ExportOptions {
            include_secrets: true,
            ..ExportOptions::default()
        };
        assert!(
            exp.is_safe_to_write(&plan, &opts),
            "include_secrets must override High finding gate"
        );
    }

    #[test]
    fn is_safe_include_secrets_does_not_override_sensitive_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let profile_dir = tmp.path().join("p");
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "id_rsa", b"-----BEGIN RSA PRIVATE KEY-----\n");
        let target = format!("{}/.ssh/id_rsa", home.display());
        let entry = make_entry("key", "id_rsa", &target);
        let profile = make_profile("t", vec![entry]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();

        let opts = ExportOptions {
            include_secrets: true,
            ..ExportOptions::default()
        };
        assert!(
            !exp.is_safe_to_write(&plan, &opts),
            "include_secrets must NOT override ExcludeSensitivePath gate"
        );
    }

    #[test]
    fn is_safe_true_for_clean_plan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let profile = make_profile("t", vec![]);
        let exp = Exporter::new(&profile, home, home);
        let plan = make_simple_plan(home, vec![]);
        assert!(
            exp.is_safe_to_write(&plan, &ExportOptions::default()),
            "clean plan must be safe"
        );
    }

    // ── count_overridden_findings tests (H-2) ────────────────────────────────

    fn plan_with_opts_and_findings(
        home: &Path,
        opts: ExportOptions,
        findings: Vec<SecretFinding>,
    ) -> super::ExportPlan {
        let item = PlannedExportItem {
            entry_id: "f".to_string(),
            source: home.join("f"),
            source_canonical: Some(home.join("f")),
            target: home.join(".config").join("f"),
            rel_in_repo: Some(PathBuf::from("dotfiles/.config/f")),
            classification: ItemClassification::Include {},
            findings,
            size_bytes: Some(10),
            is_text: Some(true),
        };
        super::ExportPlan {
            profile_name: "t".to_string(),
            items: vec![item],
            options: opts,
        }
    }

    #[test]
    fn count_overridden_no_override_is_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let plan =
            plan_with_opts_and_findings(home, ExportOptions::default(), vec![high_finding()]);
        assert_eq!(
            plan.count_overridden_findings(home),
            0,
            "without allow-secret or include-secrets, overridden must be 0"
        );
    }

    #[test]
    fn count_overridden_allow_secret_rule_counts_high() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let opts = ExportOptions {
            allow_secret_rules: vec![SecretAllowance {
                rule_id: "aws-access-key-id".to_string(),
                path_glob: None,
            }],
            ..ExportOptions::default()
        };
        let plan = plan_with_opts_and_findings(home, opts, vec![high_finding()]);
        assert_eq!(
            plan.count_overridden_findings(home),
            1,
            "--allow-secret matching the finding must count as overridden"
        );
    }

    #[test]
    fn count_overridden_include_secrets_counts_high() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let opts = ExportOptions {
            include_secrets: true,
            ..ExportOptions::default()
        };
        let plan = plan_with_opts_and_findings(home, opts, vec![high_finding()]);
        assert_eq!(
            plan.count_overridden_findings(home),
            1,
            "--include-secrets must count High findings as overridden"
        );
    }

    #[test]
    fn count_overridden_medium_finding_not_counted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let opts = ExportOptions {
            include_secrets: true,
            ..ExportOptions::default()
        };
        let plan = plan_with_opts_and_findings(home, opts, vec![medium_finding()]);
        assert_eq!(
            plan.count_overridden_findings(home),
            0,
            "Medium findings are never overridden (only High ones are blocked)"
        );
    }

    #[test]
    fn count_overridden_mismatched_rule_id_is_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let opts = ExportOptions {
            allow_secret_rules: vec![SecretAllowance {
                rule_id: "github-token".to_string(),
                path_glob: None,
            }],
            ..ExportOptions::default()
        };
        let plan = plan_with_opts_and_findings(home, opts, vec![high_finding()]);
        assert_eq!(
            plan.count_overridden_findings(home),
            0,
            "--allow-secret for a different rule must not count the finding"
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

    // ── render_readme tests ───────────────────────────────────────────────────

    fn make_full_profile() -> crate::profile::Profile {
        use crate::profile::{Dependencies, WmKind, WmSpec};
        let mut p = make_profile("my-rice", vec![]);
        p.profile.description = Some("My hyprland rice.".to_string());
        p.profile.author = Some("alice".to_string());
        p.profile.tags = vec!["hyprland".to_string(), "dark".to_string()];
        p.wm = Some(WmSpec {
            kind: WmKind::Hyprland,
            version: None,
        });
        p.dependencies = Dependencies {
            pacman: vec!["hyprland".to_string(), "waybar".to_string()],
            aur: vec!["paru".to_string()],
            optional_pacman: vec!["grim".to_string()],
        };
        p
    }

    #[test]
    fn render_readme_full_profile_has_all_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "zshrc", b"# zsh config\n");

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let mut profile = make_full_profile();
        profile.files = vec![entry];

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "0.5.0").unwrap();

        assert!(
            readme.contains("## Dependencies"),
            "must have Dependencies section"
        );
        assert!(readme.contains("hyprland"), "pacman deps must appear");
        assert!(readme.contains("paru"), "aur deps must appear");
        assert!(
            readme.contains("## Files included"),
            "must have files section"
        );
        assert!(
            readme.contains("dotfiles/.zshrc"),
            "included file path must appear"
        );
        assert!(
            readme.contains("## Installation"),
            "must have Installation section"
        );
        assert!(readme.contains("v0.5.0"), "version must appear");
        // Author and tags
        assert!(readme.contains("alice"), "author must appear");
        assert!(readme.contains("`hyprland`"), "tag must appear");
    }

    #[test]
    fn render_readme_no_deps_shows_none_declared() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let profile = make_profile("empty-rice", vec![]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "0.5.0").unwrap();

        assert!(
            readme.contains("_None declared._"),
            "no deps → must show 'None declared.'"
        );
        // Must NOT have empty pacman install command
        assert!(
            !readme.contains("sudo pacman -S \n"),
            "must not emit empty pacman line"
        );
    }

    #[test]
    fn render_readme_no_exclusions_omits_excluded_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "hypr.conf", b"monitor=,preferred,auto,1\n");
        let target = format!("{}/.config/hypr/hyprland.conf", home.display());
        let entry = make_entry("hypr", "hypr.conf", &target);
        let profile = make_profile("clean-rice", vec![entry]);

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "0.5.0").unwrap();

        assert!(
            !readme.contains("## Files excluded"),
            "no exclusions → section must be omitted"
        );
    }

    #[test]
    fn render_readme_has_exclusions_shows_excluded_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "id_rsa", b"-----BEGIN RSA PRIVATE KEY-----\n");
        let target = format!("{}/.ssh/id_rsa", home.display());
        let entry = make_entry("key", "id_rsa", &target);
        let profile = make_profile("sec-rice", vec![entry]);

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "0.5.0").unwrap();

        assert!(
            readme.contains("## Files excluded"),
            "sensitive path exclusion → section must appear"
        );
    }

    #[test]
    fn render_readme_version_and_date_in_footer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let profile = make_profile("footer-test", vec![]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "1.2.3").unwrap();

        assert!(readme.contains("v1.2.3"), "version must be in footer");
        // Date is present somewhere (format YYYY-MM-DD)
        let has_date = readme.lines().any(|l| {
            l.len() >= 10
                && l.chars().next().is_some_and(|c| c.is_ascii_digit())
                && l.chars().nth(4) == Some('-')
        }) || readme.contains("2026")
            || readme.contains("202");
        assert!(has_date, "date must appear in README");
    }

    #[test]
    fn render_readme_description_injection_prevented() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let mut profile = make_profile("test", vec![]);
        profile.profile.description = Some("# pwned\n## injected".to_string());
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let readme = exp.render_readme(&plan, "0.5.0").unwrap();

        // Must not contain unescaped headings from user description
        for line in readme.lines() {
            if line == "# my-rice"
                || line.starts_with("## ")
                    && !line.contains("Dependencies")
                    && !line.contains("Screenshots")
                    && !line.contains("Files")
                    && !line.contains("Installation")
                    && !line.contains("How this")
                    && !line.contains("Adding screenshots")
            {
                continue; // These are legitimate headings from the template
            }
            assert!(
                !line.trim_start().starts_with("# pwned"),
                "heading injection must be blocked; got: {line:?}"
            );
        }
    }

    // ── write integration tests ───────────────────────────────────────────────

    fn setup_write_test() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        crate::profile::Profile,
        crate::profile::FileEntry,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "zshrc", b"# zsh\nexport PATH=$PATH:~/bin\n");

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test-rice", vec![entry.clone()]);
        (tmp, home, profile_dir, profile, entry)
    }

    #[test]
    fn write_happy_path_full_creates_expected_files() {
        let (tmp, home, profile_dir, profile, _) = setup_write_test();
        let output_dir = tmp.path().join("out");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let report = exp
            .write(&plan, &output_dir, &ExportOptions::default(), false)
            .unwrap();

        assert!(
            output_dir.join("README.md").exists(),
            "README.md must exist"
        );
        assert!(
            output_dir.join("archdots-profile.toml").exists(),
            "profile.toml must exist"
        );
        assert!(
            output_dir.join("install.sh").exists(),
            "install.sh must exist"
        );
        assert!(
            output_dir.join(".gitignore").exists(),
            ".gitignore must exist"
        );
        assert!(
            output_dir.join("dotfiles/.zshrc").exists(),
            "dotfiles/.zshrc must exist"
        );
        assert_eq!(report.items_included, 1);

        // Verify no staging dir left behind
        let orphans: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("archdots-export.tmp")
            })
            .collect();
        assert!(orphans.is_empty(), "no staging dir must be left behind");
    }

    #[test]
    fn write_full_profile_toml_has_rewritten_sources() {
        let (tmp, home, profile_dir, profile, _) = setup_write_test();
        let output_dir = tmp.path().join("out");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        exp.write(&plan, &output_dir, &ExportOptions::default(), false)
            .unwrap();

        let toml_path = output_dir.join("archdots-profile.toml");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let parsed: crate::profile::Profile = toml::from_str(&content).unwrap();

        assert_eq!(parsed.files.len(), 1, "exported profile must have 1 entry");
        let src = &parsed.files[0].source;
        assert!(
            src.starts_with("dotfiles"),
            "exported source must start with dotfiles/; got {src:?}"
        );
    }

    #[test]
    fn write_profile_only_skips_dotfiles_and_install() {
        let (tmp, home, profile_dir, profile, _) = setup_write_test();
        let output_dir = tmp.path().join("out");

        let opts = ExportOptions {
            format: ExportFormat::ProfileOnly,
            ..ExportOptions::default()
        };
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&opts).unwrap();
        exp.write(&plan, &output_dir, &opts, false).unwrap();

        assert!(
            output_dir.join("README.md").exists(),
            "README.md must exist"
        );
        assert!(
            output_dir.join("archdots-profile.toml").exists(),
            "profile.toml must exist"
        );
        assert!(
            !output_dir.join("dotfiles").exists(),
            "dotfiles/ must NOT exist in profile-only"
        );
        assert!(
            !output_dir.join("install.sh").exists(),
            "install.sh must NOT exist in profile-only"
        );
    }

    #[test]
    fn write_nonempty_dir_without_force_returns_error() {
        let (tmp, home, profile_dir, profile, _) = setup_write_test();
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        write_file(&output_dir, "existing.txt", b"hello\n");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let result = exp.write(&plan, &output_dir, &ExportOptions::default(), false);

        assert!(
            matches!(result, Err(crate::error::ExportError::OutputNotEmpty(_))),
            "non-empty dir without force must return OutputNotEmpty; got {result:?}"
        );
    }

    #[test]
    fn write_nonempty_dir_with_force_succeeds_no_orphan() {
        let (tmp, home, profile_dir, profile, _) = setup_write_test();
        let output_dir = tmp.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        write_file(&output_dir, "existing.txt", b"hello\n");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        exp.write(&plan, &output_dir, &ExportOptions::default(), true)
            .unwrap();

        // Existing unrelated file preserved
        assert!(
            output_dir.join("existing.txt").exists(),
            "pre-existing file must be preserved with force"
        );
        assert!(
            output_dir.join("README.md").exists(),
            "README.md must be written"
        );

        // No orphan staging dir
        let orphans: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("archdots-export.tmp")
            })
            .collect();
        assert!(orphans.is_empty(), "no orphan staging dir");
    }

    #[test]
    fn write_unsafe_plan_returns_unsafe_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("p");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        write_file(&profile_dir, "z", b"export KEY=AKIAIOSFODNN7EXAMPLE\n");
        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("z", "z", &target);
        let profile = make_profile("sec", vec![entry]);

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let mut plan = exp.plan(&ExportOptions::default()).unwrap();
        exp.scan(&mut plan).unwrap();

        let output_dir = tmp.path().join("out");
        let result = exp.write(&plan, &output_dir, &ExportOptions::default(), false);
        assert!(
            matches!(result, Err(crate::error::ExportError::Unsafe)),
            "plan with High finding must return Unsafe; got {result:?}"
        );
        assert!(
            !output_dir.exists(),
            "output dir must not be created on Unsafe error"
        );
    }

    #[test]
    fn write_executable_bit_preserved() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        let script = profile_dir.join("script.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&script, perms).unwrap();

        let target = format!("{}/.local/bin/script.sh", home.display());
        let entry = make_entry("script", "script.sh", &target);
        let profile = make_profile("test", vec![entry]);
        let output_dir = tmp.path().join("out");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        exp.write(&plan, &output_dir, &ExportOptions::default(), false)
            .unwrap();

        let dst = output_dir.join("dotfiles/.local/bin/script.sh");
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "executable bit must be preserved");
    }

    #[test]
    fn write_symlink_source_is_copied_as_regular_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        let real = write_file(&real_dir, "zshrc", b"# real zshrc\n");
        let link = profile_dir.join("zshrc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let target = format!("{}/.zshrc", home.display());
        let entry = make_entry("zshrc", "zshrc", &target);
        let profile = make_profile("test", vec![entry]);
        let output_dir = tmp.path().join("out");

        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        exp.write(&plan, &output_dir, &ExportOptions::default(), false)
            .unwrap();

        let dst = output_dir.join("dotfiles/.zshrc");
        assert!(dst.exists(), "dotfiles/.zshrc must exist");
        assert!(
            !dst.is_symlink(),
            "output file must be a regular file, not a symlink"
        );
        let content = std::fs::read_to_string(&dst).unwrap();
        assert_eq!(content, "# real zshrc\n");
    }

    #[test]
    fn write_output_dir_is_regular_file_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Create a regular file where output_dir should go
        let output_dir = tmp.path().join("out.txt");
        std::fs::write(&output_dir, b"i am a file\n").unwrap();

        let profile = make_profile("test", vec![]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let result = exp.write(&plan, &output_dir, &ExportOptions::default(), false);

        assert!(
            matches!(result, Err(crate::error::ExportError::Io { .. })),
            "output path being a regular file must return Io error"
        );
    }

    #[test]
    fn write_parent_missing_returns_io_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let profile_dir = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();

        // Parent dir doesn't exist
        let output_dir = tmp.path().join("nonexistent-parent").join("out");

        let profile = make_profile("test", vec![]);
        let exp = Exporter::new(&profile, &profile_dir, &home);
        let plan = exp.plan(&ExportOptions::default()).unwrap();
        let result = exp.write(&plan, &output_dir, &ExportOptions::default(), false);

        assert!(
            matches!(result, Err(crate::error::ExportError::Io { .. })),
            "missing parent must return Io error; got {result:?}"
        );
    }
}
