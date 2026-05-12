//! `ContainerExecutionFactory` — wired by Layer 2 to bridge the workflow
//! engine and the container runtime without leaking option lists or frontend
//! types into engine internals.

use std::path::PathBuf;

use crate::data::session::{AgentName, Session, SessionId};
use crate::data::workflow_definition::WorkflowStep;
use crate::engine::container::instance::ContainerExecution;
use crate::engine::error::EngineError;

/// Resolved per-step runtime context (agent, model, working dir, session id).
#[derive(Debug, Clone)]
pub struct WorkflowRuntimeContext {
    pub step_agent: AgentName,
    pub step_model: Option<String>,
    pub git_root: PathBuf,
    pub session_id: SessionId,
}

/// Trait implemented by Layer 2: produce a fresh `ContainerExecution` for a
/// step, or inject a prompt into an already-running container.
pub trait ContainerExecutionFactory: Send + Sync {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<ContainerExecution, EngineError>;

    /// Inject an additional prompt into a running container rather than
    /// launching a new one. Returns `Ok(None)` when the runtime backend does
    /// not support prompt injection (engine then falls back to a fresh
    /// container).
    fn inject_prompt(
        &self,
        execution: &ContainerExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError>;
}
