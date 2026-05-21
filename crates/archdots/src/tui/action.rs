#![allow(dead_code)] // Variants wired up in later sessions.

use archdots_core::snapshot::SnapshotId;

use crate::tui::{
    tasks::BackgroundKind,
    ui::{Modal, StatusMessage},
    views::ViewKind,
};

/// Cross-view coordination messages returned from `handle_event` or dispatched
/// internally by `App`.
#[derive(Debug)]
pub enum Action {
    /// No-op.
    None,
    /// Switch the active tab to the given view.
    SwitchView(ViewKind),
    /// Open `DepsView` pre-seeded with this profile name.
    SelectProfileForDeps(String),
    /// Open `DiffView` pre-seeded with this profile name.
    SelectProfileForDiff(String),
    /// Spawn a background task.
    SpawnTask(BackgroundKind),
    /// Update the status bar message.
    SetStatus(StatusMessage),
    /// Open a modal overlay.
    OpenModal(Modal),
    /// Close the current modal without confirming.
    CloseModal,
    /// Request a clean exit.
    Quit,
    /// Copy the given text to the system clipboard.
    CopyToClipboard(String),
}

/// Discriminant for `Modal::Confirm` — identifies what action to take when the
/// user presses Enter.
#[derive(Debug, Clone)]
pub enum ConfirmKind {
    /// Apply the named profile.
    ApplyProfile(String),
    /// Roll back the last apply of the named profile.
    RollbackProfile(String),
    /// Restore a specific snapshot by id.
    RollbackToSnapshot(SnapshotId),
    /// Delete the named profile file from disk.
    DeleteProfile(String),
    /// Prune (delete) a specific snapshot.
    PruneSnapshot(SnapshotId),
    /// User is trying to quit while a background task is running.
    QuitWithRunningTask,
}
