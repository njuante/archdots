/// One mention of a binary in a config file.
///
/// Parsers emit one `Mention` per occurrence — the validator deduplicates
/// downstream. Line numbers are 1-based and refer to the first physical line
/// of the logical line (after continuation joining).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The binary name (basename, no path components, no arguments).
    pub binary: String,
    /// 1-based line number of the first physical line where the mention occurs.
    pub line: u32,
    /// Syntactic context in which the binary was found.
    pub source: MentionSource,
}

/// Syntactic context in which a binary was mentioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MentionSource {
    /// `exec X &` or `X &` or `pgrep … || X &` in a bspwmrc.
    BspwmExec,
    /// `exec = X` in hyprland.conf.
    HyprlandExec,
    /// `exec-once = X` in hyprland.conf.
    HyprlandExecOnce,
    /// `exec` or `exec_always` directive in an i3/sway config.
    I3SwayExec,
    /// `bindsym KEY exec X` in an i3/sway config.
    I3SwayBindsym,
    /// Indented command line in sxhkdrc.
    SxhkdCommand,
    /// `alias name=X` in a shell config.
    ShellAlias,
    /// `command -v X` probe in a shell config.
    ShellProbe,
    /// Top-level invocation `X args…` in a shell config.
    ShellInvoke,
}

/// Selects which parser to run on a config file's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParserKind {
    /// `bspwmrc` — bspwm window manager config.
    Bspwm,
    /// `sxhkdrc` — simple X hotkey daemon config.
    Sxhkd,
    /// `hyprland.conf` — Hyprland compositor config.
    Hyprland,
    /// i3 window manager config (typically `~/.config/i3/config`).
    I3,
    /// Sway compositor config (typically `~/.config/sway/config`).
    Sway,
    /// Zsh shell config (`.zshrc`, `.zshenv`, `.zprofile`, `.zlogin`).
    Zsh,
    /// Bash shell config (`.bashrc`, `.bash_profile`, `.profile`).
    Bash,
    /// Generic POSIX-ish shell config (`.xprofile`, `.xinitrc`).
    Shell,
}
