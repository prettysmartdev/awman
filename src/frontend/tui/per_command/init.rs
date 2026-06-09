//! `InitFrontend` impl for the TUI.

use crate::data::config::repo::WorkItemsConfig;
use crate::data::message::UserMessageSink;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::frontend::{DockerfileSetupDecision, InitFrontend};
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;
use crate::engine::step_status::StepStatus;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl InitFrontend for TuiCommandFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Replace aspec?".into(),
                body: "An aspec/ folder already exists. Replace it with fresh templates?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Run audit?".into(),
                body: "Run the audit to check the project setup?".into(),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError> {
        Ok(None) // Work items config is an advanced feature
    }

    fn ask_dockerfile_setup(
        &mut self,
        git_root: &std::path::Path,
    ) -> Result<DockerfileSetupDecision, EngineError> {
        let repo_cfg = crate::data::config::repo::RepoConfig::load(git_root).unwrap_or_default();
        let display_path = repo_cfg.dockerfile.as_deref().unwrap_or("Dockerfile.dev");
        let response = self
            .ask_dialog(DialogRequest::KindSelect {
                title: format!("No Dockerfile found at {display_path}"),
                options: vec![
                    (
                        "1".into(),
                        "Create Dockerfile.dev from the built-in template".into(),
                    ),
                    ("2".into(), "Use an existing Dockerfile in this repo".into()),
                    ("3".into(), "Skip for now".into()),
                ],
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        match response {
            DialogResponse::Index(1) => {
                let text_response = self
                    .ask_dialog(DialogRequest::TextInput {
                        title: "Dockerfile path".into(),
                        prompt: "Path relative to repo root:".into(),
                        default_text: None,
                    })
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                match text_response {
                    DialogResponse::Text(text) if !text.is_empty() => {
                        Ok(DockerfileSetupDecision::UseExisting(text))
                    }
                    _ => Ok(DockerfileSetupDecision::CreateNew),
                }
            }
            DialogResponse::Index(2) => Ok(DockerfileSetupDecision::Skip),
            _ => Ok(DockerfileSetupDecision::CreateNew),
        }
    }

    fn report_phase(&mut self, phase: &InitPhase) {
        self.messages.info(format!("init: {phase:?}"));
    }

    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        self.messages.info(format!("  {step}: {status:?}"));
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(super::TuiContainerProxy::new(self.status_log.clone()))
    }

    fn report_summary(&mut self, _summary: &InitSummary) {
        self.messages.success("init completed");
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::init::frontend::DockerfileSetupDecision;
    use crate::engine::init::InitFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let container_io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some((80, 24)),
        };
        let status_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
            path: vec!["init".into()],
            flags: Default::default(),
            arguments: Default::default(),
        };
        let workflow_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_state = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pty_reset_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frontend = TuiCommandFrontend::new(
            parsed,
            status_log,
            req_tx,
            resp_rx,
            container_io,
            workflow_view,
            yolo_state,
            yolo_cancel_flag,
            pty_reset_flag,
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
        );
        (frontend, req_rx, resp_tx)
    }

    #[test]
    fn kind_select_index_0_returns_create_new() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Index(0)).unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }

    #[test]
    fn kind_select_index_1_then_nonempty_text_returns_use_existing() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Index(1)).unwrap();
            let _req2 = req_rx.recv().unwrap(); // TextInput
            resp_tx
                .send(DialogResponse::Text("docker/Dockerfile".to_string()))
                .unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(
            result,
            DockerfileSetupDecision::UseExisting("docker/Dockerfile".to_string())
        );
    }

    #[test]
    fn kind_select_index_1_then_dismissed_returns_create_new() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Index(1)).unwrap();
            let _req2 = req_rx.recv().unwrap(); // TextInput
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }

    #[test]
    fn kind_select_index_1_then_empty_text_returns_create_new() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Index(1)).unwrap();
            let _req2 = req_rx.recv().unwrap(); // TextInput
            resp_tx.send(DialogResponse::Text(String::new())).unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }

    #[test]
    fn kind_select_index_2_returns_skip() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Index(2)).unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(result, DockerfileSetupDecision::Skip);
    }

    #[test]
    fn kind_select_dismissed_returns_create_new() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let git_root = tempfile::tempdir().unwrap();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap(); // KindSelect
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend.ask_dockerfile_setup(git_root.path()).unwrap();
        handle.join().unwrap();
        assert_eq!(result, DockerfileSetupDecision::CreateNew);
    }
}
