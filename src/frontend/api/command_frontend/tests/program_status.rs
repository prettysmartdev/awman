//! WI 0141: `program_status` events from `report_agent_status`.

use super::super::*;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::data::execution_event::ExecutionEvent;
use crate::data::program_status::{BlockedKind, EffectiveStatus, ProgramState};

fn status(state: ProgramState) -> EffectiveStatus {
    EffectiveStatus {
        state,
        kind: None,
        progress: None,
        msg: None,
        app: None,
        id: Some("tab1/0".into()),
    }
}

fn frontend(bus: &EventBus) -> ApiDispatchFrontend {
    let mut frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
    frontend.set_step_order(["plan".to_string(), "build".to_string()].into_iter());
    frontend
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<ExecutionEvent>) -> Vec<ExecutionEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn blocked_status_is_a_program_status_event_with_spec_spellings() {
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let mut frontend = frontend(&bus);
    let blocked = EffectiveStatus {
        kind: Some(BlockedKind::Permission),
        progress: Some(40),
        msg: Some("Allow edit to src/main.rs?".into()),
        app: Some("claude".into()),
        ..status(ProgramState::Blocked)
    };
    frontend.report_agent_status("build", Some(&blocked));

    let events = drain(&mut rx);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.payload.sse_event_type(), "program_status");
    assert!(event.payload.to_plain_text().is_none());
    let source = event.source.as_ref().expect("attributed to the step");
    assert_eq!(source.step_name.as_deref(), Some("build"));
    assert_eq!(source.step_index, Some(1));

    let json = serde_json::to_value(event).unwrap();
    assert_eq!(json["payload"]["type"], "ProgramStatus");
    assert_eq!(
        json["payload"]["data"],
        serde_json::json!({
            "state": "blocked",
            "kind": "permission",
            "progress": 40,
            "msg": "Allow edit to src/main.rs?",
            "app": "claude",
        })
    );
}

#[test]
fn every_state_uses_its_snake_case_name_and_omits_absent_fields() {
    for (state, name) in [
        (ProgramState::Idle, "idle"),
        (ProgramState::Working, "working"),
        (ProgramState::Done, "done"),
        (ProgramState::Blocked, "blocked"),
        (ProgramState::Error, "error"),
    ] {
        let payload = program_status_event(&status(state));
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["data"], serde_json::json!({ "state": name }), "{name}");
    }
    for (kind, name) in [
        (BlockedKind::Permission, "permission"),
        (BlockedKind::Question, "question"),
        (BlockedKind::Auth, "auth"),
    ] {
        let payload = program_status_event(&EffectiveStatus {
            kind: Some(kind),
            ..status(ProgramState::Blocked)
        });
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["data"]["kind"], name);
    }
}

#[test]
fn the_event_round_trips_and_never_carries_the_record_id() {
    let payload = program_status_event(&EffectiveStatus {
        msg: Some("tests failing".into()),
        ..status(ProgramState::Error)
    });
    let json = serde_json::to_string(&payload).unwrap();
    assert!(!json.contains("tab1/0"), "{json}");
    let back: EventPayload = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        back,
        EventPayload::ProgramStatus { state: ProgramState::Error, msg: Some(ref m), .. } if m == "tests failing"
    ));
}

#[test]
fn a_label_that_is_not_a_step_reports_unattributed_and_clear_emits_nothing() {
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let mut frontend = frontend(&bus);
    frontend.report_agent_status("leader", Some(&status(ProgramState::Working)));
    frontend.report_agent_status("plan", None);
    let events = drain(&mut rx);
    assert_eq!(events.len(), 1);
    assert!(events[0].source.is_none());
    assert_eq!(events[0].payload.sse_event_type(), "program_status");
}

#[test]
fn reports_interleave_with_agent_output_in_publication_order() {
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let mut frontend = frontend(&bus);
    bus.sender()
        .emit(EventPayload::StdoutLine("thinking".into()));
    frontend.report_agent_status("plan", Some(&status(ProgramState::Done)));
    let events = drain(&mut rx);
    let types: Vec<_> = events.iter().map(|e| e.payload.sse_event_type()).collect();
    assert_eq!(
        types,
        [
            EventPayload::StdoutLine(String::new()).sse_event_type(),
            "program_status"
        ]
    );
    assert!(events[0].sequence < events[1].sequence);
}

/// Review finding: a single-agent command (`exec prompt`) launches through
/// `container_frontend_for_pty`; its OSC 7501 reports must reach the event
/// stream through `set_stuck_sender`, deduplicated, and the forwarder must
/// end with the detector channel.
#[tokio::test]
async fn single_agent_reports_reach_the_event_stream_through_set_stuck_sender() {
    use crate::command::commands::agent_setup::AgentLaunchFrontend;
    use crate::engine::agent_runtime::StuckEvent;
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let mut frontend = ApiDispatchFrontend::new("exec prompt", &["hi".to_string()], bus.sender());
    let mut sink = HasAgentFrontend::container_frontend_for_pty(&mut frontend);
    sink.report_status(AgentStatus::Running {
        container_name: "awman-exec-1".into(),
    });
    let (tx, _keep) = tokio::sync::broadcast::channel(16);
    let tx = std::sync::Arc::new(tx);
    frontend.set_stuck_sender(tx.clone());
    let blocked = EffectiveStatus {
        msg: Some("approve?".into()),
        ..status(ProgramState::Blocked)
    };
    tx.send(StuckEvent::Reported(Some(blocked.clone())))
        .unwrap();
    tx.send(StuckEvent::Reported(Some(blocked))).unwrap();
    tx.send(StuckEvent::Stuck).unwrap();
    tx.send(StuckEvent::Reported(Some(status(ProgramState::Working))))
        .unwrap();

    let mut statuses = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while statuses.len() < 2 {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("program_status never arrived")
            .unwrap();
        if event.payload.sse_event_type() == "program_status" {
            statuses.push(event);
        }
    }
    let json = serde_json::to_value(&statuses[0]).unwrap();
    assert_eq!(json["payload"]["data"]["state"], "blocked");
    assert_eq!(json["payload"]["data"]["msg"], "approve?");
    assert_eq!(
        statuses[0]
            .source
            .as_ref()
            .and_then(|s| s.container.as_deref()),
        Some("awman-exec-1"),
        "attributed to the launched container"
    );
    let json = serde_json::to_value(&statuses[1]).unwrap();
    assert_eq!(
        json["payload"]["data"]["state"], "working",
        "repeat was deduplicated"
    );

    // The forwarder does not keep the detector channel alive.
    drop(_keep);
    let weak = std::sync::Arc::downgrade(&tx);
    drop(tx);
    assert!(weak.upgrade().is_none());
}

/// Review finding: the interactive decision must reach the container
/// frontend `exec prompt` actually launches with, while helper containers
/// stay piped.
#[tokio::test]
async fn interactive_exec_prompt_launch_sink_gets_the_pty_size() {
    use crate::command::interaction::InteractionDecision;
    use crate::data::config::daemon::Interaction;
    let bus = EventBus::new(16);
    let mut frontend = ApiDispatchFrontend::new(
        "exec prompt",
        &["--interactive".to_string(), "hi".to_string()],
        bus.sender(),
    )
    .with_interaction(InteractionDecision {
        interaction: Interaction::Interactive,
        pty_size: Some((160, 48)),
    });
    let io = HasAgentFrontend::container_frontend_for_pty(&mut frontend).take_io();
    assert_eq!(io.initial_size, Some((160, 48)));
    assert!(io.resize.is_some());
    let helper = HasAgentFrontend::container_frontend(&mut frontend).take_io();
    assert_eq!(helper.initial_size, None, "helpers stay piped");

    let mut headless = ApiDispatchFrontend::new("exec prompt", &["hi".to_string()], bus.sender())
        .with_interaction(InteractionDecision {
            interaction: Interaction::Headless,
            pty_size: None,
        });
    let io = HasAgentFrontend::container_frontend_for_pty(&mut headless).take_io();
    assert_eq!(io.initial_size, None);
}
