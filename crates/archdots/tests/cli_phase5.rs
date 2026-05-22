//! CLI integration tests for `archdots export` (Phase 5).
//!
//! Each test gets an isolated temporary $HOME and $XDG_CONFIG_HOME.
//! No real pacman, no real $HOME, no network.

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

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("archdots").unwrap();
        cmd.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .arg("--log-level")
            .arg("off");
        cmd
    }

    fn profiles_dir(&self) -> std::path::PathBuf {
        self.config.path().join("archdots/profiles")
    }

    /// Write a profile with one innocuous text file that has no secrets.
    fn write_clean_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();

        // Source file: a harmless shell config.
        let source_name = format!("{name}-zshrc");
        std::fs::write(
            dir.join(&source_name),
            "# .zshrc — no secrets\nexport PATH=\"$HOME/bin:$PATH\"\n",
        )
        .unwrap();

        let content = format!(
            r#"schema_version = 1

[profile]
name = "{name}"

[dependencies]

[hooks]

[[files]]
id = "zshrc"
source = "{source_name}"
target = "~/.zshrc"
"#
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }

    /// Write a profile whose single source file contains an AWS access key
    /// (triggers the `aws-access-key-id` High-severity rule).
    fn write_secrets_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let source_name = format!("{name}-env");
        // AKIAIOSFODNN7EXAMPLE matches AKIA[0-9A-Z]{16}.
        std::fs::write(
            dir.join(&source_name),
            "# env\nAWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
        )
        .unwrap();

        let content = format!(
            r#"schema_version = 1

[profile]
name = "{name}"

[dependencies]

[hooks]

[[files]]
id = "envfile"
source = "{source_name}"
target = "~/.env"
"#
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }

    /// Write a profile whose single source file is clean but whose target
    /// resolves under ~/.ssh/ — triggering the `ssh` prefix rule in
    /// sensitive_paths.toml regardless of any secret-content flags.
    fn write_ssh_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let source_name = format!("{name}-sshconfig");
        std::fs::write(
            dir.join(&source_name),
            "# ssh config — no secrets in content\nHost *\n  ServerAliveInterval 60\n",
        )
        .unwrap();

        let content = format!(
            r#"schema_version = 1

[profile]
name = "{name}"

[dependencies]

[hooks]

[[files]]
id = "sshconfig"
source = "{source_name}"
target = "~/.ssh/config"
"#
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }
}

// ── 1. --help mentions all §E flags ──────────────────────────────────────────

#[test]
fn export_help_exit_0_and_lists_flags() {
    let env = TestEnv::new();
    env.cmd()
        .args(["export", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--output").or(predicate::str::contains("-o")))
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--format"))
        .stdout(predicate::str::contains("--allow-path"))
        .stdout(predicate::str::contains("--allow-binary"))
        .stdout(predicate::str::contains("--allow-secret"))
        .stdout(predicate::str::contains("--max-bytes"))
        .stdout(predicate::str::contains("--include-secrets"))
        .stdout(predicate::str::contains("--check"))
        .stdout(predicate::str::contains("--no-install-script"))
        .stdout(predicate::str::contains("--no-readme"))
        .stdout(predicate::str::contains("--yes").or(predicate::str::contains("-y")))
        .stdout(predicate::str::contains("--json"));
}

// ── 2. Missing profile → exit 3, non-empty stderr ────────────────────────────

#[test]
fn export_missing_profile_exits_3_with_message() {
    let env = TestEnv::new();
    let output = env
        .cmd()
        .args(["export", "no-such-profile-xyz"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(
        !String::from_utf8_lossy(&output.stderr).is_empty(),
        "stderr must be non-empty"
    );
}

// ── 3. --check on a clean profile → exit 0, no output_dir created ────────────

#[test]
fn export_check_clean_profile_exits_0_and_no_output_dir() {
    let env = TestEnv::new();
    env.write_clean_profile("clean-check");

    let output_dir = env.home.path().join("clean-check-export");

    let output = env
        .cmd()
        .args([
            "export",
            "clean-check",
            "--check",
            "--output",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output_dir.exists(),
        "--check must not create the output directory"
    );
}

// ── 4. --check on a profile with AWS key → exit 2, finding in report ─────────

#[test]
fn export_check_secrets_profile_exits_2_with_finding() {
    let env = TestEnv::new();
    env.write_secrets_profile("secrets-check");

    let output_dir = env.home.path().join("secrets-check-export");

    let output = env
        .cmd()
        .args([
            "export",
            "secrets-check",
            "--check",
            "--output",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "exit 2 expected when High findings block export\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output_dir.exists(),
        "--check must not create output directory even on findings"
    );
}

// ── 5. --include-secrets on non-TTY stdin → exit 3, "requires a TTY" ─────────

#[test]
fn export_include_secrets_non_tty_exits_3() {
    let env = TestEnv::new();
    // assert_cmd invokes the binary with a pipe for stdin (non-TTY).
    let output = env
        .cmd()
        .args(["export", "any-profile", "--include-secrets"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "exit 3 expected for --include-secrets on non-TTY\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires a TTY"),
        "stderr must mention 'requires a TTY'; got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── 6. --json --check → valid JSON, schema_version=1, classification objects ──

#[test]
fn export_json_check_parses_and_has_correct_shape() {
    let env = TestEnv::new();
    env.write_clean_profile("json-check");

    let output_dir = env.home.path().join("json-check-export");

    let output = env
        .cmd()
        .args([
            "export",
            "json-check",
            "--json",
            "--check",
            "--output",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output must be valid JSON");

    assert_eq!(parsed["schema_version"], 1, "schema_version must be 1");
    assert!(parsed["profile"].is_string(), "profile must be a string");
    assert!(parsed["items"].is_array(), "items must be an array");
    assert!(parsed["wrote"].is_boolean(), "wrote must be a boolean");
    assert_eq!(
        parsed["wrote"], false,
        "wrote must be false in --check mode"
    );
    assert!(parsed["summary"].is_object(), "summary must be an object");
    assert!(
        parsed["exit_code"].is_number(),
        "exit_code must be a number"
    );

    // Every item's classification must be an object (never a bare string).
    for item in parsed["items"].as_array().unwrap_or(&vec![]) {
        let cls = &item["classification"];
        assert!(
            cls.is_object(),
            "classification must be a JSON object, got: {cls}"
        );
    }
}

// ── 7. --format profile-only + --include-secrets → exit 3 ────────────────────

#[test]
fn export_profile_only_plus_include_secrets_exits_3() {
    let env = TestEnv::new();
    // Flag validation happens before profile loading, so no profile needed.
    let output = env
        .cmd()
        .args([
            "export",
            "any-profile",
            "--format",
            "profile-only",
            "--include-secrets",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "exit 3 expected for profile-only + include-secrets\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── 8. Happy path: --yes on a clean profile → exit 0, expected files written ──

#[test]
fn export_happy_path_creates_output_files_and_no_tmp_dirs() {
    let env = TestEnv::new();
    env.write_clean_profile("happy");

    let output_dir = env.home.path().join("happy-out");

    let output = env
        .cmd()
        .args([
            "export",
            "happy",
            "--yes",
            "--output",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "exit 0 expected for clean happy-path export\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output_dir.join("README.md").exists(),
        "README.md must exist in output"
    );
    assert!(
        output_dir.join("archdots-profile.toml").exists(),
        "archdots-profile.toml must exist in output"
    );
    // The source file for "zshrc" maps to ~/.zshrc, which lands at
    // dotfiles/.zshrc inside the export.
    assert!(
        output_dir.join("dotfiles/.zshrc").exists(),
        "dotfiles/.zshrc must exist in output"
    );

    // No staging dirs must remain in output_dir's parent (§3 atomicity).
    let parent = output_dir.parent().unwrap();
    let leftover_tmp: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".archdots-export.tmp.")
        })
        .collect();
    assert!(
        leftover_tmp.is_empty(),
        "no staging dirs should remain after export; found: {:?}",
        leftover_tmp.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

// ── 9. sensitive-path filter always blocks export (§A.5, Gate 1) ─────────────

#[test]
fn export_include_secrets_does_not_bypass_sensitive_path() {
    let env = TestEnv::new();
    env.write_ssh_profile("ssh-path");

    let output_dir = env.home.path().join("ssh-path-export");

    // The source file is clean (no High findings), but the target ~/.ssh/config
    // matches the `ssh` prefix rule in sensitive_paths.toml.
    // is_safe_to_write Gate 1 (ExcludeSensitivePath) is independent of
    // include_secrets — it always returns false for sensitive-path items.
    // Exit code must be 2, not 0.
    let output = env
        .cmd()
        .args([
            "export",
            "ssh-path",
            "--check",
            "--output",
            output_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "sensitive-path filter must block export; \
         expected exit 2\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output_dir.exists(),
        "--check must not create the output directory"
    );
}
