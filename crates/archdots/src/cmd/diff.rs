//! Implementation of `archdots diff`.

use std::{fs, io::IsTerminal};

use anyhow::{bail, Context, Result};
use archdots_core::profile::{Profile, ResolveCtx};
use owo_colors::OwoColorize;
use similar::{ChangeTag, TextDiff};

use crate::xdg;

pub fn run(profile_name: &str) -> Result<()> {
    let home = xdg::home_dir()?;
    let profile_path = xdg::profiles_dir()?.join(format!("{profile_name}.toml"));

    if !profile_path.exists() {
        bail!("profile '{profile_name}' not found");
    }

    let profile = Profile::load_from_file(&profile_path)
        .with_context(|| format!("failed to load profile '{profile_name}'"))?;

    let ctx = ResolveCtx::with_home(&home);
    let use_color = std::io::stdout().is_terminal();

    for result in profile.resolved_entries(&home, &ctx) {
        let (_entry, src, tgt) = result?;

        let meta = tgt.symlink_metadata();

        if meta.is_err() {
            println!("{}: missing — would create symlink", tgt.display());
            continue;
        }

        let meta = meta.unwrap();

        if meta.file_type().is_symlink() {
            let dest = fs::read_link(&tgt)
                .with_context(|| format!("cannot read link {}", tgt.display()))?;
            if dest == src {
                println!("{}: owned (symlink to source) — no diff", tgt.display());
            } else {
                println!(
                    "{}: external symlink → {} — would replace",
                    tgt.display(),
                    dest.display()
                );
            }
            continue;
        }

        // Regular file: show unified diff
        let src_content = fs::read_to_string(&src).unwrap_or_default();
        let tgt_content = fs::read_to_string(&tgt).unwrap_or_default();

        if src_content == tgt_content {
            println!("{}: identical to source", tgt.display());
            continue;
        }

        println!("--- source: {}", src.display());
        println!("+++ target: {}", tgt.display());

        let diff = TextDiff::from_lines(&src_content, &tgt_content);
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            if use_color {
                match change.tag() {
                    ChangeTag::Delete => print!("{}", format!("{sign}{change}").red()),
                    ChangeTag::Insert => print!("{}", format!("{sign}{change}").green()),
                    ChangeTag::Equal => print!("{sign}{change}"),
                }
            } else {
                print!("{sign}{change}");
            }
        }
    }

    Ok(())
}
