use super::*;
#[test]
fn interaction_override_is_rejected_before_allocation() {
    let mut settings =
        crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
    settings.pull_interaction = crate::data::config::daemon::Interaction::Headless;
    settings.pull_allow_override = false;
    let server = crate::data::config::work_servers::WorkServerConfig::default().resolved();
    let runtime = crate::engine::container::ContainerRuntime::docker();
    let support = LeaseRuntimeSupport::from_capabilities(runtime.capabilities());
    let mut item = awman_wsp::conformance::work_item("override");
    item.exec.interaction = Some(awman_wsp::Interaction::Interactive);
    assert!(policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker"
    )
    .unwrap_err()
    .contains("override"));
}

#[test]
fn unsupported_or_expired_initial_credentials_are_rejected_before_clone() {
    let settings =
        crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
    let server = crate::data::config::work_servers::WorkServerConfig::default().resolved();
    let runtime = crate::engine::container::ContainerRuntime::docker();
    let support = LeaseRuntimeSupport::from_capabilities(runtime.capabilities());
    let mut item = awman_wsp::conformance::work_item("credentials");
    item.repo.credentials = Some(awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Bearer,
        token: Some("secret".into()),
        username: None,
        expires_at: None,
    });
    item.repo.url = "ssh://git@git.example/r".into();
    assert!(policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker"
    )
    .unwrap_err()
    .contains("HTTP"));
    item.repo.url = "https://git.example/r".into();
    item.repo.credentials.as_mut().unwrap().expires_at =
        Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    assert!(policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker"
    )
    .unwrap_err()
    .contains("expired"));
}

struct NoRunFactory;
impl ApiCommandFrontendFactory for NoRunFactory {
    type Frontend = crate::command::instance::frontend::InstanceCommandFrontend;
    fn frontend_for(
        &self,
        _: &crate::data::fs::api_db::CommandRecord,
        _: crate::command::commands::api_server::event_bus::EventBusSender,
        _: crate::data::fs::api_command_log::CommandStateWriter,
        _: crate::command::interaction::InteractionDecision,
    ) -> Self::Frontend {
        panic!("this controller test must not execute a command")
    }
}
async fn controller(url: &str) -> (tempfile::TempDir, Arc<PullController<NoRunFactory>>) {
    use crate::command::{
        commands::api_server::{runtime::WorkerServices, WorkerConfig},
        daemon_api::worker::WorkerApi,
        dispatch::Engines,
    };
    use crate::data::config::{ConfigFile, ConfigLayers, EffectiveConfig, FileKind};
    let home = tempfile::tempdir().unwrap();
    let core = crate::engine::daemon::core::test_core(home.path());
    core.executor()
        .set_share(crate::data::daemon_state::FeatureName::Worker, 2);
    let config = EffectiveConfig::resolve(ConfigLayers { global: ConfigFile::from_value(serde_json::json!({
        "daemon":{"maxConcurrentJobs":2,"features":{"worker":{"enabled":true,"maxConcurrentJobs":2}}},
        "workServers":{"team":{"url":url,"claim":"auto"},"other":{"url":url,"claim":"manual"}}
    }), FileKind::Global).unwrap(), ..Default::default() }, Default::default(), core.env().clone()).unwrap();
    let engines = Engines::from_core(&core, Arc::new(config.config().clone()));
    let services = WorkerServices::from_core(
        WorkerConfig {
            port: 9876,
            bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
            workdirs: vec![],
            workdir_roots: vec![],
            env: core.env().clone(),
        },
        engines.clone(),
        &core,
    )
    .unwrap();
    let api = Arc::new(WorkerApi {
        live_config: RwLock::new(Some(config)),
        settings_tx: None,
        store: services.store.clone(),
        lifecycle_store: services.lifecycle_store.clone(),
        canceller: services.canceller(),
        setup_cancels: services.setup_cancels.clone(),
        paths: services.paths.clone(),
        workdirs: vec![],
        workdir_roots: vec![],
        started_at: core.started_at(),
        task_handles: Default::default(),
        engines,
        sessions: services.sessions.clone(),
        event_buses: services.event_buses.clone(),
        setup_buses: Default::default(),
    });
    for name in ["team", "other"] {
        LinkStore::new(core.data_paths())
            .write(
                name,
                &awman_wsp::LinkToken {
                    refresh_token: "refresh".into(),
                },
            )
            .unwrap();
    }
    let shared = Arc::new(WorkerShared {
        api,
        command_cancels: services.command_cancels.clone(),
    });
    let controller = PullController::new(shared, Arc::new(NoRunFactory), &core).unwrap();
    controller.accepting.store(true, Ordering::SeqCst);
    (home, controller)
}
async fn protocol_mock() -> wiremock::MockServer {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/token")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"access","expires_at":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339()}))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/workers/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"worker_id":"worker-test","heartbeat_ms":15000,"server_labels":{}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/queue"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"items":[],"next_cursor":null})),
        )
        .mount(&server)
        .await;
    server
}
#[tokio::test]
async fn unchanged_labels_register_once_per_server_across_ten_ticks() {
    let server = protocol_mock().await;
    let (_home, controller) = controller(&server.uri()).await;
    for _ in 0..10 {
        controller.refresh_servers().await.unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/v1/workers/register")
            .count(),
        2,
        "one registration per server"
    );
}

#[tokio::test]
async fn status_capabilities_include_an_enabled_servers_interaction_override() {
    use crate::data::config::{ConfigFile, ConfigLayers, EffectiveConfig, FileKind};
    let server = protocol_mock().await;
    let (_home, controller) = controller(&server.uri()).await;
    let config = EffectiveConfig::resolve(ConfigLayers {
        global: ConfigFile::from_value(serde_json::json!({
            "daemon":{"features":{"worker":{"pull":{"enabled":true,"interaction":"headless","allowOverride":false}}}},
            "workServers":{"team":{"url":server.uri(),"interaction":"interactive"}}
        }), FileKind::Global).unwrap(), ..Default::default()
    }, Default::default(), controller.shared.sessions.env().clone()).unwrap();
    *controller.shared.api.live_config.write().unwrap() = Some(config);
    controller.refresh_servers().await.unwrap();
    let capabilities = PullHandleAdapter(controller.clone()).capabilities();
    assert!(capabilities.contains(&"interactive"));
    assert!(capabilities.contains(&"terminal"));
    let requests = server.received_requests().await.unwrap();
    let request: serde_json::Value = serde_json::from_slice(
        &requests
            .iter()
            .find(|r| r.url.path() == "/v1/workers/register")
            .unwrap()
            .body,
    )
    .unwrap();
    for feature in request["features"].as_array().unwrap() {
        assert!(
            capabilities.contains(&feature.as_str().unwrap()),
            "{feature}"
        );
    }
    controller.accepting.store(false, Ordering::SeqCst);
    assert!(PullHandleAdapter(controller).capabilities().is_empty());
}
#[tokio::test]
async fn automatic_claim_long_polls_and_taken_immediately_reclaims() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = protocol_mock().await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(move |request: &wiremock::Request| {
            assert!(request
                .url
                .query_pairs()
                .any(|(k, v)| k == "wait_ms" && v == "30000"));
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error":"taken","code":"taken"}))
            } else {
                ResponseTemplate::new(204)
            }
        })
        .mount(&server)
        .await;
    let (_home, controller) = controller(&server.uri()).await;
    controller.refresh_servers().await.unwrap();
    tokio::time::timeout(
        Duration::from_millis(200),
        controller.claim_next(None, "team"),
    )
    .await
    .expect("no polling-tick delay after taken")
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn a_server_gate_does_not_block_another_servers_manual_claim() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = protocol_mock().await;
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let (_home, controller) = controller(&server.uri()).await;
    controller.refresh_servers().await.unwrap();
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    controller
        .claim_gates
        .lock()
        .unwrap()
        .insert("team".into(), gate.clone());
    let held = gate.lock().await;
    let pending = tokio::spawn({
        let controller = controller.clone();
        async move { controller.claim_next(Some("blocked"), "team").await }
    });
    tokio::task::yield_now().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        controller.claim_next(Some("manual"), "other"),
    )
    .await
    .expect("server B is independent")
    .unwrap();
    drop(held);
    pending.await.unwrap().unwrap();
}

#[test]
fn interactive_non_yolo_workflows_are_admitted_and_prompt_bodies_are_not_arguments() {
    use crate::data::config::daemon::Interaction;
    let mut settings =
        crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
    let server = crate::data::config::work_servers::WorkServerConfig::default().resolved();
    let support = LeaseRuntimeSupport {
        read_only_workspace: true,
        outputs_mount: true,
    };
    let mut item = awman_wsp::conformance::work_item("interactive");
    item.kind = awman_wsp::WorkKind::Workflow;
    item.exec.dynamic = true;
    item.exec.yolo = false;
    item.exec.interaction = None;
    settings.pull_interaction = Interaction::Interactive;
    assert!(policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker"
    )
    .is_ok());
    settings.pull_interaction = Interaction::Headless;
    assert!(policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker"
    )
    .unwrap_err()
    .contains("daemon.features.worker.pull.interaction"));
    item.kind = awman_wsp::WorkKind::Prompt;
    item.exec.dynamic = false;
    item.exec.leader = None;
    item.body = "private body marker".into();
    let plan = policy::plan(
        &item,
        &BTreeMap::new(),
        &server,
        &settings,
        &support,
        "docker",
    )
    .unwrap();
    assert!(!plan
        .args
        .iter()
        .any(|arg| arg.contains("private body marker")));
}

#[tokio::test]
async fn a_server_returning_ineligible_labels_is_released_before_any_execution() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = protocol_mock().await;
    let mut item = awman_wsp::conformance::work_item("changed-labels");
    item.affinity.required.push(awman_wsp::Match {
        key: "awman/agent.codex".into(),
        op: awman_wsp::MatchOp::In,
        values: vec!["true".into()],
    });
    let lease = awman_wsp::Lease {
        id: "server-label-check".into(),
        epoch: 1,
        expires_at: Utc::now() + chrono::Duration::minutes(5),
        item,
        terminal: false,
        remote_input: false,
    };
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"lease":lease})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/leases/server-label-check/release"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let (_home, controller) = controller(&server.uri()).await;
    controller.refresh_servers().await.unwrap();
    assert!(matches!(
        controller.claim_next(Some("changed-labels"), "team").await,
        Err(WorkerError::Ineligible(_))
    ));
    let requests = server.received_requests().await.unwrap();
    let release = requests
        .iter()
        .find(|r| r.url.path().ends_with("/release"))
        .expect("lease released");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&release.body).unwrap()["reason"],
        "ineligible"
    );
    assert!(controller.leases.active().unwrap().is_empty());
}

#[tokio::test]
async fn lease_terminal_input_needs_its_grant_and_the_live_daemon_policy() {
    use crate::data::{
        config::{ConfigFile, ConfigLayers, EffectiveConfig, FileKind},
        daemon_state::FeatureName,
        ids::CommandId,
        terminal::{TerminalOwner, TerminalOwnerInfo},
    };
    use crate::engine::{
        terminal_hub::{input_channel, TerminalInput, TerminalRegistration},
        worker::terminal_relay::TerminalChannel,
    };
    use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame};
    use base64::Engine as _;
    struct Pipe {
        input: tokio::sync::mpsc::UnboundedReceiver<LeaseTerminalFrame>,
        output: tokio::sync::mpsc::UnboundedSender<LeaseTerminalFrame>,
    }
    #[async_trait::async_trait]
    impl TerminalChannel for Pipe {
        async fn send(&mut self, frame: LeaseTerminalFrame) -> Result<(), WorkerEngineError> {
            self.output
                .send(frame)
                .map_err(|e| WorkerEngineError::Transport(e.to_string()))
        }
        async fn recv(&mut self) -> Option<Result<LeaseTerminalFrame, WorkerEngineError>> {
            self.input.recv().await.map(Ok)
        }
    }
    let (_home, controller) = controller("https://unused.example").await;
    let set_policy = |allow: bool| {
        let config = EffectiveConfig::resolve(
            ConfigLayers {
                global: ConfigFile::from_value(
                    serde_json::json!({"daemon":{"terminal":{"remoteInput":allow}}}),
                    FileKind::Global,
                )
                .unwrap(),
                ..Default::default()
            },
            Default::default(),
            controller.shared.sessions.env().clone(),
        )
        .unwrap();
        *controller.shared.api.live_config.write().unwrap() = Some(config);
    };
    let owner = TerminalOwner::Command(CommandId::new());
    let (stdin, mut bytes) = input_channel();
    let terminal = controller.hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: owner.clone(),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: true,
            },
            agent: "claude".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    for granted in [false, true] {
        set_policy(false);
        let (send, input) = tokio::sync::mpsc::unbounded_channel();
        let (output, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let mut channel = Pipe { input, output };
        let relay = runner::terminal_relay(
            controller.shared.clone(),
            controller.hub.clone(),
            owner.clone(),
            granted,
        );
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let task = tokio::spawn(async move { relay.run(&mut channel, task_stop).await });
        assert!(matches!(
            frames.recv().await.unwrap().frame,
            TerminalFrame::Snapshot { .. }
        ));
        let input = || LeaseTerminalFrame {
            terminal_id: terminal.id().to_string(),
            frame: TerminalFrame::Input {
                bytes: base64::engine::general_purpose::STANDARD.encode(b"hello\n"),
            },
        };
        send.send(input()).unwrap();
        assert!(
            matches!(frames.recv().await.unwrap().frame, TerminalFrame::Error { code, .. } if code == "input_refused")
        );
        set_policy(true);
        send.send(input()).unwrap();
        if granted {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), bytes.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                b"hello\n"
            );
            set_policy(false);
            send.send(input()).unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), bytes.recv())
                .await
                .is_err(),
            "input needs both the immutable lease grant and live policy"
        );
        stop.cancel();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_host_registers_manual_labels_and_lists_only_eligible_items() {
    let home = tempfile::tempdir().unwrap();
    let mock = Arc::new(awman_wsp::mock::MockWorkServer::new());
    let serving = mock.clone().serve().await.unwrap();
    let link = mock.link();
    mock.approve_link(&link.link_id, Default::default());
    let token = awman_wsp::Client::new(&serving.base_url, String::new())
        .unwrap()
        .poll_link(&link.link_id)
        .await
        .unwrap()
        .unwrap();
    let env = crate::data::config::env::EnvSnapshot::with_overrides([(
        "AWMAN_HOME",
        home.path().to_string_lossy().into_owned(),
    )]);
    let doc = crate::data::config::ConfigFile::from_value(serde_json::json!({"tui":{"worker":{"enabled":true}}, "workServers":{"team":{"url":serving.base_url,"claim":"auto"}}}), crate::data::config::FileKind::Global).unwrap();
    doc.save_global(&env).unwrap();
    LinkStore::new(&crate::data::fs::DataPaths::from_env(&env).unwrap())
        .write("team", &token)
        .unwrap();
    let mut item = awman_wsp::conformance::work_item("tui-only");
    item.affinity.required.push(awman_wsp::Match {
        key: "awman/host".into(),
        op: awman_wsp::MatchOp::In,
        values: vec!["tui".into()],
    });
    mock.enqueue(item, Utc::now()).unwrap();
    let session = crate::data::session::Session::open_at_git_root(
        home.path().into(),
        home.path().into(),
        crate::data::session::SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = crate::command::dispatch::Engines::for_tests(home.path());
    let source = super::tui_host::TuiWorkerController::start(&session, &engines)
        .unwrap()
        .unwrap();
    let snapshot = source.snapshot();
    assert!(snapshot.loaded);
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].id, "tui-only");
    source.mark_seen();
    assert_eq!(source.snapshot().new_since_seen, 0);
    assert!(
        mock.result_for("tui-only").is_none(),
        "TUI hosts never auto-claim even when the server config says auto"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feature_cancel_releases_background_and_foreground_clones_before_job_creation() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    for foreground in [false, true] {
        let server = protocol_mock().await;
        let (_home, controller) = controller(&server.uri()).await;
        let mut item = awman_wsp::conformance::work_item("blocked");
        item.repo.url = format!("{}/repo.git", server.uri());
        item.repo.credentials = None;
        item.exec.interaction = None;
        let lease = awman_wsp::Lease {
            id: "server-cloning".into(),
            epoch: 1,
            expires_at: Utc::now() + chrono::Duration::minutes(5),
            item,
            terminal: false,
            remote_input: false,
        };
        Mock::given(method("POST"))
            .and(path("/v1/claims"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({"lease":lease})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repo.git/info/refs"))
            .respond_with(ResponseTemplate::new(404).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/leases/server-cloning/release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        controller.refresh_servers().await.unwrap();
        let c = controller.clone();
        let claim =
            tokio::spawn(
                async move { c.claim_next_for(Some("blocked"), "team", foreground).await },
            );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path() == "/repo.git/info/refs")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("clone began");
        let records = controller.leases.active().unwrap();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].job_id.is_none(),
            "must exercise the pre-command interval"
        );
        PullHandleAdapter(controller.clone()).cancel_jobs();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if controller
                    .leases
                    .get(&records[0].id)
                    .unwrap()
                    .unwrap()
                    .ended_at
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("preparing lease cancelled promptly");
        let _ = claim.await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let releases: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/release"))
            .collect();
        assert_eq!(releases.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&releases[0].body).unwrap()["reason"],
            "error"
        );
    }
}

#[tokio::test]
async fn idle_long_poll_does_not_occupy_worker_or_host_execution_capacity() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = protocol_mock().await;
    Mock::given(method("POST"))
        .and(path("/v1/claims"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let (_home, controller) = controller(&server.uri()).await;
    controller
        .share
        .executor()
        .set_share(crate::data::daemon_state::FeatureName::Worker, 1);
    controller.refresh_servers().await.unwrap();
    let c = controller.clone();
    let claim = tokio::spawn(async move { c.claim_next(None, "team").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == "/v1/claims")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(controller.share.snapshot().running_pull, 0);
    let push = tokio::time::timeout(
        Duration::from_millis(500),
        controller.share.acquire(JobClass::Push),
    )
    .await
    .expect("idle pull must not reserve the only worker slot");
    assert_eq!(push.class(), JobClass::Push);
    controller.claim_stop.lock().unwrap().cancel();
    assert!(claim.await.unwrap().is_err());
}

#[tokio::test]
async fn recovery_preserves_live_foreground_owner_and_releases_after_owner_exit() {
    use wiremock::{
        matchers::{method, path},
        Mock, ResponseTemplate,
    };
    let server = protocol_mock().await;
    let (_home, controller) = controller(&server.uri()).await;
    let lease = awman_wsp::Lease {
        id: "server-live".into(),
        epoch: 1,
        expires_at: Utc::now() + chrono::Duration::minutes(10),
        item: awman_wsp::conformance::work_item("live"),
        terminal: false,
        remote_input: false,
    };
    let record = LeaseRecord::claimed("team", &lease, LeaseHost::Tui, Utc::now());
    let owner =
        crate::data::worker::paths::try_lease_owner(controller.data_paths().root(), &record.id)
            .unwrap()
            .unwrap();
    controller.leases.insert(&record).unwrap();
    controller.recover().await.unwrap();
    assert_eq!(
        controller.leases.get(&record.id).unwrap().unwrap().state,
        record.state
    );
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.path().ends_with("/release")));
    Mock::given(method("POST"))
        .and(path("/v1/leases/server-live/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    drop(owner);
    controller.recover().await.unwrap();
    assert_eq!(
        controller.leases.get(&record.id).unwrap().unwrap().state,
        LeaseState::Released
    );
}
