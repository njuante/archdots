//! Implementation of `archdots diff`.

use std::{fs, io::IsTerminal};

use anyhow::{bail, Context, Result};
use archdots_core::profile::{Profile, ResolveCtx};
use owo_colors::OwoColorize;

use crate::{diff_util, diff_util::DiffTag, xdg};

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

        let Ok(meta) = tgt.symlink_metadata() else {
            println!("{}: missing — would create symlink", tgt.display());
            continue;
        };

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

        for (tag, text) in diff_util::compute_diff_lines(&src_content, &tgt_content) {
            let sign = match tag {
                DiffTag::Removed => "-",
                DiffTag::Added => "+",
                DiffTag::Context => " ",
            };
            if use_color {
                match tag {
                    DiffTag::Removed => print!("{}", format!("{sign}{text}").red()),
                    DiffTag::Added => print!("{}", format!("{sign}{text}").green()),
                    DiffTag::Context => print!("{sign}{text}"),
                }
            } else {
                print!("{sign}{text}");
            }
        }
    }

    Ok(())
}
