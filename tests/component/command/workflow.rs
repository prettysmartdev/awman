use super::support::AdmissionFixture;
use awman::command::commands::api_server::{CancelOutcome, CommandCanceller, TokenRegistry};
use awman::data::api_lifecycle_store::CommandLifecycleStore;
use awman::data::fs::api_db::IdempotencyBegin;
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_idempotent_submissions_have_exactly_one_owner() {
    let s = crate::support::sandbox(
        "command::workflow::concurrent_idempotent_submissions_have_exactly_one_owner",
    );
    let f = AdmissionFixture::new(&s);
    let barrier = Arc::new(Barrier::new(16));
    let outcomes = std::thread::scope(|scope| {
        let mut tasks = vec![];
        for _ in 0..16 {
            let barrier = barrier.clone();
            let store = f.store.clone();
            tasks.push(scope.spawn(move || {
                let journal =
                    awman::command::commands::api_server::admission::IdempotentAdmission::new(
                        store,
                        "run",
                        "same-principal",
                        "same-key",
                    );
                barrier.wait();
                let outcome = journal.begin("same-body").unwrap();
                barrier.wait();
                drop(journal);
                outcome
            }));
        }
        tasks
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == IdempotencyBegin::Fresh)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == IdempotencyBegin::InFlight)
            .count(),
        15
    );
}

#[test]
fn cancellation_is_idempotent_and_never_fires_another_commands_token() {
    let s = crate::support::sandbox(
        "command::workflow::cancellation_is_idempotent_and_never_fires_another_commands_token",
    );
    let f = AdmissionFixture::new(&s);
    let commands = Arc::new(TokenRegistry::new());
    let setups = Arc::new(TokenRegistry::new());
    let session = awman::data::ids::WorkerSessionId::new().to_string();
    let one_id = awman::data::ids::CommandId::new().to_string();
    let two_id = awman::data::ids::CommandId::new().to_string();
    f.store
        .insert_session(&session, s.work().to_str().unwrap(), "2026-01-01T00:00:00Z")
        .unwrap();
    let cancel = CommandCanceller::new(
        f.store.clone(),
        CommandLifecycleStore::open(f.store.clone()).unwrap(),
        commands.clone(),
        setups,
    );
    assert!(matches!(
        cancel.cancel("missing").unwrap(),
        CancelOutcome::NotFound
    ));
    f.store
        .insert_command(&one_id, &session, "exec workflow", "[]", "log")
        .unwrap();
    f.store
        .insert_command(&two_id, &session, "exec prompt", "[]", "log")
        .unwrap();
    f.store
        .update_command_started(&one_id, "2026-01-01T00:00:00Z")
        .unwrap();
    let one = commands.register(&one_id);
    let two = commands.register(&two_id);
    assert!(matches!(
        cancel.cancel(&one_id).unwrap(),
        CancelOutcome::Stopping { .. }
    ));
    assert!(one.is_cancelled());
    assert!(!two.is_cancelled());
    assert!(matches!(
        cancel.cancel(&two_id).unwrap(),
        CancelOutcome::Cancelled { .. }
    ));
    let terminal = f.store.get_command(&two_id).unwrap().unwrap();
    assert!(matches!(
        cancel.cancel(&two_id).unwrap(),
        CancelOutcome::AlreadyTerminal { .. }
    ));
    assert_eq!(f.store.get_command(&two_id).unwrap().unwrap(), terminal);
}

#[test]
fn crash_completion_is_finalized_before_cancellation_can_overwrite_it() {
    use awman::data::command_status::{CommandCompletion, CommandStatus};
    let s = crate::support::sandbox(
        "command::workflow::crash_completion_is_finalized_before_cancellation_can_overwrite_it",
    );
    let f = AdmissionFixture::new(&s);
    let session = awman::data::ids::WorkerSessionId::new().to_string();
    let id = awman::data::ids::CommandId::new().to_string();
    f.store
        .insert_session(&session, s.work().to_str().unwrap(), "2026-01-01T00:00:00Z")
        .unwrap();
    f.store
        .insert_command(&id, &session, "exec workflow", "[]", "log")
        .unwrap();
    f.store
        .update_command_started(&id, "2026-01-01T00:00:00Z")
        .unwrap();
    let row = f.store.get_command(&id).unwrap().unwrap();
    f.paths
        .write_command_completion(
            &session,
            &id,
            &CommandCompletion {
                attempt: row.attempt,
                status: CommandStatus::Done,
                exit_code: 0,
                result_json: "{\"exit_code\":0}".into(),
                finished_at: "2026-01-01T00:01:00Z".into(),
            },
        )
        .unwrap();
    let cancel = CommandCanceller::new(
        f.store.clone(),
        CommandLifecycleStore::open(f.store.clone()).unwrap(),
        Arc::new(TokenRegistry::new()),
        Arc::new(TokenRegistry::new()),
    )
    .with_completion_paths(f.paths.clone());
    assert!(matches!(
        cancel.cancel(&id).unwrap(),
        CancelOutcome::AlreadyTerminal(_)
    ));
    let row = f.store.get_command(&id).unwrap().unwrap();
    assert_eq!(row.status, CommandStatus::Done);
    assert_eq!(row.exit_code, Some(0));
    assert!(matches!(
        cancel.cancel(&id).unwrap(),
        CancelOutcome::AlreadyTerminal(_)
    ));
}

/// Guards: 8cf45de1 consumers subscribe before any producer can publish.
#[test]
fn concurrent_workflow_events_have_one_order_and_done_fences_late_output() {
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::data::execution_event::EventPayload;
    let s = crate::support::sandbox(
        "command::workflow::concurrent_workflow_events_have_one_order_and_done_fences_late_output",
    );
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let mut live = bus.subscribe();
    std::thread::scope(|scope| {
        for producer in 0..8 {
            let tx = bus.sender();
            scope.spawn(move || {
                for item in 0..32 {
                    tx.emit(EventPayload::StdoutLine(format!("{producer}:{item}")));
                }
            });
        }
    });
    bus.sender().emit(EventPayload::Done);
    bus.sender()
        .emit(EventPayload::StdoutLine("too late".into()));
    let mut sequence = 0;
    while let Ok(persisted) = log.try_recv() {
        let streamed = live.try_recv().unwrap();
        assert_eq!(persisted.sequence, sequence);
        assert_eq!(streamed.sequence, sequence);
        assert_eq!(
            serde_json::to_value(&persisted).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
        sequence += 1;
    }
    assert_eq!(sequence, 257);
    assert!(live.try_recv().is_err());
    s.finish();
}

#[cfg(test)]
mod work_item_execution {
    use crate::support;
    use awman::command::{
        commands::{
            api_server::event_bus::EventBus,
            exec_prompt::{ExecPromptCommand, ExecPromptCommandFlags},
            exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFlags},
            Command,
        },
        dispatch::Engines,
    };
    use awman::data::{
        config::flags::PrerequisiteFlags,
        fs::{ApiPaths, AuthPathResolver},
        session::{AgentHandle, Session, SessionOpenOptions},
        WorkflowStateStore,
    };
    use awman::engine::{
        agent::AgentEngine,
        agent_runtime::{
            execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities,
            ImageAcquisition, ResolvedAgentOptions,
        },
        auth::AuthEngine,
        error::EngineError,
        git::GitEngine,
        overlay::OverlayEngine,
    };
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use awman_test_support::{
        fixtures::{ConfigTree, RepoBuilder},
        TestSandbox,
    };
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    const BODY: &str = "Body data: --issue --work-item local:0 $(codex) {{work_item_title}}";

    #[derive(Default)]
    struct Runtime {
        prompts: Mutex<Vec<String>>,
        inputs: Mutex<Vec<(std::path::PathBuf, String)>>,
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
        fn build(
            &self,
            options: ResolvedAgentOptions,
        ) -> Result<Box<dyn AgentInstance>, EngineError> {
            let prompt = match options {
                ResolvedAgentOptions::Container(options) => {
                    for overlay in &options.overlays {
                        if overlay.container_path.starts_with("/awman/work-items") {
                            assert_eq!(
                                overlay.permission,
                                awman::engine::container::options::OverlayPermission::ReadOnly
                            );
                            self.inputs.lock().unwrap().push((
                                overlay.host_path.clone(),
                                std::fs::read_to_string(&overlay.host_path).unwrap(),
                            ));
                        }
                    }
                    options.seeded_prompt
                }
                ResolvedAgentOptions::Sandbox(options) => options.seeded_prompt,
            };
            self.prompts
                .lock()
                .unwrap()
                .push(prompt.unwrap_or_default());
            Err(EngineError::Other("captured by test runtime".into()))
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
        let runtime = Arc::new(Runtime::default());
        let auth = AuthPathResolver::at_home(s.home());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
        (
            Engines {
                runtime: runtime.clone(),
                container_runtime: None,
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
    fn session(s: &TestSandbox, source: &str, base: &str) -> Session {
        let repo = RepoBuilder::new(s, &source.replace(':', "-"))
            .file(
                "aspec/work-items/0007-seven.md",
                format!("# Local seven\n\n{BODY}"),
            )
            .file(
                "workflow.toml",
                "[[steps]]\nname = 'run'\nprompt = '{{work_item_content}}'\n",
            )
            .init();
        repo.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
        ConfigTree::new()
            .global(json!({
                "workItems": {"providers":{"tracker":{"baseUrl":base,"tokenEnv":"TRACKER_TOKEN"}}},
                "agents": {"auth":{"refresh":{"enabled":false}}}
            }))
            .repo(json!({"workItems":{"source":source}}))
            .write(s, Some(repo.path()));
        Session::open_at_git_root(
            repo.path().to_path_buf(),
            repo.path().to_path_buf(),
            SessionOpenOptions {
                env: Some(support::env_snapshot(s)),
                ..Default::default()
            },
        )
        .unwrap()
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
                check_only: true,
                ..Default::default()
            }),
            squad_task_prerequisites: None,
        }
    }
    fn prompt_flags(reference: &str) -> ExecPromptCommandFlags {
        ExecPromptCommandFlags {
            prompt: None,
            work_item: Some(reference.into()),
            non_interactive: true,
            plan: false,
            allow_docker: false,
            yolo: false,
            auto: false,
            agent: Some("claude".into()),
            model: None,
            launch_mode: None,
            overlay: vec![],
            agent_flags: None,
            session_worktree: None,
        }
    }
    async fn run_workflow(
        session: &Session,
        engines: &Engines,
        reference: &str,
        dynamic: bool,
    ) -> (
        Result<
            awman::command::commands::exec_workflow::ExecWorkflowOutcome,
            awman::command::error::CommandError,
        >,
        String,
    ) {
        let bus = EventBus::new(1024);
        let mut log = bus.subscribe_log();
        let frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
        let result = ExecWorkflowCommand::new(
            workflow_flags(reference, dynamic),
            engines.clone(),
            session.clone(),
        )
        .run_with_frontend(Box::new(frontend))
        .await;
        let mut output = String::new();
        while let Ok(event) = log.try_recv() {
            output.push_str(&serde_json::to_string(&event).unwrap());
        }
        (result, output)
    }
    async fn run_prompt(
        session: &Session,
        engines: &Engines,
        reference: &str,
    ) -> (
        Result<
            awman::command::commands::exec_prompt::ExecPromptOutcome,
            awman::command::error::CommandError,
        >,
        String,
    ) {
        let bus = EventBus::new(1024);
        let mut log = bus.subscribe_log();
        let frontend = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        let result =
            ExecPromptCommand::new(prompt_flags(reference), engines.clone(), session.clone())
                .run_with_frontend(Box::new(frontend))
                .await;
        let mut output = String::new();
        while let Ok(event) = log.try_recv() {
            output.push_str(&serde_json::to_string(&event).unwrap());
        }
        (result, output)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_read_commands_resolve_local_github_and_http_before_launch() {
        let mut s=support::sandbox("command::workflow::work_item_execution::all_read_commands_resolve_local_github_and_http_before_launch");
        s.env_mut().set("TRACKER_TOKEN", "test-token");
        let gh = s.install_fake(support::fake_cli_exe(), "gh");
        gh.on(&["issue","view"]).stdout(&json!({"number":7,"title":"GitHub seven","body":BODY,"url":"https://github.com/o/r/issues/7"}).to_string());
        let server = MockServer::builder()
            .listener(s.ports().tcp_listener())
            .start()
            .await;
        Mock::given(method("GET"))
            .and(path("/items/PROJ-7"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"PROJ-7","title":"HTTP seven","body":BODY})),
            )
            .mount(&server)
            .await;
        for (source, reference, title) in [
            ("local", "7", "Local seven"),
            ("github", "7", "GitHub seven"),
            ("http:tracker", "PROJ-7", "HTTP seven"),
        ] {
            let session = session(&s, source, &format!("{}/items", server.uri()));
            let (mut engines, runtime) = engines(&s, &session);
            for dynamic in [false, true] {
                let (outcome, output) = run_workflow(&session, &engines, reference, dynamic).await;
                assert!(outcome.is_ok(), "{outcome:?}\n{output}");
                assert!(
                    output.contains("Work item:") && output.contains(title),
                    "{output}"
                );
                assert_eq!(output.matches("Work item:").count(), 1, "{output}");
                assert!(runtime.prompts.lock().unwrap().is_empty());
            }
            let (outcome, output) = run_prompt(&session, &engines, reference).await;
            assert!(outcome.is_err());
            assert!(output.contains(title), "{output}");
            assert_eq!(output.matches("Work item:").count(), 1, "{output}");
            {
                let prompts = runtime.prompts.lock().unwrap();
                assert_eq!(prompts.len(), 1, "{outcome:?}\n{output}");
                assert!(prompts[0].contains(BODY), "{prompts:?}");
            }
            engines.agent_engine = Arc::new(
                AgentEngine::new(engines.overlay_engine.clone(), runtime.clone())
                    .with_lease_workspace_scope(session.git_root().to_path_buf()),
            );
            let mut flags = workflow_flags(reference, false);
            flags.prerequisites.check_only = false;
            let bus = EventBus::new(1024);
            let frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
            let outcome = ExecWorkflowCommand::new(flags, engines, session.clone())
                .run_with_frontend(Box::new(frontend))
                .await;
            let prompts = runtime.prompts.lock().unwrap();
            assert!(prompts.len() > 1, "{outcome:?}");
            assert!(prompts[1].contains(BODY), "{prompts:?}");
            let inputs = runtime.inputs.lock().unwrap();
            if source == "local" {
                assert!(inputs.is_empty());
            } else {
                assert!(!inputs.is_empty(), "{outcome:?}");
                for (host, content) in inputs.iter() {
                    assert!(content.contains(BODY));
                    assert!(
                        !host.exists(),
                        "private input must be removed after the run"
                    );
                }
            }
        }
        drop(server);
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_and_ambiguous_inputs_stop_every_read_command_before_launch() {
        let s=support::sandbox("command::workflow::work_item_execution::missing_and_ambiguous_inputs_stop_every_read_command_before_launch");
        let session = session(&s, "local", "https://tracker.example/items");
        let (engines, runtime) = engines(&s, &session);
        for dynamic in [false, true] {
            let (result, _) = run_workflow(&session, &engines, "8", dynamic).await;
            assert!(matches!(
                result,
                Err(awman::command::error::CommandError::WorkItem(_))
            ));
        }
        assert!(run_prompt(&session, &engines, "8").await.0.is_err());
        std::fs::write(
            session.git_root().join("aspec/work-items/0007-other.md"),
            "# Other",
        )
        .unwrap();
        for dynamic in [false, true] {
            let (result, _) = run_workflow(&session, &engines, "7", dynamic).await;
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("0007-other.md") && message.contains("0007-seven.md"),
                "{message}"
            );
        }
        let message = run_prompt(&session, &engines, "7")
            .await
            .0
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("0007-other.md") && message.contains("0007-seven.md"),
            "{message}"
        );
        assert!(runtime.prompts.lock().unwrap().is_empty());
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_failures_stop_every_read_command_before_launch() {
        let mut s=support::sandbox("command::workflow::work_item_execution::http_failures_stop_every_read_command_before_launch");
        s.env_mut().set("TRACKER_TOKEN", "test-token");
        let server = MockServer::builder()
            .listener(s.ports().tcp_listener())
            .start()
            .await;
        let session = session(&s, "http:tracker", &format!("{}/items", server.uri()));
        let (engines, runtime) = engines(&s, &session);
        for status in [401, 403, 404, 429] {
            Mock::given(method("GET"))
                .and(path(format!("/items/{status}")))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            for dynamic in [false, true] {
                let (result, _) =
                    run_workflow(&session, &engines, &status.to_string(), dynamic).await;
                assert!(
                    matches!(
                        result,
                        Err(awman::command::error::CommandError::WorkItem(_))
                    ),
                    "{result:?}"
                );
            }
            assert!(run_prompt(&session, &engines, &status.to_string())
                .await
                .0
                .is_err());
        }
        assert!(runtime.prompts.lock().unwrap().is_empty());
        drop(server);
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_timeout_stops_every_read_command_before_launch() {
        let mut s=support::sandbox("command::workflow::work_item_execution::http_timeout_stops_every_read_command_before_launch");
        s.env_mut().set("TRACKER_TOKEN", "test-token");
        let server = MockServer::builder()
            .listener(s.ports().tcp_listener())
            .start()
            .await;
        Mock::given(method("GET"))
            .and(path("/items/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"slow","title":"Too late","body":BODY}))
                    .set_delay(std::time::Duration::from_secs(16)),
            )
            .mount(&server)
            .await;
        let session = session(&s, "http:tracker", &format!("{}/items", server.uri()));
        let (engines, runtime) = engines(&s, &session);
        let (plain, dynamic, prompt) = tokio::join!(
            run_workflow(&session, &engines, "slow", false),
            run_workflow(&session, &engines, "slow", true),
            run_prompt(&session, &engines, "slow"),
        );
        assert!(plain.0.is_err() && dynamic.0.is_err() && prompt.0.is_err());
        assert!(plain.0.unwrap_err().to_string().contains("timed out"));
        assert!(runtime.prompts.lock().unwrap().is_empty());
        drop(server);
        s.finish();
    }
}
