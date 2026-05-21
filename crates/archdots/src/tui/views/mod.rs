#![allow(dead_code)] // Trait methods and ViewCtx fields used in later sessions.

pub mod deps;
pub mod placeholder;
pub mod profiles;
pub mod snapshots;

pub use deps::DepsView;
pub use profiles::ProfilesView;
pub use snapshots::SnapshotsView;

use ratatui::{layout::Rect, Frame};

use crate::tui::{action::Action, app::AppPaths, theme::Theme};

// ─── View trait ───────────────────────────────────────────────────────────────

/// Behaviour shared by all four tab views.
pub trait View {
    /// Draw the view into `area`.
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);

    /// Process an input event and return zero or one cross-view action.
    fn handle_event(&mut self, ev: &crossterm::event::Event, ctx: &ViewCtx<'_>) -> Action;

    /// Called when this view becomes the active tab.
    fn on_focus(&mut self, ctx: &ViewCtx<'_>);

    /// Key hint pairs shown in the footer when this view is active.
    fn footer_hints(&self) -> Vec<(&'static str, &'static str)>;
}

// ─── ViewCtx ─────────────────────────────────────────────────────────────────

/// Read-only context passed to every `View::handle_event` call.
pub struct ViewCtx<'a> {
    /// Resolved XDG paths for the current user.
    pub paths: &'a AppPaths,
    /// Active colour/unicode theme.
    pub theme: &'a Theme,
    /// `true` while a background task is running; blocks mutating actions.
    pub busy: bool,
}

// ─── ViewKind ─────────────────────────────────────────────────────────────────

/// Discriminant for the four main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Profiles,
    Snapshots,
    Deps,
    Diff,
}

impl ViewKind {
    /// Human-readable tab label.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Profiles => "Profiles",
            Self::Snapshots => "Snapshots",
            Self::Deps => "Deps",
            Self::Diff => "Diff",
        }
    }

    /// Single-character shortcut key.
    #[must_use]
    pub fn key(self) -> char {
        match self {
            Self::Profiles => '1',
            Self::Snapshots => '2',
            Self::Deps => '3',
            Self::Diff => '4',
        }
    }

    /// All variants in tab order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Profiles, Self::Snapshots, Self::Deps, Self::Diff]
    }
}
