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
    hub: Option<Arc<crate::engine::terminal_hub::TerminalHub>>,
    owner: Option<crate::data::terminal::TerminalOwnerInfo>,
    output_writer: Option<crate::data::fs::api_command_log::CommandStateWriter>,
}

impl CommandOwnedRuntime {
    pub fn new(inner: Arc<dyn AgentRuntimeEngine>, command_id: &str) -> Self {
        Self {
            output_writer: None,
            hub: inner.terminal_hub(),
            owner: crate::data::ids::CommandId::parse(command_id)
                .ok()
                .map(|id| crate::data::terminal::TerminalOwnerInfo {
                    owner: crate::data::terminal::TerminalOwner::Command(id),
                    feature: crate::data::daemon_state::FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: false,
                }),
            inner,
            prefix: command_container_prefix(command_id),
            launches: AtomicU64::new(0),
        }
    }

    pub fn with_hub(mut self, hub: Arc<crate::engine::terminal_hub::TerminalHub>) -> Self {
        self.hub = Some(hub);
        self
    }
    pub fn with_owner(mut self, owner: crate::data::terminal::TerminalOwnerInfo) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn with_output_writer(
        mut self,
        writer: crate::data::fs::api_command_log::CommandStateWriter,
    ) -> Self {
        writer.record_outputs_mounted(false);
        self.output_writer = Some(writer);
        self
    }
}

impl AgentRuntimeEngine for CommandOwnedRuntime {
    fn terminal_hub(&self) -> Option<Arc<crate::engine::terminal_hub::TerminalHub>> {
        self.hub.clone()
    }
    fn container_name_prefix(&self) -> Option<&str> {
        self.owner
            .as_ref()
            .and_then(|owner| {
                matches!(
                    owner.owner,
                    crate::data::terminal::TerminalOwner::Command(_)
                )
                .then_some(self.prefix.as_str())
            })
            .or_else(|| self.inner.container_name_prefix())
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
        let (agent, model) = match &options {
            ResolvedAgentOptions::Container(opts) => (
                opts.entrypoint
                    .as_ref()
                    .and_then(|e| e.0.first())
                    .cloned()
                    .unwrap_or_default(),
                opts.model.as_ref(),
            ),
            ResolvedAgentOptions::Sandbox(opts) => (opts.agent_id.clone(), opts.model.as_ref()),
        };
        let model = model.map(|m| match m {
            crate::engine::container::options::ModelFlagForm::Argument(s)
            | crate::engine::container::options::ModelFlagForm::Shorthand(s) => s.clone(),
        });
        let (agent, model) = match &options {
            ResolvedAgentOptions::Container(opts) => {
                opts.agent_identity.clone().unwrap_or((agent, model))
            }
            _ => (agent, model),
        };
        let options = match options {
            ResolvedAgentOptions::Container(mut container)
                if container.name.is_none()
                    && self.owner.as_ref().is_some_and(|o| {
                        matches!(o.owner, crate::data::terminal::TerminalOwner::Command(_))
                    }) =>
            {
                let n = self.launches.fetch_add(1, Ordering::Relaxed);
                // Unique across re-runs of the same command, whose earlier
                // (stopped) containers may still hold a name.
                let unique = &uuid::Uuid::new_v4().simple().to_string()[..8];
                container.name = Some(ContainerName::new(format!("{}{n}-{unique}", self.prefix)));
                ResolvedAgentOptions::Container(container)
            }
            other => other,
        };
        let mut options = options;
        if let (Some(writer), ResolvedAgentOptions::Container(options)) =
            (&self.output_writer, &mut options)
        {
            let expected = writer.output_dir();
            let mounted = options.overlays.iter().any(|overlay| {
                overlay.container_path
                    == std::path::Path::new(crate::data::fs::command_outputs::OUTPUT_MOUNT_PATH)
                    && expected.canonicalize().ok().is_some_and(|expected| {
                        overlay.host_path.canonicalize().ok().as_ref() == Some(&expected)
                    })
                    && overlay.permission
                        == crate::engine::container::options::OverlayPermission::ReadWrite
            });
            if mounted {
                if let Some(entrypoint) = &mut options.entrypoint {
                    let receipt = writer.output_mount_receipt()?;
                    let mut argv = vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf 1 > \"$1\" && rm -f -- \"$1\"; shift; exec \"$@\"".into(),
                        "awman-output-mount".into(),
                        receipt.to_string_lossy().into_owned(),
                    ];
                    argv.append(&mut entrypoint.0);
                    entrypoint.0 = argv;
                }
            }
        }
        let instance = self.inner.build(options)?;
        Ok(match (&self.hub, &self.owner) {
            (Some(hub), Some(owner)) => crate::engine::agent_runtime::execution::with_terminal_launch(instance,
                crate::engine::agent_runtime::execution::TerminalLaunch { hub: hub.clone(), owner: owner.clone(), agent, model, handle: Default::default(), drain: Arc::new(crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity()) }),
            _ => instance,
        })
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
