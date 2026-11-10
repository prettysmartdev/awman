//! Worker end to end (WI 0143), security checklist items against the mock
//! work server over HTTP and its `awman.terminal.v1` WebSocket:
//!
//! - F1: a worker cannot set server-assigned labels (the server refuses the
//!   registration; the server's value wins) nor configure `awman/` labels
//!   (refused before anything is sent).
//! - F3: remote terminal input is refused unless both sides allow it: the
//!   server refuses input for a lease without `remote_input`, and awman's
//!   relay refuses (and never delivers) input when its own side does not
//!   allow it, even though the lease does. Credentials (F2) are covered in
//!   `worker_lease_git.rs`.

#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use awman::data::daemon_state::FeatureName;
use awman::data::ids::LeaseId;
use awman::data::terminal::{TerminalOwner, TerminalOwnerInfo};
use awman::data::worker::labels::{HostFacts, WorkerHostKind};
use awman::engine::terminal_hub::{
    input_channel, TerminalHub, TerminalHubConfig, TerminalInput, TerminalRegistration,
};
use awman::engine::worker::terminal_relay::{TerminalRelay, INPUT_REFUSED};
use awman::engine::worker::{LeaseRef, WorkerEngineError, WspSession};
use awman_wsp::terminal::{LeaseTerminalFrame, TerminalFrame};
use awman_wsp::RegisterRequest;
use base64::Engine as _;
use tokio_util::sync::CancellationToken;
use wsp_lease::{claim, eventually, item, labels, Wsp};

#[tokio::test]
async fn worker_cannot_set_server_assigned_or_awman_labels() {
    let wsp = Wsp::start().await;
    // The owner approved the link with a server-assigned tier.
    let refresh = wsp.link(labels(&[("team.example/tier", "gold")])).await;
    let session = WspSession::connect("srv", wsp.base(), refresh).unwrap();
    let claim_tier = RegisterRequest {
        name: "w".into(),
        labels: labels(&[("team.example/tier", "platinum"), ("zone", "eu")]),
        capacity: 1,
        features: vec![],
    };
    match session.register(&claim_tier).await {
        Err(WorkerEngineError::Server { code, .. }) => assert_eq!(code, "server_label_refused"),
        other => panic!("a worker must not set a server-assigned label: {other:?}"),
    }
    // Without it, registration works and the server's value is the label.
    let ok = session
        .register(&RegisterRequest {
            labels: labels(&[("zone", "eu")]),
            ..claim_tier
        })
        .await
        .unwrap();
    assert_eq!(ok.server_labels, labels(&[("team.example/tier", "gold")]));

    // `awman/` labels are reported by awman, never configured.
    let facts = HostFacts::for_this_build(
        "docker",
        WorkerHostKind::Daemon,
        vec!["claude".into()],
        false,
        true,
    );
    let forged = labels(&[("awman/os", "windows")]);
    assert!(facts.worker_labels(&forged).is_err());
    assert!(
        RegisterRequest::with_labels("w".into(), facts.reported_labels(), forged, 1, vec![])
            .is_err()
    );
    // Known server-owned labels are refused, not silently overridden.
    assert!(facts
        .worker_labels(&labels(&[("zone", "eu")]))
        .unwrap()
        .with_server(labels(&[("zone", "us")]))
        .is_err());
    let merged = facts
        .worker_labels(&labels(&[("pool", "test")]))
        .unwrap()
        .with_server(labels(&[("zone", "us")]))
        .unwrap()
        .merged();
    assert_eq!(merged["zone"], "us");
    assert_eq!(merged["awman/os"], std::env::consts::OS);
}

fn input(terminal_id: &str, text: &str) -> LeaseTerminalFrame {
    LeaseTerminalFrame {
        terminal_id: terminal_id.into(),
        frame: TerminalFrame::Input {
            bytes: base64::engine::general_purpose::STANDARD.encode(text),
        },
    }
}

/// A hub terminal owned by a lease, with its stdin receiver.
fn agent_terminal(
    hub: &Arc<TerminalHub>,
    owner: &TerminalOwner,
) -> (
    awman::engine::terminal_hub::TerminalHandle,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (stdin, rx) = input_channel();
    let handle = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: owner.clone(),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: true,
            },
            agent: "claude".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    (handle, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_terminal_input_refused_unless_lease_and_worker_both_allow() {
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JNOINPUT", "https://example.invalid/r.git"));
    wsp.enqueue(item("01JLOCALOFF", "https://example.invalid/r.git"));
    wsp.mock.set_terminal("01JNOINPUT", true, false);
    wsp.mock.set_terminal("01JLOCALOFF", true, true);
    let worker = wsp
        .worker(
            "w",
            2,
            &[awman_wsp::features::TERMINAL],
            labels(&[]),
            BTreeMap::new(),
        )
        .await;

    // Server side: the lease grants no remote input, the server refuses it.
    let no_input = claim(&worker.session, Some("01JNOINPUT")).await.unwrap();
    assert!(no_input.terminal && !no_input.remote_input);
    let _socket = worker
        .session
        .open_terminal(&LeaseRef::of(&no_input))
        .await
        .unwrap();
    let refused = wsp
        .mock
        .send_terminal(&no_input.id, input("term_x", "ls\n"))
        .unwrap_err();
    assert_eq!(refused.code, "remote_input_refused");

    // Worker side: the lease allows it, but `daemon.terminal.remoteInput` is
    // off, so the relay runs with allow_input = lease && config = false.
    let lease = claim(&worker.session, Some("01JLOCALOFF")).await.unwrap();
    assert!(lease.terminal && lease.remote_input);
    let config_allows = false;
    let allow_input = lease.remote_input && config_allows;
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let owner = TerminalOwner::Lease(LeaseId::new());
    let (terminal, mut stdin) = agent_terminal(&hub, &owner);
    let tid = terminal.id().to_string();
    let mut socket = worker
        .session
        .open_terminal(&LeaseRef::of(&lease))
        .await
        .unwrap();
    let stop = CancellationToken::new();
    let relay = {
        let (hub, owner, stop) = (hub.clone(), owner.clone(), stop.clone());
        tokio::spawn(async move {
            TerminalRelay::new(hub, owner, allow_input)
                .run(&mut socket, stop)
                .await
        })
    };
    // The relay publishes the terminal (snapshot, tagged by terminal_id).
    let (mock, id) = (wsp.mock.clone(), lease.id.clone());
    let tid2 = tid.clone();
    eventually(
        Duration::from_secs(10),
        "terminal snapshot relayed",
        move || {
            mock.terminal_frames(&id)
                .iter()
                .any(|f| f.terminal_id == tid2 && matches!(f.frame, TerminalFrame::Snapshot { .. }))
        },
    )
    .await;
    wsp.mock
        .send_terminal(&lease.id, input(&tid, "rm -rf /\n"))
        .unwrap();
    let (mock, id, tid3) = (wsp.mock.clone(), lease.id.clone(), tid.clone());
    eventually(
        Duration::from_secs(10),
        "input_refused answered",
        move || {
            mock.terminal_frames(&id).iter().any(|f| {
            f.terminal_id == tid3
                && matches!(&f.frame, TerminalFrame::Error { code, .. } if code == INPUT_REFUSED)
        })
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        stdin.try_recv().is_err(),
        "refused input never reaches the agent"
    );
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), relay).await;
}
