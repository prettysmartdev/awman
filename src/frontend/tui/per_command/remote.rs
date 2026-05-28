//! `RemoteCommandFrontend` impl for the TUI, plus `RemoteWorkflowPoller`
//! for live workflow strip updates from a remote API session.

use std::sync::{Arc, Mutex};

use crate::command::commands::remote::RemoteCommandFrontend;
use crate::command::commands::remote_client::RemoteClient;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::tabs::WorkflowViewState;
use crate::frontend::tui::workflow_view::workflow_state_to_view_state;

impl RemoteCommandFrontend for TuiCommandFrontend {}

/// Polls a remote API server for workflow state and command status,
/// updating the shared `WorkflowViewState` that the TUI strip renders from.
pub struct RemoteWorkflowPoller {
    client: Arc<RemoteClient>,
    command_id: String,
    workflow_view: Arc<Mutex<Option<WorkflowViewState>>>,
}

impl RemoteWorkflowPoller {
    pub fn new(
        client: Arc<RemoteClient>,
        command_id: String,
        workflow_view: Arc<Mutex<Option<WorkflowViewState>>>,
    ) -> Self {
        Self {
            client,
            command_id,
            workflow_view,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.poll_loop().await;
        })
    }

    async fn poll_loop(&self) {
        loop {
            let status_done = self.poll_once().await;
            if status_done {
                self.poll_once().await;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn poll_once(&self) -> bool {
        let mut is_terminal = false;

        if let Ok(resp) = self.client.get_job(&self.command_id).await {
            let status = resp.body["status"].as_str().unwrap_or("");
            if status == "done" || status == "error" {
                is_terminal = true;
            }
        }

        if let Ok(Some(state_json)) = self.client.get_workflow_state(&self.command_id).await {
            if let Ok(state) =
                serde_json::from_value::<crate::data::workflow_state::WorkflowState>(state_json)
            {
                let view = workflow_state_to_view_state(&state);
                if let Ok(mut guard) = self.workflow_view.lock() {
                    *guard = Some(view);
                }
            }
        }

        is_terminal
    }
}
