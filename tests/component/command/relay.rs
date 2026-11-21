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

#[test]
fn complete_phase_plans_reach_the_overview_and_api_before_steps_start() {
    use awman::command::dispatch::{catalogue::CommandCatalogue, parsed_input};
    use awman::command::instance::frontend::{DialogChannels, InstanceCommandFrontend};
    use awman::data::instance_snapshot::{StepViewStatus, WorkflowStepKind};
    use awman::data::workflow_state::{PhaseKind, PhaseStepState, PhaseStepStatus, WorkflowState};
    use awman::engine::agent_runtime::frontend::AgentIo;
    use awman::engine::workflow::{
        actions::{WorkflowStepProgressInfo, WorkflowStepStatus},
        frontend::WorkflowFrontend,
    };

    let s = crate::support::sandbox(
        "command::relay::complete_phase_plans_reach_the_overview_and_api_before_steps_start",
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
    let (requests, _requests_rx) = std::sync::mpsc::channel();
    let (_responses, responses_rx) = std::sync::mpsc::channel();
    let (stdout, _stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut frontend = InstanceCommandFrontend::new(
        parsed_input::parse("exec workflow example.toml", CommandCatalogue::get()).unwrap(),
        DialogChannels::new(requests, responses_rx),
        AgentIo {
            stdout: stdout.clone(),
            stderr: stdout,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        feeds.clone(),
    );
    let mut state = WorkflowState::new("example".into(), &[], "hash".into(), None);
    state.setup_step_states = vec![PhaseStepState {
        description: "run_shell: prepare".into(),
        status: PhaseStepStatus::Succeeded,
    }];
    // Identical descriptions must keep distinct positions and statuses.
    state.teardown_step_states = vec![
        PhaseStepState {
            description: "run_shell: verify".into(),
            status: PhaseStepStatus::Pending,
        };
        3
    ];
    frontend.report_workflow_state(&state);
    let progress = [WorkflowStepProgressInfo {
        name: "implement".into(),
        agent: "claude".into(),
        model: Some("resolved-model".into()),
        has_step_override: true,
        status: WorkflowStepStatus::Succeeded,
        depends_on: vec![],
        max_concurrent: Some(2),
    }];
    frontend.report_workflow_progress(&progress);
    state.current_phase = WorkflowPhase::Teardown;

    for (statuses, expected) in [
        (
            [
                PhaseStepStatus::Pending,
                PhaseStepStatus::Pending,
                PhaseStepStatus::Pending,
            ],
            [
                StepViewStatus::Pending,
                StepViewStatus::Pending,
                StepViewStatus::Pending,
            ],
        ),
        (
            [
                PhaseStepStatus::Running,
                PhaseStepStatus::Pending,
                PhaseStepStatus::Pending,
            ],
            [
                StepViewStatus::Running,
                StepViewStatus::Pending,
                StepViewStatus::Pending,
            ],
        ),
        (
            [
                PhaseStepStatus::Succeeded,
                PhaseStepStatus::Running,
                PhaseStepStatus::Pending,
            ],
            [
                StepViewStatus::Done,
                StepViewStatus::Running,
                StepViewStatus::Pending,
            ],
        ),
        (
            [
                PhaseStepStatus::Succeeded,
                PhaseStepStatus::Failed {
                    error: "failed".into(),
                },
                PhaseStepStatus::Skipped,
            ],
            [
                StepViewStatus::Done,
                StepViewStatus::Error,
                StepViewStatus::Skipped,
            ],
        ),
    ] {
        for (step, status) in state.teardown_step_states.iter_mut().zip(statuses) {
            step.status = status;
        }
        frontend.report_workflow_state(&state);
        if expected[1] == StepViewStatus::Running {
            // The engine persists first, then sends the notification.
            frontend.on_phase_step_started(PhaseKind::Teardown, "run_shell: verify");
        }
        // Main-step progress must not remove pending phase rows.
        frontend.report_workflow_progress(&progress);
        let overview = feeds.workflow.snapshot().view.unwrap();
        assert_eq!(overview.steps.len(), 5);
        assert_eq!(overview.steps[0].kind, WorkflowStepKind::Setup);
        assert_eq!(overview.steps[0].status, StepViewStatus::Done);
        assert_eq!(overview.steps[1].kind, WorkflowStepKind::Agent);
        assert_eq!(overview.steps[1].agent.as_deref(), Some("claude"));
        assert_eq!(overview.steps[1].model.as_deref(), Some("resolved-model"));
        assert_eq!(overview.max_concurrent, Some(2));
        for (step, status) in overview.steps[2..].iter().zip(expected) {
            assert_eq!(step.kind, WorkflowStepKind::Teardown);
            assert_eq!(step.name, "run_shell: verify");
            assert_eq!(step.status, status);
        }
        let updates = relay.pump(Some(&session));
        assert!(updates.iter().any(|event| matches!(event,
            InstanceEvent::Delta(InstanceDelta::Workflow { view: Some(view), .. })
                if view == &overview
        )));
        let api_snapshot = serde_json::to_value(relay.snapshot()).unwrap();
        assert_eq!(
            api_snapshot["workflow"],
            serde_json::to_value(&overview).unwrap()
        );
        let worker_view = awman::command::daemon_api::workflow_view::client_view(
            serde_json::to_value(&state).unwrap(),
        );
        assert_eq!(worker_view["phase"], "teardown");
        assert_eq!(
            worker_view["teardown_step_states"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    drop(relay);
    s.finish();
}

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
