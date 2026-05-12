//! `AgentSetupFrontend` — Layer 2 lifecycle decision: download / build the
//! requested agent, fall back to default, or abort.

use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::message::{UserMessage, UserMessageSink};
use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSetupDecision {
    Setup,
    FallbackToDefault,
    Abort,
}

pub trait AgentSetupFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError>;

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName);
}

/// Marker trait implemented by every per-command frontend that needs to
/// hand a `ContainerFrontend` down to Layer-1 engines. Lets the
/// `AgentFrontendAdapter` below stay generic without each per-command frontend
/// trait having to be its own bound.
pub trait HasContainerFrontend: UserMessageSink + Send {
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;

    /// Like `container_frontend`, but the returned frontend is allowed to
    /// surrender its byte-stream I/O channels to the engine for direct PTY
    /// bridging via `ContainerFrontend::take_container_io`.
    ///
    /// Commands that intend to launch an *interactive* PTY container (chat,
    /// exec prompt) call this variant so the container's PTY is wired
    /// to the frontend's renderer instead of inheriting host stdio.
    /// Build/pull/probe paths keep using `container_frontend` so the io stays
    /// reserved for the actual interactive launch.
    ///
    /// Default impl falls back to `container_frontend` — appropriate for CLI
    /// frontends that already inherit a real host terminal.
    fn container_frontend_for_pty(&mut self) -> Box<dyn ContainerFrontend> {
        self.container_frontend()
    }
}

/// Adapter that wraps any per-command frontend implementing
/// [`HasContainerFrontend`] and exposes the engine's `AgentFrontend` trait.
/// Used by `chat`, `exec prompt`, etc. to call `AgentEngine::ensure_available`
/// without each per-command frontend trait having to implement
/// `report_step_status` itself.
pub struct AgentFrontendAdapter<'a, F: ?Sized + HasContainerFrontend> {
    inner: &'a mut F,
}

impl<'a, F: ?Sized + HasContainerFrontend> AgentFrontendAdapter<'a, F> {
    pub fn new(inner: &'a mut F) -> Self {
        Self { inner }
    }
}

impl<F: ?Sized + HasContainerFrontend> UserMessageSink for AgentFrontendAdapter<'_, F> {
    fn write_message(&mut self, msg: UserMessage) {
        self.inner.write_message(msg);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

impl<F: ?Sized + HasContainerFrontend> crate::engine::agent::AgentFrontend
    for AgentFrontendAdapter<'_, F>
{
    fn report_step_status(&mut self, step: &str, status: StepStatus) {
        let level = match &status {
            StepStatus::Failed(_) => crate::engine::message::MessageLevel::Error,
            StepStatus::Warn(_) => crate::engine::message::MessageLevel::Warning,
            _ => crate::engine::message::MessageLevel::Info,
        };
        let text = match status {
            StepStatus::Failed(msg) => format!("{step}: failed — {msg}"),
            StepStatus::Warn(msg) => format!("{step}: {msg}"),
            StepStatus::Done => format!("{step}: done"),
            StepStatus::Running => format!("{step}: running"),
            StepStatus::Skipped => format!("{step}: skipped"),
            StepStatus::Pending => format!("{step}: pending"),
        };
        self.inner.write_message(UserMessage { level, text });
    }

    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
        self.inner.container_frontend()
    }
}
