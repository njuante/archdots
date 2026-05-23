//! Integration tests for the validator pipeline.
//!
//! All tests use `MockRunner` so no real `pacman` process is spawned.

#[path = "common/mod.rs"]
mod common;

use std::path::PathBuf;

use archdots_core::error::ProfileError;
use archdots_core::packages::PackageDB;
use archdots_core::profile::{
    Dependencies, FileEntry, Hooks, LinkMode, Paths, Profile, ProfileMeta,
};
use archdots_core::validator::{
    DepKind, DepSource, DepStatus, ValidationWarning, Validator, ValidatorError, ValidatorOptions,
};
use common::MockRunner;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn db(runner: MockRunner) -> PackageDB {
    PackageDB::with_runner(Box::new(runner)).expect("PackageDB construction must succeed")
}

fn empty_profile(name: &str) -> Profile {
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
        files: vec![],
        dependencies: Dependencies::default(),
        hooks: Hooks::default(),
        paths: Paths::default(),
    }
}

fn symlink_entry(id: &str, source: &str, target: &str) -> FileEntry {
    FileEntry {
        id: id.to_string(),
        source: PathBuf::from(source),
        target: target.to_string(),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    }
}

// ── Test 1: empty profile ─────────────────────────────────────────────────────

#[test]
fn empty_profile_no_entries_no_warnings() {
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let report = v
        .validate(&empty_profile("empty"), ValidatorOptions::default())
        .unwrap();

    assert!(report.entries.is_empty());
    assert!(report.warnings.is_empty());
    assert_eq!(report.exit_code(), 0);
}

// ── Test 2: pacman dep installed ──────────────────────────────────────────────

#[test]
fn pacman_dep_installed_entry_installed() {
    let d = db(MockRunner::new()
        .expect_pacman_q("kitty 0.35.0-1\n")
        .expect_pacman_qm(""));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.pacman = vec!["kitty".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].name, "kitty");
    assert_eq!(report.entries[0].kind, DepKind::Pacman);
    assert_eq!(report.entries[0].status, DepStatus::Installed);
    assert_eq!(report.exit_code(), 0);
}

// ── Test 3: pacman dep missing → exit 1 ──────────────────────────────────────

#[test]
fn pacman_dep_missing_exit_code_1() {
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.pacman = vec!["kitty".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert_eq!(report.entries[0].status, DepStatus::Missing);
    assert_eq!(report.exit_code(), 1);
}

// ── Test 4: AUR dep installed → kind Aur, Installed ──────────────────────────

#[test]
fn aur_dep_installed_kind_aur() {
    let d = db(MockRunner::new()
        .expect_pacman_q("eww 0.6.0-1\n")
        .expect_pacman_qm("eww 0.6.0-1\n"));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.aur = vec!["eww".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert_eq!(report.entries[0].kind, DepKind::Aur);
    assert_eq!(report.entries[0].status, DepStatus::Installed);
}

// ── Test 5: AUR dep + no AUR helper → warning ────────────────────────────────

#[test]
fn aur_dep_no_helper_emits_warning() {
    let d = db(MockRunner::new()
        .expect_pacman_q("eww 0.6.0-1\n")
        .expect_pacman_qm("eww 0.6.0-1\n"));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.aur = vec!["eww".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        report.warnings.iter().any(|w| {
            matches!(w, ValidationWarning::AurDepsButNoHelper { aur_deps } if aur_deps == &["eww"])
        }),
        "expected AurDepsButNoHelper warning"
    );
}

// ── Test 6: optional dep missing → kind OptionalPacman, exit 2 ───────────────

#[test]
fn optional_dep_missing_exit_code_2() {
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.optional_pacman = vec!["picom".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert_eq!(report.entries[0].kind, DepKind::OptionalPacman);
    assert_eq!(report.entries[0].status, DepStatus::Missing);
    assert_eq!(report.exit_code(), 2);
}

// ── Test 7: exit_code precedence 1 > 2 > 0 ───────────────────────────────────

#[test]
fn exit_code_precedence_required_over_optional() {
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = PathBuf::from("/home/test");
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.pacman = vec!["kitty".to_string()];
    profile.dependencies.optional_pacman = vec!["picom".to_string()];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    // Both required and optional are missing; code 1 must win.
    assert_eq!(report.exit_code(), 1);
}

// ── Test 8: implicit entry from bspwmrc, picom installed ─────────────────────

#[test]
fn implicit_entry_bspwmrc_picom_installed() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "picom &\n").unwrap();

    let d = db(MockRunner::new()
        .expect_pacman_q("picom 10.3-1\n")
        .expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    let picom = report
        .entries
        .iter()
        .find(|e| e.name == "picom" && matches!(e.kind, DepKind::ImplicitFromConfig));
    assert!(picom.is_some(), "expected implicit picom entry");
    assert_eq!(picom.unwrap().status, DepStatus::Installed);
}

// ── Test 9: unknown binary without --deep → UnknownBinary ────────────────────

#[test]
fn unknown_binary_shallow_produces_unknown_binary_status() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "xyz123_unknown_bin &\n").unwrap();

    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        report.entries.iter().any(|e| {
            matches!(&e.status, DepStatus::UnknownBinary { binary } if binary == "xyz123_unknown_bin")
        }),
        "expected UnknownBinary entry"
    );
}

// ── Test 10: kitty declared, alacritty mentioned → DeclaredButUnused ─────────

#[test]
fn declared_but_unused_and_implicit_for_different_binary() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "alacritty &\n").unwrap();

    let d = db(MockRunner::new()
        .expect_pacman_q("kitty 0.35.0-1\nalacritty 0.13.0-1\n")
        .expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.dependencies.pacman = vec!["kitty".to_string()];
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        report.warnings.iter().any(|w| {
            matches!(w, ValidationWarning::DeclaredButUnused { name, .. } if name == "kitty")
        }),
        "expected DeclaredButUnused warning for kitty"
    );
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.name == "alacritty" && matches!(e.kind, DepKind::ImplicitFromConfig)),
        "expected implicit alacritty entry"
    );
}

// ── Test 11: exec $TERMINAL → UnresolvedVariable warning ─────────────────────

#[test]
fn exec_dollar_var_emits_unresolved_variable_warning() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "exec $TERMINAL\n").unwrap();

    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        report.warnings.iter().any(|w| {
            matches!(w, ValidationWarning::UnresolvedVariable { var, .. } if var == "TERMINAL")
        }),
        "expected UnresolvedVariable warning for TERMINAL"
    );
}

// ── Test 12: unreadable config → ConfigUnreadable, validate continues ─────────

#[test]
fn unreadable_config_emits_warning_and_validate_continues() {
    let tmp = TempDir::new().unwrap();
    // File is NOT created — read will fail.

    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        report
            .warnings
            .iter()
            .any(|w| matches!(w, ValidationWarning::ConfigUnreadable { .. })),
        "expected ConfigUnreadable warning"
    );
}

// ── Test 13: built-in ls filtered out ────────────────────────────────────────

#[test]
fn builtin_ls_filtered_not_in_entries() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "ls &\n").unwrap();

    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    assert!(
        !report.entries.iter().any(|e| e.name == "ls"),
        "ls must be filtered out by the noise filter"
    );
}

// ── Test 14: deep=true + pacman -F resolves binary ───────────────────────────

#[test]
fn deep_resolves_binary_via_pacman_f() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    std::fs::write(&bspwmrc, "xyz_deep_tool &\n").unwrap();

    let d = db(MockRunner::new()
        .expect_pacman_q("xyz-deep-pkg 1.0-1\n")
        .expect_pacman_qm("")
        .expect_pacman_f(
            "xyz_deep_tool",
            "extra/xyz-deep-pkg 1.0-1 [installed]\n    usr/bin/xyz_deep_tool\n",
            "",
        ));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let opts = ValidatorOptions {
        strict: false,
        deep: true,
    };
    let report = v.validate(&profile, opts).unwrap();

    let entry = report.entries.iter().find(|e| e.name == "xyz-deep-pkg");
    assert!(entry.is_some(), "expected implicit entry for xyz-deep-pkg");
    assert_eq!(entry.unwrap().status, DepStatus::Installed);
}

// ── Test 15: deep=true + file db not synced → warning only once ──────────────

#[test]
fn deep_files_db_not_synced_warning_emitted_once() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    // Two distinct unknown binaries — both trigger FilesDbNotSynced.
    std::fs::write(&bspwmrc, "unknown_a &\nunknown_b &\n").unwrap();

    let d = db(MockRunner::new()
        .expect_pacman_q("")
        .expect_pacman_qm("")
        .expect_pacman_f("unknown_a", "", "error: files database is empty")
        .expect_pacman_f("unknown_b", "", "error: files database is empty"));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let opts = ValidatorOptions {
        strict: false,
        deep: true,
    };
    let report = v.validate(&profile, opts).unwrap();

    let count = report
        .warnings
        .iter()
        .filter(|w| matches!(w, ValidationWarning::PacmanFilesDbNotSynced))
        .count();
    assert_eq!(
        count, 1,
        "PacmanFilesDbNotSynced must be emitted exactly once"
    );
}

// ── Test 16: 50 mentions → 1 entry with mentions.len() == 50 ─────────────────

#[test]
fn fifty_mentions_one_entry_with_all_mentions() {
    let tmp = TempDir::new().unwrap();
    let bspwmrc = tmp.path().join("bspwm").join("bspwmrc");
    std::fs::create_dir_all(bspwmrc.parent().unwrap()).unwrap();
    let content: String = (0..50).map(|_| "picom &\n").collect();
    std::fs::write(&bspwmrc, &content).unwrap();

    let d = db(MockRunner::new()
        .expect_pacman_q("picom 10.3-1\n")
        .expect_pacman_qm(""));
    let home = tmp.path().to_path_buf();
    let v = Validator::new(&d, &home);

    let mut profile = empty_profile("rice");
    profile.files = vec![symlink_entry(
        "bspwmrc",
        "bspwm/bspwmrc",
        "~/.config/bspwm/bspwmrc",
    )];

    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    let picom = report.entries.iter().find(|e| e.name == "picom").unwrap();
    if let DepSource::InferredFromConfig { mentions } = &picom.source {
        assert_eq!(mentions.len(), 50, "expected 50 located mentions for picom");
    } else {
        panic!("expected InferredFromConfig source for picom");
    }
}

// ── Test 17: duplicate AUR deps are deduplicated ─────────────────────────────

#[test]
fn validate_dedupes_aur_deps() {
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();

    let mut profile = empty_profile("dedup-aur-test");
    profile.dependencies.aur = vec!["swww".to_string(), "swww".to_string()];

    let v = Validator::new(&d, &home);
    let report = v.validate(&profile, ValidatorOptions::default()).unwrap();

    let aur_entries: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.kind == DepKind::Aur)
        .collect();
    assert_eq!(
        aur_entries.len(),
        1,
        "duplicate AUR dep must be deduplicated to one entry"
    );
    assert_eq!(aur_entries[0].name, "swww");

    let swww_unused: Vec<_> = report
        .warnings
        .iter()
        .filter(
            |w| matches!(w, ValidationWarning::DeclaredButUnused { name, .. } if name == "swww"),
        )
        .collect();
    assert_eq!(
        swww_unused.len(),
        1,
        "duplicate AUR dep must produce exactly one DeclaredButUnused warning"
    );
}

// ── Test 19: $VAR in profile target propagates as ValidatorError::Profile ──────

#[test]
fn validate_propagates_unknown_env_var_from_target() {
    // ARCHDOTS_NONEXISTENT_VAR_FOR_TEST_UNIQUE is virtually guaranteed to be
    // absent from any real environment; we use it to trigger UnknownEnvVar.
    let d = db(MockRunner::new().expect_pacman_q("").expect_pacman_qm(""));
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();

    let mut profile = empty_profile("env-var-test");
    profile.files = vec![symlink_entry(
        "f1",
        "dummy.conf",
        "$ARCHDOTS_NONEXISTENT_VAR_FOR_TEST_UNIQUE/foo.conf",
    )];

    let v = Validator::new(&d, &home);
    let err = v
        .validate(&profile, ValidatorOptions::default())
        .unwrap_err();

    assert!(
        matches!(
            err,
            ValidatorError::Profile(ProfileError::UnknownEnvVar { .. })
        ),
        "expected ValidatorError::Profile(UnknownEnvVar), got {err:?}"
    );
}
