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
