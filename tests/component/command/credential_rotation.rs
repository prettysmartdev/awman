//! Real controller + private runtime fake. Staging cannot acknowledge consumption.
use crate::data::credential_updates::{enroll, intent};
use awman::{
    command::automation::controller::*,
    data::{credential_update::*, secret::*, terminal::ViewerIdentity},
    engine::auth::redaction::RedactionSet,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
struct Runtime {
    staged: Mutex<Option<UpdateFence>>,
    refuse: bool,
}
impl CredentialDriver for Runtime {
    fn stage<'a>(
        &'a self,
        fence: &'a UpdateFence,
        material: Arc<CommandSecrets>,
    ) -> Pin<Box<dyn Future<Output = Result<StagedCredential, &'static str>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(material.names().collect::<Vec<_>>(), vec!["OPENAI_API_KEY"]);
            if self.refuse {
                return Err("authentication_refused");
            }
            *self.staged.lock().unwrap() = Some(fence.clone());
            Ok(StagedCredential {
                admission_while_pending: true,
            })
        })
    }
}
fn secrets(value: &str) -> Arc<CommandSecrets> {
    Arc::new(
        CommandSecrets::new(vec![("OPENAI_API_KEY".into(), SecretString::new(value))]).unwrap(),
    )
}
#[tokio::test]
async fn idle_staging_requires_new_generation_consumption_and_isolates_profiles() {
    let sandbox=crate::support::sandbox("command::credential_rotation::idle_staging_requires_new_generation_consumption_and_isolates_profiles");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("a"));
    let (_db2, store2, e2) = crate::data::automation_store::setup(&sandbox.tmp().join("b"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let other = enroll(&store2, &e2, CredentialUpdateMethod::Live);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let redaction = RedactionSet::new();
    redaction.add(SecretString::new("test-secret-old"));
    let host = AutomationHost {
        store: store.clone(),
        command_id: e.command_id.clone(),
        attempt: 1,
        owner_key: e.owner_key.clone(),
        events: None,
        finished: None,
    };
    let enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime.clone(),
            redaction.clone(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let material = secrets("test-secret-replacement");
    let accepted = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request.clone(),
                secrets: Some(material.clone()),
            },
            None,
        )
        .unwrap();
    // Changed values conflict while protected comparison material is retained.
    assert_eq!(
        controller
            .stage_update(
                &ViewerIdentity::local(),
                &host,
                CredentialUpdateRequest {
                    intent: request.clone(),
                    secrets: Some(secrets("test-secret-conflict"))
                },
                None
            )
            .unwrap_err(),
        "idempotency_conflict"
    );
    let fence = awman_test_support::wait::wait_for_async(
        "generation staged",
        std::time::Duration::from_secs(1),
        || {
            let runtime = runtime.clone();
            async move { runtime.staged.lock().unwrap().clone() }
        },
    )
    .await;
    let pending = store
        .get_credential_update(&e.owner_key, &e.command_id, request.request_id)
        .unwrap()
        .unwrap();
    assert!(!pending.status.is_terminal());
    assert_eq!(pending.credential_generation, 0);
    assert_eq!(
        store2
            .effective_credentials(&e2.owner_key, &e2.execution_id)
            .unwrap()
            .slots[0],
        other
    );
    assert_eq!(
        redaction.redact("test-secret-old test-secret-replacement"),
        "*** ***"
    );
    let mut stale = fence.clone();
    stale.generation -= 1;
    assert!(controller
        .confirm_update(CredentialConsumption::TokenInstalled { fence: stale })
        .is_err());
    let op = controller
        .confirm_update(CredentialConsumption::RequestAuthenticated {
            fence,
            request_sequence: 1,
        })
        .unwrap();
    assert_eq!(op.status, CredentialUpdateStatus::Applied);
    assert_eq!(op.new_execution_id, None);
    assert_eq!(op.new_generation, accepted.new_generation);
    // Once terminal, equal metadata ignores lost comparison bytes and never redelivers.
    let replay = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request,
                secrets: Some(secrets("test-secret-unused-retry")),
            },
            None,
        )
        .unwrap();
    assert_eq!(replay, op);
    drop(enrollment);
    sandbox.finish();
}
#[tokio::test]
async fn cancellation_fences_ack_and_a_file_write_never_passes_live_coverage() {
    let sandbox=crate::support::sandbox("command::credential_rotation::cancellation_fences_ack_and_a_file_write_never_passes_live_coverage");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = AutomationHost {
        store: store.clone(),
        command_id: e.command_id.clone(),
        attempt: 1,
        owner_key: e.owner_key.clone(),
        events: None,
        finished: None,
    };
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime.clone(),
            RedactionSet::new(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let op = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request,
                secrets: Some(secrets("test-secret-staged-only")),
            },
            None,
        )
        .unwrap();
    let fence = awman_test_support::wait::wait_for_async(
        "generation staged",
        std::time::Duration::from_secs(1),
        || {
            let runtime = runtime.clone();
            async move { runtime.staged.lock().unwrap().clone() }
        },
    )
    .await;
    controller.cancel(&e.execution_id);
    assert!(controller
        .confirm_update(CredentialConsumption::TokenInstalled { fence })
        .is_err());
    let result = store
        .get_credential_update(&e.owner_key, &e.command_id, op.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(result.status, CredentialUpdateStatus::Failed);
    assert_eq!(result.generation_state, GenerationState::Unknown);
    drop(enrollment);
    sandbox.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn private_fake_request_consumes_staged_generation_before_live_ack() {
    use awman::engine::credential_refresh::delivery::HelperDirectory;
    let sandbox=crate::support::sandbox("command::credential_rotation::private_fake_request_consumes_staged_generation_before_live_ack");
    sandbox.redact("test-secret-fake-request");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let host = AutomationHost {
        store: store.clone(),
        command_id: e.command_id.clone(),
        attempt: 1,
        owner_key: e.owner_key.clone(),
        events: None,
        finished: None,
    };
    let enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime.clone(),
            RedactionSet::new(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let material = secrets("test-secret-fake-request");
    controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request.clone(),
                secrets: Some(material.clone()),
            },
            None,
        )
        .unwrap();
    let fence = awman_test_support::wait::wait_for_async(
        "generation staged",
        std::time::Duration::from_secs(1),
        || {
            let runtime = runtime.clone();
            async move { runtime.staged.lock().unwrap().clone() }
        },
    )
    .await;
    let root = sandbox.tmp().join("payload-dir");
    awman::data::fs::private_files::create_private_dir(&root).unwrap();
    let directory = HelperDirectory::open(&root).unwrap();
    directory
        .stage(fence.generation, material.get("OPENAI_API_KEY").unwrap())
        .unwrap();
    assert_ne!(
        store
            .get_credential_update(&e.owner_key, &e.command_id, request.request_id)
            .unwrap()
            .unwrap()
            .status,
        CredentialUpdateStatus::Applied
    );
    // A private fake request authenticates with the newly staged value. The
    // script never receives a token in argv, emits one non-secret generation,
    // and reads both marker and token from the same opened descriptor.
    let output=sandbox.command("/bin/sh").args(["-c","set -eu; IFS= read -r expected; exec 3<\"$1\"; IFS= read -r generation <&3; actual=$(cat <&3); test \"$actual\" = \"$expected\"; printf '%s' \"$generation\"","fake-request"]).arg(root.join("payload")).stdin(awman_test_support::Stdin::Bytes(b"test-secret-fake-request\n".to_vec())).run();
    assert_eq!(output.exit, awman_test_support::Exit::Code(0));
    let consumed = String::from_utf8(output.stdout)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(consumed, fence.generation);
    let op = controller
        .confirm_update(CredentialConsumption::RequestAuthenticated {
            fence,
            request_sequence: 1,
        })
        .unwrap();
    assert_eq!(op.status, CredentialUpdateStatus::Applied);
    assert!(op.new_execution_id.is_none());
    drop(enrollment);
    sandbox.finish();
}

struct SourceFailure;
impl CredentialDriver for SourceFailure {
    fn stage<'a>(
        &'a self,
        _: &'a UpdateFence,
        _: Arc<CommandSecrets>,
    ) -> Pin<Box<dyn Future<Output = Result<StagedCredential, &'static str>> + Send + 'a>> {
        Box::pin(async {
            Ok(StagedCredential {
                admission_while_pending: true,
            })
        })
    }
    fn reread(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Arc<CommandSecrets>, &'static str>> + Send + '_>> {
        Box::pin(async { Err("source_expired") })
    }
}
#[tokio::test]
async fn source_expiry_is_pollable_and_preserves_confirmed_generation_and_refresh_owner() {
    let sandbox=crate::support::sandbox("command::credential_rotation::source_expiry_is_pollable_and_preserves_confirmed_generation_and_refresh_owner");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = crate::data::credential_updates::enroll_source(
        &store,
        &e,
        CredentialUpdateMethod::Live,
        true,
    );
    let controller = Arc::new(ExecutionController::default());
    let host = AutomationHost {
        store: store.clone(),
        command_id: e.command_id.clone(),
        attempt: 1,
        owner_key: e.owner_key.clone(),
        events: None,
        finished: None,
    };
    let enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            Arc::new(SourceFailure),
            RedactionSet::new(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let op = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request,
                secrets: Some(secrets("test-secret-detached")),
            },
            None,
        )
        .unwrap();
    assert_eq!(op.refresh_owner, RefreshOwner::Caller);
    awman_test_support::wait_for_async(
        "initial staging",
        std::time::Duration::from_secs(1),
        || {
            let store = store.clone();
            let e = e.clone();
            async move {
                store
                    .get_credential_update(&e.owner_key, &e.command_id, op.request_id)
                    .ok()
                    .flatten()
                    .filter(|o| o.reason.as_deref() == Some("awaiting_next_request"))
            }
        },
    )
    .await;
    let fence = UpdateFence {
        command_id: e.command_id.clone(),
        execution_id: e.execution_id.clone(),
        credential_slot: slot.credential_slot.clone(),
        request_id: op.request_id,
        generation: op.new_generation,
    };
    controller
        .confirm_update(CredentialConsumption::TokenInstalled { fence })
        .unwrap();
    let current = store.read_execution(&e.execution_id).unwrap();
    let slot = store
        .effective_credentials(&e.owner_key, &e.execution_id)
        .unwrap()
        .slots
        .remove(0);
    let mut request = intent(&current, &slot);
    request.reread_source = true;
    request.secret_names.clear();
    let op = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request,
                secrets: None,
            },
            None,
        )
        .unwrap();
    assert_eq!(op.refresh_owner, RefreshOwner::Awman);
    let failure = awman_test_support::wait_for_async(
        "source failure result",
        std::time::Duration::from_secs(1),
        || {
            let store = store.clone();
            let e = e.clone();
            async move {
                store
                    .get_credential_update(&e.owner_key, &e.command_id, op.request_id)
                    .ok()
                    .flatten()
                    .filter(|o| o.status.is_terminal())
            }
        },
    )
    .await;
    assert_eq!(failure.reason.as_deref(), Some("source_expired"));
    assert_eq!(failure.credential_generation, slot.credential_generation);
    assert_eq!(failure.generation_state, GenerationState::Confirmed);
    drop(enrollment);
    sandbox.finish();
}

fn credential_host(
    store: &awman::data::automation_store::AutomationStore,
    e: &awman::data::automation_records::ExecutionRecord,
) -> AutomationHost {
    AutomationHost {
        store: store.clone(),
        command_id: e.command_id.clone(),
        attempt: 1,
        owner_key: e.owner_key.clone(),
        events: None,
        finished: None,
    }
}

#[tokio::test]
async fn restart_refusal_has_no_process_turn_terminal_or_command_side_effects() {
    let sandbox = crate::support::sandbox("command::credential_rotation::restart_refusal_has_no_process_turn_terminal_or_command_side_effects");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Restart);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let redaction = RedactionSet::new();
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime.clone(),
            redaction.clone(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let before = store.read_execution(&e.execution_id).unwrap();
    assert_eq!(
        controller
            .stage_update(
                &ViewerIdentity::local(),
                &host,
                CredentialUpdateRequest {
                    intent: request.clone(),
                    secrets: Some(secrets("test-secret-refused-restart"))
                },
                None
            )
            .unwrap_err(),
        "native_capability_unavailable"
    );
    assert_eq!(
        serde_json::to_value(store.read_execution(&e.execution_id).unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert!(store
        .get_credential_update(&e.owner_key, &e.command_id, request.request_id)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .effective_credentials(&e.owner_key, &e.execution_id)
            .unwrap()
            .slots[0],
        slot
    );
    assert!(runtime.staged.lock().unwrap().is_none());
    assert!(redaction.is_empty());
    drop(_enrollment);
    sandbox.finish();
}

#[tokio::test]
async fn rejected_material_never_changes_redaction_revision_or_admission() {
    let sandbox = crate::support::sandbox("command::credential_rotation::rejected_material_never_changes_redaction_revision_or_admission");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let redaction = RedactionSet::new();
    redaction.add(SecretString::new("test-secret-confirmed"));
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime,
            redaction.clone(),
        )
        .unwrap();
    for i in 0..16 {
        let mut request = intent(&e, &slot);
        request.expected_revision += 10;
        let value = format!("test-secret-rejected-{i}");
        assert_eq!(
            controller
                .stage_update(
                    &ViewerIdentity::local(),
                    &host,
                    CredentialUpdateRequest {
                        intent: request,
                        secrets: Some(secrets(&value))
                    },
                    None
                )
                .unwrap_err(),
            "stale_revision"
        );
        assert_eq!(redaction.redact(&value), value);
    }
    assert_eq!(
        controller
            .stage_update(
                &ViewerIdentity::local(),
                &host,
                CredentialUpdateRequest {
                    intent: intent(&e, &slot),
                    secrets: Some(secrets("a"))
                },
                None
            )
            .unwrap_err(),
        "invalid_secrets"
    );
    assert_eq!(
        redaction.redact("ordinary data test-secret-confirmed"),
        "ordinary data ***"
    );
    assert!(!redaction.is_retired());
    assert_eq!(
        store.read_execution(&e.execution_id).unwrap().revision,
        e.revision
    );
    drop(_enrollment);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_controller_retries_return_one_operation() {
    let sandbox = crate::support::sandbox(
        "command::credential_rotation::concurrent_controller_retries_return_one_operation",
    );
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime,
            RedactionSet::new(),
        )
        .unwrap();
    let request = intent(&e, &slot);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let (controller, host, request, barrier) = (
            controller.clone(),
            host.clone(),
            request.clone(),
            barrier.clone(),
        );
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            controller.stage_update(
                &ViewerIdentity::local(),
                &host,
                CredentialUpdateRequest {
                    intent: request,
                    secrets: Some(secrets("test-secret-identical")),
                },
                None,
            )
        }));
    }
    let first = tasks.remove(0).await.unwrap().unwrap();
    let second = tasks.remove(0).await.unwrap().unwrap();
    assert_eq!(first.request_id, second.request_id);
    assert_eq!(first.new_generation, second.new_generation);
    drop(_enrollment);
    sandbox.finish();
}

#[tokio::test]
async fn ordinary_profile_terminal_requires_a_real_input_grant() {
    let sandbox = crate::support::sandbox(
        "command::credential_rotation::ordinary_profile_terminal_requires_a_real_input_grant",
    );
    let (db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    use awman::{
        data::{
            daemon_state::FeatureName,
            terminal::{TerminalOwner, TerminalOwnerInfo},
        },
        engine::terminal_hub::{TerminalHub, TerminalInput, TerminalRegistration},
    };
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (stdin, _input) = awman::engine::terminal_hub::input_channel();
    let handle = hub.register(
        TerminalRegistration {
            automation: None,
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(e.command_id.clone()),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "fixture".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    let id = handle.id().clone();
    let grant = hub.take_control(&id).unwrap();

    db.with_conn(|c| { c.execute("UPDATE agent_executions SET terminal_id=?, automation_mode=NULL, minimum_status_support=NULL, snapshot_json=NULL WHERE execution_id=?", rusqlite::params![id.as_str(), e.execution_id.as_str()])?; Ok(()) }).unwrap();
    let e = store.read_execution(&e.execution_id).unwrap();
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime,
            RedactionSet::new(),
        )
        .unwrap();
    assert_eq!(
        controller
            .stage_update(
                &ViewerIdentity::local(),
                &host,
                CredentialUpdateRequest {
                    intent: intent(&e, &slot),
                    secrets: Some(secrets("test-secret-terminal"))
                },
                None
            )
            .unwrap_err(),
        "control_held"
    );
    let accepted = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: intent(&e, &slot),
                secrets: Some(secrets("test-secret-held-terminal")),
            },
            Some(&grant),
        )
        .unwrap();
    assert_eq!(accepted.old_execution_id, e.execution_id);
    drop(_enrollment);
    sandbox.finish();
}

struct OwnedStage {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: std::sync::atomic::AtomicBool,
    retirements: std::sync::atomic::AtomicUsize,
}
impl CredentialDriver for OwnedStage {
    fn stage<'a>(
        &'a self,
        _: &'a UpdateFence,
        _: Arc<CommandSecrets>,
    ) -> Pin<Box<dyn Future<Output = Result<StagedCredential, &'static str>> + Send + 'a>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(StagedCredential {
                admission_while_pending: false,
            })
        })
    }
    fn retire(&self) {
        self.retirements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
#[tokio::test]
async fn cancelled_delivery_keeps_its_owned_task_until_late_writer_cleanup() {
    let sandbox = crate::support::sandbox("command::credential_rotation::cancelled_delivery_keeps_its_owned_task_until_late_writer_cleanup");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let driver = Arc::new(OwnedStage {
        entered: Default::default(),
        release: Default::default(),
        completed: Default::default(),
        retirements: Default::default(),
    });
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            driver.clone(),
            RedactionSet::new(),
        )
        .unwrap();
    let accepted = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: intent(&e, &slot),
                secrets: Some(secrets("test-secret-owned-stage")),
            },
            None,
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), driver.entered.notified())
        .await
        .unwrap();
    controller.cancel(&e.execution_id);
    let failed = store
        .get_credential_update(&e.owner_key, &e.command_id, accepted.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, CredentialUpdateStatus::Failed);
    assert_eq!(failed.generation_state, GenerationState::Unknown);
    assert!(!driver.completed.load(std::sync::atomic::Ordering::SeqCst));
    driver.release.notify_one();
    awman_test_support::wait_for_async(
        "late delivery retired",
        std::time::Duration::from_secs(1),
        || {
            let driver = driver.clone();
            async move {
                (driver.completed.load(std::sync::atomic::Ordering::SeqCst)
                    && driver.retirements.load(std::sync::atomic::Ordering::SeqCst) >= 2)
                    .then_some(())
            }
        },
    )
    .await;
    drop(_enrollment);
    sandbox.finish();
}

#[tokio::test]
async fn redaction_capacity_failure_is_pollable_without_retiring_existing_output() {
    let sandbox = crate::support::sandbox("command::credential_rotation::redaction_capacity_failure_is_pollable_without_retiring_existing_output");
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let runtime = Arc::new(Runtime {
        staged: Mutex::new(None),
        refuse: false,
    });
    let redaction = RedactionSet::new();
    // Original plus base64 leave 547 bytes, less than one maximum-sized token.
    redaction
        .try_add_many([SecretString::new("x".repeat(7_190_001))])
        .unwrap();
    assert!(!redaction.is_retired());
    let _enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            runtime.clone(),
            redaction.clone(),
        )
        .unwrap();
    let accepted = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: intent(&e, &slot),
                secrets: Some(secrets(&"y".repeat(8192))),
            },
            None,
        )
        .unwrap();
    assert_eq!(accepted.status, CredentialUpdateStatus::Failed);
    assert_eq!(accepted.reason.as_deref(), Some("output_incomplete"));
    assert_eq!(
        store
            .get_credential_update(&e.owner_key, &e.command_id, accepted.request_id)
            .unwrap(),
        Some(accepted)
    );
    assert!(!redaction.is_retired());
    assert_eq!(redaction.redact("ordinary output"), "ordinary output");
    assert!(runtime.staged.lock().unwrap().is_none());
    drop(_enrollment);
    sandbox.finish();
}

#[tokio::test]
async fn timed_out_delivery_is_pollable_before_owned_writer_finishes() {
    let sandbox = crate::support::sandbox(
        "command::credential_rotation::timed_out_delivery_is_pollable_before_owned_writer_finishes",
    );
    let (_db, store, e) = crate::data::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let host = credential_host(&store, &e);
    let controller = Arc::new(ExecutionController::default());
    let driver = Arc::new(OwnedStage {
        entered: Default::default(),
        release: Default::default(),
        completed: Default::default(),
        retirements: Default::default(),
    });
    let enrollment = controller
        .register_credentials(
            host.clone(),
            e.execution_id.clone(),
            driver.clone(),
            RedactionSet::new(),
        )
        .unwrap();
    let accepted = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: intent(&e, &slot),
                secrets: Some(secrets("test-secret-timeout-stage")),
            },
            None,
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), driver.entered.notified())
        .await
        .unwrap();
    let failure = awman_test_support::wait_for_async(
        "pollable delivery timeout",
        std::time::Duration::from_secs(3),
        || {
            let (store, e) = (store.clone(), e.clone());
            async move {
                store
                    .get_credential_update(&e.owner_key, &e.command_id, accepted.request_id)
                    .unwrap()
                    .filter(|o| o.status == CredentialUpdateStatus::Failed)
            }
        },
    )
    .await;
    assert_eq!(failure.reason.as_deref(), Some("application_unconfirmed"));
    assert_eq!(failure.generation_state, GenerationState::Unknown);
    assert!(!driver.completed.load(std::sync::atomic::Ordering::SeqCst));
    driver.release.notify_one();
    awman_test_support::wait_for_async(
        "timed out writer retired",
        std::time::Duration::from_secs(1),
        || {
            let driver = driver.clone();
            async move {
                (driver.completed.load(std::sync::atomic::Ordering::SeqCst)
                    && driver.retirements.load(std::sync::atomic::Ordering::SeqCst) >= 2)
                    .then_some(())
            }
        },
    )
    .await;
    assert_eq!(
        store
            .get_credential_update(&e.owner_key, &e.command_id, accepted.request_id)
            .unwrap(),
        Some(failure)
    );
    drop(enrollment);
    sandbox.finish();
}
