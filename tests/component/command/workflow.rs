use super::support::AdmissionFixture;
use awman::command::commands::api_server::{CancelOutcome, CommandCanceller, TokenRegistry};
use awman::data::api_lifecycle_store::CommandLifecycleStore;
use awman::data::fs::api_db::IdempotencyBegin;
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_idempotent_submissions_have_exactly_one_owner() {
    let s = crate::support::sandbox(
        "command::workflow::concurrent_idempotent_submissions_have_exactly_one_owner",
    );
    let f = AdmissionFixture::new(&s);
    let barrier = Arc::new(Barrier::new(16));
    let outcomes = std::thread::scope(|scope| {
        let mut tasks = vec![];
        for _ in 0..16 {
            let barrier = barrier.clone();
            let store = f.store.clone();
            tasks.push(scope.spawn(move || {
                let journal =
                    awman::command::commands::api_server::admission::IdempotentAdmission::new(
                        store,
                        "run",
                        "same-principal",
                        "same-key",
                    );
                barrier.wait();
                let outcome = journal.begin("same-body").unwrap();
                barrier.wait();
                drop(journal);
                outcome
            }));
        }
        tasks
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == IdempotencyBegin::Fresh)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == IdempotencyBegin::InFlight)
            .count(),
        15
    );
    s.finish();
}

#[test]
fn cancellation_is_idempotent_and_never_fires_another_commands_token() {
    let s = crate::support::sandbox(
        "command::workflow::cancellation_is_idempotent_and_never_fires_another_commands_token",
    );
    let f = AdmissionFixture::new(&s);
    let commands = Arc::new(TokenRegistry::new());
    let setups = Arc::new(TokenRegistry::new());
    let session = awman::data::ids::WorkerSessionId::new().to_string();
    let one_id = awman::data::ids::CommandId::new().to_string();
    let two_id = awman::data::ids::CommandId::new().to_string();
    f.store
        .insert_session(&session, s.work().to_str().unwrap(), "2026-01-01T00:00:00Z")
        .unwrap();
    let cancel = CommandCanceller::new(
        f.store.clone(),
        CommandLifecycleStore::open(f.store.clone()).unwrap(),
        commands.clone(),
        setups,
    );
    assert!(matches!(
        cancel.cancel("missing").unwrap(),
        CancelOutcome::NotFound
    ));
    f.store
        .insert_command(&one_id, &session, "exec workflow", "[]", "log")
        .unwrap();
    f.store
        .insert_command(&two_id, &session, "exec prompt", "[]", "log")
        .unwrap();
    f.store
        .update_command_started(&one_id, "2026-01-01T00:00:00Z")
        .unwrap();
    let one = commands.register(&one_id);
    let two = commands.register(&two_id);
    assert!(matches!(
        cancel.cancel(&one_id).unwrap(),
        CancelOutcome::Stopping { .. }
    ));
    assert!(one.is_cancelled());
    assert!(!two.is_cancelled());
    assert!(matches!(
        cancel.cancel(&two_id).unwrap(),
        CancelOutcome::Cancelled { .. }
    ));
    let terminal = f.store.get_command(&two_id).unwrap().unwrap();
    assert!(matches!(
        cancel.cancel(&two_id).unwrap(),
        CancelOutcome::AlreadyTerminal { .. }
    ));
    assert_eq!(f.store.get_command(&two_id).unwrap().unwrap(), terminal);
    s.finish();
}

#[test]
fn crash_completion_is_finalized_before_cancellation_can_overwrite_it() {
    use awman::data::command_status::{CommandCompletion, CommandStatus};
    let s = crate::support::sandbox(
        "command::workflow::crash_completion_is_finalized_before_cancellation_can_overwrite_it",
    );
    let f = AdmissionFixture::new(&s);
    let session = awman::data::ids::WorkerSessionId::new().to_string();
    let id = awman::data::ids::CommandId::new().to_string();
    f.store
        .insert_session(&session, s.work().to_str().unwrap(), "2026-01-01T00:00:00Z")
        .unwrap();
    f.store
        .insert_command(&id, &session, "exec workflow", "[]", "log")
        .unwrap();
    f.store
        .update_command_started(&id, "2026-01-01T00:00:00Z")
        .unwrap();
    let row = f.store.get_command(&id).unwrap().unwrap();
    f.paths
        .write_command_completion(
            &session,
            &id,
            &CommandCompletion {
                attempt: row.attempt,
                status: CommandStatus::Done,
                exit_code: 0,
                result_json: "{\"exit_code\":0}".into(),
                finished_at: "2026-01-01T00:01:00Z".into(),
            },
        )
        .unwrap();
    let cancel = CommandCanceller::new(
        f.store.clone(),
        CommandLifecycleStore::open(f.store.clone()).unwrap(),
        Arc::new(TokenRegistry::new()),
        Arc::new(TokenRegistry::new()),
    )
    .with_completion_paths(f.paths.clone());
    assert!(matches!(
        cancel.cancel(&id).unwrap(),
        CancelOutcome::AlreadyTerminal(_)
    ));
    let row = f.store.get_command(&id).unwrap().unwrap();
    assert_eq!(row.status, CommandStatus::Done);
    assert_eq!(row.exit_code, Some(0));
    assert!(matches!(
        cancel.cancel(&id).unwrap(),
        CancelOutcome::AlreadyTerminal(_)
    ));
    s.finish();
}

/// Guards: 8cf45de1 consumers subscribe before any producer can publish.
#[test]
fn concurrent_workflow_events_have_one_order_and_done_fences_late_output() {
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::data::execution_event::EventPayload;
    let s = crate::support::sandbox(
        "command::workflow::concurrent_workflow_events_have_one_order_and_done_fences_late_output",
    );
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let mut live = bus.subscribe();
    std::thread::scope(|scope| {
        for producer in 0..8 {
            let tx = bus.sender();
            scope.spawn(move || {
                for item in 0..32 {
                    tx.emit(EventPayload::StdoutLine(format!("{producer}:{item}")));
                }
            });
        }
    });
    bus.sender().emit(EventPayload::Done);
    bus.sender()
        .emit(EventPayload::StdoutLine("too late".into()));
    let mut sequence = 0;
    while let Ok(persisted) = log.try_recv() {
        let streamed = live.try_recv().unwrap();
        assert_eq!(persisted.sequence, sequence);
        assert_eq!(streamed.sequence, sequence);
        assert_eq!(
            serde_json::to_value(&persisted).unwrap(),
            serde_json::to_value(&streamed).unwrap()
        );
        sequence += 1;
    }
    assert_eq!(sequence, 257);
    assert!(live.try_recv().is_err());
    s.finish();
}
