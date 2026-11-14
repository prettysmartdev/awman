//! Generated action sequences run against production decision/state machines.
//! Fixed seeds and minimal action traces live beside this module.
use super::support::{prompt, AdmissionFixture};
use awman::command::commands::api_server::{admission::AdmissionError, CloseOutcome};
use awman::command::commands::{RepairDecision, WorkflowRepairLoop};
use awman::data::command_status::CommandStatus;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::session_status::SessionStatus;
use awman::data::workflow_definition::Workflow;
use awman_test_support::{evidence, fixtures::WorkflowBuilder};
use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};

const SEED: u64 = 144_310_064;
fn runner() -> TestRunner {
    evidence::note("seed", &SEED.to_string());
    TestRunner::new(Config {
        cases: 64,
        rng_seed: RngSeed::Fixed(SEED),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/component/command/models.proptest-regressions"
        )))),
        ..Config::default()
    })
}

fn session_trace(actions: &[u8], id: &'static str) {
    let s = crate::support::sandbox(id);
    s.note("seed", &SEED.to_string());
    s.note("actions", &format!("{actions:?}"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut f = AdmissionFixture::new(&s);
        let id = f
            .admission
            .admit_session(f.local(&s), None, false, None)
            .await
            .unwrap()
            .session_id;
        // Independent reference model: initializing -> ready -> closed;
        // close cancels all queued work, and reopening storage preserves it.
        let mut closed = false;
        let mut ready = false;
        let mut commands = Vec::new();
        for action in actions {
            match action {
                0 => {
                    let result = f
                        .admission
                        .admit_command(&id, prompt("literal payload"), None)
                        .await;
                    if closed {
                        assert!(matches!(result, Err(AdmissionError::Session(_))));
                    } else if !ready {
                        assert!(matches!(result, Err(AdmissionError::Setup(_))));
                    } else {
                        commands.push(result.unwrap().command_id);
                    }
                }
                1 => {
                    assert_eq!(f.lifecycle.close(&id).await.unwrap(), CloseOutcome::Closed);
                    closed = true;
                }
                2 if !closed => {
                    f.store
                        .update_setup_status(&id, SessionSetupStatus::Ready)
                        .unwrap();
                    ready = true;
                }
                3 => {
                    f = AdmissionFixture::new(&s);
                }
                _ => {}
            }
            let row = f.store.get_session(&id).unwrap().unwrap();
            assert_eq!(
                row.status,
                if closed {
                    SessionStatus::Closed
                } else {
                    SessionStatus::Active
                }
            );
            let view = f.lifecycle.queue_view(&id).unwrap().unwrap();
            assert_eq!(
                view.queue_depth,
                if closed { 0 } else { commands.len() as i64 }
            );
            for id in &commands {
                assert_eq!(
                    f.store.get_command(id).unwrap().unwrap().status,
                    if closed {
                        CommandStatus::Cancelled
                    } else {
                        CommandStatus::Queued
                    }
                );
            }
        }
    });
    s.finish();
}

#[test]
fn session_admission_close_and_reopen_follow_the_reference_model() {
    runner()
        .run(&proptest::collection::vec(0u8..4, 1..36), |actions| {
            session_trace(
                &actions,
                "command::models::session_admission_close_and_reopen_follow_the_reference_model",
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn session_minimal_reproductions_preserve_closed_admission_and_cancelled_work() {
    // Hand-minimized safety traces; these are replayed independently of generation.
    for actions in [&[1, 0][..], &[2, 0, 1, 3, 0][..], &[0, 3, 2, 0, 1, 1][..]] {
        session_trace(actions, "command::models::session_minimal_reproductions_preserve_closed_admission_and_cancelled_work");
    }
}

fn repair_trace(actions: &[u8], id: &'static str) {
    let s = crate::support::sandbox(id);
    s.note("seed", &SEED.to_string());
    s.note("actions", &format!("{actions:?}"));
    let path = WorkflowBuilder::new("accepted")
        .step("one", "literal", &[])
        .write(&s.work(), "generated");
    let workflow = Workflow::load(&path).unwrap();
    let mut sut = WorkflowRepairLoop::new(&path, "original");
    let mut failures = 0;
    for (index, action) in actions.iter().enumerate() {
        match action {
            0 => {
                let error = format!("validation-{index}");
                failures += 1;
                match sut.record(Err(error.clone())) {
                    RepairDecision::Retry {
                        attempt,
                        error: observed,
                    } => {
                        assert!(failures <= 3);
                        assert_eq!(attempt, failures);
                        assert_eq!(observed, error);
                        assert!(sut.prompt().contains(&error));
                    }
                    RepairDecision::Exhausted(message) => {
                        assert!(failures > 3);
                        assert!(message.contains(&error));
                    }
                    RepairDecision::Accepted(_) => panic!("invalid workflow was accepted"),
                }
            }
            1 => {
                std::fs::write(&path, "partial leader output").unwrap();
                sut.restart();
                failures = 0;
                assert!(!path.exists());
                assert_eq!(sut.prompt(), "original");
            }
            2 => {
                let RepairDecision::Accepted(accepted) = sut.record(Ok(workflow.clone())) else {
                    panic!("valid workflow rejected")
                };
                assert_eq!(accepted.title, workflow.title);
            }
            _ => unreachable!(),
        }
        assert_eq!(sut.attempt(), failures);
        assert_eq!(sut.is_first_attempt(), failures == 0);
        assert_eq!(
            sut.label(),
            if failures == 0 {
                "leader".into()
            } else {
                format!("leader-repair-{failures}")
            }
        );
    }
    s.finish();
}

#[test]
fn workflow_repair_accept_restart_and_exhaustion_follow_the_reference_model() {
    runner()
        .run(&proptest::collection::vec(0u8..3, 1..48), |actions| {
            repair_trace(&actions, "command::models::workflow_repair_accept_restart_and_exhaustion_follow_the_reference_model");
            Ok(())
        })
        .unwrap();
}

#[test]
fn workflow_minimal_reproductions_reset_budget_and_keep_the_fourth_failure_terminal() {
    for actions in [&[0, 0, 0, 0][..], &[0, 0, 1, 0, 2][..], &[2][..]] {
        repair_trace(actions, "command::models::workflow_minimal_reproductions_reset_budget_and_keep_the_fourth_failure_terminal");
    }
}
