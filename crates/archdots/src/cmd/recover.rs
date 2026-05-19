//! Implementation of `archdots recover`.

use anyhow::{Context, Result};
use archdots_core::{
    journal::{
        Journal, JournalAction, JournalEntry, JournalId, JournalStatus, LinkOutcome, LinkRecord,
        PriorState,
    },
    snapshot::{RestoreOptions, SnapshotManager},
};
use time::OffsetDateTime;

use crate::{cmd, xdg};

pub fn run(profile_filter: Option<&str>, yes: bool) -> Result<()> {
    let data_home = xdg::data_home()?;
    let state_home = xdg::state_home()?;

    let snapshots = SnapshotManager::open(&data_home)?;
    let journal = Journal::open(&state_home)?;

    let mut orphans = journal
        .orphaned_in_progress()
        .context("failed to scan journal for orphaned transactions")?;

    if let Some(p) = profile_filter {
        orphans.retain(|e| e.profile == p);
    }

    if orphans.is_empty() {
        println!("No orphaned transactions found.");
        return Ok(());
    }

    println!("Found {} orphaned transaction(s).", orphans.len());

    for orphan in &orphans {
        let snap_info = orphan
            .snapshot_id
            .as_ref()
            .map_or_else(|| "none".to_string(), std::string::ToString::to_string);

        println!(
            "\nOrphan: id={} profile={} ts={} snapshot={}",
            orphan.id, orphan.profile, orphan.ts, snap_info
        );

        let do_recover = if yes {
            true
        } else {
            cmd::confirm("Recover this transaction?")?
        };

        if !do_recover {
            println!("Skipped.");
            continue;
        }

        recover_one(&snapshots, &journal, orphan)?;
    }

    Ok(())
}

fn recover_one(
    snapshots: &SnapshotManager,
    journal: &Journal,
    orphan: &JournalEntry,
) -> Result<()> {
    if let Some(snap_id) = &orphan.snapshot_id {
        let restore_result = snapshots.restore(
            snap_id,
            RestoreOptions {
                dry_run: false,
                continue_on_error: true,
            },
        );

        match restore_result {
            Ok(rep) => {
                let links: Vec<LinkRecord> = rep
                    .restored
                    .iter()
                    .map(|p| LinkRecord {
                        source: std::path::PathBuf::new(),
                        target: p.clone(),
                        prior_state: PriorState::Absent,
                        outcome: LinkOutcome::Linked,
                    })
                    .collect();

                let status = if rep.skipped.is_empty() {
                    JournalStatus::Success
                } else {
                    JournalStatus::Partial
                };

                journal
                    .append(&JournalEntry {
                        schema_version: 1,
                        id: JournalId::new(),
                        ts: OffsetDateTime::now_utc(),
                        profile: orphan.profile.clone(),
                        action: JournalAction::Rollback,
                        snapshot_id: Some(snap_id.clone()),
                        links,
                        status,
                        error: if rep.skipped.is_empty() {
                            None
                        } else {
                            Some(format!("{} entries skipped", rep.skipped.len()))
                        },
                        supersedes: Some(orphan.id.clone()),
                    })
                    .context("failed to close orphan in journal")?;

                println!(
                    "Recovered: {} path(s) restored from snapshot {snap_id}",
                    rep.restored.len()
                );
            }
            Err(e) => {
                journal
                    .append(&JournalEntry {
                        schema_version: 1,
                        id: JournalId::new(),
                        ts: OffsetDateTime::now_utc(),
                        profile: orphan.profile.clone(),
                        action: JournalAction::Rollback,
                        snapshot_id: Some(snap_id.clone()),
                        links: Vec::new(),
                        status: JournalStatus::Failed,
                        error: Some(format!("restore failed: {e}")),
                        supersedes: Some(orphan.id.clone()),
                    })
                    .context("failed to close orphan in journal")?;

                eprintln!("Restore failed: {e}");
            }
        }
    } else {
        journal
            .append(&JournalEntry {
                schema_version: 1,
                id: JournalId::new(),
                ts: OffsetDateTime::now_utc(),
                profile: orphan.profile.clone(),
                action: JournalAction::Rollback,
                snapshot_id: None,
                links: Vec::new(),
                status: JournalStatus::Failed,
                error: Some(
                    "no snapshot available (crashed before snapshot was created)".to_string(),
                ),
                supersedes: Some(orphan.id.clone()),
            })
            .context("failed to close orphan in journal")?;

        println!("Closed (no snapshot to restore for this orphan).");
    }

    Ok(())
}
