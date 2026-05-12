//! `MountScopeFrontend` impl for the CLI.
//!
//! The safe non-interactive default is `MountGitRoot`.
//! When stdin is a TTY the CLI prompts; otherwise it returns the default.

use std::path::Path;

use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::error::CommandError;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl MountScopeFrontend for CliFrontend {
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        if !stdin_is_tty() {
            return Ok(MountScopeDecision::MountGitRoot);
        }
        eprintln!(
            "amux: cwd ({}) is below git root ({}). Mount [r]oot / [c]urrent dir / [a]bort?",
            cwd.display(),
            git_root.display()
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(MountScopeDecision::Abort);
        }
        Ok(match buf.trim() {
            "r" | "R" | "" => MountScopeDecision::MountGitRoot,
            "c" | "C" => MountScopeDecision::MountCurrentDirOnly,
            _ => MountScopeDecision::Abort,
        })
    }
}
