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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{handle, FakeRuntime};

    fn runtime() -> FakeRuntime {
        FakeRuntime::new().with_running(vec![
            handle("abc123def456000", "awman-one", 30),
            handle("abd999000111222", "awman-two", 20),
            handle("fff000111222333", "abc", 10),
        ])
    }

    fn resolve(rt: &FakeRuntime, r: &str) -> Result<ResolvedAgent, CommandError> {
        resolve_agent(rt, r, ResolveScope::RunningOnly)
    }

    #[test]
    fn unavailable_runtime_is_not_reported_as_missing_agent() {
        let rt = runtime();
        *rt.available.lock().unwrap() = Err("daemon down".into());
        let err = resolve(&rt, "abc1").unwrap_err();
        assert!(matches!(err, CommandError::Engine(_)));
        assert!(!rt.called("list_running_all"));
    }

    #[test]
    fn resolves_exact_id() {
        let got = resolve(&runtime(), "abd999000111222").unwrap();
        assert_eq!(got.handle.name, "awman-two");
        assert_eq!(got.state, AgentState::Running);
    }

    #[test]
    fn resolves_exact_name() {
        let got = resolve(&runtime(), "awman-one").unwrap();
        assert_eq!(got.handle.id, "abc123def456000");
    }

    #[test]
    fn resolves_unique_prefix() {
        let got = resolve(&runtime(), "abc1").unwrap();
        assert_eq!(got.handle.name, "awman-one");
        // The 12-char short id `agents ls` prints is a prefix too.
        let got = resolve(&runtime(), "abd999000111").unwrap();
        assert_eq!(got.handle.name, "awman-two");
    }

    #[test]
    fn exact_name_beats_id_prefix() {
        // "abc" is the exact name of one agent and the id prefix of another.
        let got = resolve(&runtime(), "abc").unwrap();
        assert_eq!(got.handle.id, "fff000111222333");
    }

    #[test]
    fn exact_id_beats_prefix_of_longer_id() {
        let rt = FakeRuntime::new().with_running(vec![
            handle("abc", "awman-short", 10),
            handle("abcdef", "awman-long", 20),
        ]);
        assert_eq!(resolve(&rt, "abc").unwrap().handle.name, "awman-short");
    }

    #[test]
    fn ambiguous_prefix_is_exit_2_with_candidates() {
        let err = resolve(&runtime(), "ab").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        match err {
            CommandError::AgentAmbiguous {
                reference,
                candidates,
            } => {
                assert_eq!(reference, "ab");
                assert_eq!(candidates.len(), 2);
                assert!(candidates[0].contains("awman-one"));
                assert!(candidates[1].contains("awman-two"));
            }
            other => panic!("expected AgentAmbiguous, got {other:?}"),
        }
    }

    #[test]
    fn unknown_reference_is_exit_4() {
        let err = resolve(&runtime(), "nope").unwrap_err();
        assert!(matches!(err, CommandError::AgentNotFound { .. }));
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn empty_reference_is_exit_2() {
        for r in ["", "   "] {
            let err = resolve(&runtime(), r).unwrap_err();
            assert!(matches!(err, CommandError::AgentReferenceEmpty));
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[test]
    fn stopped_only_match_for_running_scope_points_at_clean() {
        let rt = FakeRuntime::new()
            .with_running(vec![handle("run1", "awman-run", 5)])
            .with_stopped(vec![handle("stop1", "awman-stopped", 50)]);
        let err = resolve(&rt, "awman-stopped").unwrap_err();
        assert!(matches!(err, CommandError::AgentNotRunning { .. }));
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("awman clean"), "{err}");
    }

    #[test]
    fn include_stopped_scope_finds_stopped_agents_with_state() {
        let rt = FakeRuntime::new()
            .with_running(vec![handle("run1", "awman-run", 5)])
            .with_stopped(vec![handle("stop1", "awman-stopped", 50)]);
        let got = resolve_agent(&rt, "stop", ResolveScope::IncludeStopped).unwrap();
        assert_eq!(got.state, AgentState::Stopped);
        let got = resolve_agent(&rt, "run", ResolveScope::IncludeStopped).unwrap();
        assert_eq!(got.state, AgentState::Running);
    }

    #[test]
    fn match_reference_is_pure_and_returns_none_for_no_match() {
        let hs = vec![handle("aaa", "one", 1)];
        assert!(match_reference(&hs, "zzz").unwrap().is_none());
        assert_eq!(match_reference(&hs, "aa").unwrap().unwrap().id, "aaa");
    }

    #[test]
    fn map_action_error_reports_agent_that_left_the_running_list() {
        let rt = FakeRuntime::new();
        let gone = handle("gone", "awman-gone", 1);
        let err = map_action_error(&rt, &gone, CommandError::Aborted);
        assert!(matches!(err, CommandError::AgentNoLongerRunning { .. }));
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn map_action_error_keeps_original_error_when_agent_still_runs() {
        let h = handle("here", "awman-here", 1);
        let rt = FakeRuntime::new().with_running(vec![h.clone()]);
        let err = map_action_error(&rt, &h, CommandError::Aborted);
        assert!(matches!(err, CommandError::Aborted));
    }
}

// Kept beside the listing consumer so these failure tests do not change the
// shared agents fake used by unrelated workflow steps.
#[cfg(test)]
pub(crate) mod listing_failure_tests {
    use super::*;
    use crate::command::commands::agents::test_support::{FakeFrontend, FakeRuntime};
    use crate::data::session::Session;
    use crate::engine::agent_runtime::*;
    use crate::engine::error::EngineError;

    pub(crate) struct ListingFailureRuntime {
        pub inner: FakeRuntime,
        pub fail_running: bool,
    }
    impl ListingFailureRuntime {
        pub fn new(fail_running: bool) -> Self {
            Self {
                inner: FakeRuntime::new(),
                fail_running,
            }
        }
    }
    impl AgentRuntimeEngine for ListingFailureRuntime {
        fn runtime_name(&self) -> &'static str {
            "listing-fixture"
        }
        fn display_name(&self) -> &'static str {
            "listing fixture"
        }
        fn capabilities(&self) -> &Capabilities {
            self.inner.capabilities()
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            self.list_running_all()
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            if self.fail_running {
                Err(EngineError::Container("running listing failed".into()))
            } else {
                self.inner.list_running_all()
            }
        }
        fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
            Err(EngineError::Container("stopped listing failed".into()))
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            self.list_running_all()
        }
        fn stats(&self, h: &AgentHandle) -> Result<AgentStats, EngineError> {
            self.inner.stats(h)
        }
        fn stop(&self, h: &AgentHandle) -> Result<(), EngineError> {
            self.inner.stop(h)
        }
        fn attach(&self, h: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            self.inner.attach(h)
        }
        fn ready_agent(
            &self,
            _: &str,
            _: ReadyAgentOptions,
            _: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            unreachable!()
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            unreachable!()
        }
        fn build_image(
            &self,
            _: &crate::engine::container::BuildRequest,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            unreachable!()
        }
    }

    #[test]
    fn stopped_listing_failure_is_preserved_for_both_resolution_scopes() {
        for scope in [ResolveScope::RunningOnly, ResolveScope::IncludeStopped] {
            let runtime = ListingFailureRuntime::new(false);
            let err = resolve_agent(&runtime, "missing", scope).unwrap_err();
            assert!(err.to_string().contains("stopped listing failed"));
            assert!(runtime.inner.called("list_running_all"));
        }
    }

    #[tokio::test]
    async fn listing_errors_reach_status_and_kill_without_mutating_agents() {
        use crate::command::commands::agents::{
            AgentsKillCommand, AgentsKillFlags, AgentsStatusCommand, AgentsStatusFlags,
        };
        use crate::command::commands::Command;
        let runtime = std::sync::Arc::new(ListingFailureRuntime::new(false));
        let (frontend, _) = FakeFrontend::new();
        let err = AgentsKillCommand::new(
            "missing".into(),
            AgentsKillFlags { yes: true },
            runtime.clone(),
        )
        .run_with_frontend(frontend.boxed())
        .await
        .unwrap_err();
        assert!(err.to_string().contains("stopped listing failed"));
        assert!(!runtime.inner.called("stop "));
        let (frontend, _) = FakeFrontend::new();
        let runtime = std::sync::Arc::new(ListingFailureRuntime::new(true));
        let err = AgentsStatusCommand::new(AgentsStatusFlags::default(), runtime)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("running listing failed"));
    }
}
