//! Integration tests for `archdots_core::profile`.
//!
//! All filesystem operations use `tempfile::TempDir` — never touches `$HOME`.

use std::path::{Path, PathBuf};

use archdots_core::error::ProfileError;
use archdots_core::profile::{
    Dependencies, FileEntry, Hooks, LinkMode, Profile, ProfileMeta, ResolveCtx, WmKind, WmSpec,
    CURRENT_SCHEMA_VERSION,
};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn minimal_profile() -> Profile {
    Profile {
        schema_version: CURRENT_SCHEMA_VERSION,
        profile: ProfileMeta {
            name: String::from("test-rice"),
            description: Some(String::from("test profile")),
            author: None,
            created_at: None,
            tags: vec![String::from("test")],
        },
        wm: None,
        files: vec![FileEntry {
            id: String::from("zshrc"),
            source: PathBuf::from("zsh/zshrc"),
            target: String::from("~/.zshrc"),
            mode: LinkMode::Symlink,
            exec: false,
            required: true,
        }],
        dependencies: Dependencies::default(),
        hooks: Hooks::default(),
    }
}

fn profile_toml_content() -> &'static str {
    r#"schema_version = 1

[profile]
name = "test-rice"
description = "test profile"
tags = ["test"]

[[files]]
id = "zshrc"
source = "zsh/zshrc"
target = "~/.zshrc"
mode = "symlink"
exec = false
required = true

[dependencies]

[hooks]
"#
}

fn fake_ctx(home: &Path) -> ResolveCtx<'_> {
    ResolveCtx {
        home,
        env: &|_| None,
    }
}

// ── load ─────────────────────────────────────────────────────────────────────

#[test]
fn test_load_from_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    std::fs::write(&path, profile_toml_content()).unwrap();

    let p = Profile::load_from_file(&path).expect("should load valid profile");
    assert_eq!(p.profile.name, "test-rice");
    assert_eq!(p.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(p.files.len(), 1);
    assert_eq!(p.files[0].id, "zshrc");
}

#[test]
fn test_load_missing_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.toml");
    let err = Profile::load_from_file(&path).unwrap_err();
    assert!(matches!(err, ProfileError::Io { .. }));
}

#[test]
fn test_load_invalid_toml() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    std::fs::write(&path, b"not valid toml {{{{").unwrap();
    let err = Profile::load_from_file(&path).unwrap_err();
    assert!(matches!(err, ProfileError::ParseToml(_)));
}

// ── save ─────────────────────────────────────────────────────────────────────

#[test]
fn test_save_to_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    let p = minimal_profile();

    p.save_to_file(&path).expect("should save");
    assert!(path.exists(), "file should exist after save");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("test-rice"));
    assert!(content.contains("schema_version"));
}

#[test]
fn test_save_leaves_no_tmp_on_success() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    minimal_profile().save_to_file(&path).unwrap();

    let tmp = dir.path().join("profile.tmp");
    assert!(!tmp.exists(), "tmp file should be cleaned up after rename");
}

// ── roundtrip ────────────────────────────────────────────────────────────────

#[test]
fn test_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    let original = minimal_profile();

    original.save_to_file(&path).unwrap();
    let loaded = Profile::load_from_file(&path).unwrap();

    assert_eq!(original, loaded);
}

#[test]
fn test_roundtrip_with_wm() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("profile.toml");
    let mut p = minimal_profile();
    p.wm = Some(WmSpec {
        kind: WmKind::Hyprland,
        version: Some(String::from(">=0.40")),
    });
    p.dependencies.pacman.push(String::from("hyprland"));
    p.dependencies.aur.push(String::from("waybar-hyprland"));

    p.save_to_file(&path).unwrap();
    let loaded = Profile::load_from_file(&path).unwrap();
    assert_eq!(p, loaded);
    assert_eq!(loaded.wm.as_ref().unwrap().kind, WmKind::Hyprland);
}

// ── validate ─────────────────────────────────────────────────────────────────

#[test]
fn test_validate_invalid_name_empty() {
    let mut p = minimal_profile();
    p.profile.name = String::new();
    assert!(matches!(p.validate(), Err(ProfileError::InvalidName(_))));
}

#[test]
fn test_validate_invalid_name_uppercase() {
    let mut p = minimal_profile();
    p.profile.name = String::from("MyProfile");
    assert!(matches!(p.validate(), Err(ProfileError::InvalidName(_))));
}

#[test]
fn test_validate_invalid_name_spaces() {
    let mut p = minimal_profile();
    p.profile.name = String::from("my profile");
    assert!(matches!(p.validate(), Err(ProfileError::InvalidName(_))));
}

#[test]
fn test_validate_valid_name_with_hyphens_and_numbers() {
    let mut p = minimal_profile();
    p.profile.name = String::from("my-rice-01");
    assert!(p.validate().is_ok());
}

#[test]
fn test_validate_duplicate_file_id() {
    let mut p = minimal_profile();
    p.files.push(FileEntry {
        id: String::from("zshrc"), // duplicate
        source: PathBuf::from("zsh/zshrc2"),
        target: String::from("~/.zshrc2"),
        mode: LinkMode::Symlink,
        exec: false,
        required: false,
    });
    assert!(matches!(
        p.validate(),
        Err(ProfileError::DuplicateFileId(_))
    ));
}

#[test]
fn test_validate_unsupported_schema() {
    let mut p = minimal_profile();
    p.schema_version = 99;
    assert!(matches!(
        p.validate(),
        Err(ProfileError::UnsupportedSchema { .. })
    ));
}

#[test]
fn test_validate_empty_source() {
    let mut p = minimal_profile();
    p.files[0].source = PathBuf::new();
    assert!(matches!(p.validate(), Err(ProfileError::EmptyPath { .. })));
}

#[test]
fn test_validate_empty_target() {
    let mut p = minimal_profile();
    p.files[0].target = String::new();
    assert!(matches!(p.validate(), Err(ProfileError::EmptyPath { .. })));
}

#[test]
fn test_validate_source_escapes() {
    let mut p = minimal_profile();
    p.files[0].source = PathBuf::from("../../etc/passwd");
    assert!(matches!(p.validate(), Err(ProfileError::SourceEscape(_))));
}

#[test]
fn test_validate_source_absolute_is_rejected() {
    let mut p = minimal_profile();
    p.files[0].source = PathBuf::from("/etc/passwd");
    assert!(matches!(p.validate(), Err(ProfileError::SourceEscape(_))));
}

// ── resolve_target ────────────────────────────────────────────────────────────

#[test]
fn test_resolve_target_tilde() {
    let home = PathBuf::from("/home/testuser");
    let ctx = fake_ctx(&home);
    let entry = FileEntry {
        id: String::from("zshrc"),
        source: PathBuf::from("zsh/zshrc"),
        target: String::from("~/.zshrc"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let resolved = Profile::resolve_target(&entry, &ctx).unwrap();
    assert_eq!(resolved, PathBuf::from("/home/testuser/.zshrc"));
}

#[test]
fn test_resolve_target_tilde_alone() {
    let home = PathBuf::from("/home/testuser");
    let ctx = fake_ctx(&home);
    let entry = FileEntry {
        id: String::from("home"),
        source: PathBuf::from("home"),
        target: String::from("~"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let resolved = Profile::resolve_target(&entry, &ctx).unwrap();
    assert_eq!(resolved, PathBuf::from("/home/testuser"));
}

#[test]
fn test_resolve_target_env_var() {
    let home = PathBuf::from("/home/testuser");
    let ctx = ResolveCtx {
        home: &home,
        env: &|var| match var {
            "XDG_CONFIG_HOME" => Some(String::from("/home/testuser/.config")),
            _ => None,
        },
    };
    let entry = FileEntry {
        id: String::from("polybar"),
        source: PathBuf::from("polybar/config.ini"),
        target: String::from("$XDG_CONFIG_HOME/polybar/config.ini"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let resolved = Profile::resolve_target(&entry, &ctx).unwrap();
    assert_eq!(
        resolved,
        PathBuf::from("/home/testuser/.config/polybar/config.ini")
    );
}

#[test]
fn test_resolve_target_braced_env_var() {
    let home = PathBuf::from("/home/testuser");
    let ctx = ResolveCtx {
        home: &home,
        env: &|var| match var {
            "XDG_CONFIG_HOME" => Some(String::from("/home/testuser/.config")),
            _ => None,
        },
    };
    let entry = FileEntry {
        id: String::from("alacritty"),
        source: PathBuf::from("alacritty/alacritty.toml"),
        target: String::from("${XDG_CONFIG_HOME}/alacritty/alacritty.toml"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let resolved = Profile::resolve_target(&entry, &ctx).unwrap();
    assert_eq!(
        resolved,
        PathBuf::from("/home/testuser/.config/alacritty/alacritty.toml")
    );
}

#[test]
fn test_resolve_target_unknown_env_var() {
    let home = PathBuf::from("/home/testuser");
    let ctx = fake_ctx(&home);
    let entry = FileEntry {
        id: String::from("foo"),
        source: PathBuf::from("foo"),
        target: String::from("$UNKNOWN_VAR/foo"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let err = Profile::resolve_target(&entry, &ctx).unwrap_err();
    assert!(matches!(err, ProfileError::UnknownEnvVar { .. }));
}

#[test]
fn test_resolve_target_non_absolute_after_expansion() {
    let home = PathBuf::from("/home/testuser");
    let ctx = fake_ctx(&home);
    let entry = FileEntry {
        id: String::from("foo"),
        source: PathBuf::from("foo"),
        target: String::from("relative/path"),
        mode: LinkMode::Symlink,
        exec: false,
        required: true,
    };
    let err = Profile::resolve_target(&entry, &ctx).unwrap_err();
    assert!(matches!(err, ProfileError::NonAbsoluteTarget(_)));
}

// ── validate hooks ────────────────────────────────────────────────────────────

#[test]
fn test_validate_hook_pre_apply_escape() {
    let mut p = minimal_profile();
    p.hooks.pre_apply.push(PathBuf::from("../../evil.sh"));
    assert!(matches!(p.validate(), Err(ProfileError::SourceEscape(_))));
}

#[test]
fn test_validate_hook_post_apply_escape() {
    let mut p = minimal_profile();
    p.hooks
        .post_apply
        .push(PathBuf::from("../../../etc/shadow"));
    assert!(matches!(p.validate(), Err(ProfileError::SourceEscape(_))));
}

#[test]
fn test_validate_hook_absolute_rejected() {
    let mut p = minimal_profile();
    p.hooks.pre_apply.push(PathBuf::from("/etc/malicious.sh"));
    assert!(matches!(p.validate(), Err(ProfileError::SourceEscape(_))));
}

#[test]
fn test_validate_hook_relative_ok() {
    let mut p = minimal_profile();
    p.hooks.pre_apply.push(PathBuf::from("hooks/pre.sh"));
    p.hooks.post_apply.push(PathBuf::from("hooks/post.sh"));
    assert!(p.validate().is_ok());
}

// ── roundtrip all variants ────────────────────────────────────────────────────

#[test]
fn test_roundtrip_all_wm_kinds() {
    use archdots_core::profile::{WmKind, WmSpec};
    let dir = TempDir::new().unwrap();
    for kind in [
        WmKind::Bspwm,
        WmKind::Hyprland,
        WmKind::I3,
        WmKind::Sway,
        WmKind::None,
    ] {
        let mut p = minimal_profile();
        p.wm = Some(WmSpec {
            kind,
            version: None,
        });
        let path = dir.path().join(format!("wm-{kind}.toml"));
        p.save_to_file(&path).unwrap();
        let loaded = Profile::load_from_file(&path).unwrap();
        assert_eq!(loaded.wm.unwrap().kind, kind);
    }
}

#[test]
fn test_roundtrip_all_link_modes() {
    let dir = TempDir::new().unwrap();
    for mode in [LinkMode::Symlink, LinkMode::Copy, LinkMode::Template] {
        let mut p = minimal_profile();
        p.files[0].mode = mode;
        let path = dir.path().join(format!("mode-{mode}.toml"));
        p.save_to_file(&path).unwrap();
        let loaded = Profile::load_from_file(&path).unwrap();
        assert_eq!(loaded.files[0].mode, mode);
    }
}

// ── save_to_file with missing parent ─────────────────────────────────────────

#[test]
fn test_save_to_file_missing_parent_fails() {
    let dir = TempDir::new().unwrap();
    // Parent directory does not exist — save must fail, not panic.
    let path = dir.path().join("nonexistent/subdir/profile.toml");
    let err = minimal_profile().save_to_file(&path).unwrap_err();
    assert!(matches!(err, ProfileError::Io { .. }));
}

// ── resolved_entries ──────────────────────────────────────────────────────────

#[test]
fn test_resolved_entries_basic() {
    let dir = TempDir::new().unwrap();
    let home = PathBuf::from("/home/testuser");
    let ctx = fake_ctx(&home);
    let p = minimal_profile();
    let results: Vec<_> = p.resolved_entries(dir.path(), &ctx).collect();
    assert_eq!(results.len(), 1);
    let (entry, src, tgt) = results[0].as_ref().unwrap();
    assert_eq!(entry.id, "zshrc");
    assert_eq!(src, &dir.path().join("zsh/zshrc"));
    assert_eq!(tgt, &PathBuf::from("/home/testuser/.zshrc"));
}

// ── resolve_source ────────────────────────────────────────────────────────────

#[test]
fn test_resolve_source_joins_profile_dir() {
    let dir = TempDir::new().unwrap();
    let entry = FileEntry {
        id: String::from("bspwmrc"),
        source: PathBuf::from("bspwm/bspwmrc"),
        target: String::from("~/.config/bspwm/bspwmrc"),
        mode: LinkMode::Symlink,
        exec: true,
        required: true,
    };
    let resolved = Profile::resolve_source(&entry, dir.path()).unwrap();
    assert_eq!(resolved, dir.path().join("bspwm/bspwmrc"));
}

#[test]
fn test_resolve_source_escape_rejected() {
    let dir = TempDir::new().unwrap();
    let entry = FileEntry {
        id: String::from("evil"),
        source: PathBuf::from("../../../etc/shadow"),
        target: String::from("/tmp/shadow"),
        mode: LinkMode::Copy,
        exec: false,
        required: true,
    };
    let err = Profile::resolve_source(&entry, dir.path()).unwrap_err();
    assert!(matches!(err, ProfileError::SourceEscape(_)));
}
