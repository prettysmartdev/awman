//! Attach rendezvous socket — attach parity for container runtimes without a
//! native attach verb.
//!
//! `docker attach` works because dockerd holds every container's TTY
//! server-side, so any client can reconnect. Apple's `container` CLI has no
//! attach subcommand at all: the only process holding a running container's
//! PTY is the awman process that spawned `container run -it` under
//! portable-pty (the squad daemon for squad tasks; the TUI/CLI process for
//! interactive sessions). This module makes that process the rendezvous
//! point: alongside the PTY bridge it serves the live PTY over a per-container
//! unix domain socket, and `attach` on the Apple backend connects to it.
//!
//! Parity properties match `docker attach`:
//! - attach reaches the *actual agent TTY* (same PTY, same bytes) — never a
//!   sibling shell;
//! - multiple concurrent clients are allowed (output broadcast, stdin merged);
//! - ending a client session never stops the target container;
//! - a client resize resizes the real PTY, so the agent repaints (the same
//!   redraw trigger the docker attach path relies on).
//!
//! The one non-parity caveat is inherent to the runtime: on Apple the PTY
//! dies with the launching process, so attach requires that process to still
//! be alive. That is not a new fragility — the PTY (and with it the agent's
//! stdio) already had that lifetime before this module existed.
//!
//! Security: sockets live in a `0700` directory under the user's home
//! (`~/.awman/attach/`), each socket is `0600`, and nothing is ever exposed
//! beyond the local user — consistent with the local-only daemon surface in
//! `aspec/architecture/security.md`.
//!
//! ## Wire protocol
//!
//! The shared [`attach_protocol`](super::attach_protocol) frames: this server
//! sends `Output` (raw PTY bytes) and `Lagged` (this client's output is
//! incomplete; detach with error), and accepts `Stdin` and `Resize`.
//!
//! ## Socket path
//!
//! This rendezvous socket lives under `~/.awman/attach/` (or
//! `$AWMAN_ATTACH_DIR`), named by a hash of the container name. The builtin
//! runtime resolves its own attach sockets with `BuiltinPaths::attach`, inside
//! its private state home (keyed by sandbox id and owner, subject to that
//! home's socket-length budget and `AWMAN_BUILTIN_STATE_DIR`). The two stay
//! separate because the endpoints have different owners, lifetimes and state
//! roots; both are pure functions of their inputs (the home directory comes
//! from the `EnvSnapshot` too) and never read the process environment
//! themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::config::env::EnvSnapshot;
use crate::data::session::AgentHandle;
use crate::engine::error::EngineError;

/// The directory attach sockets live in: `$AWMAN_ATTACH_DIR` when set (used
/// by tests and relocated setups), else `~/.awman/attach/`. Both the serving
/// process and the attaching process derive it the same way, which is what
/// makes the socket discoverable across processes of the same user.
///
/// The variable is declared and parsed in Layer 0
/// ([`EnvSnapshot::attach_dir`]); nothing above Layer 0 reads the environment
/// directly (F-37).
pub(crate) fn default_attach_socket_dir(env: &EnvSnapshot) -> Option<PathBuf> {
    if let Some(dir) = env.attach_dir() {
        return Some(dir);
    }
    env.home_dir()
        .map(|home| home.join(".awman").join("attach"))
}

/// The socket path for one container. The filename is a hash of the container
/// name rather than the name itself: unix socket paths have a hard length
/// limit (104 bytes on macOS), which a home prefix plus a full squad container
/// name can exceed.
pub(crate) fn attach_socket_path(env: &EnvSnapshot, container_name: &str) -> Option<PathBuf> {
    let digest = crate::data::fs::hash::sha256_hex(container_name);
    default_attach_socket_dir(env).map(|dir| dir.join(format!("{}.sock", &digest[..16])))
}

/// What the socket server needs from the owning PTY bridge.
pub(crate) struct AttachHooks {
    /// Live PTY output. Each client subscribes; a lagging client is told that
    /// its stream is incomplete and disconnected.
    pub output: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// The bridge's stdin injector — client keystrokes merge with whatever
    /// the launching frontend itself sends, exactly as concurrent
    /// `docker attach` clients merge stdin.
    pub stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Resize the real PTY. The agent receives SIGWINCH and repaints, which
    /// is what gives a freshly-connected client a full screen.
    pub resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
}

/// Owns the listening socket. Dropping it (when the container's execution
/// backend is dropped after `wait()`) aborts the accept loop, every per-client
/// task with it, and removes the socket file.
pub(crate) struct AttachSocketGuard {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AttachSocketGuard {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A configured-but-not-connected attach client for a container whose PTY is
/// served over an attach socket. Produced by backends without a native attach
/// (Apple); the Docker backend keeps `docker attach`.
pub(crate) struct SocketAttachInstance {
    pub handle: AgentHandle,
    pub path: PathBuf,
}

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use tokio::io::AsyncWriteExt;

    use crate::engine::container::attach_protocol::{
        is_protocol_violation, parse_resize, read_frame, resize_payload, write_frame,
        write_frame_bounded, Tag, WRITE_TIMEOUT,
    };

    use crate::engine::agent_runtime::execution::{
        AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, CancelHandle,
        ExecutionBackend,
    };
    use crate::engine::agent_runtime::frontend::AgentFrontend;
    use crate::engine::agent_runtime::output_tail::OutputTail;
    use crate::engine::container::io_bridge::{OutputActivity, StuckConfig, StuckDetector};

    /// Bind the socket and start serving. Never fails the container run —
    /// callers log and continue without attach support on error.
    pub(crate) fn spawn_attach_socket_server(
        path: &Path,
        hooks: AttachHooks,
    ) -> Result<AttachSocketGuard, EngineError> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            // Created with 0700 directly — never a wider-mode window — and
            // re-tightened in case a predecessor left it looser.
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|e| EngineError::io(parent, e))?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        // A stale socket from a crashed predecessor is unlinked, never reused.
        let _ = std::fs::remove_file(path);
        let listener =
            std::os::unix::net::UnixListener::bind(path).map_err(|e| EngineError::io(path, e))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        listener
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(path, e))?;
        let listener =
            tokio::net::UnixListener::from_std(listener).map_err(|e| EngineError::io(path, e))?;

        let task = tokio::spawn(accept_loop(listener, hooks));
        Ok(AttachSocketGuard {
            path: path.to_path_buf(),
            task,
        })
    }

    /// Accept clients until aborted. Per-client tasks live in a `JoinSet`
    /// owned by this task, so aborting the guard tears every session down —
    /// the guard's abort is the *only* way this loop ends, so a transient
    /// accept failure (`ECONNABORTED`, fd exhaustion, …) never permanently
    /// disables attach for a still-running container.
    async fn accept_loop(listener: tokio::net::UnixListener, hooks: AttachHooks) {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let output_rx = hooks.output.subscribe();
                        let stdin = hooks.stdin.clone();
                        let resize = Arc::clone(&hooks.resize);
                        clients.spawn(serve_client(stream, output_rx, stdin, resize));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "attach socket accept failed; still serving");
                        // Pace retries so a persistent error can't spin hot.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                },
                // Reap finished sessions so the set doesn't grow unbounded.
                Some(_) = clients.join_next(), if !clients.is_empty() => {}
            }
        }
    }

    /// One attach session: forward broadcast output out, route stdin/resize
    /// frames in. Both directions run inside this one task (a `select!`, not
    /// a second spawn) so that when the accept loop's `JoinSet` aborts the
    /// task — container teardown — both socket halves drop, the fd closes,
    /// and the client sees EOF. Ends when either direction fails.
    async fn serve_client(
        stream: tokio::net::UnixStream,
        output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    ) {
        let (read_half, write_half) = stream.into_split();
        tokio::select! {
            _ = forward_output(output_rx, write_half) => {}
            _ = route_client_frames(read_half, stdin, resize) => {}
        }
    }

    /// Broadcast PTY output → client, framed.
    async fn forward_output(
        mut output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
    ) {
        loop {
            match output_rx.recv().await {
                Ok(bytes) => {
                    if write_frame_bounded(&mut write_half, Tag::Output, &bytes, WRITE_TIMEOUT)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ =
                        write_frame_bounded(&mut write_half, Tag::Lagged, &[], WRITE_TIMEOUT).await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        let _ = write_half.shutdown().await;
    }

    /// Client frames → the agent's stdin / the real PTY size.
    async fn route_client_frames(
        mut read_half: tokio::net::unix::OwnedReadHalf,
        stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    ) {
        loop {
            let Ok((tag, payload)) = read_frame(&mut read_half).await else {
                break;
            };
            match tag {
                Tag::Stdin => {
                    if stdin.send(payload).is_err() {
                        break;
                    }
                }
                Tag::Resize if parse_resize(&payload).is_some() => {
                    let (cols, rows) = parse_resize(&payload).unwrap_or_default();
                    (resize)(cols, rows);
                }
                _ => break,
            }
        }
    }

    impl AgentInstance for SocketAttachInstance {
        fn handle_preview(&self) -> AgentHandlePreview {
            AgentHandlePreview {
                id: self.handle.id.clone(),
                name: self.handle.name.clone(),
                image: self.handle.image_tag.clone(),
            }
        }

        fn run_with_frontend(
            self: Box<Self>,
            mut frontend: Box<dyn AgentFrontend>,
        ) -> Result<AgentExecution, EngineError> {
            let started_at = chrono::Utc::now();
            let handle = self.handle.clone();

            frontend.report_status(
                crate::engine::agent_runtime::frontend::AgentStatus::Running {
                    container_name: handle.name.clone(),
                },
            );
            let grace_timeout = frontend.grace_timeout();
            let stuck_timeout = frontend.stuck_timeout();
            let io = frontend.take_io();

            let stream = std::os::unix::net::UnixStream::connect(&self.path).map_err(|e| {
                EngineError::Container(format!(
                    "cannot attach to {}: no live attach endpoint ({e}). On this runtime \
                     the awman process that launched the agent serves its terminal; that \
                     process is no longer running, or the agent predates attach support.",
                    handle.name
                ))
            })?;
            // Extra handles for cancel/cancel_handle: shutting one down closes
            // the shared underlying socket, so the reader sees EOF and wait()
            // resolves — killing only this local session, never the container.
            let shutdown = stream
                .try_clone()
                .map_err(|e| EngineError::Container(format!("clone attach socket: {e}")))?;
            let shutdown_for_handle = stream
                .try_clone()
                .map_err(|e| EngineError::Container(format!("clone attach socket: {e}")))?;
            stream
                .set_nonblocking(true)
                .map_err(|e| EngineError::Container(format!("attach socket nonblocking: {e}")))?;
            let stream = tokio::net::UnixStream::from_std(stream)
                .map_err(|e| EngineError::Container(format!("attach socket register: {e}")))?;
            let (mut read_half, mut write_half) = stream.into_split();

            // Writer: an initial resize first (the repaint trigger), then
            // frontend stdin/resize as they arrive.
            let initial_size = io.initial_size;
            let mut stdin_rx = io.stdin_rx;
            let mut resize_rx = io.resize;
            let stdin_injector = io.stdin_tx.clone();
            tokio::spawn(async move {
                if let Some((cols, rows)) = initial_size {
                    if write_frame(&mut write_half, Tag::Resize, &resize_payload(cols, rows))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                loop {
                    tokio::select! {
                        bytes = stdin_rx.recv() => {
                            let Some(bytes) = bytes else { break };
                            if write_frame(&mut write_half, Tag::Stdin, &bytes).await.is_err() {
                                break;
                            }
                        }
                        size = recv_resize(&mut resize_rx) => {
                            let Some((cols, rows)) = size else { break };
                            if write_frame(&mut write_half, Tag::Resize, &resize_payload(cols, rows))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });

            // Reader: framed output → frontend stdout, with the same activity
            // tracking the PTY bridge feeds its stuck detector from.
            let output_tail = Arc::new(OutputTail::with_default_capacity());
            let output_activity = OutputActivity::new();
            let (exit_tx, exit_rx) = std::sync::mpsc::channel::<Result<(), EngineError>>();
            let cancelled = Arc::new(AtomicBool::new(false));
            let stdout_tx = io.stdout;
            let tail = Arc::clone(&output_tail);
            let act = output_activity.clone();
            tokio::spawn(async move {
                let mut outcome = Err(EngineError::Container(
                    "attach endpoint closed without an exit status; reconnect if the launcher is still alive".into(),
                ));
                loop {
                    let payload = match read_frame(&mut read_half).await {
                        Ok((Tag::Output, payload)) => payload,
                        Ok((Tag::Lagged, _)) => {
                            outcome = Err(EngineError::Container(
                                "attach output lagged; reconnect to the live agent".into(),
                            ));
                            break;
                        }
                        Ok(_) => {
                            outcome =
                                Err(EngineError::Container("invalid attach output frame".into()));
                            break;
                        }
                        Err(error) => {
                            if is_protocol_violation(&error) {
                                outcome = Err(EngineError::Container(
                                    "invalid attach output frame".into(),
                                ));
                            }
                            break;
                        }
                    };
                    act.touch();
                    tail.push_bytes(&payload);
                    let _ = stdout_tx.send(payload);
                }
                let _ = exit_tx.send(outcome);
            });

            let stuck_tx = StuckDetector::spawn(
                StuckConfig {
                    grace_timeout,
                    stuck_timeout,
                    container_start_delay: std::time::Duration::ZERO,
                    cancel_on_grace_expired: None,
                },
                &output_activity,
            );

            let backend = SocketAttachExecution {
                exit_rx,
                shutdown,
                shutdown_for_handle: Arc::new(shutdown_for_handle),
                stdin_injector,
                started_at,
                cancelled,
            };
            Ok(AgentExecution::new(
                handle,
                Box::new(backend),
                stuck_tx,
                Some(output_tail),
            ))
        }
    }

    /// Receive from an optional resize channel, pending forever when absent.
    async fn recv_resize(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>>,
    ) -> Option<(u16, u16)> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Execution backend for a socket attach session. Cancellation shuts the
    /// local socket down — never the target container, which belongs to the
    /// serving process.
    struct SocketAttachExecution {
        exit_rx: std::sync::mpsc::Receiver<Result<(), EngineError>>,
        shutdown: std::os::unix::net::UnixStream,
        shutdown_for_handle: Arc<std::os::unix::net::UnixStream>,
        stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        started_at: chrono::DateTime<chrono::Utc>,
        cancelled: Arc<AtomicBool>,
    }

    impl ExecutionBackend for SocketAttachExecution {
        fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
            // Resolves when the reader task ends: server gone (container exit,
            // launcher teardown) or this session's own shutdown.
            let outcome = self.exit_rx.recv().map_err(|_| {
                EngineError::Container("attach reader stopped without a result".into())
            })?;
            if !self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                outcome?;
            }
            Ok(AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            })
        }

        fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
            self.stdin_injector
                .send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            Ok(true)
        }

        fn cancel(&self) -> Result<(), EngineError> {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
            Ok(())
        }

        fn cancel_handle(&self) -> CancelHandle {
            let stream = Arc::clone(&self.shutdown_for_handle);
            let cancelled = Arc::clone(&self.cancelled);
            CancelHandle::new(move || {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                Ok(())
            })
        }
    }
}

#[cfg(unix)]
pub(crate) use unix_impl::spawn_attach_socket_server;

// ─── Non-unix stubs ──────────────────────────────────────────────────────────
//
// Unix sockets don't exist elsewhere; the Apple runtime is macOS-only anyway,
// so these stubs exist purely to keep cross-platform builds compiling.

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::engine::agent_runtime::execution::AgentInstance;
    use crate::engine::agent_runtime::frontend::{
        AgentFrontend, AgentIo, AgentProgress, AgentStatus,
    };

    #[test]
    fn socket_paths_are_deterministic_short_and_name_scoped() {
        let env = EnvSnapshot::with_overrides([("AWMAN_ATTACH_DIR", "/tmp/awman-attach-test")]);
        let a = attach_socket_path(&env, "awman-squad-some-quite-long-task-name-0123abcd").unwrap();
        let b = attach_socket_path(&env, "awman-squad-some-quite-long-task-name-0123abcd").unwrap();
        let c = attach_socket_path(&env, "awman-squad-other-89abcdef").unwrap();
        assert_eq!(a, b, "the path must be derivable in any process");
        assert_ne!(a, c);
        let file = a.file_name().unwrap().to_str().unwrap();
        // Hashed filename: bounded length regardless of container-name length,
        // because unix socket paths have a hard OS limit (104 bytes on macOS).
        assert_eq!(file.len(), "0123456789abcdef.sock".len(), "{file}");
    }

    #[test]
    fn default_dir_derives_from_snapshot_home_not_process_home() {
        let a = EnvSnapshot::with_overrides([("HOME", "/home/session-a")]);
        assert_eq!(
            default_attach_socket_dir(&a),
            Some(PathBuf::from("/home/session-a/.awman/attach"))
        );
        // No HOME in the snapshot: no ambient fallback.
        assert_eq!(default_attach_socket_dir(&EnvSnapshot::empty()), None);
        // An explicit attach dir wins over HOME.
        let both =
            EnvSnapshot::with_overrides([("HOME", "/home/a"), ("AWMAN_ATTACH_DIR", "/run/x")]);
        assert_eq!(
            default_attach_socket_dir(&both),
            Some(PathBuf::from("/run/x"))
        );
    }

    /// A test-side handle to a sender the frontend creates lazily.
    type SharedSender<T> = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<T>>>>;

    /// Frontend fixture mirroring the TUI attach frontend's `AgentIo` shape.
    #[derive(Clone, Default)]
    struct Captured {
        stdout: Arc<Mutex<Vec<u8>>>,
        stdin_tx: SharedSender<Vec<u8>>,
        resize_tx: SharedSender<(u16, u16)>,
    }

    struct TestFrontend {
        captured: Captured,
    }

    impl UserMessageSink for TestFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait::async_trait]
    impl AgentFrontend for TestFrontend {
        fn report_status(&mut self, _status: AgentStatus) {}
        fn report_progress(&mut self, _progress: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
            *self.captured.stdin_tx.lock().unwrap() = Some(stdin_tx.clone());
            *self.captured.resize_tx.lock().unwrap() = Some(resize_tx);
            let buf = self.captured.stdout.clone();
            tokio::spawn(async move {
                while let Some(chunk) = stdout_rx.recv().await {
                    buf.lock().unwrap().extend_from_slice(&chunk);
                }
            });
            tokio::spawn(async move { while stderr_rx.recv().await.is_some() {} });
            AgentIo {
                stdout: stdout_tx,
                stderr: stderr_tx,
                stdin_tx,
                stdin_rx,
                resize: Some(resize_rx),
                initial_size: Some((80, 24)),
            }
        }
    }

    struct ServerFixture {
        output: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
        stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        resizes: Arc<Mutex<Vec<(u16, u16)>>>,
        guard: AttachSocketGuard,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn start_server() -> ServerFixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(64);
        let output = Arc::new(output_tx);
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let resizes: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let resizes_hook = Arc::clone(&resizes);
        let guard = spawn_attach_socket_server(
            &path,
            AttachHooks {
                output: Arc::clone(&output),
                stdin: stdin_tx,
                resize: Arc::new(move |cols, rows| {
                    resizes_hook.lock().unwrap().push((cols, rows));
                }),
            },
        )
        .expect("server must bind");
        ServerFixture {
            output,
            stdin_rx,
            resizes,
            guard,
            path,
            _dir: dir,
        }
    }

    fn instance(path: &Path) -> Box<SocketAttachInstance> {
        Box::new(SocketAttachInstance {
            handle: AgentHandle {
                id: "awman-squad-t-00000001".into(),
                name: "awman-squad-t-00000001".into(),
                image_tag: "img".into(),
                started_at: chrono::Utc::now(),
            },
            path: path.to_path_buf(),
        })
    }

    async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The full attach parity contract, end to end over a real unix socket:
    /// PTY output reaches the client, client keystrokes reach the agent's
    /// stdin, resizes (the initial one included) reach the real PTY, ending
    /// one session leaves the server (and thus the container) untouched, and
    /// tearing the server down ends every client's `wait()`.
    #[tokio::test]
    async fn socket_attach_round_trips_output_stdin_and_resize() {
        let mut server = start_server();

        let captured = Captured::default();
        let mut execution = instance(&server.path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: captured.clone(),
            }))
            .expect("attach must connect");

        // The client's very first act is the initial resize — the repaint
        // trigger that fills a fresh attach screen.
        wait_until(
            || server.resizes.lock().unwrap().first() == Some(&(80, 24)),
            "the initial resize",
        )
        .await;

        // Output: broadcast (what the PTY reader taps) → client stdout.
        server.output.send(b"agent screen bytes".to_vec()).unwrap();
        wait_until(
            || captured.stdout.lock().unwrap().as_slice() == b"agent screen bytes",
            "output to reach the attach client",
        )
        .await;

        // Stdin: client keystrokes → the bridge's stdin injector.
        let stdin_tx = captured.stdin_tx.lock().unwrap().clone().unwrap();
        stdin_tx.send(b"ls\r".to_vec()).unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), server.stdin_rx.recv())
            .await
            .expect("stdin must arrive")
            .expect("stdin channel open");
        assert_eq!(received, b"ls\r");

        // Resize: client → real PTY.
        let resize_tx = captured.resize_tx.lock().unwrap().clone().unwrap();
        resize_tx.send((132, 50)).unwrap();
        wait_until(
            || server.resizes.lock().unwrap().contains(&(132, 50)),
            "the client resize",
        )
        .await;

        // Ending this session must not tear the server down: a second client
        // can still attach (docker-attach parity — multiple clients, and a
        // detach never stops the target).
        execution.cancel().expect("cancel");
        tokio::time::timeout(Duration::from_secs(5), execution.wait())
            .await
            .expect("wait must resolve after cancel")
            .expect("wait result");

        let captured2 = Captured::default();
        let mut execution2 = instance(&server.path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: captured2.clone(),
            }))
            .expect("a second client must connect after the first detached");
        // The server subscribes on accept; wait for the subscription before
        // sending, or the broadcast has zero receivers and drops the send.
        let output = Arc::clone(&server.output);
        wait_until(
            || output.receiver_count() > 0,
            "the second client's server-side subscription",
        )
        .await;
        server.output.send(b"still live".to_vec()).unwrap();
        wait_until(
            || {
                captured2
                    .stdout
                    .lock()
                    .unwrap()
                    .windows(b"still live".len())
                    .any(|w| w == b"still live")
            },
            "output to the second client",
        )
        .await;

        // Dropping the guard (container teardown) ends the remaining session
        // and removes the socket file.
        let path = server.path.clone();
        drop(server.guard);
        let ended = tokio::time::timeout(Duration::from_secs(5), execution2.wait())
            .await
            .expect("wait must resolve when the server goes away")
            .unwrap_err();
        assert!(ended.to_string().contains("without an exit status"));
        wait_until(|| !path.exists(), "socket file removal").await;
    }

    #[tokio::test]
    async fn lagging_socket_attach_reports_incomplete_output() {
        let server = start_server();
        let captured = Captured::default();
        let mut execution = instance(&server.path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: captured.clone(),
            }))
            .unwrap();
        wait_until(|| server.output.receiver_count() > 0, "attach subscription").await;
        // This test uses the current-thread executor. Sending a full ring
        // without awaiting forces the server's receiver to overrun.
        for byte in 0..256 {
            server.output.send(vec![byte as u8]).unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), execution.wait())
            .await
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("lagged"));
    }

    /// The server speaks the shared codec: raw frames in, tagged frames out,
    /// and a lagging client is sent `Lagged` (tag 6), never a stderr tag.
    #[tokio::test]
    async fn server_speaks_the_shared_codec_on_a_raw_socket() {
        use crate::engine::container::attach_protocol::{
            read_frame, resize_payload, write_frame, Tag,
        };
        let mut server = start_server();
        let mut stream = tokio::net::UnixStream::connect(&server.path).await.unwrap();
        write_frame(&mut stream, Tag::Resize, &resize_payload(100, 30))
            .await
            .unwrap();
        write_frame(&mut stream, Tag::Stdin, &[0, 255, 13])
            .await
            .unwrap();
        let stdin = tokio::time::timeout(Duration::from_secs(2), server.stdin_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stdin, [0, 255, 13]);
        assert_eq!(server.resizes.lock().unwrap().as_slice(), [(100, 30)]);
        server.output.send(b"screen".to_vec()).unwrap();
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            (Tag::Output, b"screen".to_vec())
        );
        for byte in 0..256 {
            server.output.send(vec![byte as u8]).unwrap();
        }
        loop {
            let (tag, _) = read_frame(&mut stream).await.unwrap();
            if tag == Tag::Lagged {
                assert_eq!(tag as u8, 6);
                break;
            }
            assert_eq!(tag, Tag::Output);
        }
        // Server-to-client tags are not valid client input: the server drops us.
        let mut stream = tokio::net::UnixStream::connect(&server.path).await.unwrap();
        write_frame(&mut stream, Tag::Exit, &0_i32.to_le_bytes())
            .await
            .unwrap();
        assert!(read_frame(&mut stream).await.is_err());
    }

    /// No serving process → a clear, attributable error instead of a doomed
    /// spawn (the pre-parity behaviour was an instantly-dying client).
    #[tokio::test]
    async fn attach_without_a_live_endpoint_fails_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.sock");
        let error = instance(&path)
            .run_with_frontend(Box::new(TestFrontend {
                captured: Captured::default(),
            }))
            .err()
            .expect("connecting to a missing socket must fail");
        let text = error.to_string();
        assert!(text.contains("no live attach endpoint"), "{text}");
        assert!(text.contains("awman-squad-t-00000001"), "{text}");
    }

    /// A stale socket file from a crashed predecessor must not block a new
    /// server for the same container name.
    #[tokio::test]
    async fn rebinding_over_a_stale_socket_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        // A dead socket file: bind and immediately drop the listener.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(4);
        let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let guard = spawn_attach_socket_server(
            &path,
            AttachHooks {
                output: Arc::new(output_tx),
                stdin: stdin_tx,
                resize: Arc::new(|_, _| {}),
            },
        )
        .expect("rebinding over a stale socket must succeed");
        drop(guard);
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn_attach_socket_server(
    _path: &Path,
    _hooks: AttachHooks,
) -> Result<AttachSocketGuard, EngineError> {
    Err(EngineError::Container(
        "attach sockets require a unix host".into(),
    ))
}

#[cfg(not(unix))]
impl crate::engine::agent_runtime::execution::AgentInstance for SocketAttachInstance {
    fn handle_preview(&self) -> crate::engine::agent_runtime::execution::AgentHandlePreview {
        crate::engine::agent_runtime::execution::AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        _frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<crate::engine::agent_runtime::execution::AgentExecution, EngineError> {
        Err(EngineError::Container(
            "attach sockets require a unix host".into(),
        ))
    }
}
