//! Agent reference resolution shared by `detail`, `kill`, `attach` and
//! `squad attach`.
//!
//! A reference matches in docker's order: exact id, then exact name, then a
//! unique id prefix (which covers the `short_id` that `agents ls` prints).
//! Exact matches always beat prefixes.

use crate::command::commands::agents::snapshot::{short_id, AgentState};
use crate::command::error::CommandError;
use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::AgentRuntimeEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveScope {
    /// Only running agents may match (`kill`, `attach`).
    RunningOnly,
    /// Stopped agents still present in the runtime may match too (`detail`).
    IncludeStopped,
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub handle: AgentHandle,
    pub state: AgentState,
}

/// Resolve `reference` against the runtime's own agent listing.
///
/// Errors: an empty reference is a usage error (exit 2); more than one prefix
/// match is ambiguous (exit 2, candidates listed); no match is not found
/// (exit 4). With `RunningOnly`, a reference that matches only a stopped
/// agent says so and points at `awman clean` instead.
pub fn resolve_agent(
    runtime: &dyn AgentRuntimeEngine,
    reference: &str,
    scope: ResolveScope,
) -> Result<ResolvedAgent, CommandError> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(CommandError::AgentReferenceEmpty);
    }

    runtime.availability()?;
    let running = runtime.list_running_all()?;
    match scope {
        ResolveScope::RunningOnly => {
            if let Some(handle) = match_reference(&running, reference)? {
                return Ok(ResolvedAgent {
                    handle: handle.clone(),
                    state: AgentState::Running,
                });
            }
            // A failed stopped listing is still a runtime failure. Treating
            // it as an empty list would turn an unavailable daemon into a
            // misleading "not found" response.
            let stopped = runtime.list_stopped()?;
            if !matches!(match_reference(&stopped, reference), Ok(None)) {
                return Err(CommandError::AgentNotRunning {
                    reference: reference.to_string(),
                });
            }
        }
        ResolveScope::IncludeStopped => {
            let running_count = running.len();
            let mut all = running;
            all.extend(runtime.list_stopped()?);
            if let Some(handle) = match_reference(&all, reference)? {
                let index = all
                    .iter()
                    .position(|h| std::ptr::eq(h, handle))
                    .unwrap_or_default();
                let state = if index < running_count {
                    AgentState::Running
                } else {
                    AgentState::Stopped
                };
                return Ok(ResolvedAgent {
                    handle: handle.clone(),
                    state,
                });
            }
        }
    }
    Err(CommandError::AgentNotFound {
        reference: reference.to_string(),
    })
}

/// Pure matching over a candidate list: exact id, else exact name, else a
/// unique id prefix. `Ok(None)` for no match; `Err(AgentAmbiguous)` when
/// several ids share the prefix.
pub fn match_reference<'a>(
    candidates: &'a [AgentHandle],
    reference: &str,
) -> Result<Option<&'a AgentHandle>, CommandError> {
    if let Some(handle) = candidates.iter().find(|h| h.id == reference) {
        return Ok(Some(handle));
    }
    if let Some(handle) = candidates.iter().find(|h| h.name == reference) {
        return Ok(Some(handle));
    }
    let prefixed: Vec<&AgentHandle> = candidates
        .iter()
        .filter(|h| h.id.starts_with(reference))
        .collect();
    match prefixed.as_slice() {
        [] => Ok(None),
        [handle] => Ok(Some(handle)),
        many => Err(CommandError::AgentAmbiguous {
            reference: reference.to_string(),
            candidates: many
                .iter()
                .map(|h| format!("{}  {}", short_id(&h.id), h.name))
                .collect(),
        }),
    }
}

/// Map a failed action (`stop`, `attach`, `inspect`) on a resolved agent. If
/// the agent has since left the running list, say it is no longer running
/// (exit 4); otherwise keep the original error.
pub fn map_action_error(
    runtime: &dyn AgentRuntimeEngine,
    handle: &AgentHandle,
    err: CommandError,
) -> CommandError {
    match runtime.list_running_all() {
        Ok(running) if !running.iter().any(|h| h.id == handle.id) => {
            CommandError::AgentNoLongerRunning {
                reference: handle.name.clone(),
            }
        }
        _ => err,
    }
}

// Kept beside the listing consumer so these failure tests do not change the
// shared agents fake used by unrelated workflow steps.
