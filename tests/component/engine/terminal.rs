//! A completed terminal must deliver already-queued output before its exit.
use awman::data::{
    daemon_state::FeatureName,
    ids::TabId,
    terminal::{TerminalFrame, TerminalOwner, TerminalOwnerInfo},
};
use awman::engine::terminal_hub::{TerminalHub, TerminalHubConfig, TerminalRegistration};
use std::sync::Arc;

#[cfg(unix)]
mod history;

#[tokio::test]
async fn pending_output_is_flushed_before_terminal_exit() {
    let sandbox =
        crate::support::sandbox("engine::terminal::pending_output_is_flushed_before_terminal_exit");
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let registration = TerminalRegistration {
        automation: None,
        owner: TerminalOwnerInfo {
            owner: TerminalOwner::Tab(TabId::new()),
            feature: FeatureName::Engine,
            source: None,
            attempt: 1,
            allow_remote_input: false,
        },
        agent: "fixture".into(),
        model: None,
        cols: 80,
        rows: 24,
    };
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _resize_rx) = tokio::sync::mpsc::unbounded_channel();
    let bridged = hub.bridge_agent_channels(registration, stdout_rx, stdin_tx, resize_tx);
    let mut sub = hub.subscribe(bridged.handle.id()).unwrap();
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    // No await between send and exit: the bridge task cannot drain this queue first.
    stdout_tx.send(b"last line\r\n".to_vec()).unwrap();
    bridged.flush_pending_output();
    bridged.handle.exit(0);
    assert!(
        matches!(sub.next().await, Some(TerminalFrame::Output { bytes }) if bytes == b"last line\r\n")
    );
    assert!(matches!(sub.next().await, Some(TerminalFrame::Exit { .. })));
    drop(stdout_tx);
    bridged.task.await.unwrap();
    sandbox.finish();
}

fn automation_snapshot() -> awman::data::automation_records::AutomationSnapshot {
    let fx: serde_json::Value = serde_json::from_str(include_str!(
        "../../../web/test/lib/fixtures/terminal-frames.json"
    ))
    .unwrap();
    serde_json::from_value(fx["automation_examples"]["snapshot"].clone()).unwrap()
}
fn automation_registration(
    snapshot: awman::data::automation_records::AutomationSnapshot,
) -> TerminalRegistration {
    TerminalRegistration {
        automation: Some(snapshot),
        owner: TerminalOwnerInfo {
            owner: TerminalOwner::Tab(TabId::new()),
            feature: FeatureName::Engine,
            source: None,
            attempt: 1,
            allow_remote_input: false,
        },
        agent: "fixture".into(),
        model: None,
        cols: 80,
        rows: 24,
    }
}
#[tokio::test]
async fn automation_eof_drains_and_rebind_fences_old_handles_and_grants() {
    let _sandbox = crate::support::sandbox(
        "engine::terminal::automation_eof_drains_and_rebind_fences_old_handles_and_grants",
    );
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let snapshot = automation_snapshot();
    let (stdin, mut input) = awman::engine::terminal_hub::input_channel();
    let mut registration = automation_registration(snapshot.clone());
    let mut source = awman::data::execution_event::EventSource::phase(
        awman::data::execution_event::EventSourcePhase::Main,
    );
    source.execution_id = Some(snapshot.execution_id.clone());
    registration.owner.source = Some(source);
    let old = hub.register(
        registration,
        awman::engine::terminal_hub::TerminalInput {
            stdin,
            resize: None,
        },
    );
    let mut lifecycle = hub.events();
    let mut sub = hub.subscribe(old.id()).unwrap();
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::AutomationState { .. })
    ));
    let grant = hub.take_control(old.id()).unwrap();
    let epoch = grant.epoch();
    assert!(grant.is_valid());
    grant.send_input(b"human".to_vec()).unwrap();
    assert_eq!(input.recv().await.unwrap(), b"human");
    old.exit(19);
    assert!(!grant.is_valid());
    assert!(grant.send_input(b"stale".to_vec()).is_err());
    assert_eq!(
        hub.get(old.id()).unwrap().state,
        awman::data::terminal::TerminalState::Open
    );
    old.push_output(b"draining");
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Output { .. })
    ));
    let mut replacement = snapshot.clone();
    replacement.execution_id = awman::data::ids::ExecutionId::new();
    replacement.state = awman::data::automation::AutomationState::Starting;
    replacement.revision = 0;
    let (stdin, mut new_input) = awman::engine::terminal_hub::input_channel();
    let new = hub
        .rebind_execution(
            old.id(),
            awman::engine::terminal_hub::TerminalInput {
                stdin,
                resize: None,
            },
            replacement.clone(),
            uuid::Uuid::new_v4(),
            2,
        )
        .unwrap();
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::ExecutionReplaced { .. })
    ));
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Snapshot {
            has_output: false,
            ..
        })
    ));
    assert!(
        matches!(sub.next().await,Some(TerminalFrame::AutomationState{snapshot:s}) if *s==replacement)
    );
    old.push_output(b"stale output");
    old.resize(1, 1);
    old.exit(0);
    assert!(!hub.snapshot(new.id()).unwrap().has_output);
    assert_eq!(hub.get(new.id()).unwrap().cols, 80);
    let next_grant = hub.take_control(new.id()).unwrap();
    assert_ne!(epoch, next_grant.epoch());
    drop(grant);
    assert!(next_grant.is_valid());
    next_grant.send_input(b"new input".to_vec()).unwrap();
    assert_eq!(new_input.recv().await.unwrap(), b"new input");
    new.push_output(b"new output");
    assert!(matches!(sub.next().await,Some(TerminalFrame::Output{bytes}) if bytes==b"new output"));
    hub.finish_automation(new.id(), 19).unwrap();
    assert!(
        matches!(lifecycle.try_recv(), Ok(awman::engine::terminal_hub::TerminalHubEvent::Closed { source: Some(source), .. }) if source.execution_id == Some(replacement.execution_id))
    );
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Exit { code: 19 })
    ));
    assert!(sub.next().await.is_none());
}
#[tokio::test]
async fn lag_after_eviction_recovers_screen_state_close_and_one_exit() {
    let _sandbox = crate::support::sandbox(
        "engine::terminal::lag_after_eviction_recovers_screen_state_close_and_one_exit",
    );
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig {
        subscriber_capacity: 1,
        max_finished: 0,
        ..Default::default()
    }));
    let snapshot = automation_snapshot();
    let (stdin, _input) = awman::engine::terminal_hub::input_channel();
    let handle = hub.register(
        automation_registration(snapshot.clone()),
        awman::engine::terminal_hub::TerminalInput {
            stdin,
            resize: None,
        },
    );
    let mut sub = hub.subscribe(handle.id()).unwrap();
    sub.next().await;
    sub.next().await;
    let close = TerminalFrame::ExecutionClosed {
        request_id: uuid::Uuid::new_v4(),
        execution_id: snapshot.execution_id.clone(),
        result: Box::default(),
    };
    hub.publish_automation_frame(handle.id(), close.clone())
        .unwrap();
    handle.push_output(b"final screen");
    hub.finish_automation(handle.id(), 7).unwrap();
    assert!(matches!(sub.next().await, Some(TerminalFrame::Lagged)));
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Snapshot {
            has_output: true,
            ..
        })
    ));
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::AutomationState { .. })
    ));
    assert_eq!(sub.next().await, Some(close));
    assert_eq!(sub.next().await, Some(TerminalFrame::Exit { code: 7 }));
    assert!(sub.next().await.is_none());
}
#[tokio::test]
async fn bridged_automation_eof_does_not_synthesize_success() {
    let _sandbox = crate::support::sandbox(
        "engine::terminal::bridged_automation_eof_does_not_synthesize_success",
    );
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (out, rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin, _input) = tokio::sync::mpsc::unbounded_channel();
    let (resize, _resize) = tokio::sync::mpsc::unbounded_channel();
    let bridge = hub.bridge_agent_channels(
        automation_registration(automation_snapshot()),
        rx,
        stdin,
        resize,
    );
    drop(out);
    bridge.task.await.unwrap();
    assert_eq!(
        hub.get(bridge.handle.id()).unwrap().state,
        awman::data::terminal::TerminalState::Open
    );
    hub.finish_automation(bridge.handle.id(), 23).unwrap();
    assert_eq!(hub.get(bridge.handle.id()).unwrap().exit_code, Some(23));
}

struct NoOwnerLog;
impl awman::command::terminals::OwnerEventLog for NoOwnerLog {
    fn append(&self, _: &TerminalOwner, _: awman::data::execution_event::EventPayload) {}
}
struct NoOwnerResolver;
impl awman::command::terminals::OwnerResolver for NoOwnerResolver {
    fn active_run(&self, _: &awman::data::ids::TaskId) -> Option<awman::data::ids::RunId> {
        None
    }
}
struct ReplayController;
impl awman::command::terminals::TerminalAutomation for ReplayController {
    fn submit(
        &self,
        _: &awman::data::ids::TerminalId,
        _: &awman::data::terminal::ViewerIdentity,
        grant: Option<&awman::engine::terminal_hub::InputGrant>,
        request: awman::data::automation_records::SubmitTurnRequest,
    ) -> Result<
        awman::command::terminals::OperationReply,
        awman::command::terminals::AutomationRefusal,
    > {
        assert!(grant.is_none());
        Ok(awman::command::terminals::OperationReply::Replay(vec![
            TerminalFrame::OperationError {
                request_id: request.request_id,
                code: "cached".into(),
                error: "cached refusal".into(),
            },
        ]))
    }
    fn close(
        &self,
        _: &awman::data::ids::TerminalId,
        _: &awman::data::terminal::ViewerIdentity,
        _: Option<&awman::engine::terminal_hub::InputGrant>,
        _: awman::data::automation_records::CloseExecutionRequest,
    ) -> Result<
        awman::command::terminals::OperationReply,
        awman::command::terminals::AutomationRefusal,
    > {
        Ok(awman::command::terminals::OperationReply::PendingPublication)
    }
    fn with_control_change(
        &self,
        _: &awman::data::ids::TerminalId,
        _: &awman::data::terminal::ViewerIdentity,
        action: &mut dyn FnMut() -> Result<(), awman::command::terminals::TerminalRefusal>,
    ) -> Result<(), awman::command::terminals::TerminalRefusal> {
        action()
    }
    fn with_manual_input(
        &self,
        _: &awman::data::ids::TerminalId,
        _: &awman::data::terminal::ViewerIdentity,
        _: &awman::engine::terminal_hub::InputGrant,
        _: &[u8],
        action: &mut dyn FnMut() -> Result<(), awman::command::terminals::TerminalRefusal>,
    ) -> Result<(), awman::command::terminals::TerminalRefusal> {
        action()
    }
}
#[tokio::test]
async fn absent_controller_refuses_without_bytes_and_replays_are_private_without_a_grant() {
    use awman::command::terminals::{AccessPolicy, TerminalOperations};
    use awman::data::{
        automation_records::SubmitTurnRequest,
        terminal::{TerminalClientFrame, ViewerIdentity},
    };
    let _sandbox=crate::support::sandbox("engine::terminal::absent_controller_refuses_without_bytes_and_replays_are_private_without_a_grant");
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (stdin, mut input) = awman::engine::terminal_hub::input_channel();
    let handle = hub.register(
        automation_registration(automation_snapshot()),
        awman::engine::terminal_hub::TerminalInput {
            stdin,
            resize: None,
        },
    );
    let ops = Arc::new(TerminalOperations::new(
        hub.clone(),
        Arc::new(AccessPolicy::default),
        Arc::new(NoOwnerLog),
        Arc::new(NoOwnerResolver),
    ));
    let mut caller = ops.open(handle.id(), ViewerIdentity::local()).unwrap();
    let mut other = ops.open(handle.id(), ViewerIdentity::local()).unwrap();
    caller.next_frame().await;
    caller.next_frame().await;
    other.next_frame().await;
    other.next_frame().await;
    let request = SubmitTurnRequest {
        request_id: uuid::Uuid::new_v4(),
        execution_id: automation_snapshot().execution_id,
        expected_revision: 7,
        prompt: "guarded café task".into(),
    };
    let response = caller
        .handle_client_frame(TerminalClientFrame::SubmitTurn {
            request: request.clone(),
        })
        .unwrap();
    assert!(
        matches!(&response.frames[..],[TerminalFrame::OperationError{request_id,code,..}] if *request_id==request.request_id && code=="native_capability_unavailable")
    );
    assert!(input.try_recv().is_err());
    assert!(ops.install_automation(Arc::new(ReplayController)).is_ok());
    assert!(ops.install_automation(Arc::new(ReplayController)).is_err());
    let response = caller
        .handle_client_frame(TerminalClientFrame::SubmitTurn { request })
        .unwrap();
    assert!(
        matches!(&response.frames[..],[TerminalFrame::OperationError{code,..}] if code=="cached")
    );
    handle.push_output(b"ordinary output");
    assert!(matches!(
        other.next_frame().await,
        Some(TerminalFrame::Output { .. })
    ));
    assert!(input.try_recv().is_err());
    caller
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    caller
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"manual".to_vec(),
        })
        .unwrap();
    assert_eq!(input.recv().await.unwrap(), b"manual");
    caller.detach(awman::data::terminal::DetachReason::User);
    hub.finish_automation(handle.id(), 0).unwrap();
}

#[tokio::test]
async fn sealing_bridge_input_keeps_the_output_drain_live() {
    let _sandbox = crate::support::sandbox(
        "engine::terminal::sealing_bridge_input_keeps_the_output_drain_live",
    );
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (out, rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin, _input) = tokio::sync::mpsc::unbounded_channel();
    let (resize, _resize) = tokio::sync::mpsc::unbounded_channel();
    let bridge = hub.bridge_agent_channels(
        automation_registration(automation_snapshot()),
        rx,
        stdin,
        resize,
    );
    let mut sub = hub.subscribe(bridge.handle.id()).unwrap();
    sub.next().await;
    sub.next().await;
    hub.seal_input(bridge.handle.id()).unwrap();
    out.send(b"output during drain".to_vec()).unwrap();
    assert!(
        matches!(sub.next().await,Some(TerminalFrame::Output{bytes}) if bytes==b"output during drain")
    );
    drop(out);
    bridge.task.await.unwrap();
    hub.finish_automation(bridge.handle.id(), 29).unwrap();
    assert_eq!(sub.next().await, Some(TerminalFrame::Exit { code: 29 }));
}
