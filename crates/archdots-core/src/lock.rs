//! Advisory process lock using `flock(2)` via rustix.
//!
//! [`ApplyLock`] is a RAII guard that holds an exclusive advisory lock on
//! `$state_home/lock`. The lock is per-open-file-description (per-FD), so
//! two independent `acquire` calls from the same process will conflict —
//! this is intentional and matches the `flock(2)` design for per-FD semantics.
//!
//! The lockfile is **never deleted on drop** — other processes may be waiting
//! on it with `acquire_blocking`, and unlinking would break their wait.

use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::fs::{flock, FlockOperation};

use crate::{
    error::{CoreError, LockError},
    Result,
};

const LOCK_FILENAME: &str = "lock";
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// RAII exclusive advisory lock on `$state_home/lock`.
///
/// Dropped when the guard goes out of scope; `close(fd)` releases the flock
/// automatically. The file itself is never unlinked.
#[derive(Debug)]
pub struct ApplyLock {
    _file: File,
    #[allow(dead_code)]
    path: PathBuf,
    pid: u32,
}

impl ApplyLock {
    /// Non-blocking acquire.
    ///
    /// Creates `state_home` if it does not exist.
    ///
    /// # Errors
    ///
    /// - [`LockError::Busy`] — another file description already holds the lock
    ///   (including a different FD opened by this process).
    /// - [`LockError::Io`] — filesystem error creating the directory or file.
    pub fn acquire(state_home: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_home).map_err(|e| LockError::Io {
            path: state_home.to_path_buf(),
            source: e,
        })?;

        let lock_path = state_home.join(LOCK_FILENAME);

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false) // we truncate manually after acquiring the flock
            .open(&lock_path)
            .map_err(|e| LockError::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                let pid = std::process::id();
                file.set_len(0).map_err(|e| LockError::Io {
                    path: lock_path.clone(),
                    source: e,
                })?;
                file.seek(SeekFrom::Start(0)).map_err(|e| LockError::Io {
                    path: lock_path.clone(),
                    source: e,
                })?;
                write!(file, "{pid}").map_err(|e| LockError::Io {
                    path: lock_path.clone(),
                    source: e,
                })?;
                Ok(Self {
                    _file: file,
                    path: lock_path,
                    pid,
                })
            }
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                Err(LockError::Busy {
                    pid: read_pid_from_file(&lock_path),
                }
                .into())
            }
            Err(e) => Err(LockError::Io {
                path: lock_path,
                source: std::io::Error::from_raw_os_error(e.raw_os_error()),
            }
            .into()),
        }
    }

    /// Blocking acquire with timeout. Retries every 50 ms.
    ///
    /// # Errors
    ///
    /// - [`LockError::Timeout`] — `timeout` elapsed before the lock was obtained.
    /// - [`LockError::Io`] — filesystem error on any retry attempt.
    pub fn acquire_blocking(state_home: &Path, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match Self::acquire(state_home) {
                Ok(lock) => return Ok(lock),
                Err(CoreError::Lock(LockError::Busy { .. })) => {
                    if Instant::now() >= deadline {
                        return Err(LockError::Timeout.into());
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(remaining.min(RETRY_INTERVAL));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// PID written to the lockfile when this guard was acquired (informational).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for ApplyLock {
    fn drop(&mut self) {
        // Closing _file releases the flock automatically.
        // The lockfile is intentionally NOT unlinked; waiters hold open fds.
    }
}

fn read_pid_from_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LockError;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ── 1. acquire creates state_home if missing ──────────────────────────────

    #[test]
    fn acquire_creates_state_home() {
        let dir = tmpdir();
        let state_home = dir.path().join("deeply/nested/new");
        assert!(!state_home.exists());
        let _lock = ApplyLock::acquire(&state_home).unwrap();
        assert!(state_home.exists());
    }

    // ── 2. acquire creates the lockfile ──────────────────────────────────────

    #[test]
    fn acquire_creates_lock_file() {
        let dir = tmpdir();
        let lock_path = dir.path().join(LOCK_FILENAME);
        assert!(!lock_path.exists());
        let _lock = ApplyLock::acquire(dir.path()).unwrap();
        assert!(lock_path.exists());
    }

    // ── 3. acquire writes our PID to the lockfile ─────────────────────────────

    #[test]
    fn acquire_writes_pid_to_file() {
        let dir = tmpdir();
        let _lock = ApplyLock::acquire(dir.path()).unwrap();
        let lock_path = dir.path().join(LOCK_FILENAME);
        let written_pid: u32 = std::fs::read_to_string(&lock_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(written_pid, std::process::id());
    }

    // ── 4. lock released → second acquire succeeds ───────────────────────────

    #[test]
    fn acquire_after_drop_succeeds() {
        let dir = tmpdir();
        {
            let _lock = ApplyLock::acquire(dir.path()).unwrap();
        }
        let _lock2 = ApplyLock::acquire(dir.path()).unwrap();
    }

    // ── 5. same process, different FD → Busy ─────────────────────────────────
    //
    // flock is per-open-file-description. Two open() calls on the same file
    // yield two independent OFDs; if OFD1 holds LOCK_EX, OFD2 gets EWOULDBLOCK
    // even from the same process.

    #[test]
    fn acquire_different_fd_same_process_is_busy() {
        let dir = tmpdir();
        let _lock1 = ApplyLock::acquire(dir.path()).unwrap();
        let result = ApplyLock::acquire(dir.path());
        assert!(
            matches!(result, Err(CoreError::Lock(LockError::Busy { .. }))),
            "expected Busy, got: {result:?}",
        );
    }

    // ── 6. drop releases the lock ─────────────────────────────────────────────

    #[test]
    fn drop_releases_lock() {
        let dir = tmpdir();
        let lock = ApplyLock::acquire(dir.path()).unwrap();
        drop(lock);
        let _lock2 = ApplyLock::acquire(dir.path()).unwrap();
    }

    // ── 7. acquire_blocking with 0 ms timeout and lock held → Timeout ─────────

    #[test]
    fn acquire_blocking_zero_timeout_returns_timeout() {
        let dir = tmpdir();
        let _lock = ApplyLock::acquire(dir.path()).unwrap();
        let result = ApplyLock::acquire_blocking(dir.path(), Duration::ZERO);
        assert!(
            matches!(result, Err(CoreError::Lock(LockError::Timeout))),
            "expected Timeout, got: {result:?}",
        );
    }

    // ── 8. acquire_blocking succeeds after the holder drops at ~200 ms ────────

    #[test]
    fn acquire_blocking_succeeds_after_holder_drops() {
        let dir = tmpdir();
        let state_home = dir.path().to_path_buf();

        let lock = ApplyLock::acquire(&state_home).unwrap();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(lock);
        });

        let result = ApplyLock::acquire_blocking(&state_home, Duration::from_millis(500));
        handle.join().unwrap();
        assert!(result.is_ok(), "expected Ok after holder dropped, got: {result:?}");
    }

    // ── 9. cross-process: parent sees Busy with child's PID ──────────────────
    //
    // The child process is this same test binary, re-invoked with a filter and
    // an env var that puts it into "lock-helper" mode. When in helper mode, the
    // test acquires the lock, signals readiness via a sentinel file, holds the
    // lock for 1 s, then exits — without spawning further children.
    //
    // Marked #[ignore] because:
    //   • The test binary path returned by `current_exe()` may be unavailable
    //     or non-executable in some CI sandbox environments.
    //   • The --include-ignored + filter invocation depends on test-binary
    //     argument parsing that could change across Rust toolchain versions.
    //   • Parallel test runners can cause the ready-file poll to race.
    // Run manually with:
    //   cargo test -p archdots-core -- --ignored cross_process_lock_pid

    #[test]
    #[ignore = "cross-process; fragile in sandboxed CI — run manually"]
    fn cross_process_lock_pid() {
        const HELPER_ENV: &str = "__ARCHDOTS_LOCK_HELPER_DIR";

        // ── helper branch ────────────────────────────────────────────────────
        if let Ok(helper_dir) = std::env::var(HELPER_ENV) {
            let state_home = PathBuf::from(&helper_dir);
            let _lock = ApplyLock::acquire(&state_home).expect("helper: acquire lock");
            let ready = state_home.join("helper_ready");
            std::fs::write(&ready, format!("{}", std::process::id()))
                .expect("helper: write ready file");
            std::thread::sleep(Duration::from_secs(1));
            std::process::exit(0);
        }

        // ── parent branch ────────────────────────────────────────────────────
        let dir = tmpdir();
        let state_home = dir.path().to_path_buf();

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .env(HELPER_ENV, &state_home)
            // Filter to our test; --include-ignored allows the #[ignore] test to run.
            .args(["cross_process_lock_pid", "--include-ignored", "--test-threads=1"])
            .spawn()
            .expect("spawn helper child");

        // Poll until the helper signals readiness (up to 5 s).
        let ready = state_home.join("helper_ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "timeout waiting for helper to signal readiness"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let expected_pid: u32 = std::fs::read_to_string(&ready)
            .unwrap()
            .trim()
            .parse()
            .expect("parse helper PID");

        let result = ApplyLock::acquire(&state_home);
        match result {
            Err(CoreError::Lock(LockError::Busy { pid: Some(got_pid) })) => {
                assert_eq!(got_pid, expected_pid, "Busy carries wrong PID");
            }
            other => panic!("expected Busy{{pid: Some({expected_pid})}}, got: {other:?}"),
        }

        child.wait().expect("wait for helper child");
    }
}
