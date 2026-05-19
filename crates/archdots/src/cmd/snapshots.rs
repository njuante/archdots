//! Implementation of `archdots snapshots list|show|prune`.

use std::{collections::HashMap, time::Duration};

use anyhow::{bail, Context, Result};
use archdots_core::snapshot::{PrunePolicy, SnapshotManager, SnapshotSummary, SnapshotTrigger};
use byte_unit::{Byte, UnitType};

use crate::{cmd, xdg};

const ID_W: usize = 8;
const TS_W: usize = 20;
const PROF_W: usize = 20;
const TRIG_W: usize = 12;
const CNT_W: usize = 7;
const SZ_W: usize = 10;

pub fn run_list(profile_filter: Option<&str>) -> Result<()> {
    let data_home = xdg::data_home()?;
    let snapshots = SnapshotManager::open(&data_home)?;
    let snaps_dir = data_home.join("archdots").join("snapshots");

    let summaries = snapshots.list()?;

    let filtered: Vec<_> = if let Some(p) = profile_filter {
        summaries.into_iter().filter(|s| s.profile == p).collect()
    } else {
        summaries
    };

    if filtered.is_empty() {
        println!("(no snapshots)");
        return Ok(());
    }

    println!(
        "{:<ID_W$}  {:<TS_W$}  {:<PROF_W$}  {:<TRIG_W$}  {:>CNT_W$}  {:>SZ_W$}",
        "id", "created_at", "profile", "trigger", "targets", "size"
    );
    println!(
        "{}",
        "─".repeat(ID_W + TS_W + PROF_W + TRIG_W + CNT_W + SZ_W + 20)
    );

    for s in &filtered {
        let id_str = s.id.to_string();
        let id_short = &id_str[..ID_W.min(id_str.len())];

        let ts = if s.created_at.len() > TS_W {
            &s.created_at[..TS_W]
        } else {
            &s.created_at
        };

        let prof = if s.profile.len() > PROF_W {
            format!("{}…", &s.profile[..PROF_W - 1])
        } else {
            s.profile.clone()
        };

        let trig_str = match s.trigger {
            SnapshotTrigger::PreApply => "pre_apply",
            SnapshotTrigger::Manual => "manual",
            SnapshotTrigger::PreRollback => "pre_rollback",
        };

        let archive_path = snaps_dir.join(format!("{}.tar.gz", s.id));
        let size_bytes = std::fs::metadata(&archive_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let size_display = format_size(size_bytes);

        println!(
            "{id_short:<ID_W$}  {ts:<TS_W$}  {prof:<PROF_W$}  {trig_str:<TRIG_W$}  {:>CNT_W$}  {:>SZ_W$}",
            s.target_count, size_display
        );
    }

    if filtered.len() > 20 {
        eprintln!(
            "warning: {} snapshots found; consider running `archdots snapshots prune`",
            filtered.len()
        );
    }

    Ok(())
}

pub fn run_show(id_prefix: &str) -> Result<()> {
    if id_prefix.len() < 6 {
        bail!("id prefix must be at least 6 characters");
    }

    let data_home = xdg::data_home()?;
    let snapshots = SnapshotManager::open(&data_home)?;

    let summaries = snapshots.list()?;
    let matches: Vec<_> = summaries
        .iter()
        .filter(|s| s.id.to_string().starts_with(id_prefix))
        .collect();

    match matches.len() {
        0 => bail!("no snapshot found with id prefix '{id_prefix}'"),
        1 => {
            let snap = snapshots.get(&matches[0].id)?;
            let m = &snap.manifest;

            println!("Snapshot:   {}", m.id);
            println!("Created:    {}", m.created_at);
            println!("Profile:    {}", m.profile);
            println!("Trigger:    {:?}", m.trigger);
            println!(
                "Host:       {} ({}@{})",
                m.host.hostname, m.host.user, m.host.home
            );
            println!("Version:    {}", m.host.archdots_version);
            println!("SHA-256:    {}", m.payload_sha256);
            println!("\nTargets ({}):", m.targets.len());

            for t in &m.targets {
                println!("  {:?}  {}", t.kind, t.path);
            }
        }
        _ => {
            eprintln!("Ambiguous prefix '{id_prefix}' — matches:");
            for s in &matches {
                eprintln!("  {}", s.id);
            }
            bail!("use a longer prefix to disambiguate");
        }
    }

    Ok(())
}

pub fn run_prune(
    keep: Option<usize>,
    older_than: Option<String>,
    profile_filter: Option<&str>,
    yes: bool,
) -> Result<()> {
    let data_home = xdg::data_home()?;
    let snapshots = SnapshotManager::open(&data_home)?;
    let snaps_dir = data_home.join("archdots").join("snapshots");

    let policy = build_prune_policy(keep, older_than, profile_filter)?;

    let summaries = snapshots.list()?;
    let to_remove = summaries_to_remove(&summaries, &policy);

    if to_remove.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    let freed: u64 = to_remove
        .iter()
        .map(|s| {
            let archive = snaps_dir.join(format!("{}.tar.gz", s.id));
            let sidecar = snaps_dir.join(format!("{}.manifest.json", s.id));
            std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0)
                + std::fs::metadata(&sidecar).map(|m| m.len()).unwrap_or(0)
        })
        .sum();

    println!(
        "Would remove {} snapshot(s), freeing {}.",
        to_remove.len(),
        format_size(freed)
    );

    if !yes && !cmd::confirm("Proceed with prune?")? {
        println!("Aborted.");
        return Ok(());
    }

    let report = snapshots.prune(policy)?;

    println!(
        "Pruned {} snapshot(s), freed {}.",
        report.removed.len(),
        format_size(report.freed_bytes)
    );

    Ok(())
}

fn build_prune_policy(
    keep: Option<usize>,
    older_than: Option<String>,
    profile_filter: Option<&str>,
) -> Result<PrunePolicy> {
    match (keep, older_than, profile_filter) {
        (Some(n), None, Some(_)) => Ok(PrunePolicy::KeepLastPerProfile(n)),
        (Some(n), None, None) => Ok(PrunePolicy::KeepLast(n)),
        (None, Some(dur_str), _) => {
            let dur: Duration = humantime::parse_duration(&dur_str).with_context(|| {
                format!("invalid duration '{dur_str}' (try '30d', '1h', '60m')")
            })?;
            Ok(PrunePolicy::OlderThan(dur))
        }
        _ => bail!("provide either --keep N or --older-than <duration>"),
    }
}

fn summaries_to_remove<'a>(
    summaries: &'a [SnapshotSummary],
    policy: &PrunePolicy,
) -> Vec<&'a SnapshotSummary> {
    match policy {
        PrunePolicy::KeepLast(n) => summaries.iter().skip(*n).collect(),
        PrunePolicy::OlderThan(max_age) => {
            let now = std::time::SystemTime::now();
            summaries
                .iter()
                .filter(|s| {
                    time::OffsetDateTime::parse(
                        &s.created_at,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .ok()
                    .is_some_and(|dt| {
                        let unix = dt.unix_timestamp();
                        let t = if unix >= 0 {
                            std::time::UNIX_EPOCH
                                + std::time::Duration::from_secs(unix.unsigned_abs())
                        } else {
                            std::time::UNIX_EPOCH
                        };
                        now.duration_since(t).unwrap_or_default() > *max_age
                    })
                })
                .collect()
        }
        PrunePolicy::KeepLastPerProfile(n) => {
            let mut per_profile: HashMap<&str, Vec<&SnapshotSummary>> = HashMap::new();
            for s in summaries {
                per_profile.entry(&s.profile).or_default().push(s);
            }
            per_profile
                .into_values()
                .flat_map(|group| group.into_iter().skip(*n))
                .collect()
        }
    }
}

fn format_size(bytes: u64) -> String {
    let adjusted = Byte::from_u64(bytes).get_appropriate_unit(UnitType::Binary);
    format!("{adjusted:.1}")
}
