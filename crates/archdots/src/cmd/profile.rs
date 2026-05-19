//! Implementation of `archdots profile list|show|delete`.

use std::io::Write as _;

use anyhow::{bail, Context, Result};
use archdots_core::profile::Profile;

use crate::xdg;

pub fn run_list() -> Result<()> {
    let dir = xdg::profiles_dir()?;

    if !dir.exists() {
        println!("No profiles found.");
        return Ok(());
    }

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read profiles directory {}", dir.display()))?
        .filter_map(|res| {
            let entry = res.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? == "toml" {
                Some(path.file_stem()?.to_str()?.to_string())
            } else {
                None
            }
        })
        .collect();

    names.sort();

    if names.is_empty() {
        println!("No profiles found.");
    } else {
        for name in &names {
            println!("{name}");
        }
    }

    Ok(())
}

pub fn run_show(name: &str) -> Result<()> {
    let path = xdg::profiles_dir()?.join(format!("{name}.toml"));

    if !path.exists() {
        bail!("profile '{name}' not found at {}", path.display());
    }

    let p = Profile::load_from_file(&path)
        .with_context(|| format!("failed to load profile '{name}'"))?;

    println!("Profile:      {}", p.profile.name);
    if let Some(desc) = &p.profile.description {
        println!("Description:  {desc}");
    }
    if let Some(author) = &p.profile.author {
        println!("Author:       {author}");
    }
    if let Some(date) = &p.profile.created_at {
        println!("Created:      {date}");
    }
    if !p.profile.tags.is_empty() {
        println!("Tags:         {}", p.profile.tags.join(", "));
    }
    if let Some(wm) = &p.wm {
        match &wm.version {
            Some(v) => println!("WM:           {} {v}", wm.kind),
            None => println!("WM:           {}", wm.kind),
        }
    }

    let n = p.files.len();
    println!("\nFiles ({n}):");

    if n == 0 {
        println!("  (none)");
    } else {
        let id_w = p.files.iter().map(|f| f.id.len()).max().unwrap_or(2).max(2);
        let mode_w: usize = 8;
        println!(
            "  {:<id_w$}  {:<mode_w$}  target",
            "id",
            "mode",
            id_w = id_w,
            mode_w = mode_w,
        );
        println!(
            "  {}  {}  {}",
            "─".repeat(id_w),
            "─".repeat(mode_w),
            "─".repeat(40),
        );
        for f in &p.files {
            println!(
                "  {:<id_w$}  {:<mode_w$}  {}",
                f.id,
                f.mode,
                f.target,
                id_w = id_w,
                mode_w = mode_w,
            );
        }
    }

    let has_deps = !p.dependencies.pacman.is_empty()
        || !p.dependencies.aur.is_empty()
        || !p.dependencies.optional_pacman.is_empty();

    if has_deps {
        println!("\nDependencies:");
        if !p.dependencies.pacman.is_empty() {
            println!("  pacman:   {}", p.dependencies.pacman.join(", "));
        }
        if !p.dependencies.aur.is_empty() {
            println!("  aur:      {}", p.dependencies.aur.join(", "));
        }
        if !p.dependencies.optional_pacman.is_empty() {
            println!("  optional: {}", p.dependencies.optional_pacman.join(", "));
        }
    }

    Ok(())
}

pub fn run_delete(name: &str, yes: bool) -> Result<()> {
    let path = xdg::profiles_dir()?.join(format!("{name}.toml"));

    if !path.exists() {
        bail!("profile '{name}' not found");
    }

    if !yes && !confirm(&format!("Delete profile '{name}'?"))? {
        println!("Aborted.");
        return Ok(());
    }

    std::fs::remove_file(&path)
        .with_context(|| format!("failed to delete '{}'", path.display()))?;

    println!("Profile '{name}' deleted.");
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation from stdin")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
