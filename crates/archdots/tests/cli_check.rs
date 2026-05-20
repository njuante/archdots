//! CLI integration tests for `archdots check`.
//!
//! These tests do NOT require a real `pacman` installation. Tests that depend
//! on actual package-manager output are covered by the core validator tests.

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

    fn write_minimal_profile(&self, name: &str) {
        let dir = self.profiles_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            r#"schema_version = 1

[profile]
name = "{name}"

[dependencies]

[hooks]
"#
        );
        std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
    }
}

// ── Test 1: check without profile argument → clap error ──────────────────────

#[test]
fn check_without_profile_arg_fails() {
    let env = TestEnv::new();
    env.cmd().arg("check").assert().failure();
}

// ── Test 2: check nonexistent profile → error, exit nonzero ──────────────────

#[test]
fn check_nonexistent_profile_fails_with_helpful_message() {
    let env = TestEnv::new();
    env.cmd()
        .args(["check", "nonexistent-profile"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("nonexistent-profile")
                .or(predicate::str::contains("not found")),
        );
}

// ── Test 3: --help mentions all flags ────────────────────────────────────────

#[test]
fn check_help_mentions_all_flags() {
    let env = TestEnv::new();
    env.cmd()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--strict"))
        .stdout(predicate::str::contains("--deep"))
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--verbose"));
}

// ── Test 4: pacman missing → exit 3 with clear message ───────────────────────
//
// Guards against the regression where PackageDB::new() was lazy and the error
// only surfaced inside validator.validate(), which was previously not caught.

#[test]
fn check_exits_3_when_pacman_missing() {
    let env = TestEnv::new();
    env.write_minimal_profile("check-test");

    // Force PATH that does not contain pacman.
    let output = env
        .cmd()
        .env("PATH", "/nonexistent")
        .args(["check", "--json", "check-test"])
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(-1);

    assert_eq!(
        exit_code,
        3,
        "expected exit 3 when pacman is missing; got {exit_code}.\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pacman") || stderr.contains("Arch"),
        "stderr must mention pacman or Arch; got: {stderr}"
    );
}

// ── Test 5: valid profile, --json output shape ───────────────────────────────
//
// On non-Arch hosts (no pacman): exit 3 with a clear "pacman not found" message.
// On Arch hosts: exit code in [0,1,2,3] and JSON is valid.

#[test]
fn check_json_output_valid_or_pacman_missing_exit3() {
    let env = TestEnv::new();
    env.write_minimal_profile("check-test");

    let output = env
        .cmd()
        .args(["check", "--json", "check-test"])
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(-1);

    match exit_code {
        3 => {
            // Non-Arch CI: pacman not found.
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("pacman") || stderr.contains("Arch"),
                "exit 3 must include an explanation mentioning pacman or Arch; got: {stderr}"
            );
        }
        0..=2 => {
            // Arch host with pacman: output must be valid JSON.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: serde_json::Value =
                serde_json::from_str(&stdout).expect("--json output must be valid JSON");
            assert_eq!(parsed["schema_version"], 1, "schema_version must be 1");
            assert!(
                parsed["profile"].is_string(),
                "profile field must be a string"
            );
            assert!(
                parsed["entries"].is_array(),
                "entries field must be an array"
            );
            assert!(
                parsed["warnings"].is_array(),
                "warnings field must be an array"
            );
            assert!(
                parsed["exit_code"].is_number(),
                "exit_code field must be present"
            );
        }
        _ => panic!("unexpected exit code {exit_code}"),
    }
}

// ── Test 6: profile with $NONEXISTENT target → exit 3 ────────────────────────
//
// On non-Arch (CI): exits 3 due to pacman missing before reaching the validator.
// On Arch: exits 3 because ValidatorError::Profile(UnknownEnvVar) propagates.
// Either way, exit 3 and a non-empty stderr are the contract.

#[test]
fn cli_check_exits_3_when_profile_has_unresolved_var() {
    let env = TestEnv::new();
    let dir = env.profiles_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let content = r#"schema_version = 1

[profile]
name = "env-var-test"

[dependencies]

[hooks]

[[files]]
id = "f1"
source = "foo.conf"
target = "$ARCHDOTS_NONEXISTENT_VAR_CLI_TEST_UNIQUE_99/foo.conf"
"#;
    std::fs::write(dir.join("env-var-test.toml"), content).unwrap();

    let output = env.cmd().args(["check", "env-var-test"]).output().unwrap();

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        3,
        "expected exit 3 for unresolved env var; got {exit_code}.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.is_empty(),
        "stderr must contain an error message; got empty"
    );
}

// ── Test 7: malformed profile TOML → exit 3 ──────────────────────────────────

#[test]
fn cli_check_exits_3_when_profile_file_is_malformed() {
    let env = TestEnv::new();
    let dir = env.profiles_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("broken.toml"), "[[[not valid toml").unwrap();

    let output = env.cmd().args(["check", "broken"]).output().unwrap();

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        3,
        "expected exit 3 for malformed profile; got {exit_code}.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "stderr must mention 'error'; got: {stderr}"
    );
}
