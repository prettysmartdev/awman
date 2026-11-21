//! `SquadAttachFrontend` for the instance command frontend: the Layer 2
//! producer half of `squad attach` (moved from `frontend::tui::squad_attach`,
//! WI 0142 D2). It publishes container slots and workflow views; the tab draws.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::attach::{
    format_candidates, SquadAttachFrontend, SquadContainer,
};
use crate::command::error::CommandError;
use crate::command::instance::feeds::StatusLogEntry;
use crate::command::instance::feeds::{
    workflow_state_to_view_state, ContainerSlotEvent, ContainerSlotIo, SharedSquadAttach,
};
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::command::instance::per_command::InstanceContainerProxy;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::MessageLevel;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::AgentInstance;

impl InstanceCommandFrontend {
    /// Bind the squad tab's attach session and reachability flag (squad tabs
    /// only). Formerly `with_squad_attach_context`.
    pub fn with_squad_attach(mut self, attach: Option<SharedSquadAttach>) -> Self {
        match attach {
            Some(attach) => {
                self.squad_attach_session = Some(attach.session);
                self.squad_attach_reachable = Some(attach.daemon_reachable);
            }
            None => {
                self.squad_attach_session = None;
                self.squad_attach_reachable = None;
            }
        }
        self
    }
}

impl SquadAttachFrontend for InstanceCommandFrontend {
    fn begin_attach(&mut self, task: &str) -> Result<CancellationToken, CommandError> {
        let Some(session) = &self.squad_attach_session else {
            return Err(CommandError::Other(
                "squad attach is only available from the squad tab".to_string(),
            ));
        };
        self.workflow.update_view(|view| *view = None);
        if let Ok(mut events) = self.container.events.lock() {
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
        let (agent_output_tx, mut agent_output_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let filtered_output_tx = stdout_tx.clone();
        let policy = self.host_output_policy;
        let presentation = self.presentation.clone();
        tokio::spawn(async move {
            let mut filter = presentation.host_output_filter(policy);
            while let Some(bytes) = agent_output_rx.recv().await {
                let mut safe = Vec::new();
                filter.feed(&bytes, &mut safe);
                if !safe.is_empty() && filtered_output_tx.send(safe).is_err() {
                    return;
                }
            }
            let mut safe = Vec::new();
            filter.finish(&mut safe);
            if !safe.is_empty() {
                let _ = filtered_output_tx.send(safe);
            }
        });
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let initial_size = self.pty_size.get();
        let io = AgentIo {
            stdout: agent_output_tx.clone(),
            stderr: agent_output_tx,
            stdin_tx: stdin_tx.clone(),
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        };
        let proxy = InstanceContainerProxy::with_io(self.status_log.clone(), io, None);
        let mut execution = instance
            .run_with_frontend(Box::new(proxy))
            .map_err(CommandError::from)?;

        push_slot_event(
            &self.container.events,
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
            &self.container.events,
            ContainerSlotEvent::ContainerName {
                step_name: step.to_string(),
                container_name: preview.name.clone(),
            },
        );

        let cancel = self
            .squad_attach_session
            .as_ref()
            .and_then(|session| session.cancel_token())
            .unwrap_or_default();
        let cancel_handle = execution.cancel_handle();
        let events = self.container.events.clone();
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
                        push_slot_event(&events, ContainerSlotEvent::AttachedExited { step_name, container_name: preview.name });
                        cancel.cancel();
                    }
                }
            }
        });
        Ok(())
    }

    fn on_slot_exited(&mut self, step: &str) {
        push_slot_event(
            &self.container.events,
            ContainerSlotEvent::Exited {
                step_name: step.to_string(),
            },
        );
    }

    fn on_workflow_state(&mut self, state: &WorkflowState) {
        self.workflow
            .publish_remote(state.invocation_id, workflow_state_to_view_state(state));
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
    events: &crate::command::instance::feeds::SharedContainerSlotEvents,
    event: ContainerSlotEvent,
) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
}
