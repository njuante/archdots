//! Implementation of `archdots rollback`.

use anyhow::{Context, Result};
use archdots_core::{
    journal::{Journal, JournalAction, JournalStatus},
    linker::{ApplyOptions, Linker},
    snapshot::SnapshotManager,
};

use crate::{cmd, xdg};

pub fn run(profile_name: &str, yes: bool) -> Result<()> {
    let home = xdg::home_dir()?;
    let data_home = xdg::data_home()?;
    let state_home = xdg::state_home()?;

    let snapshots = SnapshotManager::open(&data_home)?;
    let journal = Journal::open(&state_home)?;
    let linker = Linker::new(&snapshots, &journal);

    if !yes {
        // Find what we'd roll back to for the confirmation prompt.
        let snap_id = journal
            .iter()
            .context("failed to read journal")?
            .flatten()
            .filter(|e| {
                e.profile == profile_name
                    && e.action == JournalAction::Apply
                    && e.status == JournalStatus::Success
                    && e.snapshot_id.is_some()
            })
            .last()
            .and_then(|e| e.snapshot_id);

        match snap_id {
            None => {
                anyhow::bail!("no successful apply transaction found for profile '{profile_name}'");
            }
            Some(ref sid) => {
                println!("Would restore snapshot {sid}");
                if !cmd::confirm(&format!("Rollback profile '{profile_name}'?"))? {
                    println!("Aborted.");
                    return Ok(());
                }
            }
        }
    }

    let opts = ApplyOptions {
        dry_run: false,
        force: false,
        home,
        state_home,
    };

    let report = linker.rollback(profile_name, opts)?;

    println!(
        "Rollback complete: {} item(s) restored",
        report.applied.len()
    );
    if let Some(snap_id) = &report.snapshot_id {
        println!("From snapshot: {snap_id}");
    }

    Ok(())
}
