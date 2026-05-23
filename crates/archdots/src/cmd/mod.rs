pub mod apply;
pub mod check;
pub mod diff;
pub mod export;
pub mod history;
pub mod init;
pub mod profile;
pub mod recover;
pub mod rollback;
pub mod snapshots;
pub mod tui;

/// Prompt the user for confirmation on a real TTY.
///
/// Returns `Err` when stdin is not a TTY (e.g., in scripted / piped contexts)
/// to prevent accidental destructive operations. Callers that want to bypass
/// this check should use `--yes`.
pub(crate) fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("refusing to prompt: stdin is not a TTY (use --yes)");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
