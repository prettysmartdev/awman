//! Integration tests for `ContainerIo` channel correctness (WI 0081).
//!
//! Verifies that each frontend's `take_container_io()` returns live channels
//! with the correct configuration, and that bytes flow correctly between the
//! engine-side senders/receivers and the frontend's sinks.

use awman::data::execution_event::EventPayload;
use awman::engine::container::frontend::ContainerFrontend;
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman::frontend::api::event_bus::EventBus;
use awman::frontend::cli::CliFrontend;

// ─── helpers ────────────────────────────────────────────────────────────────

fn make_api_frontend(subcommand: &str) -> (ApiDispatchFrontend, EventBus) {
    let bus = EventBus::new(64);
    let fe = ApiDispatchFrontend::new(subcommand, &[], bus.sender());
    (fe, bus)
}

fn make_cli_frontend() -> CliFrontend {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    let cmd = CommandCatalogue::get().build_clap_command();
    let m = cmd
        .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
        .unwrap();
    CliFrontend::new(m)
}

// ─── CLI non-interactive ContainerIo ────────────────────────────────────────

/// `take_container_io()` on the CLI (always non-interactive in test env
/// because stdin is not a TTY) must return live stdout/stderr senders.
#[tokio::test]
async fn take_container_io_cli_non_interactive_stdout_sender_is_live() {
    let mut fe = make_cli_frontend();
    let io = fe.take_container_io();

    assert!(
        !io.stdout.is_closed(),
        "CLI non-interactive stdout sender should be live"
    );
    assert!(
        !io.stderr.is_closed(),
        "CLI non-interactive stderr sender should be live"
    );
}

/// Non-interactive CLI must not provide PTY fields: resize and initial_size
/// must both be `None` so the engine selects the `Stdio::piped()` path.
#[tokio::test]
async fn take_container_io_cli_non_interactive_has_no_pty_fields() {
    let mut fe = make_cli_frontend();
    let io = fe.take_container_io();

    assert!(
        io.resize.is_none(),
        "non-interactive CLI must not have a resize channel"
    );
    assert!(
        io.initial_size.is_none(),
        "non-interactive CLI must not have an initial PTY size"
    );
}

/// After dropping the `stdin_tx` clone returned in `ContainerIo`, the
/// `stdin_rx` receiver yields `None` immediately (EOF), which signals the
/// engine's writer task to close the container's stdin.
#[tokio::test]
async fn take_container_io_cli_non_interactive_stdin_eof_after_drop() {
    let mut fe = make_cli_frontend();
    let io = fe.take_container_io();

    let mut stdin_rx = io.stdin_rx;
    // Drop the returned stdin_tx clone — no more senders → EOF.
    drop(io.stdin_tx);

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("should not time out — EOF should be immediate");

    assert!(
        result.is_none(),
        "stdin_rx must yield None (EOF) once the last stdin sender is dropped"
    );
}

/// Bytes sent to `stdin_tx` are available on `stdin_rx` (channels are wired
/// correctly before EOF arrives).
#[tokio::test]
async fn take_container_io_cli_stdin_tx_sends_reach_stdin_rx() {
    let mut fe = make_cli_frontend();
    let io = fe.take_container_io();

    let payload = b"prompt\n".to_vec();
    io.stdin_tx.send(payload.clone()).unwrap();

    let mut stdin_rx = io.stdin_rx;
    let received = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("recv should not time out")
        .expect("channel should be open");

    assert_eq!(
        received, payload,
        "bytes sent via stdin_tx must arrive at stdin_rx"
    );
}

// ─── API ContainerIo ─────────────────────────────────────────────────────────

/// API `take_container_io()` returns live stdout and stderr senders.
#[tokio::test]
async fn take_container_io_api_stdout_and_stderr_senders_are_live() {
    let (mut fe, _bus) = make_api_frontend("exec workflow");
    let io = fe.take_container_io();

    assert!(!io.stdout.is_closed(), "API stdout sender should be live");
    assert!(!io.stderr.is_closed(), "API stderr sender should be live");
}

/// API is always non-interactive: no resize channel, no initial PTY size.
#[tokio::test]
async fn take_container_io_api_has_no_pty_fields() {
    let (mut fe, _bus) = make_api_frontend("exec workflow");
    let io = fe.take_container_io();

    assert!(io.resize.is_none(), "API must not have a resize channel");
    assert!(
        io.initial_size.is_none(),
        "API must not have an initial PTY size"
    );
}

/// Bytes sent through the API's `stdout` channel must arrive at the event bus
/// as `StdoutLine` events (line-buffered by the drain task).
#[tokio::test]
async fn take_container_io_api_stdout_bytes_arrive_at_event_bus() {
    let (mut fe, bus) = make_api_frontend("exec workflow");
    let mut rx = bus.subscribe();
    let io = fe.take_container_io();

    io.stdout.send(b"hello world\n".to_vec()).unwrap();
    // Close the channel so the drain task flushes.
    drop(io.stdout);

    // Give the drain task time to process.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let event = rx
        .try_recv()
        .expect("should have received a StdoutLine event");
    assert!(
        matches!(&event.payload, EventPayload::StdoutLine(s) if s == "hello world"),
        "stdout bytes must arrive as StdoutLine at the event bus; got {:?}",
        event.payload
    );
}

/// Bytes sent through the API's `stderr` channel must arrive at the event bus
/// as `StderrLine` events.
#[tokio::test]
async fn take_container_io_api_stderr_bytes_arrive_at_event_bus() {
    let (mut fe, bus) = make_api_frontend("exec workflow");
    let mut rx = bus.subscribe();
    let io = fe.take_container_io();

    io.stderr.send(b"error occurred\n".to_vec()).unwrap();
    drop(io.stderr);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let event = rx
        .try_recv()
        .expect("should have received a StderrLine event");
    assert!(
        matches!(&event.payload, EventPayload::StderrLine(s) if s == "error occurred"),
        "stderr bytes must arrive as StderrLine at the event bus; got {:?}",
        event.payload
    );
}

/// Multiple stdout lines in a single byte chunk are split and emitted as
/// separate `StdoutLine` events.
#[tokio::test]
async fn take_container_io_api_multiline_stdout_split_into_separate_events() {
    let (mut fe, bus) = make_api_frontend("exec workflow");
    let mut rx = bus.subscribe();
    let io = fe.take_container_io();

    io.stdout
        .send(b"line one\nline two\nline three\n".to_vec())
        .unwrap();
    drop(io.stdout);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut lines = Vec::new();
    while let Ok(evt) = rx.try_recv() {
        if let EventPayload::StdoutLine(s) = evt.payload {
            lines.push(s);
        }
    }

    assert_eq!(
        lines,
        vec!["line one", "line two", "line three"],
        "each newline-delimited chunk must become a separate StdoutLine event"
    );
}

/// API `stdin_rx` yields `None` immediately after the caller drops `stdin_tx`,
/// because the frontend itself already dropped the original sender.
#[tokio::test]
async fn take_container_io_api_stdin_eof_after_drop() {
    let (mut fe, _bus) = make_api_frontend("exec workflow");
    let io = fe.take_container_io();

    let mut stdin_rx = io.stdin_rx;
    // The API frontend drops the original sender; dropping the returned clone
    // closes the channel.
    drop(io.stdin_tx);

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("EOF should be immediate — no timeout");

    assert!(
        result.is_none(),
        "API stdin_rx must yield None (EOF) once all senders are dropped"
    );
}

// ─── Engine-side EOF semantics (piped path) ─────────────────────────────────

/// Simulates the piped path: frontend constructs ContainerIo, engine seeds
/// the prompt via `io.stdin_tx`, then drops `io.stdin_tx`. The writer task
/// (modeled here by draining `stdin_rx`) must see the seeded bytes followed
/// by `None` (EOF). This is the behaviour that `spawn_piped_docker` /
/// `spawn_piped_apple` rely on so the child container's stdin pipe closes
/// promptly after the seeded prompt is delivered.
#[tokio::test]
async fn engine_piped_path_writer_sees_eof_after_seed_drop_api() {
    let (mut fe, _bus) = make_api_frontend("exec workflow");
    let io = fe.take_container_io();

    let mut stdin_rx = io.stdin_rx;
    io.stdin_tx.send(b"seeded prompt".to_vec()).unwrap();
    io.stdin_tx.send(b"\n".to_vec()).unwrap();
    // Engine drops its sender — matches `drop(bridge.stdin_injector)` in
    // `spawn_piped_docker` / `spawn_piped_apple`.
    drop(io.stdin_tx);

    let first = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("seed bytes should arrive promptly")
        .expect("seed bytes were sent before drop");
    assert_eq!(first, b"seeded prompt");

    let newline = stdin_rx.recv().await.expect("newline was queued");
    assert_eq!(newline, b"\n");

    let eof = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("EOF should arrive promptly after final sender drop");
    assert!(
        eof.is_none(),
        "writer task must see None (EOF) once the engine drops the seed sender"
    );
}

/// Same engine-side EOF semantic, exercised through the CLI non-interactive
/// path. With the simplified frontend pattern (no spurious clone/drop), the
/// caller holds the only sender — dropping it closes the channel for the
/// writer task.
#[tokio::test]
async fn engine_piped_path_writer_sees_eof_after_seed_drop_cli() {
    let mut fe = make_cli_frontend();
    let io = fe.take_container_io();

    let mut stdin_rx = io.stdin_rx;
    io.stdin_tx.send(b"prompt\n".to_vec()).unwrap();
    drop(io.stdin_tx);

    let first = stdin_rx.recv().await.expect("seed was sent");
    assert_eq!(first, b"prompt\n");

    let eof = tokio::time::timeout(std::time::Duration::from_millis(100), stdin_rx.recv())
        .await
        .expect("EOF should be prompt");
    assert!(
        eof.is_none(),
        "writer task must see EOF after engine drops seed sender"
    );
}
