use super::support::{prompt, AdmissionFixture};
use awman::command::commands::api_server::admission::CommandSubmission;
use awman::command::commands::api_server::queue_worker::{QueueWorker, QueueWorkerDeps};
use awman::command::commands::api_server::{ApiSessionLifecycle, TokenRegistry};
use awman::data::api_lifecycle_store::CommandLifecycleStore;
use awman::data::fs::api_db::IdempotencyBegin;
use awman::data::ids::CommandId;
use awman::data::secret::{CommandSecrets, SecretString};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn admission_persists_only_names_and_idempotency_never_hashes_values() {
    let s = crate::support::sandbox(
        "command::secrets::admission_persists_only_names_and_idempotency_never_hashes_values",
    );
    let value = "test-secret-admission";
    s.redact(value);
    let f = AdmissionFixture::new(&s);
    let mut submission = prompt("go");
    submission.secrets = Some(Arc::new(
        CommandSecrets::new([("TOKEN".into(), SecretString::new(value))]).unwrap(),
    ));
    let journal = f.journal("secret-run");
    let body = serde_json::json!({"command":{"secrets":{"TOKEN":value}}});
    let mut canonical = body.clone();
    CommandSubmission::canonicalize_secrets(&mut canonical["command"]);
    assert_eq!(
        journal.begin(&canonical.to_string()).unwrap(),
        IdempotencyBegin::Fresh
    );
    let run = f
        .admission
        .admit_session(f.local(&s), Some(submission), false, Some(&journal))
        .await
        .unwrap();
    let id = run.run.as_ref().unwrap().command_id.clone();
    let lifecycle = CommandLifecycleStore::open(f.store.clone()).unwrap();
    assert_eq!(lifecycle.secret_names(&id).unwrap().unwrap(), ["TOKEN"]);
    assert!(f.engines.ephemeral_secrets.get(&id).is_some());
    f.store
        .with_conn(|conn| {
            let mut stmt = conn.prepare("PRAGMA table_info(command_secret_names)")?;
            let columns = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(columns, ["command_id", "names_json"]);
            Ok(())
        })
        .unwrap();
    let mut replay = serde_json::json!({"command":{"secrets":{"TOKEN":"test-secret-replacement"}}});
    CommandSubmission::canonicalize_secrets(&mut replay["command"]);
    assert!(matches!(
        journal.begin(&replay.to_string()).unwrap(),
        IdempotencyBegin::Replay { .. }
    ));
    fn scan(path: &std::path::Path, secret: &[u8]) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                scan(&entry.unwrap().path(), secret);
            }
        } else {
            let bytes = std::fs::read(path).unwrap();
            assert!(
                !bytes.windows(secret.len()).any(|part| part == secret),
                "secret in {}",
                path.display()
            );
        }
    }
    scan(f.paths.root(), value.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn restarted_command_fails_before_dispatch_when_ephemeral_values_are_lost() {
    let s = crate::support::sandbox(
        "command::secrets::restarted_command_fails_before_dispatch_when_ephemeral_values_are_lost",
    );
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let value = "test-secret-restart";
    s.redact(value);
    let f = AdmissionFixture::new(&s);
    let mut submission = prompt("go");
    submission.secrets = Some(Arc::new(
        CommandSecrets::new([("TOKEN".into(), SecretString::new(value))]).unwrap(),
    ));
    let run = f
        .admission
        .admit_session(f.local(&s), Some(submission), false, None)
        .await
        .unwrap();
    let id = run.run.unwrap().command_id;
    f.store
        .update_setup_status(
            &run.session_id,
            awman::data::session_setup_event::SessionSetupStatus::Ready,
        )
        .unwrap();
    // New daemon bundle over the same durable rows, with no in-memory values.
    let engines =
        awman::command::dispatch::Engines::for_env(crate::support::env_snapshot(&s)).unwrap();
    assert!(engines.ephemeral_secrets.get(&id).is_none());
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
            engines,
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
        "secrets_lost terminal row",
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
    assert_eq!(result["failure"]["stage"], "preflight");
    assert_eq!(result["failure"]["code"], "secrets_lost");
    // No setup, metadata or dispatch: the guard runs at the top of execution.
    assert!(!f
        .paths
        .command_dir(&run.session_id, id.as_str())
        .join("metadata.json")
        .exists());
    assert!(!row.result.unwrap().contains(value));
    assert!(
        docker.calls().is_empty(),
        "lost secrets must not launch a container"
    );
    assert!(CommandId::parse(id.as_str()).is_ok());
}
