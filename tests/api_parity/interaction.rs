//! Per-feature interaction over `POST /v1/worker/commands` (WI 0141).
//!
//! Push defaults to `headless` (today's behaviour); `exec prompt` and `exec
//! workflow` accept `--interactive` (or the body's `"interaction"`) when
//! `daemon.features.worker.push.allowOverride` allows it, and an override it
//! forbids is refused at submit with `400 interaction_override_refused`. An
//! interactive command's frontend asks the runtime for a PTY of
//! `daemon.terminal.{cols,rows}`; a headless one stays piped.

use std::sync::Arc;

use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::commands::api_server::queue_worker::push_run_interaction;
use awman::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;
use awman::command::dispatch::CommandFrontend;
use awman::data::config::daemon::Interaction;
use awman::data::config::EffectiveConfig;
use awman::data::fs::api_command_log::CommandStateWriter;
use awman::engine::agent_runtime::frontend::AgentFrontend;
use awman::frontend::api::routes::AppState;
use awman::frontend::api::ApiFrontendFactory;

const SESSION: &str = "ses_4c6906233871615c8475a306c8dea09b";

/// Make `daemon` the worker's live config, as a reload would.
fn set_live_daemon(state: &AppState, daemon: serde_json::Value) {
    let global: awman::data::config::ConfigFile =
        serde_json::from_value(serde_json::json!({ "daemon": daemon })).unwrap();
    let config = EffectiveConfig::from_files(
        global,
        Default::default(),
        Default::default(),
        awman::data::config::env::EnvSnapshot::empty(),
    )
    .unwrap();
    *state.live_config.write().unwrap() = Some(config);
}

async fn submit(addr: std::net::SocketAddr, body: serde_json::Value) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/worker/commands"))
        .header("x-awman-session", SESSION)
        .json(&body)
        .send()
        .await
        .expect("POST /v1/worker/commands");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

/// What the queue worker does with a claimed row.
fn run_decision(
    state: &AppState,
    row: &awman::data::fs::api_db::CommandRecord,
) -> awman::command::interaction::InteractionDecision {
    let settings = state
        .live_config
        .read()
        .unwrap()
        .as_ref()
        .map(|c| c.daemon())
        .unwrap_or_else(|| {
            awman::data::config::daemon::DaemonSettings::from_merged(&Default::default())
        });
    push_run_interaction(row, &settings)
}

#[tokio::test]
async fn real_network_pushed_interactive_runs_in_a_pty_sized_from_config() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    set_live_daemon(
        &state,
        serde_json::json!({"terminal": {"cols": 160, "rows": 48}}),
    );
    let Some((addr, server)) = super::spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let (status, body) = submit(
        addr,
        serde_json::json!({"subcommand": "exec workflow", "args": ["--dynamic", "--interactive", "do it"]}),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["flags_applied"]["headless"], false, "{body}");
    let row = state
        .store
        .get_command(body["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let decision = run_decision(&state, &row);
    assert_eq!(decision.interaction, Interaction::Interactive);

    let mut frontend = ApiFrontendFactory.frontend_for(
        &row,
        EventBus::new(8).sender(),
        CommandStateWriter::new(state.paths.clone(), &row.session_id, &row.id),
        decision,
    );
    assert_eq!(
        frontend
            .flag_bool(&["exec", "workflow"], "headless")
            .unwrap(),
        Some(false)
    );
    let io = frontend.take_io();
    assert_eq!(
        io.initial_size,
        Some((160, 48)),
        "an interactive push takes spawn_pty_bridged at daemon.terminal's size"
    );
    server.abort();
}

#[tokio::test]
async fn real_network_push_defaults_to_headless_and_stays_piped() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let Some((addr, server)) = super::spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let (status, body) = submit(
        addr,
        serde_json::json!({"subcommand": "exec prompt", "args": ["hello"]}),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["flags_applied"]["headless"], true, "{body}");
    let id = body["id"].as_str().unwrap();
    let row = state.store.get_command(id).unwrap().unwrap();
    assert_eq!(row.decode_args().unwrap(), vec!["--headless", "hello"]);
    // The recorded decision is the server's, not the client's argv.
    let shown: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{addr}/v1/worker/commands/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(shown["args"], serde_json::json!(["hello"]));

    let decision = run_decision(&state, &row);
    let mut frontend = ApiFrontendFactory.frontend_for(
        &row,
        EventBus::new(8).sender(),
        CommandStateWriter::new(state.paths.clone(), &row.session_id, &row.id),
        decision,
    );
    let io = frontend.take_io();
    assert_eq!(io.initial_size, None, "headless runs stay piped");
    assert!(io.resize.is_none());
    server.abort();
}

#[tokio::test]
async fn real_network_override_is_refused_when_push_disallows_it() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    set_live_daemon(
        &state,
        serde_json::json!({"features": {"worker": {"push": {
            "interaction": "headless", "allowOverride": false
        }}}}),
    );
    let Some((addr, server)) = super::spawn_router(Arc::clone(&state)).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    for body in [
        serde_json::json!({"subcommand": "exec prompt", "args": ["--interactive", "hello"]}),
        serde_json::json!({"subcommand": "exec prompt", "args": ["hello"], "interaction": "interactive"}),
    ] {
        let (status, response) = submit(addr, body).await;
        assert_eq!(status, 400, "{response}");
        assert_eq!(
            response["code"], "interaction_override_refused",
            "{response}"
        );
        assert_eq!(
            response["setting"],
            "daemon.features.worker.push.interaction"
        );
        assert_eq!(response["configured"], "headless");
        assert_eq!(response["requested"], "interactive");
    }
    assert!(state
        .store
        .list_commands_filtered(&awman::data::fs::api_db::CommandListFilter {
            status: None,
            session_id: None,
            labels: &[],
            limit: i64::MAX,
        })
        .unwrap()
        .is_empty());
    // Asking for the configured mode is not an override.
    let (status, response) = submit(
        addr,
        serde_json::json!({"subcommand": "exec prompt", "args": ["--headless", "hello"]}),
    )
    .await;
    assert_eq!(status, 202, "{response}");
    server.abort();
}
