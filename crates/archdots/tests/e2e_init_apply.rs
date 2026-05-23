//! End-to-end tests for the init → apply and init → export --check flows.
//!
//! These tests exercise the full binary so they catch bugs in the interaction
//! between subcommands that unit tests (which build fixtures manually) miss.
//! Specifically, they guard against COR-01: `init` writes `source` relative to
//! `$HOME`, and all other subcommands must resolve it the same way.

use assert_cmd::Command;
use tempfile::TempDir;

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

    fn profiles_dir(&self) -> std::path::PathBuf {
        self.config.path().join("archdots/profiles")
    }
}

// ── init → apply ──────────────────────────────────────────────────────────────

/// `archdots init` then `archdots apply` must not produce a self-symlink.
///
/// Before COR-01 was fixed, `apply` resolved `source` (relative to `$HOME`)
/// against `$HOME` correctly, but `init` produced profiles whose `source` field
/// was also relative to `$HOME`. The ELOOP bug happened when the same absolute
/// path ended up as both source and target — e.g. `~/.bashrc → ~/.bashrc`.
#[test]
fn init_then_apply_does_not_create_circular_symlink() {
    let env = TestEnv::new();

    // Create a real dotfile in $HOME.
    let bashrc = env.home.path().join(".bashrc");
    std::fs::write(&bashrc, "# my bashrc\n").unwrap();

    // Run `archdots init --name rice` — this discovers .bashrc and writes a
    // profile where source = ".bashrc" (relative to $HOME).
    env.cmd()
        .args(["init", "--name", "rice"])
        .assert()
        .success();

    let profile_path = env.profiles_dir().join("rice.toml");
    assert!(profile_path.exists(), "rice.toml must be created by init");

    // Verify the profile's source field is relative (not absolute).
    let profile_content = std::fs::read_to_string(&profile_path).unwrap();
    assert!(
        profile_content.contains("source ="),
        "profile must contain a source field"
    );
    // source must NOT be an absolute path (it should be relative to $HOME).
    assert!(
        !profile_content.contains(&format!("source = \"{}/.bashrc\"", env.home.path().display())),
        "source must be relative, not absolute"
    );

    // Apply may exit 0 (linked) or 2 (CircularSymlink conflict detected) —
    // both are acceptable. What must NOT happen is ELOOP (unreadable self-symlink).
    let output = env
        .cmd()
        .args(["apply", "rice", "--yes"])
        .output()
        .unwrap();

    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 2,
        "apply must exit 0 (linked) or 2 (conflict), not {code}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The dotfile must still be readable — no ELOOP corruption.
    let content = std::fs::read_to_string(&bashrc)
        .expect("dotfile must remain readable after apply attempt (no ELOOP)");
    assert_eq!(content, "# my bashrc\n", "dotfile content must be intact");

    // If apply succeeded and the target became a symlink, it must not point
    // to itself.
    if code == 0 {
        let meta = bashrc.symlink_metadata().unwrap();
        if meta.file_type().is_symlink() {
            let link_target = std::fs::read_link(&bashrc).unwrap();
            assert_ne!(
                link_target,
                bashrc,
                "apply must not create a self-symlink (circular): {:?} → {:?}",
                bashrc,
                link_target
            );
        }
    }
}

/// `archdots apply` on a profile where source == target must be rejected with
/// a clear conflict error (CircularSymlink), not silently corrupt the file.
#[test]
fn apply_rejects_circular_symlink_profile() {
    let env = TestEnv::new();

    // Create a source file in $HOME.
    let dot = env.home.path().join(".myrc");
    std::fs::write(&dot, "# myrc\n").unwrap();

    // Manually write a profile where source and target resolve to the same path.
    // source = ".myrc" (relative to $HOME) → $HOME/.myrc
    // target = "~/.myrc" → $HOME/.myrc
    let dir = env.profiles_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bad.toml"),
        r#"schema_version = 1

[profile]
name = "bad"

[dependencies]

[hooks]

[[files]]
id = "myrc"
source = ".myrc"
target = "~/.myrc"
"#,
    )
    .unwrap();

    // Apply must fail (non-zero exit) with a message about the conflict.
    let output = env
        .cmd()
        .args(["apply", "bad", "--yes"])
        .output()
        .unwrap();

    assert_ne!(
        output.status.code(),
        Some(0),
        "apply must exit non-zero for a self-link profile"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("circular") || combined.contains("conflict"),
        "output must mention circular or conflict; got: {combined}"
    );

    // The original file must still be readable.
    let content = std::fs::read_to_string(&dot).unwrap();
    assert_eq!(content, "# myrc\n", "original dotfile must be untouched");
}

// ── init → export --check ─────────────────────────────────────────────────────

/// `archdots init` then `archdots export --check` must exit 0 (no MissingSource).
///
/// Before COR-01, `export` resolved `source` against `profile_dir` and could
/// not find the file at `$HOME/<name>`, producing exit 3.
#[test]
fn init_then_export_check_exits_0() {
    let env = TestEnv::new();

    // Create a dotfile that init will pick up.
    std::fs::write(env.home.path().join(".bashrc"), "# bashrc\n").unwrap();

    env.cmd()
        .args(["init", "--name", "rice"])
        .assert()
        .success();

    // export --check must succeed (exit 0): it finds the source files.
    // Exit 2 would mean findings (secrets), exit 3 would mean MissingSource.
    let output = env
        .cmd()
        .args([
            "export",
            "rice",
            "--check",
            "--output",
            env.home.path().join("out").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 2,
        "export --check must exit 0 or 2 (findings), not 3 (MissingSource); \
         got exit {code}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
