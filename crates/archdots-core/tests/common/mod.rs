use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use archdots_core::packages::{CommandOutput, CommandRunner, PackageError};

/// A canned-response runner for tests.
///
/// Calls are matched by `(program, args)`. Any call without a registered
/// response returns [`PackageError::UnparseableOutput`] so that missing mocks
/// surface as failures rather than silent panics.
pub struct MockRunner {
    pub responses: HashMap<(String, Vec<String>), CommandOutput>,
    /// Total number of `run` invocations, shared so callers can read it after
    /// the runner is moved into a `PackageDB`.
    pub call_counter: Arc<AtomicU32>,
}

impl MockRunner {
    pub fn new() -> Self {
        Self {
            responses: HashMap::new(),
            call_counter: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn expect(mut self, program: &str, args: &[&str], output: CommandOutput) -> Self {
        let key = (
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        );
        self.responses.insert(key, output);
        self
    }

    pub fn expect_pacman_q(self, stdout: &str) -> Self {
        self.expect(
            "pacman",
            &["-Q"],
            CommandOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        )
    }

    pub fn expect_pacman_qm(self, stdout: &str) -> Self {
        self.expect(
            "pacman",
            &["-Qm"],
            CommandOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        )
    }

    pub fn expect_pacman_f(self, binary: &str, stdout: &str, stderr: &str) -> Self {
        let path = format!("/usr/bin/{binary}");
        self.expect(
            "pacman",
            &["-F", &path],
            CommandOutput {
                status: 0,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            },
        )
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, PackageError> {
        self.call_counter.fetch_add(1, Ordering::SeqCst);
        let key = (
            program.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        );
        self.responses.get(&key).cloned().ok_or_else(|| {
            PackageError::UnparseableOutput(format!(
                "MockRunner: unexpected call: {program} {args:?}"
            ))
        })
    }
}
