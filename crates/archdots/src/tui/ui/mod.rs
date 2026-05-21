pub mod layout;
pub mod modal;
pub mod status_bar;

pub use modal::{Modal, ModalOutcome};

/// Message shown in the status bar when no background task is running.
#[derive(Debug, Clone, Default)]
pub enum StatusMessage {
    /// No message; shows the generic "press ? for help" hint.
    #[default]
    None,
    /// Neutral informational text.
    Info(String),
    /// A successful operation summary.
    Success(String),
    /// An error summary (does not open a modal).
    Error(String),
}
