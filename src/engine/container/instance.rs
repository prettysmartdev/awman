//! Container-paradigm instance helpers.
//!
//! The cross-paradigm `AgentInstance` trait and `AgentExecution` type live
//! in `src/engine/agent_runtime/execution.rs`; this module keeps the
//! container-specific identity pieces the Docker/Apple backends build
//! handles from.

use std::time::SystemTime;

use chrono::Utc;

use crate::data::session::AgentHandle;
use crate::engine::container::options::{ContainerName, ImageRef};

/// Identity-only handle to a container ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerId(pub String);

impl ContainerId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Helper: build an `AgentHandle` from the assembled container facts.
pub(crate) fn handle_now(id: &ContainerId, name: &ContainerName, image: &ImageRef) -> AgentHandle {
    AgentHandle {
        id: id.0.clone(),
        image_tag: image.0.clone(),
        name: name.0.clone(),
        started_at: chrono::DateTime::<Utc>::from(SystemTime::now()),
    }
}
