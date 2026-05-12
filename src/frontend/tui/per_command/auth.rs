//! `AuthCommandFrontend` impl for the TUI.

use crate::command::commands::auth::{AuthCommandFrontend, AuthConsentChoice};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl AuthCommandFrontend for TuiCommandFrontend {
    fn ask_consent(&mut self, _default: bool) -> Result<AuthConsentChoice, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Agent credentials?".into(),
            body: "Allow amux to pass credentials to the agent container?".into(),
            keys: vec![
                ('y', "Accept".into()),
                ('n', "Decline".into()),
                ('o', "Decline once".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('y') | DialogResponse::Yes => AuthConsentChoice::Accept,
            DialogResponse::Char('n') | DialogResponse::No => AuthConsentChoice::Decline,
            _ => AuthConsentChoice::Once,
        })
    }
}
