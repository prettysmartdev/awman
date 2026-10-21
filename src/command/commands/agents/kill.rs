//! `awman agents kill` (`rm`) — stop and remove one running agent.
//!
//! Only agents in awman's own listing can be targeted, so this can never
//! stop a non-awman container. Stopped-agent cleanup stays `awman clean`'s
//! job.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;

use crate::command::commands::agents::resolve::{map_action_error, resolve_agent, ResolveScope};
use crate::command::commands::agents::snapshot::AgentSnapshot;
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::error::EngineError;

/// The answer to the kill confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillChoice {
    Kill,
    Keep,
}

#[derive(Debug, Clone, Default)]
pub struct AgentsKillFlags {
    /// Skip the confirmation prompt.
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsKillOutcome {
    pub id: String,
    pub short_id: String,
    pub name: String,
    /// False only when the agent stopped but its removal failed.
    pub removed: bool,
}

pub struct AgentsKillCommand {
    reference: String,
    flags: AgentsKillFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsKillCommand {
    pub fn new(
        reference: String,
        flags: AgentsKillFlags,
        runtime: Arc<dyn AgentRuntimeEngine>,
    ) -> Self {
        Self {
            reference,
            flags,
            runtime,
        }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ctx.args.require("id")?,
            AgentsKillFlags {
                yes: ctx.flags.bool("yes"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsKillFlags {
        &self.flags
    }

    /// Best-effort removal after a successful stop. Some backends may already
    /// remove resources on stop; unsupported removal is also accepted.
    fn remove(&self, frontend: &mut dyn AgentsCommandFrontend, snapshot: &AgentSnapshot) -> bool {
        match self.runtime.remove_agent(&snapshot.id) {
            Ok(()) | Err(EngineError::UnsupportedOnRuntime { .. }) => true,
            Err(e) => {
                let still_present = self
                    .runtime
                    .list_running_all()
                    .and_then(|mut running| {
                        running.extend(self.runtime.list_stopped()?);
                        Ok(running)
                    })
                    .map(|agents| agents.iter().any(|h| h.id == snapshot.id))
                    .unwrap_or(true);
                if still_present {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "agent {} stopped but could not be removed: {e}; `awman clean` can remove it",
                            snapshot.name
                        ),
                    });
                }
                !still_present
            }
        }
    }
}

#[async_trait]
impl Command for AgentsKillCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsKillOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let runtime = self.runtime.as_ref();
        let resolved = resolve_agent(runtime, &self.reference, ResolveScope::RunningOnly)?;
        let mut snapshot = AgentSnapshot::from_handle(&resolved.handle, resolved.state, Utc::now());
        snapshot.apply_tui_context(frontend.tui_context().as_ref());

        if !self.flags.yes {
            let prompt = crate::command::prompts::agents_kill_confirm(&snapshot);
            if frontend.confirm_kill(&prompt)? == KillChoice::Keep {
                return Err(CommandError::Aborted);
            }
        }

        runtime.stop(&resolved.handle).map_err(|e| match e {
            // Never stop the newer sandbox that reused the name.
            EngineError::BuiltinAgentReplaced { .. } => CommandError::AgentReplaced {
                reference: self.reference.clone(),
            },
            other => map_action_error(runtime, &resolved.handle, other.into()),
        })?;
        let removed = self.remove(frontend.as_mut(), &snapshot);

        let outcome = AgentsKillOutcome {
            id: snapshot.id,
            short_id: snapshot.short_id,
            name: snapshot.name,
            removed,
        };
        frontend.show_kill_result(&outcome);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{handle, FakeFrontend, FakeRuntime};
    use crate::command::commands::agents::{AgentsTuiContext, TuiTabSnapshot};

    fn runtime() -> Arc<FakeRuntime> {
        Arc::new(FakeRuntime::new().with_running(vec![
            handle("abc123def456000", "awman-one", 30),
            handle("fed123def456000", "awman-squad-triage-12ab34cd", 20),
        ]))
    }

    fn kill(reference: &str, yes: bool, runtime: Arc<FakeRuntime>) -> AgentsKillCommand {
        AgentsKillCommand::new(reference.into(), AgentsKillFlags { yes }, runtime)
    }

    #[tokio::test]
    async fn confirmation_no_stops_nothing() {
        let rt = runtime();
        let (mut frontend, log) = FakeFrontend::new();
        frontend.kill_answer = Some(KillChoice::Keep);
        let err = kill("awman-one", false, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Aborted));
        assert!(!rt.called("stop"), "calls: {:?}", rt.calls());
        assert!(!rt.called("remove"));
        assert!(log.lock().unwrap().kill_outcomes.is_empty());
    }

    #[tokio::test]
    async fn confirmation_yes_stops_then_removes() {
        let rt = runtime();
        *rt.remove_result.lock().unwrap() = Some(Ok(()));
        let (mut frontend, log) = FakeFrontend::new();
        frontend.kill_answer = Some(KillChoice::Kill);
        let outcome = kill("abc1", false, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        let calls = rt.calls();
        let stop = calls
            .iter()
            .position(|c| c == "stop abc123def456000")
            .unwrap();
        let remove = calls
            .iter()
            .position(|c| c == "remove abc123def456000")
            .unwrap();
        assert!(stop < remove);
        assert!(outcome.removed);
        assert_eq!(outcome.short_id, "abc123def456");
        assert_eq!(log.lock().unwrap().kill_outcomes.len(), 1);
    }

    #[tokio::test]
    async fn yes_flag_skips_the_prompt() {
        let rt = runtime();
        let (frontend, log) = FakeFrontend::new();
        kill("awman-one", true, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(log.lock().unwrap().kill_prompts.is_empty());
        assert!(rt.called("stop abc123def456000"));
        assert!(rt.called("remove abc123def456000"));
    }

    #[tokio::test]
    async fn unsupported_remove_is_ignored_and_counts_as_removed() {
        // The fake's default `remove_agent` answers UnsupportedOnRuntime.
        let rt = runtime();
        let (frontend, log) = FakeFrontend::new();
        let outcome = kill("awman-one", true, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(outcome.removed);
        assert!(log.lock().unwrap().messages.is_empty(), "no warning");
    }

    #[tokio::test]
    async fn failed_remove_of_still_present_agent_warns_and_reports_not_removed() {
        let rt = runtime();
        *rt.remove_result.lock().unwrap() = Some(Err(EngineError::Other("busy".into())));
        // Still listed as stopped after the failed removal.
        rt.stopped
            .lock()
            .unwrap()
            .push(handle("abc123def456000", "awman-one", 30));
        let (frontend, log) = FakeFrontend::new();
        let outcome = kill("awman-one", true, rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(!outcome.removed);
        let log = log.lock().unwrap();
        assert_eq!(log.messages.len(), 1);
        assert!(log.messages[0].text.contains("awman clean"));
    }

    #[tokio::test]
    async fn failed_remove_of_already_gone_agent_counts_as_removed() {
        let rt = runtime();
        *rt.remove_result.lock().unwrap() = Some(Err(EngineError::Other("no such".into())));
        let (frontend, log) = FakeFrontend::new();
        let outcome = kill("awman-one", true, rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(outcome.removed);
        assert!(log.lock().unwrap().messages.is_empty());
    }

    #[tokio::test]
    async fn non_interactive_without_yes_is_exit_2_and_stops_nothing() {
        let rt = runtime();
        let (frontend, _log) = FakeFrontend::new(); // default confirm_kill refuses
        let err = kill("awman-one", false, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(!rt.called("stop"));
    }

    #[tokio::test]
    async fn replaced_builtin_sandbox_gives_generation_mismatch_error() {
        let rt = runtime();
        *rt.stop_error.lock().unwrap() = Some(EngineError::BuiltinAgentReplaced {
            name: "awman-one".into(),
        });
        let (frontend, _log) = FakeFrontend::new();
        let err = kill("awman-one", true, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::AgentReplaced { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 4);
        assert!(!rt.called("remove"), "never remove the newer sandbox");
    }

    #[tokio::test]
    async fn stop_failure_for_vanished_agent_says_no_longer_running() {
        let rt = runtime();
        *rt.stop_error.lock().unwrap() = Some(EngineError::Other("gone".into()));
        rt.vanish_on_stop_error
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (frontend, _log) = FakeFrontend::new();
        let err = kill("awman-one", true, rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(
            matches!(err, CommandError::AgentNoLongerRunning { .. }),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 4);
    }

    #[tokio::test]
    async fn stop_failure_for_still_running_agent_keeps_the_engine_error() {
        let rt = runtime();
        *rt.stop_error.lock().unwrap() = Some(EngineError::Other("boom".into()));
        let (frontend, _log) = FakeFrontend::new();
        let err = kill("awman-one", true, rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Engine(_)), "{err:?}");
    }

    #[tokio::test]
    async fn stopped_only_target_points_at_clean() {
        let rt =
            Arc::new(FakeRuntime::new().with_stopped(vec![handle("stop1", "awman-stopped", 50)]));
        let (frontend, _log) = FakeFrontend::new();
        let err = kill("awman-stopped", true, rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("awman clean"), "{err}");
    }

    #[tokio::test]
    async fn prompt_mentions_squad_task_and_tui_tab() {
        let rt = runtime();
        let (mut frontend, log) = FakeFrontend::new();
        frontend.kill_answer = Some(KillChoice::Keep);
        frontend.tui = Some(AgentsTuiContext::new(vec![TuiTabSnapshot {
            tab_number: 4,
            container_name: Some("awman-one".into()),
            is_stuck: false,
            command_label: "chat".into(),
        }]));
        let _ = kill("awman-one", false, rt.clone())
            .run_with_frontend(frontend.boxed())
            .await;
        let body = log.lock().unwrap().kill_prompts[0].body.clone();
        assert!(body.contains("TUI tab 4"), "{body}");

        let (mut frontend, log) = FakeFrontend::new();
        frontend.kill_answer = Some(KillChoice::Keep);
        let _ = kill("awman-squad-triage-12ab34cd", false, rt)
            .run_with_frontend(frontend.boxed())
            .await;
        let prompt = log.lock().unwrap().kill_prompts[0].clone();
        assert!(
            prompt.body.contains("squad task `triage`"),
            "{}",
            prompt.body
        );
        assert_eq!(prompt.default_on_dismiss, Some(KillChoice::Keep));
    }
}
