//! Task-terminal startup decisions shared by `attach task_…` and `squad attach`.
//! The daemon owns terminals and input grants; viewers never attach through a
//! second runtime client. Only absence in a confirmed running run is retried.

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::error::CommandError;
use crate::data::fs::{Run, RunStatus};
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::terminal::{TerminalOwner, TerminalRecord, TerminalState};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod follow;
pub use follow::{pin_current_run, FollowEvent, PinnedRun, RunFollower};

pub const WAITING_FOR_TERMINAL: &str = "Waiting for agent terminal to start…";

/// Inject a short policy in component tests; production uses the bounded
/// ten-second window and short checks below.
#[derive(Debug, Clone, Copy)]
pub struct AttachStartupPolicy {
    pub window: Duration,
    pub check_interval: Duration,
}
impl Default for AttachStartupPolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(10),
            check_interval: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAttachEnd {
    Cancelled,
    RunCompleted,
    StartupTimedOut,
}

impl TaskAttachEnd {
    pub fn message(self, policy: AttachStartupPolicy) -> String {
        match self {
            Self::Cancelled => "Attach session ended.".into(),
            Self::RunCompleted => "The run finished or was replaced; attach session ended.".into(),
            Self::StartupTimedOut => format!(
                "The agent terminal did not start within {} seconds. Try attaching again.",
                policy.window.as_secs_f64()
            ),
        }
    }
}

#[derive(Debug)]
pub enum TaskTerminalResolution {
    Ready(Box<TerminalRecord>),
    End(TaskAttachEnd),
}

async fn current_run(gateway: &dyn TaskGateway, task: &str) -> Result<Option<Run>, CommandError> {
    Ok(gateway
        .runs(task, 1)
        .await?
        .into_iter()
        .next()
        .filter(|r| r.status == RunStatus::Running))
}

/// Pin the task and run before waiting. Recheck the run after every terminal
/// snapshot so a delayed response cannot bind a previous or replacement run.
/// There is no blocking picker: the existing current-terminal selection is
/// retained, with explicit `--step` / `--container` disambiguation available.
pub async fn resolve_task_terminal(
    gateway: Arc<dyn TaskGateway>,
    task: &str,
    step: Option<&str>,
    container: Option<&str>,
    policy: AttachStartupPolicy,
    cancel: &CancellationToken,
    progress: &mut (dyn FnMut(UserMessage) + Send),
) -> Result<TaskTerminalResolution, CommandError> {
    let resolve = async {
        let task_record = if task.starts_with("task_") {
            let id = crate::data::ids::TaskId::parse(task)
                .map_err(|error| CommandError::Other(error.to_string()))?;
            gateway
                .list()
                .await?
                .into_iter()
                .find(|t| t.id == id)
                .ok_or_else(|| CommandError::Other(format!("task {task:?} was not found")))?
        } else {
            gateway.get(task).await?
        };
        let name = &task_record.name;
        let run = current_run(gateway.as_ref(), name)
            .await?
            .filter(|r| r.task_id == task_record.id)
            .ok_or_else(|| {
                CommandError::Other(format!("no run currently in progress for task {task:?}"))
            })?;
        let deadline = tokio::time::Instant::now() + policy.window;
        let mut announced = false;
        loop {
            let check = async {
                let Some(current) = current_run(gateway.as_ref(), name).await? else {
                    return Ok(Some(TaskTerminalResolution::End(
                        TaskAttachEnd::RunCompleted,
                    )));
                };
                if current.id != run.id || current.task_id != run.task_id {
                    return Ok(Some(TaskTerminalResolution::End(
                        TaskAttachEnd::RunCompleted,
                    )));
                }
                let records = gateway.run_terminals(&run.id).await?;
                if !current_run(gateway.as_ref(), name)
                    .await?
                    .is_some_and(|r| r.id == run.id && r.task_id == run.task_id)
                {
                    return Ok(Some(TaskTerminalResolution::End(
                        TaskAttachEnd::RunCompleted,
                    )));
                }
                let mut owned: Vec<_> = records
                    .into_iter()
                    .filter(|r| r.owner_id == TerminalOwner::Run(run.id.clone()))
                    .collect();
                if let Some(selector) = container {
                    let known: Vec<_> = owned
                        .iter()
                        .filter_map(|r| r.source.as_ref()?.container.as_deref())
                        .collect();
                    if !known.is_empty() {
                        let exact = known.contains(&selector);
                        let matches: std::collections::BTreeSet<_> = known
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
                            return Err(CommandError::Other(format!("container {selector:?} is not an unambiguous running container for task {task:?}; legal containers: {}", known.join(", "))));
                        }
                        let selected = matches.first().expect("one match").to_string();
                        owned.retain(|r| {
                            r.source.as_ref().and_then(|s| s.container.as_deref())
                                == Some(selected.as_str())
                        });
                    }
                }
                if let Some(record) = owned.into_iter().find(|r| {
                    r.state == TerminalState::Open
                        && (container.is_none()
                            || r.source
                                .as_ref()
                                .and_then(|s| s.container.as_ref())
                                .is_some())
                        && step.is_none_or(|step| {
                            r.source.as_ref().and_then(|s| s.step_name.as_deref()) == Some(step)
                        })
                }) {
                    return Ok(Some(TaskTerminalResolution::Ready(Box::new(record))));
                }
                Ok(None)
            };
            let snapshot = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(TaskTerminalResolution::End(TaskAttachEnd::Cancelled)),
                _ = tokio::time::sleep_until(deadline) => return Ok(TaskTerminalResolution::End(TaskAttachEnd::StartupTimedOut)),
                snapshot = check => snapshot?,
            };
            match snapshot {
                None => {}
                Some(other) => return Ok(other),
            }
            if cancel.is_cancelled() {
                return Ok(TaskTerminalResolution::End(TaskAttachEnd::Cancelled));
            }
            if !announced {
                progress(UserMessage {
                    level: MessageLevel::Info,
                    text: WAITING_FOR_TERMINAL.into(),
                });
                announced = true;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(TaskTerminalResolution::End(TaskAttachEnd::Cancelled)),
                _ = tokio::time::sleep_until(deadline) => return Ok(TaskTerminalResolution::End(TaskAttachEnd::StartupTimedOut)),
                _ = tokio::time::sleep(policy.check_interval) => {},
            }
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(TaskTerminalResolution::End(TaskAttachEnd::Cancelled)),
        result = resolve => result,
    }
}
