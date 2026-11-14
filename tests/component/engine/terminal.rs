//! A completed terminal must deliver already-queued output before its exit.
use awman::data::{
    daemon_state::FeatureName,
    ids::TabId,
    terminal::{TerminalFrame, TerminalOwner, TerminalOwnerInfo},
};
use awman::engine::terminal_hub::{TerminalHub, TerminalHubConfig, TerminalRegistration};
use std::sync::Arc;

#[tokio::test]
async fn pending_output_is_flushed_before_terminal_exit() {
    let sandbox =
        crate::support::sandbox("engine::terminal::pending_output_is_flushed_before_terminal_exit");
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let registration = TerminalRegistration {
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
