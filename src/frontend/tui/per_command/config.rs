//! `ConfigCommandFrontend` impl for the TUI.

use crate::command::commands::config::{ConfigCommandFrontend, ConfigEditRequest, ConfigFieldRow};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{ConfigShowRow, DialogRequest, DialogResponse};

impl ConfigCommandFrontend for TuiCommandFrontend {
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        let dialog_rows: Vec<ConfigShowRow> = rows
            .iter()
            .map(|r| ConfigShowRow {
                field: r.field.clone(),
                global: r.global_value.clone().unwrap_or_default(),
                repo: r.repo_value.clone().unwrap_or_default(),
                effective: r.effective_value.clone().unwrap_or_default(),
                read_only: r.read_only,
            })
            .collect();

        let response = self.ask_dialog(DialogRequest::ConfigShow { rows: dialog_rows })?;

        match response {
            DialogResponse::Text(edit_str) => {
                // Format: "field\tvalue\tscope" where scope is "global" or "repo"
                let parts: Vec<&str> = edit_str.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    Ok(Some(ConfigEditRequest {
                        field: parts[0].to_string(),
                        value: parts[1].to_string(),
                        global: parts[2] == "global",
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }
}
