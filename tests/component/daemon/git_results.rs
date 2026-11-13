use super::support::Api;
use crate::support;
use awman::command::daemon_api::keys;
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::FakeGitHttp;
use awman_test_support::fixtures::RepoBuilder;
use serde_json::{json, Value};
use std::time::Duration;

fn worker_config() -> Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_advertises_git_auth_failure_and_push_retry_flags_once() {
    let s = support::sandbox(
        "daemon::git_results::status_advertises_git_auth_failure_and_push_retry_flags_once",
    );
    let server = Api::start(&s).await;
    let key = keys::keys_create(&server.api, "reader").unwrap();
    s.redact(&key.key);
    let (code, status) = server.get("/v1/status", Some(&key.key)).await;
    assert_eq!(code, 200);
    let capabilities = status["capabilities"].as_array().unwrap();
    // WI 0145: slice 1 and slice 2 flags, each exactly once.
    for shipped in ["git_auth_failure", "push_retry_events", "push_retry_codes"] {
        assert_eq!(
            capabilities
                .iter()
                .filter(|c| **c == json!(shipped))
                .count(),
            1,
            "{shipped}"
        );
    }
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_session_with_url_credentials_is_rejected_and_nothing_is_persisted() {
    let s = support::sandbox("daemon::git_results::remote_session_with_url_credentials_is_rejected_and_nothing_is_persisted");
    let server = Api::with_config(&s, worker_config()).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let secret = "test-secret-url-password";
    s.redact(secret);
    for url in [
        format!("https://user:{secret}@example.invalid/repo.git"),
        format!("https://user:{secret}#suffix@example.invalid/repo.git"),
        format!("https://user:{secret}?suffix@example.invalid/repo.git"),
        format!("https://user:{secret}/suffix@example.invalid/repo.git"),
        format!("https://example.invalid/repo.git?token={secret}"),
    ] {
        let (code, response) = server
            .request(
                "POST",
                "/v1/worker/sessions",
                Some(&key.key),
                None,
                Some(json!({"session_type":"remote", "repo_url":url})),
            )
            .await;
        assert_eq!(code, 400, "{response}");
        assert!(!response.to_string().contains(secret));
    }
    let (code, list) = server.get("/v1/worker/sessions", Some(&key.key)).await;
    assert_eq!(code, 200, "{list}");
    assert!(!list.to_string().contains("example.invalid"));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_run_clone_auth_failure_reports_git_auth_failed() {
    let s = support::sandbox(
        "daemon::git_results::remote_run_clone_auth_failure_reports_git_auth_failed",
    );
    s.require::<LocalGit>();
    s.require::<Loopback>();
    let repo = RepoBuilder::new(&s, "seed").file("a", "a").init();
    repo.bare_remote("origin");
    let secret = "test-secret-remote-password";
    s.redact(secret);
    let stale = "test-secret-supplied-stale-password";
    s.redact(stale);
    let credential = s.root().join("stale.credential");
    std::fs::write(&credential, format!("username=user\npassword={stale}\n")).unwrap();
    s.command("git")
        .args([
            "config",
            "--global",
            "credential.helper",
            &format!("!cat '{}'", credential.display()),
        ])
        .run()
        .assert_success();
    let remote = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("user", secret)
        .start();
    let server = Api::with_config(&s, worker_config()).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let (code, response) = server
        .request(
            "POST",
            "/v1/worker/runs",
            Some(&key.key),
            None,
            Some(
                json!({"session":{"session_type":"remote", "repo_url":remote.url("origin.git")},
            "command":{"subcommand":"exec prompt", "args":["hello"]}}),
            ),
        )
        .await;
    assert_eq!(code, 202, "{response}");
    let command_id = response["command_id"].as_str().unwrap();
    let path = format!("/v1/worker/commands/{command_id}");
    let command = awman_test_support::wait_for_async(
        "run setup failure recorded",
        Duration::from_secs(8),
        || async {
            let (code, command) = server.get(&path, Some(&key.key)).await;
            assert_eq!(code, 200, "{command}");
            (command["status"] == "error").then_some(command)
        },
    )
    .await;
    let result = &command["result"];
    assert_eq!(result["failure"]["stage"], "git_auth", "{command}");
    assert_eq!(result["failure"]["code"], "git_auth_failed");
    assert_eq!(result["setup_error"]["code"], "git_auth_failed");
    assert!(result["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("clone"));
    assert!(!command.to_string().contains(secret));
    assert!(!command.to_string().contains(stale));
    let events = awman_test_support::wait_for_async(
        "terminal events reach the durable log",
        Duration::from_secs(5),
        || async {
            let (code, events) = server
                .get(&format!("{path}/events?format=json"), Some(&key.key))
                .await;
            assert_eq!(code, 200, "{events}");
            events["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
                .then_some(events)
        },
    )
    .await;
    assert!(!events["events"].as_array().unwrap().is_empty());
    assert!(!events.to_string().contains(secret));
    assert!(!events.to_string().contains(stale));
    server.shutdown().await;
}
