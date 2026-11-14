//! WI 0145: a session created with `git_auth` whose in-memory credential did
//! not survive a daemon restart. Real L2 admission and queue worker over the
//! durable rows, with a fresh engine bundle (empty memory).
use super::support::{prompt, AdmissionFixture};
use awman::command::commands::api_server::admission::AdmissionError;
use awman::command::commands::api_server::queue_worker::{QueueWorker, QueueWorkerDeps};
use awman::command::commands::api_server::{ApiSessionLifecycle, TokenRegistry};
use awman::command::error::CommandError;
use awman::data::api_lifecycle_store::CommandLifecycleStore;
use awman::data::session_setup_event::SessionSetupStatus;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn restart_without_session_credentials_refuses_admission_and_fails_a_queued_command() {
    let s = crate::support::sandbox(
        "command::credentials::restart_without_session_credentials_refuses_admission_and_fails_a_queued_command",
    );
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let f = AdmissionFixture::new(&s);
    // A command queued before the restart, on a session whose row carries
    // the `git_auth` kind marker (the credential itself is never stored).
    let run = f
        .admission
        .admit_session(f.local(&s), Some(prompt("go")), false, None)
        .await
        .unwrap();
    let id = run.run.unwrap().command_id;
    f.store
        .update_setup_status(&run.session_id, SessionSetupStatus::Ready)
        .unwrap();
    f.sql(&format!(
        "UPDATE sessions SET git_auth_kind = 'token' WHERE id = '{}'",
        run.session_id
    ));
    assert_eq!(
        f.store
            .get_session(&run.session_id)
            .unwrap()
            .unwrap()
            .git_auth_kind
            .as_deref(),
        Some("token")
    );

    // New daemon bundle over the same rows: no session credential in memory.
    let engines =
        awman::command::dispatch::Engines::for_env(crate::support::env_snapshot(&s)).unwrap();
    assert!(engines
        .ephemeral_secrets
        .session_git_auth(&run.session_id)
        .is_none());
    let lifecycle = ApiSessionLifecycle::new(
        f.store.clone(),
        engines.clone(),
        f.sessions.clone(),
        f.paths.clone(),
    );
    let worker = QueueWorker::new(
        "restart-worker".into(),
        QueueWorkerDeps {
            store: f.store.clone(),
            engines: engines.clone(),
            sessions: f.sessions.clone(),
            event_buses: Default::default(),
            paths: f.paths.clone(),
            frontend_factory: Arc::new(awman::frontend::api::ApiFrontendFactory),
            lifecycle,
            lifecycle_store: CommandLifecycleStore::open(f.store.clone()).unwrap(),
            cancels: Arc::new(TokenRegistry::new()),
        },
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(worker.run_until(stop.clone()));
    let row = awman_test_support::wait_for_async(
        "credentials_lost terminal row",
        std::time::Duration::from_secs(5),
        || async {
            let row = f.store.get_command(id.as_str()).unwrap().unwrap();
            row.status.is_terminal().then_some(row)
        },
    )
    .await;
    stop.cancel();
    task.await.unwrap();
    let result: serde_json::Value = serde_json::from_str(row.result.as_ref().unwrap()).unwrap();
    assert_eq!(result["failure"]["stage"], "git_auth", "{result}");
    assert_eq!(result["failure"]["code"], "credentials_lost");
    assert!(result["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("open a new session"));
    // Failed before metadata, setup or any container: never a run with the
    // host's own credentials instead.
    assert!(!f
        .paths
        .command_dir(&run.session_id, id.as_str())
        .join("metadata.json")
        .exists());
    assert!(docker.calls().is_empty());

    // New admission on that session: refused with the typed error (409).
    let restarted = awman::command::commands::api_server::admission::ApiAdmission::new(
        f.store.clone(),
        CommandLifecycleStore::open(f.store.clone()).unwrap(),
        f.paths.clone(),
        engines,
        f.sessions.clone(),
        vec![s.work().canonicalize().unwrap()],
        vec![],
    );
    match restarted
        .admit_command(&run.session_id, prompt("again"), None)
        .await
    {
        Err(AdmissionError::CommandAdmission(error @ CommandError::CredentialsLost)) => {
            assert_eq!(
                (error.stage(), error.code()),
                ("git_auth", "credentials_lost")
            );
            assert_eq!(error.http_status(), 409);
        }
        Err(other) => panic!("expected credentials_lost, got {other:?}"),
        Ok(_) => panic!("a session without its credential admitted a command"),
    }
    assert_eq!(
        f.store
            .list_commands_for_session(&run.session_id, i64::MAX)
            .unwrap()
            .len(),
        1,
        "nothing queued"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn command_socket_startup_failure_never_dispatches_without_session_auth() {
    use awman::command::dispatch::ephemeral_secrets::SessionGitCredential;
    use awman::data::git_transport::{GitAuthKind, GitAuthSpec};
    use awman::data::secret::SecretString;
    use awman::engine::git::lease_transport::CredentialScope;
    let s=crate::support::sandbox("command::credentials::command_socket_startup_failure_never_dispatches_without_session_auth");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let f = AdmissionFixture::new(&s);
    let run = f
        .admission
        .admit_session(f.local(&s), Some(prompt("go")), false, None)
        .await
        .unwrap();
    let id = run.run.unwrap().command_id;
    f.store
        .update_setup_status(&run.session_id, SessionSetupStatus::Ready)
        .unwrap();
    f.engines.ephemeral_secrets.insert_session_git_auth(
        &run.session_id,
        SessionGitCredential {
            spec: Arc::new(
                GitAuthSpec::new(
                    GitAuthKind::Token,
                    None,
                    // A valid synthetic token deliberately appears in the
                    // fixed failure text, proving queue-bus registration.
                    SecretString::new("could not start the command credential socket"),
                )
                .unwrap(),
            ),
            scope: CredentialScope::for_http_url("https://git.example/repo.git").unwrap(),
        },
    );
    // A file at the private socket namespace deterministically refuses startup.
    let namespace = awman::command::session_git_transport::credential_socket_dir(&f.engines);
    f.sessions
        .open_or_create_with_key(Some(run.session_id.clone()), s.work(), Default::default())
        .unwrap();
    std::fs::create_dir_all(namespace.parent().unwrap()).unwrap();
    std::fs::write(namespace, "occupied").unwrap();
    let worker = QueueWorker::new(
        "socket-failure-worker".into(),
        QueueWorkerDeps {
            store: f.store.clone(),
            engines: f.engines.clone(),
            sessions: f.sessions.clone(),
            event_buses: Default::default(),
            paths: f.paths.clone(),
            frontend_factory: Arc::new(awman::frontend::api::ApiFrontendFactory),
            lifecycle: f.lifecycle.clone(),
            lifecycle_store: CommandLifecycleStore::open(f.store.clone()).unwrap(),
            cancels: Arc::new(TokenRegistry::new()),
        },
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(worker.run_until(stop.clone()));
    let row = awman_test_support::wait_for_async(
        "socket failure result",
        std::time::Duration::from_secs(5),
        || async {
            let row = f.store.get_command(id.as_str()).unwrap().unwrap();
            row.status.is_terminal().then_some(row)
        },
    )
    .await;
    stop.cancel();
    task.await.unwrap();
    let result: serde_json::Value = serde_json::from_str(row.result.as_ref().unwrap()).unwrap();
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .starts_with("session_git_auth_unavailable_in_container"),
        "{result}"
    );
    assert!(!result
        .to_string()
        .contains("could not start the command credential socket"));
    assert!(docker.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_session_credential_marker_fails_before_metadata_or_dispatch() {
    let s=crate::support::sandbox("command::credentials::unreadable_session_credential_marker_fails_before_metadata_or_dispatch");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let f = AdmissionFixture::new(&s);
    let run = f
        .admission
        .admit_session(f.local(&s), Some(prompt("go")), false, None)
        .await
        .unwrap();
    let id = run.run.unwrap().command_id;
    f.store
        .update_setup_status(&run.session_id, SessionSetupStatus::Ready)
        .unwrap();
    // The queue can claim this ready session, but decoding its policy fails.
    f.sql(&format!(
        "UPDATE sessions SET git_auth_kind = X'00ff' WHERE id='{}'",
        run.session_id
    ));
    assert!(f.store.get_session(&run.session_id).is_err());
    let worker = QueueWorker::new(
        "policy-failure-worker".into(),
        QueueWorkerDeps {
            store: f.store.clone(),
            engines: f.engines.clone(),
            sessions: f.sessions.clone(),
            event_buses: Default::default(),
            paths: f.paths.clone(),
            frontend_factory: Arc::new(awman::frontend::api::ApiFrontendFactory),
            lifecycle: f.lifecycle.clone(),
            lifecycle_store: CommandLifecycleStore::open(f.store.clone()).unwrap(),
            cancels: Arc::new(TokenRegistry::new()),
        },
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(worker.run_until(stop.clone()));
    let row = awman_test_support::wait_for_async(
        "policy read failure result",
        std::time::Duration::from_secs(5),
        || async {
            let row = f.store.get_command(id.as_str()).unwrap().unwrap();
            row.status.is_terminal().then_some(row)
        },
    )
    .await;
    stop.cancel();
    task.await.unwrap();
    assert_eq!(
        row.status,
        awman::data::command_status::CommandStatus::Error
    );
    assert!(!f
        .paths
        .command_dir(&run.session_id, id.as_str())
        .join("metadata.json")
        .exists());
    assert!(docker.calls().is_empty());
}
