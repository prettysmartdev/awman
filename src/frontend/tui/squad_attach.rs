//! TUI presentation for the Layer 2 `squad attach` command.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::attach::{
    format_candidates, SquadAttachFrontend, SquadContainer,
};
use crate::command::dispatch::Dispatch;
use crate::command::error::CommandError;
use crate::data::message::MessageLevel;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::AgentInstance;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::per_command::TuiContainerProxy;
use crate::frontend::tui::tabs::{ContainerSlotEvent, ContainerSlotIo};
use crate::frontend::tui::user_message::StatusLogEntry;
use crate::frontend::tui::workflow_view::workflow_state_to_view_state;

/// Presentation-level cancellation state shared by a squad tab and its
/// command frontend.
pub struct SquadAttachSession {
    root_cancel: CancellationToken,
    current: Mutex<Option<(String, CancellationToken)>>,
}

impl SquadAttachSession {
    pub fn new(root_cancel: CancellationToken) -> Self {
        Self {
            root_cancel,
            current: Mutex::new(None),
        }
    }

    pub fn begin(&self, task: &str) -> CancellationToken {
        let _ = self.end();
        let cancel = self.root_cancel.child_token();
        if let Ok(mut current) = self.current.lock() {
            *current = Some((task.to_string(), cancel.clone()));
        }
        cancel
    }

    pub fn end(&self) -> Option<String> {
        let current = self.current.lock().ok()?.take();
        current.map(|(task, cancel)| {
            cancel.cancel();
            task
        })
    }

    pub fn task(&self) -> Option<String> {
        self.current
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(task, _)| task.clone()))
    }

    fn cancel_token(&self) -> Option<CancellationToken> {
        self.current
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(_, cancel)| cancel.clone()))
    }
}

impl TuiCommandFrontend {
    pub(crate) fn with_squad_attach_context(
        mut self,
        session: Option<Arc<SquadAttachSession>>,
        reachable: Option<Arc<AtomicBool>>,
    ) -> Self {
        self.squad_attach_session = session;
        self.squad_attach_reachable = reachable;
        self
    }
}

impl SquadAttachFrontend for TuiCommandFrontend {
    fn begin_attach(&mut self, task: &str) -> Result<CancellationToken, CommandError> {
        let Some(session) = &self.squad_attach_session else {
            return Err(CommandError::Other(
                "squad attach is only available from the squad tab".to_string(),
            ));
        };
        if let Ok(mut view) = self.workflow_view.lock() {
            *view = None;
        }
        if let Ok(mut events) = self.container_slot_events.lock() {
            events.clear();
            events.push_back(ContainerSlotEvent::Exited {
                step_name: String::new(),
            });
        }
        Ok(session.begin(task))
    }

    fn ask_pick_candidate(
        &mut self,
        candidates: &[SquadContainer],
    ) -> Result<Option<usize>, CommandError> {
        let response = self.ask_dialog(DialogRequest::ListPicker {
            title: "Attach to which container?".to_string(),
            items: format_candidates(candidates),
        })?;
        match response {
            DialogResponse::Index(index) if index < candidates.len() => Ok(Some(index)),
            _ => Ok(None),
        }
    }

    fn on_slot_metadata(&mut self, step: &str, agent: &str, model: Option<&str>) {
        self.squad_attach_metadata.insert(
            step.to_string(),
            (agent.to_string(), model.map(str::to_string)),
        );
    }

    fn on_slot_attached(
        &mut self,
        step: &str,
        instance: Box<dyn AgentInstance>,
    ) -> Result<(), CommandError> {
        let preview = instance.handle_preview();
        let (agent, model) = self
            .squad_attach_metadata
            .remove(step)
            .unwrap_or_else(|| ("agent".to_string(), None));

        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let initial_size = crossterm::terminal::size()
            .map(|(cols, rows)| {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            })
            .unwrap_or((80, 24));
        let io = AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx.clone(),
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        };
        let container_name = Arc::new(Mutex::new(None));
        let proxy = TuiContainerProxy::with_io(self.status_log.clone(), io, container_name);
        let mut execution = instance
            .run_with_frontend(Box::new(proxy))
            .map_err(CommandError::from)?;

        push_slot_event(
            &self.container_slot_events,
            ContainerSlotEvent::Launched {
                step_name: step.to_string(),
                agent,
                model,
                io: Some(ContainerSlotIo {
                    stdout_rx,
                    stdin_tx,
                    resize_tx,
                }),
            },
        );
        push_slot_event(
            &self.container_slot_events,
            ContainerSlotEvent::ContainerName {
                step_name: step.to_string(),
                container_name: preview.name,
            },
        );

        let cancel = self
            .squad_attach_session
            .as_ref()
            .and_then(|session| session.cancel_token())
            .unwrap_or_default();
        let cancel_handle = execution.cancel_handle();
        let events = self.container_slot_events.clone();
        let status_log = self.status_log.clone();
        let output_tail = execution.output_tail();
        let step_name = step.to_string();
        let single_attach = step == "evaluation" || self.squad_attach_has_explicit_target();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(handle) = cancel_handle {
                        let _ = handle.cancel();
                    }
                }
                result = execution.wait() => {
                    let summary = match &result {
                        Ok(info) => format!(
                            "attach session for {step_name:?} ended (local attach client exit code {})",
                            info.exit_code
                        ),
                        Err(error) => format!("attach session for {step_name:?} failed: {error}"),
                    };
                    let failed = !matches!(&result, Ok(info) if info.exit_code == 0);
                    if let Ok(mut log) = status_log.lock() {
                        log.push(StatusLogEntry {
                            level: if failed { MessageLevel::Error } else { MessageLevel::Info },
                            text: summary,
                        });
                        if failed {
                            if let Some(tail) = output_tail.as_ref() {
                                for line in tail.snapshot().iter().filter(|line| !line.trim().is_empty()) {
                                    log.push(StatusLogEntry {
                                        level: MessageLevel::Info,
                                        text: format!("  {line}"),
                                    });
                                }
                            }
                        }
                    }
                    if single_attach {
                        push_slot_event(&events, ContainerSlotEvent::Exited { step_name });
                        cancel.cancel();
                    }
                }
            }
        });
        Ok(())
    }

    fn on_slot_exited(&mut self, step: &str) {
        push_slot_event(
            &self.container_slot_events,
            ContainerSlotEvent::Exited {
                step_name: step.to_string(),
            },
        );
    }

    fn on_workflow_state(&mut self, state: &WorkflowState) {
        if let Ok(mut view) = self.workflow_view.lock() {
            *view = Some(workflow_state_to_view_state(state));
        }
    }

    fn reachable_flag(&self) -> Option<Arc<AtomicBool>> {
        self.squad_attach_reachable.clone()
    }

    fn on_attach_finished(&mut self) {
        if let Some(session) = &self.squad_attach_session {
            let _ = session.end();
        }
    }
}

fn push_slot_event(
    events: &crate::frontend::tui::tabs::SharedContainerSlotEvents,
    event: ContainerSlotEvent,
) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}

/// Route the squad-tab attach action through the ordinary Dispatch path.
pub fn start_squad_attach(app: &mut App, task: &str) {
    let raw = format!("squad attach {task}");
    match Dispatch::<TuiCommandFrontend>::parse_command_box_input(&raw) {
        Ok(parsed) => app.spawn_command(parsed),
        Err(error) => app.status_bar.text = error.to_string(),
    }
}

/// End the local attach session while leaving every daemon-owned container
/// running.
pub fn detach_squad_attach(app: &mut App) -> bool {
    let tab = app.active_tab_mut();
    let Some(state) = tab.squad.as_ref() else {
        return false;
    };
    let Some(task) = state.end_attach() else {
        return false;
    };
    tab.command_result_rx = None;
    tab.end_attach_session();
    app.focus = Focus::ExecutionWindow;
    app.status_bar.text =
        format!("Detached from {task}. Its containers are still running — press 'a' to reattach.");
    app.needs_redraw = true;
    true
}
