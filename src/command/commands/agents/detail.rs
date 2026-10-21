//! `awman agents detail` (`inspect`) — one agent in depth.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;

use crate::command::commands::agents::resolve::{map_action_error, resolve_agent, ResolveScope};
use crate::command::commands::agents::snapshot::{AgentSnapshot, AgentState};
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::engine::agent_runtime::{AgentDetail, AgentRuntimeEngine};

#[derive(Debug, Clone, Default)]
pub struct AgentsDetailFlags {
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsDetailOutcome {
    pub snapshot: AgentSnapshot,
    pub detail: AgentDetail,
}

impl AgentsDetailOutcome {
    /// The human-readable key/value blocks, in display order: Identity,
    /// Lifecycle, Resources, Runtime. Every frontend renders from this.
    pub fn groups(&self) -> Vec<(&'static str, Vec<(String, String)>)> {
        let s = &self.snapshot;
        let pair = |k: &str, v: String| (k.to_string(), v);
        let mut runtime = Vec::new();
        if let Some(owner) = &self.detail.owner {
            let liveness = if owner.alive { "running" } else { "exited" };
            runtime.push(pair("owner pid", format!("{} ({liveness})", owner.pid)));
        }
        runtime.extend(
            self.detail
                .runtime_extras
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        vec![
            (
                "Identity",
                vec![
                    pair("id", s.id.clone()),
                    pair("name", s.name.clone()),
                    pair("image", s.image.clone()),
                    pair("source", s.source_cell()),
                ],
            ),
            (
                "Lifecycle",
                vec![
                    pair("state", s.state.label().to_string()),
                    pair("started", s.started_at.to_rfc3339()),
                    pair("uptime", s.uptime_cell()),
                ],
            ),
            (
                "Resources",
                vec![pair("cpu", s.cpu_cell()), pair("memory", s.mem_cell())],
            ),
            ("Runtime", runtime),
        ]
    }
}

pub struct AgentsDetailCommand {
    reference: String,
    flags: AgentsDetailFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsDetailCommand {
    pub fn new(
        reference: String,
        flags: AgentsDetailFlags,
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
            AgentsDetailFlags {
                json: ctx.flags.bool("json"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsDetailFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for AgentsDetailCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsDetailOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let runtime = self.runtime.as_ref();
        let resolved = resolve_agent(runtime, &self.reference, ResolveScope::IncludeStopped)?;
        let detail = runtime.inspect(&resolved.handle).map_err(|e| {
            let err = CommandError::from(e);
            if resolved.state == AgentState::Stopped {
                err
            } else {
                map_action_error(runtime, &resolved.handle, err)
            }
        })?;

        let state = if detail.running {
            AgentState::Running
        } else {
            AgentState::Stopped
        };
        let mut snapshot = AgentSnapshot::from_handle(&resolved.handle, state, Utc::now());
        snapshot.cpu_percent = detail.cpu_percent;
        snapshot.memory_mb = detail.memory_mb;
        snapshot.apply_tui_context(frontend.tui_context().as_ref());

        let outcome = AgentsDetailOutcome { snapshot, detail };
        frontend.show_detail(&outcome);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{
        handle, owner_detail, FakeFrontend, FakeRuntime,
    };

    fn detail(reference: &str, runtime: Arc<FakeRuntime>) -> AgentsDetailCommand {
        AgentsDetailCommand::new(reference.into(), AgentsDetailFlags::default(), runtime)
    }

    #[tokio::test]
    async fn inspected_lifecycle_replaces_the_preinspection_running_state() {
        let h = handle("a1", "awman-one", 5);
        let rt = FakeRuntime::new().with_running(vec![h.clone()]);
        let mut detail = owner_detail(&h, 99, true);
        detail.running = false;
        *rt.inspect_override.lock().unwrap() = Some(detail);
        let (frontend, _) = FakeFrontend::new();
        let outcome = super::tests::detail("a1", Arc::new(rt))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(outcome.snapshot.state, AgentState::Stopped);
        assert!(!outcome.detail.running);
        assert_eq!(outcome.groups()[1].1[0].1, "stopped");
    }

    #[tokio::test]
    async fn default_inspect_fills_common_fields() {
        let rt = Arc::new(FakeRuntime::new().with_running(vec![handle(
            "abc123def456000",
            "awman-one",
            30,
        )]));
        let (frontend, log) = FakeFrontend::new();
        let outcome = detail("abc1", rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        let d = &outcome.detail;
        assert_eq!(d.id, "abc123def456000");
        assert_eq!(d.name, "awman-one");
        assert_eq!(d.image, "awman-test:latest");
        assert!(d.running);
        assert_eq!(d.cpu_percent, Some(10.0));
        assert_eq!(d.memory_mb, Some(100.0));
        assert!(d.owner.is_none());
        assert!(d.runtime_extras.is_empty());
        assert_eq!(outcome.snapshot.cpu_percent, Some(10.0));
        assert_eq!(outcome.snapshot.state, AgentState::Running);
        assert_eq!(log.lock().unwrap().detail_outcomes, 1);
    }

    #[tokio::test]
    async fn runtime_without_stats_leaves_resources_unreported() {
        let mut rt = FakeRuntime::new().with_running(vec![handle("a1", "awman-one", 5)]);
        rt.caps.reports_resource_stats = false;
        let (frontend, _log) = FakeFrontend::new();
        let outcome = detail("a1", Arc::new(rt))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(outcome.detail.cpu_percent, None);
        let resources = &outcome.groups()[2];
        assert_eq!(resources.0, "Resources");
        assert_eq!(resources.1[0], ("cpu".to_string(), "-".to_string()));
        assert_eq!(resources.1[1], ("memory".to_string(), "-".to_string()));
    }

    #[tokio::test]
    async fn runtime_extras_pass_through_in_order_after_owner() {
        let h = handle("a1", "awman-one", 5);
        let rt = FakeRuntime::new().with_running(vec![h.clone()]);
        let mut d = owner_detail(&h, 99, true);
        d.runtime_extras.insert("vcpus".into(), "4".into());
        d.runtime_extras.insert("memory".into(), "2048 MB".into());
        d.runtime_extras.insert("network".into(), "open".into());
        *rt.inspect_override.lock().unwrap() = Some(d);
        let (frontend, _log) = FakeFrontend::new();
        let outcome = detail("a1", Arc::new(rt))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        let groups = outcome.groups();
        let names: Vec<&str> = groups.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["Identity", "Lifecycle", "Resources", "Runtime"]);
        let runtime: Vec<(&str, &str)> = groups[3]
            .1
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // BTreeMap order (memory, network, vcpus) after the owner row.
        assert_eq!(
            runtime,
            [
                ("owner pid", "99 (running)"),
                ("memory", "2048 MB"),
                ("network", "open"),
                ("vcpus", "4"),
            ]
        );
    }

    #[tokio::test]
    async fn exited_owner_is_labelled_exited() {
        let h = handle("a1", "awman-one", 5);
        let rt = FakeRuntime::new().with_running(vec![h.clone()]);
        *rt.inspect_override.lock().unwrap() = Some(owner_detail(&h, 7, false));
        let (frontend, _log) = FakeFrontend::new();
        let outcome = detail("a1", Arc::new(rt))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(outcome.groups()[3].1[0].1, "7 (exited)");
    }

    #[tokio::test]
    async fn stopped_agents_can_be_inspected() {
        let rt = FakeRuntime::new().with_stopped(vec![handle("s1", "awman-stopped", 50)]);
        let (frontend, _log) = FakeFrontend::new();
        let outcome = detail("awman-stopped", Arc::new(rt))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(outcome.snapshot.state, AgentState::Stopped);
        assert!(!outcome.detail.running);
        assert_eq!(outcome.groups()[1].1[0].1, "stopped");
    }

    #[tokio::test]
    async fn unknown_agent_is_exit_4() {
        let (frontend, _log) = FakeFrontend::new();
        let err = detail("nope", Arc::new(FakeRuntime::new()))
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }
}
