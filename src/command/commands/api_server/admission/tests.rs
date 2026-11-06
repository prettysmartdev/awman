use super::*;
use crate::data::session_status::SessionStatus;
use serde_json::json;

struct Fixture {
    tmp: tempfile::TempDir,
    admission: ApiAdmission,
}

#[tokio::test]
async fn failed_barrier_never_leaves_an_unprotected_session() {
    for with_command in [false, true] {
        let f = Fixture::new();
        f.sql(
            "CREATE TRIGGER fail_barrier BEFORE UPDATE OF resource_kind ON idempotency_keys
            WHEN NEW.resource_kind='compensating_session'
            BEGIN SELECT RAISE(ABORT, 'barrier failed'); END;
            CREATE TRIGGER fail_remote BEFORE UPDATE OF repo_url ON sessions
            BEGIN SELECT RAISE(ABORT, 'remote failed'); END;",
        );
        let journal = f.journal("barrier", "retry");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        assert!(f
            .admission
            .admit_session(
                remote(f.local()),
                with_command.then(command),
                true,
                Some(&journal)
            )
            .await
            .is_err());
        assert!(
            f.admission.store.list_sessions().unwrap().is_empty(),
            "barrier and insertion roll back together"
        );
        journal.release().unwrap();
        drop(journal);
        f.sql("DROP TRIGGER fail_barrier; DROP TRIGGER fail_remote;");
        let retry = f.journal("barrier", "retry");
        assert_eq!(retry.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        f.admission
            .admit_session(
                remote(f.local()),
                with_command.then(command),
                true,
                Some(&retry),
            )
            .await
            .unwrap();
        assert_eq!(f.admission.store.list_sessions().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn failed_barrier_refresh_still_closes_and_preserves_pending_rollback() {
    for fail_close in [false, true] {
        let f = Fixture::new();
        f.sql("CREATE TRIGGER fail_barrier_refresh BEFORE UPDATE OF resource_kind ON idempotency_keys
            WHEN NEW.resource_kind='compensating_session' AND EXISTS(SELECT 1 FROM sessions WHERE id=NEW.resource_id)
            BEGIN SELECT RAISE(ABORT, 'barrier refresh failed'); END;
            CREATE TRIGGER fail_remote BEFORE UPDATE OF repo_url ON sessions
            BEGIN SELECT RAISE(ABORT, 'remote failed'); END;");
        if fail_close {
            f.sql("CREATE TRIGGER fail_close BEFORE UPDATE OF status ON sessions BEGIN SELECT RAISE(ABORT, 'close failed'); END;");
        }
        let journal = f.journal("refresh", "retry");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        assert!(f
            .admission
            .admit_session(remote(f.local()), None, false, Some(&journal))
            .await
            .is_err());
        assert_eq!(
            f.only_session().status,
            if fail_close {
                SessionStatus::Active
            } else {
                SessionStatus::Closed
            }
        );
        journal.release().unwrap();
        drop(journal);
        let retry = f.journal("refresh", "retry");
        assert_eq!(
            retry.begin("canonical").unwrap(),
            if fail_close {
                IdempotencyBegin::InFlight
            } else {
                IdempotencyBegin::Fresh
            }
        );
        assert_eq!(f.admission.store.list_sessions().unwrap().len(), 1);
    }
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(tmp.path()).unwrap());
        let admission = ApiAdmission::new(
            Arc::clone(&store),
            CommandLifecycleStore::open(store).unwrap(),
            ApiPaths::at_root(tmp.path()),
            Engines::for_tests(tmp.path()),
            Arc::new(SessionManager::in_memory(
                crate::data::config::env::EnvSnapshot::empty(),
            )),
            vec![tmp.path().canonicalize().unwrap()],
            vec![],
        );
        Self { tmp, admission }
    }

    fn local(&self) -> SessionSubmission {
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(self.tmp.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
            labels: serde_json::from_value(json!({"test/team": "test"})).unwrap(),
        }
    }

    fn journal(&self, route: &'static str, key: &str) -> IdempotentAdmission {
        IdempotentAdmission::new(Arc::clone(&self.admission.store), route, "test", key)
    }

    fn sql(&self, sql: &str) {
        self.admission
            .store
            .with_conn(|conn| {
                conn.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    }

    fn only_session(&self) -> crate::data::fs::api_db::SessionRecord {
        let mut rows = self.admission.store.list_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        rows.pop().unwrap()
    }
}

fn command() -> CommandSubmission {
    CommandSubmission {
        interaction: None,
        subcommand: "exec prompt".into(),
        args: vec!["once".into()],
        labels: Labels::empty(),
    }
}

fn remote(mut submission: SessionSubmission) -> SessionSubmission {
    submission.request = SessionCreateRequest {
        session_type: Some("remote".into()),
        repo_url: Some("https://example.com/repo.git".into()),
        ..Default::default()
    };
    submission
}

#[tokio::test]
async fn remote_fields_failure_closes_session_before_keyed_retry() {
    for with_command in [false, true] {
        let f = Fixture::new();
        f.sql("CREATE TRIGGER fail_remote BEFORE UPDATE OF repo_url ON sessions BEGIN SELECT RAISE(ABORT, 'remote fields failed'); END;");
        let journal = f.journal("remote", "retry");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        let error = f
            .admission
            .admit_session(
                remote(f.local()),
                with_command.then(command),
                true,
                Some(&journal),
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            AdmissionError::Storage {
                what: "Failed to create session",
                ..
            }
        ));
        assert_eq!(f.only_session().status, SessionStatus::Closed);
        journal.release().unwrap();
        drop(journal);
        f.sql("DROP TRIGGER fail_remote;");
        let retry = f.journal("remote", "retry");
        assert_eq!(retry.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        f.admission
            .admit_session(
                remote(f.local()),
                with_command.then(command),
                true,
                Some(&retry),
            )
            .await
            .unwrap();
        assert_eq!(
            f.admission
                .store
                .list_sessions_by_status(Some(SessionStatus::Active))
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn failed_compensation_blocks_retry_and_success_replay_until_session_is_closed() {
    for with_command in [false, true] {
        let f = Fixture::new();
        f.sql("CREATE TRIGGER fail_labels BEFORE INSERT ON labels BEGIN SELECT RAISE(ABORT, 'labels failed'); END;
            CREATE TRIGGER fail_close BEFORE UPDATE OF status ON sessions BEGIN SELECT RAISE(ABORT, 'close failed'); END;");
        let journal = f.journal("failed-close", "retry");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        assert!(f
            .admission
            .admit_session(f.local(), with_command.then(command), true, Some(&journal))
            .await
            .is_err());
        let session = f.only_session();
        assert_eq!(session.status, SessionStatus::Active);
        // Neither HTTP error completion nor explicit release nor Drop can
        // erase the barrier or turn this failed admission into a 202 replay.
        assert!(!journal.complete(500, "failure").unwrap());
        journal.release().unwrap();
        drop(journal);
        let retry = f.journal("failed-close", "retry");
        assert_eq!(
            retry.begin("canonical").unwrap(),
            IdempotencyBegin::InFlight
        );
        assert_eq!(retry.begin("changed").unwrap(), IdempotencyBegin::Mismatch);
        let hash: String = Sha256::digest(b"canonical")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            f.admission
                .store
                .idempotency_begin(
                    retry.scope(),
                    "retry",
                    &hash,
                    &chrono::Utc::now().to_rfc3339(),
                    IdempotencyOwner {
                        instance: "restarted",
                        request: "retry"
                    }
                )
                .unwrap(),
            IdempotencyBegin::InFlight
        );
        assert_eq!(f.admission.store.list_sessions().unwrap().len(), 1);
        f.sql("DROP TRIGGER fail_close;");
        f.admission.lifecycle().close(&session.id).await.unwrap();
        assert_eq!(retry.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    }
}

#[test]
fn dropping_admission_during_remote_compensation_never_replays_accepted() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let f = Fixture::new();
        f.sql("CREATE TRIGGER fail_labels BEFORE INSERT ON labels BEGIN SELECT RAISE(ABORT, 'labels failed'); END;");
        // Hold the sole blocking thread so clone cleanup cannot finish.
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || { started.send(()).unwrap(); wait.recv().unwrap(); });
        ready.await.unwrap();
        let journal = f.journal("interrupted-close", "retry");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        let mut admission = Box::pin(f.admission.admit_session(remote(f.local()), None, false, Some(&journal)));
        assert!(futures_util::poll!(&mut admission).is_pending());
        assert_eq!(f.only_session().status, SessionStatus::Closing);
        drop(admission);
        drop(journal);
        assert_eq!(f.journal("interrupted-close", "retry").begin("canonical").unwrap(), IdempotencyBegin::InFlight);
        release.send(()).unwrap();
        blocker.await.unwrap();
        // The dropped close future cannot publish closed after its await.
        assert_eq!(f.only_session().status, SessionStatus::Closing);
        f.admission.lifecycle().close(&f.only_session().id).await.unwrap();
        assert_eq!(f.journal("interrupted-close", "retry").begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    });
}

fn assert_replay(journal: &IdempotentAdmission, body: &str) {
    assert_eq!(
        journal.begin("canonical").unwrap(),
        IdempotencyBegin::Replay {
            status: if journal.route.ends_with("/sessions") {
                201
            } else {
                202
            },
            body: body.into(),
        }
    );
}

#[tokio::test]
async fn run_crash_order_records_barrier_then_session_fields_and_labels_then_link_then_command() {
    let f = Fixture::new();
    f.sql(
        r#"
        CREATE TRIGGER check_session_order BEFORE INSERT ON sessions BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM idempotency_keys WHERE resource_kind='compensating_session'
                    AND intent_status IS NULL AND resource_id=NEW.id
            ) THEN RAISE(ABORT, 'session before rollback barrier') END;
        END;
        CREATE TRIGGER check_link_order BEFORE INSERT ON runs BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM sessions WHERE id=NEW.session_id AND setup_status='initializing'
                    AND repo_url='https://example.com/repo.git' AND branch='topic' AND push=1
            ) OR NOT EXISTS (
                SELECT 1 FROM labels WHERE owner_kind='session' AND owner_id=NEW.session_id
                    AND key='test/team' AND value='test'
            ) THEN RAISE(ABORT, 'link before session metadata') END;
        END;
        CREATE TRIGGER check_command_order BEFORE INSERT ON commands BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM runs WHERE command_id=NEW.id AND session_id=NEW.session_id
            ) THEN RAISE(ABORT, 'command before run link') END;
        END;
    "#,
    );
    let journal = f.journal("/v1/runs", "cmd_a53108f7543b75adbb34afc035d4cdf6");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    let mut submission = f.local();
    submission.request = SessionCreateRequest {
        session_type: Some("remote".into()),
        repo_url: Some("https://example.com/repo.git".into()),
        branch: Some("topic".into()),
        push: true,
        ..Default::default()
    };
    let admitted = f
        .admission
        .admit_session(submission, Some(command()), true, Some(&journal))
        .await
        .unwrap();
    let run = admitted.run.unwrap();
    assert!(run.close_on_finish);
    assert_eq!(
        f.admission
            .lifecycle_store
            .run_for_command(&run.command_id)
            .unwrap(),
        Some(run.clone())
    );
    assert_eq!(
        f.only_session().setup_status,
        SessionSetupStatus::Initializing
    );
    assert_eq!(
        f.admission
            .store
            .get_command(run.command_id.as_str())
            .unwrap()
            .unwrap()
            .session_id,
        admitted.session_id
    );
    // No HTTP response has been completed: dropping here models a cancelled
    // handler after the last write, and must still replay the exact bytes.
    drop(journal);
    assert_replay(
        &f.journal("/v1/runs", "cmd_a53108f7543b75adbb34afc035d4cdf6"),
        &admitted.response_body,
    );
    assert_eq!(
        f.admission
            .store
            .list_commands_for_session(&admitted.session_id, 100)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn fully_recorded_session_replays_without_setup() {
    let f = Fixture::new();
    f.sql(
        r#"
        CREATE TRIGGER session_intent BEFORE INSERT ON sessions BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM idempotency_keys WHERE resource_kind='compensating_session'
                    AND resource_id=NEW.id AND intent_status IS NULL
            ) THEN RAISE(ABORT, 'missing session rollback barrier') END;
        END;
    "#,
    );
    let journal = f.journal("/v1/sessions", "ses_21d6f40cfb511982e4424e0e250a9557");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    let admitted = f
        .admission
        .admit_session(f.local(), None, false, Some(&journal))
        .await
        .unwrap();
    assert!(admitted.run.is_none());
    drop(journal);
    assert_replay(
        &f.journal("/v1/sessions", "ses_21d6f40cfb511982e4424e0e250a9557"),
        &admitted.response_body,
    );
    assert_eq!(f.only_session().status, SessionStatus::Active);
}

#[tokio::test]
async fn command_admission_records_intent_before_the_row_and_preserves_flags_and_labels() {
    let f = Fixture::new();
    let admitted = f
        .admission
        .admit_session(f.local(), None, false, None)
        .await
        .unwrap();
    f.admission
        .store
        .update_setup_status(&admitted.session_id, SessionSetupStatus::Ready)
        .unwrap();
    f.sql(
        r#"
        CREATE TRIGGER command_intent BEFORE INSERT ON commands BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM idempotency_keys WHERE resource_kind='command'
                    AND resource_id=NEW.id AND intent_status=202
            ) THEN RAISE(ABORT, 'missing command intent') END;
        END;
    "#,
    );
    let journal = f.journal("/v1/commands", "cmd_1dccadfed7bcbb036c56a4afb97e906f");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    let mut submission = command();
    submission.labels = serde_json::from_value(json!({"test/job": "check"})).unwrap();
    let command = f
        .admission
        .admit_command(&admitted.session_id, submission, Some(&journal))
        .await
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&command.response_body).unwrap();
    assert_eq!(response["flags_applied"]["yolo"], true);
    assert_eq!(response["flags_applied"]["headless"], true);
    assert_eq!(response["flags_applied"], command.flags_applied);
    assert_eq!(
        f.admission
            .store
            .labels_for(LabelOwner::Command, &command.command_id)
            .unwrap()
            .get("test/job"),
        Some("check")
    );
    drop(journal);
    assert_replay(
        &f.journal("/v1/commands", "cmd_1dccadfed7bcbb036c56a4afb97e906f"),
        &command.response_body,
    );
}

#[tokio::test]
async fn failed_labels_run_link_and_enqueue_close_the_session_without_admitting_a_command() {
    for (table, what, has_run) in [
        ("labels", "Failed to store session labels", false),
        ("runs", "Failed to record run", false),
        ("commands", "Failed to enqueue command", true),
    ] {
        let f = Fixture::new();
        f.sql(&format!("CREATE TRIGGER fail_write BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"));
        let journal = f.journal("/v1/runs", "failed");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        let error = f
            .admission
            .admit_session(f.local(), Some(command()), false, Some(&journal))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error, AdmissionError::Storage { what: actual, .. } if actual == what),
            "{error:?}"
        );
        let session = f.only_session();
        assert_eq!(session.status, SessionStatus::Closed, "{table}");
        assert!(f
            .admission
            .store
            .list_commands_for_session(&session.id, 100)
            .unwrap()
            .is_empty());
        assert_eq!(
            f.admission
                .lifecycle_store
                .run_for_session(&crate::data::ids::WorkerSessionId::parse(&session.id).unwrap())
                .unwrap()
                .is_some(),
            has_run
        );
        drop(journal);
        assert_eq!(
            f.journal("/v1/runs", "failed").begin("canonical").unwrap(),
            IdempotencyBegin::Fresh
        );
    }
}

#[tokio::test]
async fn failed_session_labels_can_be_released_after_compensation() {
    let f = Fixture::new();
    f.sql("CREATE TRIGGER fail_labels BEFORE INSERT ON labels BEGIN SELECT RAISE(ABORT, 'injected'); END;");
    let journal = f.journal("/v1/sessions", "failed");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    assert!(f
        .admission
        .admit_session(f.local(), None, false, Some(&journal))
        .await
        .is_err());
    assert_eq!(f.only_session().status, SessionStatus::Closed);
    journal.release().unwrap();
    drop(journal);
    assert_eq!(
        f.journal("/v1/sessions", "failed")
            .begin("canonical")
            .unwrap(),
        IdempotencyBegin::Fresh
    );
}

#[tokio::test]
async fn dropped_future_releases_intent_without_a_resource_and_preserves_admitted_resource() {
    for admit in [false, true] {
        let f = Fixture::new();
        let journal = f.journal("/v1/sessions", "drop");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        let body = if admit {
            f.admission
                .admit_session(f.local(), None, false, Some(&journal))
                .await
                .unwrap()
                .response_body
        } else {
            journal
                .record("intent only", IdempotencyResource::Session("absent"))
                .unwrap();
            String::new()
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _journal = journal;
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let retry = f.journal("/v1/sessions", "drop");
        if admit {
            assert_replay(&retry, &body);
        } else {
            assert_eq!(retry.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        }
    }
}

#[test]
fn panic_and_non_owner_drops_release_only_their_own_reservation() {
    let f = Fixture::new();
    let owner = f.journal("/v1/commands", "key");
    assert_eq!(owner.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    let contender = f.journal("/v1/commands", "key");
    assert_eq!(
        contender.begin("canonical").unwrap(),
        IdempotencyBegin::InFlight
    );
    drop(contender);
    assert_eq!(
        f.journal("/v1/commands", "key").begin("canonical").unwrap(),
        IdempotencyBegin::InFlight
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owner = owner;
            panic!("handler panic");
        }))
        .is_err()
    );
    assert_eq!(
        f.journal("/v1/commands", "key").begin("canonical").unwrap(),
        IdempotencyBegin::Fresh
    );
}

#[tokio::test]
async fn crash_recovery_uses_the_recorded_resource_without_a_completed_response() {
    for admit in [false, true] {
        let f = Fixture::new();
        let journal = f.journal("/v1/sessions", "crash");
        assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
        let body = if admit {
            f.admission
                .admit_session(f.local(), None, false, Some(&journal))
                .await
                .unwrap()
                .response_body
        } else {
            journal
                .record("intent", IdempotencyResource::Session("not-created"))
                .unwrap();
            String::new()
        };
        // An abruptly exited daemon runs no Drop. Leave its unfinished row
        // exactly as it was, then make the replacement daemon's identity differ.
        journal.active.store(false, Ordering::Relaxed);
        drop(journal);
        f.sql("UPDATE idempotency_keys SET owner_instance='exited-daemon'");
        let retry = f.journal("/v1/sessions", "crash");
        if admit {
            assert_replay(&retry, &body);
            assert_eq!(f.admission.store.list_sessions().unwrap().len(), 1);
        } else {
            assert_eq!(retry.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
            assert!(f.admission.store.list_sessions().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn failed_response_storage_does_not_release_admitted_work() {
    let f = Fixture::new();
    let journal = f.journal("/v1/sessions", "response-failure");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    let admitted = f
        .admission
        .admit_session(f.local(), None, false, Some(&journal))
        .await
        .unwrap();
    f.sql("CREATE TRIGGER fail_complete BEFORE UPDATE OF status ON idempotency_keys BEGIN SELECT RAISE(ABORT, 'injected'); END;");
    assert!(journal.complete(202, &admitted.response_body).is_err());
    drop(journal);
    f.sql("DROP TRIGGER fail_complete");
    assert_replay(
        &f.journal("/v1/sessions", "response-failure"),
        &admitted.response_body,
    );
    assert_eq!(f.admission.store.list_sessions().unwrap().len(), 1);
}

#[tokio::test]
async fn validation_precedes_writes_and_session_and_setup_rejections_are_typed() {
    let f = Fixture::new();
    let invalid = CommandSubmission {
        interaction: None,
        args: vec!["--unknown-flag".into()],
        ..command()
    };
    assert!(matches!(
        f.admission
            .admit_session(f.local(), Some(invalid), false, None)
            .await,
        Err(AdmissionError::CommandValidation(_))
    ));
    assert!(f.admission.store.list_sessions().unwrap().is_empty());
    assert!(matches!(
        f.admission.admit_command("missing", command(), None).await,
        Err(AdmissionError::Session(SessionCommandAdmission::NotFound))
    ));
    let admitted = f
        .admission
        .admit_session(f.local(), None, false, None)
        .await
        .unwrap();
    assert!(matches!(
        f.admission
            .admit_command(&admitted.session_id, command(), None)
            .await,
        Err(AdmissionError::Setup(SetupReadiness::Pending { .. }))
    ));
    f.admission
        .lifecycle()
        .close(&admitted.session_id)
        .await
        .unwrap();
    assert!(matches!(
        f.admission
            .admit_command(&admitted.session_id, command(), None)
            .await,
        Err(AdmissionError::Session(SessionCommandAdmission::Closed))
    ));
}

#[test]
fn canonical_fingerprint_keeps_legacy_raw_fields_and_shares_session_selection_forms() {
    let header = json!({"subcommand": "ready", "args": [], "unknown": 1});
    let mut body = header.clone();
    body["session_id"] = json!("ses_03c7c0ace395d80182db07ae2c30f034");
    let canonical =
        CommandSubmission::idempotency_canonical(&header, "ses_03c7c0ace395d80182db07ae2c30f034");
    assert_eq!(
        canonical,
        format!("{header}\nses_03c7c0ace395d80182db07ae2c30f034")
    );
    assert_eq!(
        CommandSubmission::idempotency_canonical(&body, "ses_03c7c0ace395d80182db07ae2c30f034"),
        canonical
    );
    body["labels"] = json!({});
    assert_ne!(
        CommandSubmission::idempotency_canonical(&body, "ses_03c7c0ace395d80182db07ae2c30f034"),
        canonical
    );
}

#[tokio::test]
async fn failed_command_enqueue_releases_the_key_and_keeps_the_existing_session_open() {
    let f = Fixture::new();
    let session = f
        .admission
        .admit_session(f.local(), None, false, None)
        .await
        .unwrap();
    f.admission
        .store
        .update_setup_status(&session.session_id, SessionSetupStatus::Ready)
        .unwrap();
    f.sql("CREATE TRIGGER fail_command BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT, 'injected'); END;");
    let journal = f.journal("/v1/commands", "failed-command");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    assert!(matches!(
        f.admission
            .admit_command(&session.session_id, command(), Some(&journal))
            .await,
        Err(AdmissionError::Storage {
            what: "Failed to enqueue command",
            ..
        })
    ));
    drop(journal);
    assert_eq!(f.only_session().status, SessionStatus::Active);
    assert!(f
        .admission
        .store
        .list_commands_for_session(&session.session_id, 100)
        .unwrap()
        .is_empty());
    assert_eq!(
        f.journal("/v1/commands", "failed-command")
            .begin("canonical")
            .unwrap(),
        IdempotencyBegin::Fresh
    );
}

#[tokio::test]
async fn lost_reservation_prevents_resource_creation_and_stale_drop_keeps_the_new_owner() {
    let f = Fixture::new();
    let journal = f.journal("/v1/sessions", "lost");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    f.sql("UPDATE idempotency_keys SET owner_request='another-live-request'");
    assert!(matches!(
        f.admission
            .admit_session(f.local(), None, false, Some(&journal))
            .await,
        Err(AdmissionError::ReservationLost)
    ));
    assert!(f.admission.store.list_sessions().unwrap().is_empty());
    drop(journal);
    assert_eq!(
        f.journal("/v1/sessions", "lost")
            .begin("canonical")
            .unwrap(),
        IdempotencyBegin::InFlight
    );
}

#[test]
fn completed_rejections_replay_and_cannot_be_released_or_reused_for_a_different_body() {
    let f = Fixture::new();
    let journal = f.journal("/v1/commands", "rejected");
    assert_eq!(journal.begin("canonical").unwrap(), IdempotencyBegin::Fresh);
    assert!(journal.complete(400, "rejected body").unwrap());
    journal.release().unwrap();
    drop(journal);
    let retry = f.journal("/v1/commands", "rejected");
    assert_eq!(
        retry.begin("canonical").unwrap(),
        IdempotencyBegin::Replay {
            status: 400,
            body: "rejected body".into()
        }
    );
    assert_eq!(
        retry.begin("different").unwrap(),
        IdempotencyBegin::Mismatch
    );
}

#[tokio::test]
async fn managed_session_admission_preserves_the_prerequisite_ceiling_and_policy_report() {
    let f = Fixture::new();
    let session = f
        .admission
        .admit_session(f.local(), None, false, None)
        .await
        .unwrap();
    f.admission
        .store
        .update_setup_status(&session.session_id, SessionSetupStatus::Ready)
        .unwrap();
    f.admission
        .sessions
        .open_or_create_with_key(
            Some(session.session_id.clone()),
            f.tmp.path().to_path_buf(),
            Default::default(),
        )
        .unwrap();
    let workflow = |token: &str| CommandSubmission {
        interaction: None,
        subcommand: "exec workflow".into(),
        args: vec![
            "workflow.toml".into(),
            "--prerequisite-token".into(),
            token.into(),
        ],
        labels: Labels::empty(),
    };
    assert!(matches!(
        f.admission
            .admit_command(&session.session_id, workflow("acquire"), None)
            .await,
        Err(AdmissionError::CommandAdmission(
            CommandError::PrerequisiteWideningRefused { .. }
        ))
    ));
    assert!(f
        .admission
        .store
        .list_commands_for_session(&session.session_id, 100)
        .unwrap()
        .is_empty());
    let admitted = f
        .admission
        .admit_command(&session.session_id, workflow("deny"), None)
        .await
        .unwrap();
    assert_eq!(
        admitted.flags_applied["prerequisites"]["githubToken"]["use"],
        "deny"
    );
    assert_eq!(
        admitted.flags_applied["prerequisites"]["unresolved"],
        "fail"
    );
}

// ── IdempotencyGate: the reservation sequence without HTTP ─────────────────

fn gate(f: &Fixture, auth: &str, key: Option<&str>, canonical: &str) -> IdempotencyGate {
    IdempotencyGate::open(&f.admission.store, "/v1/test", auth, key, canonical).unwrap()
}

fn fresh(gate: IdempotencyGate) -> IdempotentRun {
    match gate {
        IdempotencyGate::Fresh(run) => run,
        _ => panic!("expected a fresh reservation"),
    }
}

#[test]
fn gate_without_a_key_runs_unjournaled_and_invalid_keys_are_refused() {
    let f = Fixture::new();
    let auth = "owner";
    assert!(matches!(
        gate(&f, auth, None, "cmd_4a8a08f09d37b73795649038408b5f33"),
        IdempotencyGate::Unkeyed
    ));
    let too_long = "k".repeat(crate::data::fs::api_db::IDEMPOTENCY_KEY_MAX_BYTES + 1);
    for key in ["", "has space", "tab\tkey", "ключ", too_long.as_str()] {
        assert!(
            matches!(
                IdempotencyGate::open(
                    &f.admission.store,
                    "/v1/test",
                    auth,
                    Some(key),
                    "cmd_4a8a08f09d37b73795649038408b5f33"
                ),
                Err(IdempotencyGateError::InvalidKey)
            ),
            "{key:?} must be refused"
        );
    }
    let longest = "k".repeat(crate::data::fs::api_db::IDEMPOTENCY_KEY_MAX_BYTES);
    fresh(gate(
        &f,
        auth,
        Some(&longest),
        "cmd_4a8a08f09d37b73795649038408b5f33",
    ))
    .finish(IdempotentResult::ServerError);
}

#[test]
fn gate_sequences_in_flight_replay_and_mismatch() {
    let f = Fixture::new();
    let auth = "owner";
    let run = fresh(gate(&f, auth, Some("k1"), "body"));
    assert!(matches!(
        gate(&f, auth, Some("k1"), "body"),
        IdempotencyGate::InFlight
    ));
    run.finish(IdempotentResult::Final {
        status: 202,
        body: r#"{"ok":true}"#,
    });
    match gate(&f, auth, Some("k1"), "body") {
        IdempotencyGate::Replay { status, body } => {
            assert_eq!(status, 202);
            assert_eq!(body, r#"{"ok":true}"#);
        }
        _ => panic!("a finished request must replay"),
    }
    assert!(matches!(
        gate(&f, auth, Some("k1"), "other body"),
        IdempotencyGate::Mismatch
    ));
    // Client errors are final answers too and replay the same way.
    fresh(gate(
        &f,
        auth,
        Some("k2"),
        "cmd_bae60998ffe4923b131e3d6e4c19993e",
    ))
    .finish(IdempotentResult::Final {
        status: 400,
        body: r#"{"error":"cmd_bae60998ffe4923b131e3d6e4c19993e"}"#,
    });
    assert!(matches!(
        gate(&f, auth, Some("k2"), "cmd_bae60998ffe4923b131e3d6e4c19993e"),
        IdempotencyGate::Replay { status: 400, .. }
    ));
}

#[test]
fn gate_releases_after_a_server_error_or_a_dropped_run() {
    let f = Fixture::new();
    let auth = "owner";
    fresh(gate(&f, auth, Some("k"), "body")).finish(IdempotentResult::ServerError);
    // A dropped (cancelled or panicking) handler releases its reservation.
    drop(fresh(gate(&f, auth, Some("k"), "body")));
    let run = fresh(gate(&f, auth, Some("k"), "body"));
    // The journal lent to admission is the reservation itself: it stays
    // held until the last clone is gone.
    let journal = run.admission();
    drop(run);
    assert!(matches!(
        gate(&f, auth, Some("k"), "body"),
        IdempotencyGate::InFlight
    ));
    drop(journal);
    fresh(gate(&f, auth, Some("k"), "body"));
}

#[test]
fn gate_scopes_keys_by_principal() {
    let f = Fixture::new();
    let anonymous = "owner";
    let alice = "key_alice";
    fresh(gate(&f, anonymous, Some("k"), "body")).finish(IdempotentResult::Final {
        status: 202,
        body: "{}",
    });
    // The same key under another principal is a separate reservation.
    fresh(gate(&f, alice, Some("k"), "different body")).finish(IdempotentResult::ServerError);
}
