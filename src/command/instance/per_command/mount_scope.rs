//! `MountScopeFrontend` impl for the TUI.

use std::path::Path;

use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse, MountScopeState};

impl MountScopeFrontend for InstanceCommandFrontend {
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.mount_scope());
        }
        let response = self.ask_dialog(DialogRequest::MountScope(MountScopeState {
            git_root: git_root.display().to_string(),
            cwd: cwd.display().to_string(),
        }))?;
        Ok(match response {
            DialogResponse::Char('r') => MountScopeDecision::MountGitRoot,
            DialogResponse::Char('c') => MountScopeDecision::MountCurrentDirOnly,
            _ => MountScopeDecision::Abort,
        })
    }
}
