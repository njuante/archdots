//! Integration tests for `archdots_core::detector`.
//!
//! All filesystem operations use `tempfile::TempDir`; `$HOME` is never touched.

use std::path::PathBuf;

use archdots_core::detector::{Category, DetectedDotfile, Detector};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_detector(home: &std::path::Path) -> Detector {
    Detector::new(home).expect("embedded catalog must parse")
}

/// Create a file (and all parent directories) inside the fake home.
fn touch(home: &std::path::Path, rel: &str) {
    let abs = home.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, b"").unwrap();
}

/// Create a symlink `link_rel -> target_rel` inside the fake home.
fn symlink(home: &std::path::Path, link_rel: &str, target_rel: &str) {
    let link = home.join(link_rel);
    let target = home.join(target_rel);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
}

fn find_entry<'a>(results: &'a [DetectedDotfile], name: &str) -> Vec<&'a DetectedDotfile> {
    results.iter().filter(|d| d.name == name).collect()
}

fn find_existing<'a>(results: &'a [DetectedDotfile], name: &str) -> Vec<&'a DetectedDotfile> {
    find_entry(results, name)
        .into_iter()
        .filter(|d| d.exists)
        .collect()
}

// ── catalog / construction ────────────────────────────────────────────────────

#[test]
fn test_catalog_parses() {
    let dir = TempDir::new().unwrap();
    // Just constructing the detector is enough to assert the catalog is valid.
    make_detector(dir.path());
}

#[test]
fn test_scan_returns_entries_for_all_known_apps() {
    let dir = TempDir::new().unwrap();
    let d = make_detector(dir.path());
    let results = d.scan();
    // With an empty home every entry has exists=false, but all are present.
    assert!(
        !results.is_empty(),
        "scan must return at least one entry even on an empty home"
    );
}

#[test]
fn test_all_categories_represented() {
    let dir = TempDir::new().unwrap();
    let d = make_detector(dir.path());
    let results = d.scan();

    let categories: std::collections::HashSet<Category> =
        results.iter().map(|r| r.category).collect();

    for expected in [
        Category::Wm,
        Category::Bar,
        Category::Launcher,
        Category::Terminal,
        Category::Shell,
        Category::Editor,
        Category::Prompt,
        Category::Compositor,
        Category::Notification,
        Category::Misc,
    ] {
        assert!(
            categories.contains(&expected),
            "category {expected:?} must have at least one entry in the catalog"
        );
    }
}

// ── exists flag ───────────────────────────────────────────────────────────────

#[test]
fn test_scan_empty_home_all_not_existing() {
    let dir = TempDir::new().unwrap();
    let d = make_detector(dir.path());
    let results = d.scan();
    assert!(
        results.iter().all(|r| !r.exists),
        "all entries must have exists=false on an empty home"
    );
}

#[test]
fn test_scan_detects_existing_file() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/bspwm/bspwmrc");

    let results = make_detector(dir.path()).scan();
    let found = find_existing(&results, "bspwm");

    assert_eq!(found.len(), 1);
    assert!(found[0].exists);
    assert!(!found[0].is_symlink);
    assert_eq!(found[0].path, dir.path().join(".config/bspwm/bspwmrc"));
}

#[test]
fn test_scan_only_existing_path_is_flagged() {
    let dir = TempDir::new().unwrap();
    // i3 has two paths: .config/i3/config and .i3/config — touch only the first.
    touch(dir.path(), ".config/i3/config");

    let results = make_detector(dir.path()).scan();
    let i3_entries = find_entry(&results, "i3");

    let existing: Vec<_> = i3_entries.iter().filter(|d| d.exists).collect();
    let missing: Vec<_> = i3_entries.iter().filter(|d| !d.exists).collect();

    assert_eq!(existing.len(), 1, "exactly one i3 path should exist");
    assert_eq!(missing.len(), 1, "the other i3 path should be missing");
    assert_eq!(existing[0].path, dir.path().join(".config/i3/config"));
    assert_eq!(missing[0].path, dir.path().join(".i3/config"));
}

// ── symlink detection ─────────────────────────────────────────────────────────

#[test]
fn test_scan_detects_symlink() {
    let dir = TempDir::new().unwrap();
    // Create a real file and a symlink pointing to it.
    touch(dir.path(), "real_zshrc");
    symlink(dir.path(), ".zshrc", "real_zshrc");

    let results = make_detector(dir.path()).scan();
    let zsh = find_existing(&results, "zsh");
    let zshrc = zsh.iter().find(|d| d.path.ends_with(".zshrc")).unwrap();

    assert!(zshrc.exists, ".zshrc must exist");
    assert!(zshrc.is_symlink, ".zshrc must be detected as a symlink");
    assert!(
        zshrc.target_if_link.is_some(),
        "symlink target must be populated"
    );
    assert_eq!(
        zshrc.target_if_link.as_ref().unwrap(),
        &dir.path().join("real_zshrc"),
    );
}

#[test]
fn test_scan_regular_file_not_symlink() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/starship.toml");

    let results = make_detector(dir.path()).scan();
    let entry = find_existing(&results, "starship");

    assert_eq!(entry.len(), 1);
    assert!(!entry[0].is_symlink);
    assert!(entry[0].target_if_link.is_none());
}

// ── path construction ─────────────────────────────────────────────────────────

#[test]
fn test_scan_paths_are_absolute() {
    let dir = TempDir::new().unwrap();
    let results = make_detector(dir.path()).scan();
    for entry in &results {
        assert!(
            entry.path.is_absolute(),
            "path {} must be absolute",
            entry.path.display()
        );
    }
}

#[test]
fn test_scan_paths_rooted_in_home() {
    let dir = TempDir::new().unwrap();
    let results = make_detector(dir.path()).scan();
    for entry in &results {
        assert!(
            entry.path.starts_with(dir.path()),
            "path {} must be inside the home dir",
            entry.path.display()
        );
    }
}

// ── specific known apps ───────────────────────────────────────────────────────

#[test]
fn test_hyprland_detected() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/hypr/hyprland.conf");
    let results = make_detector(dir.path()).scan();
    let found = find_existing(&results, "hyprland");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].category, Category::Wm);
}

#[test]
fn test_neovim_detected() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/nvim/init.lua");
    let results = make_detector(dir.path()).scan();
    let found = find_existing(&results, "neovim");
    assert!(!found.is_empty());
    assert_eq!(found[0].category, Category::Editor);
}

#[test]
fn test_starship_detected() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/starship.toml");
    let results = make_detector(dir.path()).scan();
    let found = find_existing(&results, "starship");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].category, Category::Prompt);
}

#[test]
fn test_multiple_apps_detected_together() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/bspwm/bspwmrc");
    touch(dir.path(), ".config/polybar/config.ini");
    touch(dir.path(), ".config/alacritty/alacritty.toml");
    touch(dir.path(), ".zshrc");
    touch(dir.path(), ".config/nvim/init.lua");
    touch(dir.path(), ".config/dunst/dunstrc");

    let results = make_detector(dir.path()).scan();
    let existing: Vec<_> = results.iter().filter(|r| r.exists).collect();

    // At least one entry per app we created
    assert!(!find_existing(&results, "bspwm").is_empty());
    assert!(!find_existing(&results, "polybar").is_empty());
    assert!(!find_existing(&results, "alacritty").is_empty());
    assert!(!find_existing(&results, "zsh").is_empty());
    assert!(!find_existing(&results, "neovim").is_empty());
    assert!(!find_existing(&results, "dunst").is_empty());

    // Non-touched apps must still be not-existing
    assert!(find_existing(&results, "sway").is_empty());
    assert!(find_existing(&results, "vim").is_empty());

    // Total existing count must be exactly 6 paths created
    assert_eq!(existing.len(), 6);
}

// ── DetectedDotfile fields ────────────────────────────────────────────────────

#[test]
fn test_detected_dotfile_name_matches_catalog() {
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".config/kitty/kitty.conf");
    let results = make_detector(dir.path()).scan();
    let kitty: Vec<&DetectedDotfile> = results.iter().filter(|d| d.name == "kitty").collect();
    assert!(!kitty.is_empty());
    assert_eq!(kitty[0].name, "kitty");
    assert_eq!(kitty[0].category, Category::Terminal);
}

#[test]
fn test_scan_with_different_home_dirs() {
    // Two detectors pointing at different TempDirs must not interfere.
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    touch(dir_a.path(), ".zshrc");

    let results_a = make_detector(dir_a.path()).scan();
    let results_b = make_detector(dir_b.path()).scan();

    assert!(!find_existing(&results_a, "zsh").is_empty());
    assert!(find_existing(&results_b, "zsh").is_empty());
}

// ── path type ────────────────────────────────────────────────────────────────

#[test]
fn test_detected_path_type() {
    // DetectedDotfile.path must be PathBuf, not &Path.
    let dir = TempDir::new().unwrap();
    touch(dir.path(), ".tmux.conf");
    let results = make_detector(dir.path()).scan();
    let tmux = find_existing(&results, "tmux");
    let path: PathBuf = tmux[0].path.clone(); // clone works only on PathBuf
    assert!(path.ends_with(".tmux.conf"));
}
