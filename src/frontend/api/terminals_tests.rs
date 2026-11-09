//! `/v1/terminals` transport: HTTP records and lists, the `awman.terminal.v1`
//! WebSocket, and the input grant over both listeners (WI 0141).

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::data::daemon_state::FeatureName;
use crate::data::execution_event::{EventPayload, EventSource, EventSourcePhase, ExecutionEvent};
use crate::data::ids::{CommandId, TabId};
use crate::data::terminal::{TerminalOwner, TerminalOwnerInfo};
use crate::engine::terminal_hub::{TerminalHandle, TerminalInput, TerminalRegistration};
use crate::frontend::api::routes::{build_router_for_listener, ListenerPolicy};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Daemon {
    _tmp: tempfile::TempDir,
    api: Arc<DaemonApi>,
    key: Option<String>,
    addr: SocketAddr,
}

fn config(home: &std::path::Path, remote_input: bool) -> crate::data::config::EffectiveConfig {
    use crate::data::config::{ConfigFile, ConfigLayers, EffectiveConfig, FileKind};
    EffectiveConfig::resolve(
        ConfigLayers {
            global: ConfigFile::from_value(
                serde_json::json!({"daemon": {
                    "features": {"worker": {"enabled": true}},
                    "terminal": {"remoteInput": remote_input},
                }}),
                FileKind::Global,
            )
            .unwrap(),
            ..Default::default()
        },
        Default::default(),
        crate::data::config::env::EnvSnapshot::with_overrides([(
            "AWMAN_HOME",
            home.to_string_lossy().as_ref(),
        )]),
    )
    .unwrap()
}

async fn daemon(policy: ListenerPolicy, remote_input: bool) -> Daemon {
    let tmp = tempfile::tempdir().unwrap();
    let state = crate::frontend::api::routes::tests::make_test_state(tmp.path());
    *state.live_config.write().unwrap() = Some(config(tmp.path(), remote_input));
    let api = Arc::new(DaemonApi::for_worker_tests(state));
    let key = matches!(policy, ListenerPolicy::Network).then(|| {
        api.runtime
            .keys()
            .create("viewer")
            .unwrap()
            .1
            .as_str()
            .to_owned()
    });
    let mut router = build_router_for_listener(api.clone(), policy);
    if key.is_none() {
        router = router.layer(axum::Extension(
            crate::frontend::api::local::OwnerAuthenticated,
        ));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Daemon {
        _tmp: tmp,
        api,
        key,
        addr,
    }
}

impl Daemon {
    fn register(
        &self,
        owner: TerminalOwner,
        allow_remote_input: bool,
    ) -> (TerminalHandle, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let (stdin, rx) = crate::engine::terminal_hub::input_channel();
        let handle = self.api.terminals.hub().register(
            TerminalRegistration {
                owner: TerminalOwnerInfo {
                    owner,
                    feature: FeatureName::Worker,
                    source: Some(EventSource::step(EventSourcePhase::Main, "build", 1)),
                    attempt: 1,
                    allow_remote_input,
                },
                agent: "claude".into(),
                model: Some("opus".into()),
                cols: 40,
                rows: 10,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        (handle, rx)
    }

    /// A live event bus for `cmd`, as the queue worker keeps while it runs.
    async fn bus(&self, cmd: &CommandId) -> tokio::sync::broadcast::Receiver<ExecutionEvent> {
        let bus = Arc::new(EventBus::new(64));
        let rx = bus.subscribe();
        self.api
            .runtime
            .event_buses()
            .lock()
            .await
            .insert(cmd.as_str().to_owned(), bus);
        rx
    }

    async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let mut request = reqwest::Client::new().get(format!("http://{}{path}", self.addr));
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or_default())
    }

    async fn connect_with(
        &self,
        id: &TerminalId,
        actor: Option<&str>,
        query: &str,
    ) -> Result<Ws, u16> {
        let mut request = format!("ws://{}/v1/terminals/{id}{query}", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(
            "sec-websocket-protocol",
            TERMINAL_SUBPROTOCOL.parse().unwrap(),
        );
        if let Some(key) = &self.key {
            headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        }
        if let Some(actor) = actor {
            headers.insert(ACTOR_HEADER, actor.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, response)) => {
                assert_eq!(
                    response.headers()["sec-websocket-protocol"],
                    TERMINAL_SUBPROTOCOL
                );
                Ok(ws)
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(other) => panic!("connect failed: {other}"),
        }
    }

    async fn connect(&self, id: &TerminalId, actor: Option<&str>) -> Ws {
        self.connect_with(id, actor, "").await.unwrap()
    }
}

/// The next text message as JSON (frames and `error` replies).
async fn next_json(ws: &mut Ws) -> serde_json::Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("frame in time")
            .expect("socket open")
            .unwrap();
        match message {
            WsMessage::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("unexpected message {other:?}"),
        }
    }
}

async fn next_frame(ws: &mut Ws) -> TerminalFrame {
    serde_json::from_value(next_json(ws).await).unwrap()
}

async fn send(ws: &mut Ws, frame: TerminalClientFrame) {
    ws.send(WsMessage::Text(
        serde_json::to_string(&frame).unwrap().into(),
    ))
    .await
    .unwrap();
}

fn screen_text(cols: u16, rows: u16, ansi: &[u8]) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(ansi);
    parser.screen().contents()
}

/// The next grant or detach event; lifecycle events (`terminal_opened`) are
/// skipped.
async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<ExecutionEvent>) -> ExecutionEvent {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("event in time")
            .unwrap();
        if !matches!(
            event.payload,
            EventPayload::TerminalOpened { .. } | EventPayload::TerminalClosed { .. }
        ) {
            return event;
        }
    }
}

// ─── HTTP ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_by_owner_pages_with_items_and_next_cursor() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let cmd = CommandId::new();
    let owner = TerminalOwner::Command(cmd.clone());
    let handles: Vec<_> = (0..3).map(|_| d.register(owner.clone(), false)).collect();
    d.register(TerminalOwner::Command(CommandId::new()), false);

    let (status, page) = d
        .get(&format!("/v1/terminals?owner_id={cmd}&limit=2"))
        .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let cursor = page["next_cursor"].as_str().expect("more").to_owned();
    for item in page["items"].as_array().unwrap() {
        assert!(item["id"].as_str().unwrap().starts_with("term_"));
        assert_eq!(item["owner_id"], cmd.as_str());
    }
    let (_, rest) = d
        .get(&format!(
            "/v1/terminals?owner_id={cmd}&limit=2&cursor={cursor}"
        ))
        .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert!(rest["next_cursor"].is_null());
    // Newest first: the first registered is last.
    assert_eq!(rest["items"][0]["id"], handles[0].0.id().as_str());

    let (_, all) = d.get("/v1/terminals").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 4);

    // A finished terminal stays listed while its owner exists.
    handles[1].0.exit(0);
    let (_, after) = d.get(&format!("/v1/terminals?owner_id={cmd}")).await;
    assert_eq!(after["items"].as_array().unwrap().len(), 3);
    assert!(after["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["state"] == "exited" && i["exit_code"] == 0));
}

#[tokio::test]
async fn list_rejects_bad_owner_ids_and_a_task_without_a_run_has_none() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (status, body) = d.get("/v1/terminals?owner_id=nope").await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], "invalid_request");
    let task = crate::data::ids::TaskId::new();
    let (status, body) = d.get(&format!("/v1/terminals?owner_id={task}")).await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({"items": [], "next_cursor": null}));
}

#[tokio::test]
async fn get_returns_the_record_and_old_ids_are_gone() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let cmd = CommandId::new();
    let (handle, _rx) = d.register(TerminalOwner::Command(cmd.clone()), false);
    let (status, record) = d.get(&format!("/v1/terminals/{}", handle.id())).await;
    assert_eq!(status, 200);
    assert_eq!(record["id"], handle.id().as_str());
    assert_eq!(record["owner_id"], cmd.as_str());
    assert_eq!(record["feature"], "worker");
    assert_eq!(record["agent"], "claude");
    assert_eq!(record["model"], "opus");
    assert_eq!(record["attempt"], 1);
    assert_eq!(
        (record["cols"].as_u64(), record["rows"].as_u64()),
        (Some(40), Some(10))
    );
    assert_eq!(record["state"], "open");
    assert_eq!(record["source"]["step_name"], "build");
    assert!(record["created_at"].is_string());

    // An id from an earlier daemon (or one forgotten since) is gone.
    let (status, body) = d.get(&format!("/v1/terminals/{}", TerminalId::new())).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (410, Some("terminal_gone"))
    );
    handle.exit(0);
    d.api.terminals.hub().forget(handle.id());
    let (status, _) = d.get(&format!("/v1/terminals/{}", handle.id())).await;
    assert_eq!(status, 410);
    // Not a terminal id at all.
    let (status, body) = d.get("/v1/terminals/cmd_123").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (404, Some("terminal_not_found"))
    );
}

#[tokio::test]
async fn removed_per_feature_terminal_paths_are_404() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let cmd = CommandId::new();
    let term = TerminalId::new();
    for path in [
        format!("/v1/commands/{cmd}/terminals"),
        format!("/v1/commands/{cmd}/terminals/{term}"),
        format!("/v1/worker/commands/{cmd}/terminals"),
        format!("/v1/engine/terminals/{term}"),
        "/v1/engine/terminals".to_owned(),
    ] {
        let (status, body) = d.get(&path).await;
        assert_eq!(status, 404, "{path}: {body}");
    }
}

#[tokio::test]
async fn network_listener_serves_command_terminals_but_refuses_engine_tabs() {
    let d = daemon(ListenerPolicy::Network, false).await;
    let cmd = CommandId::new();
    let (command_term, _a) = d.register(TerminalOwner::Command(cmd.clone()), false);
    let (tab_term, _b) = d.register(TerminalOwner::Tab(TabId::new()), false);

    let (status, record) = d.get(&format!("/v1/terminals/{}", command_term.id())).await;
    assert_eq!(status, 200, "{record}");
    let (status, body) = d.get(&format!("/v1/terminals/{}", tab_term.id())).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (404, Some("terminal_not_found"))
    );
    let (_, all) = d.get("/v1/terminals").await;
    let ids: Vec<_> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, [command_term.id().as_str()]);
    assert_eq!(
        d.connect_with(tab_term.id(), None, "").await.err(),
        Some(404)
    );
    // The same key scope as the command's events: no key, no terminal.
    let response = reqwest::get(format!("http://{}/v1/terminals", d.addr))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
}

#[tokio::test]
async fn upgrade_requires_the_subprotocol() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let request = format!("ws://{}/v1/terminals/{}", d.addr, handle.id())
        .into_client_request()
        .unwrap();
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 400)
        }
        other => panic!("expected 400, got {:?}", other.map(|_| ())),
    }
}

// ─── WebSocket frames ───────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_on_connect_matches_the_mirror_and_output_streams() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    handle.push_output(b"hello\r\n\x1b[31mred\x1b[0m world");

    // A late joiner sees the current screen, not a blank one.
    let mut ws = d.connect(handle.id(), None).await;
    let TerminalFrame::Snapshot {
        cols, rows, screen, ..
    } = next_frame(&mut ws).await
    else {
        panic!("snapshot first")
    };
    assert_eq!((cols, rows), (40, 10));
    let mirror = d.api.terminals.hub().snapshot(handle.id()).unwrap();
    assert_eq!(screen, mirror.screen);
    let text = screen_text(cols, rows, &screen);
    assert!(
        text.contains("hello") && text.contains("red world"),
        "{text}"
    );

    handle.push_output(b"\r\nmore");
    assert_eq!(
        next_frame(&mut ws).await,
        TerminalFrame::Output {
            bytes: b"\r\nmore".to_vec()
        }
    );
    handle.resize(50, 12);
    assert_eq!(
        next_frame(&mut ws).await,
        TerminalFrame::Resize { cols: 50, rows: 12 }
    );
    handle.exit(7);
    assert_eq!(next_frame(&mut ws).await, TerminalFrame::Exit { code: 7 });
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap();
    assert!(matches!(closed, Some(Ok(WsMessage::Close(_))) | None));
}

#[tokio::test]
async fn frames_are_json_text_with_base64_bytes() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let mut ws = d.connect(handle.id(), None).await;
    let snapshot = next_json(&mut ws).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert!(snapshot["screen"].is_string());
    handle.push_output(b"hi");
    assert_eq!(
        next_json(&mut ws).await,
        serde_json::json!({"type": "output", "bytes": "aGk="})
    );
}

#[tokio::test]
async fn a_slow_consumer_gets_lagged_then_a_fresh_snapshot() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let mut ws = d.connect(handle.id(), None).await;
    assert!(matches!(
        next_frame(&mut ws).await,
        TerminalFrame::Snapshot { .. }
    ));
    // The test runtime is single-threaded: the socket task cannot drain the
    // subscription while this loop runs, so it overflows.
    let capacity = d.api.terminals.hub().config().subscriber_capacity;
    for n in 0..(capacity * 4) {
        handle.push_output(format!("line {n}\r\n").as_bytes());
    }
    let mut saw_lagged = false;
    loop {
        match next_frame(&mut ws).await {
            TerminalFrame::Lagged => saw_lagged = true,
            TerminalFrame::Snapshot {
                cols, rows, screen, ..
            } if saw_lagged => {
                let text = screen_text(cols, rows, &screen);
                assert!(
                    text.contains(&format!("line {}", capacity * 4 - 1)),
                    "{text}"
                );
                break;
            }
            TerminalFrame::Output { .. } if !saw_lagged => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

// ─── Input and the grant ────────────────────────────────────────────────

#[tokio::test]
async fn local_take_control_is_exclusive_and_logged_and_input_reaches_the_pty() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let cmd = CommandId::new();
    let mut events = d.bus(&cmd).await;
    let (handle, mut stdin) = d.register(TerminalOwner::Command(cmd), false);
    let mut first = d.connect(handle.id(), None).await;
    let mut second = d.connect(handle.id(), None).await;
    next_frame(&mut first).await;
    next_frame(&mut second).await;

    // Input before the grant is refused, and the socket stays open.
    send(
        &mut first,
        TerminalClientFrame::Input {
            bytes: b"x".to_vec(),
        },
    )
    .await;
    let refused = next_json(&mut first).await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["code"], "input_not_allowed");

    send(&mut first, TerminalClientFrame::TakeControl).await;
    let event = next_event(&mut events).await;
    assert!(matches!(
        &event.payload,
        EventPayload::TerminalControl { holder, terminal_id, .. }
            if holder == "local" && terminal_id == handle.id()
    ));
    assert_eq!(event.payload.sse_event_type(), "terminal_control");
    assert_eq!(event.source.unwrap().step_name.as_deref(), Some("build"));

    send(&mut second, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut second).await["code"], "control_held");

    send(
        &mut first,
        TerminalClientFrame::Input {
            bytes: b"y\r".to_vec(),
        },
    )
    .await;
    let typed = tokio::time::timeout(std::time::Duration::from_secs(5), stdin.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(typed, b"y\r");
    // Keystrokes are never logged.
    while let Ok(event) = events.try_recv() {
        assert!(
            matches!(event.payload, EventPayload::TerminalOpened { .. }),
            "{:?}",
            event.payload
        );
    }

    // The holder resizes the PTY; viewers see it.
    send(
        &mut first,
        TerminalClientFrame::Resize { cols: 60, rows: 20 },
    )
    .await;
    assert_eq!(
        next_frame(&mut second).await,
        TerminalFrame::Resize { cols: 60, rows: 20 }
    );
    // A non-holder's resize is refused.
    send(
        &mut second,
        TerminalClientFrame::Resize { cols: 10, rows: 5 },
    )
    .await;
    assert_eq!(next_json(&mut second).await["code"], "input_not_allowed");

    // Detaching releases the grant (logged with an empty holder) and logs
    // the local viewer's detach reason.
    send(
        &mut first,
        TerminalClientFrame::Detached {
            reason: DetachReason::User,
        },
    )
    .await;
    let released = next_event(&mut events).await;
    assert!(
        matches!(&released.payload, EventPayload::TerminalControl { holder, .. } if holder.is_empty())
    );
    let detached = next_event(&mut events).await;
    assert!(matches!(
        detached.payload,
        EventPayload::TerminalViewerDetached {
            reason: DetachReason::User,
            ..
        }
    ));
    // The PTY falls back to its registration size.
    assert_eq!(
        next_frame(&mut second).await,
        TerminalFrame::Resize { cols: 40, rows: 10 }
    );
    send(&mut second, TerminalClientFrame::TakeControl).await;
    let event = next_event(&mut events).await;
    assert!(
        matches!(&event.payload, EventPayload::TerminalControl { holder, .. } if holder == "local")
    );
}

#[tokio::test]
async fn network_input_is_refused_unless_both_settings_allow_it() {
    // remoteInput off, allow_remote_input on.
    let d = daemon(ListenerPolicy::Network, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), true);
    let mut ws = d.connect(handle.id(), Some("webapp")).await;
    next_frame(&mut ws).await;
    send(&mut ws, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut ws).await["code"], "input_not_allowed");

    // remoteInput on, allow_remote_input off.
    let d = daemon(ListenerPolicy::Network, true).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let mut ws = d.connect(handle.id(), Some("webapp")).await;
    next_frame(&mut ws).await;
    send(&mut ws, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut ws).await["code"], "input_not_allowed");
}

#[tokio::test]
async fn network_grant_is_named_by_the_actor_header() {
    let d = daemon(ListenerPolicy::Network, true).await;
    let cmd = CommandId::new();
    let mut events = d.bus(&cmd).await;
    let (handle, mut stdin) = d.register(TerminalOwner::Command(cmd), true);
    let mut alice = d.connect(handle.id(), Some("alice")).await;
    let mut bob = d.connect(handle.id(), Some("bob")).await;
    next_frame(&mut alice).await;
    next_frame(&mut bob).await;
    send(&mut alice, TerminalClientFrame::TakeControl).await;
    let event = next_event(&mut events).await;
    assert!(
        matches!(&event.payload, EventPayload::TerminalControl { holder, .. } if holder == "alice")
    );
    send(&mut bob, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut bob).await["code"], "control_held");
    send(
        &mut alice,
        TerminalClientFrame::Input {
            bytes: b"ok".to_vec(),
        },
    )
    .await;
    assert_eq!(stdin.recv().await.unwrap(), b"ok");

    // Without x-awman-actor a network client cannot be named, so it never
    // gets the grant.
    let mut anonymous = d.connect(handle.id(), None).await;
    next_frame(&mut anonymous).await;
    send(&mut anonymous, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut anonymous).await["code"], "input_not_allowed");

    // Closing the socket without `detached` releases the grant.
    drop(alice);
    let released = next_event(&mut events).await;
    assert!(
        matches!(&released.payload, EventPayload::TerminalControl { holder, .. } if holder.is_empty())
    );
    send(&mut bob, TerminalClientFrame::TakeControl).await;
    let event = next_event(&mut events).await;
    assert!(
        matches!(&event.payload, EventPayload::TerminalControl { holder, .. } if holder == "bob")
    );
}

#[tokio::test]
async fn read_only_viewers_never_get_the_grant() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let mut ws = d
        .connect_with(handle.id(), None, "?read_only=true")
        .await
        .unwrap();
    next_frame(&mut ws).await;
    send(&mut ws, TerminalClientFrame::TakeControl).await;
    assert_eq!(next_json(&mut ws).await["code"], "input_not_allowed");
}

#[tokio::test]
async fn malformed_client_frames_close_the_socket() {
    let d = daemon(ListenerPolicy::Local, false).await;
    let (handle, _rx) = d.register(TerminalOwner::Command(CommandId::new()), false);
    let mut ws = d.connect(handle.id(), None).await;
    next_frame(&mut ws).await;
    ws.send(WsMessage::Text(r#"{"type":"shell"}"#.into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap();
    match message {
        Some(Ok(WsMessage::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1007),
        other => panic!("expected a close frame, got {other:?}"),
    }
}

#[test]
fn refusals_map_onto_the_error_table() {
    let cases = [
        (TerminalRefusal::NotFound, "terminal_not_found", 404),
        (TerminalRefusal::NetworkRefused, "terminal_not_found", 404),
        (
            TerminalRefusal::NoTerminal("cmd_x".into()),
            "terminal_not_found",
            404,
        ),
        (TerminalRefusal::Gone, "terminal_gone", 410),
        (TerminalRefusal::Closed, "conflict", 409),
        (TerminalRefusal::InputNotAllowed, "input_not_allowed", 403),
        (TerminalRefusal::ControlHeld, "control_held", 409),
        (
            TerminalRefusal::InvalidOwner("x".into()),
            "invalid_request",
            400,
        ),
    ];
    for (refusal, code, status) in cases {
        let error = refusal_error(&refusal);
        assert_eq!(
            (error.code, error.status.as_u16()),
            (code, status),
            "{refusal:?}"
        );
    }
}

/// Review finding: a valid `namespace/name` annotation must survive the
/// HTTP handshake (a `/` is illegal in a header name) with its logical key
/// and value unchanged, including non-ASCII values.
#[test]
fn annotation_headers_round_trip_through_a_real_header_map() {
    let mut headers = HeaderMap::new();
    for (key, value) in [("rex/block", "blk_1"), ("ui.app/label", "grüße 100%")] {
        let (name, value) = crate::data::terminal::annotation_header(key, value);
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("legal header name"),
            axum::http::HeaderValue::from_str(&value).expect("legal header value"),
        );
    }
    let viewer = viewer_identity(CommandListener::Local, &headers, false).unwrap();
    assert_eq!(viewer.annotations.get("rex/block").unwrap(), "blk_1");
    assert_eq!(
        viewer.annotations.get("ui.app/label").unwrap(),
        "grüße 100%"
    );

    let mut bad = HeaderMap::new();
    bad.insert(
        axum::http::HeaderName::from_static("x-awman-annotation-nonamespace"),
        axum::http::HeaderValue::from_static("x"),
    );
    let error = viewer_identity(CommandListener::Local, &bad, false).unwrap_err();
    assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(error.code, "invalid_annotation");
}
