//! `AgentSetupFrontend` — Layer 2 lifecycle decision: download / build the
//! requested agent, fall back to default, or abort.

use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::data::step_status::StepStatus;
use crate::engine::agent_runtime::frontend::AgentFrontend;

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
/// hand a `AgentFrontend` down to Layer-1 engines. Lets the
/// `AgentFrontendAdapter` below stay generic without each per-command frontend
/// trait having to be its own bound.
pub trait HasAgentFrontend: UserMessageSink + Send {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend>;

    /// Like `container_frontend`, but the returned frontend is allowed to
    /// surrender its byte-stream I/O channels to the engine for direct PTY
    /// bridging via `AgentFrontend::take_io`.
    ///
    /// Commands that intend to launch an *interactive* PTY container (chat,
    /// exec prompt) call this variant so the container's PTY is wired
    /// to the frontend's renderer instead of inheriting host stdio.
    /// Build/pull/probe paths keep using `container_frontend` so the io stays
    /// reserved for the actual interactive launch.
    ///
    /// Default impl falls back to `container_frontend` — appropriate for CLI
    /// frontends that already inherit a real host terminal.
    fn container_frontend_for_pty(&mut self) -> Box<dyn AgentFrontend> {
        self.container_frontend()
    }
}

/// Adapter that wraps any per-command frontend implementing
/// [`HasAgentFrontend`] and exposes the engine's `AgentFrontend` trait.
/// Used by `chat`, `exec prompt`, etc. to call `AgentEngine::ensure_available`
/// without each per-command frontend trait having to implement
/// `report_step_status` itself.
pub struct AgentFrontendAdapter<'a, F: ?Sized + HasAgentFrontend> {
    inner: &'a mut F,
}

impl<'a, F: ?Sized + HasAgentFrontend> AgentFrontendAdapter<'a, F> {
    pub fn new(inner: &'a mut F) -> Self {
        Self { inner }
    }
}

impl<F: ?Sized + HasAgentFrontend> UserMessageSink for AgentFrontendAdapter<'_, F> {
    fn write_message(&mut self, msg: UserMessage) {
        self.inner.write_message(msg);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

impl<F: ?Sized + HasAgentFrontend> crate::engine::agent::AgentImageFrontend
    for AgentFrontendAdapter<'_, F>
{
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: StepStatus,
    ) {
        let level = match &status {
            StepStatus::Failed(_) => crate::data::message::MessageLevel::Error,
            StepStatus::Warn(_) => crate::data::message::MessageLevel::Warning,
            _ => crate::data::message::MessageLevel::Info,
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

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.inner.container_frontend()
    }
}

/// The surface every command that launches an agent needs from its frontend.
///
/// `chat`, `exec prompt`, `exec workflow` and `specs` each declared the same
/// mount-scope / agent-setup / agent-auth / container-frontend bounds plus
/// their own copy of `set_pty_active` and `set_stuck_sender`. This trait is
/// that shared surface, declared once (F-35).
pub trait AgentLaunchFrontend:
    crate::command::commands::mount_scope::MountScopeFrontend
    + AgentSetupFrontend
    + crate::command::commands::agent_auth::AgentAuthFrontend
    + HasAgentFrontend
{
    /// Inform the frontend that host stdio is now owned by a running
    /// container (`true`) or has been released (`false`).
    ///
    /// **Required, with no default.** The two frontends that previously took
    /// the no-op default — `SpecsCommandFrontend` and
    /// `ExecPromptCommandFrontend` — were the CLI and API, and both already
    /// had a real implementation through their `chat` / `exec workflow` impls;
    /// the default only ever hid the fact that the same type answered the same
    /// question two different ways depending on which command was running. A
    /// frontend that genuinely has nothing to gate writes an empty body and
    /// says so there, which is visible in review; a silent default is not.
    fn set_pty_active(&mut self, active: bool);

    /// Called after the agent container launches. The sender is the broadcast
    /// channel from the container's stuck detector; the TUI stores it so the
    /// tab can subscribe for stuck-colouring. CLI and API frontends ignore it,
    /// so this one keeps its no-op default.
    fn set_stuck_sender(
        &mut self,
        _sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
    }
}

/// An agent frontend that binds no I/O and reports nothing.
///
/// For frontends that satisfy [`AgentLaunchFrontend`] only to be dispatchable
/// and never actually launch a container — the `new`/`specs` test fakes, for
/// instance. Anything that really runs an agent must hand back a real sink.
pub struct NullAgentFrontend;

impl UserMessageSink for NullAgentFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for NullAgentFrontend {
    fn report_status(&mut self, _status: crate::engine::agent_runtime::frontend::AgentStatus) {}
    fn report_progress(
        &mut self,
        _progress: crate::engine::agent_runtime::frontend::AgentProgress,
    ) {
    }
    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}
