//! Durable generations, CAS, recovery and no secret persistence.
use awman::data::{automation_records::*, automation_store::*, credential_update::*};
use uuid::Uuid;

#[test]
fn credential_state_and_execution_revision_commit_or_rollback_together() {
    let sandbox = crate::support::sandbox(
        "data::credential_updates::credential_state_and_execution_revision_commit_or_rollback_together",
    );
    let (db, store, e) = super::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let request = intent(&e, &slot);
    store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    let snapshot = || {
        store
            .effective_credentials(&e.owner_key, &e.execution_id)
            .unwrap()
    };
    let fault = |enabled| {
        db.with_conn(|conn| {
        conn.execute_batch(if enabled {
            "CREATE TRIGGER fail_credential_projection BEFORE UPDATE OF snapshot_json ON agent_executions BEGIN SELECT RAISE(ABORT,'injected'); END;"
        } else { "DROP TRIGGER fail_credential_projection;" })?;
        Ok(())
    }).unwrap()
    };
    let reserved = snapshot();
    fault(true);
    assert!(store
        .stage_credential_generation(&e.owner_key, &e.command_id, request.request_id, true, None)
        .is_err());
    assert_eq!(snapshot(), reserved);
    fault(false);
    store
        .stage_credential_generation(&e.owner_key, &e.command_id, request.request_id, true, None)
        .unwrap();
    let staged = snapshot();
    assert_eq!(staged.revision, reserved.revision + 1);
    assert!(staged.slots[0].admission_while_pending);
    fault(true);
    assert!(store
        .ack_credential_generation(
            &e.owner_key,
            &e.command_id,
            request.request_id,
            &e.execution_id,
            2,
            None
        )
        .is_err());
    assert_eq!(snapshot(), staged);
    assert!(store
        .fail_credential_update(
            &e.owner_key,
            &e.command_id,
            request.request_id,
            CredentialUpdateFailure {
                code: "credential_update_failed",
                reason: "application_unconfirmed",
                exposed: true,
            },
            None
        )
        .is_err());
    assert_eq!(snapshot(), staged);
    fault(false);
    store
        .fail_credential_update(
            &e.owner_key,
            &e.command_id,
            request.request_id,
            CredentialUpdateFailure {
                code: "credential_update_failed",
                reason: "application_unconfirmed",
                exposed: true,
            },
            None,
        )
        .unwrap();
    let failed = snapshot();
    assert_eq!(failed.revision, staged.revision + 1);
    assert_eq!(failed.slots[0].generation_state, GenerationState::Unknown);
    let mut retry = request;
    retry.request_id = Uuid::new_v4();
    retry.expected_revision = failed.revision;
    store
        .reserve_credential_update(&e.owner_key, &e.command_id, &retry, 2000)
        .unwrap();
    // Certainty loss derives a restart route. The stopped/old execution cannot
    // consume that generation; retain the committed reservation for recovery.
    assert!(store
        .ack_credential_generation(
            &e.owner_key,
            &e.command_id,
            retry.request_id,
            &e.execution_id,
            3,
            None
        )
        .is_err());
    let confirmed = snapshot();
    assert_eq!(confirmed.revision, failed.revision + 1);
    assert_eq!(
        confirmed.slots[0].generation_state,
        GenerationState::Unknown
    );
    let record = store.read_execution(&e.execution_id).unwrap();
    assert_eq!(record.snapshot.unwrap().revision, confirmed.revision);
}
pub(crate) fn enroll(
    store: &AutomationStore,
    e: &ExecutionRecord,
    method: CredentialUpdateMethod,
) -> EffectiveCredentialSlot {
    enroll_source(store, e, method, false)
}
pub(crate) fn enroll_source(
    store: &AutomationStore,
    e: &ExecutionRecord,
    method: CredentialUpdateMethod,
    reread: bool,
) -> EffectiveCredentialSlot {
    let authority = store
        .credential_authority(&e.owner_key, &"a".repeat(64), "fixture-source")
        .unwrap();
    store
        .register_credential_slot(
            &e.owner_key,
            &e.execution_id,
            authority,
            EffectiveCredentialSlot {
                route: CredentialUpdateRoute {
                    credential_kind: CredentialKind::ApiKey,
                    delivery: if method == CredentialUpdateMethod::Live {
                        CredentialDelivery::Helper
                    } else {
                        CredentialDelivery::Env
                    },
                    model_provider: Some("fixture".into()),
                    live_update: Some(method == CredentialUpdateMethod::Live),
                    restart_supported: Some(true),
                    refresh_owner: if reread {
                        RefreshOwner::Awman
                    } else {
                        RefreshOwner::Caller
                    },
                    update_boundary: Some(UpdateBoundary::NextRequest),
                    max_apply_delay: Some(2000),
                    restart_scope: Some(RestartScope::Execution),
                    conversation_continuity: Some(ConversationContinuity::NativeResume),
                    reason: None,
                },
                update_method: method,
                credential_slot: String::new(),
                secret_names: vec!["OPENAI_API_KEY".into()],
                credential_generation: 0,
                pending_generation: None,
                generation_state: GenerationState::Unknown,
                admission_while_pending: false,
                available_operation: Some(CredentialOperationAvailability {
                    operation: "worker.commands.credentials.update".into(),
                    replacement_secrets: true,
                    reread_source: reread,
                }),
                latest_update: None,
                identity_continuity: IdentityContinuity::Unverified,
            },
        )
        .unwrap()
}
pub(crate) fn intent(
    e: &ExecutionRecord,
    slot: &EffectiveCredentialSlot,
) -> CredentialUpdateIntent {
    CredentialUpdateIntent {
        request_id: Uuid::new_v4(),
        execution_id: e.execution_id.clone(),
        expected_revision: e.revision,
        credential_slot: slot.credential_slot.clone(),
        expected_credential_generation: slot.credential_generation,
        reread_source: false,
        secret_names: slot.secret_names.clone(),
    }
}
#[test]
fn generations_burn_on_failure_and_terminal_results_are_immutable() {
    let sandbox = crate::support::sandbox(
        "data::credential_updates::generations_burn_on_failure_and_terminal_results_are_immutable",
    );
    let (db, store, e) = super::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    assert_eq!(slot.pending_generation, Some(1));
    let request = intent(&e, &slot);
    let accepted = store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    assert_eq!(accepted.new_generation, 2);
    store
        .stage_credential_generation(&e.owner_key, &e.command_id, request.request_id, true, None)
        .unwrap();
    assert_eq!(
        store
            .effective_credentials(&e.owner_key, &e.execution_id)
            .unwrap()
            .slots[0]
            .credential_generation,
        0
    );
    let failed = store
        .fail_credential_update(
            &e.owner_key,
            &e.command_id,
            request.request_id,
            CredentialUpdateFailure {
                code: "credential_update_failed",
                reason: "application_unconfirmed",
                exposed: true,
            },
            None,
        )
        .unwrap();
    assert_eq!(failed.generation_state, GenerationState::Unknown);
    assert!(store
        .ack_credential_generation(
            &e.owner_key,
            &e.command_id,
            request.request_id,
            &e.execution_id,
            2,
            None
        )
        .is_err());
    let reopened = AutomationStore::open(db).unwrap();
    assert_eq!(
        reopened
            .lookup_credential_update(&e.owner_key, &e.command_id, &request)
            .unwrap(),
        Some(failed)
    );
    let mut next = request.clone();
    next.request_id = Uuid::new_v4();
    next.expected_revision = reopened.read_execution(&e.execution_id).unwrap().revision;
    let accepted = reopened
        .reserve_credential_update(&e.owner_key, &e.command_id, &next, 2000)
        .unwrap();
    assert_eq!(accepted.new_generation, 3);
    let effective = reopened
        .effective_credentials(&e.owner_key, &e.execution_id)
        .unwrap();
    assert_eq!(effective.slots[0].pending_generation, Some(3));
}
#[test]
fn two_store_instances_serialize_active_updates_and_recovery_loses_material() {
    let sandbox=crate::support::sandbox("data::credential_updates::two_store_instances_serialize_active_updates_and_recovery_loses_material");
    let path = sandbox.tmp().join("db");
    let (_db, store, e) = super::automation_store::setup(&path);
    let other = AutomationStore::open(std::sync::Arc::new(
        awman::data::fs::api_db::SqliteSessionStore::open(&path).unwrap(),
    ))
    .unwrap();
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let request = intent(&e, &slot);
    let accepted = store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    let mut conflicting = request.clone();
    conflicting.request_id = Uuid::new_v4();
    conflicting.expected_revision += 1;
    assert!(other
        .reserve_credential_update(&e.owner_key, &e.command_id, &conflicting, 2000)
        .is_err());
    assert_eq!(
        other
            .lookup_credential_update(&e.owner_key, &e.command_id, &request)
            .unwrap(),
        Some(accepted)
    );
    other.recover_credential_updates().unwrap();
    let failed = store
        .get_credential_update(&e.owner_key, &e.command_id, request.request_id)
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, CredentialUpdateStatus::Failed);
    assert_eq!(failed.failure.unwrap().code, "credentials_lost");
    let connection = awman::data::fs::api_db::SqliteSessionStore::open(&path).unwrap();
    let _ = AutomationStore::open(std::sync::Arc::new(connection)).unwrap();
}

#[test]
fn concurrent_identical_reservations_share_one_generation_and_request_namespace() {
    let sandbox=crate::support::sandbox("data::credential_updates::concurrent_identical_reservations_share_one_generation_and_request_namespace");
    let path = sandbox.tmp().join("db");
    let (_db, store, e) = super::automation_store::setup(&path);
    let other = AutomationStore::open(std::sync::Arc::new(
        awman::data::fs::api_db::SqliteSessionStore::open(&path).unwrap(),
    ))
    .unwrap();
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let request = intent(&e, &slot);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        let a = barrier.clone();
        let e = &e;
        let request = &request;
        let store = &store;
        let other = &other;
        let left = scope.spawn(move || {
            a.wait();
            store.reserve_credential_update(&e.owner_key, &e.command_id, request, 2000)
        });
        let b = barrier.clone();
        let right = scope.spawn(move || {
            b.wait();
            other.reserve_credential_update(&e.owner_key, &e.command_id, request, 2000)
        });
        barrier.wait();
        assert_eq!(
            left.join().unwrap().unwrap(),
            right.join().unwrap().unwrap()
        );
    });
    assert!(operation_digest(
        Operation::CredentialUpdate,
        &e.execution_id,
        e.revision,
        "test-secret-never-hashed"
    )
    .is_err());
    let db = std::fs::read(path.join(awman::data::fs::api_paths::API_DB_FILENAME)).unwrap();
    assert!(!db
        .windows(b"test-secret".len())
        .any(|s| s == b"test-secret"));
}

#[test]
fn pre_rotation_database_migrates_twice_without_losing_request_replay() {
    let sandbox=crate::support::sandbox("data::credential_updates::pre_rotation_database_migrates_twice_without_losing_request_replay");
    let root = sandbox.tmp().join("db");
    let (db, _store, e) = super::automation_store::setup(&root);
    let request = Uuid::new_v4();
    db.with_conn(|c| {
        c.execute_batch("DROP TABLE agent_credential_updates; DROP TABLE agent_credential_slots; DROP TABLE agent_credential_authorities;")?;
        let current:String=c.query_row("SELECT sql FROM sqlite_master WHERE name='agent_request_dedup'",[],|r|r.get(0))?;
        let legacy=current.replace(",'credential_update'","").replace("operation_digest TEXT,","operation_digest TEXT NOT NULL,");
        c.execute_batch("DROP TABLE agent_request_dedup;")?;c.execute_batch(&legacy)?;
        c.execute("INSERT INTO agent_request_dedup(owner_key,execution_id,request_id,command_id,operation,operation_digest,response_json,created_at) VALUES(?,?,?,?,'close_execution','fixture-digest',?,?)",rusqlite::params![e.owner_key,e.execution_id,request.to_string(),e.command_id,serde_json::to_string(&StoredReply::PendingClose).unwrap(),chrono::Utc::now().to_rfc3339()])?;
        Ok(())
    }).unwrap();
    for _ in 0..2 {
        let reopened = AutomationStore::open(std::sync::Arc::new(
            awman::data::fs::api_db::SqliteSessionStore::open(&root).unwrap(),
        ))
        .unwrap();
        assert!(matches!(
            reopened
                .lookup_request(&e.owner_key, &e.execution_id, request, "fixture-digest")
                .unwrap(),
            RequestLookup::Replay {
                response: StoredReply::PendingClose,
                ..
            }
        ));
    }
}

#[test]
fn initial_consumption_is_confirmed_only_before_a_new_generation_is_allocated() {
    let sandbox=crate::support::sandbox("data::credential_updates::initial_consumption_is_confirmed_only_before_a_new_generation_is_allocated");
    let (_db, store, e) = super::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    assert_eq!(slot.credential_generation, 0);
    store
        .ack_initial_credential_generation(
            &e.owner_key,
            &e.execution_id,
            &slot.credential_slot,
            None,
        )
        .unwrap();
    let projection = store
        .effective_credentials(&e.owner_key, &e.execution_id)
        .unwrap();
    assert_eq!(projection.slots[0].credential_generation, 1);
    let record = store.read_execution(&e.execution_id).unwrap();
    let request = intent(&record, &projection.slots[0]);
    store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    assert!(store
        .ack_initial_credential_generation(
            &e.owner_key,
            &e.execution_id,
            &slot.credential_slot,
            None
        )
        .is_err());
}

#[test]
fn restart_requires_continuity_before_reservation_and_rejects_old_consumption() {
    let sandbox = crate::support::sandbox("data::credential_updates::restart_requires_continuity_before_reservation_and_rejects_old_consumption");
    let (db, store, e) = super::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Restart);
    let request = intent(&e, &slot);
    db.with_conn(|c| { c.execute("UPDATE agent_credential_slots SET record_json=json_set(record_json,'$.route.conversation_continuity',NULL) WHERE slot=?", [&slot.credential_slot])?; Ok(()) }).unwrap();
    assert!(store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .is_err());
    assert_eq!(
        store.read_execution(&e.execution_id).unwrap().revision,
        e.revision
    );
    db.with_conn(|c| { c.execute("UPDATE agent_credential_slots SET record_json=json_set(record_json,'$.route.conversation_continuity','native_resume') WHERE slot=?", [&slot.credential_slot])?; Ok(()) }).unwrap();
    let op = store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    store
        .mark_credential_update_exposed(&e.owner_key, &e.command_id, op.request_id)
        .unwrap();
    assert!(store
        .ack_credential_generation(
            &e.owner_key,
            &e.command_id,
            op.request_id,
            &e.execution_id,
            op.new_generation,
            None
        )
        .is_err());
    assert_eq!(
        store
            .get_credential_update(&e.owner_key, &e.command_id, op.request_id)
            .unwrap()
            .unwrap()
            .status,
        CredentialUpdateStatus::Pending
    );
}

#[test]
fn crash_recovery_preserves_confirmed_material_until_delivery_started() {
    let sandbox = crate::support::sandbox("data::credential_updates::crash_recovery_preserves_confirmed_material_until_delivery_started");
    for exposed in [false, true] {
        let (_db, store, e) =
            super::automation_store::setup(&sandbox.tmp().join(format!("db-{exposed}")));
        let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
        store
            .ack_initial_credential_generation(
                &e.owner_key,
                &e.execution_id,
                &slot.credential_slot,
                None,
            )
            .unwrap();
        let current = store.read_execution(&e.execution_id).unwrap();
        let confirmed = store
            .effective_credentials(&e.owner_key, &e.execution_id)
            .unwrap()
            .slots
            .remove(0);
        let request = intent(&current, &confirmed);
        store
            .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
            .unwrap();
        if exposed {
            store
                .mark_credential_update_exposed(&e.owner_key, &e.command_id, request.request_id)
                .unwrap();
        }
        store.recover_credential_updates().unwrap();
        let slot = store
            .effective_credentials(&e.owner_key, &e.execution_id)
            .unwrap()
            .slots
            .remove(0);
        assert_eq!(slot.credential_generation, 1);
        assert_eq!(
            slot.generation_state,
            if exposed {
                GenerationState::Unknown
            } else {
                GenerationState::Confirmed
            }
        );
        assert_eq!(slot.update_method, slot.route.method(slot.generation_state));
        assert!(slot.latest_update.unwrap().status.is_terminal());
    }
}

#[test]
fn exposed_column_upgrade_is_idempotent_and_new_reservations_remain_unexposed() {
    let sandbox = crate::support::sandbox("data::credential_updates::exposed_column_upgrade_is_idempotent_and_new_reservations_remain_unexposed");
    let (db, store, e) = super::automation_store::setup(&sandbox.tmp().join("db"));
    let slot = enroll(&store, &e, CredentialUpdateMethod::Live);
    let request = intent(&e, &slot);
    store
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    db.with_conn(|c| {
        c.execute_batch("ALTER TABLE agent_credential_updates DROP COLUMN exposed;")?;
        Ok(())
    })
    .unwrap();
    let upgraded = AutomationStore::open(db.clone()).unwrap();
    upgraded.recover_credential_updates().unwrap();
    let recovered = upgraded
        .effective_credentials(&e.owner_key, &e.execution_id)
        .unwrap();
    assert_eq!(
        recovered.slots[0].generation_state,
        GenerationState::Unknown
    );
    let current = upgraded.read_execution(&e.execution_id).unwrap();
    let request = intent(&current, &recovered.slots[0]);
    upgraded
        .reserve_credential_update(&e.owner_key, &e.command_id, &request, 2000)
        .unwrap();
    let _again = AutomationStore::open(db.clone()).unwrap();
    db.with_conn(|c| {
        let exposed: bool = c.query_row(
            "SELECT exposed FROM agent_credential_updates WHERE request_id=?",
            [request.request_id.to_string()],
            |r| r.get(0),
        )?;
        assert!(!exposed);
        Ok(())
    })
    .unwrap();
}
