//! `ConfigCommandFrontend` impl for the TUI.

use crate::command::commands::config::{
    ConfigCommandFrontend, ConfigEditRejection, ConfigEditRequest, ConfigFieldRow,
};
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{
    ConfigShowRejectedEdit, ConfigShowRow, DialogRequest, DialogResponse,
};

impl ConfigCommandFrontend for TuiCommandFrontend {
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
        rejected: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        let dialog_rows: Vec<ConfigShowRow> = rows
            .iter()
            .map(|r| ConfigShowRow {
                field: r.field.clone(),
                global: r.global_value.clone().unwrap_or_default(),
                repo: r.repo_value.clone().unwrap_or_default(),
                effective: r.effective_value.clone().unwrap_or_default(),
                read_only: r.read_only,
                global_writable: r.global_writable,
                repo_writable: r.repo_writable,
                value_hint: r.value_hint.clone(),
            })
            .collect();

        // Reopen on the rejected field if there is one, else on the row the
        // user last edited (the command layer re-presents the table after
        // every save) instead of jumping back to the top.
        let selected = rejected
            .map(|r| r.field.as_str())
            .or(self.last_config_edit_field.as_deref())
            .and_then(|field| dialog_rows.iter().position(|r| r.field == field))
            .unwrap_or(0);

        let response = self.ask_dialog(DialogRequest::ConfigShow {
            rows: dialog_rows,
            selected,
            rejected: rejected.map(|r| ConfigShowRejectedEdit {
                field: r.field.clone(),
                value: r.value.clone(),
                global: r.global,
                reason: r.reason.clone(),
            }),
        })?;

        match response {
            DialogResponse::Text(edit_str) => {
                // Format: "field\tvalue\tscope" where scope is "global" or "repo"
                let parts: Vec<&str> = edit_str.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    self.last_config_edit_field = Some(parts[0].to_string());
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
