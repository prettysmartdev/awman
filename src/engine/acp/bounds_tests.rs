//! Bounded ACP transport against stalled consumers.
//!
//! A scripted agent speaks JSON-RPC over the real transport and feeds stdout
//! exactly as the piped runtime bridge does (acquiring the stdout gate per
//! 4 KiB chunk; its stdin side releases what it reads). Small budgets keep
//! each case inside the unit deadline; production budgets are exercised by
//! the component tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::acp::budget::{AcpBudgets, AcpQueueStats, QueueUsage};
use crate::engine::acp::client::{AcpCancelHandle, AcpClient, AcpTransport};
use crate::engine::acp::frontend::AcpFrontend;
use crate::engine::acp::protocol::{PermissionDecision, PermissionRequest, SessionUpdate};
use crate::engine::acp::session::{AcpSession, PermissionPolicy};
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, CancelHandle, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::{AcpIoGates, AgentFrontend};
use crate::engine::error::EngineError;

const LIMIT: Duration = Duration::from_millis(900);

fn budgets() -> AcpBudgets {
    AcpBudgets {
        stdout_items: 8,
        stdout_bytes: 16 * 1024,
        stdin_items: 4,
        stdin_bytes: 16 * 1024,
        stderr_items: 4,
        stderr_bytes: 256,
        bulk_items: 8,
        bulk_bytes: 16 * 1024,
        control_items: 2,
        control_bytes: 16 * 1024,
        decode_ahead_items: 8,
        decode_ahead_bytes: 16 * 1024,
        pending_requests: 4,
    }
}

struct Quiet;
impl UserMessageSink for Quiet {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

struct Done;
impl ExecutionBackend for Done {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let now = chrono::DateTime::from_timestamp(0, 0).unwrap();
        Ok(AgentExitInfo {
            exit_code: 0,
            signal: None,
            started_at: now,
            ended_at: now,
        })
    }
    fn cancel(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::new(|| Ok(()))
    }
}

fn execution() -> AgentExecution {
    let (stuck, _) = tokio::sync::broadcast::channel(4);
    AgentExecution::new(
        crate::data::session::AgentHandle {
            id: "fake".into(),
            name: "fake".into(),
            image_tag: "fake:latest".into(),
            started_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        },
        Box::new(Done),
        Arc::new(stuck),
        None,
    )
}

/// The container side of the transport.
#[derive(Clone)]
struct Agent {
    stdout: mpsc::UnboundedSender<Vec<u8>>,
    stderr: mpsc::UnboundedSender<Vec<u8>>,
    gates: AcpIoGates,
    /// Honour the stdout gate like the piped bridge does.
    gated: bool,
    lines_sent: Arc<AtomicUsize>,
}

impl Agent {
    /// Write one line in bridge-sized chunks. `false` once the client is gone.
    async fn send(&self, message: Value) -> bool {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        for chunk in bytes.chunks(4096) {
            if self.gated && self.gates.stdout.acquire(chunk.len()).await.is_err() {
                return false;
            }
            if self.stdout.send(chunk.to_vec()).is_err() {
                return false;
            }
        }
        self.lines_sent.fetch_add(1, Ordering::SeqCst);
        true
    }
    async fn reply(&self, id: &Value, result: Value) -> bool {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }
    async fn chunk(&self, n: usize) -> bool {
        // ~1 KiB per line.
        let text = format!("{n:06}:{}", "x".repeat(1000));
        self.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "s1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
        }}))
        .await
    }
    async fn permission(&self, id: i64) -> bool {
        self.send(
            json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
                "sessionId": "s1",
                "toolCall": {"toolCallId": format!("t{id}"), "title": "edit"},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]
            }}),
        )
        .await
    }
}

/// A session wired to a scripted agent; `from_client` yields what the
/// client wrote to the agent's stdin.
fn harness(gated: bool) -> (AcpSession, Agent, mpsc::UnboundedReceiver<Value>) {
    harness_reading(gated, usize::MAX)
}

/// Like [`harness`], but the agent stops reading stdin after `reads` writes.
fn harness_reading(
    gated: bool,
    reads: usize,
) -> (AcpSession, Agent, mpsc::UnboundedReceiver<Value>) {
    let (transport, agent, from_client) = wire(gated, reads);
    let session = AcpSession::from_transport(
        execution(),
        transport,
        Box::new(Quiet),
        PermissionPolicy::Ask,
    );
    (session, agent, from_client)
}

/// A transport whose container side is a scripted agent.
fn wire(gated: bool, reads: usize) -> (AcpTransport, Agent, mpsc::UnboundedReceiver<Value>) {
    let (mut frontend, transport) = AcpTransport::with_budgets(budgets());
    let gates = frontend.acp_io_gates().unwrap();
    let io = frontend.take_io();
    if gated {
        gates.stdout.attach();
    }
    gates.stdin.attach();
    let (to_agent, from_client) = mpsc::unbounded_channel();
    let stdin_gate = gates.stdin.clone();
    let mut stdin = io.stdin_rx;
    tokio::spawn(async move {
        let mut buf = Vec::new();
        for _ in 0..reads {
            let Some(bytes) = stdin.recv().await else {
                return;
            };
            stdin_gate.release(bytes.len());
            buf.extend(bytes);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                    let _ = to_agent.send(message);
                }
            }
        }
        // Stop reading but keep the pipe open, like a wedged agent.
        std::future::pending::<()>().await;
        drop(stdin);
    });
    let agent = Agent {
        stdout: io.stdout,
        stderr: io.stderr,
        gates,
        gated,
        lines_sent: Arc::new(AtomicUsize::new(0)),
    };
    (transport, agent, from_client)
}

/// Answer `initialize` and `session/new`.
async fn handshake(agent: &Agent, from_client: &mut mpsc::UnboundedReceiver<Value>) {
    let init = from_client.recv().await.unwrap();
    agent
        .reply(&init["id"], json!({"protocolVersion": 1}))
        .await;
    let new = from_client.recv().await.unwrap();
    agent.reply(&new["id"], json!({"sessionId": "s1"})).await;
}

/// Next message from the client with this method (skipping others).
async fn expect(from_client: &mut mpsc::UnboundedReceiver<Value>, method: &str) -> Value {
    loop {
        let message = from_client.recv().await.expect("client closed stdin");
        if message["method"] == method {
            return message;
        }
    }
}

/// A frontend whose permission prompt and next-prompt calls block until the
/// test answers, like a person who has not responded yet.
struct Stalled {
    rendered: Arc<StdMutex<Vec<usize>>>,
    asked: Arc<AtomicUsize>,
    decisions: std_mpsc::Receiver<PermissionDecision>,
    prompts: std_mpsc::Receiver<Option<String>>,
}

impl AcpFrontend for Stalled {
    fn render_update(&mut self, update: SessionUpdate) {
        if let SessionUpdate::AgentMessageChunk { chunk } = update {
            if let crate::engine::acp::ContentBlock::Text { text } = chunk.content {
                self.rendered
                    .lock()
                    .unwrap()
                    .push(text[..6].parse().unwrap());
            }
        }
    }
    fn request_permission(&mut self, _request: PermissionRequest) -> PermissionDecision {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.decisions
            .recv()
            .unwrap_or(PermissionDecision::Cancelled)
    }
    fn next_prompt(&mut self) -> Option<String> {
        self.prompts.recv().unwrap_or(None)
    }
}

struct Controls {
    rendered: Arc<StdMutex<Vec<usize>>>,
    asked: Arc<AtomicUsize>,
    decide: std_mpsc::Sender<PermissionDecision>,
    prompt: std_mpsc::Sender<Option<String>>,
}

fn stalled() -> (Stalled, Controls) {
    let rendered = Arc::new(StdMutex::new(Vec::new()));
    let asked = Arc::new(AtomicUsize::new(0));
    let (decide, decisions) = std_mpsc::channel();
    let (prompt, prompts) = std_mpsc::channel();
    (
        Stalled {
            rendered: rendered.clone(),
            asked: asked.clone(),
            decisions,
            prompts,
        },
        Controls {
            rendered,
            asked,
            decide,
            prompt,
        },
    )
}

fn full(q: &QueueUsage, item_bytes: usize) -> bool {
    q.items == q.max_items || q.bytes + item_bytes > q.max_bytes
}

/// Wait until bulk updates and the stdout transport are both at budget.
async fn saturated(handle: &AcpCancelHandle) -> AcpQueueStats {
    awman_test_support::wait_for_async("bulk and stdout queues at budget", LIMIT, || {
        let stats = handle.stats();
        let ready = full(&stats.bulk, 1100) && full(&stats.stdout, 4096);
        std::future::ready(ready.then_some(stats))
    })
    .await
}

fn assert_within_budgets(stats: &AcpQueueStats) {
    let b = budgets();
    for (name, q, items, bytes) in [
        ("stdout", stats.stdout, b.stdout_items, b.stdout_bytes),
        ("stdin", stats.stdin, b.stdin_items, b.stdin_bytes),
        ("stderr", stats.stderr, b.stderr_items, b.stderr_bytes),
        ("bulk", stats.bulk, b.bulk_items, b.bulk_bytes),
        ("control", stats.control, b.control_items, b.control_bytes),
    ] {
        assert!(
            q.high_items <= items && q.high_bytes <= bytes,
            "{name} exceeded its budget: {q:?}"
        );
    }
    assert!(stats.high_pending_requests <= b.pending_requests);
    // Decode-ahead may exceed its byte budget by at most one 4 KiB chunk.
    let ahead = stats.decode_ahead;
    assert!(
        ahead.high_items <= b.decode_ahead_items && ahead.high_bytes <= b.decode_ahead_bytes + 4096,
        "decode-ahead exceeded its budget: {ahead:?}"
    );
}

const FLOOD: usize = 200;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_permission_prompt_bounds_every_queue_and_loses_no_output() {
    let (mut acp, agent, mut from_client) = harness(true);
    let script = agent.clone();
    let agent_task = tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        let prompt = expect(&mut from_client, "session/prompt").await;
        script.permission(100).await;
        for n in 0..FLOOD {
            assert!(script.chunk(n).await, "client vanished mid-flood");
        }
        let answer = loop {
            let m = from_client.recv().await.unwrap();
            if m["id"] == 100 {
                break m;
            }
        };
        script
            .reply(&prompt["id"], json!({"stopReason": "end_turn"}))
            .await;
        answer
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    let (mut frontend, controls) = stalled();
    let driver = tokio::spawn(async move {
        let result = acp
            .drive_with_initial_prompt(&mut frontend, Some("go".into()))
            .await;
        (result, acp)
    });

    let at_budget = saturated(&handle).await;
    assert_eq!(controls.asked.load(Ordering::SeqCst), 1);
    assert!(
        agent.lines_sent.load(Ordering::SeqCst) < FLOOD,
        "the agent must be held back, not buffered"
    );
    assert_within_budgets(&at_budget);

    controls
        .decide
        .send(PermissionDecision::Selected {
            option_id: "allow".into(),
        })
        .unwrap();
    controls.prompt.send(None).unwrap();
    let answer = tokio::time::timeout(LIMIT, agent_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow");
    let (result, acp) = tokio::time::timeout(LIMIT, driver).await.unwrap().unwrap();
    result.unwrap();
    assert_eq!(
        *controls.rendered.lock().unwrap(),
        (0..FLOOD).collect::<Vec<_>>(),
        "every output chunk is rendered once, in order"
    );
    let stats = acp.queue_stats();
    assert_within_budgets(&stats);
    eprintln!(
        "stalled-permission high water: stdout {}B/{} items, bulk {}B/{} items, control {} items",
        stats.stdout.high_bytes,
        stats.stdout.high_items,
        stats.bulk.high_bytes,
        stats.bulk.high_items,
        stats.control.high_items
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_during_backpressure_stops_the_reader_and_frees_queued_data() {
    let (mut acp, agent, mut from_client) = harness(true);
    let script = agent.clone();
    let agent_task = tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        expect(&mut from_client, "session/prompt").await;
        script.permission(100).await;
        let mut n = 0;
        // Sustained output until the client goes away.
        while script.chunk(n).await {
            n += 1;
        }
        n
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    let (mut frontend, controls) = stalled();
    let driver = tokio::spawn(async move {
        acp.drive_with_initial_prompt(&mut frontend, Some("go".into()))
            .await
    });
    saturated(&handle).await;

    let started = Instant::now();
    handle.cancel();
    let stopped = awman_test_support::wait_for_async("reader cleanup", LIMIT, || {
        let stats = handle.stats();
        let clean = stats.reader_stopped
            && stats.bulk.items == 0
            && stats.bulk.bytes == 0
            && stats.control.items == 0
            && stats.stdout.bytes == 0;
        std::future::ready(clean.then_some(stats))
    })
    .await;
    let sent = tokio::time::timeout(LIMIT, agent_task)
        .await
        .expect("a producer blocked on a full transport is released by cancel")
        .unwrap();
    let latency = started.elapsed();
    assert_within_budgets(&stopped);
    // The frontend is still inside its synchronous prompt; once it returns
    // the driver ends at once instead of waiting on the agent.
    controls.decide.send(PermissionDecision::Cancelled).unwrap();
    let result = tokio::time::timeout(LIMIT, driver).await.unwrap().unwrap();
    assert!(result.is_ok(), "{result:?}");
    eprintln!("cancellation released the reader in {latency:?} after {sent} lines");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_behind_saturated_output_is_answered_once_a_blocked_prompt_returns() {
    let (mut acp, agent, mut from_client) = harness(true);
    let script = agent.clone();
    let agent_task = tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        let first = expect(&mut from_client, "session/prompt").await;
        script
            .reply(&first["id"], json!({"stopReason": "end_turn"}))
            .await;
        // Between turns: the driver is inside `next_prompt`.
        for n in 0..FLOOD {
            assert!(script.chunk(n).await);
        }
        script.permission(500).await;
        let (mut prompt, mut answer) = (None, None);
        while prompt.is_none() || answer.is_none() {
            let m = from_client.recv().await.unwrap();
            if m["method"] == "session/prompt" {
                prompt = Some(m);
            } else if m["id"] == 500 {
                answer = Some(m);
            }
        }
        script
            .reply(&prompt.unwrap()["id"], json!({"stopReason": "end_turn"}))
            .await;
        answer.unwrap()
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    let (mut frontend, controls) = stalled();
    let driver = tokio::spawn(async move {
        acp.drive_with_initial_prompt(&mut frontend, Some("first".into()))
            .await
    });
    // Either bounded queue can stop the producer here. Requiring both to be
    // full depends on which task the executor schedules first and can time
    // out even though the blocked-prompt/backpressure condition is present.
    let at_budget = awman_test_support::wait_for_async("output queue at budget", LIMIT, || {
        let stats = handle.stats();
        let ready = full(&stats.bulk, 1100) || full(&stats.stdout, 4096);
        std::future::ready(ready.then_some(stats))
    })
    .await;
    assert_eq!(controls.asked.load(Ordering::SeqCst), 0);
    assert!(
        agent.lines_sent.load(Ordering::SeqCst) < FLOOD,
        "the permission is behind a producer blocked by output backpressure"
    );
    assert_within_budgets(&at_budget);

    controls
        .decide
        .send(PermissionDecision::Selected {
            option_id: "allow".into(),
        })
        .unwrap();
    controls.prompt.send(Some("second".into())).unwrap();
    controls.prompt.send(None).unwrap();
    let answer = tokio::time::timeout(LIMIT, agent_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow");
    tokio::time::timeout(LIMIT, driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        *controls.rendered.lock().unwrap(),
        (0..FLOOD).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_flood_fails_the_session_explicitly() {
    let (mut acp, agent, mut from_client) = harness(true);
    let script = agent.clone();
    tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        expect(&mut from_client, "session/prompt").await;
        for id in 10..20 {
            if !script.permission(id).await {
                break;
            }
        }
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    let (mut frontend, controls) = stalled();
    let driver = tokio::spawn(async move {
        acp.drive_with_initial_prompt(&mut frontend, Some("go".into()))
            .await
    });
    awman_test_support::wait_for_async("control overflow", LIMIT, || {
        std::future::ready(handle.is_cancelled().then_some(()))
    })
    .await;
    // The driver may have ended before it reached the (stalled) frontend.
    let _ = controls.decide.send(PermissionDecision::Cancelled);
    let result = tokio::time::timeout(LIMIT, driver).await.unwrap().unwrap();
    assert!(
        matches!(result, Err(EngineError::AcpOverflow { queue: "control" })),
        "{result:?}"
    );
    assert!(handle.stats().control.high_items <= budgets().control_items);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ungated_runtime_stream_overflows_explicitly_instead_of_growing() {
    let (mut acp, agent, mut from_client) = harness(false);
    let script = agent.clone();
    tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        expect(&mut from_client, "session/prompt").await;
        script.permission(100).await;
        let mut n = 0;
        while n < 10 * FLOOD && script.chunk(n).await {
            n += 1;
        }
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    let (mut frontend, controls) = stalled();
    let driver = tokio::spawn(async move {
        acp.drive_with_initial_prompt(&mut frontend, Some("go".into()))
            .await
    });
    awman_test_support::wait_for_async("stdout overflow", LIMIT, || {
        std::future::ready(handle.is_cancelled().then_some(()))
    })
    .await;
    // The overflow may end the driver before it ever asks.
    let _ = controls.decide.send(PermissionDecision::Cancelled);
    let result = tokio::time::timeout(LIMIT, driver).await.unwrap().unwrap();
    assert!(
        matches!(
            result,
            Err(EngineError::AcpOverflow {
                queue: "transport_stdout"
            })
        ),
        "{result:?}"
    );
    assert_within_budgets(&handle.stats());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_answered_requests_hold_bounded_slots_that_drop_and_cancel_release() {
    let (frontend, transport) = AcpTransport::with_budgets(budgets());
    let mut frontend = frontend;
    let gates = frontend.acp_io_gates().unwrap();
    let io = frontend.take_io();
    // The agent reads nothing it is asked and never answers.
    let _keep = (io.stdout, io.stderr, io.stdin_rx, gates);
    let client = Arc::new(AcpClient::new(transport, Box::new(Quiet)));
    let handle = client.cancel_handle();
    let mut waiting = Vec::new();
    for _ in 0..budgets().pending_requests {
        let client = client.clone();
        waiting.push(tokio::spawn(async move {
            client.request("never/answered", json!({})).await
        }));
    }
    awman_test_support::wait_for_async("every pending slot in use", LIMIT, || {
        let n = handle.stats().pending_requests;
        std::future::ready((n == budgets().pending_requests).then_some(()))
    })
    .await;
    let refused = client.request("one/more", json!({})).await;
    assert!(
        matches!(
            refused,
            Err(EngineError::AcpOverflow {
                queue: "pending_requests"
            })
        ),
        "{refused:?}"
    );
    // A caller that stops waiting (abort, timeout, drop) frees its slot.
    waiting.pop().unwrap().abort();
    let one_free = || {
        let n = handle.stats().pending_requests;
        std::future::ready((n == budgets().pending_requests - 1).then_some(()))
    };
    awman_test_support::wait_for_async("an aborted request to free its slot", LIMIT, one_free)
        .await;
    let gave_up = tokio::time::timeout(
        Duration::from_millis(1),
        client.request("given/up", json!({})),
    )
    .await;
    assert!(gave_up.is_err(), "never answered: the caller times out");
    awman_test_support::wait_for_async("a timed-out request to free its slot", LIMIT, one_free)
        .await;
    handle.cancel();
    for task in waiting {
        let result = tokio::time::timeout(LIMIT, task).await.unwrap().unwrap();
        assert!(result.is_err());
    }
    assert_eq!(handle.stats().pending_requests, 0);
    assert!(handle.stats().high_pending_requests <= budgets().pending_requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_that_stops_reading_stdin_cannot_hold_up_teardown() {
    // The agent reads the two handshake requests and then never reads again.
    let (mut acp, agent, mut from_client) = harness_reading(true, 2);
    let script = agent.clone();
    tokio::spawn(async move { handshake(&script, &mut from_client).await });
    acp.initialize("/workspace", None).await.unwrap();
    // Cancel notifications fill the stdin budget; the next send waits.
    for _ in 0..budgets().stdin_items {
        acp.cancel().await.unwrap();
    }
    assert_eq!(acp.queue_stats().stdin.items, budgets().stdin_items);
    assert!(
        futures_util::FutureExt::now_or_never(acp.cancel()).is_none(),
        "a full stdin makes senders wait"
    );
    let started = Instant::now();
    tokio::time::timeout(LIMIT, acp.shutdown())
        .await
        .expect("shutdown must not wait for stdin space")
        .unwrap();
    let stats = acp.queue_stats();
    assert!(stats.reader_stopped || acp.cancel_handle().is_cancelled());
    assert!(stats.stdin.high_items <= budgets().stdin_items);
    eprintln!("teardown with a wedged stdin took {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_session_releases_a_producer_blocked_on_a_full_transport() {
    let (mut acp, agent, mut from_client) = harness(true);
    let script = agent.clone();
    let agent_task = tokio::spawn(async move {
        handshake(&script, &mut from_client).await;
        let mut n = 0;
        while script.chunk(n).await {
            n += 1;
        }
        n
    });
    acp.initialize("/workspace", None).await.unwrap();
    let handle = acp.cancel_handle();
    // Nobody drives the session: output backs up to the budgets.
    saturated(&handle).await;
    drop(acp);
    let sent = tokio::time::timeout(LIMIT, agent_task)
        .await
        .expect("the blocked producer is released when the consumer disappears")
        .unwrap();
    let stats = awman_test_support::wait_for_async("owned reader cleanup", LIMIT, || {
        let stats = handle.stats();
        let clean = stats.reader_stopped && stats.bulk.bytes == 0 && stats.stdout.bytes == 0;
        std::future::ready(clean.then_some(stats))
    })
    .await;
    assert_within_budgets(&stats);
    assert!(sent < FLOOD, "{sent} lines got out before the drop");
}

/// A client over a scripted agent, without a session driver.
fn client(gated: bool) -> (AcpClient, Agent, mpsc::UnboundedReceiver<Value>) {
    let (transport, agent, from_client) = wire(gated, usize::MAX);
    (
        AcpClient::new(transport, Box::new(Quiet)),
        agent,
        from_client,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_and_response_arrive_while_bulk_stays_saturated() {
    let (mut client, agent, mut from_client) = client(true);
    let _updates = client.take_updates(); // never drained
    let mut incoming = client.take_incoming();
    let handle = client.cancel_handle();
    let request = tokio::spawn({
        let client = Arc::new(client);
        let c = client.clone();
        async move {
            let result = c.request("session/load", json!({})).await;
            (result, client)
        }
    });
    let load = expect(&mut from_client, "session/load").await;
    // Fill bulk (8 items) and leave three more updates waiting behind it,
    // then ask for permission and answer the request. Nobody drains bulk.
    let script = agent.clone();
    tokio::spawn(async move {
        for n in 0..11 {
            script.chunk(n).await;
        }
        script.permission(7).await;
        script.reply(&load["id"], json!({"loaded": true})).await;
    });
    let permission = tokio::time::timeout(LIMIT, incoming.recv())
        .await
        .expect("a permission request must not wait behind saturated bulk output")
        .expect("connection open");
    assert!(matches!(
        permission,
        crate::engine::acp::IncomingRequest::Permission(_)
    ));
    let (result, client) = tokio::time::timeout(LIMIT, request)
        .await
        .expect("an RPC response must not wait behind saturated bulk output")
        .unwrap();
    assert_eq!(result.unwrap(), json!({"loaded": true}));
    let stats = handle.stats();
    assert_eq!(
        stats.bulk.items,
        budgets().bulk_items,
        "bulk stayed saturated"
    );
    assert_within_budgets(&stats);
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_responses_are_charged_to_the_control_budget() {
    use futures_util::FutureExt;
    let (client, agent, mut from_client) = client(true);
    let handle = client.cancel_handle();
    let control_items = |h: &AcpCancelHandle| h.stats().control.items;
    // Requests that reach their wait (sent), then are not polled again.
    let mut waiting: Vec<_> = (0..5)
        .map(|_| Box::pin(client.request("session/load", json!({}))))
        .collect();
    let mut ids = Vec::new();
    for request in waiting.iter_mut().take(4) {
        assert!(request.now_or_never().is_none());
        ids.push(expect(&mut from_client, "session/load").await["id"].clone());
    }
    // Two unread answers fill the control budget (2 items); the pending
    // slots they held are free, the bytes are not.
    for id in &ids[..2] {
        agent.reply(id, json!({"blob": "y".repeat(1000)})).await;
    }
    awman_test_support::wait_for_async("responses charged", LIMIT, || {
        std::future::ready((control_items(&handle) == 2).then_some(()))
    })
    .await;
    assert!(handle.stats().control.bytes > 2000);
    assert_eq!(handle.stats().pending_requests, 2);
    // Taking an answer returns its credit; so does abandoning the wait.
    let first = (&mut waiting[0]).await.unwrap();
    assert_eq!(first["blob"].as_str().unwrap().len(), 1000);
    assert_eq!(control_items(&handle), 1);
    drop(waiting.remove(1));
    assert_eq!(control_items(&handle), 0);
    // Two more unread answers fill it again …
    for id in &ids[2..4] {
        agent.reply(id, json!({"blob": "z"})).await;
    }
    awman_test_support::wait_for_async("responses charged again", LIMIT, || {
        std::future::ready((control_items(&handle) == 2).then_some(()))
    })
    .await;
    // … and one more is an explicit control overflow, not hidden memory.
    let last = waiting.last_mut().unwrap();
    assert!(last.now_or_never().is_none());
    let id = expect(&mut from_client, "session/load").await["id"].clone();
    agent.reply(&id, json!({"blob": "w"})).await;
    awman_test_support::wait_for_async("control overflow", LIMIT, || {
        std::future::ready(handle.is_cancelled().then_some(()))
    })
    .await;
    assert!(matches!(
        client.failure(),
        Some(EngineError::AcpOverflow { queue: "control" })
    ));
    let last = waiting.pop().unwrap().await;
    assert!(
        matches!(last, Err(EngineError::AcpOverflow { queue: "control" })),
        "{last:?}"
    );
    drop(waiting);
    assert_eq!(control_items(&handle), 0);
    assert!(handle.stats().control.high_items <= budgets().control_items);
}

#[tokio::test(flavor = "current_thread")]
async fn stderr_is_bounded_at_ingress_while_its_pump_cannot_run() {
    use crate::engine::agent_runtime::frontend::send_lossy;
    let (client, agent, _from_client) = client(true);
    let handle = client.cancel_handle();
    let gate = agent.gates.stderr.clone();
    gate.attach();
    // A bridge thread producing 2048 chunks while the single executor thread
    // is busy: nothing beyond the stderr budget may be held.
    let (chunks, size) = (2048usize, 64usize);
    for _ in 0..chunks {
        assert!(send_lossy(Some(&gate), &agent.stderr, vec![b'e'; size]));
    }
    let held = gate.usage();
    let b = budgets();
    assert!(
        held.items <= b.stderr_items && held.bytes <= b.stderr_bytes,
        "{held:?}"
    );
    // Once the pump runs, what was dropped at ingress is counted.
    let total = (chunks * size) as u64;
    let stats = awman_test_support::wait_for_async("ingress drops counted", LIMIT, || {
        let stats = handle.stats();
        let counted = stats.stderr_dropped_bytes >= total - b.stderr_bytes as u64;
        std::future::ready(counted.then_some(stats))
    })
    .await;
    assert!(stats.stderr_transport.high_bytes <= b.stderr_bytes);
    assert!(stats.stderr.high_bytes <= b.stderr_bytes);
    drop(client);
}

#[tokio::test(flavor = "current_thread")]
async fn a_producer_that_cannot_wait_fails_the_session_instead_of_queueing() {
    use crate::engine::agent_runtime::frontend::send_bounded;
    let (client, agent, _from_client) = client(true);
    let handle = client.cancel_handle();
    let gate = agent.gates.stdout.clone();
    // An event loop forwarding output while the client cannot run.
    let mut accepted = 0;
    for _ in 0..64 {
        if !send_bounded(Some(&gate), &agent.stdout, vec![b' '; 1024]) {
            break;
        }
        accepted += 1;
    }
    assert_eq!(accepted, budgets().stdout_items);
    awman_test_support::wait_for_async("stdout overflow", LIMIT, || {
        std::future::ready(handle.is_cancelled().then_some(()))
    })
    .await;
    assert!(matches!(
        client.failure(),
        Some(EngineError::AcpOverflow {
            queue: "transport_stdout"
        })
    ));
    assert!(handle.stats().stdout.high_items <= budgets().stdout_items);
}

#[tokio::test]
async fn an_outbound_message_over_the_line_limit_is_refused_before_it_is_queued() {
    let (client, _agent, _from_client) = client(true);
    let huge = "p".repeat(crate::engine::acp::protocol::MAX_ACP_LINE_BYTES);
    let result = client
        .notify("session/prompt", json!({ "text": huge }))
        .await;
    assert!(matches!(result, Err(EngineError::Acp(_))), "{result:?}");
    assert_eq!(client.stats().stdin.high_items, 0);
}
