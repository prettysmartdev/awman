//! Production relay contracts retained from the nextgen daemon fixes.
use awman::command::instance::{
    feeds::{ContainerSlotEvent, ContainerSlotIo, TabCommandFeeds},
    relay::{InstanceEvent, TabRelay},
};
use awman::data::{
    frontend_protocol::{TabKindWire, WorkflowRequestWire},
    ids::TabId,
    instance_snapshot::InstanceDelta,
    session::{Session, SessionOpenOptions},
    workflow_state::{WorkflowPhase, WorkflowSummary},
};
use awman::engine::workflow::EngineRequest;

#[tokio::test]
async fn attaching_stuck_events_preserves_workflow_requests_and_parallel_backbone() {
    let s = crate::support::sandbox(
        "command::relay::attaching_stuck_events_preserves_workflow_requests_and_parallel_backbone",
    );
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let feeds = TabCommandFeeds::new();
    let mut relay = TabRelay::new(
        TabId::new(),
        &session,
        TabKindWire::Normal,
        feeds.clone(),
        None,
        (80, 24),
    );
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let launch_id = feeds.container.launch_id();
    let (_initial_stdout, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _initial_stdin) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _initial_resize) = tokio::sync::mpsc::unbounded_channel();
    feeds
        .container
        .push(ContainerSlotEvent::SequentialLaunched {
            launch_id,
            title: Some("initial step".into()),
            io: ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        });
    feeds.container.push(ContainerSlotEvent::EngineAttached {
        launch_id,
        requests: Some(requests),
        stuck: None,
    });
    relay.pump(Some(&session));
    let (stuck, rx) = tokio::sync::broadcast::channel(8);
    feeds.container.push(ContainerSlotEvent::EngineAttached {
        launch_id,
        requests: None,
        stuck: Some(rx),
    });
    relay.pump(Some(&session));
    relay.request_workflow(WorkflowRequestWire::OpenControlBoard {
        step_name: "build".into(),
    });
    assert!(
        matches!(received.try_recv(), Ok(EngineRequest::OpenControlBoard {step_name}) if step_name == "build")
    );
    feeds.container.push(ContainerSlotEvent::GroupStarted);
    relay.pump(Some(&session));
    let (_stdout, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _stdin) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _resize) = tokio::sync::mpsc::unbounded_channel();
    feeds
        .container
        .push(ContainerSlotEvent::SequentialLaunched {
            launch_id: uuid::Uuid::new_v4(),
            title: Some("next step".into()),
            io: ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        });
    relay.pump(Some(&session));
    let snapshot = relay.snapshot();
    let backbone = snapshot
        .slots
        .iter()
        .find(|slot| slot.step_name == "next step")
        .unwrap();
    assert!(
        backbone.dormant,
        "parallel steps still own the visible slots"
    );
    drop(stuck);
    drop(relay);
    s.finish();
}

#[test]
fn workflow_progress_is_relayed_even_when_the_phase_does_not_change() {
    let s = crate::support::sandbox(
        "command::relay::workflow_progress_is_relayed_even_when_the_phase_does_not_change",
    );
    let mut session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let mut relay = TabRelay::new(
        TabId::new(),
        &session,
        TabKindWire::Normal,
        TabCommandFeeds::new(),
        None,
        (80, 24),
    );
    let mut summary = WorkflowSummary {
        invocation_id: uuid::Uuid::new_v4(),
        workflow_name: "build".into(),
        work_item: None,
        phase: WorkflowPhase::Main,
        total_steps: 2,
        completed_steps: 0,
        failed_steps: 0,
        current_step: Some("compile".into()),
        updated_at: chrono::Utc::now(),
    };
    for (step, completed) in [("compile", 0), ("test", 1)] {
        summary.current_step = Some(step.into());
        summary.completed_steps = completed;
        session.state_mut().current_workflow = Some(summary.clone());
        let events = relay.pump(Some(&session));
        assert!(events.iter().any(|event| matches!(event, InstanceEvent::Delta(InstanceDelta::Phase {
            current_workflow: Some(workflow), .. }) if workflow.current_step.as_deref() == Some(step) && workflow.completed_steps == completed)));
    }
    drop(relay);
    s.finish();
}
