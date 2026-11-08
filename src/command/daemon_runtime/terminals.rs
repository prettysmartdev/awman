//! The daemon's one [`TerminalOperations`] (WI 0141): access policy read
//! from live config, terminal events written to the owner's event log, and
//! squad task → run lookup for `awman attach task_…`.

use std::sync::{Arc, Weak};

use crate::command::terminals::{AccessPolicy, OwnerEventLog, OwnerResolver, TerminalOperations};
use crate::data::daemon_state::FeatureName;
use crate::data::execution_event::{EventPayload, EventSource};
use crate::data::ids::{RunId, TaskId};
use crate::data::terminal::TerminalOwner;

use super::DaemonRuntime;

impl DaemonRuntime {
    /// The terminal operations every transport (`/v1/terminals`, the local
    /// attach) uses. Built once; the first call also starts the lifecycle
    /// publisher (`terminal_opened` / `terminal_closed`), so bootstrap calls
    /// it before any feature can start a PTY.
    pub fn terminals(self: &Arc<Self>) -> Arc<TerminalOperations> {
        self.terminals
            .get_or_init(|| {
                let weak = Arc::downgrade(self);
                let ops = Arc::new(TerminalOperations::new(
                    self.core.terminal_hub().clone(),
                    Arc::new(move || policy_for(&weak)),
                    Arc::new(CommandEventLog::new(self)),
                    Arc::new(TaskRuns {
                        runtime: Arc::downgrade(self),
                    }),
                ));
                if tokio::runtime::Handle::try_current().is_ok() {
                    ops.spawn_lifecycle_publisher();
                }
                ops
            })
            .clone()
    }
}

/// `network_features` is what the network listener actually serves:
/// listed in `daemon.network.features` and enabled by config.
fn policy_for(runtime: &Weak<DaemonRuntime>) -> AccessPolicy {
    let Some(runtime) = runtime.upgrade() else {
        return AccessPolicy::default();
    };
    AccessPolicy {
        network_features: FeatureName::ALL
            .into_iter()
            .filter(|f| runtime.network_serves(*f))
            .collect(),
        remote_input: runtime.config().daemon().terminal.remote_input,
    }
}

/// A single reliable queue preserves publication order even while the async
/// command bus map is locked. PTY threads never drop or independently spawn
/// contending event sends.
struct CommandEventLog {
    sender: tokio::sync::mpsc::UnboundedSender<(TerminalOwner, Option<EventSource>, EventPayload)>,
}
impl CommandEventLog {
    fn new(runtime: &Arc<DaemonRuntime>) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<(
            TerminalOwner,
            Option<EventSource>,
            EventPayload,
        )>();
        let runtime = Arc::downgrade(runtime);
        tokio::spawn(async move {
            while let Some((owner, source, payload)) = receiver.recv().await {
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                match owner {
                    TerminalOwner::Command(id) => {
                        if let Some(bus) = runtime.event_buses.lock().await.get(id.as_str()) {
                            let sender = bus.sender();
                            match source {
                                Some(source) => sender.with_source(source).emit(payload),
                                None => sender.emit(payload),
                            }
                        }
                    }
                    TerminalOwner::Run(id) => {
                        let write = || -> Result<(), crate::data::error::DataError> {
                            let Some(name) = runtime.core.task_store().task_name_for_run(&id)?
                            else {
                                return Ok(());
                            };
                            let paths = crate::data::fs::SquadPaths::from_root(
                                runtime.core.data_paths().root().join("squad"),
                            );
                            crate::data::fs::SquadRunPaths::new(&paths.task_dir(&name)?, &id)?
                                .append_event(source, payload)
                        };
                        if let Err(error) = write() {
                            tracing::warn!(%error, run_id = %id, "terminal event log write failed");
                        }
                    }
                    _ => {
                        tracing::debug!(owner = %owner, event = payload.sse_event_type(), "terminal event")
                    }
                }
            }
        });
        Self { sender }
    }
}
impl OwnerEventLog for CommandEventLog {
    fn append(&self, owner: &TerminalOwner, payload: EventPayload) {
        self.append_with_source(owner, None, payload);
    }
    fn append_with_source(
        &self,
        owner: &TerminalOwner,
        source: Option<&EventSource>,
        payload: EventPayload,
    ) {
        let _ = self.sender.send((owner.clone(), source.cloned(), payload));
    }
}

struct TaskRuns {
    runtime: Weak<DaemonRuntime>,
}

impl OwnerResolver for TaskRuns {
    fn active_run(&self, task: &TaskId) -> Option<RunId> {
        let runtime = self.runtime.upgrade()?;
        let run = runtime.core.task_store().running_run_for(task).ok()??;
        Some(run.id)
    }
}
