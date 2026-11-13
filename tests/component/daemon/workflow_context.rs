//! Workflow scratch access through the production router, pin writer and retention policy.
use super::support::Api;
use crate::support;
use awman::command::{
    commands::api_server::event_bus::EventBus,
    context::{retention::ScratchRetention, workflow_api::WorkflowContextService},
    daemon_api::keys,
    worker::events::persist_workflow_snapshot,
};
use awman::data::{
    api_lifecycle_store::{CommandLifecycleStore, RunRecord},
    command_status::CommandStatus,
    context::{CommandInvocationPins, ContextLimits},
    fs::{api_command_log::CommandStateWriter, context_dirs::ContextDirResolver},
    ids::{CommandId, WorkerSessionId},
    workflow_state::WorkflowState,
};
use awman::engine::workflow::context_files::{WorkflowContextError, WorkflowContextReader};
use awman_test_support::TestSandbox;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Duration};

fn worker_config() -> Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true},"pull":{"enabled":false}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

struct Scratch {
    session: WorkerSessionId,
    command: CommandId,
    dir: PathBuf,
    state: WorkflowState,
}
impl Scratch {
    fn create(server: &Api, sandbox: &TestSandbox, workflow: bool) -> Self {
        let worker = server.api.worker.as_ref().unwrap();
        let session = WorkerSessionId::new();
        let command = CommandId::new();
        worker
            .store
            .insert_session(
                session.as_str(),
                sandbox.work().to_str().unwrap(),
                &chrono::Utc::now().to_rfc3339(),
            )
            .unwrap();
        // A running row is never picked up by the push queue; no agent execution
        // is needed to exercise the file and command lifecycle boundaries.
        worker
            .store
            .insert_running_lease_command(
                command.as_str(),
                session.as_str(),
                if workflow {
                    "exec workflow"
                } else {
                    "exec prompt"
                },
                "[]",
                false,
            )
            .unwrap();
        let state = WorkflowState::new("scratch-test".into(), &[], "hash".into(), None);
        let dir =
            ContextDirResolver::at_home(sandbox.awman_home()).workflow_dir(state.invocation_id);
        let result = Self {
            session,
            command,
            dir,
            state,
        };
        if workflow {
            result.persist(server, 1);
        }
        result
    }
    fn persist(&self, server: &Api, attempt: u32) {
        let writer = CommandStateWriter::new(
            server.api.worker.as_ref().unwrap().paths.clone(),
            self.session.as_str(),
            self.command.as_str(),
        );
        persist_workflow_snapshot(
            &writer,
            &EventBus::for_attempt(4, attempt).sender(),
            &self.state,
        );
    }
    fn note(&self, path: &str, bytes: &[u8]) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    fn finish(&self, server: &Api, status: CommandStatus) {
        server
            .api
            .worker
            .as_ref()
            .unwrap()
            .store
            .update_command_finished(
                self.command.as_str(),
                status,
                Some(if status == CommandStatus::Done { 0 } else { 1 }),
                &chrono::Utc::now().to_rfc3339(),
            )
            .unwrap();
    }
    fn url(&self) -> String {
        format!("/v1/worker/commands/{}/context", self.command)
    }
}

fn key(server: &Api, sandbox: &TestSandbox) -> String {
    let created = keys::keys_create(&server.api, "scratch-reader").unwrap();
    sandbox.redact(&created.key);
    created.key
}
async fn download(server: &Api, key: &str, path: &str) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("{}{path}", server.base))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
}
fn assert_private(value: &Value, sandbox: &TestSandbox) {
    assert!(!value
        .to_string()
        .contains(sandbox.awman_home().to_str().unwrap()));
    assert!(!value.to_string().contains("invocation_id"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_and_downloads_inflight_complete_failed_and_cancelled_with_honest_metadata() {
    let sandbox = support::sandbox("daemon::workflow_context::lists_and_downloads_inflight_complete_failed_and_cancelled_with_honest_metadata");
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    for status in [
        CommandStatus::Running,
        CommandStatus::Done,
        CommandStatus::Error,
        CommandStatus::Cancelled,
    ] {
        let scratch = Scratch::create(&server, &sandbox, true);
        scratch.note("nested/evidence.md", b"hello");
        scratch.note("z.md", b"other");
        if status != CommandStatus::Running {
            scratch.finish(&server, status);
        }
        let (code, list) = server.get(&scratch.url(), Some(&key)).await;
        assert_eq!(code, 200, "{list}");
        assert_eq!(list["next_cursor"], Value::Null);
        assert_eq!(list["items"].as_array().unwrap().len(), 2);
        let meta = &list["items"][0];
        assert_eq!(meta["path"], "nested/evidence.md");
        assert_eq!(meta["size_bytes"], 5);
        assert_eq!(
            meta["sha256"],
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        chrono::DateTime::parse_from_rfc3339(meta["modified_at"].as_str().unwrap()).unwrap();
        assert_private(&list, &sandbox);
        let response = download(
            &server,
            &key,
            &format!("{}/files/nested/evidence.md", scratch.url()),
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["etag"],
            format!("\"{}\"", meta["sha256"].as_str().unwrap())
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["content-disposition"], "attachment");
        assert_eq!(
            response.headers()["content-security-policy"],
            "sandbox; default-src 'none'"
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"hello");
        let (_, first) = server
            .get(&format!("{}?limit=1", scratch.url()), Some(&key))
            .await;
        let cursor = first["next_cursor"].as_str().unwrap();
        let (_, second) = server
            .get(
                &format!("{}?limit=1&cursor={cursor}", scratch.url()),
                Some(&key),
            )
            .await;
        assert_eq!(second["items"][0]["path"], "z.md");
    }
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_without_workflow_or_retained_directory_and_removed_file_is_not_found() {
    let sandbox = support::sandbox("daemon::workflow_context::empty_without_workflow_or_retained_directory_and_removed_file_is_not_found");
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    for workflow in [false, true] {
        let scratch = Scratch::create(&server, &sandbox, workflow);
        let (code, list) = server.get(&scratch.url(), Some(&key)).await;
        assert_eq!(code, 200, "{list}");
        assert_eq!(list, json!({"items":[], "next_cursor":null}));
        if workflow {
            scratch.note("removed.md", b"gone");
            assert_eq!(
                server.get(&scratch.url(), Some(&key)).await.1["items"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            std::fs::remove_file(scratch.dir.join("removed.md")).unwrap();
            std::fs::remove_dir_all(&scratch.dir).unwrap();
            scratch.finish(&server, CommandStatus::Done);
            assert_eq!(
                server.get(&scratch.url(), Some(&key)).await.1,
                json!({"items":[], "next_cursor":null})
            );
        }
        let (code, error) = server
            .get(&format!("{}/files/removed.md", scratch.url()), Some(&key))
            .await;
        assert_eq!(code, 404, "{error}");
        assert_eq!(error["code"], "not_found");
        assert_private(&error, &sandbox);
    }
    let (code, error) = server
        .get(
            &format!("/v1/worker/commands/{}/context", CommandId::new()),
            Some(&key),
        )
        .await;
    assert_eq!(code, 404);
    assert_eq!(error["code"], "command_not_found");
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerun_and_restart_use_only_the_current_attempt_pin() {
    let sandbox = support::sandbox(
        "daemon::workflow_context::rerun_and_restart_use_only_the_current_attempt_pin",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    let mut scratch = Scratch::create(&server, &sandbox, true);
    scratch.note("old.md", b"old attempt");
    let worker = server.api.worker.as_ref().unwrap();
    worker
        .store
        .with_conn(|conn| {
            conn.execute(
                "UPDATE commands SET attempt=2 WHERE id=?1",
                [scratch.command.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server.get(&scratch.url(), Some(&key)).await.1["items"],
        json!([])
    );
    assert_eq!(
        server
            .get(&format!("{}/files/old.md", scratch.url()), Some(&key))
            .await
            .0,
        404
    );
    // A restart while only the stale pin exists must still expose nothing.
    scratch.finish(&server, CommandStatus::Done);
    worker
        .store
        .close_session_force(scratch.session.as_str(), &chrono::Utc::now().to_rfc3339())
        .unwrap();
    server.shutdown().await;
    let server = Api::restart_with_config(&sandbox, worker_config()).await;
    assert_eq!(
        server.get(&scratch.url(), Some(&key)).await.1["items"],
        json!([])
    );
    let old = scratch.state.clone();
    scratch.state = WorkflowState::new("rerun".into(), &[], "hash".into(), None);
    scratch.dir =
        ContextDirResolver::at_home(sandbox.awman_home()).workflow_dir(scratch.state.invocation_id);
    scratch.persist(&server, 2);
    scratch.note("new.md", b"new attempt");
    // Late writers cannot replace the new attempt's pin, even from attempt 2.
    let pins = CommandInvocationPins::new(server.api.worker.as_ref().unwrap().paths.clone());
    pins.pin(
        scratch.session.as_str(),
        scratch.command.as_str(),
        1,
        old.invocation_id,
    )
    .unwrap();
    pins.pin(
        scratch.session.as_str(),
        scratch.command.as_str(),
        2,
        old.invocation_id,
    )
    .unwrap();
    server.shutdown().await;
    let server = Api::restart_with_config(&sandbox, worker_config()).await;
    let (code, list) = server.get(&scratch.url(), Some(&key)).await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["path"], "new.md");
    assert_eq!(
        download(&server, &key, &format!("{}/files/new.md", scratch.url()))
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"new attempt"
    );
    assert_eq!(
        server
            .get(&format!("{}/files/old.md", scratch.url()), Some(&key))
            .await
            .0,
        404
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_on_finish_retains_notes_until_the_closed_session_is_purged() {
    let sandbox = support::sandbox("daemon::workflow_context::close_on_finish_retains_notes_until_the_closed_session_is_purged");
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    let mut scratches = Vec::new();
    for status in [
        CommandStatus::Done,
        CommandStatus::Error,
        CommandStatus::Cancelled,
    ] {
        let scratch = Scratch::create(&server, &sandbox, true);
        scratch.note("note.md", b"retained");
        scratch.finish(&server, status);
        CommandLifecycleStore::open(server.api.worker.as_ref().unwrap().store.clone())
            .unwrap()
            .record_run(&RunRecord {
                session_id: scratch.session.clone(),
                command_id: scratch.command.clone(),
                close_on_finish: true,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();
        scratches.push(scratch);
    }
    // Startup runs the production close_on_finish reconciliation for terminal runs.
    server.shutdown().await;
    let server = Api::restart_with_config(&sandbox, worker_config()).await;
    let worker = server.api.worker.as_ref().unwrap();
    let retention = ScratchRetention::new(
        worker.paths.clone(),
        ContextDirResolver::at_home(sandbox.awman_home()),
    );
    for scratch in &scratches {
        assert_eq!(
            worker
                .store
                .get_session(scratch.session.as_str())
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "closed"
        );
        assert_eq!(server.get(&scratch.url(), Some(&key)).await.0, 200);
        assert_eq!(
            download(&server, &key, &format!("{}/files/note.md", scratch.url()))
                .await
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"retained"
        );
    }
    assert!(worker
        .store
        .delete_closed_sessions_older_than(24)
        .unwrap()
        .is_empty());
    assert_eq!(retention.sweep_at_startup(&worker.store), 0);
    let expired = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
    worker
        .store
        .with_conn(|conn| {
            for scratch in &scratches {
                conn.execute(
                    "UPDATE sessions SET closed_at=?1 WHERE id=?2",
                    [expired.as_str(), scratch.session.as_str()],
                )?;
            }
            Ok(())
        })
        .unwrap();
    server.shutdown().await;
    // The real startup retention sweep removes scratch only after purging rows.
    let server = Api::restart_with_config(&sandbox, worker_config()).await;
    let worker = server.api.worker.as_ref().unwrap();
    for scratch in &scratches {
        assert!(worker
            .store
            .get_command(scratch.command.as_str())
            .unwrap()
            .is_none());
        assert!(!scratch.dir.exists());
        assert!(!worker
            .paths
            .command_invocation_pin_path(scratch.session.as_str(), scratch.command.as_str())
            .exists());
        assert_eq!(server.get(&scratch.url(), Some(&key)).await.0, 404);
    }
    assert_eq!(
        ScratchRetention::new(
            worker.paths.clone(),
            ContextDirResolver::at_home(sandbox.awman_home())
        )
        .sweep_at_startup(&worker.store),
        0
    );
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paths_symlinks_nonregular_files_and_replacements_fail_closed() {
    let sandbox = support::sandbox(
        "daemon::workflow_context::paths_symlinks_nonregular_files_and_replacements_fail_closed",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    let scratch = Scratch::create(&server, &sandbox, true);
    scratch.note("safe.md", b"safe");
    let worker = server.api.worker.as_ref().unwrap();
    let service = WorkflowContextService::new(
        worker.paths.clone(),
        ContextDirResolver::at_home(sandbox.awman_home()),
    );
    // Direct typed calls preserve literal dot/absolute segments that HTTP clients normalize.
    for path in [
        "../outside",
        "/absolute",
        "a/../b",
        "a//b",
        "a\\b",
        "C:/a",
        "a\0b",
    ] {
        assert_eq!(
            service
                .get(&worker.store, &scratch.command, path)
                .unwrap_err()
                .code(),
            "invalid_request"
        );
    }
    for path in ["%2e%2e%2foutside", "%2fabsolute", "a%5cb", "a%00b"] {
        let (code, error) = server
            .get(&format!("{}/files/{path}", scratch.url()), Some(&key))
            .await;
        assert_eq!(code, 400, "{path}: {error}");
        assert_eq!(error["code"], "invalid_request");
        assert_private(&error, &sandbox);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = sandbox.root().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), b"host secret").unwrap();
        symlink(outside.join("secret.md"), scratch.dir.join("link.md")).unwrap();
        symlink(&outside, scratch.dir.join("linked-dir")).unwrap();
        nix::unistd::mkfifo(&scratch.dir.join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        std::fs::create_dir(scratch.dir.join("directory")).unwrap();
        let (code, list) = server.get(&scratch.url(), Some(&key)).await;
        assert_eq!(code, 200, "{list}");
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        for path in ["link.md", "linked-dir/secret.md", "fifo", "directory"] {
            let (code, error) = server
                .get(&format!("{}/files/{path}", scratch.url()), Some(&key))
                .await;
            assert_eq!(code, 404, "{path}: {error}");
            assert_eq!(error["code"], "not_found");
        }
        // The file was listed as regular; swap in a link before downloading it.
        std::fs::remove_file(scratch.dir.join("safe.md")).unwrap();
        symlink(outside.join("secret.md"), scratch.dir.join("safe.md")).unwrap();
        assert_eq!(
            server
                .get(&format!("{}/files/safe.md", scratch.url()), Some(&key))
                .await
                .1["code"],
            "not_found"
        );
        // A reader constructed before the invocation root is replaced must also refuse it.
        let reader = WorkflowContextReader::for_invocation(
            &ContextDirResolver::at_home(sandbox.awman_home()),
            scratch.state.invocation_id,
        )
        .unwrap();
        let moved = scratch.dir.with_extension("moved");
        std::fs::rename(&scratch.dir, &moved).unwrap();
        symlink(&outside, &scratch.dir).unwrap();
        assert!(reader.read("secret.md").is_err());
        assert!(reader.list(&AtomicBool::new(false)).is_err());
        let (code, error) = server.get(&scratch.url(), Some(&key)).await;
        assert_eq!(code, 503);
        assert_private(&error, &sandbox);
    }
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_total_and_cancellation_limits_never_return_partial_content() {
    let sandbox = support::sandbox(
        "daemon::workflow_context::file_total_and_cancellation_limits_never_return_partial_content",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    let scratch = Scratch::create(&server, &sandbox, true);
    scratch.note("large", b"");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(scratch.dir.join("large"))
        .unwrap();
    file.set_len(ContextLimits::WORKFLOW.max_file_bytes + 1)
        .unwrap();
    let (code, error) = server
        .get(&format!("{}/files/large", scratch.url()), Some(&key))
        .await;
    assert_eq!(code, 413, "{error}");
    assert_eq!(error["code"], "context_too_large");
    // Total hash budget is checked before reading a too-large sparse file.
    scratch.note("small.md", b"safe");
    file.set_len(ContextLimits::WORKFLOW.max_total_bytes + 1)
        .unwrap();
    let (code, error) = server.get(&scratch.url(), Some(&key)).await;
    assert_eq!(code, 503, "{error}");
    assert_eq!(error["code"], "unavailable");
    assert!(error.get("items").is_none());
    assert_private(&error, &sandbox);
    let reader = WorkflowContextReader::for_invocation(
        &ContextDirResolver::at_home(sandbox.awman_home()),
        scratch.state.invocation_id,
    )
    .unwrap();
    assert!(matches!(
        reader.list(&AtomicBool::new(true)),
        Err(WorkflowContextError::Incomplete)
    ));
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_is_separate_and_authorization_matches_outputs() {
    let sandbox = support::sandbox(
        "daemon::workflow_context::capability_is_separate_and_authorization_matches_outputs",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = key(&server, &sandbox);
    let scratch = Scratch::create(&server, &sandbox, true);
    let (_, status) = server.get("/v1/status", Some(&key)).await;
    let capabilities = status["capabilities"].as_array().unwrap();
    assert_eq!(
        capabilities
            .iter()
            .filter(|c| **c == "workflow_context")
            .count(),
        1
    );
    for (name, enabled) in [
        ("context_api", awman::command::context::repo_api::SUPPORTED),
        (
            "lease_workflow_context",
            awman::command::worker::lease_workflow_context::SUPPORTED,
        ),
    ] {
        // Lease flags additionally require a running pull worker (absent here).
        let expected = usize::from(name == "context_api" && enabled);
        assert_eq!(
            capabilities.iter().filter(|c| **c == name).count(),
            expected
        );
    }
    for suffix in ["context", "context/files/a.md", "outputs"] {
        let path = format!("/v1/worker/commands/{}/{suffix}", scratch.command);
        assert_eq!(server.get(&path, None).await.0, 401);
        assert_eq!(server.get(&path, Some("invalid-key")).await.0, 401);
    }
    // The same worker-only paths are concealed on an engine-only listener.
    let concealed_sandbox = support::sandbox("daemon::workflow_context::engine_only_listener");
    let concealed = Api::start(&concealed_sandbox).await;
    let concealed_key = self::key(&concealed, &concealed_sandbox);
    for suffix in ["context", "context/files/a.md", "outputs"] {
        assert_eq!(
            concealed
                .get(
                    &format!("/v1/worker/commands/{}/{suffix}", scratch.command),
                    Some(&concealed_key),
                )
                .await
                .0,
            404
        );
    }
    concealed.shutdown().await;
    concealed_sandbox.finish();
    server.shutdown().await;
    sandbox.finish();
}

#[test]
fn retention_preserves_shared_invocations_and_retries_unsafe_directory_removal() {
    use awman::data::fs::{api_db::SqliteSessionStore, api_paths::ApiPaths};
    let sandbox = support::sandbox("daemon::workflow_context::retention_preserves_shared_invocations_and_retries_unsafe_directory_removal");
    let paths = ApiPaths::from_env(&support::env_snapshot(&sandbox)).unwrap();
    let store = SqliteSessionStore::open_from_paths(&paths).unwrap();
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let pins = CommandInvocationPins::new(paths.clone());
    let retention = ScratchRetention::new(paths, resolver.clone());
    let invocation = uuid::Uuid::new_v4();
    let dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("shared.md"), b"shared").unwrap();
    let mut sessions = Vec::new();
    let expired = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
    for _ in 0..2 {
        let session = WorkerSessionId::new();
        let command = CommandId::new();
        store
            .insert_session(session.as_str(), sandbox.work().to_str().unwrap(), &expired)
            .unwrap();
        store
            .insert_command(
                command.as_str(),
                session.as_str(),
                "exec workflow",
                "[]",
                "",
            )
            .unwrap();
        pins.pin(session.as_str(), command.as_str(), 1, invocation)
            .unwrap();
        sessions.push((session, command));
    }
    store
        .close_session_force(sessions[0].0.as_str(), &expired)
        .unwrap();
    assert_eq!(
        store.delete_closed_sessions_older_than(24).unwrap().len(),
        1
    );
    retention.sweep_at_startup(&store);
    assert!(dir.join("shared.md").exists());
    assert!(pins
        .all_for_session(sessions[0].0.as_str())
        .unwrap()
        .is_empty());
    store
        .close_session_force(sessions[1].0.as_str(), &expired)
        .unwrap();
    store.delete_closed_sessions_older_than(24).unwrap();
    #[cfg(unix)]
    {
        // A bad root refuses deletion, retains its retry pin and leaves the
        // symlink target untouched. Fixing the root lets the next sweep finish.
        let moved = dir.with_extension("moved");
        std::fs::rename(&dir, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &dir).unwrap();
        assert_eq!(retention.sweep_at_startup(&store), 0);
        assert!(moved.join("shared.md").exists());
        assert_eq!(
            pins.all_for_session(sessions[1].0.as_str()).unwrap().len(),
            1
        );
        std::fs::remove_file(&dir).unwrap();
        std::fs::rename(&moved, &dir).unwrap();
    }
    assert_eq!(retention.sweep_at_startup(&store), 1);
    assert!(!dir.exists());
    assert!(pins
        .all_for_session(sessions[1].0.as_str())
        .unwrap()
        .is_empty());
    drop(store);
    sandbox.finish();
}

#[test]
fn retention_keeps_all_attempts_survives_corrupt_pins_and_waits_for_live_workflows() {
    use awman::data::fs::{api_db::SqliteSessionStore, api_paths::ApiPaths};
    let sandbox = support::sandbox("daemon::workflow_context::retention_keeps_all_attempts_survives_corrupt_pins_and_waits_for_live_workflows");
    let paths = ApiPaths::from_env(&support::env_snapshot(&sandbox)).unwrap();
    let store = SqliteSessionStore::open_from_paths(&paths).unwrap();
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let pins = CommandInvocationPins::new(paths.clone());
    let retention = ScratchRetention::new(paths.clone(), resolver.clone());
    let invocations = [
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    ];
    for invocation in invocations {
        std::fs::create_dir_all(resolver.workflow_dir(invocation)).unwrap();
    }
    pins.pin("expired", "command", 1, invocations[0]).unwrap();
    // More than one invocation within an attempt must also be retained.
    pins.pin("expired", "command", 1, invocations[1]).unwrap();
    pins.pin("expired", "command", 2, invocations[2]).unwrap();
    paths.prepare_command_dir("bad", "bad").unwrap();
    std::fs::write(paths.command_invocation_pin_path("bad", "bad"), b"corrupt").unwrap();
    let in_use = resolver.workflow_lease(invocations[0]).unwrap();
    assert_eq!(retention.sweep_at_startup(&store), 2);
    assert!(resolver.workflow_dir(invocations[0]).exists());
    assert!(!resolver.workflow_dir(invocations[1]).exists());
    assert!(!resolver.workflow_dir(invocations[2]).exists());
    assert_eq!(
        pins.all_for_session("expired").unwrap().len(),
        1,
        "busy invocation keeps its retry pin"
    );
    drop(in_use);
    retention.sweep_at_startup(&store);
    assert!(!resolver.workflow_dir(invocations[0]).exists());
    assert!(pins.all_for_session("expired").unwrap().is_empty());
}
