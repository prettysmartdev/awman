//! WI 0145: `git_auth` on the session-creation route, end to end through the
//! daemon (admission, setup, push-back head check, restart), plus the
//! `session_git_auth` / `command_secrets` capability flags.
#![cfg(unix)]
use super::support::Api;
use crate::support;
use awman::command::daemon_api::keys;
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::{FakeGitHttp, GitHttpFault};
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{wait_for_async, TestSandbox};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "test-secret-daemon-session-token";

fn worker_config() -> Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

fn sandbox(id: &'static str) -> TestSandbox {
    let s = support::sandbox(id);
    s.require::<LocalGit>();
    s.require::<Loopback>();
    s.redact(TOKEN);
    s
}

/// The daemon embedded in this test binary runs the built `awman` as its
/// Git credential helper (production uses its own executable).
fn with_helper(server: Api) -> Api {
    server.api.engines.ephemeral_secrets.use_helper_program(
        awman::engine::git::lease_transport::HelperProgram::awman(
            support::awman_exe().to_path_buf(),
        ),
    );
    server
}

async fn start(s: &TestSandbox) -> Api {
    with_helper(Api::with_config(s, worker_config()).await)
}

fn git_auth(password: &str) -> Value {
    json!({"kind":"basic","username":"alice","password":password})
}

/// Every file under `root` (database, logs, setup state, idempotency
/// records, clones) is free of `secret` and of its Basic encoding.
fn assert_not_persisted(root: &Path, secret: &str) {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("alice:{secret}"));
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                for needle in [secret.as_bytes(), encoded.as_bytes()] {
                    assert!(
                        !bytes.windows(needle.len()).any(|w| w == needle),
                        "{} holds the session credential",
                        path.display()
                    );
                }
            }
        }
    }
}

async fn wait_setup(server: &Api, key: &str, id: &str) -> Value {
    wait_for_async(
        "session setup finished",
        Duration::from_secs(30),
        || async {
            let (code, session) = server
                .get(&format!("/v1/worker/sessions/{id}"), Some(key))
                .await;
            assert_eq!(code, 200, "{session}");
            matches!(session["setup_status"].as_str(), Some("ready" | "failed")).then_some(session)
        },
    )
    .await
}

async fn wait_terminal(server: &Api, key: &str, command_id: &str) -> Value {
    wait_for_async("command finished", Duration::from_secs(60), || async {
        let (code, command) = server
            .get(&format!("/v1/worker/commands/{command_id}"), Some(key))
            .await;
        assert_eq!(code, 200, "{command}");
        matches!(
            command["status"].as_str(),
            Some("done" | "error" | "cancelled")
        )
        .then_some(command)
    })
    .await
}

fn seed(s: &TestSandbox) -> awman_test_support::fixtures::Repo<'_> {
    let seed = RepoBuilder::new(s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    seed.create_branch("feature");
    seed.write("f", "f");
    seed.commit("feature");
    seed.push("origin", "feature");
    seed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_with_git_auth_clones_fetches_and_advertises_with_the_caller_token() {
    let s = sandbox("daemon::session_git_auth::session_with_git_auth_clones_fetches_and_advertises_with_the_caller_token");
    let _seed = seed(&s);
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .start();
    let url = http.url("origin.git");
    let server = start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let (code, created) = server
        .request(
            "POST",
            "/v1/worker/sessions",
            Some(&key.key),
            None,
            Some(
                json!({"session_type":"remote","repo_url":url,"branch":"work",
                "merge_refs":["feature"],"push":true,"git_auth":git_auth(TOKEN)}),
            ),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["git_auth"], json!({"kind":"basic"}));
    let id = created["id"].as_str().unwrap().to_string();
    let session = wait_setup(&server, &key.key, &id).await;
    assert_eq!(session["git_auth"], json!({"kind":"basic"}), "{session}");
    // Clone, merge-ref advertisement and setup fetch authenticated as the
    // caller (the sandbox has no Git credential helper of its own).
    assert_ne!(session["setup_error"]["stage"], "git_auth", "{session}");
    assert_ne!(session["setup_error"]["stage"], "clone", "{session}");
    let users: Vec<_> = http.requests().into_iter().flatten().collect();
    assert!(users.len() >= 3, "{users:?}");
    assert!(users.iter().all(|u| u == "alice"), "{users:?}");
    // Clone, branch and merge succeeded: setup got as far as the ready checks
    // (which need a container runtime this harness does not provide). The
    // clean `origin` and push-back with the ticket are proven at the engine
    // tier (`engine::git_credentials`).
    assert!(
        session["setup_status"] == "ready" || session["setup_error"]["stage"] == "ready",
        "{session}"
    );
    assert_eq!(
        server
            .api
            .engines
            .ephemeral_secrets
            .session_git_auth(&id)
            .is_some(),
        session["setup_status"] == "ready",
        "only a ready session retains its credential"
    );
    assert!(!session.to_string().contains(TOKEN));
    server.shutdown().await;
    assert_not_persisted(s.root(), TOKEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_auth_never_appears_in_session_record_events_logs_or_idempotency_store() {
    let s = sandbox("daemon::session_git_auth::git_auth_never_appears_in_session_record_events_logs_or_idempotency_store");
    let _seed = seed(&s);
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .start();
    let url = http.url("origin.git");
    let server = start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let other = "test-secret-daemon-other-token";
    s.redact(other);
    let post = |password: &'static str| {
        let (base, key, url) = (server.base.clone(), key.key.clone(), url.clone());
        async move {
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("{base}/v1/worker/runs"))
                .bearer_auth(key)
                .header("Idempotency-Key", "session-git-auth-key-1")
                .json(&json!({"session":{"session_type":"remote","repo_url":url,
                    "git_auth":git_auth(password)},
                    "command":{"subcommand":"exec prompt","args":["hello"]}}))
                .send()
                .await
                .unwrap();
            let code = response.status().as_u16();
            (code, response.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let (code, first) = post(TOKEN).await;
    assert_eq!(code, 202, "{first}");
    // Same key, same kind and username, another token: a replay of the
    // first request (the fingerprint never covers the secret).
    let (code, replay) = post(other).await;
    assert_eq!(code, 202, "{replay}");
    assert_eq!(first["command_id"], replay["command_id"]);
    let command_id = first["command_id"].as_str().unwrap();
    let command = wait_terminal(&server, &key.key, command_id).await;
    let (_, events) = server
        .get(
            &format!("/v1/worker/commands/{command_id}/events?format=json"),
            Some(&key.key),
        )
        .await;
    let (_, sessions) = server.get("/v1/worker/sessions", Some(&key.key)).await;
    for body in [&command, &events, &sessions] {
        assert!(!body.to_string().contains(TOKEN));
        assert!(!body.to_string().contains(other));
    }
    let users: Vec<_> = http.requests().into_iter().flatten().collect();
    assert!(users.iter().all(|u| u == "alice"), "{users:?}");
    server.shutdown().await;
    assert_not_persisted(s.root(), TOKEN);
    assert_not_persisted(s.root(), other);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_or_wrong_token_reports_git_auth_failed() {
    let s = sandbox("daemon::session_git_auth::expired_or_wrong_token_reports_git_auth_failed");
    let _seed = seed(&s);
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .start();
    let server = start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let wrong = "test-secret-daemon-expired-token";
    s.redact(wrong);
    let (code, response) = server
        .request(
            "POST",
            "/v1/worker/runs",
            Some(&key.key),
            None,
            Some(
                json!({"session":{"session_type":"remote","repo_url":http.url("origin.git"),
                "git_auth":git_auth(wrong)},
                "command":{"subcommand":"exec prompt","args":["hello"]}}),
            ),
        )
        .await;
    assert_eq!(code, 202, "{response}");
    let command = wait_terminal(&server, &key.key, response["command_id"].as_str().unwrap()).await;
    let result = &command["result"];
    assert_eq!(result["failure"]["stage"], "git_auth", "{command}");
    assert_eq!(result["failure"]["code"], "git_auth_failed");
    assert_eq!(result["setup_error"]["code"], "git_auth_failed");
    assert!(!command.to_string().contains(wrong));
    // The merge-ref advertisement at admission is refused the same way, as
    // a typed admission error.
    let (code, refused) = server
        .request(
            "POST",
            "/v1/worker/sessions",
            Some(&key.key),
            None,
            Some(
                json!({"session_type":"remote","repo_url":http.url("origin.git"),
                "branch":"work","merge_refs":["feature"],"git_auth":git_auth(wrong)}),
            ),
        )
        .await;
    assert_eq!(code, 422, "{refused}");
    assert_eq!(refused["code"], "git_auth_failed");
    assert!(!refused.to_string().contains(wrong));
    server.shutdown().await;
    assert_not_persisted(s.root(), wrong);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_or_unsupported_git_auth_is_refused_before_anything_is_persisted() {
    let s = sandbox("daemon::session_git_auth::invalid_or_unsupported_git_auth_is_refused_before_anything_is_persisted");
    let server = start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let secret = "test-secret-daemon-refused";
    s.redact(secret);
    for (body, status, code) in [
        (
            json!({"session_type":"remote","repo_url":"https://git.example/o/r.git",
                "git_auth":{"kind":"token","password":secret}}),
            400,
            "invalid_git_auth",
        ),
        (
            json!({"session_type":"remote","repo_url":"ssh://git@git.example/o/r.git",
                "git_auth":{"kind":"token","token":secret}}),
            400,
            "git_auth_unsupported_url",
        ),
        (
            json!({"session_type":"remote","repo_url":"http://git.example/o/r.git",
                "git_auth":{"kind":"token","token":secret}}),
            400,
            "git_auth_unsupported_url",
        ),
        (
            json!({"session_type":"local","workdir":s.work(),
                "git_auth":{"kind":"token","token":secret}}),
            400,
            "invalid_request",
        ),
    ] {
        let (got, response) = server
            .request(
                "POST",
                "/v1/worker/sessions",
                Some(&key.key),
                None,
                Some(body),
            )
            .await;
        assert_eq!(got, status, "{response}");
        assert_eq!(response["code"], code, "{response}");
        assert!(!response.to_string().contains(secret));
    }
    let (_, list) = server.get("/v1/worker/sessions", Some(&key.key)).await;
    assert!(!list.to_string().contains("git.example"), "{list}");
    server.shutdown().await;
    assert_not_persisted(s.root(), secret);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_daemon_restart_the_session_reports_credentials_lost() {
    let s = sandbox(
        "daemon::session_git_auth::after_daemon_restart_the_session_reports_credentials_lost",
    );
    let _seed = seed(&s);
    let hold = Arc::new(AtomicBool::new(false));
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .fault({
            let hold = Arc::clone(&hold);
            move |_| {
                hold.load(Ordering::SeqCst)
                    .then_some(GitHttpFault::StallBeforeResponse)
            }
        })
        .start();
    let url = http.url("origin.git");
    let server = start(&s).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    // A run whose setup is still cloning when the daemon restarts. (Admission
    // after a restart is `command::credentials`: this harness cannot make a
    // session ready without a container runtime.)
    hold.store(true, Ordering::SeqCst);
    let (code, run) = server
        .request(
            "POST",
            "/v1/worker/runs",
            Some(&key.key),
            None,
            Some(json!({"session":{"session_type":"remote","repo_url":url,
                "git_auth":git_auth(TOKEN)},
                "command":{"subcommand":"exec prompt","args":["hello"]}})),
        )
        .await;
    assert_eq!(code, 202, "{run}");
    let run_command = run["command_id"].as_str().unwrap().to_string();
    let run_session = run["session_id"].as_str().unwrap().to_string();
    wait_for_async(
        "the run's clone is in flight",
        Duration::from_secs(30),
        || async {
            let (_, session) = server
                .get(
                    &format!("/v1/worker/sessions/{run_session}"),
                    Some(&key.key),
                )
                .await;
            (session["setup_status"] == "cloning_repository").then_some(())
        },
    )
    .await;
    let worker = server.api.worker.clone().unwrap();
    let (store, paths) = (worker.store.clone(), worker.paths.clone());
    server.shutdown().await;
    hold.store(false, Ordering::SeqCst);
    // A graceful stop cancels the in-flight setup; put the rows back the way a
    // crash leaves them (setup still cloning, run command still queued).
    // (The cancelled setup finishes in the background; let it settle first.)
    wait_for_async(
        "the cancelled setup settled",
        Duration::from_secs(30),
        || async {
            let command = store.get_command(&run_command).unwrap().unwrap();
            let session = store.get_session(&run_session).unwrap().unwrap();
            // A setup cancelled by the stop closes its run session last.
            (command.status.is_terminal()
                && session.setup_status.is_terminal()
                && session.status == awman::data::session_status::SessionStatus::Closed)
                .then_some(())
        },
    )
    .await;
    let _ = std::fs::remove_file(paths.command_completion_path(&run_session, &run_command));
    for log in ["events.log", "output.log"] {
        let _ = std::fs::remove_file(paths.command_dir(&run_session, &run_command).join(log));
    }
    store
        .with_conn(|conn| {
            conn.execute_batch(&format!(
                "UPDATE sessions SET setup_status = 'cloning_repository', status = 'active',
                     closed_at = NULL WHERE id = '{run_session}';
                 UPDATE commands SET status = 'queued', exit_code = NULL, finished_at = NULL,
                     result = NULL, started_at = NULL WHERE id = '{run_command}';
                 DELETE FROM command_cancellations WHERE command_id = '{run_command}';"
            ))?;
            Ok(())
        })
        .unwrap();

    let server = with_helper(Api::restart_with_config(&s, worker_config()).await);
    // The interrupted run: setup and command both report credentials_lost.
    let command = wait_terminal(&server, &key.key, &run_command).await;
    assert_eq!(
        command["result"]["failure"]["stage"], "git_auth",
        "{command}"
    );
    assert_eq!(command["result"]["failure"]["code"], "credentials_lost");
    assert_eq!(command["result"]["setup_error"]["code"], "credentials_lost");
    server.shutdown().await;
    assert_not_persisted(s.root(), TOKEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_advertises_session_git_auth_and_command_secrets() {
    let s = support::sandbox(
        "daemon::session_git_auth::status_advertises_session_git_auth_and_command_secrets",
    );
    let server = Api::start(&s).await;
    let key = keys::keys_create(&server.api, "reader").unwrap();
    s.redact(&key.key);
    let (code, status) = server.get("/v1/status", Some(&key.key)).await;
    assert_eq!(code, 200);
    let capabilities = status["capabilities"].as_array().unwrap();
    for flag in [
        "git_auth_failure",
        "push_retry_events",
        "push_retry_codes",
        "session_git_auth",
        "command_secrets",
    ] {
        assert_eq!(
            capabilities.iter().filter(|c| **c == json!(flag)).count(),
            1,
            "{flag}"
        );
    }
    server.shutdown().await;
}
