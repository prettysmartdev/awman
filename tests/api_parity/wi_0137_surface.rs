//! WI 0137 API and engine surface regression tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use awman::command::commands::api_server::{CommandCanceller, TokenRegistry};
use awman::command::dispatch::Engines;
use awman::data::agent_output_format::AgentOutputFormat;
use awman::data::execution_event::{EventPayload, EventSource, EventSourcePhase, ExecutionEvent};
use awman::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session::SessionKind;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::workflow_state::{WorkflowPhase, WorkflowState};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::{build_router, AppState, AuthMode, API_FEATURES};
use axum::http::header::CONTENT_TYPE;

const SESSION: &str = "surface-session";

fn state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().unwrap();
    let store = Arc::new(SqliteSessionStore::open(paths.root()).unwrap());
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        config_notices: Default::default(),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    let lifecycle_store = awman::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
    let setup_cancels = Arc::new(TokenRegistry::new());
    let canceller = CommandCanceller::new(
        Arc::clone(&store),
        lifecycle_store.clone(),
        Arc::new(TokenRegistry::new()),
        Arc::clone(&setup_cancels),
    );
    store
        .insert_session_full(NewSessionRow {
            id: SESSION,
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    Arc::new(AppState {
        lifecycle_store,
        setup_cancels,
        canceller,
        store,
        paths,
        workdirs: vec![],
        workdir_roots: vec![],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

async fn server(state: Arc<AppState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    format!("http://{addr}")
}

fn command(state: &AppState, id: &str) -> std::path::PathBuf {
    state.paths.prepare_command_dir(SESSION, id).unwrap();
    state
        .store
        .enqueue_command(
            id,
            SESSION,
            "exec prompt",
            "[]",
            &state.paths.command_log_path(SESSION, id).to_string_lossy(),
        )
        .unwrap();
    state.paths.command_output_dir(SESSION, id)
}

#[tokio::test]
async fn outputs_routes_list_fetch_headers_and_reject_unsafe_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    let output = command(&app, "outputs");
    std::fs::create_dir_all(output.join("nested")).unwrap();
    std::fs::write(output.join("nested/report.json"), b"{\"ok\":true}").unwrap();
    std::fs::write(output.join("plain.unknown"), b"data").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("/etc/passwd", output.join("passwd-link")).unwrap();
        symlink("/etc", output.join("host-dir")).unwrap();
        nix::unistd::mkfifo(&output.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    }
    let base = server(app.clone()).await;
    let client = reqwest::Client::new();
    let listing: serde_json::Value = client
        .get(format!("{base}/v1/commands/outputs/outputs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths: Vec<_> = listing["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["nested/report.json", "plain.unknown"]);
    assert_eq!(listing["total"]["files"], 2);

    let response = client
        .get(format!(
            "{base}/v1/commands/outputs/outputs/nested/report.json"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(response.bytes().await.unwrap(), "{\"ok\":true}");
    let unknown_type = client
        .get(format!("{base}/v1/commands/outputs/outputs/plain.unknown"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unknown_type.headers()[CONTENT_TYPE],
        "application/octet-stream"
    );

    #[cfg(unix)]
    for path in ["passwd-link", "host-dir/passwd"] {
        assert_eq!(
            client
                .get(format!("{base}/v1/commands/outputs/outputs/{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn output_handler_returns_400_for_dot_absolute_and_empty_segments() {
    use axum::extract::{Path as AxumPath, State};
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    for path in ["../etc/passwd", "/etc/passwd", "nested//file", ""] {
        let response = awman::frontend::api::outputs::handle_get_output(
            State(app.clone()),
            AxumPath(("missing-command".to_owned(), path.to_owned())),
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "{path:?}"
        );
    }
}

#[test]
fn prompt_output_directory_clears_on_rerun_and_workflow_preparation_keeps_files() {
    use awman::data::fs::command_outputs::prepare_output_dir;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("outputs");
    prepare_output_dir(&dir, true).unwrap();
    std::fs::write(dir.join("old.json"), "old").unwrap();
    prepare_output_dir(&dir, true).unwrap();
    assert!(
        !dir.join("old.json").exists(),
        "exec prompt re-run clears prior output"
    );
    std::fs::write(dir.join("kept.json"), "kept").unwrap();
    prepare_output_dir(&dir, false).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("kept.json")).unwrap(),
        "kept"
    );
}

#[tokio::test]
async fn outputs_route_lists_but_does_not_serve_files_over_16_mib() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    let output = command(&app, "large-output");
    std::fs::create_dir_all(&output).unwrap();
    let file = std::fs::File::create(output.join("large.bin")).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    let base = server(app).await;
    let client = reqwest::Client::new();
    let listing: serde_json::Value = client
        .get(format!("{base}/v1/commands/large-output/outputs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["total"]["bytes"], 16 * 1024 * 1024 + 1);
    let response = client
        .get(format!("{base}/v1/commands/large-output/outputs/large.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

fn log_event(sequence: u64, step: &str, index: usize, text: &str) -> ExecutionEvent {
    ExecutionEvent {
        timestamp: chrono::Utc::now(),
        attempt: 1,
        sequence,
        source: Some(
            EventSource::step(EventSourcePhase::Main, step, index)
                .with_container(format!("container-{step}")),
        ),
        payload: EventPayload::StdoutLine(text.into()),
    }
}

#[tokio::test]
async fn step_filter_applies_to_replay_and_resumes_from_its_global_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    command(&app, "step-filter");
    let mut writer = awman::data::fs::api_command_log::CommandLogWriter::open_append(
        &app.paths,
        SESSION,
        "step-filter",
    )
    .await
    .unwrap();
    for event in [
        log_event(0, "alpha", 0, "alpha first"),
        log_event(1, "beta", 1, "beta hidden"),
        log_event(2, "alpha", 0, "alpha second"),
    ] {
        writer.write_event(&event).await;
    }
    let done = ExecutionEvent {
        timestamp: chrono::Utc::now(),
        attempt: 1,
        sequence: 3,
        source: None,
        payload: EventPayload::Done,
    };
    writer.write_event(&done).await;
    writer.flush().await;
    app.store
        .complete_command("step-filter", "done", Some(0), None)
        .unwrap();
    let base = server(app).await;
    let client = reqwest::Client::new();
    let first = client
        .get(format!("{base}/v1/commands/step-filter/logs?step=alpha"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(first.contains("alpha first"));
    assert!(first.contains("alpha second"));
    assert!(!first.contains("beta hidden"));
    assert!(
        first.contains("id: 1:2"),
        "global cursor is retained across skipped beta event: {first}"
    );
    assert!(first.contains("event: done"));

    let resumed = client
        .get(format!("{base}/v1/commands/step-filter/logs?step=alpha"))
        .header("Last-Event-ID", "1:2")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!resumed.contains("alpha first"));
    assert!(!resumed.contains("beta hidden"));
    assert!(resumed.contains("event: done"));
}

#[tokio::test]
async fn step_filter_applies_to_live_bus_events_and_keeps_terminal_events() {
    use awman::command::commands::api_server::event_bus::EventBus;
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    command(&app, "live-step-filter");
    let bus = Arc::new(EventBus::for_attempt(32, 1));
    app.event_buses
        .lock()
        .await
        .insert("live-step-filter".into(), bus.clone());
    let base = server(app.clone()).await;
    let response = reqwest::Client::new()
        .get(format!(
            "{base}/v1/commands/live-step-filter/logs?step=alpha"
        ))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let alpha = bus
        .sender()
        .with_source(EventSource::step(EventSourcePhase::Main, "alpha", 0));
    let beta = bus
        .sender()
        .with_source(EventSource::step(EventSourcePhase::Main, "beta", 1));
    alpha.emit(EventPayload::StdoutLine("live alpha".into()));
    beta.emit(EventPayload::StdoutLine("live beta hidden".into()));
    app.store
        .complete_command("live-step-filter", "done", Some(0), None)
        .unwrap();
    bus.sender().emit(EventPayload::Done);
    let body = tokio::time::timeout(std::time::Duration::from_secs(3), response.text())
        .await
        .unwrap()
        .unwrap();
    assert!(body.contains("live alpha"), "{body}");
    assert!(!body.contains("live beta hidden"), "{body}");
    assert!(
        body.contains("event: done"),
        "filtered stream must retain terminal marker: {body}"
    );
}

#[tokio::test]
async fn workflow_alias_matches_canonical_route_and_status_is_snake_case() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    command(&app, "workflow-alias");
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::StepState;
    let step = WorkflowStep {
        name: "done-step".into(),
        depends_on: vec![],
        prompt_template: "ok".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    let mut workflow = WorkflowState::new("wf".into(), &[step], "hash".into(), None);
    workflow.current_phase = WorkflowPhase::Done;
    workflow
        .step_states
        .insert("done-step".into(), StepState::Succeeded);
    app.paths
        .write_command_workflow_state(SESSION, "workflow-alias", &workflow)
        .unwrap();
    let base = server(app).await;
    let client = reqwest::Client::new();
    let a = client
        .get(format!("{base}/v1/workflows/workflow-alias"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let b = client
        .get(format!("{base}/v1/commands/workflow-alias/workflow"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(a, b);
    let view: serde_json::Value = serde_json::from_slice(&a).unwrap();
    assert_eq!(view["phase"], "done");
    assert_eq!(view["steps"][0]["status"], "succeeded");
}

#[test]
fn api_features_advertise_wi_0137_surface() {
    for feature in [
        "structured_results",
        "outputs",
        "event_source",
        "step_activity",
    ] {
        assert!(API_FEATURES.contains(&feature), "missing {feature}");
    }
}

#[test]
fn structured_translator_fixtures_emit_bounded_activity_and_final_message() {
    use awman::data::execution_event::STEP_ACTIVITY_SUMMARY_MAX_CHARS;
    use awman::engine::agent_output::{translator_for, OutputStream, Translated};

    let fixtures = [
        (
            "claude_stream_json.ndjson",
            AgentOutputFormat::ClaudeStreamJson,
            7,
        ),
        ("codex_json.ndjson", AgentOutputFormat::CodexJson, 9),
    ];
    for (fixture, format, activity_count) in fixtures {
        let contents = match fixture {
            "claude_stream_json.ndjson" => {
                include_str!("../../src/engine/agent_output/testdata/claude_stream_json.ndjson")
            }
            _ => include_str!("../../src/engine/agent_output/testdata/codex_json.ndjson"),
        };
        let mut translator = translator_for(format);
        let mut translated = Vec::new();
        for line in contents.lines() {
            translated.extend(translator.feed_line(OutputStream::Stdout, line));
        }
        translated.extend(translator.finish());
        let activities: Vec<_> = translated
            .iter()
            .filter_map(|item| match item {
                Translated::Activity { kind, summary } => {
                    assert!(summary.chars().count() <= STEP_ACTIVITY_SUMMARY_MAX_CHARS);
                    assert!(!summary.contains(['\n', '\r']));
                    Some(*kind)
                }
                _ => None,
            })
            .collect();
        assert_eq!(activities.len(), activity_count, "{fixture}");
        use awman::data::execution_event::StepActivityKind::{
            Message, Thinking, ToolResult, ToolUse,
        };
        let expected = if format == AgentOutputFormat::ClaudeStreamJson {
            vec![
                Thinking, Message, ToolUse, ToolResult, ToolUse, ToolResult, Message,
            ]
        } else {
            vec![
                Thinking, Message, ToolUse, ToolResult, ToolResult, ToolUse, ToolResult,
                ToolResult, Message,
            ]
        };
        assert_eq!(activities, expected, "{fixture}");
        let all_lines: Vec<_> = translated
            .iter()
            .filter_map(|item| match item {
                Translated::Line { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(all_lines.len(), 3, "no raw JSON frames: {all_lines:?}");
        for payload in [
            "fn main",
            "aggregated_output",
            "all checks passed",
            "Finished `dev`",
        ] {
            assert!(
                !format!("{translated:?}").contains(payload),
                "tool payload {payload} escaped the translator"
            );
        }

        assert!(translated.iter().any(|item| matches!(item,
            Translated::Line { text, .. } if text.starts_with("malformed:"))));
        let final_messages: Vec<_> = translated
            .iter()
            .filter_map(|item| match item {
                Translated::FinalMessage(message) => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            final_messages,
            ["Implemented structured output.\nValidation: cargo check passed."],
            "{fixture}"
        );
        let final_lines: Vec<_> = translated
            .iter()
            .filter_map(|item| match item {
                Translated::Line { text, .. }
                    if text.starts_with("Implemented") || text.starts_with("Validation:") =>
                {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            final_lines,
            [
                "Implemented structured output.",
                "Validation: cargo check passed."
            ],
            "{fixture}"
        );
    }

    let mut text = translator_for(AgentOutputFormat::Text);
    assert!(text
        .feed_line(OutputStream::Stdout, "plain text")
        .iter()
        .all(|item| matches!(item, Translated::Line { .. })));
    assert!(text.finish().is_empty());
}

#[test]
fn workflow_stage_depth_groups_linear_fanout_fanin_and_excludes_phase_steps() {
    use awman::data::workflow_definition::WorkflowStep;
    fn step(name: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            name: name.into(),
            depends_on: deps.iter().map(|s| (*s).into()).collect(),
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }
    let cases = [
        (
            vec![step("a", &[]), step("b", &["a"])],
            vec![vec!["a"], vec!["b"]],
        ),
        (
            vec![
                step("a", &[]),
                step("b", &[]),
                step("c", &["a", "b"]),
                step("d", &[]),
            ],
            vec![vec!["a", "b", "d"], vec!["c"]],
        ),
    ];
    for (steps, expected) in cases {
        let state = WorkflowState::new("wf".into(), &steps, "hash".into(), None);
        let stages = state.stages();
        assert_eq!(stages, expected);
    }
}

#[test]
fn api_workflow_view_stages_match_shared_tui_grouping_with_setup_and_teardown() {
    use awman::data::execution_event::{CommandStatusKind, StepStatusKind};
    use awman::data::workflow_definition::WorkflowStep;
    use awman::data::workflow_state::{PhaseStepState, PhaseStepStatus, StepState};
    let steps = vec![
        WorkflowStep {
            name: "left".into(),
            depends_on: vec![],
            prompt_template: "".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        },
        WorkflowStep {
            name: "right".into(),
            depends_on: vec![],
            prompt_template: "".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        },
        WorkflowStep {
            name: "join".into(),
            depends_on: vec!["left".into(), "right".into()],
            prompt_template: "".into(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        },
    ];
    let mut state = WorkflowState::new("wf".into(), &steps, "hash".into(), None);
    state.setup_step_states = vec![PhaseStepState {
        description: "prepare".into(),
        status: PhaseStepStatus::Succeeded,
    }];
    state.teardown_step_states = vec![PhaseStepState {
        description: "cleanup".into(),
        status: PhaseStepStatus::Pending,
    }];
    state
        .step_states
        .insert("left".into(), StepState::Succeeded);
    let json = serde_json::to_value(&state).unwrap();
    let client = awman::frontend::api::workflow_view::client_view(json);
    assert_eq!(
        client["stages"],
        serde_json::json!([["left", "right"], ["join"]])
    );
    assert_eq!(client["setup_step_states"][0]["state"], "succeeded");
    assert_eq!(client["teardown_step_states"][0]["state"], "pending");
    assert_eq!(
        client["steps"][0]["status"],
        serde_json::to_value(StepStatusKind::Succeeded).unwrap()
    );
    assert_eq!(
        serde_json::to_value(CommandStatusKind::Error).unwrap(),
        "error"
    );
}

#[tokio::test]
async fn parallel_output_emitters_keep_step_container_and_retry_attribution() {
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::data::execution_event::{EventPayload, EventSource, EventSourcePhase};
    let bus = EventBus::new(32);
    let mut rx = bus.subscribe();
    let alpha = bus.sender().with_source(
        EventSource::step(EventSourcePhase::Main, "alpha", 0)
            .with_container("container-alpha")
            .with_step_attempt(2),
    );
    let beta = bus.sender().with_source(
        EventSource::step(EventSourcePhase::Main, "beta", 1)
            .with_container("container-beta")
            .with_step_attempt(1),
    );
    alpha.emit(EventPayload::StdoutLine("a1".into()));
    beta.emit(EventPayload::StdoutLine("b1".into()));
    alpha.emit(EventPayload::StdoutLine("a2".into()));
    for expected in [
        ("alpha", 0, "container-alpha", 2),
        ("beta", 1, "container-beta", 1),
        ("alpha", 0, "container-alpha", 2),
    ] {
        let event = rx.recv().await.unwrap();
        assert!(matches!(event.payload, EventPayload::StdoutLine(_)));
        let source = event.source.unwrap();
        assert_eq!(source.step_name.as_deref(), Some(expected.0));
        assert_eq!(source.step_index, Some(expected.1));
        assert_eq!(source.container.as_deref(), Some(expected.2));
        assert_eq!(source.step_attempt, Some(expected.3));
    }
    let leader = bus
        .sender()
        .with_source(EventSource::phase(EventSourcePhase::Leader));
    leader.emit(EventPayload::StdoutLine("leader".into()));
    let source = rx.recv().await.unwrap().source.unwrap();
    assert_eq!(source.phase, EventSourcePhase::Leader);
    assert!(source.step_name.is_none());
}

#[test]
fn workflow_attempt_counter_survives_restore_and_retry() {
    use awman::data::workflow_definition::WorkflowStep;
    let step = WorkflowStep {
        name: "retry-me".into(),
        depends_on: vec![],
        prompt_template: "".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    let mut state = WorkflowState::new("wf".into(), &[step], "hash".into(), None);
    state.record_step_launch("retry-me");
    let restored: WorkflowState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(restored.step_attempt("retry-me"), Some(1));
    let mut resumed = restored;
    assert_eq!(resumed.record_step_launch("retry-me"), 2);
    assert_eq!(resumed.step_attempt("retry-me"), Some(2));
}

#[tokio::test]
async fn active_outputs_are_downloads_with_a_scriptless_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    let output = command(&app, "active");
    std::fs::create_dir_all(&output).unwrap();
    let base = server(app).await;
    for path in ["report.html", "report.svg"] {
        std::fs::write(output.join(path), "<script>fetch('/v1/sessions')</script>").unwrap();
        let response = reqwest::get(format!("{base}/v1/commands/active/outputs/{path}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-disposition"], "attachment");
        assert_eq!(
            response.headers()["content-security-policy"],
            "sandbox; default-src 'none'"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
}

#[tokio::test]
async fn json_log_snapshots_apply_the_step_filter_without_renumbering() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    command(&app, "snapshot");
    let mut writer = awman::data::fs::api_command_log::CommandLogWriter::open_append(
        &app.paths, SESSION, "snapshot",
    )
    .await
    .unwrap();
    for (sequence, step) in [(0, "alpha"), (1, "beta"), (2, "alpha")] {
        writer
            .write_event(&ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: 1,
                sequence,
                source: Some(EventSource::step(EventSourcePhase::Main, step, 0)),
                payload: EventPayload::StdoutLine(step.into()),
            })
            .await;
    }
    writer.flush().await;
    let base = server(app).await;
    let json: serde_json::Value = reqwest::get(format!(
        "{base}/v1/commands/snapshot/logs?format=json&step=alpha"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        json["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["sequence"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 2]
    );
}

#[tokio::test]
async fn terminal_row_waits_for_the_actual_final_pair_and_preserves_resume() {
    use awman::data::execution_event::CommandStatusKind;
    use awman::data::fs::api_command_log::CommandLogWriter;
    use awman::data::fs::api_db::CommandResult;
    for step in ["", "?step=alpha"] {
        let tmp = tempfile::tempdir().unwrap();
        let app = state(tmp.path());
        command(&app, "race");
        let result = CommandResult {
            outcome: Some(CommandStatusKind::Done),
            exit_code: Some(0),
            ..Default::default()
        };
        app.store
            .complete_command(
                "race",
                "done",
                Some(0),
                Some(&serde_json::to_string(&result).unwrap()),
            )
            .unwrap();
        let mut writer = CommandLogWriter::open_append(&app.paths, SESSION, "race")
            .await
            .unwrap();
        writer
            .write_event(&ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: 1,
                sequence: 0,
                source: Some(EventSource::step(EventSourcePhase::Main, "alpha", 0)),
                payload: EventPayload::StdoutLine("hello".into()),
            })
            .await;
        writer.flush().await;
        let base = server(app).await;
        let url = format!("{base}/v1/commands/race/logs{step}");
        let request_url = url.clone();
        let request = tokio::spawn(async move {
            reqwest::get(request_url)
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !request.is_finished(),
            "the terminal row alone cannot close the stream"
        );
        writer
            .write_event(&ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: 1,
                sequence: 1,
                source: None,
                payload: EventPayload::CommandStatus {
                    status: CommandStatusKind::Done,
                    exit_code: Some(0),
                    error: None,
                    result: Some(Box::new(result.clone())),
                },
            })
            .await;
        writer
            .write_event(&ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: 1,
                sequence: 2,
                source: None,
                payload: EventPayload::Done,
            })
            .await;
        writer.flush().await;
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body.matches("event: command_status").count(), 1, "{body}");
        assert!(body.contains("id: 1:1\nevent: command_status"), "{body}");
        assert!(body.contains("id: 1:2\nevent: done"), "{body}");
        let resumed = reqwest::Client::new()
            .get(&url)
            .header("Last-Event-ID", "1:0")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(
            resumed.matches("event: command_status").count(),
            1,
            "{resumed}"
        );
    }
}

#[tokio::test]
async fn oversized_sparse_listing_returns_busy_without_hashing_the_entire_file() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    let output = command(&app, "sparse");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::File::create(output.join("huge"))
        .unwrap()
        .set_len(1 << 40)
        .unwrap();
    let base = server(app).await;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reqwest::get(format!("{base}/v1/commands/sparse/outputs")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 503);
}

#[tokio::test]
async fn a_terminal_row_without_a_writable_log_recovers_without_minting_cursor_ids() {
    use awman::data::execution_event::CommandStatusKind;
    use awman::data::fs::api_db::CommandResult;
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    command(&app, "no-log");
    let result = CommandResult {
        outcome: Some(CommandStatusKind::Error),
        exit_code: Some(1),
        ..Default::default()
    };
    app.store
        .complete_command(
            "no-log",
            "error",
            Some(1),
            Some(&serde_json::to_string(&result).unwrap()),
        )
        .unwrap();
    let base = server(app).await;
    let body = tokio::time::timeout(std::time::Duration::from_secs(7), async {
        reqwest::get(format!("{base}/v1/commands/no-log/logs"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(body.matches("event: command_status").count(), 1, "{body}");
    assert_eq!(body.matches("event: done").count(), 1, "{body}");
    assert!(
        !body.contains("id:"),
        "recovery notifications cannot consume persisted ids: {body}"
    );
}
