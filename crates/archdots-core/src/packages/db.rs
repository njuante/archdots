use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use once_cell::sync::OnceCell;

use crate::error::PackageError;

use super::providers::{load_curated_providers, CuratedProvider};
use super::runner::{CommandRunner, SystemRunner};

/// A single installed package as reported by `pacman -Q`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkg {
    /// Package name.
    pub name: String,
    /// Installed version string.
    pub version: String,
    /// Whether the package originates from a repo or the AUR.
    pub source: PkgSource,
}

/// Origin of an installed package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PkgSource {
    /// Present in `pacman -Q` but absent from `pacman -Qm`.
    Repo,
    /// Present in `pacman -Qm` (foreign / AUR package).
    Aur,
}

/// Detected AUR helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AurHelper {
    /// `paru` is installed.
    Paru,
    /// `yay` is installed.
    Yay,
}

/// Result of resolving a binary name to an Arch package.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderHit {
    /// Resolved via the embedded curated table.
    Curated {
        /// Arch package that provides the binary.
        package: String,
        /// Source of the package.
        source: PkgSource,
    },
    /// Resolved via `pacman -F` (only when `deep == true`).
    PacmanFiles {
        /// Arch package that provides the binary.
        package: String,
        /// Source of the package.
        source: PkgSource,
    },
    /// No mapping known.
    Unknown,
    /// `deep` was requested but the pacman file database is not synced.
    /// The user must run `sudo pacman -Fy` before `--deep` can resolve binaries.
    FilesDbNotSynced,
}

/// Wrapper over `pacman -Q` / `pacman -Qm` / `pacman -F`.
///
/// All queries are memoised for the lifetime of the instance. Caches use
/// interior mutability (`OnceCell`, `RefCell`) so the public API is
/// immutably borrowable. `PackageDB` is single-threaded by design.
pub struct PackageDB {
    runner: Box<dyn CommandRunner>,
    lock_path: PathBuf,
    installed: OnceCell<HashMap<String, Pkg>>,
    aur: OnceCell<HashSet<String>>,
    aur_helper: OnceCell<Option<AurHelper>>,
    providers_curated: HashMap<String, CuratedProvider>,
    provider_cache: RefCell<HashMap<String, ProviderHit>>,
}

impl PackageDB {
    /// Construct using the real system command runner.
    ///
    /// # Errors
    /// Returns [`PackageError::CuratedTableParseError`] if the embedded
    /// `binary_providers.toml` cannot be parsed (should never happen in a
    /// release build).
    pub fn new() -> Result<Self, PackageError> {
        Self::with_runner(Box::new(SystemRunner))
    }

    /// Construct with an injected runner (used by tests via `MockRunner`).
    ///
    /// # Errors
    /// Returns [`PackageError::CuratedTableParseError`] if the embedded
    /// `binary_providers.toml` cannot be parsed.
    pub fn with_runner(runner: Box<dyn CommandRunner>) -> Result<Self, PackageError> {
        let providers_curated = load_curated_providers()?;
        Ok(Self {
            runner,
            lock_path: PathBuf::from("/var/lib/pacman/db.lck"),
            installed: OnceCell::new(),
            aur: OnceCell::new(),
            aur_helper: OnceCell::new(),
            providers_curated,
            provider_cache: RefCell::new(HashMap::new()),
        })
    }

    /// Returns `true` if `name` is currently installed.
    ///
    /// # Errors
    /// Propagates any error from [`Self::installed`].
    pub fn is_installed(&self, name: &str) -> Result<bool, PackageError> {
        Ok(self.installed()?.contains_key(name))
    }

    /// Returns the installed `Pkg` for `name`, or `None` if not installed.
    ///
    /// The returned `Pkg.source` reflects the correct AUR / repo classification.
    ///
    /// # Errors
    /// Propagates errors from [`Self::installed`] or [`Self::aur_packages`].
    pub fn lookup(&self, name: &str) -> Result<Option<Pkg>, PackageError> {
        let Some(mut pkg) = self.installed()?.get(name).cloned() else {
            return Ok(None);
        };
        if self.aur_packages()?.contains(name) {
            pkg.source = PkgSource::Aur;
        }
        Ok(Some(pkg))
    }

    /// Returns the full installed-packages map (`pacman -Q`), lazily populated.
    ///
    /// All entries carry `source = Repo` by default; use [`Self::lookup`] for
    /// accurate AUR classification.
    ///
    /// # Errors
    /// Returns [`PackageError::DatabaseLocked`] if `/var/lib/pacman/db.lck`
    /// exists, [`PackageError::PacmanMissing`] if `pacman` is not on `PATH`,
    /// [`PackageError::PacmanFailed`] on a non-zero exit, or
    /// [`PackageError::UnparseableOutput`] for malformed output lines.
    pub fn installed(&self) -> Result<&HashMap<String, Pkg>, PackageError> {
        self.installed.get_or_try_init(|| self.fetch_installed())
    }

    /// Returns the set of AUR (foreign) package names (`pacman -Qm`), lazily populated.
    ///
    /// # Errors
    /// Returns [`PackageError::PacmanFailed`] on non-zero exit or
    /// [`PackageError::UnparseableOutput`] for malformed lines.
    pub fn aur_packages(&self) -> Result<&HashSet<String>, PackageError> {
        self.aur.get_or_try_init(|| self.fetch_aur())
    }

    /// Returns the detected AUR helper, or `None` if neither `paru` nor `yay`
    /// is installed. When both are present, `Paru` takes precedence.
    ///
    /// # Errors
    /// Propagates errors from [`Self::installed`].
    pub fn detect_aur_helper(&self) -> Result<Option<AurHelper>, PackageError> {
        self.aur_helper
            .get_or_try_init(|| -> Result<Option<AurHelper>, PackageError> {
                let installed = self.installed()?;
                if installed.contains_key("paru") {
                    Ok(Some(AurHelper::Paru))
                } else if installed.contains_key("yay") {
                    Ok(Some(AurHelper::Yay))
                } else {
                    Ok(None)
                }
            })
            .copied()
    }

    /// Resolves `binary` to the Arch package that provides it.
    ///
    /// 1. Curated table lookup (always).
    /// 2. `pacman -F /usr/bin/<binary>` (only when `deep == true`).
    /// 3. `ProviderHit::Unknown` as fallback.
    ///
    /// Results are memoised per binary.
    ///
    /// # Errors
    /// Propagates errors from [`Self::aur_packages`] or the underlying runner.
    pub fn provider_of(&self, binary: &str, deep: bool) -> Result<ProviderHit, PackageError> {
        if let Some(cached) = self.provider_cache.borrow().get(binary) {
            return Ok(cached.clone());
        }
        let hit = self.resolve_provider(binary, deep)?;
        self.provider_cache
            .borrow_mut()
            .insert(binary.to_string(), hit.clone());
        Ok(hit)
    }

    /// Returns the number of entries in the curated binary→package table.
    pub fn curated_binary_count(&self) -> usize {
        self.providers_curated.len()
    }

    // ── private helpers ────────────────────────────────────────────────────────

    fn fetch_installed(&self) -> Result<HashMap<String, Pkg>, PackageError> {
        if self.lock_path.exists() {
            return Err(PackageError::DatabaseLocked);
        }
        let output = self.runner.run("pacman", &["-Q"])?;
        if output.status != 0 {
            return Err(PackageError::PacmanFailed {
                code: output.status,
                stderr: output.stderr,
            });
        }
        parse_pacman_q_output(&output.stdout)
    }

    fn fetch_aur(&self) -> Result<HashSet<String>, PackageError> {
        let output = self.runner.run("pacman", &["-Qm"])?;
        if output.status != 0 {
            return Err(PackageError::PacmanFailed {
                code: output.status,
                stderr: output.stderr,
            });
        }
        parse_pacman_qm_output(&output.stdout)
    }

    fn resolve_provider(&self, binary: &str, deep: bool) -> Result<ProviderHit, PackageError> {
        if let Some(curated) = self.providers_curated.get(binary) {
            let source =
                if curated.explicitly_aur || self.aur_packages()?.contains(&curated.package) {
                    PkgSource::Aur
                } else {
                    PkgSource::Repo
                };
            return Ok(ProviderHit::Curated {
                package: curated.package.clone(),
                source,
            });
        }

        if !deep {
            return Ok(ProviderHit::Unknown);
        }

        let path = format!("/usr/bin/{binary}");
        let output = self.runner.run("pacman", &["-F", &path])?;

        let stderr_lower = output.stderr.to_lowercase();
        if stderr_lower.contains("files database is empty")
            || stderr_lower.contains("use 'pacman -fy'")
            || stderr_lower.contains("sync the database first")
        {
            return Ok(ProviderHit::FilesDbNotSynced);
        }

        let first_line = output.stdout.lines().next().unwrap_or("").trim();
        if first_line.is_empty() {
            return Ok(ProviderHit::Unknown);
        }

        // Parse "<repo>/<name> <version> [installed]"
        if let Some(slash) = first_line.find('/') {
            let after_slash = &first_line[slash + 1..];
            if let Some(pkg_name) = after_slash.split_whitespace().next() {
                let source = if self.aur_packages()?.contains(pkg_name) {
                    PkgSource::Aur
                } else {
                    PkgSource::Repo
                };
                return Ok(ProviderHit::PacmanFiles {
                    package: pkg_name.to_string(),
                    source,
                });
            }
        }

        Ok(ProviderHit::Unknown)
    }
}

fn parse_pacman_q_output(stdout: &str) -> Result<HashMap<String, Pkg>, PackageError> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        let version = tokens
            .next()
            .ok_or_else(|| PackageError::UnparseableOutput(line.to_string()))?;
        map.insert(
            name.to_string(),
            Pkg {
                name: name.to_string(),
                version: version.to_string(),
                source: PkgSource::Repo,
            },
        );
    }
    Ok(map)
}

fn parse_pacman_qm_output(stdout: &str) -> Result<HashSet<String>, PackageError> {
    let mut set = HashSet::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        if tokens.next().is_none() {
            return Err(PackageError::UnparseableOutput(line.to_string()));
        }
        set.insert(name.to_string());
    }
    Ok(set)
}
