//! Resumable, unambiguous command event streams.
//!
//! In-process tests drive the real Axum router (`GET /v1/worker/commands/{id}/events`)
//! over loopback with a hand-rolled SSE reader. A worker is modelled with the
//! same parts the queue worker uses (store claim, `EventBus::for_attempt`,
//! `CommandLogWriter`), so no Docker is needed. One end-to-end test restarts a
//! real `awman api start` process (fake `docker` on `PATH`) to see a genuine
//! re-queue.
//!
//! Long waits use tokio's paused clock, never real 300 s sleeps.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::Engines;
use awman::data::command_status::CommandStatus;
use awman::data::execution_event::{EventPayload, ExecutionEvent};
use awman::data::fs::api_command_log::CommandLogWriter;
use awman::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session::SessionKind;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::frontend::AgentFrontend;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman::frontend::api::routes::{build_router, AppState};

// ─── Fixtures ────────────────────────────────────────────────────────────────

const SESSION: &str = "ses_f5b338d6bca36d47ee04d93d08c57861";

fn make_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = Arc::new(SqliteSessionStore::open(paths.root()).expect("open sqlite"));
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    let lifecycle_store = awman::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
    let setup_cancels = Arc::new(awman::command::commands::api_server::TokenRegistry::new());
    let canceller = awman::command::commands::api_server::CommandCanceller::new(
        Arc::clone(&store),
        lifecycle_store.clone(),
        Arc::new(awman::command::commands::api_server::TokenRegistry::new()),
        Arc::clone(&setup_cancels),
    );
    store
        .insert_session_full(NewSessionRow {
            id: SESSION,
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    Arc::new(AppState {
        live_config: std::sync::RwLock::new(None),
        settings_tx: None,
        lifecycle_store,
        setup_cancels,
        canceller,
        store,
        paths,
        workdirs: vec![],
        workdir_roots: Vec::new(),
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

async fn serve(state: Arc<AppState>) -> String {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn enqueue(state: &AppState, id: &str) {
    state.paths.prepare_command_dir(SESSION, id).unwrap();
    let log = state.paths.command_log_path(SESSION, id);
    state
        .store
        .enqueue_command(id, SESSION, "exec prompt", "[]", &log.to_string_lossy())
        .unwrap();
}

/// What the queue worker does at the start of an attempt: claim the row, make
/// the bus for that attempt, register the reliable log consumer before any
/// emit, publish the bus.
struct Attempt {
    bus: Arc<EventBus>,
    writer: tokio::task::JoinHandle<()>,
}

async fn start_attempt(state: &Arc<AppState>, id: &str) -> Attempt {
    let record = state.store.claim_next_command("w").unwrap().unwrap();
    assert_eq!(record.id, id);
    let bus = Arc::new(EventBus::for_attempt(4096, record.attempt));
    let mut log_rx = bus.subscribe_log();
    let mut writer = CommandLogWriter::open_append(&state.paths, SESSION, id)
        .await
        .unwrap();
    let writer = tokio::spawn(async move {
        while let Some(event) = log_rx.recv().await {
            let done = matches!(event.payload, EventPayload::Done);
            writer.write_event(&event).await;
            writer.flush().await;
            if done {
                break;
            }
        }
    });
    state
        .event_buses
        .lock()
        .await
        .insert(id.to_string(), Arc::clone(&bus));
    Attempt { bus, writer }
}

impl Attempt {
    fn stdout(&self, text: &str) {
        self.bus
            .sender()
            .emit(EventPayload::StdoutLine(text.to_string()));
    }

    /// What the worker does on exit: emit `Done`, wait for the log writer to
    /// flush it, and only then persist the terminal row (queue_worker.rs).
    async fn finish(self, state: &AppState, id: &str) {
        self.bus.sender().emit(EventPayload::Done);
        self.writer.await.unwrap();
        state
            .store
            .complete_command(id, CommandStatus::Done, Some(0), None)
            .unwrap();
        state.event_buses.lock().await.remove(id);
    }
}

/// A command that already ran to completion in one attempt: `lines` stdout
/// lines then `Done`, with the row terminal.
async fn finished_command(state: &Arc<AppState>, id: &str, lines: usize) {
    enqueue(state, id);
    let attempt = start_attempt(state, id).await;
    for i in 0..lines {
        attempt.stdout(&format!("line {i}"));
    }
    attempt.finish(state, id).await;
}

// ─── SSE client ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct Frame {
    id: Option<String>,
    event: Option<String>,
    data: Option<String>,
    comment: Option<String>,
}

impl Frame {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(self.data.as_deref().expect("frame has data")).unwrap()
    }
    fn is_event(&self, name: &str) -> bool {
        self.event.as_deref() == Some(name)
    }
    fn text(&self) -> String {
        self.json()["payload"]["data"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

struct Sse {
    response: reqwest::Response,
    buf: String,
}

impl Sse {
    async fn open(
        base: &str,
        command: &str,
        query: &str,
        last_event_id: Option<&str>,
    ) -> Result<Sse, (u16, String)> {
        let mut request = reqwest::Client::new()
            .get(format!("{base}/v1/worker/commands/{command}/events{query}"));
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }
        let response = request.send().await.expect("GET logs");
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err((status, response.text().await.unwrap_or_default()));
        }
        Ok(Sse {
            response,
            buf: String::new(),
        })
    }

    async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut frame = Frame::default();
                for line in block.lines() {
                    if let Some(rest) = line.strip_prefix("id:") {
                        frame.id = Some(rest.trim().to_string());
                    } else if let Some(rest) = line.strip_prefix("event:") {
                        frame.event = Some(rest.trim().to_string());
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        frame.data = Some(rest.trim_start().to_string());
                    } else if let Some(rest) = line.strip_prefix(':') {
                        frame.comment = Some(rest.trim().to_string());
                    }
                }
                return Some(frame);
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self
                    .buf
                    .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n")),
                _ => return None,
            }
        }
    }

    /// Read to the end of the stream.
    async fn all(mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            frames.push(frame);
        }
        frames
    }
}

async fn stream(base: &str, command: &str, query: &str, last: Option<&str>) -> Vec<Frame> {
    Sse::open(base, command, query, last)
        .await
        .expect("stream opens")
        .all()
        .await
}

/// Frames that carry an id (persisted events).
fn ids(frames: &[Frame]) -> Vec<String> {
    frames.iter().filter_map(|f| f.id.clone()).collect()
}

fn parse_id(id: &str) -> (u32, u64) {
    let (a, s) = id.split_once(':').expect("id is attempt:sequence");
    (a.parse().unwrap(), s.parse().unwrap())
}

fn assert_strictly_increasing(ids: &[String]) {
    let parsed: Vec<_> = ids.iter().map(|id| parse_id(id)).collect();
    assert!(
        parsed.windows(2).all(|w| w[0] < w[1]),
        "ids must strictly increase: {ids:?}"
    );
}

fn count_done(frames: &[Frame]) -> usize {
    frames.iter().filter(|f| f.is_event("done")).count()
}

/// Fail `body` if it outlasts `REAL_TIME_GUARD` on the wall clock. Paused-clock
/// tests use this rather than `tokio::time::timeout`: virtual time auto-advances
/// while a read waits on a real socket, so a virtual deadline can expire on a
/// healthy stream.
const REAL_TIME_GUARD: Duration = Duration::from_secs(120);

async fn within_real_time<F: std::future::Future>(body: F) -> F::Output {
    let (expired, on_expiry) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(REAL_TIME_GUARD);
        let _ = expired.send(());
    });
    tokio::select! {
        output = body => output,
        _ = on_expiry => panic!("test body still running after {REAL_TIME_GUARD:?}"),
    }
}

// ─── Queued past 300 s ───────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn queued_command_waiting_past_300s_keeps_the_stream_open_with_15s_keepalives() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_84fbe0280a6e0c725f00f8847aa6be49");

    within_real_time(async {
        let started = tokio::time::Instant::now();
        let mut sse = Sse::open(&base, "cmd_84fbe0280a6e0c725f00f8847aa6be49", "", None)
            .await
            .unwrap();

        // The informational frame is outside the cursor namespace: no id.
        let waiting = sse.next().await.unwrap();
        assert!(waiting.is_event("status_message"), "{waiting:?}");
        assert!(waiting.id.is_none(), "waiting frame must not carry an id");
        assert!(waiting.text().is_empty());
        assert!(waiting.json()["payload"]["data"]["message"]
            .as_str()
            .unwrap()
            .contains("Waiting for a worker"));

        // Well past the old 300 s deadline: only keepalive comments. Count
        // frames rather than read the clock per frame: virtual time can run
        // ahead of delivery over a real socket. The cadence is tested on
        // `wait_for_worker` itself in routes.rs.
        for _ in 0..22 {
            let frame = sse
                .next()
                .await
                .expect("stream must stay open while queued");
            assert!(
                frame.event.is_none() && frame.id.is_none(),
                "unexpected frame while queued: {frame:?} at {:?}",
                started.elapsed()
            );
            assert!(frame.comment.is_some(), "{frame:?}");
        }
        assert!(started.elapsed() >= Duration::from_secs(330));

        // The row is still queued: no Done was fabricated.
        let row = state
            .store
            .get_command("cmd_84fbe0280a6e0c725f00f8847aa6be49")
            .unwrap()
            .unwrap();
        assert_eq!(row.status.as_str(), "queued");

        // A worker finally claims it; the same connection sees real output
        // and exactly one terminal Done.
        let attempt = start_attempt(&state, "cmd_84fbe0280a6e0c725f00f8847aa6be49").await;
        attempt.stdout("finally running");
        attempt
            .finish(&state, "cmd_84fbe0280a6e0c725f00f8847aa6be49")
            .await;
        let mut rest = Vec::new();
        while let Some(frame) = sse.next().await {
            if frame.comment.is_none() {
                rest.push(frame);
            }
        }
        assert_eq!(rest.len(), 2, "{rest:?}");
        assert_eq!(rest[0].id.as_deref(), Some("1:0"));
        assert_eq!(rest[0].text(), "finally running");
        assert!(rest[1].is_event("done"));
        assert_eq!(rest[1].id.as_deref(), Some("1:1"));
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_queued_stream_resumes_by_id_after_the_client_reconnects_past_300s() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_f64cff138020a2060a9817272f563b3c");

    within_real_time(async {
        // First connection waits 20 s of keepalives, then drops.
        let mut first = Sse::open(&base, "cmd_f64cff138020a2060a9817272f563b3c", "", None)
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            first.next().await.unwrap();
        }
        drop(first);

        tokio::time::sleep(Duration::from_secs(400)).await;
        let attempt = start_attempt(&state, "cmd_f64cff138020a2060a9817272f563b3c").await;
        attempt.stdout("a");
        attempt.stdout("b");
        attempt
            .finish(&state, "cmd_f64cff138020a2060a9817272f563b3c")
            .await;

        // A client that saw nothing resumes from nothing; one that saw `1:0`
        // gets the rest.
        let all = stream(&base, "cmd_f64cff138020a2060a9817272f563b3c", "", None).await;
        assert_eq!(ids(&all), ["1:0", "1:1", "1:2"]);
        let tail = stream(
            &base,
            "cmd_f64cff138020a2060a9817272f563b3c",
            "",
            Some("1:0"),
        )
        .await;
        assert_eq!(ids(&tail), ["1:1", "1:2"]);
    })
    .await;
}

// ─── Resume by id ────────────────────────────────────────────────────

#[tokio::test]
async fn ids_are_attempt_colon_sequence_and_done_has_the_next_id() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    finished_command(&state, "cmd_bf516925bb37a8544c8ee19a24e15c05", 3).await;

    let frames = stream(&base, "cmd_bf516925bb37a8544c8ee19a24e15c05", "", None).await;
    assert_eq!(ids(&frames), ["1:0", "1:1", "1:2", "1:3"]);
    assert_eq!(count_done(&frames), 1);
    assert!(frames.last().unwrap().is_event("done"));
    for frame in &frames {
        let json = frame.json();
        assert_eq!(json["attempt"], 1);
        assert_eq!(
            frame.id.as_deref().unwrap(),
            format!("{}:{}", json["attempt"], json["sequence"])
        );
    }
}

#[tokio::test]
async fn resuming_from_any_point_has_no_duplicates_and_no_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    finished_command(&state, "cmd_fe47aa7c733c490d36e80508d5dc4019", 12).await;

    let full = stream(&base, "cmd_fe47aa7c733c490d36e80508d5dc4019", "", None).await;
    let full_ids = ids(&full);
    assert_eq!(full_ids.len(), 13);

    for (cut, seen) in full_ids.iter().enumerate() {
        let expected = &full_ids[cut + 1..];
        // Via the header.
        let via_header = stream(
            &base,
            "cmd_fe47aa7c733c490d36e80508d5dc4019",
            "",
            Some(seen),
        )
        .await;
        // Via the query parameter.
        let via_query = stream(
            &base,
            "cmd_fe47aa7c733c490d36e80508d5dc4019",
            &format!("?after={seen}"),
            None,
        )
        .await;
        for (how, frames) in [("header", via_header), ("after", via_query)] {
            let resumed = ids(&frames);
            if cut + 1 == full_ids.len() {
                // Reconnecting at the terminal id still learns the command
                // is over (a repeated Done), never more events.
                assert_eq!(count_done(&frames), 1, "{how} at {seen}");
                assert_eq!(resumed.len(), 1, "{how} at {seen}: {resumed:?}");
                continue;
            }
            assert_eq!(resumed, expected, "{how} after {seen}");
            // No duplicates, no gaps: prefix + resumed == full.
            let rebuilt: Vec<_> = full_ids[..=cut].iter().chain(&resumed).cloned().collect();
            assert_eq!(rebuilt, full_ids, "{how} after {seen}");
            assert_eq!(count_done(&frames), 1);
        }
    }
}

#[tokio::test]
async fn after_query_overrides_last_event_id() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    finished_command(&state, "cmd_e81f51b496552d5548d2739e87daf131", 5).await;

    let frames = stream(
        &base,
        "cmd_e81f51b496552d5548d2739e87daf131",
        "?after=1:3",
        Some("1:0"),
    )
    .await;
    assert_eq!(ids(&frames), ["1:4", "1:5"]);
    // A valid query also beats a malformed header.
    let frames = stream(
        &base,
        "cmd_e81f51b496552d5548d2739e87daf131",
        "?after=1:3",
        Some("garbage"),
    )
    .await;
    assert_eq!(ids(&frames), ["1:4", "1:5"]);
}

#[tokio::test]
async fn resuming_a_live_stream_across_a_disconnect_loses_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_d0dbe915091d400bd8ee7f27f0791303");
    let attempt = start_attempt(&state, "cmd_d0dbe915091d400bd8ee7f27f0791303").await;
    for i in 0..5 {
        attempt.stdout(&format!("l{i}"));
    }

    // First client reads three events, then drops.
    let mut first = Sse::open(&base, "cmd_d0dbe915091d400bd8ee7f27f0791303", "", None)
        .await
        .unwrap();
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let frame = first.next().await.unwrap();
        if let Some(id) = frame.id {
            seen.push(id);
        }
    }
    drop(first);

    for i in 5..9 {
        attempt.stdout(&format!("l{i}"));
    }
    let last_seen = seen.last().unwrap().clone();
    let resumed = tokio::spawn({
        let base = base.clone();
        async move {
            stream(
                &base,
                "cmd_d0dbe915091d400bd8ee7f27f0791303",
                "",
                Some(&last_seen),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    attempt.stdout("l9");
    attempt
        .finish(&state, "cmd_d0dbe915091d400bd8ee7f27f0791303")
        .await;
    let resumed = resumed.await.unwrap();

    let rebuilt: Vec<String> = seen.into_iter().chain(ids(&resumed)).collect();
    let expected: Vec<String> = (0..=10).map(|n| format!("1:{n}")).collect();
    assert_eq!(rebuilt, expected, "no gaps and no duplicates");
    assert_eq!(count_done(&resumed), 1);
}

/// Events a client connected too late to receive live, and that the log did
/// not hold yet when it was replayed, arrive with no further output.
#[tokio::test]
async fn events_the_log_writer_had_not_flushed_at_connect_still_arrive() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_9da3be8f92c0bbfd7a7d6d6ec0b3fad3");
    let record = state.store.claim_next_command("w").unwrap().unwrap();
    let bus = Arc::new(EventBus::for_attempt(4096, record.attempt));
    let mut log_rx = bus.subscribe_log();
    state.event_buses.lock().await.insert(
        "cmd_9da3be8f92c0bbfd7a7d6d6ec0b3fad3".to_string(),
        Arc::clone(&bus),
    );
    for i in 0..3 {
        bus.sender().emit(EventPayload::StdoutLine(format!("l{i}")));
    }

    // The client replays an empty log, and only then does the writer catch up.
    let mut client = Sse::open(&base, "cmd_9da3be8f92c0bbfd7a7d6d6ec0b3fad3", "", None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut writer = CommandLogWriter::open_append(
        &state.paths,
        SESSION,
        "cmd_9da3be8f92c0bbfd7a7d6d6ec0b3fad3",
    )
    .await
    .unwrap();
    for _ in 0..3 {
        writer.write_event(&log_rx.recv().await.unwrap()).await;
    }
    writer.flush().await;

    let mut seen = Vec::new();
    while seen.len() < 3 {
        let frame = tokio::time::timeout(Duration::from_secs(10), client.next())
            .await
            .expect("flushed events reach a quiet stream")
            .unwrap();
        seen.extend(frame.id);
    }
    assert_eq!(seen, ["1:0", "1:1", "1:2"]);
}

#[tokio::test]
async fn a_stale_attempt_id_replays_from_the_start_of_the_current_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;

    // Attempt 1 crashes mid-run (no Done); the restart re-queues it.
    enqueue(&state, "cmd_36f34fd8319cf30f8e132ef294c616af");
    let first = start_attempt(&state, "cmd_36f34fd8319cf30f8e132ef294c616af").await;
    first.stdout("old a");
    first.stdout("old b");
    drop(first.bus.sender());
    // Let the log writer persist, then "crash".
    tokio::time::sleep(Duration::from_millis(100)).await;
    first.writer.abort();
    state
        .event_buses
        .lock()
        .await
        .remove("cmd_36f34fd8319cf30f8e132ef294c616af");
    assert_eq!(
        state.store.recover_stale_commands(0).unwrap(),
        ["cmd_36f34fd8319cf30f8e132ef294c616af"]
    );
    let second = start_attempt(&state, "cmd_36f34fd8319cf30f8e132ef294c616af").await;
    second.stdout("new a");
    second
        .finish(&state, "cmd_36f34fd8319cf30f8e132ef294c616af")
        .await;

    // The old attempt's events are no longer kept (log pruned/rotated).
    let log = state
        .paths
        .command_events_log_path(SESSION, "cmd_36f34fd8319cf30f8e132ef294c616af");
    let kept: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| l.contains(r#""attempt":2"#))
        .map(str::to_string)
        .collect();
    assert!(!kept.is_empty());
    std::fs::write(&log, kept.join("\n") + "\n").unwrap();

    let full_current = stream(&base, "cmd_36f34fd8319cf30f8e132ef294c616af", "", None).await;
    let current_ids = ids(&full_current);
    assert!(
        current_ids.iter().all(|id| id.starts_with("2:")),
        "{current_ids:?}"
    );

    for stale in ["1:1", "1:99", "0:0", "7:3"] {
        for (query, header) in [("", Some(stale)), (&*format!("?after={stale}"), None)] {
            let frames = stream(&base, "cmd_36f34fd8319cf30f8e132ef294c616af", query, header).await;
            assert_eq!(ids(&frames), current_ids, "stale cursor {stale}");
            assert!(
                frames
                    .iter()
                    .any(|f| f.comment.as_deref() == Some("resuming from the start of attempt 2")),
                "fallback is announced: {frames:?}"
            );
            assert_eq!(count_done(&frames), 1);
        }
    }
}

#[tokio::test]
async fn malformed_ids_are_rejected_with_400() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    finished_command(&state, "cmd_bae60998ffe4923b131e3d6e4c19993e", 2).await;

    for bad in [
        "",
        "5",
        "1:",
        ":1",
        "a:b",
        "-1:0",
        "1:2:3",
        "1:99999999999999999999999",
    ] {
        let by_header =
            Sse::open(&base, "cmd_bae60998ffe4923b131e3d6e4c19993e", "", Some(bad)).await;
        // An empty header value is not "absent": it is malformed too.
        assert_eq!(by_header.err().map(|e| e.0), Some(400), "header {bad:?}");
        let by_query = Sse::open(
            &base,
            "cmd_bae60998ffe4923b131e3d6e4c19993e",
            &format!("?after={bad}"),
            None,
        )
        .await;
        assert_eq!(by_query.err().map(|e| e.0), Some(400), "after {bad:?}");
    }
    // A bad `after` is rejected even with a good header.
    let both = Sse::open(
        &base,
        "cmd_bae60998ffe4923b131e3d6e4c19993e",
        "?after=x",
        Some("1:0"),
    )
    .await;
    assert_eq!(both.err().map(|e| e.0), Some(400));
    // The JSON snapshot ignores resume inputs entirely.
    let json: serde_json::Value = reqwest::get(format!(
        "{base}/v1/worker/commands/cmd_bae60998ffe4923b131e3d6e4c19993e/events?format=json&after=x"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), 3);
    assert_eq!(json["attempt"], 1);
}

// ─── Old clients ─────────────────────────────────────────────────────

#[tokio::test]
async fn clients_that_ignore_id_see_unchanged_event_names_and_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_149603e6c03516362a8da23f624db945");
    let attempt = start_attempt(&state, "cmd_149603e6c03516362a8da23f624db945").await;
    attempt.stdout("hello");
    attempt
        .bus
        .sender()
        .emit(EventPayload::StderrLine("oops".into()));
    attempt.bus.sender().emit(EventPayload::StatusMessage {
        phase: "setup".into(),
        message: "ready".into(),
    });
    attempt
        .finish(&state, "cmd_149603e6c03516362a8da23f624db945")
        .await;

    // Read the raw bytes the way a client without resume support does: only `event:` and
    // `data:` lines matter, `id:` lines are skipped.
    let raw = reqwest::get(format!(
        "{base}/v1/worker/commands/cmd_149603e6c03516362a8da23f624db945/events"
    ))
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    let mut legacy: Vec<(String, serde_json::Value)> = Vec::new();
    let mut event = None;
    for line in raw.lines() {
        if let Some(name) = line.strip_prefix("event:") {
            event = Some(name.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data:") {
            legacy.push((
                event.take().expect("data follows an event line"),
                serde_json::from_str(data.trim()).expect("data is JSON"),
            ));
        }
    }
    let names: Vec<_> = legacy.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["stdout_line", "stderr_line", "status_message", "done"]
    );
    let (_, stdout) = &legacy[0];
    assert_eq!(stdout["payload"]["type"], "StdoutLine");
    assert_eq!(stdout["payload"]["data"], "hello");
    assert!(stdout["timestamp"].is_string());
    assert_eq!(stdout["sequence"], 0);
    let (_, status) = &legacy[2];
    assert_eq!(status["payload"]["data"]["phase"], "setup");
    assert_eq!(status["payload"]["data"]["message"], "ready");
    assert_eq!(legacy[3].1["payload"]["type"], "Done");

    // Every event line is followed by an id line only as an *addition*.
    assert!(raw.lines().filter(|l| l.starts_with("id:")).count() == 4);
    // The full-log JSON snapshot keeps the same typed events.
    let json: serde_json::Value = reqwest::get(format!(
        "{base}/v1/worker/commands/cmd_149603e6c03516362a8da23f624db945/events?format=json"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn a_pre_attempt_events_log_streams_as_attempt_one() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_228c70bfc5589c58c044e03fff0e17eb");
    state.store.claim_next_command("w").unwrap().unwrap();
    state
        .store
        .complete_command(
            "cmd_228c70bfc5589c58c044e03fff0e17eb",
            CommandStatus::Done,
            Some(0),
            None,
        )
        .unwrap();
    // Lines as an older awman wrote them: no `attempt` field.
    let legacy = [
        r#"{"timestamp":"2026-01-01T00:00:00Z","sequence":0,"payload":{"type":"StdoutLine","data":"x"}}"#,
        r#"{"timestamp":"2026-01-01T00:00:01Z","sequence":1,"payload":{"type":"Done"}}"#,
    ];
    std::fs::write(
        state
            .paths
            .command_events_log_path(SESSION, "cmd_228c70bfc5589c58c044e03fff0e17eb"),
        legacy.join("\n") + "\n",
    )
    .unwrap();

    let frames = stream(&base, "cmd_228c70bfc5589c58c044e03fff0e17eb", "", None).await;
    assert_eq!(ids(&frames), ["1:0", "1:1"]);
    let resumed = stream(
        &base,
        "cmd_228c70bfc5589c58c044e03fff0e17eb",
        "",
        Some("1:0"),
    )
    .await;
    assert_eq!(ids(&resumed), ["1:1"]);
}

// ─── Output right up to exit ─────────────────────────────────────────

/// One run through the real frontend drains, bus and log writer, read back
/// over the real route. The last stdout line must precede `Done` in both the
/// stream and `events.log`.
async fn run_to_exit(state: &Arc<AppState>, base: &str, run: usize) {
    let id = format!("cmd_{run:032x}");
    enqueue(state, &id);
    let attempt = start_attempt(state, &id).await;
    let mut frontend = ApiDispatchFrontend::new("exec prompt", &[], attempt.bus.sender());
    let io = AgentFrontend::take_io(&mut frontend);
    let (stdout, stderr) = (io.stdout.clone(), io.stderr.clone());
    drop(io);

    // A client attached before the run ends.
    let live = tokio::spawn({
        let (base, id) = (base.to_string(), id.clone());
        async move { stream(&base, &id, "", None).await }
    });

    let last = format!("final-{run}");
    for i in 0..(10 + run % 30) {
        stdout.send(format!("line {i}\n").into_bytes()).unwrap();
        if i % 5 == 0 {
            stderr.send(format!("warn {i}\n").into_bytes()).unwrap();
        }
    }
    // No trailing newline on even runs; split across chunks.
    let tail = if run.is_multiple_of(2) {
        last.clone()
    } else {
        format!("{last}\n")
    };
    let (a, b) = tail.split_at(3);
    stdout.send(a.as_bytes().to_vec()).unwrap();
    stdout.send(b.as_bytes().to_vec()).unwrap();
    // The process exits: pipes close. Then the worker's completion sequence.
    drop((stdout, stderr));
    assert!(
        attempt
            .bus
            .wait_output_drained(Duration::from_secs(5))
            .await,
        "run {run}: drains must finish"
    );
    attempt.finish(state, &id).await;

    let live = live.await.unwrap();
    let late = stream(base, &id, "", None).await;
    let log: Vec<ExecutionEvent> = state.paths.read_command_events(SESSION, &id);

    let check = |name: &str, kinds: Vec<(String, String)>| {
        let done = kinds
            .iter()
            .position(|(k, _)| k == "done")
            .unwrap_or_else(|| panic!("run {run}: {name} has no Done"));
        assert_eq!(done, kinds.len() - 1, "run {run}: {name} Done must be last");
        let last_stdout = kinds.iter().rposition(|(k, _)| k == "stdout_line").unwrap();
        assert_eq!(kinds[last_stdout].1, last, "run {run}: {name}");
        assert!(last_stdout < done);
    };
    let from_frames = |frames: &[Frame]| -> Vec<(String, String)> {
        frames
            .iter()
            .filter(|f| f.event.is_some())
            .map(|f| {
                (
                    f.event.clone().unwrap(),
                    if f.is_event("stdout_line") {
                        f.text()
                    } else {
                        String::new()
                    },
                )
            })
            .collect()
    };
    check("live stream", from_frames(&live));
    check("replayed stream", from_frames(&late));
    check(
        "events.log",
        log.iter()
            .map(|e| match &e.payload {
                EventPayload::StdoutLine(s) => ("stdout_line".to_string(), s.clone()),
                EventPayload::Done => ("done".to_string(), String::new()),
                _ => ("other".to_string(), String::new()),
            })
            .collect(),
    );
    assert_eq!(count_done(&live), 1, "run {run}");
    assert_strictly_increasing(&ids(&live));
    assert_eq!(ids(&live), ids(&late), "run {run}: live == replay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_last_stdout_line_precedes_done_in_stream_and_events_log() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    // Claims are ordered per session, so runs go one after another; the
    // concurrency under test is producer vs drain vs worker vs client.
    for run in 0..60 {
        run_to_exit(&state, &base, run).await;
    }
}

// ─── A real re-queue after restart ───────────────────────────────────

#[cfg(unix)]
mod restart {
    use super::*;
    use crate::api_process::{AgentMode, ApiHome};
    use crate::git_daemon::Origin;
    use serde_json::{json, Value};

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_requeue_after_restart_increments_attempt_and_ids_keep_increasing() {
        crate::real_git_skip!();
        let Some(origin) = Origin::start() else {
            return;
        };
        let home = ApiHome::new();
        home.set_agent(AgentMode::Slow);
        let mut server = home.start().await;
        let session = server
            .ready_session(json!({"session_type": "remote", "repo_url": origin.url}))
            .await;

        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/commands", server.url))
            .header("x-awman-session", &session)
            .json(&json!({"subcommand": "exec prompt", "args": ["slow"]}))
            .send()
            .await
            .unwrap();
        let created: Value = response.json().await.unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let pid = home.control().join("slow.pid");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !pid.exists() {
            assert!(Instant::now() < deadline, "slow agent never started");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let before = server.get(&format!("/v1/worker/commands/{id}")).await;
        assert_eq!(before["attempt"], 1, "{before}");

        // Crash the daemon mid-run; the restart re-queues the command and the
        // second attempt finishes promptly.
        server.kill();
        home.set_agent(AgentMode::Edit);
        let server = home.start().await;
        let deadline = Instant::now() + Duration::from_secs(120);
        let record = loop {
            let record = server.get(&format!("/v1/worker/commands/{id}")).await;
            if record["status"] == "done" || record["status"] == "error" {
                break record;
            }
            assert!(
                Instant::now() < deadline,
                "never finished: {record}\n{}",
                server.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(record["attempt"], 2, "{record}");

        let frames = stream(&server.url, &id, "", None).await;
        let all = ids(&frames);
        assert_strictly_increasing(&all);
        let first_second_attempt = all
            .iter()
            .position(|i| i.starts_with("2:"))
            .expect("attempt 2");
        assert!(all[..first_second_attempt]
            .iter()
            .all(|i| i.starts_with("1:")));
        assert_eq!(
            all[first_second_attempt], "2:0",
            "sequence restarts per attempt"
        );
        let requeue = &frames[frames
            .iter()
            .position(|f| f.id.as_deref() == Some("2:0"))
            .unwrap()];
        assert!(requeue.is_event("status_message"), "{requeue:?}");
        assert_eq!(
            requeue.json()["payload"]["data"]["message"],
            "re-queued after restart (attempt 2)"
        );
        assert_eq!(count_done(&frames), 1, "a single Done for the command");
        assert!(frames.last().unwrap().is_event("done"));

        // Resuming from the end of attempt 1 yields exactly attempt 2.
        let last_of_first = &all[first_second_attempt - 1];
        let tail = stream(&server.url, &id, "", Some(last_of_first)).await;
        assert_eq!(ids(&tail), all[first_second_attempt..]);

        // The full-log snapshot is the appended history for both attempts.
        let snapshot = server
            .get(&format!("/v1/worker/commands/{id}/events?format=json"))
            .await;
        assert_eq!(snapshot["attempt"], 2);
        let attempts: Vec<u64> = snapshot["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["attempt"].as_u64().unwrap())
            .collect();
        assert!(attempts.contains(&1) && attempts.contains(&2));
        assert!(attempts.windows(2).all(|w| w[0] <= w[1]));
    }
}

// Regression coverage for the adversarial review's publication/replay boundaries.
#[tokio::test(start_paused = true)]
async fn delayed_persistence_repairs_attach_and_broadcast_lag_before_completion() {
    for attach_gap in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path());
        let base = serve(Arc::clone(&state)).await;
        enqueue(&state, "cmd_df9bcbd6578a1e49c06b7ec2874f9e23");
        state.store.claim_next_command("w").unwrap().unwrap();
        let bus = Arc::new(EventBus::new(4));
        let mut log_rx = bus.subscribe_log();
        let mut writer = CommandLogWriter::open_append(
            &state.paths,
            SESSION,
            "cmd_df9bcbd6578a1e49c06b7ec2874f9e23",
        )
        .await
        .unwrap();
        state.event_buses.lock().await.insert(
            "cmd_df9bcbd6578a1e49c06b7ec2874f9e23".into(),
            Arc::clone(&bus),
        );
        bus.sender().emit(EventPayload::StdoutLine("0".into()));
        writer.write_event(&log_rx.recv().await.unwrap()).await;
        writer.flush().await;
        if attach_gap {
            // Published before subscription, still queued for persistence.
            for n in 1..100 {
                bus.sender().emit(EventPayload::StdoutLine(n.to_string()));
            }
        }
        let mut client = Sse::open(&base, "cmd_df9bcbd6578a1e49c06b7ec2874f9e23", "", None)
            .await
            .unwrap();
        assert_eq!(client.next().await.unwrap().id.as_deref(), Some("1:0"));
        if !attach_gap {
            // No yield: exceed the live broadcast capacity deterministically.
            for n in 1..100 {
                bus.sender().emit(EventPayload::StdoutLine(n.to_string()));
            }
        }
        bus.sender().emit(EventPayload::StdoutLine("100".into()));
        // Before the writer resumes, no higher cursor may escape the gap.
        let pending = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                let frame = client.next().await.expect("gap recovery stays open");
                assert!(frame.id.is_none(), "unrecovered cursor: {frame:?}");
            }
        })
        .await;
        assert!(pending.is_err());
        let writer = tokio::spawn(async move {
            while let Some(event) = log_rx.recv().await {
                writer.write_event(&event).await;
                writer.flush().await;
                if matches!(event.payload, EventPayload::Done) {
                    break;
                }
            }
        });
        within_real_time(async {
            for n in 1..=100 {
                let frame = loop {
                    let frame = client.next().await.unwrap();
                    if frame.id.is_some() {
                        break frame;
                    }
                };
                assert_eq!(frame.id, Some(format!("1:{n}")));
                assert_eq!(frame.text(), n.to_string());
            }
        })
        .await;
        // Recovery must happen while running, not merely in final replay.
        assert_eq!(
            state
                .store
                .get_command("cmd_df9bcbd6578a1e49c06b7ec2874f9e23")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "running"
        );
        bus.sender().emit(EventPayload::Done);
        writer.await.unwrap();
        state
            .store
            .complete_command(
                "cmd_df9bcbd6578a1e49c06b7ec2874f9e23",
                CommandStatus::Done,
                Some(0),
                None,
            )
            .unwrap();
        let tail = client.all().await;
        assert_eq!(ids(&tail), ["1:101"]);
        assert_eq!(count_done(&tail), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn unrecoverable_live_gap_closes_without_advancing_cursor_or_done() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_ea21841da70e6405af19fabc4ff8bdd9");
    state.store.claim_next_command("w").unwrap().unwrap();
    let bus = Arc::new(EventBus::new(4));
    state.event_buses.lock().await.insert(
        "cmd_ea21841da70e6405af19fabc4ff8bdd9".into(),
        Arc::clone(&bus),
    );
    let mut client = Sse::open(&base, "cmd_ea21841da70e6405af19fabc4ff8bdd9", "", None)
        .await
        .unwrap();
    // First keepalive proves subscription is established, without real sleep.
    assert!(client.next().await.unwrap().comment.is_some());
    for n in 0..100 {
        bus.sender().emit(EventPayload::StdoutLine(n.to_string()));
    }
    let frames = within_real_time(client.all()).await;
    assert!(ids(&frames).is_empty());
    assert_eq!(count_done(&frames), 0);
    assert_eq!(
        state
            .store
            .get_command("cmd_ea21841da70e6405af19fabc4ff8bdd9")
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "running"
    );
}

#[tokio::test]
async fn inverted_legacy_records_and_ahead_terminal_cursor_keep_completion_unambiguous() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    finished_command(&state, "cmd_184401f335d6b8661e04dfea47b9dcd5", 3).await;
    let mut events = state
        .paths
        .read_command_events(SESSION, "cmd_184401f335d6b8661e04dfea47b9dcd5");
    events.swap(0, 2);
    let mut writer = std::fs::File::create(
        state
            .paths
            .command_events_log_path(SESSION, "cmd_184401f335d6b8661e04dfea47b9dcd5"),
    )
    .unwrap();
    use std::io::Write as _;
    for event in events {
        writeln!(writer, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }
    let frames = stream(&base, "cmd_184401f335d6b8661e04dfea47b9dcd5", "", None).await;
    assert_eq!(ids(&frames), ["1:0", "1:1", "1:2", "1:3"]);
    let frames = stream(
        &base,
        "cmd_184401f335d6b8661e04dfea47b9dcd5",
        "",
        Some("1:999"),
    )
    .await;
    assert_eq!(ids(&frames), ["1:3"]);
    assert_eq!(count_done(&frames), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_queued_stream_without_done() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    enqueue(&state, "cmd_5924f03a95ee6f7277e5bdd1e81b8fdc");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = build_router(state).layer(axum::Extension(shutdown.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut client = Sse::open(&base, "cmd_5924f03a95ee6f7277e5bdd1e81b8fdc", "", None)
        .await
        .unwrap();
    assert!(client.next().await.unwrap().is_event("status_message"));
    shutdown.cancel();
    let frames = within_real_time(client.all()).await;
    assert_eq!(count_done(&frames), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_git_worker_drains_chatty_process_before_done_in_live_stream_and_log() {
    crate::real_git_skip!();
    let Some(origin) = crate::git_daemon::Origin::start() else {
        return;
    };
    let home = crate::api_process::ApiHome::new();
    home.set_agent(crate::api_process::AgentMode::Chatty);
    let server = home.start().await;
    let session = server
        .ready_session(serde_json::json!({"session_type": "remote", "repo_url": origin.url}))
        .await;
    for run in 0..8 {
        let created: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/worker/commands", server.url))
            .header("x-awman-session", &session)
            .json(&serde_json::json!({"subcommand": "exec prompt", "args": ["burst"]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap();
        let frames =
            tokio::time::timeout(Duration::from_secs(60), stream(&server.url, id, "", None))
                .await
                .unwrap();
        let record = server.get(&format!("/v1/worker/commands/{id}")).await;
        assert_eq!(
            record["status"],
            "done",
            "run {run}: {record}\n{}",
            server.log()
        );
        let log = crate::api_process::events_log(&record).await;
        let persisted: Vec<ExecutionEvent> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            ids(&frames),
            persisted
                .iter()
                .map(|e| e.cursor().to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(count_done(&frames), 1);
        assert!(frames.last().unwrap().is_event("done"));
        for (kind, last) in [
            ("stdout_line", "LAST-STDOUT"),
            ("stderr_line", "LAST-STDERR"),
        ] {
            let output: Vec<_> = frames.iter().filter(|f| f.is_event(kind)).collect();
            assert_eq!(output.len(), 2001, "run {run}: {kind}");
            assert_eq!(output.last().unwrap().json()["payload"]["data"], last);
        }
    }
    assert!(
        !server.log().contains("output drain did not finish"),
        "{}",
        server.log()
    );
}

#[tokio::test]
async fn crash_after_persisted_done_keeps_history_but_only_current_done_ends_sse() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_state(tmp.path());
    let base = serve(Arc::clone(&state)).await;
    enqueue(&state, "cmd_0355b7339d8462a7b418a34ef9fe62c3");
    let first = start_attempt(&state, "cmd_0355b7339d8462a7b418a34ef9fe62c3").await;
    first.stdout("cmd_149603e6c03516362a8da23f624db945");
    first.bus.sender().emit(EventPayload::Done);
    first.writer.await.unwrap();
    // Crash after log flush, before terminal row publication.
    state
        .event_buses
        .lock()
        .await
        .remove("cmd_0355b7339d8462a7b418a34ef9fe62c3");
    assert_eq!(
        state.store.recover_stale_commands(0).unwrap(),
        ["cmd_0355b7339d8462a7b418a34ef9fe62c3"]
    );
    let mut client = Sse::open(&base, "cmd_0355b7339d8462a7b418a34ef9fe62c3", "", None)
        .await
        .unwrap();
    assert_eq!(client.next().await.unwrap().id.as_deref(), Some("1:0"));
    assert!(client.next().await.unwrap().is_event("status_message"));
    let second = start_attempt(&state, "cmd_0355b7339d8462a7b418a34ef9fe62c3").await;
    second.stdout("new");
    second
        .finish(&state, "cmd_0355b7339d8462a7b418a34ef9fe62c3")
        .await;
    let tail = client.all().await;
    assert_eq!(ids(&tail), ["2:0", "2:1"]);
    assert_eq!(count_done(&tail), 1);
    let persisted = state
        .paths
        .read_command_events(SESSION, "cmd_0355b7339d8462a7b418a34ef9fe62c3");
    assert_eq!(
        persisted
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Done))
            .count(),
        2
    );
    let resumed = stream(
        &base,
        "cmd_0355b7339d8462a7b418a34ef9fe62c3",
        "",
        Some("1:1"),
    )
    .await;
    assert_eq!(ids(&resumed), ["2:0", "2:1"]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_git_cancelling_chatty_process_drains_and_sends_done_without_timeout() {
    crate::real_git_skip!();
    let Some(origin) = crate::git_daemon::Origin::start() else {
        return;
    };
    let home = crate::api_process::ApiHome::new();
    home.set_agent(crate::api_process::AgentMode::ChattySlow);
    let server = home.start().await;
    let session = server
        .ready_session(serde_json::json!({"session_type": "remote", "repo_url": origin.url}))
        .await;
    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/worker/commands", server.url))
        .header("x-awman-session", &session)
        .json(&serde_json::json!({"subcommand": "exec prompt", "args": ["burst"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    let mut client = Sse::open(&server.url, id, "", None).await.unwrap();
    // An output event proves this is a running cancellation, not a queued one.
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), client.next())
            .await
            .unwrap()
            .unwrap();
        if frame.is_event("stdout_line") {
            break;
        }
        assert!(!frame.is_event("done"));
    }
    let cancelled = reqwest::Client::new()
        .post(format!("{}/v1/worker/commands/{id}/cancel", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status().as_u16(), 202);
    let tail = tokio::time::timeout(Duration::from_secs(15), client.all())
        .await
        .unwrap();
    assert_eq!(count_done(&tail), 1);
    let record = server.get(&format!("/v1/worker/commands/{id}")).await;
    assert_eq!(record["status"], "cancelled", "{record}\n{}", server.log());
    assert!(
        !server.log().contains("output drain did not finish"),
        "{}",
        server.log()
    );
}
