//! The ephemeral `engine` feature: enabled for the length of one CLI
//! invocation through `PATCH /v1/features/engine`, idled out on a schedule.
//!
use serde_json::json;

use crate::api_process::ApiHome;
use crate::support::*;

fn engine_config(home: &ApiHome, allow: bool, idle: &str) {
    home.write_config_exact(json!({"daemon":{"features":{
        "worker":{"enabled":true},
        "engine":{"enabled":false,"allowEphemeral":allow,"idleTimeout":idle}
    }}}));
}

async fn patch_engine(
    server: &crate::api_process::ApiServer,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    http(
        server,
        reqwest::Method::PATCH,
        "/v1/features/engine",
        None,
        Some(body),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ephemeral_engine_is_enabled_then_idles_out() {
    let home = ApiHome::new();
    engine_config(&home, true, "2s");
    let server = home.start().await;

    let (code, body) = patch_engine(&server, json!({"enabled": true, "ephemeral": true})).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"], "ephemeral");
    assert_eq!(body["enabled_by"], "ephemeral");
    wait_for_status(&server, "engine idled out", |s| {
        s["features"]["engine"]["state"] == "off"
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_ephemeral_false_refuses_and_persistent_changes_need_config() {
    let home = ApiHome::new();
    engine_config(&home, false, "2s");
    let server = home.start().await;

    let (code, body) = patch_engine(&server, json!({"enabled": true, "ephemeral": true})).await;
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], "ephemeral_not_allowed");
    let (code, body) = patch_engine(&server, json!({"enabled": true})).await;
    assert_eq!(
        code, 400,
        "a persistent change goes through config set: {body}"
    );
    let (code, _) = http(
        &server,
        reqwest::Method::PATCH,
        "/v1/features/nope",
        None,
        Some(json!({"enabled": true, "ephemeral": true})),
    )
    .await;
    assert_eq!(code, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_exits_when_the_last_ephemeral_feature_idles_out() {
    let home = ApiHome::new();
    // Start with a persistent worker, then leave only the ephemeral engine.
    home.write_config_exact(json!({"daemon":{"features":{
        "worker":{"enabled":true},
        "engine":{"enabled":false,"allowEphemeral":true,"idleTimeout":"2s"}
    }}}));
    let server = home.start().await;
    let (code, body) = patch_engine(&server, json!({"enabled": true, "ephemeral": true})).await;
    assert_eq!(code, 200, "{body}");
    home.cli_ok(&[
        "config",
        "set",
        "daemon.features.worker.enabled",
        "false",
        "--global",
    ]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while home.cli_ok(&["daemon", "status"]).contains("pid") {
        assert!(std::time::Instant::now() < deadline, "daemon never exited");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
