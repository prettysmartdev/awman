//! The facts a one-time squad key disclosure is made of.
//!
//! The squad daemon mints a bearer key on its first start. The plaintext
//! exists nowhere else — only its hash reaches disk — so the process that
//! mints it must hand the user both the key and the shell line that makes it
//! usable, or the key is lost.
//!
//! This module owns the two facts that are not presentation: which shell the
//! user runs, and therefore what the export line and the startup file are.
//! Until WI 0114 F-56 it also drew a box-drawing banner and wrote three
//! paragraphs of prose, and handed the result up as `UserMessage.text` — so
//! every frontend received terminal art it could not restyle, and the API
//! serialised `═` runs into JSON. The rendering now lives in each frontend
//! (`src/frontend/cli/per_command/squad.rs` draws the CLI's banner); Layer 1
//! says only *that* a key was minted and what it is.

use crate::data::config::env::{EnvSnapshot, AWMAN_SQUAD_KEY};

/// A user's login shell, insofar as it changes the snippet we print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlavor {
    Zsh,
    Bash,
    Fish,
    /// Anything else, including an unset `$SHELL`. The snippet stays correct by
    /// naming a POSIX `export` and hedging on the file, rather than guessing.
    Unknown,
}

impl ShellFlavor {
    /// Classify from a `$SHELL` value such as `/bin/zsh`. Matching is on the
    /// trailing path component so `/opt/homebrew/bin/fish` resolves too.
    pub fn from_shell_path(shell: Option<&str>) -> Self {
        let Some(shell) = shell.filter(|s| !s.is_empty()) else {
            return Self::Unknown;
        };
        let name = shell.rsplit('/').next().unwrap_or(shell);
        match name {
            "zsh" => Self::Zsh,
            "bash" | "sh" => Self::Bash,
            "fish" => Self::Fish,
            _ => Self::Unknown,
        }
    }

    /// Read `$SHELL` from a captured environment snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Self {
        Self::from_shell_path(env.shell())
    }

    /// The startup file the export belongs in, as displayed to the user.
    ///
    /// A fact about the shell, not a sentence: a frontend puts it in one.
    pub fn rc_file(self) -> &'static str {
        match self {
            Self::Zsh => "~/.zshrc",
            Self::Bash => "~/.bashrc",
            Self::Fish => "~/.config/fish/config.fish",
            // Named as an example, not as a fact, when we do not know the shell.
            Self::Unknown => "your shell's startup file (~/.zshrc, ~/.bashrc, …)",
        }
    }

    /// The export statement itself. fish has no `export`.
    fn export_line(self, key: &str) -> String {
        match self {
            Self::Fish => format!("set -gx {AWMAN_SQUAD_KEY} {key}"),
            _ => format!("export {AWMAN_SQUAD_KEY}={key}"),
        }
    }
}

/// The bare export line alone, without the banner or the surrounding notes —
/// what a "copy the .zshrc snippet" action puts on the clipboard, so pasting
/// it into a shell startup file doesn't also paste prose.
pub fn export_snippet(key: &str, shell: ShellFlavor) -> String {
    shell.export_line(key)
}

/// Everything a one-time squad key disclosure consists of.
///
/// Built exactly once, by whichever process minted the key — never by the
/// detached daemon child, whose stdout is a log file the key must not reach.
/// A frontend that is handed one of these displays it; one that drops it has
/// lost the key for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDisclosure {
    /// The plaintext key.
    pub key: String,
    /// The shell it was resolved against, for the startup file's name.
    pub shell: ShellFlavor,
    /// The line to add to that startup file. Pre-built because it is the fact
    /// worth keeping from the old renderer, and because it is what a "copy
    /// the snippet" action puts on the clipboard verbatim.
    pub export_line: String,
}

impl KeyDisclosure {
    pub fn new(key: impl Into<String>, shell: ShellFlavor) -> Self {
        let key = key.into();
        let export_line = export_snippet(&key, shell);
        Self {
            key,
            shell,
            export_line,
        }
    }

    /// The startup file the export belongs in, as displayed to the user.
    pub fn rc_file(&self) -> &'static str {
        self.shell.rc_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_flavor_reads_the_trailing_path_component() {
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/bin/zsh")),
            ShellFlavor::Zsh
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/bin/bash")),
            ShellFlavor::Bash
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/opt/homebrew/bin/fish")),
            ShellFlavor::Fish
        );
        assert_eq!(
            ShellFlavor::from_shell_path(Some("/usr/bin/nu")),
            ShellFlavor::Unknown
        );
    }

    #[test]
    fn shell_flavor_of_an_unset_or_empty_shell_is_unknown() {
        assert_eq!(ShellFlavor::from_shell_path(None), ShellFlavor::Unknown);
        assert_eq!(ShellFlavor::from_shell_path(Some("")), ShellFlavor::Unknown);
    }

    /// The export line is the one thing Layer 1 still spells, because it is a
    /// fact about the shell rather than a way of saying it. The prose that
    /// used to surround it is asserted in the CLI frontend now (F-56).
    #[test]
    fn the_export_line_is_the_documented_env_var_with_the_key() {
        let disclosure = KeyDisclosure::new("deadbeef", ShellFlavor::Zsh);
        assert_eq!(disclosure.export_line, "export AWMAN_SQUAD_KEY=deadbeef");
        assert_eq!(disclosure.rc_file(), "~/.zshrc");
    }

    #[test]
    fn fish_gets_set_gx_rather_than_export() {
        let disclosure = KeyDisclosure::new("deadbeef", ShellFlavor::Fish);
        assert_eq!(
            disclosure.export_line, "set -gx AWMAN_SQUAD_KEY deadbeef",
            "fish has no `export`"
        );
        assert_eq!(disclosure.rc_file(), "~/.config/fish/config.fish");
    }

    #[test]
    fn unknown_shell_still_yields_a_posix_export() {
        let disclosure = KeyDisclosure::new("deadbeef", ShellFlavor::Unknown);
        assert_eq!(disclosure.export_line, "export AWMAN_SQUAD_KEY=deadbeef");
        assert!(
            disclosure.rc_file().contains("~/.zshrc"),
            "an unknown shell names examples, not a fact: {}",
            disclosure.rc_file()
        );
    }

    /// `export_snippet` is what the clipboard action copies, and it is the
    /// same string the disclosure carries — one spelling, not two.
    #[test]
    fn the_disclosures_export_line_is_the_clipboard_snippet() {
        for shell in [
            ShellFlavor::Zsh,
            ShellFlavor::Bash,
            ShellFlavor::Fish,
            ShellFlavor::Unknown,
        ] {
            let disclosure = KeyDisclosure::new("deadbeef", shell);
            assert_eq!(disclosure.export_line, export_snippet("deadbeef", shell));
        }
    }
}
