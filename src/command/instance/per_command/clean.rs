//! `CleanCommandFrontend` impl for the TUI.
//!
//! Presents the itemized cleanup summary in a yes/no confirmation dialog and
//! proceeds only when the user confirms.

use crate::command::commands::clean::{CleanCommandFrontend, CleanSummary};
use crate::command::error::CommandError;
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse};

impl CleanCommandFrontend for InstanceCommandFrontend {
    fn confirm_deletion(&mut self, summary: &CleanSummary) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Confirm clean".to_string(),
            body: summary.render(),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }
}
