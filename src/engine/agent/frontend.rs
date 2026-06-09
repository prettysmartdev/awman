//! `AgentFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Not to be confused with the cross-paradigm runtime frontend trait
//! `crate::engine::agent_runtime::frontend::AgentFrontend` (referenced here
//! by qualified path): this trait reports agent *setup* progress, while the
//! runtime frontend binds a running agent's I/O.

use crate::data::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

/// Frontend trait the engine uses to report agent setup progress.
pub trait AgentFrontend: UserMessageSink + Send {
    /// Report a named step's status (e.g. "Downloading Dockerfile",
    /// "Building image").
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to build/run a container. Returns the runtime
    /// frontend for streaming build output.
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>;
}
