//! The TUI task viewer follows one pinned run, using the same terminal
//! transport, replay pipeline and daemon grants as ordinary attach.
use super::*;
use crate::command::commands::remote_client::{RemoteWorkflowPoller, SquadTaskWorkflowSource};
use crate::command::commands::squad::attach::{
    pin_current_run, AttachStartupPolicy, FollowEvent, RunFollower, TaskAttachEnd,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

struct SlotTask {
    step_name: String,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SlotTask {
    fn drop(&mut self) {
        // Cancels the viewer bridge, never the producer or its container.
        self.cancel.cancel();
    }
}

struct FollowTasks {
    cancel: CancellationToken,
    poller: tokio::task::JoinHandle<()>,
    slots: HashMap<TerminalId, SlotTask>,
}
/// One deadline for the whole viewer teardown, well below the presenter's
/// reply timeout, so a reattach right after Ctrl-\\ is never refused while
/// many slots or a half-dead socket wind down.
const FOLLOW_TEARDOWN_DEADLINE: Duration = Duration::from_secs(3);

impl FollowTasks {
    /// Cancel the poller and every slot bridge at once, then wait for all of
    /// them together under [`FOLLOW_TEARDOWN_DEADLINE`]; whatever is still
    /// running after it is aborted. Producers are never touched.
    async fn teardown(&mut self) {
        self.cancel.cancel();
        let mut slots: Vec<SlotTask> = self.slots.drain().map(|(_, slot)| slot).collect();
        for slot in &slots {
            slot.cancel.cancel();
        }
        let joined = {
            let poller = &mut self.poller;
            let bridges = slots.iter_mut().map(|slot| &mut slot.task);
            tokio::time::timeout(
                FOLLOW_TEARDOWN_DEADLINE,
                futures_util::future::join(poller, futures_util::future::join_all(bridges)),
            )
            .await
        };
        if joined.is_err() {
            self.poller.abort();
            for slot in &slots {
                slot.task.abort();
            }
        }
    }
}

impl Drop for FollowTasks {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl AttachCommand {
    pub(super) async fn gateway(&self) -> Result<Arc<dyn TaskGateway>, CommandError> {
        match self.task_gateway.clone() {
            Some(gateway) => Ok(gateway),
            None => super::super::squad::supervisor::DaemonResolver::from_env(&self.env)?
                .gateway_for(crate::command::dispatch::catalogue::GatewayNeed::Running)
                .await?
                .ok_or(CommandError::DaemonNotRunning),
        }
    }

    pub(super) async fn follow_task(
        &self,
        gateway: Arc<dyn TaskGateway>,
        frontend: &mut dyn AttachCommandFrontend,
        cancel: &CancellationToken,
    ) -> Result<AttachOutcome, CommandError> {
        let AttachTarget::Task(id) = &self.target else {
            unreachable!("task follower target")
        };
        frontend.attach_view_state(AttachViewState::Resolving);
        let pin = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CommandError::Aborted),
            pin = pin_current_run(gateway.as_ref(), self.task_name.as_deref().unwrap_or(id.as_str())) => pin?,
        };
        let source = Arc::new(SquadTaskWorkflowSource::pinned(
            gateway.clone(),
            &pin.task,
            pin.run_id.clone(),
        ));
        let reachable = Arc::new(AtomicBool::new(true));
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(None);
        let session_cancel = cancel.child_token();
        let poller = RemoteWorkflowPoller::new(
            source,
            Box::new(move |state| {
                let _ = state_tx.send(Some(state.clone()));
            }),
        )
        .with_reachable(reachable.clone())
        .with_stop_on_disappearance(false)
        .start(session_cancel.clone());
        let mut tasks = FollowTasks {
            cancel: session_cancel,
            poller,
            slots: HashMap::new(),
        };
        let mut follower = RunFollower::new(
            gateway.clone(),
            pin,
            AttachStartupPolicy::default(),
            self.step.clone(),
            self.container.clone(),
        );
        let hangup = frontend.hangup();
        let mut saw_terminal = false;
        let mut saw_workflow = false;
        let mut last_terminal = None;
        let mut last_state = AttachViewState::Resolving;
        let mut workflow_open = true;
        // Keep the discovery future alive while the workflow watch changes;
        // restarting it could starve terminal discovery on a busy workflow.
        let result = 'follow: loop {
            let next = follower.next(cancel);
            tokio::pin!(next);
            let event = loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break 'follow Ok((TaskAttachEnd::Cancelled, DetachReason::User)),
                    _ = hangup.cancelled() => break 'follow Ok((TaskAttachEnd::Cancelled, DetachReason::Hangup)),
                    event = &mut next => break event,
                    changed = state_rx.changed(), if workflow_open => {
                        if changed.is_ok() {
                            let state = state_rx.borrow_and_update().clone();
                            if let Some(state) = state {
                                frontend.publish_workflow(&state);
                                saw_workflow = true;
                            }
                        } else {
                            workflow_open = false;
                        }
                    }
                }
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => break Err(error),
            };
            let state = match event {
                FollowEvent::RunEnded(end) => break Ok((end, DetachReason::User)),
                FollowEvent::DaemonUnreachable(_) => AttachViewState::DaemonUnreachable,
                FollowEvent::Snapshot(records) => {
                    let ids: HashSet<_> = records.iter().map(|r| r.id.clone()).collect();
                    tasks.slots.retain(|id, task| {
                        if !ids.contains(id) {
                            frontend.remove_terminal_slot(&task.step_name);
                            return false;
                        }
                        true
                    });
                    let mut connection_failed = false;
                    for record in &records {
                        if tasks
                            .slots
                            .get(&record.id)
                            .is_some_and(|slot| !slot.task.is_finished())
                        {
                            continue;
                        }
                        let options = TerminalClientOptions {
                            annotations: self.annotations.clone(),
                            actor: None,
                            read_only: self.read_only,
                            history: true,
                        };
                        let stream = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break 'follow Ok((TaskAttachEnd::Cancelled, DetachReason::User)),
                            result = tokio::time::timeout(Duration::from_secs(2), gateway.open_terminal(&record.id, &options, &self.env)) => match result {
                                Ok(Ok(stream)) => stream,
                                _ => { connection_failed = true; continue; }
                            }
                        };
                        let source = record.source.as_ref();
                        let label = source
                            .and_then(|s| s.step_name.as_deref())
                            .unwrap_or("leader");
                        let step_name = format!("{label}:{}", record.id);
                        let slot_cancel = tasks.cancel.child_token();
                        let task = match frontend.bind_terminal_slot(
                            AttachSlot {
                                step_name: step_name.clone(),
                                agent: record.agent.clone(),
                                container: source
                                    .and_then(|s| s.container.clone())
                                    .unwrap_or_else(|| record.id.to_string()),
                            },
                            stream,
                            self.read_only,
                            self.host_output_policy,
                            slot_cancel.clone(),
                        ) {
                            Ok(task) => task,
                            Err(error) => break 'follow Err(error),
                        };
                        tasks.slots.insert(
                            record.id.clone(),
                            SlotTask {
                                step_name,
                                cancel: slot_cancel,
                                task,
                            },
                        );
                        saw_terminal = true;
                        last_terminal = Some(record.id.clone());
                    }
                    if connection_failed || !reachable.load(Ordering::Relaxed) {
                        AttachViewState::DaemonUnreachable
                    } else if !records.is_empty() {
                        AttachViewState::Live
                    } else if !saw_terminal {
                        AttachViewState::WaitingForTerminal
                    } else if !saw_workflow {
                        AttachViewState::WorkflowStateMissing
                    } else {
                        AttachViewState::BetweenTerminals
                    }
                }
            };
            if state != last_state {
                frontend.attach_view_state(state.clone());
                last_state = state;
            }
        };
        tasks.teardown().await;
        let (end, detach_reason) = result?;
        frontend.attach_view_state(AttachViewState::Ended(end));
        Ok(AttachOutcome {
            terminal_id: last_terminal,
            startup_end: Some(end),
            exit_code: 0,
            detach_reason,
        })
    }
}

#[cfg(test)]
mod teardown_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn many_stuck_bridges_are_torn_down_under_one_deadline() {
        let cancel = CancellationToken::new();
        let poller = tokio::spawn(std::future::pending::<()>());
        let mut slots = HashMap::new();
        for _ in 0..7 {
            slots.insert(
                TerminalId::new(),
                SlotTask {
                    step_name: "build".into(),
                    cancel: CancellationToken::new(),
                    // A bridge stuck on a half-dead socket ignores its token.
                    task: tokio::spawn(std::future::pending::<()>()),
                },
            );
        }
        let mut tasks = FollowTasks {
            cancel,
            poller,
            slots,
        };
        let started = tokio::time::Instant::now();
        tasks.teardown().await;
        let elapsed = started.elapsed();
        assert!(elapsed <= FOLLOW_TEARDOWN_DEADLINE, "{elapsed:?}");
        assert!(tasks.slots.is_empty());
        tokio::task::yield_now().await;
        assert!(tasks.poller.is_finished());
    }
}
