//! `AgentImageFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Not to be confused with the cross-paradigm runtime frontend trait
//! `crate::engine::agent_runtime::frontend::AgentFrontend` (referenced here
//! by qualified path): this trait reports agent *image setup* progress and
//! hands the engine a runtime frontend, while the runtime frontend binds a
//! running agent's I/O.
//!
//! `ReadyFrontend` and `InitFrontend` extend this trait, so a frontend that
//! drives `awman ready` or `awman init` declares these two methods once.

use crate::data::message::UserMessageSink;
use crate::data::setup_step::SetupStep;
use crate::data::step_status::StepStatus;

/// Frontend trait the engine uses to report agent setup progress.
pub trait AgentImageFrontend: UserMessageSink + Send {
    /// Report a step's status.
    ///
    /// `step` was a free-form `&str` key until WI 0114 F-45; it is now the
    /// closed [`SetupStep`] set, whose `Display` reproduces those exact
    /// strings. A frontend that only wants the label writes
    /// `step.to_string()`.
    fn report_step_status(&mut self, step: &SetupStep, status: StepStatus);

    /// The engine is about to build/run a container. Returns the runtime
    /// frontend for streaming build output.
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>;
}
