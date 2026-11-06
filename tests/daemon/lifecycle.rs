//! `awman daemon start|stop|status|logs` and config hints, end to end.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api_process::{AgentMode, ApiHome};
use crate::support::*;

/// Stops a background daemon when the test ends, pass or fail.
struct StopOnDrop<'a>(&'a ApiHome);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.cli(&["daemon", "stop", "--now"]);
    }
}

#[test]
fn background_start_status_logs_and_stop() {
    let home = ApiHome::new();
    let _guard = StopOnDrop(&home);

    let started = home.cli_ok(&["daemon", "start", "--background"]);
    assert!(!started.is_empty());
    let status: Value =
        serde_json::from_str(&home.cli_ok(&["daemon", "status", "--json"])).unwrap();
    assert!(status["pid"].as_u64().unwrap() > 0, "{status}");

    // A second start is refused with the running pid.
    let error = home.cli_err(&["daemon", "start", "--background"]);
    assert!(error.contains("already running"), "{error}");

    let logs = home.cli_ok(&["daemon", "logs"]);
    assert!(logs.contains("listening"), "{logs}");

    let out = home.cli_ok(&["daemon", "stop"]);
    assert!(out.contains("stopped"), "{out}");
    assert!(home.cli_ok(&["daemon", "status"]).contains("not running"));
    let status: Value =
        serde_json::from_str(&home.cli_ok(&["daemon", "status", "--json"])).unwrap();
    assert_eq!(status["running"], false);
    // Stopping again is a clear no-op error, not a crash.
    let error = home.cli_err(&["daemon", "stop"]);
    assert!(error.contains("not running"), "{error}");
}

#[test]
fn start_without_any_enabled_feature_is_refused_with_the_command_to_run() {
    let home = ApiHome::new();
    home.write_config_exact(json!({}));
    let error = home.cli_err(&["daemon", "start"]);
    assert!(
        error.contains("no daemon feature is enabled")
            && error.contains("daemon.features.worker.enabled"),
        "{error}"
    );
}

#[test]
fn removed_api_and_squad_lifecycle_commands_and_keys_say_where_they_moved() {
    let home = ApiHome::new();
    for args in [
        vec!["api", "start"],
        vec!["squad", "start"],
        vec!["squad", "stop"],
    ] {
        let output = home.cli(&args);
        assert!(!output.status.success(), "{args:?}");
    }
    for (key, hint) in [
        ("api.port", "daemon.network.listen"),
        ("api.bind", "daemon.network.listen"),
        ("workers", "daemon.maxConcurrentJobs"),
    ] {
        let error = home.cli_err(&["config", "set", key, "1", "--global"]);
        assert!(error.contains(hint), "{key}: {error}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_now_cancels_running_commands_and_the_daemon_exits() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    repo.allow_in(&home, json!({}));
    let mut server = home.start().await;
    let session = repo.session(&server).await;
    let command = submit(&server, &session, &["running at stop"]).await;
    wait_slow_agent(&home).await;
    wait_status(&server, &command, &["running"]).await;

    let started = Instant::now();
    let out = home.cli_ok(&["daemon", "stop", "--now"]);
    assert!(out.contains("stopped"), "{out}");
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "a --now stop does not wait out the 30s agent"
    );
    assert!(home.cli_ok(&["daemon", "status"]).contains("not running"));
    server.kill();
}

#[test]
fn background_start_honors_and_retains_set_overrides() {
    let home = ApiHome::new();
    home.write_config_exact(json!({}));
    let _guard = StopOnDrop(&home);
    home.cli_ok(&[
        "--set",
        "daemon.features.engine.enabled=true",
        "--set",
        "daemon.maxConcurrentJobs=7",
        "daemon",
        "start",
        "--background",
    ]);
    let status: Value =
        serde_json::from_str(&home.cli_ok(&["daemon", "status", "--json"])).unwrap();
    assert_eq!(status["features"]["engine"]["enabled_by"], "config");
    assert_eq!(status["max_concurrent_jobs"], 7);
    home.cli_ok(&["config", "set", "daemon.verboseSetup", "true", "--global"]);
    let status: Value =
        serde_json::from_str(&home.cli_ok(&["daemon", "status", "--json"])).unwrap();
    assert_eq!(status["features"]["engine"]["state"], "on");
    assert_eq!(status["max_concurrent_jobs"], 7);
}
