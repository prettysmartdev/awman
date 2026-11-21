//! The daemon runner owns the reconnect loop for the same live lease epoch.
#![cfg(unix)]
use crate::daemon::lease_exec::{Host, Options};
use awman::data::{
    daemon_state::FeatureName,
    ids::{CommandId, LeaseId},
    terminal::{TerminalOwner, TerminalOwnerInfo},
    worker::wire::LeaseResource,
};
use awman::engine::terminal_hub::{
    input_channel, TerminalHandle, TerminalInput, TerminalRegistration,
};
use awman_test_support::wait_for_async;
use awman_wsp::{mock::MockWorkServer, terminal::TerminalFrame};
use serde_json::json;
use std::time::Duration;

async fn running(id: &'static str) -> (Host, LeaseResource, TerminalHandle) {
    let host = Host::start(
        id,
        Options {
            mock: MockWorkServer::new().with_lease_ttl(chrono::Duration::minutes(5)),
            extra: json!({"daemon":{"features":{"worker":{"pull":{"interaction":"interactive"}}}}}),
            ..Default::default()
        },
        |_| {},
        |docker| {
            docker
                .on(&["run"])
                .stdout("agent running\n")
                .hold_until_released();
        },
    )
    .await;
    let item = host.item("terminal");
    host.wsp.server().enqueue(item, chrono::Utc::now()).unwrap();
    host.wsp.server().set_terminal("terminal", true, false);
    let lease = awman::command::daemon_api::worker_pull::queue_claim(
        &host.api().api,
        "terminal",
        Some("mock"),
    )
    .await
    .unwrap();
    let job = wait_for_async("lease job", Duration::from_secs(10), || async {
        awman::command::daemon_api::worker_pull::leases_get(
            &host.api().api,
            &LeaseId::parse(&lease.id).unwrap(),
        )
        .ok()
        .and_then(|lease| lease.job_id)
    })
    .await;
    let (stdin, _input) = input_channel();
    let terminal = host.api().api.runtime.core().terminal_hub().register(
        TerminalRegistration {
            automation: None,
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(CommandId::parse(&job).unwrap()),
                feature: FeatureName::Worker,
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
    );
    wait_for_async("initial snapshot", Duration::from_secs(5), || async {
        host.wsp
            .server()
            .terminal_frames(&lease.server_lease_id)
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Snapshot { .. }))
            .then_some(())
    })
    .await;
    host.docker.wait_until_held(Duration::from_secs(10));
    (host, lease, terminal)
}

async fn reconnected(host: &Host, lease: &LeaseResource) {
    wait_for_async("replacement snapshot", Duration::from_secs(5), || async {
        let connections = host
            .wsp
            .server()
            .terminal_connection_frames(&lease.server_lease_id);
        connections
            .get(1)
            .filter(|frames| {
                frames
                    .iter()
                    .any(|frame| matches!(frame.frame, TerminalFrame::Snapshot { .. }))
            })
            .map(|_| ())
    })
    .await;
}

async fn finish(host: Host, lease: LeaseResource, terminal: TerminalHandle) {
    terminal.exit(0);
    host.docker.release();
    let report = host.terminal(&lease).await;
    assert!(report.release.is_none());
    assert_eq!(report.result.unwrap()["result"]["outcome"], json!("done"));
    // Hold observation past the first retry deadline: completed workers must
    // never open another terminal socket.
    let count = host
        .wsp
        .server()
        .terminal_open_attempts(&lease.server_lease_id);
    let until = std::time::Instant::now() + Duration::from_millis(1100);
    wait_for_async(
        "completed lease stays terminal",
        Duration::from_secs(3),
        || async { (std::time::Instant::now() >= until).then_some(()) },
    )
    .await;
    assert_eq!(
        host.wsp
            .server()
            .terminal_open_attempts(&lease.server_lease_id),
        count
    );
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_reconnects_with_snapshot_before_output_and_completes_done() {
    let (host, lease, terminal) = running("worker::terminal_reconnect::daemon_reconnects_with_snapshot_before_output_and_completes_done").await;
    host.wsp
        .server()
        .drop_terminal(&lease.server_lease_id)
        .unwrap();
    terminal.push_output(b"offline");
    reconnected(&host, &lease).await;
    terminal.push_output(b"online");
    wait_for_async("replacement output", Duration::from_secs(5), || async {
        host.wsp
            .server()
            .terminal_connection_frames(&lease.server_lease_id)[1]
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Output { .. }))
            .then_some(())
    })
    .await;
    let frames = host
        .wsp
        .server()
        .terminal_connection_frames(&lease.server_lease_id);
    assert!(matches!(frames[1][0].frame, TerminalFrame::Snapshot { .. }));
    finish(host, lease, terminal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_resends_exit_when_the_socket_drops_after_the_step_exits() {
    let (host, lease, terminal) = running(
        "worker::terminal_reconnect::daemon_resends_exit_when_the_socket_drops_after_the_step_exits",
    )
    .await;
    terminal.exit(0);
    wait_for_async("original exit", Duration::from_secs(5), || async {
        host.wsp
            .server()
            .terminal_frames(&lease.server_lease_id)
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Exit { .. }))
            .then_some(())
    })
    .await;
    host.wsp
        .server()
        .drop_terminal(&lease.server_lease_id)
        .unwrap();
    reconnected(&host, &lease).await;
    wait_for_async("replacement exit", Duration::from_secs(5), || async {
        host.wsp
            .server()
            .terminal_connection_frames(&lease.server_lease_id)[1]
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Exit { code: Some(0) }))
            .then_some(())
    })
    .await;
    finish(host, lease, terminal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_terminal_404_stops_retries_and_the_lease_completes_done() {
    let (host, lease, terminal) = running("worker::terminal_reconnect::daemon_terminal_404_stops_retries_and_the_lease_completes_done").await;
    host.wsp.server().refuse_terminal("terminal", "not_found");
    host.wsp
        .server()
        .drop_terminal(&lease.server_lease_id)
        .unwrap();
    wait_for_async("404 reconnect", Duration::from_secs(5), || async {
        (host
            .wsp
            .server()
            .terminal_open_attempts(&lease.server_lease_id)
            == 2)
            .then_some(())
    })
    .await;
    let until = std::time::Instant::now() + Duration::from_millis(1200);
    wait_for_async(
        "404 remains disabled while executing",
        Duration::from_secs(3),
        || async { (std::time::Instant::now() >= until).then_some(()) },
    )
    .await;
    assert_eq!(
        host.wsp
            .server()
            .terminal_open_attempts(&lease.server_lease_id),
        2
    );
    finish(host, lease, terminal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_terminal_409_fences_and_discards_the_result() {
    let (host, lease, terminal) =
        running("worker::terminal_reconnect::daemon_terminal_409_fences_and_discards_the_result")
            .await;
    host.wsp.server().refuse_terminal("terminal", "lease_lost");
    host.wsp
        .server()
        .drop_terminal(&lease.server_lease_id)
        .unwrap();
    wait_for_async("409 reconnect", Duration::from_secs(5), || async {
        (host
            .wsp
            .server()
            .terminal_open_attempts(&lease.server_lease_id)
            == 2)
            .then_some(())
    })
    .await;
    // The fake container's stop command records cancellation but cannot end
    // its separate held CLI process. Let that process unwind after fencing.
    host.docker.release();
    let state = wait_for_async("lease fenced", Duration::from_secs(8), || async {
        awman::command::daemon_api::worker_pull::leases_get(
            &host.api().api,
            &LeaseId::parse(&lease.id).unwrap(),
        )
        .ok()
        .filter(|lease| lease.state == awman::data::worker::LeaseState::Lost)
    })
    .await;
    assert_eq!(state.state, awman::data::worker::LeaseState::Lost);
    assert!(host.wsp.server().result_for("terminal").is_none());
    assert!(host
        .wsp
        .server()
        .release_for(&lease.server_lease_id)
        .is_none());
    assert_eq!(
        host.wsp
            .server()
            .terminal_open_attempts(&lease.server_lease_id),
        2
    );
    terminal.exit(0);
    host.docker.release();
    host.finish(|_, _| {}).await;
}
