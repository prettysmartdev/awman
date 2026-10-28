//! Command-owned agent containers (WI 0135).
//!
//! The queue worker runs each API command against [`CommandOwnedRuntime`],
//! which names every container that command launches
//! `awman-cmd-<command id>-<n>`. The container name is the one identity
//! channel every runtime tier honours, so after a daemon restart the
//! containers a cancelled command left behind are found by that prefix and
//! stopped, and no other command's agents are touched.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::data::oci_identity::ImageIdentity;
use crate::data::session::Session;
use crate::engine::agent_runtime::{
    AgentDetail, AgentHandle, AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities,
    ImageImportRequest, ImportedImage, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::container::options::ContainerName;
use crate::engine::error::EngineError;

/// Name prefix of every container launched for `command_id`.
pub fn command_container_prefix(command_id: &str) -> String {
    let id: String = command_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("awman-cmd-{id}-")
}

/// Stop and remove every running container launched for `command_id`, the
/// way `agents kill` does. Returns how many were stopped.
pub fn stop_command_containers(
    runtime: &dyn AgentRuntimeEngine,
    command_id: &str,
) -> Result<usize, EngineError> {
    let handles = runtime.list_running_with_name_prefix(&command_container_prefix(command_id))?;
    let mut stopped = 0;
    let mut failure = None;
    for handle in &handles {
        match crate::command::commands::agents::kill::stop_and_remove_agent(runtime, handle) {
            Ok(_) => stopped += 1,
            Err(error) => {
                tracing::warn!(
                command_id,
                container = %handle.name,
                %error,
                "Failed to stop a cancelled command's container"
                );
                failure = Some(error);
            }
        }
    }
    failure.map_or(Ok(stopped), Err)
}

/// Delegates to `inner`, naming each container-paradigm launch that has no
/// explicit name after the command. Sandbox names are deterministic per
/// worktree and are left alone.
pub struct CommandOwnedRuntime {
    inner: Arc<dyn AgentRuntimeEngine>,
    prefix: String,
    launches: AtomicU64,
}

impl CommandOwnedRuntime {
    pub fn new(inner: Arc<dyn AgentRuntimeEngine>, command_id: &str) -> Self {
        Self {
            inner,
            prefix: command_container_prefix(command_id),
            launches: AtomicU64::new(0),
        }
    }
}

impl AgentRuntimeEngine for CommandOwnedRuntime {
    fn container_name_prefix(&self) -> Option<&str> {
        Some(&self.prefix)
    }

    fn runtime_name(&self) -> &'static str {
        self.inner.runtime_name()
    }

    fn display_name(&self) -> &'static str {
        self.inner.display_name()
    }

    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn availability(&self) -> Result<(), EngineError> {
        self.inner.availability()
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        let options = match options {
            ResolvedAgentOptions::Container(mut container) if container.name.is_none() => {
                let n = self.launches.fetch_add(1, Ordering::Relaxed);
                // Unique across re-runs of the same command, whose earlier
                // (stopped) containers may still hold a name.
                let unique = &uuid::Uuid::new_v4().simple().to_string()[..8];
                container.name = Some(ContainerName::new(format!("{}{n}-{unique}", self.prefix)));
                ResolvedAgentOptions::Container(container)
            }
            other => other,
        };
        self.inner.build(options)
    }

    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.inner.list_running(session)
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.inner.list_running_all()
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.inner.stats(handle)
    }

    fn stats_by_name(&self, name: &str) -> Result<AgentStats, EngineError> {
        self.inner.stats_by_name(name)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.inner.stop(handle)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.inner.attach(handle)
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        self.inner.list_running_with_name_prefix(prefix)
    }

    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.inner.list_stopped()
    }

    fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError> {
        self.inner.inspect(handle)
    }

    fn reattach_after_owner_exit(&self) -> bool {
        self.inner.reattach_after_owner_exit()
    }

    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        self.inner.remove_agent(id)
    }

    fn ready_agent(
        &self,
        agent: &str,
        opts: ReadyAgentOptions,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        self.inner.ready_agent(agent, opts, sink)
    }

    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        self.inner.image_exists(tag)
    }

    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError> {
        self.inner.image_home_dir(tag)
    }

    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        self.inner.build_image(request, on_line)
    }

    fn import_image(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.inner.import_image(request, sink)
    }

    fn import_image_cancellable(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<ImportedImage, EngineError> {
        self.inner.import_image_cancellable(request, sink, cancel)
    }

    fn image_identity(&self, tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        self.inner.image_identity(tag)
    }

    fn image_matches_source(&self, request: &ImageImportRequest) -> Result<bool, EngineError> {
        self.inner.image_matches_source(request)
    }
}
