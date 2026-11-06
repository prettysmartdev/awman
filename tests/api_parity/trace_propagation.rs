use awman::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;
use awman::command::daemon_api::DaemonApi;
use awman::command::dispatch::CommandFrontend;
use awman::data::daemon_state::CommandListener;
use awman::frontend::api::routes::{build_router_for_listener, ListenerPolicy};
use std::sync::Arc;

#[tokio::test]
async fn traceparent_and_listener_are_stored_for_network_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = super::http_consistency::state(root.path());
    let api = Arc::new(DaemonApi::for_worker_tests(state.clone()));
    let (_, key) = api.runtime.keys().create("trace-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(
        axum::serve(
            listener,
            build_router_for_listener(api, ListenerPolicy::Network),
        )
        .into_future(),
    );
    let client = reqwest::Client::new();
    let trace = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    for (header, expected) in [
        (trace, Some(trace)),
        ("invalid", None),
        (
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            None,
        ),
    ] {
        let response = client.post(format!("http://{addr}/v1/worker/commands"))
            .bearer_auth(key.as_str()).header("traceparent", header)
            .json(&serde_json::json!({"session_id":"ses_4c6906233871615c8475a306c8dea09b","subcommand":"exec prompt","args":["hello"]}))
            .send().await.unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, 202, "{body}");
        let command: serde_json::Value = client
            .get(format!(
                "http://{addr}/v1/worker/commands/{}",
                body["id"].as_str().unwrap()
            ))
            .bearer_auth(key.as_str())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(command["listener"], CommandListener::Network.as_str());
        assert_eq!(command["trace_parent"].as_str(), expected);
        let row = state
            .store
            .get_command(body["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let frontend = awman::frontend::api::ApiFrontendFactory.frontend_for(
            &row,
            awman::command::commands::api_server::event_bus::EventBus::new(8).sender(),
            awman::data::fs::api_command_log::CommandStateWriter::new(
                state.paths.clone(),
                &row.session_id,
                &row.id,
            ),
        );
        assert_eq!(
            frontend.kind(),
            awman::command::dispatch::catalogue::FrontendKind::Daemon {
                feature: awman::data::daemon_state::FeatureName::Worker,
                listener: CommandListener::Network,
            }
        );
        if expected.is_some() {
            let mut stream = client
                .get(format!(
                    "http://{addr}/v1/worker/commands/{}/events",
                    body["id"].as_str().unwrap()
                ))
                .bearer_auth(key.as_str())
                .send()
                .await
                .unwrap();
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.chunk())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let text = String::from_utf8_lossy(&chunk);
            assert!(text.contains("event: queue_status"), "{text}");
            assert!(text.contains(trace), "{text}");
            assert!(text.contains("network"), "{text}");
        }
    }
    task.abort();
}
use std::future::IntoFuture;
