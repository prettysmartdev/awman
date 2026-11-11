//! Binary-clean exec and launcher-hosted attach. The attach client never owns
//! the target lifecycle. Lag/disconnect is an error, never fabricated exit 0.
use super::{driver::*, paths::BuiltinPaths};
use crate::engine::agent_runtime::program_status::{
    ProgramStatusBridgeConfig, ProgramStatusStream, ProgramStatusTracker,
};
use crate::engine::container::attach_protocol::{
    parse_resize, read_frame, resize_payload, write_chunked_bounded, write_frame,
    write_frame_bounded, Tag, WRITE_TIMEOUT as ATTACH_WRITE_TIMEOUT,
};
use crate::{
    data::session::AgentHandle,
    engine::{
        agent_runtime::{
            execution::{
                AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, CancelHandle,
                ExecutionBackend, FrontendWaitGuard,
            },
            frontend::{AgentFrontend, AgentIo, AgentStatus},
            output_tail::OutputTail,
        },
        container::io_bridge::{OutputActivity, StuckConfig, StuckDetector},
        error::EngineError,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

fn failed() -> EngineError {
    EngineError::Other("builtin exec stream closed or failed before reporting exit status".into())
}

struct Execution {
    done: std::sync::mpsc::Receiver<Result<AgentExitInfo, EngineError>>,
    control: Arc<dyn ExecControl>,
    frontend: Option<Arc<Mutex<Box<dyn AgentFrontend>>>>,
    tasks: Vec<tokio::task::AbortHandle>,
    /// Whether awman may inject input (ACP and interactive sessions).
    /// Injection goes straight to the exec's control channel, never through a
    /// clone of the frontend's stdin sender: holding such a clone would keep
    /// the input stream open forever, so the frontend closing stdin would
    /// never reach the agent as EOF.
    injectable: bool,
    cancel: ExecCancelHandle,
    /// Tells the event task the frontend is gone (dropped wait or dropped
    /// execution), so it drops the frontend's stdout/stderr senders at once
    /// instead of holding them until the guest exits. It keeps watching for
    /// the exit, which drives attach and owner teardown.
    io_release: Arc<tokio::sync::Notify>,
    _leases: Vec<crate::engine::credential_refresh::CredentialLease>,
}
pub struct CleanupTarget {
    pub driver: Arc<dyn SandboxDriver>,
    pub owner: String,
    pub remove_on_exit: bool,
}
pub struct ExecBridgeOptions {
    pub handle: AgentHandle,
    pub session: ExecSession,
    pub cleanup: Option<CleanupTarget>,
    pub attach_path: Option<PathBuf>,
    pub persistent_stdin: bool,
    pub seeded_prompt: Option<String>,
    pub leases: Vec<crate::engine::credential_refresh::CredentialLease>,
    /// Held by the event task, not the execution: a dropped execution leaves
    /// the guest running through its cancel grace, and it can read these until
    /// it exits.
    pub prompt_files: Vec<crate::engine::container::options::PromptFileGuard>,
    pub io: AgentIo,
    /// How long an interrupted agent gets to exit before it is killed and its
    /// sandbox stopped. Production launches use [`DEFAULT_CANCEL_GRACE`].
    pub cancel_grace: Duration,
}
pub struct ExecBridge(ExecBridgeOptions);

/// An aborted observer cannot prove guest exit. Preserve the paths on that
/// exceptional drop, but release their descriptors with the last launch guard.
struct ObservedPromptFiles(Vec<crate::engine::container::options::PromptFileGuard>);
impl ObservedPromptFiles {
    fn guest_ended(&mut self) {
        self.0.clear();
    }
}
impl Drop for ObservedPromptFiles {
    fn drop(&mut self) {
        for file in &self.0 {
            file.preserve_for_unconfirmed_guest();
        }
    }
}
impl ExecutionBackend for Execution {
    fn frontend_for_wait(&mut self) -> Option<FrontendWaitGuard> {
        let frontend = self.frontend.take()?;
        let tasks = self.tasks.clone();
        let io_release = self.io_release.clone();
        Some(FrontendWaitGuard::from_shared(frontend, move || {
            io_release.notify_one();
            // The event task owns the attach server and must reach guest exit
            // cleanup. Only the input and resize pumps belong in this list.
            for task in tasks {
                task.abort();
            }
        }))
    }

    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        self.done.recv().map_err(|_| failed())?
    }
    fn cancel(&self) -> Result<(), EngineError> {
        self.cancel.cancel()
    }
    fn cancel_handle(&self) -> CancelHandle {
        let cancel = self.cancel.clone();
        CancelHandle::new(move || cancel.cancel())
    }
    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if !self.injectable {
            return Ok(false);
        }
        self.control.send(Control::Stdin(bytes.to_vec()))?;
        Ok(true)
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        self.io_release.notify_one();
        for task in &self.tasks {
            task.abort();
        }
    }
}
/// Default time an interrupted agent gets to exit before it is killed
/// ([`ExecBridgeOptions::cancel_grace`]); matches the
/// `docker stop` default used by the Docker backend's cancel.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Launch-scoped lifecycle target for automatic cleanup (cancel/stuck).
#[derive(Clone)]
struct CancelTarget {
    driver: Arc<dyn SandboxDriver>,
    id: String,
    token: String,
    remove: bool,
    /// Set by whichever of the event task (agent exited) and the grace timer
    /// (agent ignored the interrupt) reaches cleanup first; the other skips
    /// it, so `finish_owned` never runs twice concurrently.
    cleanup_claimed: Arc<AtomicBool>,
    /// What the grace timer's stop did, for an event task that lost the claim.
    stop_result: Arc<tokio::sync::watch::Sender<Option<bool>>>,
}

/// One grace timer shared by user cancellation, dropped waits and the stuck detector.
#[derive(Clone)]
struct ExecCancelHandle {
    control: Arc<dyn ExecControl>,
    owner: Option<CancelTarget>,
    grace: Duration,
    requested: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
}
impl ExecCancelHandle {
    fn new(control: Arc<dyn ExecControl>, owner: Option<CancelTarget>, grace: Duration) -> Self {
        Self {
            control,
            owner,
            grace,
            requested: Arc::new(AtomicBool::new(false)),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    fn cancel(&self) -> Result<(), EngineError> {
        if self.requested.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Cleanup must still be scheduled if the guest control channel has died.
        let result = self.control.send(Control::Interrupt);
        if let Some(target) = self.owner.clone() {
            let control = self.control.clone();
            let grace = self.grace;
            let requested = self.requested.clone();
            self.runtime.spawn(async move {
                tokio::time::sleep(grace).await;
                if target.cleanup_claimed.swap(true, Ordering::AcqRel) {
                    return;
                }
                let _ = control.send(Control::Kill);
                let stop = tokio::task::spawn_blocking(move || {
                    target.driver.finish_owned(
                        &target.id,
                        &target.token,
                        Duration::from_secs(2),
                        target.remove,
                    )
                })
                .await;
                let stopped = stop_confirmed(stop, &target.cleanup_claimed);
                target.stop_result.send_replace(Some(stopped));
                if !stopped {
                    requested.store(false, Ordering::Release);
                }
            });
        }
        result
    }
}

/// Whether an owner-scoped stop finished. A failed one gives up the claim, so
/// whoever reaches cleanup next tries again.
fn stop_confirmed(
    stop: Result<Result<(), EngineError>, tokio::task::JoinError>,
    cleanup_claimed: &AtomicBool,
) -> bool {
    let error = match stop {
        Ok(Ok(())) => return true,
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    };
    tracing::error!(%error, "Failed to stop builtin agent; keeping its prompt files");
    cleanup_claimed.store(false, Ordering::Release);
    false
}

impl ExecBridge {
    pub fn new(options: ExecBridgeOptions) -> Self {
        Self(options)
    }
    pub fn run(self, mut frontend: Box<dyn AgentFrontend>) -> Result<AgentExecution, EngineError> {
        let ExecBridgeOptions {
            handle,
            session,
            cleanup,
            attach_path,
            persistent_stdin,
            seeded_prompt,
            leases,
            prompt_files,
            mut io,
            cancel_grace,
        } = self.0;
        if let Some(prompt) = seeded_prompt {
            // Same bytes as the Docker path: the prompt plus a terminating newline.
            io.stdin_tx
                .send(prompt.into_bytes())
                .map_err(|_| failed())?;
            io.stdin_tx.send(b"\n".to_vec()).map_err(|_| failed())?;
        }
        let status_config = ProgramStatusBridgeConfig {
            answer_queries: io.initial_size.is_some(),
            ..frontend.program_status_config()
        };
        drop(io.stdin_tx);
        let control = session.control.clone();
        let terminal = match (
            io.initial_size,
            frontend.terminal_launch(),
            control.terminal_stdin(),
        ) {
            (Some((cols, rows)), Some(launch), Some(stdin)) => {
                let resize_control = control.clone();
                Some((
                    launch.clone(),
                    launch.register(
                        cols,
                        rows,
                        crate::engine::terminal_hub::TerminalInput {
                            stdin,
                            resize: Some(Arc::new(move |cols, rows| {
                                let _ = resize_control.send(Control::Resize(cols, rows));
                            })),
                        },
                    ),
                ))
            }
            _ => None,
        };
        let server = if let Some(path) = attach_path {
            Some(AttachServer::bind(
                &path,
                &session,
                terminal
                    .as_ref()
                    .map(|(launch, terminal)| (launch.hub.clone(), terminal.id().clone())),
            )?)
        } else {
            None
        };
        let input_control = control.clone();
        let input_task = tokio::spawn(async move {
            while let Some(bytes) = io.stdin_rx.recv().await {
                if input_control.send(Control::Stdin(bytes)).is_err() {
                    break;
                }
            }
            let _ = input_control.send(Control::Eof);
        });
        let input_abort = input_task.abort_handle();
        let resize_control = control.clone();
        let resize_terminal = terminal.clone();
        let resize_task = tokio::spawn(async move {
            if let Some(mut resize) = io.resize {
                while let Some((cols, rows)) = resize.recv().await {
                    if let Some((launch, terminal)) = &resize_terminal {
                        let _ = launch.hub.set_default_size(terminal.id(), cols, rows);
                        continue;
                    }
                    let _ = resize_control.send(Control::Resize(cols, rows));
                }
            }
        });
        let resize_abort = resize_task.abort_handle();
        let tail = Arc::new(OutputTail::with_default_capacity());
        let output_activity = OutputActivity::new();
        let program_status = ProgramStatusTracker::shared();
        let cleanup_claimed = Arc::new(AtomicBool::new(false));
        let (stop_result, mut grace_stop) = tokio::sync::watch::channel(None);
        let stop_result = Arc::new(stop_result);
        let owner = cleanup.as_ref().map(|target| CancelTarget {
            driver: target.driver.clone(),
            id: handle.id.clone(),
            token: target.owner.clone(),
            remove: target.remove_on_exit,
            cleanup_claimed: cleanup_claimed.clone(),
            stop_result,
        });
        let cancel = ExecCancelHandle::new(control.clone(), owner.clone(), cancel_grace);
        let stuck_cancel = cancel.clone();
        let cancel_grace = owner.is_some().then(|| {
            Arc::new(move || {
                let _ = stuck_cancel.cancel();
            }) as crate::engine::container::io_bridge::CancelFn
        });
        let stuck = StuckDetector::spawn(
            StuckConfig {
                grace_timeout: frontend.grace_timeout(),
                stuck_timeout: frontend.stuck_timeout(),
                container_start_delay: Duration::ZERO,
                cancel_on_grace_expired: cancel_grace,
                poll_interval: crate::engine::container::io_bridge::STUCK_POLL_INTERVAL,
                program_status: Some(program_status.clone()),
            },
            &output_activity,
        );
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let started = chrono::Utc::now();
        let id = handle.id.clone();
        let output_tail = tail.clone();
        frontend.report_status(AgentStatus::Running {
            container_name: handle.name.clone(),
        });
        let frontend = Arc::new(Mutex::new(frontend));
        let frontend_weak = Arc::downgrade(&frontend);
        let io_release = Arc::new(tokio::sync::Notify::new());
        let released = io_release.clone();
        let mut sinks = Some((io.stdout, io.stderr));
        let mut prompt_files = ObservedPromptFiles(prompt_files);
        let mut stdout_status = ProgramStatusStream::new(program_status.clone(), status_config);
        let mut stderr_status = ProgramStatusStream::new(program_status.clone(), status_config);
        let query_control = control.clone();
        let event_task = tokio::spawn(async move {
            let mut events = session.events;
            let outcome = loop {
                let event = tokio::select! {
                    biased;
                    _ = released.notified(), if sinks.is_some() => {
                        sinks = None;
                        continue;
                    }
                    event = events.recv() => event,
                };
                match event {
                    Ok(ExecEvent::Stdout(bytes)) => {
                        output_activity.touch();
                        let bytes = stdout_status.feed(&bytes, |reply| {
                            let _ = query_control.send(Control::Stdin(reply.to_vec()));
                        });
                        output_tail.push_bytes(&bytes);
                        if let Some((_, terminal)) = &terminal {
                            terminal.push_output(&bytes);
                        }
                        if let Some((stdout, _)) = &sinks {
                            let _ = stdout.send(bytes.into_owned());
                        }
                    }
                    Ok(ExecEvent::Stderr(bytes)) => {
                        output_activity.touch();
                        let bytes = stderr_status.feed(&bytes, |reply| {
                            let _ = query_control.send(Control::Stdin(reply.to_vec()));
                        });
                        output_tail.push_bytes(&bytes);
                        if let Some((_, terminal)) = &terminal {
                            terminal.push_output(&bytes);
                        }
                        if let Some((_, stderr)) = &sinks {
                            let _ = stderr.send(bytes.into_owned());
                        }
                    }
                    Ok(ExecEvent::Exited(code)) => {
                        if let Some(frontend) = frontend_weak.upgrade() {
                            frontend
                                .lock()
                                .unwrap()
                                .report_status(AgentStatus::Exited(code));
                        }
                        break Ok(AgentExitInfo {
                            exit_code: code,
                            signal: None,
                            started_at: started,
                            ended_at: chrono::Utc::now(),
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // An ACP frontend must never see a truncated stream followed
                        // by a plausible exit status. The guest may still be alive;
                        // this is a transport failure, not its exit code.
                        tracing::warn!(skipped, "builtin exec output lagged; stream failed");
                        if let Some(frontend) = frontend_weak.upgrade() {
                            frontend.lock().unwrap().report_status(AgentStatus::Failed(
                                "builtin exec output lagged".into(),
                            ));
                        }
                        break Err(EngineError::Other(
                            "builtin exec output lagged; reconnect or restart the agent".into(),
                        ));
                    }
                    Ok(ExecEvent::Failed) | Err(_) => {
                        if let Some(frontend) = frontend_weak.upgrade() {
                            frontend.lock().unwrap().report_status(AgentStatus::Failed(
                                "builtin exec stream failed".into(),
                            ));
                        }
                        break Err(failed());
                    }
                }
            };
            for (stream, is_stderr) in [(&mut stdout_status, false), (&mut stderr_status, true)] {
                let bytes = stream.finish();
                output_tail.push_bytes(&bytes);
                if let Some((_, terminal)) = &terminal {
                    terminal.push_output(&bytes);
                }
                if let Some((stdout, stderr)) = &sinks {
                    let _ = if is_stderr { stderr } else { stdout }.send(bytes);
                }
            }
            if let Some((_, terminal)) = &terminal {
                terminal.exit(outcome.as_ref().map_or(-1, |info| info.exit_code));
            }
            drop(sinks);
            drop(frontend_weak);
            input_task.abort();
            resize_task.abort();
            // Give already connected attach clients the terminal frame before the
            // listener is closed. Their tasks hold independent event subscriptions.
            if let Some(server) = server {
                server.finish();
            }
            // The guest can read its prompt files until its exit is observed or
            // its stop confirmed. A failed stream establishes neither.
            let mut guest_ended = outcome.is_ok();
            if let Some(target) = cleanup {
                if cleanup_claimed.swap(true, Ordering::AcqRel) {
                    if !guest_ended {
                        guest_ended = grace_stop
                            .wait_for(Option::is_some)
                            .await
                            .is_ok_and(|stopped| *stopped == Some(true));
                    }
                } else {
                    let stop = tokio::task::spawn_blocking(move || {
                        target.driver.finish_owned(
                            &id,
                            &target.owner,
                            Duration::from_secs(2),
                            target.remove_on_exit,
                        )
                    })
                    .await;
                    guest_ended |= stop_confirmed(stop, &cleanup_claimed);
                }
            }
            if guest_ended {
                prompt_files.guest_ended();
                let _ = done_tx.send(outcome);
            } else {
                // Report the stream failure promptly, but retain this launch's
                // files in a reachable owner. A later cancellation or exit can
                // still establish that the guest no longer needs them.
                let _ = done_tx.send(outcome);
                if prompt_files.0.is_empty() {
                    return;
                }
                let mut stream_open = true;
                let mut stop_open = true;
                loop {
                    if *grace_stop.borrow() == Some(true) {
                        break;
                    }
                    tokio::select! {
                        event = events.recv(), if stream_open => match event {
                            Ok(ExecEvent::Exited(_)) => break,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => stream_open = false,
                            _ => {}
                        },
                        changed = grace_stop.changed(), if stop_open => {
                            if changed.is_err() { stop_open = false; }
                        },
                        // With no evidence source left, keep the files for the
                        // unknown guest rather than deleting them or busy-looping.
                        _ = std::future::pending::<()>() => {}
                    }
                }
                prompt_files.guest_ended();
            }
        });
        // Dropping the JoinHandle detaches the event task. It must outlive a
        // dropped wait so it can observe exit and run the server/owner teardown.
        drop(event_task);
        let tasks = vec![input_abort, resize_abort];
        Ok(AgentExecution::new(
            handle,
            Box::new(Execution {
                done,
                control,
                frontend: Some(frontend),
                tasks,
                injectable: persistent_stdin,
                cancel,
                io_release,
                _leases: leases,
            }),
            stuck,
            Some(tail),
        )
        .with_program_status(program_status))
    }
}

struct AttachServer {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl AttachServer {
    fn finish(self) {
        // Drop performs cleanup on both the normal and aborted task paths.
    }
}
impl Drop for AttachServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
impl AttachServer {
    fn bind(
        path: &Path,
        session: &ExecSession,
        terminal: Option<(
            Arc<crate::engine::terminal_hub::TerminalHub>,
            crate::data::ids::TerminalId,
        )>,
    ) -> Result<Self, EngineError> {
        use std::os::unix::fs::PermissionsExt;
        // Never unlink a live or unknown endpoint. The SDK refuses duplicate names;
        // an existing attach endpoint is a failed start, requiring explicit cleanup.
        let listener =
            std::os::unix::net::UnixListener::bind(path).map_err(|e| EngineError::io(path, e))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| EngineError::io(path, e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(path, e))?;
        let listener =
            tokio::net::UnixListener::from_std(listener).map_err(|e| EngineError::io(path, e))?;
        let events = session.broadcast.clone();
        let control = session.control.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Some((hub, id)) = &terminal {
                    tokio::spawn(crate::engine::container::attach_socket::serve_hub_client(
                        stream,
                        hub.clone(),
                        id.clone(),
                    ));
                    continue;
                }
                let receiver = events.subscribe();
                let control = control.clone();
                tokio::spawn(serve_client(stream, receiver, control));
            }
        });
        Ok(Self {
            path: path.into(),
            task,
        })
    }
}

async fn serve_client(
    stream: tokio::net::UnixStream,
    mut events: tokio::sync::broadcast::Receiver<ExecEvent>,
    control: Arc<dyn ExecControl>,
) {
    let (mut reader, mut writer) = stream.into_split();
    let mut stdout_scanner = crate::engine::agent_runtime::program_status::Osc7501Scanner::new();
    let mut stderr_scanner = crate::engine::agent_runtime::program_status::Osc7501Scanner::new();
    loop {
        // A frame can arrive in fragments. Keep this future alive while
        // events are forwarded; recreating it in select! discards partially
        // consumed headers/payloads whenever the other branch wins.
        let frame = read_frame(&mut reader);
        tokio::pin!(frame);
        loop {
            tokio::select! {
                frame=&mut frame=>{match frame {
                    Ok((Tag::Stdin,bytes))=>{if control.send(Control::Stdin(bytes)).is_err(){return;}},
                    Ok((Tag::Resize,bytes)) if parse_resize(&bytes).is_some()=>{let (cols,rows)=parse_resize(&bytes).unwrap_or_default();let _=control.send(Control::Resize(cols,rows));},
                    _=>return,
                } break;},
                event=events.recv()=>{
                    // The stream is no longer complete. Send an explicit
                    // terminal failure, then disconnect this client only.
                    if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) {
                        let _ = write_frame_bounded(&mut writer, Tag::Failed, &[], ATTACH_WRITE_TIMEOUT).await;
                        return;
                    }
                    let (tag,bytes,terminal)=match event {Ok(ExecEvent::Stdout(b))=>(Tag::Output,b,false),Ok(ExecEvent::Stderr(b))=>(Tag::Stderr,b,false),Ok(ExecEvent::Exited(c))=>(Tag::Exit,c.to_le_bytes().to_vec(),true),_=>(Tag::Failed,Vec::new(),true)};
                    if terminal {
                        for (tag, scanner) in [(Tag::Output, &mut stdout_scanner), (Tag::Stderr, &mut stderr_scanner)] {
                            let pending = scanner.finish();
                            if !pending.is_empty() && write_chunked_bounded(&mut writer, tag, &pending, ATTACH_WRITE_TIMEOUT).await.is_err() { return; }
                        }
                    }
                    let bytes = match tag { Tag::Output => stdout_scanner.feed(&bytes).forward, Tag::Stderr => stderr_scanner.feed(&bytes).forward, _ => std::borrow::Cow::Borrowed(bytes.as_slice()) };
                    let error=write_chunked_bounded(&mut writer,tag,&bytes,ATTACH_WRITE_TIMEOUT).await.is_err();
                    if error||terminal{return;}
                }
            }
        }
    }
}

pub struct AttachInstance {
    pub handle: AgentHandle,
    pub paths: BuiltinPaths,
    pub owner: String,
}
impl AgentInstance for AttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }
    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let path = self.paths.attach(&self.handle.id, &self.owner);
        let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|error| {
            EngineError::BuiltinAttachUnavailable {
                name: self.handle.name.clone(),
                detail: error.to_string(),
            }
        })?;
        stream
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(&path, e))?;
        let stream =
            tokio::net::UnixStream::from_std(stream).map_err(|e| EngineError::io(&path, e))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (terminal_stdin, terminal_input) = crate::engine::terminal_hub::input_channel();
        let control: Arc<dyn ExecControl> = Arc::new(AttachControl(tx, terminal_stdin));
        let (broadcast, events) = tokio::sync::broadcast::channel(4096);
        tokio::spawn(client(stream, rx, broadcast.clone(), terminal_input));
        {
            let mut frontend = frontend;
            let io = frontend.take_io();
            ExecBridge::new(ExecBridgeOptions {
                handle: self.handle,
                session: ExecSession {
                    events,
                    broadcast,
                    control,
                },
                cleanup: None,
                attach_path: None,
                persistent_stdin: true,
                seeded_prompt: None,
                leases: Vec::new(),
                prompt_files: Vec::new(),
                io,
                cancel_grace: DEFAULT_CANCEL_GRACE,
            })
            .run(frontend)
        }
    }
}
struct AttachControl(
    tokio::sync::mpsc::UnboundedSender<Control>,
    tokio::sync::mpsc::Sender<Vec<u8>>,
);
impl ExecControl for AttachControl {
    fn terminal_stdin(&self) -> Option<tokio::sync::mpsc::Sender<Vec<u8>>> {
        Some(self.1.clone())
    }
    fn send(&self, command: Control) -> Result<(), EngineError> {
        self.0.send(command).map_err(|_| failed())
    }
}
async fn client(
    stream: tokio::net::UnixStream,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Control>,
    events: tokio::sync::broadcast::Sender<ExecEvent>,
    mut terminal_input: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (mut reader, mut writer) = stream.into_split();
    loop {
        let frame = read_frame(&mut reader);
        tokio::pin!(frame);
        loop {
            tokio::select! {
                Some(bytes) = terminal_input.recv() => {
                    if write_frame(&mut writer, Tag::Stdin, &bytes).await.is_err() { let _ = events.send(ExecEvent::Failed); return; }
                }
                frame=&mut frame=>{
                    let (event,terminal)=match frame {Ok((Tag::Output,b))=>(ExecEvent::Stdout(b),false),Ok((Tag::Stderr,b))=>(ExecEvent::Stderr(b),false),Ok((Tag::Exit,b)) if b.len()==4=>(ExecEvent::Exited(i32::from_le_bytes(b.try_into().unwrap())),true),_=>(ExecEvent::Failed,true)};
                    let _=events.send(event);if terminal{return;} break;
                },
                command=commands.recv()=>{
                    let (tag,bytes)=match command {Some(Control::Stdin(b))=>(Tag::Stdin,b),Some(Control::Resize(c,r))=>(Tag::Resize,resize_payload(c,r).to_vec()),Some(Control::Interrupt)=>{let _=events.send(ExecEvent::Failed);return;},Some(Control::Eof)=>continue,_=>return};
                    if write_frame(&mut writer,tag,&bytes).await.is_err(){let _=events.send(ExecEvent::Failed);return;}
                }
            }
        }
    }
}
