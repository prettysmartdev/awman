//! Production daemon/controller integration. Only the container CLI is fake;
//! registration, routes, admission, helper, checkout, dispatch and reporting are real.
#![cfg(unix)]
#[path = "helpers/api_process.rs"]
mod api_process;
#[path = "helpers/smart_http.rs"]
mod smart_http;
use api_process::{AgentMode, ApiHome, ApiServer};
use awman_wsp::{
    mock::{MockWorkServer, ServingMock},
    WorkItem,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

struct Fixture {
    root: tempfile::TempDir,
    home: ApiHome,
    mock: Arc<MockWorkServer>,
    _serving: ServingMock,
    repo: smart_http::SmartHttpRepo,
    server: ApiServer,
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_queue_picker_in_a_real_tty_runs_the_selected_item_real_git() {
    use std::io::{Read, Write};
    let f = fixture(false).await;
    f.home.cli_ok(&[
        "config",
        "set",
        "daemon.features.worker.pull.interaction",
        "interactive",
    ]);
    post_fixture(&f, "/v1/config/reload", json!(null)).await;
    let mut work = item(&f, "tty-picked");
    work.exec.interaction = Some(awman_wsp::Interaction::Interactive);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    f.mock.set_terminal("tty-picked", true, false);
    let command = f.home.command(
        &[("ANTHROPIC_API_KEY", "fake-agent-key")],
        &["queue", "pick"],
    );
    let mut builder = portable_pty::CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    builder.env_clear();
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            builder.env(key, value);
        }
    }
    builder.env("TERM", "xterm-256color");
    builder.cwd(f.root.path().join("seed"));
    let output = tokio::task::spawn_blocking(move || {
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut child = pty.slave.spawn_command(builder).unwrap();
        drop(pty.slave);
        struct Kill(Box<dyn portable_pty::ChildKiller + Send + Sync>);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
            }
        }
        let _kill = Kill(child.clone_killer());
        let mut reader = pty.master.try_clone_reader().unwrap();
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = bytes.clone();
        let _reader = std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                observed.lock().unwrap().extend_from_slice(&buffer[..n]);
            }
        });
        let mut writer = pty.master.take_writer().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let text = String::from_utf8_lossy(&bytes.lock().unwrap()).into_owned();
            if text.contains("Pick a work item") && text.contains("tty-picked") {
                break;
            }
            assert!(child.try_wait().unwrap().is_none(), "picker exited: {text}");
            assert!(std::time::Instant::now() < until, "picker deadline: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
        writer.write_all(b"\r").unwrap();
        writer.flush().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let text = String::from_utf8_lossy(&bytes.lock().unwrap()).into_owned();
                assert!(status.success(), "{text}");
                break text;
            }
            assert!(
                std::time::Instant::now() < until,
                "execution deadline: {}",
                String::from_utf8_lossy(&bytes.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    })
    .await
    .unwrap();
    let result = f
        .mock
        .result_for("tty-picked")
        .expect("picker completed a real lease")
        .result;
    assert_eq!(result["outcome"], "done", "{result}\n{output}");
    assert!(result["pushed"]["sha"].is_string(), "{result}");
    let leases = f.server.get("/v1/worker/leases").await;
    let frames = f
        .mock
        .terminal_frames(leases["items"][0]["server_lease_id"].as_str().unwrap());
    assert!(
        frames.iter().any(|frame| matches!(
            frame.frame,
            awman_wsp::terminal::TerminalFrame::Snapshot { .. }
        )),
        "{frames:?}"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame.frame,
            awman_wsp::terminal::TerminalFrame::Exit { code: Some(0) }
        )),
        "terminal exit must drain before result: {frames:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_queue_pick_runs_to_completion_and_json_is_only_the_result_real_git() {
    let f = fixture(false).await;
    f.mock
        .enqueue(item(&f, "cli-picked"), chrono::Utc::now())
        .unwrap();
    let mut command = f.home.command(
        &[("ANTHROPIC_API_KEY", "fake-agent-key")],
        &["queue", "pick", "cli-picked", "--server", "team", "--json"],
    );
    command.current_dir(f.root.path().join("seed"));
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\ndaemon: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        f.server.log()
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("exactly one JSON result");
    assert_eq!(result["outcome"], "done", "{result}");
    assert!(result["pushed"]["sha"].is_string(), "{result}");
    assert_eq!(f.mock.result_for("cli-picked").unwrap().result, result);
    let mut command = f.home.command(&[], &["queue", "pick", "--json"]);
    command.current_dir(f.root.path().join("seed"));
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"].as_str().unwrap().contains("item-id"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_foreground_structured_agent_output_is_preserved_in_the_result_real_git() {
    let f = fixture(false).await;
    std::fs::write(f.home.control().join("mode"), "structured").unwrap();
    std::fs::write(f.home.control().join("agent.ndjson"),
        "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"foreground final message\"}\n").unwrap();
    f.mock
        .enqueue(item(&f, "structured-picked"), chrono::Utc::now())
        .unwrap();
    let mut command = f.home.command(
        &[("ANTHROPIC_API_KEY", "fake-agent-key")],
        &["queue", "pick", "structured-picked", "--json"],
    );
    command.current_dir(f.root.path().join("seed"));
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["final_message"], "foreground final message",
        "{result}"
    );
    assert_eq!(
        f.mock.result_for("structured-picked").unwrap().result,
        result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_handshake_loss_alone_fences_daemon_execution_and_publication_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.home.cli_ok(&[
        "config",
        "set",
        "daemon.features.worker.pull.interaction",
        "interactive",
    ]);
    post_fixture(&f, "/v1/config/reload", json!(null)).await;
    let mut work = item(&f, "terminal-lost");
    work.exec.interaction = Some(awman_wsp::Interaction::Interactive);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    f.mock.set_terminal("terminal-lost", true, false);
    f.mock.refuse_terminal("terminal-lost", "lease_lost");
    let lease = claim(&f, "terminal-lost").await;
    let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "lost", "{ended}");
    assert!(f.mock.result_for("terminal-lost").is_none());
    assert!(f
        .mock
        .release_for(lease["server_lease_id"].as_str().unwrap())
        .is_none());
    assert!(
        !smart_http::git(&f.repo.bare, &["branch", "--list", "wsp/terminal-lost"])
            .contains("wsp/terminal-lost")
    );
    let command = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}",
            ended["job_id"].as_str().unwrap()
        ))
        .await;
    assert_ne!(command["status"], "running", "{command}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_and_daemon_instances_execute_their_picked_lease_and_publish_real_git() {
    use awman::{
        command::{
            dispatch::{catalogue::CommandCatalogue, Engines},
            instance::{
                backend::InstanceBackend,
                feeds::TabCommandFeeds,
                frontend::PlainPresentation,
                local::LocalInstanceBackend,
                remote::{RemoteInstanceBackend, RemoteOpen},
                InitialTabOpened, InitialTabSpec, InstanceParts, SubmitRequest,
            },
        },
        data::{
            config::env::EnvSnapshot,
            frontend_protocol::*,
            session::{Session, SessionOpenOptions},
            session_manager::SessionManager,
        },
    };
    for remote in [false, true] {
        let f = fixture(false).await;
        std::fs::write(f.home.control().join("mode"), "structured").unwrap();
        std::fs::write(
            f.home.control().join("agent.ndjson"),
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"instance final message\"}\n",
        )
        .unwrap();
        f.home
            .cli_ok(&["config", "set", "tui.worker.enabled", "true", "--global"]);
        f.home
            .cli_ok(&["config", "set", "daemon.features.engine.enabled", "true"]);
        post_fixture(&f, "/v1/config/reload", json!(null)).await;
        let command = f
            .home
            .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
        let env = EnvSnapshot::with_overrides(command.get_envs().filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        }));
        let root = f.root.path().join("seed");
        let session = Session::open_at_git_root(
            root.clone(),
            root.clone(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let backend: Arc<dyn InstanceBackend> = if remote {
            let endpoint = awman::engine::remote::local::discover(
                &awman::data::fs::DataPaths::from_env(&env).unwrap().daemon(),
            )
            .unwrap()
            .unwrap();
            let hello = Hello {
                protocol: FRONTEND_SUBPROTOCOL.into(),
                build: env!("CARGO_PKG_VERSION").into(),
                kind: FrontendKindWire::Tui,
                role: FrontendRole::Primary,
                annotations: Default::default(),
                cwd: root,
                argv: vec!["awman".into()],
                env: env.to_hello_env(),
                term_size: TermSize {
                    cols: 100,
                    rows: 30,
                },
                stdin_is_tty: true,
                stdout_is_tty: true,
                view: None,
                resume: None,
            };
            RemoteInstanceBackend::connect(RemoteOpen::primary(
                &endpoint,
                hello,
                env.clone(),
                CommandCatalogue::get(),
            ))
            .await
            .unwrap()
        } else {
            let captured = env.clone();
            LocalInstanceBackend::open(InstanceParts {
                catalogue: CommandCatalogue::get(),
                engines: Engines::build(&session).unwrap(),
                session_manager: Arc::new(SessionManager::in_memory(env)),
                runtime: tokio::runtime::Handle::current(),
                env: Arc::new(move || captured.clone()),
                presentation: Arc::new(PlainPresentation),
            })
        };
        let InitialTabOpened::Normal { tab } = backend.open_initial(InitialTabSpec::Session {
            session,
            fatal_startup_error: true,
            pty_size: (100, 30),
        }) else {
            panic!("normal initial tab")
        };
        f.mock
            .enqueue(item(&f, "instance-picked"), chrono::Utc::now())
            .unwrap();
        let source = backend.queue_source(&tab.session).unwrap().unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(15);
        while !source
            .snapshot()
            .items
            .iter()
            .any(|row| row.id == "instance-picked")
        {
            assert!(
                tokio::time::Instant::now() < until,
                "remote={remote}: {:?}",
                source.snapshot()
            );
            source.refresh();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            source
                .snapshot()
                .items
                .iter()
                .find(|row| row.id == "instance-picked")
                .unwrap()
                .body
                .as_deref(),
            Some(item(&f, "instance-picked").body.as_str())
        );
        let picked = source.pick("team", "instance-picked").unwrap();
        let submitted = backend
            .submit(SubmitRequest {
                lease_id: Some(picked.lease_id),
                session_id: tab.session.id(),
                parsed: awman::command::dispatch::parsed_input::parse(
                    &picked.command_line,
                    CommandCatalogue::get(),
                )
                .unwrap(),
                feeds: TabCommandFeeds::new(),
                pty_size: (100, 30),
                squad_attach: None,
            })
            .unwrap();
        let outcome = tokio::task::spawn_blocking(move || {
            submitted
                .feeds
                .command_result_rx
                .recv_timeout(Duration::from_secs(45))
                .unwrap()
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome.exit_code(), 0, "remote={remote}");
        let reported = f
            .mock
            .result_for("instance-picked")
            .expect("instance completed lease");
        assert_eq!(
            reported.result["final_message"], "instance final message",
            "remote={remote}: {reported:?}"
        );
        assert_eq!(
            reported.result["outcome"], "done",
            "remote={remote}: {reported:?}"
        );
        assert!(reported.result["pushed"]["sha"].is_string(), "{reported:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_cancellation_stops_an_already_running_container_and_drains_events_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    let mut work = item(&f, "heartbeat-stop");
    work.exec.timeout_ms = Some(60_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let lease = claim(&f, "heartbeat-stop").await;
    wait(&f, || f.home.control().join("slow.pid").exists()).await;
    let upstream = lease["server_lease_id"].as_str().unwrap();
    f.mock
        .request_cancel(upstream, "stop the running container");
    wait(&f, || f.mock.result_for("heartbeat-stop").is_some()).await;
    let result = f.mock.result_for("heartbeat-stop").unwrap();
    assert_eq!(result.result["outcome"], "cancelled", "{result:?}");
    let log = std::fs::read_to_string(f.home.control().join("docker.log")).unwrap();
    assert!(
        log.lines().any(|line| line.starts_with("docker stop ")),
        "heartbeat must stop the launched runtime: {log}"
    );
    let events = f.mock.events_for(upstream);
    assert_eq!(events.last().unwrap()["payload"]["type"], "done");
    assert!(events
        .iter()
        .any(|event| event["payload"]["type"] == "command_status"
            && event["payload"]["data"]["status"] == "cancelled"));
    assert!(
        !smart_http::git(&f.repo.bare, &["branch", "--list", "wsp/heartbeat-stop"])
            .contains("wsp/heartbeat-stop")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_release_waits_for_partial_workflow_result_and_final_events_real_git() {
    use awman::command::{
        dispatch::Engines,
        worker::{pick::QueuePickCommand, queue_source::LeaseCloseChoice},
    };
    use awman::data::{
        config::env::EnvSnapshot,
        session::{Session, SessionOpenOptions},
    };
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::OutputsResume);
    let mut work = item(&f, "partial-release");
    work.kind = awman_wsp::WorkKind::Workflow;
    work.exec.timeout_ms = Some(60_000);
    work.exec.workflow_toml = Some("[[steps]]\nname='first'\nagent='claude'\nprompt='first'\n[[steps]]\nname='second'\ndepends_on=['first']\nagent='claude'\nprompt='second'\n".into());
    work.outputs = Some(awman_wsp::Outputs { max_bytes: 4096 });
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let command = f
        .home
        .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
    let env = EnvSnapshot::with_overrides(command.get_envs().filter_map(|(k, v)| {
        v.map(|v| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
    }));
    let root = f.root.path().join("seed");
    let session = Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let pick = QueuePickCommand::new(
        session,
        engines,
        Some("team".into()),
        Some("partial-release".into()),
        true,
    );
    let (source, run) = pick.prepare(&mut ForegroundPresentation).await.unwrap();
    let lease = f
        .server
        .get(&format!("/v1/worker/leases/{}", run.lease_id))
        .await;
    let upstream = lease["server_lease_id"].as_str().unwrap().to_string();
    let owner = source.clone();
    let execution = run.clone();
    let running = tokio::spawn(async move {
        QueuePickCommand::run_prepared(owner, execution, &mut ForegroundPresentation).await
    });
    wait(&f, || {
        f.mock.events_for(&upstream).iter().any(|e| {
            e["payload"]["type"] == "workflow_state"
                && e["payload"]["data"]["steps"]
                    .as_array()
                    .is_some_and(|steps| {
                        steps
                            .iter()
                            .any(|step| step["name"] == "second" && step["status"] == "running")
                    })
        })
    })
    .await;
    source
        .finish_lease(&run.lease_id, LeaseCloseChoice::Release)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        result.outcome,
        Some(awman::data::execution_event::CommandStatusKind::Cancelled)
    );
    assert!(
        result.git.is_some(),
        "partial checkout facts survive: {result:?}"
    );
    assert!(
        result
            .outputs
            .as_ref()
            .is_some_and(|outputs| outputs.files > 0),
        "completed-step output survives: {result:?}"
    );
    let release = f.mock.release_for(&upstream).unwrap();
    assert_eq!(release.reason, awman_wsp::ReleaseReason::UserDeclined);
    assert_eq!(
        release.result.unwrap(),
        serde_json::to_value(&result).unwrap()
    );
    assert!(f.mock.result_for("partial-release").is_none());
    let events = f.mock.events_for(&upstream);
    assert_eq!(events.last().unwrap()["payload"]["type"], "done");
    assert!(events
        .iter()
        .any(|event| event["payload"]["type"] == "workflow_state"
            && event.to_string().contains("cancelled")));
}

async fn fixture(auto: bool) -> Fixture {
    fixture_options(auto, 1, true).await
}
async fn fixture_options(auto: bool, capacity: usize, dockerfile: bool) -> Fixture {
    fixture_repository(auto, capacity, dockerfile, Some("daemon-repo-secret")).await
}
async fn fixture_repository(
    auto: bool,
    capacity: usize,
    dockerfile: bool,
    token: Option<&str>,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let repo = smart_http::SmartHttpRepo::start(root.path(), token).await;
    let seed = root.path().join("seed");
    std::fs::create_dir_all(seed.join(".awman")).unwrap();
    if dockerfile {
        std::fs::write(seed.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
    } else {
        std::fs::write(seed.join("README.md"), "uninitialised repository\n").unwrap();
    }
    smart_http::commit_all(&seed, "agent fixture");
    smart_http::git(&seed, &["push", "-q", repo.bare.to_str().unwrap(), "main"]);
    let mock = Arc::new(MockWorkServer::new());
    let serving = mock.clone().serve().await.unwrap();
    let link = mock.link();
    mock.approve_link(&link.link_id, Default::default());
    let token = awman_wsp::Client::new(&serving.base_url, String::new())
        .unwrap()
        .poll_link(&link.link_id)
        .await
        .unwrap()
        .unwrap();
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"maxConcurrentJobs":2,"features":{"worker":{"maxConcurrentJobs":2,"push":{"workDirs":{"allow":[seed]}},"pull":{"enabled":true,"interaction":"headless","workRoot":root.path().join("worker")}}}},"workServers":{"team":{"url":serving.base_url,"claim":if auto {"auto"} else {"manual"},"maxConcurrentJobs":capacity}}}));
    let env = awman::data::config::env::EnvSnapshot::with_overrides([
        ("HOME", home.home().to_str().unwrap().to_string()),
        (
            "XDG_CONFIG_HOME",
            home.home().join(".config").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME",
            home.home()
                .join(".local/share")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let links = awman::data::worker::link_store::LinkStore::new(
        &awman::data::fs::DataPaths::from_env(&env).unwrap(),
    );
    links.write("team", &token).unwrap();
    let server = home
        .start_with_env(&[("ANTHROPIC_API_KEY", "fake-agent-key")])
        .await;
    let fixture = Fixture {
        root,
        home,
        mock,
        _serving: serving,
        repo,
        server,
    };
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let servers = fixture.server.get("/v1/worker/servers").await;
        if servers["items"][0]["state"] == "registered" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "{servers}\n{}",
            fixture.server.log()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    fixture
}
fn item(f: &Fixture, id: &str) -> WorkItem {
    let mut item = awman_wsp::conformance::work_item(id);
    item.exec.agent = Some("claude".into());
    item.exec.timeout_ms = Some(20000);
    item.kind = awman_wsp::WorkKind::Prompt;
    item.exec.dynamic = false;
    item.exec.workflow_toml = None;
    item.exec.interaction = Some(awman_wsp::Interaction::Headless);
    item.repo.url = f.repo.url.clone();
    item.repo.base_ref = "main".into();
    item.repo.branch = format!("wsp/{id}");
    item.repo.credentials = Some(awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Bearer,
        token: Some("daemon-repo-secret".into()),
        username: None,
        expires_at: None,
    });
    item.push = Some(awman_wsp::Push {
        enabled: true,
        branch: format!("wsp/{id}"),
    });
    item
}
async fn wait(f: &Fixture, test: impl Fn() -> bool) {
    let until = tokio::time::Instant::now() + Duration::from_secs(60);
    while !test() {
        assert!(
            tokio::time::Instant::now() < until,
            "timed out:\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn claim(f: &Fixture, id: &str) -> Value {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/worker/queue/{id}/claim?server=team",
            f.server.url
        ))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status.as_u16(), 201, "{body}\n{}", f.server.log());
    body
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_auto_pull_uses_actual_helper_dispatch_push_and_public_events_real_git() {
    let f = fixture(true).await;
    f.mock
        .enqueue(item(&f, "automatic"), chrono::Utc::now())
        .unwrap();
    wait(&f, || f.mock.result_for("automatic").is_some()).await;
    let result = f.mock.result_for("automatic").unwrap();
    assert!(
        result.outputs_mounted,
        "the launched guest acknowledged its output bind"
    );
    assert_eq!(
        result.result["outcome"],
        "done",
        "{}\n{}",
        result.result,
        f.server.log()
    );
    assert!(
        result.result["pushed"]["sha"].is_string(),
        "{}",
        result.result
    );
    let leases = f.server.get("/v1/worker/leases?server=team").await;
    let lease = &leases["items"][0];
    assert_eq!(lease["state"], "done", "{leases}");
    let job = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}",
            lease["job_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(job["origin"]["kind"], "pull", "{job}");
    let events = f
        .mock
        .events_for(lease["server_lease_id"].as_str().unwrap());
    assert!(!events.is_empty());
    let json_log = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}/events?format=json",
            lease["job_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(json_log["events"], serde_json::to_value(&events).unwrap());
    assert!(events
        .iter()
        .all(|event| event.get("emitted_at").is_some() && event.get("timestamp").is_none()));
    assert_eq!(events.last().unwrap()["payload"]["type"], "done");
    assert!(!f.server.log().contains("daemon-repo-secret"));
    assert!(!f
        .root
        .path()
        .join("worker/leases")
        .read_dir()
        .unwrap()
        .any(|e| e.is_ok()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_manual_ineligible_releases_before_clone_and_paginates_servers_real_git() {
    let f = fixture(false).await;
    let mut work = item(&f, "unsupported");
    work.exec.overlays.push("../..:/private".into());
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/worker/queue/unsupported/claim?server=team",
            f.server.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "ineligible");
    assert!(!f.root.path().join("worker/leases").exists());
    assert!(f.repo.requests().is_empty());
    let page = f.server.get("/v1/worker/servers?limit=1").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_release_waits_for_execution_cancellation_and_reports_once_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "release"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "release").await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/worker/leases/{}/release",
            f.server.url,
            lease["id"].as_str().unwrap()
        ))
        .json(&json!({"reason":"user_declined"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        200,
        "{}",
        response.text().await.unwrap()
    );
    assert!(f.mock.result_for("release").is_none());
    assert_eq!(
        f.mock
            .release_for(lease["server_lease_id"].as_str().unwrap())
            .unwrap()
            .reason,
        awman_wsp::ReleaseReason::UserDeclined
    );
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/worker/leases/{}/release",
            f.server.url,
            lease["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
}

async fn running_lease(f: &Fixture, id: &str) -> Value {
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let page = f.server.get("/v1/worker/leases?state=active").await;
        if let Some(record) = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["item_id"] == id && row["job_id"].is_string())
        {
            if f.home.control().join("slow.pid").exists() {
                return record.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < until,
            "no running lease:\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn wait_ended(f: &Fixture, lease_id: &str) -> Value {
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let record = f.server.get(&format!("/v1/worker/leases/{lease_id}")).await;
        if matches!(record["state"].as_str(), Some("done" | "released" | "lost")) {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "lease never ended: {record}\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_revocation_cancels_execution_discards_result_and_never_pushes_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "revoke"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "revoke").await;
    let _ = running_lease(&f, "revoke").await;
    // Read the stored refresh token through its actual owner-only store.
    let env = awman::data::config::env::EnvSnapshot::with_overrides([
        ("HOME", f.home.home().to_string_lossy().into_owned()),
        (
            "XDG_DATA_HOME",
            f.home
                .home()
                .join(".local/share")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let links = awman::data::worker::link_store::LinkStore::new(
        &awman::data::fs::DataPaths::from_env(&env).unwrap(),
    );
    f.mock
        .revoke(&links.read("team").unwrap().unwrap().refresh_token);
    f.mock.invalidate_access_tokens();
    let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "lost");
    assert!(f.mock.result_for("revoke").is_none());
    assert!(!f.repo.requests().iter().any(|request| request.is_push()));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_unlink_after_completed_history_deletes_token_immediately_real_git() {
    let f = fixture(true).await;
    f.mock
        .enqueue(item(&f, "finished"), chrono::Utc::now())
        .unwrap();
    wait(&f, || f.mock.result_for("finished").is_some()).await;
    let response = tokio::task::spawn_blocking({
        let mut command = f
            .home
            .command(&[], &["config", "unset", "workServers.team"]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    let env = awman::data::config::env::EnvSnapshot::with_overrides([
        ("HOME", f.home.home().to_string_lossy().into_owned()),
        (
            "XDG_DATA_HOME",
            f.home
                .home()
                .join(".local/share")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let links = awman::data::worker::link_store::LinkStore::new(
        &awman::data::fs::DataPaths::from_env(&env).unwrap(),
    );
    assert!(
        links.read("team").unwrap().is_none(),
        "completed lease history must not defer unlink"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_disabling_pull_stops_claims_and_preserves_running_lease_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "disable"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "disable").await;
    let running = running_lease(&f, "disable").await;
    f.home.write_config(json!({"daemon":{"features":{"worker":{"pull":{"enabled":false}}}},"workServers":{"team":{"url":f._serving.base_url,"claim":"manual"}}}));
    let client = reqwest::Client::new();
    assert!(client
        .post(format!("{}/v1/config/reload", f.server.url))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let record = f
        .server
        .get(&format!(
            "/v1/worker/leases/{}",
            lease["id"].as_str().unwrap()
        ))
        .await;
    assert!(
        record["ended_at"].is_null(),
        "disable drains the lease: {record}"
    );
    let response = client
        .post(format!(
            "{}/v1/worker/queue/other/claim?server=team",
            f.server.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    client
        .post(format!(
            "{}/v1/worker/commands/{}/cancel",
            f.server.url,
            running["job_id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    wait_ended(&f, lease["id"].as_str().unwrap()).await;
    let release = f
        .mock
        .release_for(lease["server_lease_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(release.reason, awman_wsp::ReleaseReason::Error);
    assert_eq!(release.result.unwrap()["outcome"], "cancelled");
    assert!(f.mock.result_for("disable").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_runs_inline_toml_and_dynamic_workflows_from_private_body_real_git() {
    let f = fixture(false).await;
    for dynamic in [false, true] {
        let id = if dynamic { "dynamic" } else { "inline-toml" };
        let mut work = item(&f, id);
        work.kind = awman_wsp::WorkKind::Workflow;
        work.exec.dynamic = dynamic;
        work.exec.yolo = true;
        work.exec.leader = dynamic.then(|| "claude::claude-opus-4-8".into());
        work.exec.workflow_toml = (!dynamic).then(|| "[[steps]]\nname='implement'\nagent='claude'\nprompt='implement the supplied work item'\n".into());
        work.body = format!("Private body for {id}; do not create a repository work-item file.");
        f.mock.enqueue(work, chrono::Utc::now()).unwrap();
        let lease = claim(&f, id).await;
        wait(&f, || f.mock.result_for(id).is_some()).await;
        let result = f.mock.result_for(id).unwrap();
        assert_eq!(
            result.result["outcome"],
            "done",
            "{}\n{}",
            result.result,
            f.server.log()
        );
        let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
        assert_eq!(ended["state"], "done");
        let files = smart_http::git(
            &f.repo.bare,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &format!("refs/heads/wsp/{id}"),
            ],
        );
        assert!(
            !files.contains("work-item.md") && !files.contains("workflow.toml"),
            "{files}"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_releases_persisted_lease_and_does_not_rerun_it_real_git() {
    let mut f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "restart"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "restart").await;
    let running = running_lease(&f, "restart").await;
    f.server.kill();
    // The fake CLI has no container daemon to reap orphaned fake processes.
    for file in ["slow.child", "slow.pid"] {
        if let Ok(pid) = std::fs::read_to_string(f.home.control().join(file)) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
    f.home.set_agent(AgentMode::NoOp);
    f.server = f
        .home
        .start_with_env(&[("ANTHROPIC_API_KEY", "fake-agent-key")])
        .await;
    let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
    assert_eq!(ended["state"], "released", "{ended}\n{}", f.server.log());
    assert_eq!(
        f.mock
            .release_for(lease["server_lease_id"].as_str().unwrap())
            .unwrap()
            .reason,
        awman_wsp::ReleaseReason::Shutdown
    );
    assert!(f.mock.result_for("restart").is_none());
    let command = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}",
            running["job_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(command["status"], "cancelled", "{command}");
    assert!(!f.repo.requests().iter().any(|request| request.is_push()));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_stop_now_releases_running_pull_before_exiting_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "shutdown"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "shutdown").await;
    let _ = running_lease(&f, "shutdown").await;
    let response = tokio::task::spawn_blocking({
        let mut command = f.home.command(&[], &["daemon", "stop", "--now"]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        response.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&response.stderr),
        f.server.log()
    );
    assert_eq!(
        f.mock
            .release_for(lease["server_lease_id"].as_str().unwrap())
            .unwrap()
            .reason,
        awman_wsp::ReleaseReason::Shutdown
    );
    assert!(f.mock.result_for("shutdown").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_default_stop_waits_for_running_pull_result_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    let mut work = item(&f, "drain");
    work.exec.timeout_ms = Some(60_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let lease = claim(&f, "drain").await;
    let _ = running_lease(&f, "drain").await;
    let response = tokio::task::spawn_blocking({
        let mut command = f.home.command(&[], &["daemon", "stop"]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        response.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&response.stderr),
        f.server.log()
    );
    let result = f
        .mock
        .result_for("drain")
        .expect("drained lease reported a result");
    assert_eq!(result.result["outcome"], "done", "{}", result.result);
    assert!(f
        .mock
        .release_for(lease["server_lease_id"].as_str().unwrap())
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_persists_recovery_link_before_command_row_is_visible_real_git() {
    let f = fixture(false).await;
    let env = awman::data::config::env::EnvSnapshot::with_overrides([
        ("HOME", f.home.home().to_string_lossy().into_owned()),
        (
            "XDG_DATA_HOME",
            f.home
                .home()
                .join(".local/share")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let paths = awman::data::fs::DataPaths::from_env(&env).unwrap();
    let store = awman::data::fs::api_db::SqliteSessionStore::open(paths.root()).unwrap();
    // Enforce the crash-safety invariant at the exact visibility boundary.
    store.with_conn(|connection| {
        connection.execute_batch("CREATE TRIGGER require_pull_recovery_link BEFORE INSERT ON commands
            WHEN NEW.worker_id = 'pull' AND NOT EXISTS (SELECT 1 FROM worker_leases WHERE job_id = NEW.id)
            BEGIN SELECT RAISE(FAIL, 'missing durable lease recovery link'); END;")?;
        Ok(())
    }).unwrap();
    f.mock
        .enqueue(item(&f, "durable-link"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "durable-link").await;
    wait(&f, || f.mock.result_for("durable-link").is_some()).await;
    let result = f.mock.result_for("durable-link").unwrap();
    assert_eq!(
        result.result["outcome"],
        "done",
        "{}\n{}",
        result.result,
        f.server.log()
    );
    assert_eq!(
        wait_ended(&f, lease["id"].as_str().unwrap()).await["state"],
        "done"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_auto_runs_three_in_server_order_and_refreshes_queued_count_real_git() {
    let f = fixture(true).await;
    f.home.set_agent(AgentMode::Slow);
    for id in ["ordered-1", "ordered-2", "ordered-3"] {
        let mut work = item(&f, id);
        work.exec.timeout_ms = Some(60_000);
        f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    }
    let _ = running_lease(&f, "ordered-1").await;
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = f.server.get("/v1/status").await;
        if status["features"]["worker"]["pull"]["servers"][0]["queued_eligible"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "queued count not refreshed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.home.set_agent(AgentMode::Edit);
    wait(&f, || f.mock.result_for("ordered-3").is_some()).await;
    let leases = f.server.get("/v1/worker/leases?server=team").await;
    let mut rows = leases["items"].as_array().unwrap().clone();
    rows.sort_by_key(|l| l["claimed_at"].as_str().unwrap().to_owned());
    assert_eq!(
        rows.iter()
            .map(|l| l["item_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["ordered-1", "ordered-2", "ordered-3"]
    );
    for id in ["ordered-1", "ordered-2", "ordered-3"] {
        assert_eq!(f.mock.result_for(id).unwrap().result["outcome"], "done");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_server_capacity_two_starts_two_leases_together_real_git() {
    let f = fixture_options(true, 2, true).await;
    f.home.set_agent(AgentMode::Slow);
    for id in ["parallel-1", "parallel-2", "parallel-3"] {
        f.mock.enqueue(item(&f, id), chrono::Utc::now()).unwrap();
    }
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let page = f.server.get("/v1/worker/leases?state=active").await;
        let running = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["state"] == "running")
            .count();
        if running == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "two leases did not start: {page}\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(f.mock.result_for("parallel-1").is_none());
    assert!(f.mock.result_for("parallel-2").is_none());
    let all = f.server.get("/v1/worker/leases").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 2);
    f.home.set_agent(AgentMode::Edit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_uninitialised_clone_provisions_without_publishing_recipe_real_git() {
    let f = fixture_options(false, 1, false).await;
    f.mock
        .enqueue(item(&f, "first-party-init"), chrono::Utc::now())
        .unwrap();
    claim(&f, "first-party-init").await;
    wait(&f, || f.mock.result_for("first-party-init").is_some()).await;
    let result = f.mock.result_for("first-party-init").unwrap();
    assert_eq!(
        result.result["outcome"],
        "done",
        "{}\n{}",
        result.result,
        f.server.log()
    );
    let output = std::process::Command::new("git")
        .args([
            "--git-dir",
            f.repo.bare.to_str().unwrap(),
            "ls-tree",
            "-r",
            "--name-only",
            "wsp/first-party-init",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(".awman/Dockerfile.claude"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_missing_custom_agent_recipe_names_explicit_init_remedy_real_git() {
    let f = fixture_options(false, 1, false).await;
    let mut work = item(&f, "custom-init");
    work.exec.agent = Some("my-agent".into());
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    claim(&f, "custom-init").await;
    wait(&f, || f.mock.result_for("custom-init").is_some()).await;
    let result = f.mock.result_for("custom-init").unwrap();
    assert!(!result.outputs_mounted, "preflight never launched a guest");
    assert_eq!(result.result["failure"]["stage"], "preflight");
    assert!(
        result.result["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("awman init --agent my-agent"),
        "{}",
        result.result
    );
    assert!(!f.repo.requests().iter().any(|r| r.is_push()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_prompt_body_is_private_and_lease_result_is_the_command_result_real_git() {
    let f = fixture(false).await;
    let mut work = item(&f, "private-body");
    work.body = "private-marker-do-not-persist-in-args".into();
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let lease = claim(&f, "private-body").await;
    wait(&f, || f.mock.result_for("private-body").is_some()).await;
    let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
    let record = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}",
            ended["job_id"].as_str().unwrap()
        ))
        .await;
    assert!(
        !record["args"].to_string().contains("private-marker"),
        "{record}"
    );
    assert!(!f.server.log().contains("private-marker"));
    let wsp = f.mock.result_for("private-body").unwrap();
    assert_eq!(
        wsp.result, record["result"],
        "WSP embeds the stored CommandResult unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_api_leases_release_matches_raw_release_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    f.mock
        .enqueue(item(&f, "action-release"), chrono::Utc::now())
        .unwrap();
    let lease = claim(&f, "action-release").await;
    let argument = format!("id={}", lease["id"].as_str().unwrap());
    let output = tokio::task::spawn_blocking({
        let mut command = f
            .home
            .command(&[], &["daemon", "api", "leases.release", &argument]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let release = f
        .mock
        .release_for(lease["server_lease_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(release.reason, awman_wsp::ReleaseReason::UserDeclined);
    assert!(f.mock.result_for("action-release").is_none());
}

struct ForegroundPresentation;
impl awman::command::worker::pick::QueueFrontend for ForegroundPresentation {
    fn pick_item(
        &mut self,
        rows: &[awman::command::daemon_api::worker_pull::QueueRow],
    ) -> Result<Option<String>, awman::command::error::CommandError> {
        Ok(rows.first().map(|row| row.id.clone()))
    }
    fn confirm_checkout(
        &mut self,
        _: &awman::data::prompt::Prompt<awman::command::worker::queue_source::CheckoutChoice>,
    ) -> awman::command::worker::queue_source::CheckoutChoice {
        awman::command::worker::queue_source::CheckoutChoice::FreshClone
    }
    fn input_available(&self) -> bool {
        true
    }
}
impl awman::command::worker::pick::QueueRunFrontendFactory for ForegroundPresentation {
    type Frontend = awman::frontend::api::command_frontend::ApiDispatchFrontend;
    fn command_frontend(
        &mut self,
        input: awman::command::dispatch::ParsedCommandBoxInput,
        events: awman::command::commands::api_server::event_bus::EventBusSender,
        writer: awman::data::fs::api_command_log::CommandStateWriter,
    ) -> Result<Self::Frontend, awman::command::error::CommandError> {
        Ok(
            awman::frontend::api::command_frontend::ApiDispatchFrontend::from_input(input, events)
                .with_state_writer(writer)
                .deferring_done(),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_pick_reuses_daemon_registration_runs_dispatch_and_reports_real_git() {
    use awman::{
        command::{dispatch::Engines, worker::pick::QueuePickCommand},
        data::{
            config::env::EnvSnapshot,
            session::{Session, SessionOpenOptions},
        },
    };
    let f = fixture(false).await;
    let mut work = item(&f, "foreground");
    work.affinity.required.push(awman_wsp::Match {
        key: "awman/host".into(),
        op: awman_wsp::MatchOp::In,
        values: vec!["daemon".into()],
    });
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let command = f
        .home
        .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let env = EnvSnapshot::with_overrides(values);
    let root = f.root.path().join("seed");
    let session = Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let pick = QueuePickCommand::new(
        session,
        engines,
        Some("team".into()),
        Some("foreground".into()),
        false,
    );
    let result = pick
        .run_with_frontend(ForegroundPresentation)
        .await
        .unwrap();
    assert_eq!(
        result.outcome,
        Some(awman::data::execution_event::CommandStatusKind::Done),
        "{result:?}\n{}",
        f.server.log()
    );
    let reported = f.mock.result_for("foreground").unwrap();
    assert_eq!(reported.result, serde_json::to_value(result).unwrap());
    let page = f.server.get("/v1/worker/leases").await;
    let lease = &page["items"][0];
    assert_eq!(lease["host"]["kind"], "tui");
    let events = f
        .mock
        .events_for(lease["server_lease_id"].as_str().unwrap());
    assert!(!events.is_empty());
    let servers = f.server.get("/v1/worker/servers").await;
    assert_eq!(servers["items"][0]["labels"]["awman/host"], "daemon");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_manual_host_runs_and_reports_when_daemon_worker_is_off_real_git() {
    use awman::{
        command::{
            dispatch::Engines,
            worker::pick::{QueueCommand, QueuePickCommand},
        },
        data::{
            config::env::EnvSnapshot,
            session::{Session, SessionOpenOptions},
        },
    };
    // Anonymous Git lets the in-process host exercise the production path
    // without re-executing this test harness as the awman credential helper.
    let f = fixture_repository(false, 1, true, None).await;
    f.home.set_agent(AgentMode::NoOp);
    let root = f.root.path().join("seed");
    f.home.write_config_exact(json!({"daemon":{"features":{"worker":{"enabled":false,"pull":{"interaction":"headless","workRoot":f.root.path().join("worker")}}}},"workServers":{"team":{"url":f._serving.base_url,"claim":"auto"}}}));
    post_fixture(&f, "/v1/config/reload", json!(null)).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if f.server.get("/v1/status").await["features"]["worker"]["state"] == "off" {
            break;
        }
        assert!(tokio::time::Instant::now() < until, "{}", f.server.log());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut work = item(&f, "manual-no-daemon");
    work.repo.credentials = None;
    work.push = None;
    work.affinity.required.push(awman_wsp::Match {
        key: "awman/host".into(),
        op: awman_wsp::MatchOp::In,
        values: vec!["tui".into()],
    });
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let command = f
        .home
        .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let session = Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides(values)),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let listing = QueueCommand::new(session.clone(), engines.clone(), Some("team".into()), true)
        .list()
        .await
        .unwrap();
    assert_eq!(listing.items[0].id, "manual-no-daemon");
    let result = QueuePickCommand::new(
        session,
        engines,
        Some("team".into()),
        Some("manual-no-daemon".into()),
        false,
    )
    .run_with_frontend(ForegroundPresentation)
    .await
    .unwrap();
    assert_eq!(
        result.outcome,
        Some(awman::data::execution_event::CommandStatusKind::Done),
        "{result:?}"
    );
    assert_eq!(
        f.mock.result_for("manual-no-daemon").unwrap().result,
        serde_json::to_value(result).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_real_workflow_snapshot_is_identical_on_sse_and_wsp_real_git() {
    let f = fixture(false).await;
    let mut work = item(&f, "snapshots");
    work.kind = awman_wsp::WorkKind::Workflow;
    work.exec.workflow_toml =
        Some("[[steps]]\nname='implement'\nagent='claude'\nprompt='implement'\n".into());
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let lease = claim(&f, "snapshots").await;
    wait(&f, || f.mock.result_for("snapshots").is_some()).await;
    let ended = wait_ended(&f, lease["id"].as_str().unwrap()).await;
    let stream = reqwest::get(format!(
        "{}/v1/worker/commands/{}/events",
        f.server.url,
        ended["job_id"].as_str().unwrap()
    ))
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    let sse: Vec<Value> = stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect();
    let wsp = f
        .mock
        .events_for(lease["server_lease_id"].as_str().unwrap());
    let snapshots = |events: &[Value]| {
        events
            .iter()
            .filter(|e| e["payload"]["type"] == "workflow_state")
            .cloned()
            .collect::<Vec<_>>()
    };
    let push = snapshots(&sse);
    let pull = snapshots(&wsp);
    assert!(
        push.len() >= 3,
        "initial, running, final snapshots: {stream}"
    );
    assert_eq!(push, pull);
    assert!(pull
        .windows(2)
        .all(|pair| pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()));
    let final_view = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}/workflow",
            ended["job_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(pull.last().unwrap()["payload"]["data"], final_view);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reported_agent_images_and_interaction_labels_match_live_policy_real_git() {
    let f = fixture(false).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let servers = f.server.get("/v1/worker/servers").await;
        if servers["items"][0]["labels"]["awman/agent.claude"] == "true" {
            break;
        }
        assert!(tokio::time::Instant::now() < until, "{servers}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut work = item(&f, "agent-label");
    work.affinity.required.push(awman_wsp::Match {
        key: "awman/agent.claude".into(),
        op: awman_wsp::MatchOp::In,
        values: vec!["true".into()],
    });
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    claim(&f, "agent-label").await;
    wait(&f, || f.mock.result_for("agent-label").is_some()).await;
    assert_eq!(
        f.mock.result_for("agent-label").unwrap().result["outcome"],
        "done"
    );
    f.home.write_config(json!({"daemon":{"features":{"worker":{"pull":{"interaction":"headless","allowOverride":false}}}},"workServers":{"team":{"url":f._serving.base_url,"claim":"manual"}}}));
    reqwest::Client::new()
        .post(format!("{}/v1/config/reload", f.server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let servers = f.server.get("/v1/worker/servers").await;
        if servers["items"][0]["labels"]
            .get("awman/interaction.interactive")
            .is_none()
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until, "{servers}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = f.server.get("/v1/status").await;
    assert!(
        !status["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("interactive")),
        "{status}"
    );
}

struct WorktreePresentation;
impl awman::command::worker::pick::QueueFrontend for WorktreePresentation {
    fn pick_item(
        &mut self,
        _: &[awman::command::daemon_api::worker_pull::QueueRow],
    ) -> Result<Option<String>, awman::command::error::CommandError> {
        Ok(None)
    }
    fn confirm_checkout(
        &mut self,
        _: &awman::data::prompt::Prompt<awman::command::worker::queue_source::CheckoutChoice>,
    ) -> awman::command::worker::queue_source::CheckoutChoice {
        awman::command::worker::queue_source::CheckoutChoice::Worktree
    }
    fn input_available(&self) -> bool {
        true
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matching_origin_pick_runs_and_publishes_from_the_users_worktree_real_git() {
    use awman::{
        command::{dispatch::Engines, worker::pick::QueuePickCommand},
        data::{
            config::env::EnvSnapshot,
            session::{Session, SessionOpenOptions},
        },
    };
    let f = fixture(false).await;
    let root = f.root.path().join("seed");
    smart_http::git(&root, &["remote", "add", "origin", &f.repo.url]);
    f.mock
        .enqueue(item(&f, "own-checkout"), chrono::Utc::now())
        .unwrap();
    let command = f
        .home
        .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let session = Session::open_at_git_root(
        root.clone(),
        root.clone(),
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides(values)),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let git = engines.git_engine.clone();
    let pick = QueuePickCommand::new(
        session,
        engines,
        Some("team".into()),
        Some("own-checkout".into()),
        false,
    );
    let (source, run) = pick.prepare(&mut WorktreePresentation).await.unwrap();
    assert_ne!(run.cwd, root);
    assert_eq!(
        git.git_common_dir(&run.cwd).unwrap(),
        git.git_common_dir(&root).unwrap()
    );
    assert!(source
        .session_for_lease(&run.lease_id)
        .unwrap()
        .inline_work_item()
        .unwrap()
        .is_file());
    let result =
        QueuePickCommand::run_prepared(source.clone(), run.clone(), &mut ForegroundPresentation)
            .await
            .unwrap();
    assert_eq!(
        result.outcome,
        Some(awman::data::execution_event::CommandStatusKind::Done),
        "{result:?}"
    );
    let result_json = serde_json::to_value(&result).unwrap();
    let sha = smart_http::git(&f.repo.bare, &["rev-parse", "refs/heads/wsp/own-checkout"]);
    assert_eq!(result_json["pushed"]["sha"], sha.trim());
    assert_eq!(
        smart_http::git(&run.cwd, &["rev-parse", "HEAD"]).trim(),
        sha.trim()
    );
    assert!(
        !root.join("agent-edit.txt").exists(),
        "the invoking branch remains separate"
    );
    let leases = f.server.get("/v1/worker/leases").await;
    assert_eq!(
        f.mock.result_for("own-checkout").unwrap().result,
        result_json
    );
    assert!(run.cwd.is_dir(), "the person's worktree remains available");
    assert!(source.session_for_lease(&run.lease_id).is_none());
    let events = f
        .mock
        .events_for(leases["items"][0]["server_lease_id"].as_str().unwrap());
    assert_eq!(events.last().unwrap()["payload"]["type"], "done");
    let row = f
        .server
        .get(&format!(
            "/v1/worker/commands/{}",
            leases["items"][0]["job_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(row["status"], "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsetting_a_server_drains_its_lease_then_deletes_the_token_real_git() {
    let f = fixture(true).await;
    f.home.set_agent(AgentMode::Slow);
    let mut work = item(&f, "unlink-running");
    work.exec.timeout_ms = Some(60_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    running_lease(&f, "unlink-running").await;
    let command = f.home.command(&[], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let env = awman::data::config::env::EnvSnapshot::with_overrides(values);
    let paths = awman::data::fs::DataPaths::from_env(&env).unwrap();
    let links = awman::data::worker::link_store::LinkStore::new(&paths);
    let output = tokio::task::spawn_blocking({
        let mut command = f
            .home
            .command(&[], &["config", "unset", "workServers.team"]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        links.read("team").unwrap().is_some(),
        "running lease retains its reporting token"
    );
    f.mock
        .enqueue(item(&f, "after-unlink"), chrono::Utc::now())
        .unwrap();
    wait(&f, || f.mock.result_for("unlink-running").is_some()).await;
    assert_eq!(
        f.mock.result_for("unlink-running").unwrap().result["outcome"],
        "done"
    );
    wait(&f, || links.read("team").unwrap().is_none()).await;
    assert!(f.mock.result_for("after-unlink").is_none());
    assert!(f.server.get("/v1/worker/leases").await["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["item_id"] != "after-unlink"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_the_whole_worker_refuses_push_and_drains_a_pulled_job_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    let mut work = item(&f, "feature-drain");
    work.exec.timeout_ms = Some(60_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let lease = claim(&f, "feature-drain").await;
    running_lease(&f, "feature-drain").await;
    f.home.write_config_exact(json!({"daemon":{"features":{"worker":{"enabled":false},"squad":{"enabled":true,"envPersistence":"none"}}},"workServers":{"team":{"url":f._serving.base_url,"claim":"auto"}}}));
    let reload = tokio::spawn({
        let url = f.server.url.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{url}/v1/config/reload"))
                .send()
                .await
                .unwrap()
        }
    });
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = f.server.get("/v1/status").await;
        if status["features"]["worker"]["state"] == "draining" {
            break;
        }
        assert!(tokio::time::Instant::now() < until, "{status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let response = reqwest::Client::new()
        .post(format!("{}/v1/worker/sessions", f.server.url))
        .json(&json!({"workdir":f.root.path().join("seed")}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "feature_disabled"
    );
    assert!(
        f.mock.result_for("feature-drain").is_none(),
        "the running lease is still finishing"
    );
    wait(&f, || f.mock.result_for("feature-drain").is_some()).await;
    assert_eq!(
        f.mock.result_for("feature-drain").unwrap().result["outcome"],
        "done"
    );
    assert!(f
        .mock
        .release_for(lease["server_lease_id"].as_str().unwrap())
        .is_none());
    assert!(reload.await.unwrap().status().is_success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_servers_each_get_one_slot_from_the_shared_worker_capacity_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    let link = f.mock.link();
    f.mock.approve_link(&link.link_id, Default::default());
    let token = awman_wsp::Client::new(&f._serving.base_url, String::new())
        .unwrap()
        .poll_link(&link.link_id)
        .await
        .unwrap()
        .unwrap();
    let command = f.home.command(&[], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let env = awman::data::config::env::EnvSnapshot::with_overrides(values);
    awman::data::worker::link_store::LinkStore::new(
        &awman::data::fs::DataPaths::from_env(&env).unwrap(),
    )
    .write("other", &token)
    .unwrap();
    f.home.write_config(json!({"daemon":{"maxConcurrentJobs":2,"features":{"worker":{"maxConcurrentJobs":2,"pull":{"interaction":"headless","workRoot":f.root.path().join("worker")}}}},"workServers":{
        "team":{"url":f._serving.base_url,"claim":"auto","maxConcurrentJobs":1,"labels":{"pool":"team"}},
        "other":{"url":f._serving.base_url,"claim":"auto","maxConcurrentJobs":1,"labels":{"pool":"other"}}
    }}));
    reqwest::Client::new()
        .post(format!("{}/v1/config/reload", f.server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    for pool in ["team", "other"] {
        for index in 0..2 {
            let mut work = item(&f, &format!("{pool}-{index}"));
            work.affinity.required.push(awman_wsp::Match {
                key: "pool".into(),
                op: awman_wsp::MatchOp::In,
                values: vec![pool.into()],
            });
            f.mock.enqueue(work, chrono::Utc::now()).unwrap();
        }
    }
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let page = f.server.get("/v1/worker/leases?state=active").await;
        let rows = page["items"].as_array().unwrap();
        if rows.len() == 2 && rows.iter().all(|l| l["state"] == "running") {
            assert_eq!(rows.iter().filter(|l| l["server"] == "team").count(), 1);
            assert_eq!(rows.iter().filter(|l| l["server"] == "other").count(), 1);
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "{page}\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(f.mock.result_for("team-0").is_none());
    assert!(f.mock.result_for("other-0").is_none());
}

async fn post_fixture(f: &Fixture, path: &str, body: Value) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", f.server.url))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "{path}: {status} {body}\n{}",
        f.server.log()
    );
    body
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn squad_push_pull_and_singleton_engine_share_one_process_and_database_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    std::fs::write(f.home.control().join("hold-leader"), "").unwrap();
    let seed = f.root.path().join("seed");
    f.home.write_config(json!({"daemon":{"maxConcurrentJobs":4,"features":{
        "worker":{"maxConcurrentJobs":2,"push":{"workDirs":{"allow":[seed]}},"pull":{"interaction":"headless","workRoot":f.root.path().join("worker")}},
        "squad":{"enabled":true,"envPersistence":"none","maxConcurrentJobs":1},
        "engine":{"enabled":true}
    }},"workServers":{"team":{"url":f._serving.base_url,"claim":"manual","maxConcurrentJobs":1}}}));
    reqwest::Client::new()
        .post(format!("{}/v1/config/reload", f.server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let instance = post_fixture(&f, "/v1/engine/instances", json!({})).await;
    assert!(instance["id"].as_str().unwrap().starts_with("ins_"));
    let task = post_fixture(&f, "/v1/squad/tasks", json!({"name":"all-features","description":"daemon gate","workspace":{"kind":"custom","path":seed},"mount_scope":"gitroot","interval_ms":3600000,"agent":"claude"})).await;
    let run = post_fixture(
        &f,
        &format!("/v1/squad/tasks/{}/runs", task["id"].as_str().unwrap()),
        json!({}),
    )
    .await;
    // A squad trigger is picked up on its established 30-second tick.
    // Wait for that independent scheduler before starting the short jobs.
    let until = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let status = f.server.get("/v1/status").await;
        if status["features"]["squad"]["running"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "squad did not start: {status}\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut work = item(&f, "gate-pull");
    work.exec.timeout_ms = Some(60_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    claim(&f, "gate-pull").await;
    running_lease(&f, "gate-pull").await;
    let pushed = post_fixture(&f, "/v1/worker/runs", json!({"session":{"workdir":seed},"command":{"subcommand":"exec prompt","args":["--headless","--yolo","--agent","claude","--","gate push"]}})).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let status = f.server.get("/v1/status").await;
        if status["features"]["worker"]["running"]
            .as_u64()
            .unwrap_or(0)
            == 2
            && status["features"]["squad"]["running"].as_u64().unwrap_or(0) >= 1
        {
            assert_eq!(status["features"]["worker"]["push"]["running"], 1);
            assert_eq!(
                status["features"]["worker"]["pull"]["servers"][0]["running"],
                1
            );
            assert_eq!(status["features"]["engine"]["state"], "on");
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "all features did not overlap: {status}\n{}",
            f.server.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let db = f.home.home().join(".local/share/awman/data/awman.db");
    let connection = rusqlite::Connection::open(db).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM commands WHERE status='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    let pulled: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM worker_leases WHERE job_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pulled, 1);
    let tasks: i64 = connection
        .query_row("SELECT COUNT(*) FROM squad_tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tasks, 1);
    post_fixture(
        &f,
        &format!("/v1/squad/runs/{}/cancel", run["id"].as_str().unwrap()),
        json!(null),
    )
    .await;
    post_fixture(
        &f,
        &format!(
            "/v1/worker/commands/{}/cancel",
            pushed["command_id"].as_str().unwrap()
        ),
        json!(null),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_pull_foreground_and_builtin_share_typed_prompt_results_real_git() {
    use awman::{
        command::{
            dispatch::{Dispatch, Engines},
            worker::pick::QueuePickCommand,
        },
        data::{
            config::env::EnvSnapshot,
            fs::api_db::CommandResult,
            session::{Session, SessionOpenOptions},
        },
    };
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::NoOp);
    let seed = f.root.path().join("seed");
    let args: Vec<String> = [
        "--headless",
        "--yolo",
        "--agent",
        "claude",
        "--",
        "same typed prompt",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let pushed = post_fixture(
        &f,
        "/v1/worker/runs",
        json!({"session":{"workdir":seed},"command":{"subcommand":"exec prompt","args":args}}),
    )
    .await;
    let pushed_id = pushed["command_id"].as_str().unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(20);
    let pushed_result: CommandResult = loop {
        let record = f
            .server
            .get(&format!("/v1/worker/commands/{pushed_id}"))
            .await;
        if !record["result"].is_null() {
            break serde_json::from_value(record["result"].clone()).unwrap();
        }
        assert!(tokio::time::Instant::now() < until, "{record}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let command = f
        .home
        .command(&[("ANTHROPIC_API_KEY", "fake-agent-key")], &[]);
    let values: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let session = Session::open_at_git_root(
        seed.clone(),
        seed.clone(),
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides(values)),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let mut manual = item(&f, "same-manual");
    manual.body = "same typed prompt".into();
    manual.push = None;
    f.mock.enqueue(manual, chrono::Utc::now()).unwrap();
    let manual = QueuePickCommand::new(
        session.clone(),
        engines.clone(),
        Some("team".into()),
        Some("same-manual".into()),
        false,
    )
    .run_with_frontend(ForegroundPresentation)
    .await
    .unwrap();
    let paths = awman::data::fs::api_paths::ApiPaths::from_env(session.env()).unwrap();
    let sid = awman::data::ids::WorkerSessionId::new().to_string();
    let cid = awman::data::ids::CommandId::new().to_string();
    paths.prepare_command_dir(&sid, &cid).unwrap();
    let writer = awman::data::fs::api_command_log::CommandStateWriter::new(paths, &sid, &cid);
    let bus = awman::command::commands::api_server::event_bus::EventBus::new(4096);
    let frontend = awman::frontend::api::command_frontend::ApiDispatchFrontend::new(
        "exec prompt",
        &args,
        bus.sender(),
    )
    .with_state_writer(writer.clone())
    .deferring_done();
    let git = engines.git_engine.clone();
    let base = git.command_base_sha(&seed);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session)),
        engines,
    )
    .with_output_dir(writer.output_dir());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(handle.block_on(dispatch.run_command(&["exec", "prompt"])));
        })
        .unwrap();
    let outcome = rx.await.unwrap();
    let mut builtin = awman::command::commands::api_server::queue_worker::foreground_result(
        &outcome,
        writer.result_contributions(),
    );
    builtin.git = git.command_result(&seed, base.as_deref()).unwrap();
    builtin.outputs = Some(awman::data::fs::command_outputs::summarize_output_dir(
        &writer.output_dir(),
    ));
    let output = tokio::task::spawn_blocking({
        let mut command = f
            .home
            .command(&[], &["config", "set", "workServers.team.claim", "auto"]);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut automatic = item(&f, "same-auto");
    automatic.body = "same typed prompt".into();
    automatic.push = None;
    f.mock.enqueue(automatic, chrono::Utc::now()).unwrap();
    wait(&f, || f.mock.result_for("same-auto").is_some()).await;
    let pull: CommandResult =
        serde_json::from_value(f.mock.result_for("same-auto").unwrap().result).unwrap();
    // Repository identities and legacy explicit-null setup metadata differ by
    // host. All command outcome, failure, output and message fields must agree.
    let normalize = |mut result: CommandResult| {
        result.git = None;
        result.setup_error = None;
        result
    };
    let expected = normalize(pushed_result);
    assert_eq!(
        expected.outcome,
        Some(awman::data::execution_event::CommandStatusKind::Done)
    );
    assert_eq!(normalize(manual), expected);
    assert_eq!(normalize(builtin), expected);
    assert_eq!(normalize(pull), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_terminal_proxy_relays_output_and_enforces_live_input_policy_real_git() {
    use awman::{
        command::{
            dispatch::Engines,
            worker::{pick::QueuePickCommand, queue_source::LeaseCloseChoice},
        },
        data::{
            config::env::EnvSnapshot,
            session::{Session, SessionOpenOptions},
            terminal::*,
        },
        engine::terminal_hub::{TerminalInput, TerminalRegistration},
    };
    use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame as WireFrame};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let f = fixture(false).await;
    f.home.cli_ok(&[
        "config",
        "set",
        "daemon.features.worker.pull.interaction",
        "interactive",
    ]);
    post_fixture(&f, "/v1/config/reload", json!(null)).await;
    let mut work = item(&f, "foreground-terminal");
    work.exec.interaction = Some(awman_wsp::Interaction::Interactive);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    f.mock.set_terminal("foreground-terminal", true, true);
    let command = f.home.command(&[], &[]);
    let env = EnvSnapshot::with_overrides(command.get_envs().filter_map(|(k, v)| {
        v.map(|v| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
    }));
    let root = f.root.path().join("seed");
    let session = Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let (source, run) = QueuePickCommand::new(
        session,
        engines,
        Some("team".into()),
        Some("foreground-terminal".into()),
        true,
    )
    .prepare(&mut ForegroundPresentation)
    .await
    .unwrap();
    let resource = f
        .server
        .get(&format!("/v1/worker/leases/{}", run.lease_id))
        .await;
    let upstream = resource["server_lease_id"].as_str().unwrap();
    let hub = source
        .engines_for_lease(&run.lease_id)
        .unwrap()
        .runtime
        .terminal_hub()
        .unwrap();
    let (stdin, mut received) = tokio::sync::mpsc::channel(8);
    let terminal = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(
                    awman::data::ids::CommandId::parse(resource["job_id"].as_str().unwrap())
                        .unwrap(),
                ),
                feature: awman::data::daemon_state::FeatureName::Worker,
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
    wait(&f, || {
        f.mock
            .terminal_frames(upstream)
            .iter()
            .any(|f| matches!(f.frame, WireFrame::Snapshot { .. }))
    })
    .await;
    terminal.push_output(b"foreground terminal output");
    wait(&f, || f.mock.terminal_frames(upstream).iter().any(|f| matches!(&f.frame,WireFrame::Output{bytes} if STANDARD.decode(bytes).unwrap()==b"foreground terminal output"))).await;
    let input = || LeaseTerminalFrame {
        terminal_id: terminal.id().to_string(),
        frame: WireFrame::Input {
            bytes: STANDARD.encode("approved input"),
        },
    };
    f.mock.send_terminal(upstream, input()).unwrap();
    wait(&f, || {
        f.mock
            .terminal_frames(upstream)
            .iter()
            .any(|f| matches!(&f.frame,WireFrame::Error{code,..} if code=="input_refused"))
    })
    .await;
    assert!(received.try_recv().is_err());
    f.home
        .cli_ok(&["config", "set", "daemon.terminal.remoteInput", "true"]);
    post_fixture(&f, "/v1/config/reload", json!(null)).await;
    f.mock.send_terminal(upstream, input()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap(),
        b"approved input"
    );
    terminal.exit(0);
    wait(&f, || {
        f.mock
            .terminal_frames(upstream)
            .iter()
            .any(|f| matches!(f.frame, WireFrame::Exit { code: Some(0) }))
    })
    .await;
    source
        .finish_lease(&run.lease_id, LeaseCloseChoice::Release)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashed_foreground_owner_releases_lease_and_stops_container_real_git() {
    let f = fixture(false).await;
    f.home.set_agent(AgentMode::Slow);
    std::fs::write(f.home.control().join("slow.seconds"), "120").unwrap();
    let mut work = item(&f, "crashed-owner");
    work.exec.timeout_ms = Some(120_000);
    f.mock.enqueue(work, chrono::Utc::now()).unwrap();
    let mut command = f.home.command(
        &[("ANTHROPIC_API_KEY", "fake-agent-key")],
        &["queue", "pick", "crashed-owner", "--json"],
    );
    command.current_dir(f.root.path().join("seed"));
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let guard = Kill(&mut child);
    wait(&f, || f.home.control().join("slow.pid").exists()).await;
    let leases = f.server.get("/v1/worker/leases").await;
    let upstream = leases["items"][0]["server_lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    guard.0.kill().unwrap();
    guard.0.wait().unwrap();
    // Poll the mock, not the local lease resource: local GET is the owner heartbeat.
    wait(&f, || f.mock.release_for(&upstream).is_some()).await;
    let release = f.mock.release_for(&upstream).unwrap();
    assert_eq!(release.reason, awman_wsp::ReleaseReason::Error);
    assert!(release.message.unwrap().contains("owner disconnected"));
    assert!(f.mock.result_for("crashed-owner").is_none());
    let log = std::fs::read_to_string(f.home.control().join("docker.log")).unwrap();
    assert!(
        log.lines().any(|line| line.starts_with("docker stop ")),
        "{log}"
    );
}
