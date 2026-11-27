//! WI 0105: bounded terminal history through the real terminal transport.
//! The production daemon router serves `GET /v1/terminals/{id}` on a private
//! Unix socket; the viewer is the production `TerminalStream` client. Output
//! comes from an owned producer registered on the daemon's own hub.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use awman::command::terminals::{ReplayEffect, ReplayFeed, Unfiltered, EARLIER_OUTPUT_MARKER};
use awman::data::{
    daemon_state::{CommandListener, FeatureName, Principal, RequestOrigin},
    ids::{CommandId, TabId},
    terminal::{
        TerminalClientFrame, TerminalFrame, TerminalOwner, TerminalOwnerInfo, ViewerIdentity,
    },
};
use awman::engine::remote::terminal_client::{TerminalClientOptions, TerminalStream};
use awman::engine::terminal_hub::{
    input_channel, SubscribeOptions, TerminalHandle, TerminalInput, TerminalRegistration,
};
use awman::frontend::api::routes::{build_router_for_listener, ListenerPolicy};
use awman_test_support::TestSandbox;

use crate::daemon::support::Api;

const DEADLINE: Duration = Duration::from_secs(10);

struct Daemon {
    api: Api,
    endpoint: PathBuf,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// hygiene-allow(H4): the production client needs a Unix socket; it lives in a private directory under the sandbox sock_dir, and `shutdown` cancels and joins the server task and removes the socket and directory.
    async fn start(sandbox: &TestSandbox) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let api = Api::start(sandbox).await;
        // The client refuses a socket whose directory is not private (0700).
        let dir = sandbox.sock_dir().join("t");
        awman::data::fs::private_files::create_private_dir(&dir).unwrap();
        let endpoint = dir.join("terminals.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let router = build_router_for_listener(api.api.clone(), ListenerPolicy::Local)
            .layer(axum::Extension(
                awman::frontend::api::local::OwnerAuthenticated,
            ))
            .layer(axum::Extension(RequestOrigin::OwnerSocket));
        let stop = tokio_util::sync::CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            api,
            endpoint,
            stop,
            task,
        }
    }

    fn register(&self, owner: TerminalOwner) -> TerminalHandle {
        let (stdin, input) = input_channel();
        // The producer never reads input in these tests; keep it open.
        std::mem::forget(input);
        self.api.api.terminals.hub().register(
            TerminalRegistration {
                automation: None,
                owner: TerminalOwnerInfo {
                    owner,
                    feature: FeatureName::Engine,
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
        )
    }

    async fn connect(&self, handle: &TerminalHandle, history: bool) -> TerminalStream {
        let options = TerminalClientOptions {
            history,
            read_only: true,
            ..Default::default()
        };
        tokio::time::timeout(
            DEADLINE,
            TerminalStream::connect_local(&self.endpoint, handle.id(), &options),
        )
        .await
        .expect("connect deadline")
        .unwrap()
    }

    async fn shutdown(self) {
        self.stop.cancel();
        self.task.await.unwrap();
        let _ = std::fs::remove_file(&self.endpoint);
        if let Some(dir) = self.endpoint.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        self.api.shutdown().await;
    }
}

async fn next(stream: &mut TerminalStream) -> TerminalFrame {
    tokio::time::timeout(DEADLINE, stream.next())
        .await
        .expect("frame deadline")
        .unwrap()
        .expect("stream open")
}

/// Frames up to and including the first snapshot.
async fn until_snapshot(stream: &mut TerminalStream) -> Vec<TerminalFrame> {
    let mut frames = Vec::new();
    loop {
        let frame = next(stream).await;
        let done = matches!(frame, TerminalFrame::Snapshot { .. });
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

fn history_of(frames: &[TerminalFrame]) -> (Vec<u8>, bool, usize) {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut count = 0;
    for frame in frames {
        if let TerminalFrame::History {
            bytes: chunk,
            truncated: t,
        } = frame
        {
            assert!(
                !*t || count == 0,
                "only the first history frame is truncated"
            );
            truncated |= t;
            count += 1;
            bytes.extend_from_slice(chunk);
        }
    }
    (bytes, truncated, count)
}

fn lines(range: std::ops::Range<u32>) -> String {
    range.map(|n| format!("line {n:04}\r\n")).collect()
}

#[tokio::test]
async fn output_emitted_before_attach_is_replayed_then_live_output_follows() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::output_emitted_before_attach_is_replayed_then_live_output_follows",
    );
    let daemon = Daemon::start(&sandbox).await;
    let handle = daemon.register(TerminalOwner::Tab(TabId::new()));
    handle.push_output(lines(0..50).as_bytes());

    let mut stream = daemon.connect(&handle, true).await;
    let frames = until_snapshot(&mut stream).await;
    assert!(matches!(frames[0], TerminalFrame::History { .. }));
    assert!(matches!(
        frames.last(),
        Some(TerminalFrame::Snapshot {
            has_output: true,
            ..
        })
    ));
    let (bytes, truncated, _) = history_of(&frames);
    assert!(!truncated);
    assert_eq!(String::from_utf8(bytes).unwrap(), lines(0..50));
    handle.push_output(b"live\r\n");
    assert_eq!(
        next(&mut stream).await,
        TerminalFrame::Output {
            bytes: b"live\r\n".to_vec()
        }
    );
    stream.close().await.unwrap();

    // Without `history` the stream is exactly what it was before WI 0105.
    let mut plain = daemon.connect(&handle, false).await;
    assert!(matches!(
        next(&mut plain).await,
        TerminalFrame::Snapshot { .. }
    ));
    plain.close().await.unwrap();

    handle.exit(0);
    daemon.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn eviction_bounds_history_and_the_viewer_marks_it() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::eviction_bounds_history_and_the_viewer_marks_it",
    );
    let daemon = Daemon::start(&sandbox).await;
    let ring = daemon.api.api.terminals.hub().config().ring_bytes;
    let handle = daemon.register(TerminalOwner::Tab(TabId::new()));
    // Well over the retained limit, in many chunks.
    let total: u32 = (ring as u32 / 11) * 2;
    for start in (0..total).step_by(100) {
        handle.push_output(lines(start..(start + 100).min(total)).as_bytes());
    }
    let mut stream = daemon.connect(&handle, true).await;
    let frames = until_snapshot(&mut stream).await;
    let (bytes, truncated, count) = history_of(&frames);
    assert!(truncated);
    assert!(bytes.len() <= ring, "{} > {ring}", bytes.len());
    assert!(count <= ring.div_ceil(16 * 1024));
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("line "), "starts on a line boundary");
    assert!(text.ends_with(&format!("line {:04}\r\n", total - 1)));
    assert!(!text.contains("line 0000\r\n"), "oldest output was evicted");

    // The viewer writes the marker once, before the first history bytes.
    let mut feed = ReplayFeed::new();
    let mut out = Vec::new();
    for frame in &frames {
        feed.apply(frame, &mut Unfiltered, &mut out);
    }
    assert!(out.starts_with(EARLIER_OUTPUT_MARKER));
    assert_eq!(
        out.windows(EARLIER_OUTPUT_MARKER.len())
            .filter(|w| *w == EARLIER_OUTPUT_MARKER)
            .count(),
        1
    );
    stream.close().await.unwrap();
    handle.exit(0);
    daemon.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn reconnect_without_history_does_not_replay_and_a_fresh_viewer_does() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::reconnect_without_history_does_not_replay_and_a_fresh_viewer_does",
    );
    let daemon = Daemon::start(&sandbox).await;
    let handle = daemon.register(TerminalOwner::Tab(TabId::new()));
    handle.push_output(lines(0..5).as_bytes());

    let mut first = daemon.connect(&handle, true).await;
    let (bytes, _, _) = history_of(&until_snapshot(&mut first).await);
    assert_eq!(String::from_utf8(bytes).unwrap(), lines(0..5));
    first.close().await.unwrap();

    handle.push_output(lines(5..8).as_bytes());
    // A viewer that kept its emulator reconnects without history.
    let mut kept = daemon.connect(&handle, false).await;
    let frames = until_snapshot(&mut kept).await;
    assert_eq!(history_of(&frames).2, 0);
    assert_eq!(frames.len(), 1);
    kept.close().await.unwrap();

    // A fresh viewer gets everything retained, including the output emitted
    // while nobody was attached.
    let mut fresh = daemon.connect(&handle, true).await;
    let (bytes, _, _) = history_of(&until_snapshot(&mut fresh).await);
    assert_eq!(String::from_utf8(bytes).unwrap(), lines(0..8));
    fresh.close().await.unwrap();

    handle.exit(0);
    daemon.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn a_slow_viewer_recovers_with_a_snapshot_and_no_history() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::a_slow_viewer_recovers_with_a_snapshot_and_no_history",
    );
    let daemon = Daemon::start(&sandbox).await;
    let handle = daemon.register(TerminalOwner::Tab(TabId::new()));
    handle.push_output(b"before\r\n");
    let mut stream = daemon.connect(&handle, true).await;
    let (bytes, _, _) = history_of(&until_snapshot(&mut stream).await);
    assert_eq!(bytes, b"before\r\n");
    // Stop reading while the producer floods far past the socket buffers and
    // the hub's per-viewer frame capacity.
    let chunk = vec![b'x'; 8 * 1024];
    for _ in 0..2048 {
        handle.push_output(&chunk);
    }
    let mut feed = ReplayFeed::new();
    let mut lags = 0;
    let mut out = Vec::new();
    let mut after_lag = Vec::new();
    loop {
        let frame = next(&mut stream).await;
        if lags > 0 {
            after_lag.push(frame.clone());
        }
        if feed.apply(&frame, &mut Unfiltered, &mut out) == ReplayEffect::FirstLag {
            lags += 1;
        }
        if matches!(frame, TerminalFrame::Snapshot { .. }) && lags > 0 {
            break;
        }
    }
    assert_eq!(lags, 1, "the viewer reports the first lag once");
    assert!(matches!(
        after_lag.last(),
        Some(TerminalFrame::Snapshot { .. })
    ));
    assert!(!after_lag
        .iter()
        .any(|f| matches!(f, TerminalFrame::History { .. })));
    stream
        .send(TerminalClientFrame::Detached {
            reason: awman::data::terminal::DetachReason::User,
        })
        .await
        .unwrap();
    drop(stream);
    handle.exit(0);
    daemon.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn finished_terminal_replays_history_before_its_exit() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::finished_terminal_replays_history_before_its_exit",
    );
    let daemon = Daemon::start(&sandbox).await;
    let handle = daemon.register(TerminalOwner::Tab(TabId::new()));
    handle.push_output(b"last words\r\n");
    handle.exit(4);
    let mut stream = daemon.connect(&handle, true).await;
    let frames = until_snapshot(&mut stream).await;
    assert_eq!(history_of(&frames).0, b"last words\r\n");
    assert_eq!(next(&mut stream).await, TerminalFrame::Exit { code: 4 });
    daemon.shutdown().await;
    sandbox.finish();
}

fn network_viewer() -> ViewerIdentity {
    ViewerIdentity {
        origin: RequestOrigin::Network,
        principal: Principal::Owner,
        listener_name: Some("component".into()),
        listener: CommandListener::Network,
        actor: None,
        annotations: Default::default(),
        read_only: true,
    }
}

#[tokio::test]
async fn network_viewers_get_history_only_where_output_is_admitted() {
    let sandbox = crate::support::sandbox(
        "engine::terminal::history::network_viewers_get_history_only_where_output_is_admitted",
    );
    let daemon = Daemon::start(&sandbox).await;
    let ops: &Arc<_> = &daemon.api.api.terminals;
    let history = SubscribeOptions { history: true };
    // The component listener serves `engine` (tab terminals) only.
    let served = daemon.register(TerminalOwner::Tab(TabId::new()));
    let unserved = daemon.register(TerminalOwner::Command(CommandId::new()));
    served.push_output(b"visible\r\n");
    unserved.push_output(b"secret\r\n");
    assert!(ops
        .open_with(unserved.id(), network_viewer(), history)
        .is_err());

    let mut session = ops
        .open_with(served.id(), network_viewer(), history)
        .unwrap();
    assert_eq!(
        session.next_frame().await,
        Some(TerminalFrame::History {
            bytes: b"visible\r\n".to_vec(),
            truncated: false
        })
    );
    assert!(matches!(
        session.next_frame().await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    session.detach(awman::data::terminal::DetachReason::User);

    // A live policy change ends a session before any queued history frame
    // is served, exactly as for output.
    served.push_output(b"more\r\n");
    let mut session = ops
        .open_with(served.id(), network_viewer(), history)
        .unwrap();
    let env = crate::support::env_snapshot(&sandbox);
    let path = awman::data::config::file::global_config_path(&env).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["daemon"]["network"]["listeners"]["component"]["features"] = serde_json::json!(["squad"]);
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    daemon.api.api.runtime.reload_config().await.unwrap();
    assert_eq!(session.next_frame().await, None);
    assert!(session.access_revoked());

    served.exit(0);
    unserved.exit(0);
    daemon.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn an_execution_rebind_restarts_history() {
    let _sandbox =
        crate::support::sandbox("engine::terminal::history::an_execution_rebind_restarts_history");
    let hub = Arc::new(awman::engine::terminal_hub::TerminalHub::new(
        Default::default(),
    ));
    let snapshot = super::automation_snapshot();
    let (stdin, _input) = input_channel();
    let old = hub.register(
        super::automation_registration(snapshot.clone()),
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    old.push_output(b"first execution\r\n");
    hub.seal_input(old.id()).unwrap();
    let mut replacement = snapshot.clone();
    replacement.execution_id = awman::data::ids::ExecutionId::new();
    replacement.revision = 0;
    let (stdin, _new_input) = input_channel();
    let new = hub
        .rebind_execution(
            old.id(),
            TerminalInput {
                stdin,
                resize: None,
            },
            replacement,
            uuid::Uuid::new_v4(),
            1,
        )
        .unwrap();
    new.push_output(b"second execution\r\n");
    let mut sub = hub
        .subscribe_with(new.id(), SubscribeOptions { history: true })
        .unwrap();
    assert_eq!(
        sub.next().await,
        Some(TerminalFrame::History {
            bytes: b"second execution\r\n".to_vec(),
            truncated: false
        })
    );
    assert!(matches!(
        sub.next().await,
        Some(TerminalFrame::Snapshot { .. })
    ));
    hub.finish_automation(new.id(), 0).unwrap();
}
