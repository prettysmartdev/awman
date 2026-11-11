use super::support::Api;
use crate::support;
use awman::command::daemon_api::keys;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_auth_is_required_keys_are_owner_only_and_revocation_is_immediate() {
    let sandbox = support::sandbox(
        "daemon::api::network_auth_is_required_keys_are_owner_only_and_revocation_is_immediate",
    );
    let server = Api::start(&sandbox).await;
    assert_eq!(server.get("/v1/health", Some("invalid-key")).await.0, 200);
    assert_eq!(server.get("/v1/status", None).await.0, 401);
    assert_eq!(server.get("/v1/status", Some("invalid-key")).await.0, 401);
    let created = keys::keys_create(&server.api, "component").unwrap();
    sandbox.redact(&created.key);
    let (code, status) = server.get("/v1/status", Some(&created.key)).await;
    assert_eq!(code, 200, "{status}");
    assert_eq!(status["status"], "ok");
    assert_eq!(status["protocol"]["api"], 1);
    let typed: awman::command::daemon_api::core::StatusResponse =
        serde_json::from_value(status.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), status);
    assert_eq!(server.get("/v1/keys", Some(&created.key)).await.0, 404);
    assert_eq!(
        server
            .request("POST", "/v1/shutdown", Some(&created.key), None, None)
            .await
            .0,
        404
    );
    assert!(!server.api.runtime.exit_signal().is_cancelled());
    let list = serde_json::to_string(&keys::keys_list(&server.api).unwrap()).unwrap();
    assert!(list.contains(created.id.as_str()));
    assert!(!list.contains(&created.key));
    assert!(keys::keys_delete(&server.api, &created.id).unwrap());
    assert_eq!(server.get("/v1/status", Some(&created.key)).await.0, 401);
    server.shutdown().await;
    sandbox.finish();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_visibility_hides_unserved_features_and_malformed_routes_are_bounded() {
    let sandbox = support::sandbox(
        "daemon::api::network_visibility_hides_unserved_features_and_malformed_routes_are_bounded",
    );
    let server = Api::start(&sandbox).await;
    let created = keys::keys_create(&server.api, "reader").unwrap();
    sandbox.redact(&created.key);
    for path in [
        "/v1/worker/commands",
        "/v1/tasks",
        "/v1/keys",
        "/v1/nonexistent",
    ] {
        assert_eq!(server.get(path, Some(&created.key)).await.0, 404, "{path}");
    }
    for path in [
        "/v1/engine/instances?limit=0",
        "/v1/engine/instances?cursor=bad",
    ] {
        assert_eq!(server.get(path, Some(&created.key)).await.0, 400, "{path}");
    }
    server.shutdown().await;
    sandbox.finish();
}
