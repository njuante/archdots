//! Background task spawning and result routing.
#![allow(dead_code)] // Variants/fields wired up in later sessions.
//!
//! At most one task runs at a time. Workers own clones of the paths they need
//! and never hold references into `App`. Panics are caught by `catch_unwind`
//! and forwarded on the channel as errors.

#![allow(clippy::module_name_repetitions)]

use std::panic::AssertUnwindSafe;
use std::sync::mpsc;

use archdots_core::{
    linker::ApplyReport,
    snapshot::{PruneReport, SnapshotId, SnapshotSummary},
    validator::ValidationReport,
};

use crate::tui::app::AppPaths;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Opaque monotonic task identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

/// Which operation a background worker should execute.
#[derive(Debug, Clone)]
pub enum BackgroundKind {
    /// Apply the named profile's symlink plan.
    Apply { profile: String },
    /// Roll back the last apply for the named profile.
    Rollback { profile: String },
    /// Restore a specific snapshot by id (profile read from manifest).
    RollbackToSnapshot { id: SnapshotId },
    /// Validate dependencies for the named profile.
    Check { profile: String, deep: bool },
    /// Re-read all snapshot summaries.
    RefreshSnapshots,
    /// Delete a specific snapshot.
    PruneSnapshot { id: SnapshotId },
    /// Inject a panic for testing the `catch_unwind` path.
    #[cfg(test)]
    PanicForTest,
}

/// Normalised result from a completed worker.
#[derive(Debug)]
pub enum TaskResult {
    Apply(Result<ApplyReport, anyhow::Error>),
    Rollback(Result<ApplyReport, anyhow::Error>),
    Check(Result<ValidationReport, anyhow::Error>),
    SnapshotList(Result<Vec<SnapshotSummary>, anyhow::Error>),
    Prune(Result<PruneReport, anyhow::Error>),
}

/// Message sent from a worker thread to `App` when the task finishes.
#[derive(Debug)]
pub enum TaskMessage {
    Completed {
        id: TaskId,
        kind: BackgroundKind,
        result: TaskResult,
    },
}

// ─── Spawn ────────────────────────────────────────────────────────────────────

/// Spawn `kind` on a fresh OS thread.
///
/// Panics in the worker are caught via `catch_unwind`; the resulting error is
/// sent as a `TaskResult` variant. The thread never borrows from `App`.
pub fn spawn(id: TaskId, kind: BackgroundKind, paths: AppPaths, tx: mpsc::Sender<TaskMessage>) {
    std::thread::Builder::new()
        .name(format!("archdots-task-{}", id.0))
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| execute(&kind, &paths)));
            let result = match result {
                Ok(r) => r,
                Err(payload) => panic_to_result(&kind, &payload),
            };
            let _ = tx.send(TaskMessage::Completed { id, kind, result });
        })
        .expect("failed to spawn worker thread");
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn execute(kind: &BackgroundKind, _paths: &AppPaths) -> TaskResult {
    match kind {
        BackgroundKind::Apply { .. } => todo!("apply worker (Phase 4, Session 3)"),
        BackgroundKind::Rollback { .. } => todo!("rollback worker (Phase 4, Session 3)"),
        BackgroundKind::RollbackToSnapshot { .. } => {
            todo!("rollback_to_snapshot worker (Phase 4, Session 3)")
        }
        BackgroundKind::Check { .. } => todo!("check worker (Phase 4, Session 4)"),
        BackgroundKind::RefreshSnapshots => todo!("refresh_snapshots worker (Phase 4, Session 3)"),
        BackgroundKind::PruneSnapshot { .. } => {
            todo!("prune_snapshot worker (Phase 4, Session 3)")
        }
        #[cfg(test)]
        BackgroundKind::PanicForTest => panic!("intentional test panic"),
    }
}

/// Convert a `catch_unwind` payload into the appropriate `TaskResult::*::Err`.
fn panic_to_result(kind: &BackgroundKind, payload: &Box<dyn std::any::Any + Send>) -> TaskResult {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "worker thread panicked (no message)".to_owned()
    };
    let err = anyhow::anyhow!("panic in worker: {msg}");
    match kind {
        BackgroundKind::Apply { .. } => TaskResult::Apply(Err(err)),
        BackgroundKind::Rollback { .. } | BackgroundKind::RollbackToSnapshot { .. } => {
            TaskResult::Rollback(Err(err))
        }
        BackgroundKind::Check { .. } => TaskResult::Check(Err(err)),
        BackgroundKind::RefreshSnapshots => TaskResult::SnapshotList(Err(err)),
        BackgroundKind::PruneSnapshot { .. } => TaskResult::Prune(Err(err)),
        #[cfg(test)]
        BackgroundKind::PanicForTest => TaskResult::SnapshotList(Err(err)),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn fake_paths() -> AppPaths {
        AppPaths {
            home: PathBuf::from("/tmp/test-home"),
            profiles_dir: PathBuf::from("/tmp/test-home/.config/archdots/profiles"),
            data_home: PathBuf::from("/tmp/test-home/.local/share"),
            state_home: PathBuf::from("/tmp/test-home/.local/state"),
        }
    }

    #[test]
    fn task_panic_caught_and_sent_as_error() {
        let (tx, rx) = mpsc::channel();
        spawn(TaskId(1), BackgroundKind::PanicForTest, fake_paths(), tx);

        // Worker panics; catch_unwind converts it to a TaskResult::SnapshotList(Err).
        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("task must complete");

        match msg {
            TaskMessage::Completed {
                result: TaskResult::SnapshotList(Err(e)),
                ..
            } => {
                assert!(e.to_string().contains("panic in worker"));
            }
            other @ TaskMessage::Completed { .. } => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn spawn_returns_immediately() {
        let (tx, _rx) = mpsc::channel();
        let start = Instant::now();
        spawn(TaskId(2), BackgroundKind::PanicForTest, fake_paths(), tx);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "spawn must return in < 10 ms, took {elapsed:?}"
        );
        // Allow the worker thread to finish so TempDir (if used) can clean up.
    }
}
