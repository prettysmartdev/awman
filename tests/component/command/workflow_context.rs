//! Context propagation through the production dynamic command and launch policy.
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::{
    commands::{
        exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFlags},
        Command,
    },
    dispatch::Engines,
};
use awman::data::session::Session;
use awman::data::{
    config::flags::PrerequisiteFlags,
    fs::{ApiPaths, AuthPathResolver},
    session::{AgentHandle, SessionOpenOptions, SessionType},
    WorkflowStateStore,
};
use awman::engine::error::EngineError;
use awman::engine::{
    agent::AgentEngine,
    agent_runtime::{
        execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ResolvedAgentOptions,
    },
    auth::AuthEngine,
    container::options::OverlayPermission,
    git::GitEngine,
    overlay::OverlayEngine,
};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::{
    fixtures::{ConfigTree, RepoBuilder},
    TestSandbox,
};
use serde_json::json;
use std::sync::Mutex;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

struct Runtime {
    launches: Mutex<Vec<PathBuf>>,
    container: Arc<awman::engine::container::ContainerRuntime>,
}
impl AgentRuntimeEngine for Runtime {
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn awman::data::message::UserMessageSink,
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
        _: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("refusal must precede image build")
    }
    fn runtime_name(&self) -> &'static str {
        "test"
    }
    fn display_name(&self) -> &'static str {
        "refusal sentinel"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: false,
            image_acquisition: ImageAcquisition::Build,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        let ResolvedAgentOptions::Container(options) = options else {
            panic!("container options expected")
        };
        let context = options
            .overlays
            .iter()
            .find(|o| o.container_path == Path::new("/awman/context/workflow"))
            .expect("workflow context mount");
        assert_eq!(context.permission, OverlayPermission::ReadWrite);
        let (prompt_file, _, _) = options
            .system_prompt_file
            .as_ref()
            .expect("workflow system instructions");
        let prompt = std::fs::read_to_string(prompt_file).unwrap();
        assert!(
            prompt.contains("read any files left by previous steps"),
            "{prompt}"
        );
        assert!(prompt.contains("Write your outputs, notes"), "{prompt}");
        assert!(!prompt.contains("one-shot"), "{prompt}");
        let mut launches = self.launches.lock().unwrap();
        if launches.is_empty() {
            assert!(prompt.contains("Dynamic workflow planning"), "{prompt}");
            assert!(prompt.contains("Work item: #7"), "{prompt}");
            std::fs::write(context.host_path.join("workflow.toml"), "title = 'Context regression'\nagent = 'claude'\n[[steps]]\nname = 'read-notes'\nprompt = 'Read leader notes'\n").unwrap();
            std::fs::write(context.host_path.join("leader-notes.md"), "shared plan").unwrap();
        } else {
            assert!(prompt.contains("read-notes (step 1 of 1)"), "{prompt}");
            assert_eq!(
                std::fs::read_to_string(context.host_path.join("leader-notes.md")).unwrap(),
                "shared plan"
            );
            std::fs::write(context.host_path.join("step-notes.md"), "shared result").unwrap();
        }
        launches.push(context.host_path.clone());
        self.container.build(*options)
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

fn engines(s: &TestSandbox, session: &Session) -> (Engines, Arc<Runtime>) {
    let env = session.env().clone();
    let container = awman::engine::container::ContainerRuntime::docker_handle(env.clone());
    let runtime = Arc::new(Runtime {
        launches: Mutex::new(Vec::new()),
        container: container.clone(),
    });
    let auth = AuthPathResolver::at_home(s.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    (
        Engines {
            runtime: runtime.clone(),
            container_runtime: Some(container),
            sandbox_runtime: None,
            git_engine: Arc::new(GitEngine::with_env(env.clone())),
            overlay_engine: overlay.clone(),
            auth_engine: Arc::new(AuthEngine::with_paths(
                auth,
                ApiPaths::from_env(&env).unwrap(),
            )),
            agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
            workflow_state_store: Arc::new(WorkflowStateStore::new(session)),
            credential_monitor: None,
            global_config: Arc::new(Default::default()),
            ephemeral_secrets: Arc::new(Default::default()),
        },
        runtime,
    )
}
fn workflow_flags(reference: &str, dynamic: bool) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: (!dynamic).then(|| "workflow.toml".into()),
        work_item: Some(reference.into()),
        non_interactive: true,
        plan: false,
        allow_docker: false,
        worktree: false,
        yolo: false,
        auto: true,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        max_concurrent: None,
        dynamic,
        leader: None,
        on_dirty_merge_target: None,
        on_merge_failure: None,
        on_existing_worktree: None,
        prerequisites: Box::new(PrerequisiteFlags {
            check_only: false,
            ..Default::default()
        }),
        squad_task_prerequisites: None,
    }
}

#[test]
fn dynamic_leader_and_generated_steps_share_context_and_instructions() {
    // Match the API command worker's stack for the full workflow future.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(dynamic_context_scenario());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn dynamic_context_scenario() {
    let mut s = crate::support::sandbox("command::workflow_context::dynamic_leader_and_generated_steps_share_context_and_instructions");
    let root = s.root().to_path_buf();
    for key in ["TMPDIR", "TEMP", "TMP"] {
        s.env_mut().set(key, &root);
    }
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    docker.on(&["image", "inspect"]).stdout("[]\n");
    docker.on(&["run"]).stdout("completed\n");
    docker.on(&["rm"]);
    docker.on(&["stop"]);
    let repo = RepoBuilder::new(&s, "project")
        .file(
            "aspec/work-items/0007-seven.md",
            "# Shared context\n\nPreserve notes.",
        )
        .file(".awman/Dockerfile.claude", "FROM scratch\n")
        .init();
    ConfigTree::new()
        .global(json!({"agents":{"auth":{"refresh":{"enabled":false}}}}))
        .repo(json!({"workItems":{"source":"local"}}))
        .write(&s, Some(repo.path()));
    let env = awman::data::config::env::EnvSnapshot::from_full_env(
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
    let mut session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    session.set_session_type(SessionType::Remote {
        repo_url: "https://example.invalid/project.git".into(),
        branch: "main".into(),
        cloned_path: repo.path().to_path_buf(),
    });
    let (engines, runtime) = engines(&s, &session);
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
    let outcome = ExecWorkflowCommand::new(workflow_flags("7", true), engines, session)
        .run_with_frontend(Box::new(frontend))
        .await;
    let mut output = String::new();
    while let Ok(event) = log.try_recv() {
        output.push_str(&serde_json::to_string(&event).unwrap());
    }
    assert!(!output.contains("workflow state will be empty"), "{output}");
    assert_eq!(outcome.unwrap().exit_code, Some(0), "{output}");
    let launches = runtime.launches.lock().unwrap();
    assert_eq!(launches.len(), 2, "{output}");
    assert_eq!(launches[0], launches[1]);
    assert_eq!(
        std::fs::read_to_string(launches[0].join("step-notes.md")).unwrap(),
        "shared result"
    );
    drop(launches);
    s.finish();
}
