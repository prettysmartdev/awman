//! Attach-socket coverage beyond the inline round-trip tests: file modes and
//! cleanup, concurrent clients, a malformed client, the client reader's
//! handling of hostile or truncated server streams, and the execution
//! backend's stdin injection and cancel handle.
//!
//! Every wait is event-driven (a channel the server hook signals, or the
//! socket itself) with a generous `timeout` only as a hang guard; nothing
//! sleeps.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::execution::{AgentExecution, AgentInstance};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::container::attach_protocol::{read_frame, resize_payload, write_frame, Tag};

const GUARD: Duration = Duration::from_secs(10);

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(GUARD, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

struct Server {
    output: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    stdin_rx: UnboundedReceiver<Vec<u8>>,
    resize_rx: UnboundedReceiver<(u16, u16)>,
    guard: Option<AttachSocketGuard>,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

fn start(dir: tempfile::TempDir, path: PathBuf) -> Server {
    let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(64);
    let output = Arc::new(output_tx);
    let (stdin_tx, stdin_rx) = unbounded_channel();
    let (resize_tx, resize_rx) = unbounded_channel();
    let guard = spawn_attach_socket_server(
        &path,
        AttachHooks {
            terminal: None,
            output: Arc::clone(&output),
            stdin: stdin_tx,
            resize: Arc::new(move |cols, rows| {
                let _ = resize_tx.send((cols, rows));
            }),
        },
    )
    .expect("server must bind");
    Server {
        output,
        stdin_rx,
        resize_rx,
        guard: Some(guard),
        path,
        _dir: dir,
    }
}

fn start_default() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attach").join("agent.sock");
    start(dir, path)
}

/// Connect a raw client and wait until the server has subscribed it: the
/// server subscribes on accept, before reading the client's first frame, so
/// a resize observed by the hook proves the subscription exists.
async fn raw_client(server: &mut Server, cols: u16) -> tokio::net::UnixStream {
    let mut stream = tokio::net::UnixStream::connect(&server.path).await.unwrap();
    write_frame(&mut stream, Tag::Resize, &resize_payload(cols, 1))
        .await
        .unwrap();
    let seen = within("the sync resize", server.resize_rx.recv()).await;
    assert_eq!(seen, Some((cols, 1)));
    stream
}

// ─── frontend fixture ───────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Captured {
    stdout: Arc<Mutex<Option<UnboundedReceiver<Vec<u8>>>>>,
    stdin_tx: Arc<Mutex<Option<UnboundedSender<Vec<u8>>>>>,
}

struct Frontend {
    captured: Captured,
    initial_size: Option<(u16, u16)>,
}

impl UserMessageSink for Frontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for Frontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, stdout_rx) = unbounded_channel::<Vec<u8>>();
        let (stderr_tx, _stderr_rx) = unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = unbounded_channel::<Vec<u8>>();
        *self.captured.stdout.lock().unwrap() = Some(stdout_rx);
        *self.captured.stdin_tx.lock().unwrap() = Some(stdin_tx.clone());
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: self.initial_size,
        }
    }
}

fn attach(path: &Path, initial_size: Option<(u16, u16)>) -> (AgentExecution, Captured) {
    let captured = Captured::default();
    let execution = Box::new(SocketAttachInstance {
        handle: AgentHandle {
            id: "awman-t-1".into(),
            name: "awman-t-1".into(),
            image_tag: "img".into(),
            started_at: chrono::Utc::now(),
        },
        path: path.to_path_buf(),
    })
    .run_with_frontend(Box::new(Frontend {
        captured: captured.clone(),
        initial_size,
    }))
    .expect("attach must connect");
    (execution, captured)
}

/// A one-shot fake server: accepts a single client, writes `bytes`, then
/// closes (or holds the connection open when `hold` is set).
fn fake_server(bytes: Vec<u8>, hold: bool) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        if hold {
            std::future::pending::<()>().await;
        }
    });
    (dir, path)
}

fn frame(tag: Tag, payload: &[u8]) -> Vec<u8> {
    crate::engine::container::attach_protocol::encode_frame(tag, payload)
}

// ─── server ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn server_files_are_owner_only_and_removed_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("attach");
    std::fs::create_dir(&parent).unwrap();
    // A predecessor left the directory looser: it is re-tightened.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = parent.join("x.sock");
    let mut server = start(dir, path.clone());

    assert_eq!(mode(&parent), 0o700);
    assert_eq!(mode(&path), 0o600);
    let owner = path.with_extension("owner");
    assert_eq!(mode(&owner), 0o600);
    assert_eq!(
        std::fs::read_to_string(&owner).unwrap(),
        std::process::id().to_string()
    );

    drop(server.guard.take());
    assert!(!path.exists());
    assert!(!owner.exists());
}

#[tokio::test]
async fn server_creates_a_missing_parent_owner_only() {
    let server = start_default();
    assert_eq!(mode(server.path.parent().unwrap()), 0o700);
}

#[tokio::test]
async fn concurrent_clients_share_output_and_merge_stdin() {
    let mut server = start_default();
    let mut a = raw_client(&mut server, 10).await;
    let mut b = raw_client(&mut server, 20).await;
    assert_eq!(server.output.receiver_count(), 2);

    server.output.send(b"frame-1".to_vec()).unwrap();
    for stream in [&mut a, &mut b] {
        assert_eq!(
            within("broadcast output", read_frame(stream))
                .await
                .unwrap(),
            (Tag::Output, b"frame-1".to_vec())
        );
    }

    write_frame(&mut a, Tag::Stdin, b"from-a").await.unwrap();
    write_frame(&mut b, Tag::Stdin, b"from-b").await.unwrap();
    let mut got = vec![
        within("stdin a/b", server.stdin_rx.recv()).await.unwrap(),
        within("stdin a/b", server.stdin_rx.recv()).await.unwrap(),
    ];
    got.sort();
    assert_eq!(got, vec![b"from-a".to_vec(), b"from-b".to_vec()]);

    // One client leaving does not affect the other.
    drop(a);
    server.output.send(b"frame-2".to_vec()).unwrap();
    assert_eq!(
        within("output after a left", read_frame(&mut b))
            .await
            .unwrap(),
        (Tag::Output, b"frame-2".to_vec())
    );
}

#[tokio::test]
async fn a_malformed_resize_drops_only_that_client_and_never_resizes() {
    let mut server = start_default();
    let mut good = raw_client(&mut server, 30).await;

    let mut bad = tokio::net::UnixStream::connect(&server.path).await.unwrap();
    write_frame(&mut bad, Tag::Resize, &[1, 2, 3])
        .await
        .unwrap();
    // The server hangs up on the bad client.
    assert!(within("bad client drop", read_frame(&mut bad))
        .await
        .is_err());

    // The accept loop and the good client keep working, and the bad resize
    // never reached the PTY.
    write_frame(&mut good, Tag::Resize, &resize_payload(31, 2))
        .await
        .unwrap();
    assert_eq!(
        within("good resize", server.resize_rx.recv()).await,
        Some((31, 2))
    );
    let _late = raw_client(&mut server, 32).await;
}

#[tokio::test]
async fn an_unknown_tag_from_a_client_drops_it() {
    let mut server = start_default();
    let mut stream = tokio::net::UnixStream::connect(&server.path).await.unwrap();
    stream.write_all(&[0x7f, 0, 0, 0, 0]).await.unwrap();
    assert!(within("drop", read_frame(&mut stream)).await.is_err());
    let _still_serving = raw_client(&mut server, 40).await;
}

// ─── client reader ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_server_sending_a_non_output_tag_is_an_invalid_frame() {
    let mut bytes = frame(Tag::Output, b"hello");
    bytes.extend(frame(Tag::Stdin, b"nope"));
    let (_dir, path) = fake_server(bytes, true);
    let (mut execution, captured) = attach(&path, None);
    let err = within("wait", execution.wait()).await.unwrap_err();
    assert!(
        err.to_string().contains("invalid attach output frame"),
        "{err}"
    );
    let mut rx = captured.stdout.lock().unwrap().take().unwrap();
    assert_eq!(rx.recv().await.unwrap(), b"hello");
}

#[tokio::test]
async fn garbage_from_the_server_is_a_protocol_violation() {
    let (_dir, path) = fake_server(vec![0xee, 1, 0, 0, 0, 0], true);
    let (mut execution, _) = attach(&path, None);
    let err = within("wait", execution.wait()).await.unwrap_err();
    assert!(
        err.to_string().contains("invalid attach output frame"),
        "{err}"
    );
}

#[tokio::test]
async fn an_oversized_frame_header_is_a_protocol_violation() {
    let mut bytes = vec![Tag::Output as u8];
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    let (_dir, path) = fake_server(bytes, true);
    let (mut execution, _) = attach(&path, None);
    let err = within("wait", execution.wait()).await.unwrap_err();
    assert!(
        err.to_string().contains("invalid attach output frame"),
        "{err}"
    );
}

#[tokio::test]
async fn a_truncated_frame_is_a_closed_endpoint_not_a_violation() {
    // Header promises 10 bytes, only 3 arrive, then EOF.
    let mut bytes = vec![Tag::Output as u8];
    bytes.extend_from_slice(&10u32.to_le_bytes());
    bytes.extend_from_slice(b"abc");
    let (_dir, path) = fake_server(bytes, false);
    let (mut execution, _) = attach(&path, None);
    let err = within("wait", execution.wait()).await.unwrap_err();
    assert!(err.to_string().contains("without an exit status"), "{err}");
}

#[tokio::test]
async fn a_lagged_frame_from_the_server_ends_the_session_with_a_reason() {
    let (_dir, path) = fake_server(frame(Tag::Lagged, &[]), true);
    let (mut execution, _) = attach(&path, None);
    let err = within("wait", execution.wait()).await.unwrap_err();
    assert!(err.to_string().contains("lagged"), "{err}");
}

// ─── execution backend ──────────────────────────────────────────────────────

#[tokio::test]
async fn injected_stdin_reaches_the_server_and_no_resize_without_a_size() {
    let mut server = start_default();
    let (mut execution, _) = attach(&server.path, None);
    assert!(execution.try_inject_stdin(b"next prompt\r").unwrap());
    assert_eq!(
        within("injected stdin", server.stdin_rx.recv())
            .await
            .unwrap(),
        b"next prompt\r"
    );
    // No initial size: no resize was ever sent.
    assert!(server.resize_rx.try_recv().is_err());

    // A standalone cancel handle ends this session cleanly.
    let cancel = execution.cancel_handle().unwrap();
    cancel.cancel().unwrap();
    let info = within("wait after cancel", execution.wait()).await.unwrap();
    assert_eq!(info.exit_code, 0);
    // The server (and the container it stands for) keeps serving.
    assert!(server.path.exists());
    let _again = raw_client(&mut server, 50).await;
}

#[tokio::test]
async fn frontend_stdin_is_forwarded_after_the_initial_resize() {
    let mut server = start_default();
    let (mut execution, captured) = attach(&server.path, Some((120, 40)));
    assert_eq!(
        within("initial resize", server.resize_rx.recv()).await,
        Some((120, 40))
    );
    let stdin_tx = captured.stdin_tx.lock().unwrap().clone().unwrap();
    stdin_tx.send(b"k".to_vec()).unwrap();
    assert_eq!(
        within("frontend stdin", server.stdin_rx.recv())
            .await
            .unwrap(),
        b"k"
    );
    execution.cancel().unwrap();
    within("wait", execution.wait()).await.unwrap();
}

#[test]
fn handle_preview_reports_the_target_container() {
    let instance = SocketAttachInstance {
        handle: AgentHandle {
            id: "id-1".into(),
            name: "name-1".into(),
            image_tag: "tag-1".into(),
            started_at: chrono::Utc::now(),
        },
        path: PathBuf::from("/nonexistent.sock"),
    };
    let preview = instance.handle_preview();
    assert_eq!(preview.id, "id-1");
    assert_eq!(preview.name, "name-1");
    assert_eq!(preview.image, "tag-1");
}

#[tokio::test]
async fn binary_attach_uses_the_hub_snapshot_and_exclusive_input_capability() {
    use crate::data::{daemon_state::FeatureName, ids::*, terminal::*};
    use crate::engine::terminal_hub::*;
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (stdin, mut input) = input_channel();
    let terminal = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(CommandId::new()),
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
    terminal.push_output(b"existing screen");
    let (socket, mut viewer) = tokio::net::UnixStream::pair().unwrap();
    let task = tokio::spawn(serve_hub_client(socket, hub.clone(), terminal.id().clone()));
    let (tag, screen) = within("initial snapshot", read_frame(&mut viewer))
        .await
        .unwrap();
    assert_eq!(tag, Tag::Output);
    assert!(String::from_utf8_lossy(&screen).contains("existing screen"));
    assert!(matches!(
        hub.take_control(terminal.id()),
        Err(TerminalHubError::ControlHeld)
    ));
    write_frame(&mut viewer, Tag::Stdin, b"input")
        .await
        .unwrap();
    assert_eq!(
        within("bounded input", input.recv()).await.unwrap(),
        b"input"
    );
    write_frame(&mut viewer, Tag::Resize, &resize_payload(100, 30))
        .await
        .unwrap();
    terminal.exit(9);
    let (tag, code) = within("exit frame", read_frame(&mut viewer)).await.unwrap();
    assert_eq!((tag, code), (Tag::Exit, 9_i32.to_le_bytes().to_vec()));
    within("attach task teardown", task).await.unwrap();
}

#[tokio::test]
async fn the_socket_runtime_client_preserves_the_hub_exit_code_and_pending_output() {
    let mut bytes = frame(Tag::Output, b"hello\x1b");
    bytes.extend(frame(Tag::Exit, &7_i32.to_le_bytes()));
    let (_dir, path) = fake_server(bytes, false);
    let (mut execution, captured) = attach(&path, None);
    assert_eq!(
        within("terminal exit", execution.wait())
            .await
            .unwrap()
            .exit_code,
        7
    );
    let mut output = captured.stdout.lock().unwrap().take().unwrap();
    assert_eq!(output.recv().await.unwrap(), b"hello");
    assert_eq!(output.recv().await.unwrap(), b"\x1b");
}

#[tokio::test]
async fn hub_clients_drain_the_terminal_exit_after_the_listener_guard_closes() {
    use crate::data::{daemon_state::FeatureName, ids::CommandId, terminal::*};
    use crate::engine::terminal_hub::*;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.sock");
    let hub = Arc::new(TerminalHub::new(Default::default()));
    let (stdin, _input) = input_channel();
    let terminal = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(CommandId::new()),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "test".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    terminal.push_output(b"existing screen");
    let (output, _) = tokio::sync::broadcast::channel(16);
    let (unused_stdin, _unused_input) = unbounded_channel();
    let guard = spawn_attach_socket_server(
        &path,
        AttachHooks {
            terminal: Some((hub.clone(), terminal.id().clone())),
            output: Arc::new(output),
            stdin: unused_stdin,
            resize: Arc::new(|_, _| {}),
        },
    )
    .unwrap();
    let (mut execution, captured) = attach(&path, None);
    let mut output = captured.stdout.lock().unwrap().take().unwrap();
    let screen = within("hub snapshot", output.recv()).await.unwrap();
    assert!(String::from_utf8_lossy(&screen).contains("existing screen"));
    drop(guard);
    terminal.push_output(b"last bytes\x1b");
    terminal.exit(7);
    assert_eq!(
        within("exit after listener teardown", execution.wait())
            .await
            .unwrap()
            .exit_code,
        7
    );
    let mut tail = Vec::new();
    while let Some(bytes) = within("output EOF", output.recv()).await {
        tail.extend(bytes);
    }
    assert_eq!(tail, b"last bytes\x1b");
}
