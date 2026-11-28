//! Run-scoped discovery. A terminal is an ephemeral slot, never the lifetime
//! of the Squad viewer. No runtime discovery or input policy lives in the TUI.
use super::*;
use crate::data::ids::{RunId, TaskId};

#[derive(Debug, Clone)]
pub struct PinnedRun {
    pub task: String,
    pub task_id: TaskId,
    pub run_id: RunId,
}

impl PinnedRun {
    pub async fn is_current(&self, gateway: &dyn TaskGateway) -> Result<bool, CommandError> {
        Ok(current_run(gateway, &self.task)
            .await?
            .is_some_and(|run| run.id == self.run_id && run.task_id == self.task_id))
    }
}

pub async fn pin_current_run(
    gateway: &dyn TaskGateway,
    task: &str,
) -> Result<PinnedRun, CommandError> {
    let record = if task.starts_with("task_") {
        let id = TaskId::parse(task).map_err(|error| CommandError::Other(error.to_string()))?;
        gateway
            .list()
            .await?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| CommandError::Other(format!("task {task:?} was not found")))?
    } else {
        gateway.get(task).await?
    };
    let run = current_run(gateway, &record.name)
        .await?
        .filter(|run| run.task_id == record.id)
        .ok_or_else(|| {
            CommandError::Other(format!("no run currently in progress for task {task:?}"))
        })?;
    Ok(PinnedRun {
        task: record.name,
        task_id: record.id,
        run_id: run.id,
    })
}

#[derive(Debug)]
pub enum FollowEvent {
    /// Complete open-slot set, validated against the pin before and after
    /// discovery. Empty is normal during image preparation or phase handoff.
    Snapshot(Vec<TerminalRecord>),
    DaemonUnreachable(String),
    RunEnded(TaskAttachEnd),
}

pub struct RunFollower {
    gateway: Arc<dyn TaskGateway>,
    pub pin: PinnedRun,
    policy: AttachStartupPolicy,
    step: Option<String>,
    container: Option<String>,
    next_poll: tokio::time::Instant,
}

impl RunFollower {
    pub fn new(
        gateway: Arc<dyn TaskGateway>,
        pin: PinnedRun,
        policy: AttachStartupPolicy,
        step: Option<String>,
        container: Option<String>,
    ) -> Self {
        Self {
            gateway,
            pin,
            policy,
            step,
            container,
            next_poll: tokio::time::Instant::now(),
        }
    }

    pub async fn next(&mut self, cancel: &CancellationToken) -> Result<FollowEvent, CommandError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(FollowEvent::RunEnded(TaskAttachEnd::Cancelled)),
            _ = tokio::time::sleep_until(self.next_poll) => {}
        }
        self.next_poll = tokio::time::Instant::now() + self.policy.check_interval;
        let read = async {
            if !self.pin.is_current(self.gateway.as_ref()).await? {
                return Ok(None);
            }
            let terminals = self.gateway.run_terminals(&self.pin.run_id).await?;
            // Never publish a stale response that crossed a run replacement.
            if !self.pin.is_current(self.gateway.as_ref()).await? {
                return Ok(None);
            }
            Ok::<_, CommandError>(Some(terminals))
        };
        let snapshot = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(FollowEvent::RunEnded(TaskAttachEnd::Cancelled)),
            snapshot = read => snapshot,
        };
        let mut records = match snapshot {
            Err(error) => return Ok(FollowEvent::DaemonUnreachable(error.to_string())),
            Ok(None) => return Ok(FollowEvent::RunEnded(TaskAttachEnd::RunCompleted)),
            Ok(Some(records)) => records,
        };
        records.retain(|record| record.owner_id == TerminalOwner::Run(self.pin.run_id.clone()));
        // Resolve an explicit prefix once; later terminals cannot change its
        // meaning or silently move the viewer to another container.
        if let Some(selector) = &self.container {
            let known: std::collections::BTreeSet<_> = records
                .iter()
                .filter_map(|r| r.source.as_ref()?.container.as_deref())
                .collect();
            if !known.is_empty() {
                let exact = known.contains(selector.as_str());
                let matches: Vec<_> = known
                    .iter()
                    .copied()
                    .filter(|id| {
                        if exact {
                            *id == selector
                        } else {
                            id.starts_with(selector)
                        }
                    })
                    .collect();
                if matches.len() != 1 {
                    return Err(CommandError::Other(format!(
                        "container {selector:?} is not an unambiguous running container for task {:?}; legal containers: {}",
                        self.pin.task, known.iter().copied().collect::<Vec<_>>().join(", ")
                    )));
                }
                self.container = Some(matches[0].to_string());
            }
        }
        records.retain(|record| {
            record.state == TerminalState::Open
                && self.step.as_deref().is_none_or(|step| {
                    record.source.as_ref().and_then(|s| s.step_name.as_deref()) == Some(step)
                })
                && self.container.as_deref().is_none_or(|container| {
                    record.source.as_ref().and_then(|s| s.container.as_deref()) == Some(container)
                })
        });
        Ok(FollowEvent::Snapshot(records))
    }
}
