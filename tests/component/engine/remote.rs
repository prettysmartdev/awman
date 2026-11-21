//! `engine::remote` — `RemoteClient` against a loopback mock server on a
//! sandbox-owned listener: HTTP status mapping and a refused connection.
//! Exercises the public command-status API and its transport error contract.

use crate::support;
use awman::engine::error::EngineError;
use awman::engine::remote::RemoteClient;
use awman_test_support::TestSandbox;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

async fn mock_server(sandbox: &TestSandbox) -> MockServer {
    MockServer::builder()
        .listener(sandbox.ports().tcp_listener())
        .start()
        .await
}

async fn respond(server: &MockServer, path: &str, status: u16, body: serde_json::Value) {
    Mock::given(matchers::method("GET"))
        .and(matchers::path(path))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn send_command_returns_the_parsed_response_on_200() {
    let sandbox =
        support::sandbox("engine::remote::send_command_returns_the_parsed_response_on_200");
    let server = mock_server(&sandbox).await;
    respond(
        &server,
        "/v1/worker/commands/current",
        200,
        serde_json::json!({"ok": true}),
    )
    .await;
    let client = RemoteClient::new(&server.uri(), None).unwrap();
    let response = client.get_command("current").await.unwrap();
    assert_eq!(response.status, 200);
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn send_command_maps_4xx_and_5xx_to_remote_http_status() {
    let sandbox =
        support::sandbox("engine::remote::send_command_maps_4xx_and_5xx_to_remote_http_status");
    let server = mock_server(&sandbox).await;
    respond(
        &server,
        "/v1/worker/commands/rejected",
        400,
        serde_json::json!({"error": "bad request"}),
    )
    .await;
    respond(
        &server,
        "/v1/worker/commands/current",
        500,
        serde_json::json!({"error": "internal server error"}),
    )
    .await;
    let client = RemoteClient::new(&server.uri(), None).unwrap();
    let bad = client.get_command("rejected").await;
    assert!(
        matches!(bad, Err(EngineError::RemoteHttpStatus { status: 400, .. })),
        "{bad:?}"
    );
    let failed = client.get_command("current").await;
    assert!(
        matches!(
            failed,
            Err(EngineError::RemoteHttpStatus { status: 500, .. })
        ),
        "{failed:?}"
    );
    drop(server);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_maps_to_remote_connection_refused() {
    let sandbox =
        support::sandbox("engine::remote::a_refused_connection_maps_to_remote_connection_refused");
    // An address this sandbox owned and closed: nothing listens there.
    let listener = sandbox.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = RemoteClient::new(&format!("http://{addr}"), None).unwrap();
    let result = client.get_command("current").await;
    assert!(
        matches!(result, Err(EngineError::RemoteConnectionRefused(_))),
        "{result:?}"
    );
    sandbox.finish();
}

// ── Following a command's event stream (SSE decoding, cancellation, reconnect) ──
//
// An owned fake SSE server on a sandbox listener plays one script per
// connection and records every request, so each test can assert what was
// resumed (`Last-Event-ID`) and that nothing but the events route was called
// (no remote cancel, no resubmission).

mod follow {
    use crate::support;
    use awman::data::execution_event::{EventCursor, EventPayload, ExecutionEvent};
    use awman::engine::error::EngineError;
    use awman::engine::remote::{
        run_cancellable, ExecutionEventSink, FollowEnd, ReconnectNotice, ReconnectPolicy,
        RemoteClient, MAX_SSE_FRAME_BYTES,
    };
    use awman_test_support::{wait::wait_for_async, TestSandbox};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_util::sync::CancellationToken;

    const EVENTS_PATH: &str = "/v1/worker/commands/cmd-1/events";
    const DEADLINE: Duration = Duration::from_secs(10);

    #[derive(Clone)]
    enum Step {
        /// Write these bytes as one write, flushed.
        Send(Vec<u8>),
        /// Keep the connection open until the client closes it.
        Hold,
    }

    #[derive(Debug, Clone)]
    struct Recorded {
        method: String,
        path: String,
        last_event_id: Option<String>,
    }

    struct FakeSse {
        uri: String,
        requests: Arc<Mutex<Vec<Recorded>>>,
        /// Connections the client closed while the server held them open.
        client_closed: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeSse {
        /// `scripts[i]` answers the i-th events request; later requests get 503.
        fn start(sandbox: &TestSandbox, scripts: Vec<Vec<Step>>) -> Self {
            let listener = sandbox.ports().tcp_listener();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let client_closed = Arc::new(AtomicUsize::new(0));
            let scripts = Arc::new(scripts);
            let next_script = Arc::new(AtomicUsize::new(0));
            let task = {
                let requests = requests.clone();
                let client_closed = client_closed.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((socket, _)) = listener.accept().await else {
                            return;
                        };
                        let requests = requests.clone();
                        let client_closed = client_closed.clone();
                        let scripts = scripts.clone();
                        let next_script = next_script.clone();
                        tokio::spawn(async move {
                            let mut reader = BufReader::new(socket);
                            let mut request_line = String::new();
                            if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or_default().to_string();
                            let path = parts.next().unwrap_or_default().to_string();
                            let mut last_event_id = None;
                            loop {
                                let mut line = String::new();
                                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                let line = line.trim_end();
                                if line.is_empty() {
                                    break;
                                }
                                if let Some((name, value)) = line.split_once(':') {
                                    if name.eq_ignore_ascii_case("last-event-id") {
                                        last_event_id = Some(value.trim().to_string());
                                    }
                                }
                            }
                            requests.lock().unwrap().push(Recorded {
                                method: method.clone(),
                                path: path.clone(),
                                last_event_id,
                            });
                            let mut socket = reader.into_inner();
                            let script = (method == "GET" && path == EVENTS_PATH)
                                .then(|| scripts.get(next_script.fetch_add(1, Ordering::SeqCst)))
                                .flatten();
                            let Some(script) = script else {
                                let _ = socket
                                    .write_all(
                                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                                    )
                                    .await;
                                return;
                            };
                            if socket
                                .write_all(
                                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                                )
                                .await
                                .is_err()
                            {
                                return;
                            }
                            for step in script {
                                match step {
                                    Step::Send(bytes) => {
                                        if socket.write_all(bytes).await.is_err()
                                            || socket.flush().await.is_err()
                                        {
                                            return;
                                        }
                                    }
                                    Step::Hold => {
                                        let mut buf = [0u8; 64];
                                        while matches!(socket.read(&mut buf).await, Ok(n) if n > 0)
                                        {
                                        }
                                        client_closed.fetch_add(1, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                            let _ = socket.shutdown().await;
                        });
                    }
                })
            };
            Self {
                uri: format!("http://{addr}"),
                requests,
                client_closed,
                task,
            }
        }

        fn client(&self) -> RemoteClient {
            RemoteClient::new(&self.uri, None).unwrap()
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        /// Every request went to the events route: nothing cancelled or
        /// resubmitted the command.
        fn assert_only_event_requests(&self) {
            for request in self.requests() {
                assert_eq!(
                    (request.method.as_str(), request.path.as_str()),
                    ("GET", EVENTS_PATH),
                    "unexpected request {request:?}"
                );
            }
        }

        fn resume_ids(&self) -> Vec<Option<String>> {
            self.requests()
                .into_iter()
                .map(|request| request.last_event_id)
                .collect()
        }
    }

    impl Drop for FakeSse {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn event(attempt: u32, sequence: u64, payload: EventPayload) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt,
            sequence,
            source: None,
            payload,
        }
    }

    /// The server's wire shape for a persisted event (`id:` + `event:` + `data:`).
    fn frame(attempt: u32, sequence: u64, payload: EventPayload) -> Vec<u8> {
        let event = event(attempt, sequence, payload);
        format!(
            "id: {attempt}:{sequence}\nevent: {}\ndata: {}\n\n",
            event.payload.sse_event_type(),
            serde_json::to_string(&event.public_value()).unwrap()
        )
        .into_bytes()
    }

    fn line(attempt: u32, sequence: u64, text: &str) -> Vec<u8> {
        frame(attempt, sequence, EventPayload::StdoutLine(text.into()))
    }

    fn done(attempt: u32, sequence: u64) -> Vec<u8> {
        frame(attempt, sequence, EventPayload::Done)
    }

    fn send(bytes: Vec<u8>) -> Step {
        Step::Send(bytes)
    }

    fn fast_policy(attempts: usize) -> ReconnectPolicy {
        ReconnectPolicy {
            delays: vec![Duration::from_millis(5); attempts],
        }
    }

    #[derive(Default)]
    struct Recorder {
        lines: Vec<String>,
        notices: Vec<ReconnectNotice>,
        done_seen: usize,
        /// Cancelled from inside `on_reconnecting`, to land in the delay.
        cancel_on_reconnect: Option<CancellationToken>,
    }

    impl ExecutionEventSink for Recorder {
        fn on_event(&mut self, event: ExecutionEvent) -> bool {
            match event.payload {
                EventPayload::StdoutLine(text) => self.lines.push(text),
                EventPayload::Done => self.done_seen += 1,
                _ => {}
            }
            false
        }

        fn on_reconnecting(&mut self, notice: &ReconnectNotice) {
            self.notices.push(notice.clone());
            if let Some(cancel) = &self.cancel_on_reconnect {
                cancel.cancel();
            }
        }
    }

    async fn follow(
        server: &FakeSse,
        sink: &mut Recorder,
        cancel: &CancellationToken,
        policy: &ReconnectPolicy,
    ) -> awman::engine::remote::FollowReport {
        let client = server.client();
        tokio::time::timeout(
            DEADLINE,
            client.follow_job_events("cmd-1", sink, cancel, policy),
        )
        .await
        .expect("follow returned within the deadline")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unicode_output_split_across_writes_arrives_intact() {
        let sandbox = support::sandbox(
            "engine::remote::follow::unicode_output_split_across_writes_arrives_intact",
        );
        let mut wire = b": keepalive\n\n".to_vec();
        wire.extend(line(1, 0, "héllo — 日本語"));
        wire.extend(b"event: queue_status\ndata: {\"command_id\":\"cmd-1\"}\n\n");
        wire.extend(line(1, 1, "rocket 🚀 ✓"));
        wire.extend(b"id: 1:2\nevent: stdout_line\ndata: {not json\n\n");
        wire.extend(
            String::from_utf8(line(1, 3, "crlf ünïcode"))
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes(),
        );
        wire.extend(done(1, 4));
        // Cut every 3 bytes: multibyte characters and delimiters straddle writes.
        let script = wire.chunks(3).map(|c| send(c.to_vec())).collect();
        let server = FakeSse::start(&sandbox, vec![script]);
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(0),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(
            sink.lines,
            ["héllo — 日本語", "rocket 🚀 ✓", "crlf ünïcode"]
        );
        assert_eq!(sink.done_seen, 1);
        assert_eq!(report.malformed, 1, "the undecodable frame is counted");
        assert_eq!(report.last_cursor, Some(EventCursor::new(1, 4)));
        assert_eq!(report.connections, 1);
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_on_a_comment_only_stream_detaches_without_contacting_the_server() {
        let sandbox = support::sandbox(
            "engine::remote::follow::cancel_on_a_comment_only_stream_detaches_without_contacting_the_server",
        );
        let server = FakeSse::start(
            &sandbox,
            vec![vec![
                send(b": keepalive\n\n".to_vec()),
                send(b": waiting for command progress\n\n".to_vec()),
                Step::Hold,
            ]],
        );
        let client = server.client();
        let cancel = CancellationToken::new();
        let (fire, signal) = tokio::sync::oneshot::channel::<()>();
        let mut sink = Recorder::default();
        let policy = ReconnectPolicy::default();
        let follow = run_cancellable(
            &cancel,
            async {
                let _ = signal.await;
            },
            client.follow_job_events("cmd-1", &mut sink, &cancel, &policy),
        );
        let requests = server.requests.clone();
        let interrupt = async {
            wait_for_async("the follow to connect", DEADLINE, || {
                let connected = !requests.lock().unwrap().is_empty();
                async move { connected.then_some(()) }
            })
            .await;
            let _ = fire.send(());
            Instant::now()
        };
        let (report, fired_at) =
            tokio::time::timeout(DEADLINE, async { tokio::join!(follow, interrupt) })
                .await
                .expect("follow returned within the deadline");

        assert!(
            matches!(report.end, FollowEnd::Detached),
            "{:?}",
            report.end
        );
        assert!(
            fired_at.elapsed() < Duration::from_secs(2),
            "detached {:?} after the interrupt",
            fired_at.elapsed()
        );
        assert!(sink.lines.is_empty() && sink.notices.is_empty());
        assert_eq!(report.connections, 1);
        let closed = server.client_closed.clone();
        wait_for_async("the server to see the client close", DEADLINE, || {
            let n = closed.load(Ordering::SeqCst);
            async move { (n == 1).then_some(()) }
        })
        .await;
        assert_eq!(server.requests().len(), 1, "no cancel request was sent");
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_cancellable_drops_the_signal_listener_when_the_work_ends() {
        let sandbox = support::sandbox(
            "engine::remote::follow::run_cancellable_drops_the_signal_listener_when_the_work_ends",
        );
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let guard = Guard(dropped.clone());
        let value = run_cancellable(
            &cancel,
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            },
            async { 7 },
        )
        .await;
        assert_eq!(value, 7);
        assert!(dropped.load(Ordering::SeqCst), "listener outlived the work");
        assert!(!cancel.is_cancelled());
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_during_the_reconnect_delay_detaches_promptly() {
        let sandbox = support::sandbox(
            "engine::remote::follow::cancel_during_the_reconnect_delay_detaches_promptly",
        );
        let server = FakeSse::start(&sandbox, vec![vec![send(line(1, 0, "a"))]]);
        let cancel = CancellationToken::new();
        let mut sink = Recorder {
            cancel_on_reconnect: Some(cancel.clone()),
            ..Recorder::default()
        };
        let policy = ReconnectPolicy {
            delays: vec![Duration::from_secs(600)],
        };
        let started = Instant::now();
        let report = follow(&server, &mut sink, &cancel, &policy).await;

        assert!(
            matches!(report.end, FollowEnd::Detached),
            "{:?}",
            report.end
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(sink.lines, ["a"]);
        assert_eq!(sink.notices.len(), 1);
        assert_eq!(report.connections, 1, "no reconnect after cancellation");
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_mid_frame_resumes_from_the_last_delivered_cursor_without_duplicates() {
        let sandbox = support::sandbox(
            "engine::remote::follow::eof_mid_frame_resumes_from_the_last_delivered_cursor_without_duplicates",
        );
        let mut partial = line(1, 2, "c");
        partial.truncate(partial.len() / 2);
        let server = FakeSse::start(
            &sandbox,
            vec![
                vec![send(line(1, 0, "a")), send(line(1, 1, "b")), send(partial)],
                // A replay that repeats an already-delivered event.
                vec![
                    send(line(1, 1, "b")),
                    send(line(1, 2, "c")),
                    send(line(1, 3, "d")),
                    send(done(1, 4)),
                ],
            ],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(5),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(sink.lines, ["a", "b", "c", "d"], "continuous, no duplicate");
        assert_eq!(server.resume_ids(), [None, Some("1:1".to_string())]);
        assert_eq!(sink.notices.len(), 1);
        assert!(
            sink.notices[0].reason.contains("mid-frame"),
            "{}",
            sink.notices[0].reason
        );
        assert_eq!(sink.notices[0].resume_from, Some(EventCursor::new(1, 1)));
        assert_eq!(report.connections, 2);
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_after_done_ends_the_follow_without_reconnecting() {
        let sandbox = support::sandbox(
            "engine::remote::follow::eof_after_done_ends_the_follow_without_reconnecting",
        );
        let server = FakeSse::start(
            &sandbox,
            vec![vec![send(line(1, 0, "a")), send(done(1, 1))]],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(5),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(sink.lines, ["a"]);
        assert!(sink.notices.is_empty());
        assert_eq!(server.requests().len(), 1);
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replay_restarted_at_a_new_attempt_is_delivered_in_full() {
        let sandbox = support::sandbox(
            "engine::remote::follow::a_replay_restarted_at_a_new_attempt_is_delivered_in_full",
        );
        // The command was re-queued: the server replays attempt 2 from its
        // start (`ReplayFrom::StartOfAttempt`), whose sequence restarts at 0.
        let server = FakeSse::start(
            &sandbox,
            vec![
                vec![send(line(1, 0, "a1")), send(line(1, 1, "b1"))],
                vec![
                    send(b": resuming from the start of attempt 2\n\n".to_vec()),
                    send(line(2, 0, "a2")),
                    send(line(2, 1, "b2")),
                    send(done(2, 2)),
                ],
            ],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(5),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(sink.lines, ["a1", "b1", "a2", "b2"]);
        assert_eq!(server.resume_ids(), [None, Some("1:1".to_string())]);
        assert_eq!(report.last_cursor, Some(EventCursor::new(2, 2)));
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replay_behind_the_cursor_is_suppressed_and_its_done_still_ends_the_follow() {
        let sandbox = support::sandbox(
            "engine::remote::follow::a_replay_behind_the_cursor_is_suppressed_and_its_done_still_ends_the_follow",
        );
        // The server no longer knows attempt 2 and replays attempt 1 from
        // its start: every event is at or before the client's cursor.
        let server = FakeSse::start(
            &sandbox,
            vec![
                vec![send(line(2, 0, "x")), send(line(2, 1, "y"))],
                vec![send(line(1, 0, "old")), send(done(1, 1))],
            ],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(5),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(sink.lines, ["x", "y"]);
        assert_eq!(server.resume_ids(), [None, Some("2:1".to_string())]);
        assert_eq!(report.last_cursor, Some(EventCursor::new(2, 1)));
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_eof_without_progress_exhausts_the_budget_as_incomplete() {
        let sandbox = support::sandbox(
            "engine::remote::follow::repeated_eof_without_progress_exhausts_the_budget_as_incomplete",
        );
        let keepalive_then_close = || vec![send(b": keepalive\n\n".to_vec())];
        let server = FakeSse::start(
            &sandbox,
            vec![
                vec![send(line(1, 0, "a"))],
                keepalive_then_close(),
                keepalive_then_close(),
                keepalive_then_close(),
            ],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(3),
        )
        .await;

        match &report.end {
            FollowEnd::Incomplete { reconnects, .. } => assert_eq!(*reconnects, 3),
            other => panic!("{other:?}"),
        }
        assert_eq!(sink.lines, ["a"]);
        assert_eq!(
            sink.notices.iter().map(|n| n.attempt).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(report.connections, 4);
        assert_eq!(
            server.resume_ids(),
            [
                None,
                Some("1:0".into()),
                Some("1:0".into()),
                Some("1:0".into())
            ]
        );
        server.assert_only_event_requests();
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_that_delivers_progress_resets_the_reconnect_budget() {
        let sandbox = support::sandbox(
            "engine::remote::follow::a_connection_that_delivers_progress_resets_the_reconnect_budget",
        );
        let server = FakeSse::start(
            &sandbox,
            vec![
                vec![send(line(1, 0, "a"))],
                vec![send(line(1, 1, "b"))],
                vec![send(line(1, 2, "c"))],
                vec![send(done(1, 3))],
            ],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(1),
        )
        .await;

        assert!(matches!(report.end, FollowEnd::Done), "{:?}", report.end);
        assert_eq!(sink.lines, ["a", "b", "c"]);
        assert_eq!(report.connections, 4);
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_oversized_frame_is_a_protocol_failure_that_is_not_retried() {
        let sandbox = support::sandbox(
            "engine::remote::follow::an_oversized_frame_is_a_protocol_failure_that_is_not_retried",
        );
        let mut huge = b"data: ".to_vec();
        huge.resize(MAX_SSE_FRAME_BYTES + 1024, b'a');
        let server = FakeSse::start(
            &sandbox,
            vec![vec![send(line(1, 0, "a")), send(huge), Step::Hold]],
        );
        let mut sink = Recorder::default();
        let report = follow(
            &server,
            &mut sink,
            &CancellationToken::new(),
            &fast_policy(5),
        )
        .await;

        assert!(
            matches!(
                report.end,
                FollowEnd::Failed(EngineError::RemoteProtocol(_))
            ),
            "{:?}",
            report.end
        );
        assert_eq!(sink.lines, ["a"]);
        assert!(sink.notices.is_empty(), "not retried");
        assert_eq!(report.connections, 1);
        drop(server);
        sandbox.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_error_status_is_not_retried() {
        let sandbox =
            support::sandbox("engine::remote::follow::a_client_error_status_is_not_retried");
        let listener = sandbox.ports().tcp_listener();
        let server = wiremock::MockServer::builder()
            .listener(listener)
            .start()
            .await;
        wiremock::Mock::given(wiremock::matchers::path(EVENTS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let mut sink = Recorder::default();
        let report = tokio::time::timeout(
            DEADLINE,
            client.follow_job_events(
                "cmd-1",
                &mut sink,
                &CancellationToken::new(),
                &fast_policy(5),
            ),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                report.end,
                FollowEnd::Failed(EngineError::RemoteHttpStatus { status: 404, .. })
            ),
            "{:?}",
            report.end
        );
        assert_eq!(report.connections, 1);
        server.verify().await;
        drop(server);
        sandbox.finish();
    }
}

// `remote exec --follow` at the command boundary: an interrupt detaches and
// the command returns at once. It must not issue a fresh status lookup, which
// a server that accepts but never answers would hold for the full request
// timeout.
mod command_detach {
    use crate::support;
    use awman::command::commands::remote::{
        RemoteCommand, RemoteCommandFrontend, RemoteExecPromptFlags, RemoteOutcome,
        RemoteSubcommand,
    };
    use awman::command::commands::Command;
    use awman::command::dispatch::Engines;
    use awman::data::message::{UserMessage, UserMessageSink};
    use awman::data::session::{Session, SessionOpenOptions};
    use awman_test_support::fixtures::ConfigTree;
    use awman_test_support::wait::wait_for_async;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    const DEADLINE: Duration = Duration::from_secs(10);
    const SUBMIT_PATH: &str = "/v1/worker/commands";
    const EVENTS_PATH: &str = "/v1/worker/commands/cmd-1/events";

    #[derive(Default, Clone)]
    struct Messages(Arc<Mutex<Vec<String>>>);
    impl UserMessageSink for Messages {
        fn write_message(&mut self, msg: UserMessage) {
            self.0.lock().unwrap().push(msg.text);
        }
        fn replay_queued(&mut self) {}
    }
    impl RemoteCommandFrontend for Messages {}

    /// Answers the submission, holds the events stream open after a comment,
    /// and accepts every other request without ever answering it.
    fn start_server(
        sandbox: &awman_test_support::TestSandbox,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = sandbox.ports().tcp_listener();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(socket);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let mut body_len = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                body_len = value.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    let mut body = vec![0u8; body_len];
                    if reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    requests
                        .lock()
                        .unwrap()
                        .push((method.clone(), path.clone()));
                    let mut socket = reader.into_inner();
                    let reply: Option<&[u8]> = match (method.as_str(), path.as_str()) {
                        ("POST", SUBMIT_PATH) => Some(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 14\r\nconnection: close\r\n\r\n{\"id\":\"cmd-1\"}",
                        ),
                        ("GET", EVENTS_PATH) => Some(
                            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n: waiting\n\n",
                        ),
                        _ => None,
                    };
                    if let Some(reply) = reply {
                        if socket.write_all(reply).await.is_err() || socket.flush().await.is_err() {
                            return;
                        }
                        if path == SUBMIT_PATH {
                            let _ = socket.shutdown().await;
                            return;
                        }
                    }
                    // Hold the connection (events stream, or an unanswered
                    // status lookup) until the client closes it.
                    let mut buf = [0u8; 64];
                    while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        (format!("http://{addr}"), task)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_interrupted_follow_returns_without_waiting_for_a_status_lookup() {
        let sandbox = support::sandbox(
            "engine::remote::command_detach::an_interrupted_follow_returns_without_waiting_for_a_status_lookup",
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (uri, server) = start_server(&sandbox, requests.clone());
        ConfigTree::new()
            .global(serde_json::json!({
                "remote": {"default": "owned", "targets": {"owned": {"url": uri}}}
            }))
            .write(&sandbox, None);
        let env = support::env_snapshot(&sandbox);
        let session = Session::open_at_directory_root(
            sandbox.work(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let engines = Engines::for_env(env).unwrap();

        // The interrupt fires once the follow has connected to the events route.
        let connected = requests.clone();
        let interrupt = Box::pin(async move {
            wait_for_async("the follow to connect", DEADLINE, || {
                let seen = connected
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, path)| path == EVENTS_PATH);
                async move { seen.then_some(()) }
            })
            .await;
        });
        let command = RemoteCommand::new(
            RemoteSubcommand::ExecPrompt(RemoteExecPromptFlags {
                prompt: Some("hello".into()),
                remote_addr: None,
                session: Some("sess-1".into()),
                follow: true,
                api_key: None,
                forwarded: Vec::new(),
            }),
            engines,
            session,
        )
        .with_follow_interrupt(interrupt);
        let messages = Messages::default();
        let outcome = tokio::time::timeout(
            DEADLINE,
            command.run_with_frontend(Box::new(messages.clone())),
        )
        .await
        .expect("the detached command returned without waiting on the server")
        .unwrap();

        let RemoteOutcome::ExecPrompt(outcome) = outcome else {
            panic!("unexpected outcome {outcome:?}");
        };
        assert_eq!(outcome.command_id, "cmd-1");
        assert!(outcome.followed);
        assert_eq!(outcome.status, None, "status is unknown after detaching");
        assert_eq!(outcome.client_exit_code(), 1);
        assert!(messages
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.starts_with("Follow interrupted.")));
        assert_eq!(
            *requests.lock().unwrap(),
            [
                ("POST".to_string(), SUBMIT_PATH.to_string()),
                ("GET".to_string(), EVENTS_PATH.to_string()),
            ],
            "one submission, one follow; no status lookup or cancel"
        );
        server.abort();
        sandbox.finish();
    }
}
