//! Sandbox admission must refuse before any runtime method is reached.
use crate::support;
use awman::{
    command::{
        commands::squad::{
            gateway::{CreateTask, LocalTaskGateway, TaskGateway},
            runtime_guard::require_container_tier,
        },
        dispatch::Engines,
    },
    data::{
        fs::{ApiPaths, AuthPathResolver, MountScope, SquadPaths, TaskStore, TaskWorkspace},
        session::{AgentHandle, Session},
        WorkflowStateStore,
    },
    engine::{
        agent::AgentEngine,
        agent_runtime::{
            execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities,
            ImageAcquisition, ResolvedAgentOptions,
        },
        auth::AuthEngine,
        error::EngineError,
        git::GitEngine,
        overlay::OverlayEngine,
        squad::{
            env_state::{DaemonEnvState, Salt},
            SchedulerStatus,
        },
    },
};
use std::sync::{Arc, Mutex};

struct ForbiddenRuntime;
impl AgentRuntimeEngine for ForbiddenRuntime {
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn awman::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("refusal must precede agent setup")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        panic!("refusal must precede image lookup")
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        panic!("refusal must precede image inspection")
    }
    fn build_image(
        &self,
        _: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("refusal must precede image build")
    }
    fn runtime_name(&self) -> &'static str {
        "docker-sbx-experimental"
    }
    fn display_name(&self) -> &'static str {
        "refusal sentinel"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: false,
            kit_declarative: true,
            has_image_store: false,
            image_acquisition: ImageAcquisition::Kit,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        panic!("refusal must precede runtime probing")
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("refusal must precede runtime launch")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("runtime must not be reached")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("runtime must not be reached")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("runtime must not be reached")
    }
}

#[tokio::test]
async fn sandbox_refusal_precedes_task_allocation_and_every_runtime_call() {
    let sandbox = support::sandbox(
        "squad::refusal::sandbox_refusal_precedes_task_allocation_and_every_runtime_call",
    );
    let env = support::env_snapshot(&sandbox);
    let runtime = Arc::new(ForbiddenRuntime);
    let auth_paths = AuthPathResolver::at_home(sandbox.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth_paths,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(sandbox.work())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    let error = require_container_tier(&engines).unwrap_err().to_string();
    assert_eq!(error, "squad requires a container runtime. The configured runtime \"docker-sbx-experimental\" cannot mount squad's task directories or run workflow setup/teardown steps. Set runtime to \"docker\" or \"apple-containers\" to use squad.");
    let store = Arc::new(TaskStore::open(std::path::Path::new(":memory:")).unwrap());
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    let existing = awman::data::fs::task_store::Task {
        id: awman::data::ids::TaskId::new(),
        name: "existing-task".into(),
        description: "created before the runtime switch".into(),
        repo_scope: sandbox.work(),
        mount_scope: MountScope::Directory,
        overlays: vec![],
        interval_secs: 60,
        status: awman::data::fs::task_store::TaskStatus::Paused,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: vec![],
        prerequisites: None,
    };
    store.create(&existing).unwrap();
    let before = store.list().unwrap();

    let state = Arc::new(DaemonEnvState::with_payload(
        Box::new(awman::data::fs::daemon_env::NoStore),
        awman::data::fs::daemon_env::EnvPersistence::None,
        Salt::random(),
    ));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        engines,
        Arc::new(Mutex::new(SchedulerStatus::default())),
        SquadPaths::from_root(sandbox.awman_home()),
        state,
    )
    .with_env(env);
    let refused = gateway
        .create(CreateTask {
            name: "refused".into(),
            description: "untrusted task".into(),
            workspace: TaskWorkspace::Default,
            mount_scope: MountScope::Directory,
            interval_secs: 60,
            agent: None,
            model: None,
            overlays: vec![],
            agents_to_models: Default::default(),
            prerequisites: None,
        })
        .await
        .unwrap_err();
    assert_eq!(refused.to_string(), error);
    assert_eq!(
        store.list().unwrap(),
        before,
        "runtime refusal must preserve every field of existing tasks"
    );
    assert!(!sandbox.awman_home().join("squad/tasks/refused").exists());
    drop(gateway);
    drop(store);
    sandbox.finish();
}
