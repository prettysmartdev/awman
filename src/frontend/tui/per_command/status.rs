//! `StatusCommandFrontend` impl for the TUI.

use crate::command::commands::status::{StatusCommandFrontend, StatusContainerRow};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::tabs::StatusDashboardData;

impl StatusCommandFrontend for TuiCommandFrontend {
    fn tui_context(&self) -> Option<crate::command::commands::status::StatusCommandTuiContext> {
        self.tui_context_shared.lock().ok().map(|g| g.clone())
    }

    fn should_continue_watching(&mut self) -> bool {
        true
    }

    fn write_clear_marker(&mut self) {
        if let Ok(mut dash) = self.status_dashboard.lock() {
            *dash = None;
        }
    }

    fn write_status_dashboard(&mut self, containers: &[StatusContainerRow], tip: &str) {
        if let Ok(mut dash) = self.status_dashboard.lock() {
            *dash = Some(StatusDashboardData {
                containers: containers.to_vec(),
                tip: tip.to_string(),
            });
        }
    }
}
