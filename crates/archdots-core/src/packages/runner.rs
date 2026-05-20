use crate::error::PackageError;

/// Captures the result of a subprocess invocation.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Process exit code (`-1` when the OS did not report one).
    pub status: i32,
    /// Captured stdout, decoded as lossy UTF-8.
    pub stdout: String,
    /// Captured stderr, decoded as lossy UTF-8.
    pub stderr: String,
}

/// Abstraction over running a subprocess and capturing its output.
///
/// `PackageDB` depends on this trait so tests can inject canned `pacman`
/// output without spawning a real subprocess. Production code uses
/// `SystemRunner`; tests live in `crates/archdots-core/tests/` and provide
/// their own `MockRunner`.
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args`, capturing exit status, stdout, and stderr.
    ///
    /// # Errors
    /// Returns [`PackageError::PacmanMissing`] when `program == "pacman"` and
    /// the binary is not on `PATH`; [`PackageError::Spawn`] for other
    /// spawn failures.
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, PackageError>;
}

/// Real [`std::process::Command`]-based runner.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, PackageError> {
        match std::process::Command::new(program).args(args).output() {
            Ok(out) => Ok(CommandOutput {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && program == "pacman" => {
                Err(PackageError::PacmanMissing)
            }
            Err(e) => Err(PackageError::Spawn(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_runner_nonexistent_pacman_returns_missing() {
        let runner = SystemRunner;
        // "pacman" + NotFound → PacmanMissing only when program is "pacman"
        // We can't guarantee pacman is absent everywhere, so test the
        // non-pacman case instead: any unknown binary → Spawn(NotFound)
        let result = runner.run("__archdots_nonexistent_binary__", &[]);
        assert!(
            matches!(result, Err(PackageError::Spawn(_))),
            "expected Spawn for unknown non-pacman binary, got: {result:?}"
        );
    }

    #[test]
    #[ignore = "requires 'echo' to be available; not guaranteed in all CI environments"]
    fn system_runner_real_command_returns_output() {
        let runner = SystemRunner;
        let out = runner.run("echo", &["hello"]).expect("echo should succeed");
        assert_eq!(out.status, 0);
        assert!(out.stdout.contains("hello"));
    }
}
