//! Implementation of `archdots history`.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use archdots_core::journal::{Journal, JournalAction, JournalStatus};
use owo_colors::OwoColorize;

use crate::xdg;

const TS_W: usize = 19;
const ID_W: usize = 6;
const PROF_W: usize = 20;
const ACT_W: usize = 10;
const STAT_W: usize = 11;
const SNAP_W: usize = 8;

pub fn run(profile_filter: Option<&str>, limit: usize, all: bool) -> Result<()> {
    let state_home = xdg::state_home()?;
    let journal = Journal::open(&state_home)?;

    let mut entries: Vec<_> = journal
        .iter()
        .context("failed to read journal")?
        .flatten()
        .collect();

    if let Some(p) = profile_filter {
        entries.retain(|e| e.profile == p);
    }

    if entries.is_empty() {
        println!("(no entries)");
        return Ok(());
    }

    let display: Vec<_> = if all {
        entries
    } else {
        let len = entries.len();
        entries
            .into_iter()
            .skip(len.saturating_sub(limit))
            .collect()
    };

    let use_color = std::io::stdout().is_terminal();

    println!(
        "{:<TS_W$}  {:<ID_W$}  {:<PROF_W$}  {:<ACT_W$}  {:<STAT_W$}  {:<SNAP_W$}  links",
        "timestamp", "id", "profile", "action", "status", "snapshot"
    );
    println!(
        "{}",
        "─".repeat(TS_W + ID_W + PROF_W + ACT_W + STAT_W + SNAP_W + 20)
    );

    let ts_fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

    for entry in &display {
        let ts_str = entry.ts.format(&ts_fmt).unwrap_or_else(|_| "?".to_string());

        let id_str = entry.id.to_string();
        let id_short = &id_str[..ID_W.min(id_str.len())];

        let prof = if entry.profile.len() > PROF_W {
            format!("{}…", &entry.profile[..PROF_W - 1])
        } else {
            entry.profile.clone()
        };

        let action_str = match entry.action {
            JournalAction::Apply => "apply",
            JournalAction::Rollback => "rollback",
        };

        let snap_str = entry.snapshot_id.as_ref().map_or_else(
            || "-".to_string(),
            |s| {
                let ss = s.to_string();
                ss[..SNAP_W.min(ss.len())].to_string()
            },
        );

        let status_str = match entry.status {
            JournalStatus::InProgress => "in_progress",
            JournalStatus::Success => "success",
            JournalStatus::Partial => "partial",
            JournalStatus::Failed => "failed",
        };

        let status_colored: String = if use_color {
            match entry.status {
                JournalStatus::Success => status_str.green().to_string(),
                JournalStatus::Partial => status_str.yellow().to_string(),
                JournalStatus::Failed => status_str.red().to_string(),
                JournalStatus::InProgress => status_str.cyan().to_string(),
            }
        } else {
            status_str.to_string()
        };

        println!(
            "{ts_str:<TS_W$}  {id_short:<ID_W$}  {prof:<PROF_W$}  {action_str:<ACT_W$}  {status_colored:<STAT_W$}  {snap_str:<SNAP_W$}  {}",
            entry.links.len()
        );
    }

    Ok(())
}
