//! `GET /v1/status` field naming (0139) and serde round trip.

use crate::api_process::ApiHome;
use awman::command::daemon_api::core::StatusResponse;

#[tokio::test(flavor = "multi_thread")]
async fn status_has_per_feature_objects_and_ms_suffixed_durations() {
    let home = ApiHome::new();
    let server = home.start().await;
    let status = server.get("/v1/status").await;

    assert_eq!(status["status"], "ok");
    assert_eq!(status["protocol"]["api"], 1);
    assert!(status["uptime_ms"].is_u64(), "{status}");
    assert!(status["max_concurrent_jobs"].is_u64(), "{status}");
    for feature in ["worker", "squad", "engine"] {
        let record = &status["features"][feature];
        assert_eq!(record["name"], feature, "{status}");
        assert!(record["state"].is_string(), "{record}");
        assert!(record["since_at"].is_string(), "{record}");
        assert!(record["running"].is_u64() && record["queued"].is_u64());
    }
    assert_eq!(status["features"]["worker"]["state"], "on");
    assert_eq!(status["features"]["squad"]["state"], "off");
    assert_eq!(status["features"]["engine"]["state"], "off");
    assert_eq!(status["listeners"]["local"]["transport"], "unix_socket");
    assert!(status["listeners"]["network"].is_null());

    // 0139 naming: durations are `_ms`, timestamps `_at`; no `_secs` anywhere.
    let text = status.to_string();
    assert!(!text.contains("_secs"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_round_trips_through_the_typed_response() {
    let home = ApiHome::new();
    let server = home.start().await;
    let status = server.get("/v1/status").await;
    let typed: StatusResponse = serde_json::from_value(status.clone()).expect("typed status");
    assert_eq!(serde_json::to_value(&typed).unwrap(), status);
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_cap_defaults_to_four_and_follows_config_live() {
    let home = ApiHome::new();
    let server = home.start().await;
    assert_eq!(server.get("/v1/status").await["max_concurrent_jobs"], 4);
    let out = home.cli_ok(&["config", "set", "daemon.maxConcurrentJobs", "3", "--global"]);
    assert!(out.contains("applied to the running daemon"), "{out}");
    crate::support::wait_for_status(&server, "cap 3", |s| s["max_concurrent_jobs"] == 3).await;
}

/// WI 0142: engine PTYs now register with the hub. The engine_parity terminal
/// gate proves reconnect, input ownership, resize and exit on the real socket.
#[tokio::test(flavor = "multi_thread")]
async fn capabilities_advertise_only_what_works() {
    let home = ApiHome::new();
    let server = home.start().await;
    let status = server.get("/v1/status").await;
    let capabilities: Vec<&str> = status["capabilities"]
        .as_array()
        .expect("capabilities")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert!(capabilities.contains(&"agent_status"), "{status}");
    for capability in [
        "terminal",
        "interactive",
        "engine_instances",
        "frontend_protocol",
    ] {
        assert!(capabilities.contains(&capability), "{status}");
    }
    assert_eq!(status["protocol"]["frontend"], 1);
    // No agent has been started in this fixture, so the hub is empty.
    let terminals = server.get("/v1/terminals").await;
    assert_eq!(terminals["items"], serde_json::json!([]), "{terminals}");
}

/// WI 0143: `worker` keeps 0140's per-feature fields and adds `push` and
/// `pull`; with the feature on, the pull controller advertises its capabilities,
/// and the pull routes answer on the local listener.
#[tokio::test(flavor = "multi_thread")]
async fn worker_status_has_push_and_pull_sides_and_pull_routes_answer() {
    let home = ApiHome::new();
    let server = home.start().await;
    let status = server.get("/v1/status").await;
    let worker = &status["features"]["worker"];
    assert_eq!(
        worker["push"],
        serde_json::json!({"running": 0, "queued": 0}),
        "{worker}"
    );
    assert_eq!(
        worker["pull"],
        serde_json::json!({"servers": []}),
        "{worker}"
    );
    assert!(
        status["features"]["squad"].get("pull").is_none(),
        "{status}"
    );
    let capabilities = status["capabilities"].as_array().unwrap();
    for capability in [
        "workflow_dynamic",
        "workflow_file",
        "prompt",
        "workflow_state_events",
        "outputs_mount_state",
    ] {
        assert!(
            capabilities.contains(&serde_json::json!(capability)),
            "{status}"
        );
    }
    // The enabled worker feature runs the pull controller, which advertises its
    // capabilities (`lease_read_only` depends on the runtime) even with no server.
    for capability in [
        "worker_pull",
        "lease_credentials",
        "lease_inline_work_item",
        "lease_git_auth_failure",
        "lease_push_retry",
    ] {
        assert!(
            capabilities.contains(&serde_json::json!(capability)),
            "{status}"
        );
    }
    assert_eq!(
        server.get("/v1/worker/servers").await,
        serde_json::json!({"items": [], "next_cursor": null})
    );
    assert_eq!(
        server.get("/v1/worker/leases").await,
        serde_json::json!({"items": [], "next_cursor": null})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_worker_omits_pull_capabilities_even_when_the_handle_exists() {
    let home = ApiHome::new();
    home.write_config_exact(serde_json::json!({"daemon":{"features":{"worker":{"enabled":false},"squad":{"enabled":true,"envPersistence":"none"}}}}));
    let server = home.start().await;
    let status = server.get("/v1/status").await;
    assert_eq!(status["features"]["worker"]["state"], "off");
    for cap in [
        "worker_pull",
        "lease_credentials",
        "lease_inline_work_item",
        "lease_read_only",
        "lease_git_auth_failure",
        "lease_push_retry",
    ] {
        assert!(
            !status["capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(cap)),
            "{status}"
        );
    }
}
