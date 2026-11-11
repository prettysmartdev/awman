//! `engine::remote` — `RemoteClient` against a loopback mock server on a
//! sandbox-owned listener: HTTP status mapping and a refused connection.
//! Re-derived from the socket-bound inline tests of `remote::client`.

use crate::support;
use awman::engine::error::EngineError;
use awman::engine::remote::RemoteClient;
use awman_test_support::TestSandbox;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

async fn mock_server(sandbox: &TestSandbox) -> MockServer {
    MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await
}

async fn respond(server: &MockServer, path: &str, status: u16, body: serde_json::Value) {
    Mock::given(matchers::method("POST"))
        .and(matchers::path(path))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn send_command_returns_the_parsed_response_on_200() {
    let sandbox =
        support::sandbox("engine::remote::send_command_returns_the_parsed_response_on_200");
    let server = mock_server(&sandbox).await;
    respond(
        &server,
        "/v1/agents/status",
        200,
        serde_json::json!({"ok": true}),
    )
    .await;
    let client = RemoteClient::new(&server.uri(), None).unwrap();
    let response = client
        .send_command(&["agents", "status"], &[])
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn send_command_maps_4xx_and_5xx_to_remote_http_status() {
    let sandbox =
        support::sandbox("engine::remote::send_command_maps_4xx_and_5xx_to_remote_http_status");
    let server = mock_server(&sandbox).await;
    respond(
        &server,
        "/v1/exec/workflow",
        400,
        serde_json::json!({"error": "bad request"}),
    )
    .await;
    respond(
        &server,
        "/v1/agents/status",
        500,
        serde_json::json!({"error": "internal server error"}),
    )
    .await;
    let client = RemoteClient::new(&server.uri(), None).unwrap();
    let bad = client.send_command(&["exec", "workflow"], &[]).await;
    assert!(
        matches!(bad, Err(EngineError::RemoteHttpStatus { status: 400, .. })),
        "{bad:?}"
    );
    let failed = client.send_command(&["agents", "status"], &[]).await;
    assert!(
        matches!(
            failed,
            Err(EngineError::RemoteHttpStatus { status: 500, .. })
        ),
        "{failed:?}"
    );
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_maps_to_remote_connection_refused() {
    let sandbox =
        support::sandbox("engine::remote::a_refused_connection_maps_to_remote_connection_refused");
    // An address this sandbox owned and closed: nothing listens there.
    let listener = sandbox.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = RemoteClient::new(&format!("http://{addr}"), None).unwrap();
    let result = client.send_command(&["agents", "status"], &[]).await;
    assert!(
        matches!(result, Err(EngineError::RemoteConnectionRefused(_))),
        "{result:?}"
    );
    sandbox.finish();
}
