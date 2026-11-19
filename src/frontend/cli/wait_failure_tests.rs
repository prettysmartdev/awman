//! Component proof of both command wait paths using an in-memory runtime.
use super::CommandOutcome;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::dispatch::{Dispatch, Engines};
use crate::data::{
    config::env::EnvSnapshot,
    session::{AgentHandle, Session, SessionOpenOptions},
};
use crate::engine::{
    agent::AgentEngine,
    agent_runtime::{
        execution::{CancelHandle, ExecutionBackend},
        frontend::AgentFrontend,
        AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentRuntimeEngine,
        AgentStats, Capabilities, ImageAcquisition, ReadyAgentOptions, ResolvedAgentOptions,
    },
    error::EngineError,
};
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::{
    fixtures::{ConfigTree, RepoBuilder},
    TestCase, TestSandbox,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn relayed_chat_and_exec_require_exit_codes() {
    for kind in ["Chat", "ExecPrompt"] {
        let value = json!({
            "kind": kind,
            "payload": {"automation": null, "agent": null, "exit_code": null}
        });
        assert!(
            crate::frontend::cli::per_command::relay::decode_outcome(value, false).is_err(),
            "{kind}"
        );
        let value = json!({"kind": kind, "payload": {"automation": null, "agent": null}});
        assert!(
            crate::frontend::cli::per_command::relay::decode_outcome(value, false).is_err(),
            "{kind}"
        );
    }
}

struct Runtime {
    exit: Option<i32>,
    observed: Arc<Mutex<Vec<Duration>>>,
}
static CAPS: Capabilities = Capabilities {
    arbitrary_host_mounts: true,
    kit_declarative: false,
    has_image_store: true,
    image_acquisition: ImageAcquisition::Build,
    per_launch_vm_settings: false,
    reports_resource_stats: false,
};
impl AgentRuntimeEngine for Runtime {
    fn runtime_name(&self) -> &'static str {
        "fake"
    }
    fn display_name(&self) -> &'static str {
        "fake runtime"
    }
    fn capabilities(&self) -> &Capabilities {
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/home/awman".into()))
    }
    fn build_image(
        &self,
        _: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("images are already available")
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(Instance {
            exit: self.exit,
            observed: self.observed.clone(),
        }))
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("no metrics requested")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no attach requested")
    }
}
struct Instance {
    exit: Option<i32>,
    observed: Arc<Mutex<Vec<Duration>>>,
}
impl AgentInstance for Instance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: "fake".into(),
            name: "fake".into(),
            image: "fake:latest".into(),
        }
    }
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        self.observed.lock().unwrap().push(frontend.stuck_timeout());
        let (sender, _) = tokio::sync::broadcast::channel(16);
        Ok(AgentExecution::new(
            AgentHandle {
                id: "fake".into(),
                name: "fake".into(),
                image_tag: "fake:latest".into(),
                started_at: chrono::Utc::now(),
            },
            Box::new(Wait(self.exit)),
            Arc::new(sender),
            None,
        ))
    }
}
struct Wait(Option<i32>);
impl ExecutionBackend for Wait {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        match self.0 {
            Some(code) => Ok(AgentExitInfo {
                exit_code: code,
                signal: None,
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
            }),
            None => Err(EngineError::Container("controlled wait failure".into())),
        }
    }
    fn cancel(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::new(|| Ok(()))
    }
}

#[tokio::test]
async fn chat_and_exec_wait_failures_and_known_exits_project_through_dispatch() {
    let s = TestSandbox::for_test(TestCase::component("frontend::cli::wait_failure_tests::chat_and_exec_wait_failures_and_known_exits_project_through_dispatch"));
    ConfigTree::new().global(json!({"runtime":{"kind":"docker"},"agents":{"stuckTimeout":"40s","auth":{"mode":"none","refresh":{"enabled":false}}}})).write(&s, None);
    let repo = RepoBuilder::new(&s, "wait-failures")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.claude", "FROM scratch\n")
        .init();
    let env = EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    for path in [&["chat"][..], &["exec", "prompt"][..]] {
        for exit in [None, Some(42), Some(0)] {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let runtime = Arc::new(Runtime {
                exit,
                observed: observed.clone(),
            });
            let mut session = Session::open_at_git_root(
                repo.path().into(),
                repo.path().into(),
                SessionOpenOptions {
                    env: Some(env.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            // Worker leases and pushed commands may carry their entire task
            // as a staged inline work item, with no prompt argv or flag.
            let inline_task = path.len() > 1 && exit == Some(0);
            if inline_task {
                let body = s.tmp().join("inline-task.md");
                std::fs::write(&body, "review the inline task").unwrap();
                session.set_inline_work_item(body);
            }
            let mut engines = Engines::for_env(env.clone()).unwrap();
            engines.runtime = runtime.clone();
            engines.container_runtime = None;
            engines.agent_engine =
                Arc::new(AgentEngine::new(engines.overlay_engine.clone(), runtime));
            let bus = EventBus::new(32);
            let mut args: Vec<String> = ["--agent", "claude", "--set", "agents.stuckTimeout=55s"]
                .into_iter()
                .map(str::to_string)
                .collect();
            if path.len() > 1 && !inline_task {
                args.push("review".into());
            }
            let result = Dispatch::new(
                ApiDispatchFrontend::new(&path.join(" "), &args, bus.sender()),
                Arc::new(tokio::sync::RwLock::new(session)),
                engines,
            )
            .run_command(path)
            .await;
            match exit {
                None => {
                    let error = result.unwrap_err();
                    assert_eq!(
                        error.code(),
                        crate::data::agent_failure::RUNTIME_WAIT_FAILED
                    );
                    assert_eq!(error.exit_code(), 1);
                }
                Some(code) => {
                    let outcome = result.unwrap();
                    assert!(matches!(
                        outcome,
                        CommandOutcome::Chat(_) | CommandOutcome::ExecPrompt(_)
                    ));
                    assert_eq!(outcome.exit_code(), code);
                }
            }
            assert_eq!(*observed.lock().unwrap(), [Duration::from_secs(55)]);
        }
    }
    s.finish();
}
