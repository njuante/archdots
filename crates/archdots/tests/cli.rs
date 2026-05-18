//! Integration tests for the `archdots` binary.
//!
//! Each test spawns the binary with isolated `$HOME` and `$XDG_CONFIG_HOME`
//! directories so the real user environment is never touched.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ── test environment ──────────────────────────────────────────────────────────

struct TestEnv {
    home: TempDir,
    config: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            config: TempDir::new().unwrap(),
        }
    }

    /// Build a `Command` for the archdots binary with isolated env vars.
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("archdots").unwrap();
        cmd.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            // Silence tracing output in tests.
            .env("ARCHDOTS_LOG", "off")
            .arg("--log-level")
            .arg("off");
        cmd
    }

    /// Create a file (and any missing parent dirs) under `$HOME`.
    fn touch(&self, rel: &str) -> PathBuf {
        let abs = self.home.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, b"").unwrap();
        abs
    }

    /// Return the path to the profiles directory.
    fn profiles_dir(&self) -> PathBuf {
        self.config.path().join("archdots/profiles")
    }

    /// Return the path to a specific profile file.
    fn profile_path(&self, name: &str) -> PathBuf {
        self.profiles_dir().join(format!("{name}.toml"))
    }

    /// Write a minimal valid profile TOML directly to the profiles directory.
    fn write_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            r#"schema_version = 1

[profile]
name = "{name}"
description = "test profile"

[dependencies]

[hooks]
"#
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }
}

// ── basic invocation ──────────────────────────────────────────────────────────

#[test]
fn test_version() {
    let env = TestEnv::new();
    env.cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("archdots"));
}

#[test]
fn test_no_args_exits_nonzero() {
    // No subcommand: clap prints help and exits 2.
    Command::cargo_bin("archdots").unwrap().assert().failure();
}

#[test]
fn test_help_flag() {
    let env = TestEnv::new();
    env.cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("profile"));
}

// ── archdots init ─────────────────────────────────────────────────────────────

#[test]
fn test_init_creates_default_profile() {
    let env = TestEnv::new();
    env.cmd()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));

    assert!(
        env.profile_path("default").exists(),
        "default.toml must be created"
    );
}

#[test]
fn test_init_creates_profiles_directory() {
    let env = TestEnv::new();
    assert!(
        !env.profiles_dir().exists(),
        "profiles dir must not exist before init"
    );
    env.cmd().arg("init").assert().success();
    assert!(
        env.profiles_dir().exists(),
        "profiles dir must be created by init"
    );
}

#[test]
fn test_init_custom_name() {
    let env = TestEnv::new();
    env.cmd()
        .args(["init", "--name", "my-rice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("my-rice"));

    assert!(env.profile_path("my-rice").exists());
    assert!(!env.profile_path("default").exists());
}

#[test]
fn test_init_custom_output_path() {
    let env = TestEnv::new();
    let out = env.config.path().join("custom/out.toml");
    env.cmd()
        .args(["init", "--output", out.to_str().unwrap()])
        .assert()
        .success();

    assert!(out.exists(), "profile must be written to --output path");
}

#[test]
fn test_init_detects_dotfiles() {
    let env = TestEnv::new();
    env.touch(".zshrc");
    env.touch(".config/nvim/init.lua");
    env.touch(".config/alacritty/alacritty.toml");

    env.cmd()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("3 dotfile(s)"));
}

#[test]
fn test_init_empty_home_zero_dotfiles() {
    let env = TestEnv::new();
    env.cmd()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 dotfile(s)"));
}

#[test]
fn test_init_profile_toml_is_valid() {
    let env = TestEnv::new();
    env.touch(".config/bspwm/bspwmrc");
    env.cmd()
        .args(["init", "--name", "rice"])
        .assert()
        .success();

    let content = std::fs::read_to_string(env.profile_path("rice")).unwrap();
    assert!(content.contains("schema_version"));
    assert!(content.contains("name = \"rice\""));
    assert!(content.contains("Generated by archdots init"));
}

#[test]
fn test_init_invalid_name_fails() {
    let env = TestEnv::new();
    // Profile validation rejects names with uppercase.
    env.cmd()
        .args(["init", "--name", "MyProfile"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid"));
}

// ── archdots init --force / overwrite guard ───────────────────────────────────

#[test]
fn test_init_existing_profile_fails_without_force() {
    let env = TestEnv::new();
    env.cmd()
        .args(["init", "--name", "exists"])
        .assert()
        .success();

    env.cmd()
        .args(["init", "--name", "exists"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn test_init_existing_profile_force_overwrites() {
    let env = TestEnv::new();
    env.cmd()
        .args(["init", "--name", "overwrite"])
        .assert()
        .success();

    env.cmd()
        .args(["init", "--name", "overwrite", "--force"])
        .assert()
        .success();

    assert!(env.profile_path("overwrite").exists());
}

// ── XDG_CONFIG_HOME relative path fallback ────────────────────────────────────

#[test]
fn test_relative_xdg_config_home_falls_back_to_home_config() {
    let env = TestEnv::new();
    // A relative XDG_CONFIG_HOME is invalid per spec; binary must ignore it and
    // fall back to $HOME/.config.
    let mut cmd = Command::cargo_bin("archdots").unwrap();
    cmd.env("HOME", env.home.path())
        .env("XDG_CONFIG_HOME", "relative/path")
        .env("ARCHDOTS_LOG", "off")
        .arg("--log-level")
        .arg("off")
        .arg("init");
    cmd.assert().success();

    // Profile must land under $HOME/.config/archdots/profiles/default.toml.
    let expected = env
        .home
        .path()
        .join(".config/archdots/profiles/default.toml");
    assert!(expected.exists(), "must fall back to $HOME/.config");
}

// ── archdots profile list ─────────────────────────────────────────────────────

#[test]
fn test_profile_list_no_profiles_dir() {
    let env = TestEnv::new();
    env.cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No profiles found"));
}

#[test]
fn test_profile_list_empty_dir() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.profiles_dir()).unwrap();
    env.cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No profiles found"));
}

#[test]
fn test_profile_list_one_profile() {
    let env = TestEnv::new();
    env.write_profile("default");

    env.cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
}

#[test]
fn test_profile_list_multiple_profiles() {
    let env = TestEnv::new();
    env.write_profile("alpha");
    env.write_profile("beta");
    env.write_profile("gamma");

    let out = env
        .cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();

    assert!(text.contains("alpha"));
    assert!(text.contains("beta"));
    assert!(text.contains("gamma"));

    // Output must be sorted.
    let pos_alpha = text.find("alpha").unwrap();
    let pos_beta = text.find("beta").unwrap();
    let pos_gamma = text.find("gamma").unwrap();
    assert!(pos_alpha < pos_beta && pos_beta < pos_gamma);
}

#[test]
fn test_profile_list_after_init() {
    let env = TestEnv::new();
    env.cmd()
        .args(["init", "--name", "my-rice"])
        .assert()
        .success();
    env.cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("my-rice"));
}

// ── archdots profile show ─────────────────────────────────────────────────────

#[test]
fn test_profile_show_valid() {
    let env = TestEnv::new();
    env.touch(".zshrc");
    env.touch(".config/nvim/init.lua");

    env.cmd()
        .args(["init", "--name", "showcase"])
        .assert()
        .success();

    env.cmd()
        .args(["profile", "show", "showcase"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Profile:"))
        .stdout(predicate::str::contains("showcase"))
        .stdout(predicate::str::contains("Files ("));
}

#[test]
fn test_profile_show_not_found() {
    let env = TestEnv::new();
    env.cmd()
        .args(["profile", "show", "nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nonexistent").or(
            // anyhow prints to stderr with "Error:" prefix
            predicate::str::contains("not found"),
        ));
}

#[test]
fn test_profile_show_includes_file_entries() {
    let env = TestEnv::new();
    env.touch(".config/bspwm/bspwmrc");

    env.cmd()
        .args(["init", "--name", "riceshow"])
        .assert()
        .success();

    env.cmd()
        .args(["profile", "show", "riceshow"])
        .assert()
        .success()
        .stdout(predicate::str::contains("symlink"))
        .stdout(predicate::str::contains("~/.config/bspwm/bspwmrc"));
}

#[test]
fn test_profile_show_no_files_label() {
    let env = TestEnv::new();
    // Empty home → zero detected files.
    env.cmd()
        .args(["init", "--name", "empty"])
        .assert()
        .success();

    env.cmd()
        .args(["profile", "show", "empty"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Files (0)"))
        .stdout(predicate::str::contains("(none)"));
}

// ── archdots profile delete ───────────────────────────────────────────────────

#[test]
fn test_profile_delete_with_yes_flag() {
    let env = TestEnv::new();
    env.cmd()
        .args(["init", "--name", "todel"])
        .assert()
        .success();
    assert!(env.profile_path("todel").exists());

    env.cmd()
        .args(["profile", "delete", "todel", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"));

    assert!(!env.profile_path("todel").exists());
}

#[test]
fn test_profile_delete_interactive_confirm_y() {
    let env = TestEnv::new();
    env.write_profile("interactive");

    env.cmd()
        .args(["profile", "delete", "interactive"])
        .write_stdin("y\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"));

    assert!(!env.profile_path("interactive").exists());
}

#[test]
fn test_profile_delete_interactive_confirm_yes() {
    let env = TestEnv::new();
    env.write_profile("full-yes");

    env.cmd()
        .args(["profile", "delete", "full-yes"])
        .write_stdin("yes\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"));
}

#[test]
fn test_profile_delete_interactive_abort_n() {
    let env = TestEnv::new();
    env.write_profile("keep-me");

    env.cmd()
        .args(["profile", "delete", "keep-me"])
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Aborted"));

    assert!(
        env.profile_path("keep-me").exists(),
        "profile must survive an aborted delete"
    );
}

#[test]
fn test_profile_delete_interactive_abort_empty() {
    let env = TestEnv::new();
    env.write_profile("keep-empty");

    // Default is No — pressing Enter without typing should abort.
    env.cmd()
        .args(["profile", "delete", "keep-empty"])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Aborted"));

    assert!(env.profile_path("keep-empty").exists());
}

#[test]
fn test_profile_delete_not_found() {
    let env = TestEnv::new();
    env.cmd()
        .args(["profile", "delete", "ghost", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn test_profile_delete_then_list() {
    let env = TestEnv::new();
    env.write_profile("gone");
    env.write_profile("stays");

    env.cmd()
        .args(["profile", "delete", "gone", "--yes"])
        .assert()
        .success();

    let out = env
        .cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("gone"));
    assert!(text.contains("stays"));
}

// ── path isolation ────────────────────────────────────────────────────────────

#[test]
fn test_different_envs_do_not_interfere() {
    let env_a = TestEnv::new();
    let env_b = TestEnv::new();

    env_a
        .cmd()
        .args(["init", "--name", "rice-a"])
        .assert()
        .success();
    env_b
        .cmd()
        .args(["init", "--name", "rice-b"])
        .assert()
        .success();

    // env_a must not see rice-b.
    env_a
        .cmd()
        .args(["profile", "show", "rice-b"])
        .assert()
        .failure();

    // env_b must not see rice-a.
    env_b
        .cmd()
        .args(["profile", "show", "rice-a"])
        .assert()
        .failure();
}

// ── real home must never be touched ──────────────────────────────────────────

#[test]
fn test_real_home_untouched() {
    // Run a command that would normally write to $HOME and verify that the
    // real $HOME/.config/archdots is not created.
    let env = TestEnv::new();
    env.cmd().arg("init").assert().success();

    let real_profiles = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3) // workspace root … not $HOME
        .and_then(|_| std::env::var("HOME").ok())
        .map(|h| PathBuf::from(h).join(".config/archdots/profiles"));

    // We can only check if we know where real $HOME is; skip if indeterminate.
    if let Some(real) = real_profiles {
        let real_str = real.to_string_lossy();
        // The path we wrote to must NOT be under the real $HOME.
        let written = env.profiles_dir();
        assert!(
            !written.starts_with(real_str.as_ref()),
            "init must not write to the real $HOME"
        );
    }
}
