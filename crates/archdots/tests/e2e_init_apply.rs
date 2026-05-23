//! End-to-end tests for the init → apply and init → export --check flows.
//!
//! These tests exercise the full binary so they catch bugs in the interaction
//! between subcommands that unit tests (which build fixtures manually) miss.
//!
//! Originally written to guard COR-01 (the v0.5.1 CircularSymlink incident).
//! From v0.6.0 onward, `init` copies dotfiles into a managed staging
//! directory under `$XDG_DATA_HOME` and writes `[paths] source_root` into the
//! profile, so init → apply now succeeds cleanly and produces real symlinks
//! back into `$HOME`.

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

/// `archdots init` then `archdots apply` must succeed and produce a real
/// symlink from `$HOME` into the managed staging directory.
#[test]
fn init_then_apply_links_to_managed_dir() {
    let env = TestEnv::new();

    let bashrc = env.home.path().join(".bashrc");
    std::fs::write(&bashrc, "# my bashrc\n").unwrap();

    env.cmd()
        .args(["init", "--name", "rice"])
        .assert()
        .success();

    let profile_path = env.profiles_dir().join("rice.toml");
    assert!(profile_path.exists(), "rice.toml must be created by init");

    // The profile must declare a [paths] source_root pointing at the
    // managed staging dir. Without it we would regress to the v0.5
    // CircularSymlink behaviour.
    let profile_content = std::fs::read_to_string(&profile_path).unwrap();
    assert!(
        profile_content.contains("[paths]") && profile_content.contains("source_root"),
        "profile must include [paths] source_root\n--- got ---\n{profile_content}"
    );

    // The copy must exist under $XDG_DATA_HOME.
    let managed_copy = env
        .data
        .path()
        .join("archdots/profiles/rice/dotfiles/.bashrc");
    assert!(
        managed_copy.exists(),
        "init must copy .bashrc into the managed staging dir at {}",
        managed_copy.display()
    );
    assert_eq!(
        std::fs::read_to_string(&managed_copy).unwrap(),
        "# my bashrc\n",
        "managed copy content must match the original"
    );

    // apply must succeed cleanly now that source and target differ.
    let output = env.cmd().args(["apply", "rice", "--yes"]).output().unwrap();
    let code = output.status.code().unwrap_or(-1);
    assert_eq!(
        code,
        0,
        "apply must exit 0 after the v0.6 init redesign\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The target must now be a symlink and resolve to the managed copy.
    let meta = bashrc.symlink_metadata().unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "apply must replace ~/.bashrc with a symlink"
    );
    let canonical_target = std::fs::canonicalize(&bashrc).unwrap();
    let canonical_managed = std::fs::canonicalize(&managed_copy).unwrap();
    assert_eq!(
        canonical_target, canonical_managed,
        "symlink must resolve to the managed copy"
    );
    // Content stays readable through the symlink.
    assert_eq!(
        std::fs::read_to_string(&bashrc).unwrap(),
        "# my bashrc\n",
        "content must still be readable through the symlink"
    );
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
    let output = env.cmd().args(["apply", "bad", "--yes"]).output().unwrap();

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
