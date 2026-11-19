//! Real authenticated routes, bounded typed decoder and origin refusals.
use super::support::Api;
use serde_json::json;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_routes_refuse_network_and_do_not_echo_decoder_secrets() {
    let sandbox=crate::support::sandbox("daemon::credential_updates::credential_routes_refuse_network_and_do_not_echo_decoder_secrets");
    let server=Api::with_config(&sandbox,json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true}}},"network":{"listeners":{"component":{"features":["engine","worker"]}}}}})).await;
    let key = awman::command::daemon_api::keys::keys_create(&server.api, "worker").unwrap();
    sandbox.redact(&key.key);
    let command = awman::data::ids::CommandId::new();
    let request = uuid::Uuid::new_v4();
    let path = format!("/v1/worker/commands/{command}/credentials");
    let (status, body) = server
        .request(
            "POST",
            &path,
            Some(&key.key),
            None,
            Some(json!({"secrets":{"TOKEN":"test-secret-refused"}})),
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(body["code"], "forbidden");
    assert!(!body.to_string().contains("test-secret"));
    let (status, _) = server
        .get(
            &format!("/v1/worker/commands/{command}/credential-updates/{request}"),
            Some(&key.key),
        )
        .await;
    assert_eq!(status, 403);
    let owner = server.serve_owner_on_private_address(&sandbox).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for raw in [
        r#"{"test-secret-field":"test-secret-value"}"#,
        r#"{"secrets":{"test-secret-name":"test-secret-value"}}"#,
        r#"{"secrets":{"TOKEN":123456789}}"#,
        r#"{"request_id":"test-secret-invalid-uuid"}"#,
    ] {
        let response = client
            .post(format!("{}{path}", owner.base))
            .header("content-type", "application/json")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        let body = response.text().await.unwrap();
        assert!(!body.contains("test-secret"));
        assert!(!body.contains("123456789"));
    }
    let response = client
        .post(format!("{}{path}", owner.base))
        .header("content-type", "application/json")
        .header("forwarded", "for=127.0.0.1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    let response = client
        .post(format!("{}{path}", owner.base))
        .header("content-type", "application/json")
        .body(" ".repeat(2 * 1024 * 1024 + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 413);
    owner.stop().await;
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_ingress_requires_transport_provenance_without_forwarding_headers() {
    use awman::data::daemon_state::RequestOrigin;
    let sandbox = crate::support::sandbox("daemon::credential_updates::credential_ingress_requires_transport_provenance_without_forwarding_headers");
    let server = Api::with_config(
        &sandbox,
        json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true}}}}}),
    )
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let command = awman::data::ids::CommandId::new();
    for origin in [
        None,
        Some(RequestOrigin::Unknown),
        Some(RequestOrigin::Relay),
        Some(RequestOrigin::WorkServer),
        Some(RequestOrigin::Browser),
        Some(RequestOrigin::Network),
    ] {
        let ingress = server
            .serve_ingress_on_private_address(&sandbox, origin)
            .await;
        let response = client
            .post(format!(
                "{}/v1/worker/commands/{command}/credentials",
                ingress.base
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{origin:?}");
        let response = client
            .get(format!(
                "{}/v1/worker/commands/{command}/credential-updates/{}",
                ingress.base,
                uuid::Uuid::new_v4()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{origin:?}");
        ingress.stop().await;
    }
    server.shutdown().await;
    sandbox.finish();
}

#[test]
fn command_projection_omits_absent_owner_credential_metadata() {
    let sandbox = crate::support::sandbox(
        "daemon::credential_updates::command_projection_omits_absent_owner_credential_metadata",
    );
    let response: awman::frontend::api::routes::CommandResponse = serde_json::from_value(json!({
        "id": awman::data::ids::CommandId::new(), "attempt":1,
        "session_id": awman::data::ids::WorkerSessionId::new(), "subcommand":"chat",
        "args":[], "status":"running", "labels":{}
    }))
    .unwrap();
    let wire = serde_json::to_value(response).unwrap();
    assert!(wire.get("credential_updates").is_none());
    sandbox.finish();
}
