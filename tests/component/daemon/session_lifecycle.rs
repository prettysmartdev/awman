//! The typed API and HTTP adapter share ownership of admitted setup work.
use super::support::Api;
use crate::support;
use awman::command::commands::api_server::admission::{CommandSubmission, SessionSubmission};
use awman::command::commands::api_server::session_setup::{
    BusSessionSetupPresenter, SessionSetupBus,
};
use awman::command::daemon_api::{keys, worker};
use awman::command::session_create::SessionCreateRequest;
use awman::command::session_setup::SessionSetupPresenter;
use awman::data::command_status::CommandStatus;
use awman::data::daemon_state::CommandRequestContext;
use awman::data::execution_event::EventPayload;
use awman::data::fs::api_labels::Labels;
use awman::data::ids::CommandId;
use awman::data::session_setup_event::{SessionSetupStatus, SetupEventPayload};
use awman::data::session_status::SessionStatus;
use awman::engine::git::GitFrontend;
use awman::engine::ready::frontend::ReadyFrontend;
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::wait_for_async;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn config(workdir: Option<&Path>) -> Value {
    let mut config = json!({"daemon":{"features":{"worker":{"enabled":true,
        "push":{"enabled":true}}},"network":{"listeners":{"component":{
        "features":["engine","worker"]}}}}});
    if let Some(workdir) = workdir {
        config["daemon"]["features"]["worker"]["push"]["workDirs"] = json!({"allow":[workdir]});
    }
    config
}

fn remote() -> SessionSubmission {
    SessionSubmission {
        request: SessionCreateRequest {
            session_type: Some("remote".into()),
            // Every test cancels before clone, so no external system is used.
            repo_url: Some("https://example.invalid/project.git".into()),
            ..Default::default()
        },
        labels: Labels::empty(),
    }
}

fn command() -> CommandSubmission {
    CommandSubmission {
        subcommand: "exec prompt".into(),
        args: vec!["hello".into()],
        secrets: None,
        allow_remote_input: false,
        interaction: None,
        labels: Labels::empty(),
        work_item: None,
    }
}

async fn settled(api: &worker::WorkerApi, id: &str) {
    wait_for_async("setup owner finalized", Duration::from_secs(8), || async {
        let tasks = api.task_handles.lock().await;
        let row = api.store.get_session(id).unwrap().unwrap();
        (row.setup_status.is_terminal() && tasks.iter().all(|t| t.is_finished())).then_some(())
    })
    .await;
    assert!(!api.setup_cancels.is_registered(id));
    let saved = api.paths.read_setup_state(id).expect("terminal snapshot");
    assert!(saved.status.is_terminal());
}

fn assert_cancelled_once(api: &worker::WorkerApi, session: &str, command: &CommandId) {
    assert_eq!(
        api.store.get_session(session).unwrap().unwrap().status,
        SessionStatus::Closed
    );
    assert_eq!(
        api.store
            .get_command(command.as_str())
            .unwrap()
            .unwrap()
            .status,
        CommandStatus::Cancelled
    );
    let saved = api.paths.read_setup_state(session).unwrap();
    assert_eq!(saved.error.unwrap().stage, "cancelled");
    let events = api.paths.read_command_events(session, command.as_str());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Done))
            .count(),
        1,
        "exactly one durable finalization: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_and_http_setup_have_identical_authority_state_and_events() {
    let s = support::sandbox(
        "daemon::session_lifecycle::typed_and_http_setup_have_identical_authority_state_and_events",
    );
    let outside = s.root().join("outside/Dockerfile");
    // A deterministic ready failure after session open, before image or agent work.
    let repo = RepoBuilder::new(&s, "project")
        .file(
            ".awman/config.json",
            json!({"runtime":{"dockerfile":outside}}).to_string(),
        )
        .init();
    let server = Api::with_config(&s, config(Some(repo.path()))).await;
    let api = server.api.worker.as_ref().unwrap();
    let typed = worker::sessions_create(
        api,
        CommandRequestContext::default(),
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(repo.path().display().to_string()),
                ..Default::default()
            },
            labels: Labels::empty(),
        },
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let (code, http) = server
        .request(
            "POST",
            "/v1/worker/sessions",
            Some(&key.key),
            None,
            Some(json!({"workdir":repo.path()})),
        )
        .await;
    assert_eq!(code, 201, "{http}");
    let http_id = http["id"].as_str().unwrap();
    for id in [typed.session_id.as_str(), http_id] {
        settled(api, id).await;
        let row = api.store.get_session(id).unwrap().unwrap();
        assert_eq!(row.setup_status, SessionSetupStatus::Failed);
        let session = api.sessions.get_by_key(id).expect("setup opened session");
        let session = session.read().await;
        let canonical_repo = repo.path().canonicalize().unwrap();
        assert_eq!(session.git_root(), canonical_repo);
        assert_eq!(
            session.granted_mount_authority().unwrap().root(),
            canonical_repo
        );
        let bus = api.setup_buses.lock().await.get(id).unwrap().clone();
        let saved = api.paths.read_setup_state(id).unwrap();
        assert_eq!(saved.error.as_ref().unwrap().stage, "ready");
        assert_eq!(
            serde_json::to_value(bus.snapshot()).unwrap(),
            serde_json::to_value(saved).unwrap()
        );
    }
    assert!(!outside.exists());
    // A restored session can retain its recorded setup result while its grant
    // needs revalidation. Both callers refuse before allocating a command.
    api.sessions
        .get_by_key(&typed.session_id)
        .unwrap()
        .write()
        .await
        .require_mount_revalidation("current policy no longer grants this workdir");
    let typed_id = awman::data::ids::WorkerSessionId::parse(&typed.session_id).unwrap();
    assert!(matches!(
        worker::commands_create(api, CommandRequestContext::default(), &typed_id, command(), None)
            .await,
        Err(awman::command::commands::api_server::admission::AdmissionError::MountAuthorityRevalidationRequired { .. })
    ));
    let (code, refused) = server
        .request(
            "POST",
            "/v1/worker/commands",
            Some(&key.key),
            None,
            Some(json!({"session_id":typed_id, "subcommand":"exec prompt", "args":["hello"]})),
        )
        .await;
    assert_eq!(code, 409, "{refused}");
    assert_eq!(refused["code"], "mount_authority_revalidation_required");
    assert!(api
        .store
        .list_commands_for_session(&typed.session_id, 10)
        .unwrap()
        .is_empty());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_run_registers_before_return_and_outlives_its_caller() {
    let s = support::sandbox(
        "daemon::session_lifecycle::typed_run_registers_before_return_and_outlives_its_caller",
    );
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    let buses = api.setup_buses.lock().await;
    let caller_api = api.clone();
    let caller = tokio::spawn(async move {
        let admitted = worker::runs_create(
            &caller_api,
            CommandRequestContext::default(),
            remote(),
            command(),
            false,
            None,
        )
        .await
        .unwrap();
        assert!(caller_api.setup_cancels.is_registered(&admitted.session_id));
        admitted
    });
    let admitted = caller.await.unwrap();
    let session = admitted.session_id.clone();
    let command = admitted.run.as_ref().unwrap().command_id.clone();
    drop(admitted);
    assert!(matches!(
        worker::commands_cancel(api, &command).await.unwrap(),
        awman::command::commands::api_server::CancelOutcome::Cancelled(_)
    ));
    drop(buses);
    settled(api, &session).await;
    assert_cancelled_once(api, &session, &command);
    // Repeated cancellation must not append another final pair.
    worker::commands_cancel(api, &command).await.unwrap();
    assert_cancelled_once(api, &session, &command);
    server.shutdown().await;
}

#[derive(Default)]
struct Counts {
    created: AtomicUsize,
    failed: AtomicUsize,
    finished: AtomicUsize,
}
struct RecordingPresenter {
    inner: BusSessionSetupPresenter,
    counts: Arc<Counts>,
}
#[async_trait::async_trait]
impl SessionSetupPresenter for RecordingPresenter {
    fn log(&mut self, line: &str) {
        self.inner.log(line);
    }
    fn emit(&mut self, event: SetupEventPayload) {
        if matches!(event, SetupEventPayload::SetupFailed { .. }) {
            self.counts.failed.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.emit(event);
    }
    fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend> {
        self.inner.ready_frontend()
    }
    fn git_log_sink(&mut self) -> Box<dyn GitFrontend + Send> {
        self.inner.git_log_sink()
    }
    async fn finished(&mut self) {
        self.counts.finished.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_cancel_before_token_registration_is_reconciled_and_finalized_once() {
    let s = support::sandbox("daemon::session_lifecycle::durable_cancel_before_token_registration_is_reconciled_and_finalized_once");
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    let buses = api.setup_buses.lock().await;
    // Commit cancellation at the earliest observable queue boundary. No token
    // exists yet, precisely the race a listing/cancel client can win.
    api.store
        .with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER early_setup_cancel AFTER INSERT ON commands BEGIN
         UPDATE commands SET status='cancelled', exit_code=130,
         finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=NEW.id; END;",
            )?;
            Ok(())
        })
        .unwrap();
    let counts = Arc::new(Counts::default());
    let record = counts.clone();
    let admitted = worker::sessions_create_presented(
        api,
        CommandRequestContext::default(),
        remote(),
        Some(command()),
        true,
        None,
        move |id, bus: Arc<SessionSetupBus>| {
            record.created.fetch_add(1, Ordering::SeqCst);
            Box::new(RecordingPresenter {
                inner: BusSessionSetupPresenter::new(id, bus, false),
                counts: record,
            })
        },
    )
    .await
    .unwrap();
    let command = admitted.run.unwrap().command_id;
    settled(api, &admitted.session_id).await;
    assert_cancelled_once(api, &admitted.session_id, &command);
    assert_eq!(counts.created.load(Ordering::SeqCst), 1);
    assert_eq!(counts.failed.load(Ordering::SeqCst), 1);
    assert_eq!(counts.finished.load(Ordering::SeqCst), 1);
    assert!(
        api.sessions.get_by_key(&admitted.session_id).is_none(),
        "cancel precedes open/clone"
    );
    drop(buses);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_disconnect_and_idempotent_retry_keep_one_setup_owner() {
    let s = support::sandbox(
        "daemon::session_lifecycle::http_disconnect_and_idempotent_retry_keep_one_setup_owner",
    );
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    let key = keys::keys_create(&server.api, "worker").unwrap();
    s.redact(&key.key);
    let buses = api.setup_buses.lock().await;
    let body = json!({"session":{"session_type":"remote", "repo_url":"https://example.invalid/project.git"},
        "command":{"subcommand":"exec prompt", "args":["hello"]}, "close_on_finish":true});
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let request = client
        .post(format!("{}/v1/worker/runs", server.base))
        .bearer_auth(&key.key)
        .header("Idempotency-Key", "setup-disconnect")
        .json(&body);
    let caller = tokio::spawn(async move { request.send().await });
    let run = wait_for_async(
        "HTTP run durably admitted",
        Duration::from_secs(5),
        || async { api.lifecycle_store.list_runs().unwrap().into_iter().next() },
    )
    .await;
    caller.abort();
    let _ = caller.await;
    // The run link is visible before full admission commits. A retry during
    // that interval correctly returns in-flight; retry until it can replay.
    let response = wait_for_async(
        "disconnected admission can replay",
        Duration::from_secs(5),
        || async {
            let response = client
                .post(format!("{}/v1/worker/runs", server.base))
                .bearer_auth(&key.key)
                .header("Idempotency-Key", "setup-disconnect")
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            if status == 409 {
                assert_eq!(body["code"], "idempotency_in_flight", "{body}");
                None
            } else {
                assert_eq!(status, 202, "{body}");
                Some(body)
            }
        },
    )
    .await;
    assert_eq!(response["session_id"], run.session_id.as_str());
    assert_eq!(response["command_id"], run.command_id.as_str());
    assert_eq!(api.lifecycle_store.list_runs().unwrap().len(), 1);
    assert_eq!(api.task_handles.lock().await.len(), 1);
    worker::commands_cancel(api, &run.command_id).await.unwrap();
    drop(buses);
    settled(api, run.session_id.as_str()).await;
    assert_cancelled_once(api, run.session_id.as_str(), &run.command_id);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_repository_process_conflicts_and_bad_labels_allocate_no_setup() {
    let s = support::sandbox("daemon::session_lifecycle::known_repository_process_conflicts_and_bad_labels_allocate_no_setup");
    let repo = RepoBuilder::new(&s, "project")
        .file(
            ".awman/config.json",
            r#"{"agents":{"auth":{"refresh":{"enabled":true}}}}"#,
        )
        .init();
    let server = Api::with_config(&s, config(Some(repo.path()))).await;
    let api = server.api.worker.as_ref().unwrap();
    let error = worker::sessions_create(
        api,
        CommandRequestContext::default(),
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(repo.path().display().to_string()),
                ..Default::default()
            },
            labels: Labels::empty(),
        },
        None,
        false,
        None,
    )
    .await
    .err()
    .expect("binding refused");
    assert!(
        matches!(
            error,
            awman::command::commands::api_server::admission::AdmissionError::SessionCreation(
                awman::command::error::CommandError::ProcessScopedSetting { .. }
            )
        ),
        "{error:?}"
    );
    let mut invalid = remote();
    invalid.labels =
        Labels::from_stored([(String::from("awman/reserved"), String::from("x"))].into());
    assert!(worker::sessions_create(
        api,
        CommandRequestContext::default(),
        invalid,
        None,
        false,
        None
    )
    .await
    .is_err());
    assert!(api.store.list_sessions().unwrap().is_empty());
    assert!(api.task_handles.lock().await.is_empty());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_joins_setup_finalization_even_when_presentation_is_blocked() {
    let s = support::sandbox("daemon::session_lifecycle::shutdown_joins_setup_finalization_even_when_presentation_is_blocked");
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.clone().unwrap();
    let database = Arc::downgrade(&api.store);
    let buses = api.setup_buses.lock().await;
    let admitted = worker::runs_create(
        &api,
        CommandRequestContext::default(),
        remote(),
        command(),
        false,
        None,
    )
    .await
    .unwrap();
    let run = admitted.run.unwrap();
    server.shutdown().await;
    assert!(!api.setup_cancels.is_registered(&admitted.session_id));
    assert_cancelled_once(&api, &admitted.session_id, &run.command_id);
    drop(buses);
    drop(api);
    assert!(
        database.upgrade().is_none(),
        "shutdown must join the setup owner and release its store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_worker_refuses_new_setup_before_any_durable_write() {
    use awman::command::commands::api_server::admission::AdmissionError;
    let s = support::sandbox(
        "daemon::session_lifecycle::stopping_worker_refuses_new_setup_before_any_durable_write",
    );
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    let tasks = api.task_handles.lock().await;
    api.setup_stopping.store(true, Ordering::Release);
    drop(tasks);
    let result = worker::runs_create(
        api,
        CommandRequestContext::default(),
        remote(),
        command(),
        true,
        None,
    )
    .await;
    assert!(matches!(result, Err(AdmissionError::FeatureDisabled)));
    assert!(api.store.list_sessions().unwrap().is_empty());
    assert!(api.task_handles.lock().await.is_empty());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_key_write_failure_compensates_the_session_without_scheduling() {
    let s = support::sandbox("daemon::session_lifecycle::context_key_write_failure_compensates_the_session_without_scheduling");
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    api.store.with_conn(|c| { c.execute_batch("CREATE TRIGGER fail_context BEFORE UPDATE OF context_key ON sessions BEGIN SELECT RAISE(ABORT,'context write failed'); END;")?; Ok(()) }).unwrap();
    let result = worker::runs_create(
        api,
        CommandRequestContext::default(),
        remote(),
        command(),
        true,
        None,
    )
    .await;
    assert!(result.is_err());
    let sessions = api.store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, SessionStatus::Closed);
    assert!(api.task_handles.lock().await.is_empty());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presenter_panic_closes_setup_and_finalizes_the_run() {
    let s = support::sandbox(
        "daemon::session_lifecycle::presenter_panic_closes_setup_and_finalizes_the_run",
    );
    let server = Api::with_config(&s, config(None)).await;
    let api = server.api.worker.as_ref().unwrap();
    let admitted = worker::sessions_create_presented(
        api,
        CommandRequestContext::default(),
        remote(),
        Some(command()),
        true,
        None,
        |_, _| panic!("injected setup presenter panic"),
    )
    .await
    .unwrap();
    let run = admitted.run.unwrap();
    wait_for_async(
        "panicked setup finalized",
        Duration::from_secs(4),
        || async {
            (api.store
                .get_session(&admitted.session_id)
                .unwrap()
                .unwrap()
                .status
                == SessionStatus::Closed
                && api
                    .store
                    .get_command(run.command_id.as_str())
                    .unwrap()
                    .unwrap()
                    .status
                    .is_terminal()
                && api
                    .task_handles
                    .lock()
                    .await
                    .iter()
                    .all(|t| t.is_finished()))
            .then_some(())
        },
    )
    .await;
    assert!(!api.setup_cancels.is_registered(&admitted.session_id));
    assert_eq!(
        api.store
            .get_command(run.command_id.as_str())
            .unwrap()
            .unwrap()
            .status,
        CommandStatus::Error
    );
    let events = api
        .paths
        .read_command_events(&admitted.session_id, run.command_id.as_str());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Done))
            .count(),
        1
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_run_finalization_failure_is_retried_by_the_setup_owner() {
    let s = support::sandbox("daemon::session_lifecycle::transient_run_finalization_failure_is_retried_by_the_setup_owner");
    let repo = RepoBuilder::new(&s, "retry-project")
        .file(
            ".awman/config.json",
            json!({"runtime":{"dockerfile":s.tmp().join("outside/Dockerfile")}}).to_string(),
        )
        .init();
    let server = Api::with_config(&s, config(Some(repo.path()))).await;
    let api = server.api.worker.as_ref().unwrap();
    api.store.with_conn(|c| {
        c.execute_batch("CREATE TABLE finalization_faults(remaining INTEGER NOT NULL); INSERT INTO finalization_faults VALUES(1); CREATE TRIGGER fail_first_run_finish BEFORE UPDATE OF status ON commands WHEN OLD.status='queued' AND NEW.status='error' AND (SELECT remaining FROM finalization_faults)=1 BEGIN UPDATE finalization_faults SET remaining=0; SELECT RAISE(FAIL,'transient finalization write failure'); END;")?;
        Ok(())
    }).unwrap();
    let admitted = worker::runs_create(
        api,
        CommandRequestContext::default(),
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(repo.path().display().to_string()),
                ..Default::default()
            },
            labels: Labels::empty(),
        },
        command(),
        false,
        None,
    )
    .await
    .unwrap();
    settled(api, &admitted.session_id).await;
    let cid = admitted.run.unwrap().command_id;
    assert_eq!(
        api.store.get_command(cid.as_str()).unwrap().unwrap().status,
        CommandStatus::Error
    );
    api.store
        .with_conn(|c| {
            assert_eq!(
                c.query_row("SELECT remaining FROM finalization_faults", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
    let events = api
        .paths
        .read_command_events(&admitted.session_id, cid.as_str());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Done))
            .count(),
        1
    );
    server.shutdown().await;
}
