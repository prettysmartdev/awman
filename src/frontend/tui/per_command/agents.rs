//! `AgentsCommandFrontend` impl for the TUI.
//!
//! `agents status` writes the tab's dashboard slot, which the execution
//! window draws as a `Table` (`render/execution_window.rs`). `list`, `detail`
//! and `kill` report through the status log, one log entry per line, built
//! from the same Layer 2 cell builders the CLI uses. The kill confirmation is
//! the shared `Prompt` drawn through the dialog router. `attach` needs nothing
//! here beyond [`AgentsCommandFrontend::attach_cancel`]: it binds the tab's
//! own container window through `AgentLaunchFrontend`, like `chat`.

use tokio_util::sync::CancellationToken;

use crate::command::commands::agents::{
    AgentsCommandFrontend, AgentsDetailOutcome, AgentsKillOutcome, AgentsListOutcome,
    AgentsOverview, AgentsTuiContext, KillChoice, LIST_COLUMNS,
};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::frontend::render_helpers::format_table;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::tabs::AgentsDashboardData;

impl AgentsCommandFrontend for TuiCommandFrontend {
    fn tui_context(&self) -> Option<AgentsTuiContext> {
        self.tui_context_shared.lock().ok().map(|g| g.clone())
    }

    /// The tab's command token: a new command in the tab, closing the tab,
    /// or Esc / Ctrl-C while the dashboard is watching all cancel it.
    fn begin_watch(&mut self) -> CancellationToken {
        self.command_cancel.clone()
    }

    fn render_overview(&mut self, overview: &AgentsOverview, watching: bool) {
        if let Ok(mut dash) = self.agents_dashboard.lock() {
            *dash = Some(AgentsDashboardData {
                overview: overview.clone(),
                watching,
            });
        }
    }

    /// Keep the last frame on screen; only the footer changes, from the
    /// refresh hint to the collection time.
    fn end_watch(&mut self) {
        if let Ok(mut dash) = self.agents_dashboard.lock() {
            if let Some(data) = dash.as_mut() {
                data.watching = false;
            }
        }
    }

    fn show_list(&mut self, outcome: &AgentsListOutcome) {
        if outcome.quiet {
            for agent in &outcome.agents {
                self.info_line(agent.short_id.clone());
            }
            return;
        }
        if outcome.agents.is_empty() {
            self.info_line(if outcome.include_stopped {
                "No agents found.".to_string()
            } else {
                "No agents running.".to_string()
            });
            return;
        }
        let rows: Vec<Vec<String>> = outcome
            .agents
            .iter()
            .map(|a| a.list_cells(outcome.no_trunc))
            .collect();
        for line in format_table(LIST_COLUMNS, &rows).lines() {
            self.info_line(line.to_string());
        }
    }

    fn show_detail(&mut self, outcome: &AgentsDetailOutcome) {
        let groups = outcome.groups();
        let key_width = groups
            .iter()
            .flat_map(|(_, pairs)| pairs.iter().map(|(k, _)| k.chars().count()))
            .max()
            .unwrap_or(0);
        for (i, (heading, pairs)) in groups.iter().enumerate() {
            if pairs.is_empty() {
                continue;
            }
            if i > 0 {
                self.info_line(String::new());
            }
            self.info_line((*heading).to_string());
            for (key, value) in pairs {
                self.info_line(format!("  {key:<key_width$}  {value}"));
            }
        }
    }

    fn show_kill_result(&mut self, outcome: &AgentsKillOutcome) {
        let verb = if outcome.removed {
            "Stopped and removed"
        } else {
            "Stopped"
        };
        self.write_message(UserMessage {
            level: MessageLevel::Success,
            text: format!("{verb} agent {} ({}).", outcome.name, outcome.short_id),
        });
    }

    fn confirm_kill(&mut self, prompt: &Prompt<KillChoice>) -> Result<KillChoice, CommandError> {
        self.pick_from_keyed_prompt(prompt)
    }

    /// Detaching never stops the agent: cancelling only ends this tab's
    /// session with it.
    fn attach_cancel(&self) -> CancellationToken {
        self.command_cancel.clone()
    }
}

impl TuiCommandFrontend {
    /// One status-log entry. The log draws each entry as a single line, so
    /// multi-line output is written one entry per line.
    fn info_line(&mut self, text: String) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text,
        });
    }
}
