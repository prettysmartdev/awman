//! `AgentFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::engine::container::frontend::ContainerFrontend;
use crate::engine::message::UserMessageSink;
use crate::engine::step_status::StepStatus;

/// Frontend trait the engine uses to report agent setup progress.
pub trait AgentFrontend: UserMessageSink + Send {
    /// Report a named step's status (e.g. "Downloading Dockerfile",
    /// "Building image").
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to build/run a container. Returns the container
    /// frontend for streaming build output.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}
