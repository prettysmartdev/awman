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
//! This rendezvous socket lives under `<data home>/attach/` (`$AWMAN_HOME/attach`,
//! else `$XDG_DATA_HOME/awman/attach`, else `~/.awman/attach`), named by a hash of the container name. The builtin
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
use crate::engine::agent_runtime::program_status::{ProgramStatusStream, ProgramStatusTracker};
use crate::engine::error::EngineError;

/// The directory attach sockets live in: `<data home>/attach/` (`$AWMAN_HOME`,
/// else `$XDG_DATA_HOME/awman`, else `~/.awman`). Both the serving process and
/// the attaching process derive it the same way, which is what makes the
/// socket discoverable across processes of the same user. With none of those
/// in the snapshot there is no ambient fallback.
pub(crate) fn default_attach_socket_dir(env: &EnvSnapshot) -> Option<PathBuf> {
    if let Some(home) = crate::data::fs::data_paths::awman_home(env) {
        return Some(home.join("attach"));
    }
    if let Some(xdg) = env.xdg_data_home() {
        return Some(xdg.join("awman").join("attach"));
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
    pub terminal: Option<(
        Arc<crate::engine::terminal_hub::TerminalHub>,
        crate::data::ids::TerminalId,
    )>,
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
        let _ = std::fs::remove_file(self.path.with_extension("owner"));
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

        // Same private directory as the socket. A stale PID is checked for liveness by inspect.
        let owner_path = path.with_extension("owner");
        std::fs::write(&owner_path, std::process::id().to_string())
            .map_err(|e| EngineError::io(&owner_path, e))?;
        let _ = std::fs::set_permissions(&owner_path, std::fs::Permissions::from_mode(0o600));
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
                        if let Some((hub, id)) = &hooks.terminal {
                            // The listener may close in the backend's wait
                            // before the execution wrapper publishes Exit.
                            // Hub subscriptions finish on that event (or hub
                            // shutdown), so let them drain independently.
                            tokio::spawn(serve_hub_client(stream, hub.clone(), id.clone()));
                            continue;
                        }
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

    /// The internal binary attach transport subscribes to the same daemon hub
    /// and exclusive capability as WebSocket viewers.
    pub(crate) async fn serve_hub_client(
        stream: tokio::net::UnixStream,
        hub: Arc<crate::engine::terminal_hub::TerminalHub>,
        id: crate::data::ids::TerminalId,
    ) {
        use crate::data::terminal::TerminalFrame;
        let Ok(mut output) = hub.subscribe(&id) else {
            return;
        };
        let grant = hub.take_control(&id).ok();
        drop(hub); // subscription and grant deliberately retain only weak references
        let (mut reader, mut writer) = stream.into_split();
        loop {
            let input = read_frame(&mut reader);
            tokio::pin!(input);
            loop {
                tokio::select! {
                    frame = output.next() => {
                        let (tag, bytes, exited) = match frame {
                            Some(TerminalFrame::Snapshot { screen, .. } | TerminalFrame::Output { bytes: screen }) => (Tag::Output, screen, false),
                            Some(TerminalFrame::Exit { code }) => (Tag::Exit, code.to_le_bytes().to_vec(), true),
                            Some(_) => continue,
                            None => return,
                        };
                        if write_frame_bounded(&mut writer, tag, &bytes, WRITE_TIMEOUT).await.is_err() || exited { return; }
                    }
                    frame = &mut input => {
                        match frame {
                            Ok((Tag::Stdin, bytes)) => {
                                if let Some(grant) = &grant {
                                    if matches!(grant.send_input(bytes), Err(crate::engine::terminal_hub::TerminalHubError::InputBackpressure)) { return; }
                                }
                            }
                            Ok((Tag::Resize, bytes)) => {
                                if let (Some(grant), Some((cols, rows))) = (&grant, parse_resize(&bytes)) {
                                    // Exit may revoke the grant while this
                                    // frame is in flight. Keep reading so the
                                    // queued terminal exit code still arrives.
                                    let _ = grant.resize(cols, rows);
                                }
                            }
                            _ => return,
                        }
                        break;
                    }
                }
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
            let mut status_config = frontend.program_status_config();
            let io = frontend.take_io();
            status_config.answer_queries = io.initial_size.is_some();

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
            let program_status = ProgramStatusTracker::shared();
            let mut status_stream = ProgramStatusStream::new(program_status.clone(), status_config);
            let query_stdin = stdin_injector.clone();
            let (exit_tx, exit_rx) = std::sync::mpsc::channel::<Result<i32, EngineError>>();
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
                        Ok((Tag::Exit, payload)) if payload.len() == 4 => {
                            outcome = Ok(i32::from_le_bytes(
                                payload.try_into().expect("checked exit payload"),
                            ));
                            break;
                        }
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
                    let bytes = status_stream.feed(&payload, |reply| {
                        let _ = query_stdin.send(reply.to_vec());
                    });
                    tail.push_bytes(&bytes);
                    let _ = stdout_tx.send(bytes.into_owned());
                }
                let bytes = status_stream.finish();
                tail.push_bytes(&bytes);
                if !bytes.is_empty() {
                    let _ = stdout_tx.send(bytes);
                }
                let _ = exit_tx.send(outcome);
            });

            let stuck_tx = StuckDetector::spawn(
                StuckConfig {
                    grace_timeout,
                    stuck_timeout,
                    container_start_delay: std::time::Duration::ZERO,
                    cancel_on_grace_expired: None,
                    poll_interval: crate::engine::container::io_bridge::STUCK_POLL_INTERVAL,
                    program_status: Some(program_status.clone()),
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
            Ok(
                AgentExecution::new(handle, Box::new(backend), stuck_tx, Some(output_tail))
                    .with_program_status(program_status),
            )
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
        exit_rx: std::sync::mpsc::Receiver<Result<i32, EngineError>>,
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
            let exit_code = if !self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                outcome?
            } else {
                0
            };
            Ok(AgentExitInfo {
                exit_code,
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
pub(crate) use unix_impl::serve_hub_client;
#[cfg(unix)]
pub(crate) use unix_impl::spawn_attach_socket_server;

// ─── Non-unix stubs ──────────────────────────────────────────────────────────
//
// Unix sockets don't exist elsewhere; the Apple runtime is macOS-only anyway,
// so these stubs exist purely to keep cross-platform builds compiling.

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
