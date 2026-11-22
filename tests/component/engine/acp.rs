//! `engine::acp` — bounded ACP transport with production budgets.
//!
//! The first case runs the real Docker launch path (`acp: true`, piped stdio,
//! the shared I/O bridge) against a fake `docker` that writes far more valid
//! ACP output than every queue together may hold, while nothing consumes the
//! session. The bridge must stop reading (the fake stays blocked in its
//! write), every queue must stay within its budget, and cancelling must stop
//! the reader and release the bridge at once.

use std::time::{Duration, Instant};

use awman::data::message::{UserMessage, UserMessageSink};
use awman::engine::acp::protocol::MAX_ACP_LINE_BYTES;
use awman::engine::acp::{
    AcpBudgets, AcpClient, AcpQueueStats, AcpSession, AcpTransport, ContentBlock, PermissionPolicy,
    QueueUsage, SessionUpdate,
};
use awman::engine::agent_runtime::frontend::{send_gated, AgentFrontend};
use awman::engine::container::options::{ContainerName, ImageRef, ResolvedContainerOptions};
use awman::engine::container::ContainerRuntime;

struct Quiet;
impl UserMessageSink for Quiet {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}

fn message_line(n: usize, text_len: usize) -> String {
    let text = format!("{n:06}:{}", "x".repeat(text_len));
    format!(
        "{}\n",
        serde_json::json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "s1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
        }})
    )
}

fn full(q: &QueueUsage, item_bytes: usize) -> bool {
    q.items == q.max_items || q.bytes + item_bytes > q.max_bytes
}

fn assert_within(stats: &AcpQueueStats, budgets: &AcpBudgets) {
    for (name, q, items, bytes) in [
        (
            "stdout",
            stats.stdout,
            budgets.stdout_items,
            budgets.stdout_bytes,
        ),
        (
            "stdin",
            stats.stdin,
            budgets.stdin_items,
            budgets.stdin_bytes,
        ),
        (
            "stderr",
            stats.stderr,
            budgets.stderr_items,
            budgets.stderr_bytes,
        ),
        ("bulk", stats.bulk, budgets.bulk_items, budgets.bulk_bytes),
        (
            "stderr transport",
            stats.stderr_transport,
            budgets.stderr_items,
            budgets.stderr_bytes,
        ),
        (
            "decode-ahead",
            stats.decode_ahead,
            budgets.decode_ahead_items,
            // At most one bridge chunk beyond the budget.
            budgets.decode_ahead_bytes + 4096,
        ),
        (
            "control",
            stats.control,
            budgets.control_items,
            budgets.control_bytes,
        ),
    ] {
        assert!(
            q.high_items <= items && q.high_bytes <= bytes,
            "{name} exceeded its budget: {q:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_consumer_holds_the_docker_bridge_within_the_acp_budgets() {
    let mut s = crate::support::sandbox(
        "engine::acp::a_stalled_consumer_holds_the_docker_bridge_within_the_acp_budgets",
    );
    s.env_mut().set("AWMAN_TEST_DOCKER", "1");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    // 512 lines of ~64 KiB: about 32 MiB, twice the stdout and bulk budgets
    // combined, all valid protocol.
    let canned: String = (0..512).map(|n| message_line(n, 64 * 1024)).collect();
    docker.on(&["run"]).stdout(&canned);
    docker.on(&["stop"]);
    docker.on(&["rm"]);
    let runtime = ContainerRuntime::docker_in(crate::support::env_snapshot(&s));
    let instance = runtime
        .build(ResolvedContainerOptions {
            image: Some(ImageRef::new("fixture")),
            name: Some(ContainerName::new("awman-acp-bounds")),
            startup_grace: Some(Duration::MAX),
            acp: true,
            ..Default::default()
        })
        .unwrap();
    let (frontend, transport) = AcpTransport::channel();
    let execution = instance.run_with_frontend(Box::new(frontend)).unwrap();
    let mut acp =
        AcpSession::from_transport(execution, transport, Box::new(Quiet), PermissionPolicy::Ask);
    let handle = acp.cancel_handle();
    let budgets = AcpBudgets::default();

    // Nothing drives the session: the consumer is stalled.
    let stalled = awman_test_support::wait_for_async(
        "the bridge to fill the stdout and bulk budgets",
        Duration::from_secs(15),
        || {
            let stats = handle.stats();
            let at_budget = full(&stats.bulk, 70 * 1024) && full(&stats.stdout, 4096);
            std::future::ready(at_budget.then_some(stats))
        },
    )
    .await;
    let run = docker
        .calls()
        .into_iter()
        .find(|call| call.argv.first().map(String::as_str) == Some("run"))
        .expect("docker run was invoked");
    assert_eq!(
        run.exit, None,
        "the agent must be blocked writing, not drained into host memory"
    );
    let held = stalled.stdout.bytes + stalled.bulk.bytes;
    assert!(held < canned.len(), "{held} of {} bytes held", canned.len());
    assert_within(&stalled, &budgets);

    let started = Instant::now();
    handle.cancel();
    let stopped = awman_test_support::wait_for_async(
        "the reader to stop and release its queues",
        Duration::from_secs(5),
        || {
            let stats = handle.stats();
            let clean = stats.reader_stopped && stats.bulk.bytes == 0 && stats.stdout.bytes == 0;
            std::future::ready(clean.then_some(stats))
        },
    )
    .await;
    let cancel_latency = started.elapsed();
    // The bridge resumes and discards: the fake finishes its write.
    docker.wait_for_calls(1, Duration::from_secs(10));
    awman_test_support::wait_for_async(
        "the fake agent to finish once the bridge discards",
        Duration::from_secs(10),
        || {
            let done = docker.calls().iter().any(|call| {
                call.argv.first().map(String::as_str) == Some("run") && call.exit.is_some()
            });
            std::future::ready(done.then_some(()))
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), acp.shutdown())
        .await
        .expect("shutdown after cancellation")
        .unwrap();
    assert_within(&stopped, &budgets);
    eprintln!(
        "acp stress (docker bridge, production budgets): stdout high water {} B / {} items, \
         bulk high water {} B / {} items, held {held} of {} B, cancel latency {cancel_latency:?}",
        stopped.stdout.high_bytes,
        stopped.stdout.high_items,
        stopped.bulk.high_bytes,
        stopped.bulk.high_items,
        canned.len()
    );
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn maximum_size_valid_lines_are_delivered_whole_within_the_budgets() {
    let (mut frontend, transport) = AcpTransport::channel();
    let gates = frontend.acp_io_gates().unwrap();
    let io = frontend.take_io();
    gates.stdout.attach();
    let mut client = AcpClient::new(transport, Box::new(Quiet));
    let mut updates = client.take_updates();
    let handle = client.cancel_handle();

    // A line of exactly MAX_ACP_LINE_BYTES before its newline.
    let probe = message_line(0, 0);
    let overhead = probe.len() - 1 - "000000:".len();
    let text_len = MAX_ACP_LINE_BYTES - overhead - "000000:".len();
    let lines: Vec<String> = (0..2).map(|n| message_line(n, text_len)).collect();
    assert_eq!(lines[0].len(), MAX_ACP_LINE_BYTES + 1);
    let stdout = io.stdout.clone();
    let gate = gates.stdout.clone();
    let producer = tokio::task::spawn_blocking(move || {
        for line in lines {
            for chunk in line.as_bytes().chunks(4096) {
                assert!(send_gated(Some(&gate), &stdout, chunk.to_vec()));
            }
        }
    });
    for n in 0..2 {
        let update = tokio::time::timeout(Duration::from_secs(15), updates.recv())
            .await
            .expect("a maximum-size line must fit")
            .expect("transport open");
        let SessionUpdate::AgentMessageChunk { chunk } = update else {
            panic!("unexpected update");
        };
        let ContentBlock::Text { text } = chunk.content else {
            panic!("not text");
        };
        assert_eq!(text.len(), text_len + "000000:".len());
        assert!(text.starts_with(&format!("{n:06}:")));
    }
    tokio::time::timeout(Duration::from_secs(5), producer)
        .await
        .unwrap()
        .unwrap();
    let stats = handle.stats();
    assert_within(&stats, &AcpBudgets::default());
    assert_eq!(
        stats.bulk.high_items, 1,
        "a second max line waits for the first"
    );
    eprintln!(
        "acp max line: stdout high water {} B / {} items, bulk high water {} B",
        stats.stdout.high_bytes, stats.stdout.high_items, stats.bulk.high_bytes
    );
    drop(client);
    assert!(
        handle.is_cancelled(),
        "dropping the client releases the transport"
    );
    drop(io);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_docker_bridge_bounds_acp_stderr_at_ingress() {
    let mut s =
        crate::support::sandbox("engine::acp::the_docker_bridge_bounds_acp_stderr_at_ingress");
    s.env_mut().set("AWMAN_TEST_DOCKER", "1");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    // 4 MiB of diagnostics, four times the stderr budget.
    let noise = "diagnostic line from the agent\n".repeat(4 * 1024 * 1024 / 31);
    docker
        .on(&["run"])
        .stdout(&message_line(0, 16))
        .stderr(&noise);
    docker.on(&["stop"]);
    docker.on(&["rm"]);
    let runtime = ContainerRuntime::docker_in(crate::support::env_snapshot(&s));
    let instance = runtime
        .build(ResolvedContainerOptions {
            image: Some(ImageRef::new("fixture")),
            name: Some(ContainerName::new("awman-acp-stderr")),
            startup_grace: Some(Duration::MAX),
            acp: true,
            ..Default::default()
        })
        .unwrap();
    let (frontend, transport) = AcpTransport::channel();
    let execution = instance.run_with_frontend(Box::new(frontend)).unwrap();
    let mut acp =
        AcpSession::from_transport(execution, transport, Box::new(Quiet), PermissionPolicy::Ask);
    let handle = acp.cancel_handle();
    awman_test_support::wait_for_async(
        "the fake agent to finish writing stderr",
        Duration::from_secs(15),
        || {
            let done = docker.calls().iter().any(|call| {
                call.argv.first().map(String::as_str) == Some("run") && call.exit.is_some()
            });
            std::future::ready(done.then_some(()))
        },
    )
    .await;
    let stats = handle.stats();
    // The bridge charged stderr to the transport budget (it is wired), and
    // nothing in transit or in the ring ever exceeded it.
    assert!(
        stats.stderr_transport.high_items > 0,
        "the bridge must send ACP stderr through its gate: {:?}",
        stats.stderr_transport
    );
    assert_within(&stats, &AcpBudgets::default());
    let _ = tokio::time::timeout(Duration::from_secs(10), acp.shutdown())
        .await
        .expect("shutdown");
    s.finish();
}
