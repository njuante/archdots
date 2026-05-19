//! Integration tests for Phase 2 CLI subcommands: apply, diff, rollback,
//! history, snapshots, recover.
//!
//! Each test spawns the binary with fully isolated XDG directories so the
//! real user environment is never touched.

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ── test environment ──────────────────────────────────────────────────────────

struct TestEnv {
    home: TempDir,
    config: TempDir,
    data: TempDir,
    state: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            config: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("archdots").unwrap();
        cmd.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_DATA_HOME", self.data.path())
            .env("XDG_STATE_HOME", self.state.path())
            .arg("--log-level")
            .arg("off");
        cmd
    }

    /// Create a file (and missing parent dirs) under `$HOME`.
    fn touch(&self, rel: &str) -> PathBuf {
        let abs = self.home.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, format!("# {rel}")).unwrap();
        abs
    }

    fn profiles_dir(&self) -> PathBuf {
        self.config.path().join("archdots/profiles")
    }

    /// Write a profile with no file entries.
    fn write_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "schema_version = 1\n\n[profile]\nname = \"{name}\"\n\n[dependencies]\n\n[hooks]\n"
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }

    /// Write a profile with one [[files]] entry.
    ///
    /// `source` is relative to `$HOME` (profile_dir used at apply time).
    /// `target` is the `~`-prefixed path of the symlink destination.
    fn write_profile_with_file(&self, name: &str, source: &str, target: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "schema_version = 1\n\n[profile]\nname = \"{name}\"\n\n[[files]]\nid = \"f1\"\nsource = \"{source}\"\ntarget = \"{target}\"\n\n[dependencies]\n\n[hooks]\n"
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }
}

// ── apply ─────────────────────────────────────────────────────────────────────

#[test]
fn test_apply_profile_not_found() {
    let env = TestEnv::new();
    env.cmd()
        .args(["apply", "nonexistent", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn test_apply_dry_run_no_changes() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"));

    assert!(
        !env.home.path().join(".zshrc").exists(),
        "dry-run must not create the symlink"
    );
}

#[test]
fn test_apply_creates_symlink() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("linked"));

    let link = env.home.path().join(".zshrc");
    assert!(link.exists(), "symlink must be created");
    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        ".zshrc must be a symlink"
    );
}

#[test]
fn test_apply_idempotent() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();
    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already-owned").or(predicate::str::contains("skipped")));
}

#[test]
fn test_apply_reports_journal_id() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Journal ID:"));
}

#[test]
fn test_apply_conflict_source_missing_exits_nonzero() {
    let env = TestEnv::new();
    // Profile references a source file that does not exist.
    env.write_profile_with_file("rice", "dotfiles/missing", "~/.missing");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .failure();
}

// ── diff ──────────────────────────────────────────────────────────────────────

#[test]
fn test_diff_profile_not_found() {
    let env = TestEnv::new();
    env.cmd()
        .args(["diff", "nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn test_diff_missing_target_shows_message() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["diff", "rice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("missing"));
}

#[test]
fn test_diff_owned_symlink_shows_no_diff() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();

    env.cmd()
        .args(["diff", "rice"])
        .assert()
        .success()
        .stdout(predicate::str::contains("owned").or(predicate::str::contains("no diff")));
}

// ── rollback ──────────────────────────────────────────────────────────────────

#[test]
fn test_rollback_no_history_fails() {
    let env = TestEnv::new();
    env.write_profile("rice");

    env.cmd()
        .args(["rollback", "--profile", "rice", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rice"));
}

#[test]
fn test_rollback_with_yes_after_apply() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();

    assert!(env.home.path().join(".zshrc").exists());

    env.cmd()
        .args(["rollback", "--profile", "rice", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Rollback complete"));
}

// ── history ───────────────────────────────────────────────────────────────────

#[test]
fn test_history_empty() {
    let env = TestEnv::new();
    env.cmd()
        .args(["history"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no entries"));
}

#[test]
fn test_history_after_apply() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();

    env.cmd()
        .args(["history"])
        .assert()
        .success()
        .stdout(predicate::str::contains("apply"))
        .stdout(predicate::str::contains("rice"));
}

#[test]
fn test_history_profile_filter() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();

    env.cmd()
        .args(["history", "--profile", "other"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no entries"));
}

// ── snapshots ─────────────────────────────────────────────────────────────────

#[test]
fn test_snapshots_list_empty() {
    let env = TestEnv::new();
    env.cmd()
        .args(["snapshots", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no snapshots"));
}

#[test]
fn test_snapshots_list_after_apply() {
    let env = TestEnv::new();
    env.touch("dotfiles/zshrc");
    env.write_profile_with_file("rice", "dotfiles/zshrc", "~/.zshrc");

    env.cmd()
        .args(["apply", "rice", "--yes"])
        .assert()
        .success();

    env.cmd()
        .args(["snapshots", "list"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn test_snapshots_show_short_prefix_fails() {
    let env = TestEnv::new();
    env.cmd()
        .args(["snapshots", "show", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("6"));
}

#[test]
fn test_snapshots_prune_no_policy_fails() {
    let env = TestEnv::new();
    env.cmd()
        .args(["snapshots", "prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--keep").or(predicate::str::contains("--older-than")));
}

// ── recover ───────────────────────────────────────────────────────────────────

#[test]
fn test_recover_no_orphans() {
    let env = TestEnv::new();
    env.cmd()
        .args(["recover", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No orphaned transactions"));
}
