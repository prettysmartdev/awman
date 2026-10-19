//! Binary-clean exec and launcher-hosted attach. The attach client never owns
//! the target lifecycle. Lag/disconnect is an error, never fabricated exit 0.
use super::{driver::*, paths::BuiltinPaths};
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
    pub io: AgentIo,
}
pub struct ExecBridge(ExecBridgeOptions);
impl ExecutionBackend for Execution {
    fn frontend_for_wait(&mut self) -> Option<FrontendWaitGuard> {
        let frontend = self.frontend.take()?;
        let tasks = self.tasks.clone();
        Some(FrontendWaitGuard::from_shared(frontend, move || {
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
        for task in &self.tasks {
            task.abort();
        }
    }
}
/// Time an interrupted agent gets to exit before it is killed; matches the
/// `docker stop` default used by the Docker backend's cancel.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Launch-scoped lifecycle target for automatic cleanup (cancel/stuck).
#[derive(Clone)]
struct CancelTarget {
    driver: Arc<dyn SandboxDriver>,
    id: String,
    token: String,
    remove: bool,
    finished: Arc<AtomicBool>,
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
            self.runtime.spawn(async move {
                tokio::time::sleep(grace).await;
                if target.finished.load(Ordering::Acquire) {
                    return;
                }
                let _ = control.send(Control::Kill);
                let _ = tokio::task::spawn_blocking(move || {
                    target.driver.finish_owned(
                        &target.id,
                        &target.token,
                        Duration::from_secs(2),
                        target.remove,
                    )
                })
                .await;
            });
        }
        result
    }
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
            mut io,
        } = self.0;
        if let Some(prompt) = seeded_prompt {
            // Same bytes as the Docker path: the prompt plus a terminating newline.
            io.stdin_tx
                .send(prompt.into_bytes())
                .map_err(|_| failed())?;
            io.stdin_tx.send(b"\n".to_vec()).map_err(|_| failed())?;
        }
        drop(io.stdin_tx);
        let control = session.control.clone();
        let server = if let Some(path) = attach_path {
            Some(AttachServer::bind(&path, &session)?)
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
        let resize_task = tokio::spawn(async move {
            if let Some(mut resize) = io.resize {
                while let Some((cols, rows)) = resize.recv().await {
                    let _ = resize_control.send(Control::Resize(cols, rows));
                }
            }
        });
        let resize_abort = resize_task.abort_handle();
        let tail = Arc::new(OutputTail::with_default_capacity());
        let output_activity = OutputActivity::new();
        let finished = Arc::new(AtomicBool::new(false));
        let owner = cleanup.as_ref().map(|target| CancelTarget {
            driver: target.driver.clone(),
            id: handle.id.clone(),
            token: target.owner.clone(),
            remove: target.remove_on_exit,
            finished: finished.clone(),
        });
        let cancel = ExecCancelHandle::new(control.clone(), owner.clone(), CANCEL_GRACE);
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
        let event_task = tokio::spawn(async move {
            let mut events = session.events;
            let outcome = loop {
                match events.recv().await {
                    Ok(ExecEvent::Stdout(bytes)) => {
                        output_tail.push_bytes(&bytes);
                        output_activity.touch();
                        let _ = io.stdout.send(bytes);
                    }
                    Ok(ExecEvent::Stderr(bytes)) => {
                        output_tail.push_bytes(&bytes);
                        output_activity.touch();
                        let _ = io.stderr.send(bytes);
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
            drop(frontend_weak);
            input_task.abort();
            resize_task.abort();
            // Give already connected attach clients the terminal frame before the
            // listener is closed. Their tasks hold independent event subscriptions.
            if let Some(server) = server {
                server.finish();
            }
            if let Some(target) = cleanup {
                let _ = tokio::task::spawn_blocking(move || {
                    target.driver.finish_owned(
                        &id,
                        &target.owner,
                        Duration::from_secs(2),
                        target.remove_on_exit,
                    )
                })
                .await;
            }
            finished.store(true, Ordering::Release);
            let _ = done_tx.send(outcome);
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
                _leases: leases,
            }),
            stuck,
            Some(tail),
        ))
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
    fn bind(path: &Path, session: &ExecSession) -> Result<Self, EngineError> {
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
        let control: Arc<dyn ExecControl> = Arc::new(AttachControl(tx));
        let (broadcast, events) = tokio::sync::broadcast::channel(4096);
        tokio::spawn(client(stream, rx, broadcast.clone()));
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
                io,
            })
            .run(frontend)
        }
    }
}
struct AttachControl(tokio::sync::mpsc::UnboundedSender<Control>);
impl ExecControl for AttachControl {
    fn send(&self, command: Control) -> Result<(), EngineError> {
        self.0.send(command).map_err(|_| failed())
    }
}
async fn client(
    stream: tokio::net::UnixStream,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Control>,
    events: tokio::sync::broadcast::Sender<ExecEvent>,
) {
    let (mut reader, mut writer) = stream.into_split();
    loop {
        let frame = read_frame(&mut reader);
        tokio::pin!(frame);
        loop {
            tokio::select! {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    struct RecordingControl(tokio::sync::mpsc::UnboundedSender<Control>);
    impl ExecControl for RecordingControl {
        fn send(&self, command: Control) -> Result<(), EngineError> {
            self.0.send(command).map_err(|_| failed())
        }
    }

    #[tokio::test]
    async fn options_forward_seeded_input_without_retaking_frontend_io() {
        use crate::engine::container::builtin::testing::{eventually, frontend, recorder, Sent};
        let (mut frontend, peer) = frontend(None);
        // The TestFrontend panics if take_io is called twice.
        let io = frontend.take_io();
        let (control, log) = recorder();
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let mut execution = ExecBridge::new(ExecBridgeOptions {
            handle: AgentHandle {
                id: "options".into(),
                name: "options".into(),
                image_tag: "test".into(),
                started_at: chrono::Utc::now(),
            },
            session: ExecSession {
                events,
                broadcast: broadcast.clone(),
                control,
            },
            cleanup: None,
            attach_path: None,
            persistent_stdin: false,
            seeded_prompt: Some("a seeded prompt".into()),
            leases: Vec::new(),
            io,
        })
        .run(frontend)
        .unwrap();
        drop(peer.stdin);
        eventually("seed then EOF", || {
            *log.lock().unwrap()
                == [
                    Sent::Stdin(b"a seeded prompt".to_vec()),
                    Sent::Stdin(b"\n".to_vec()),
                    Sent::Eof,
                ]
        })
        .await;
        assert!(!execution.try_inject_stdin(b"later").unwrap());
        assert!(broadcast.send(ExecEvent::Exited(4)).is_ok());
        assert_eq!(execution.wait().await.unwrap().exit_code, 4);
    }
    #[tokio::test]
    async fn partial_attach_frames_survive_simultaneous_output_and_input() {
        let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (events, receiver) = tokio::sync::broadcast::channel(16);
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(serve_client(
            server,
            receiver,
            Arc::new(RecordingControl(commands)),
        ));
        peer.write_all(&[Tag::Stdin as u8, 3, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(events.send(ExecEvent::Stdout(b"out".to_vec())).is_ok());
        assert_eq!(
            read_frame(&mut peer).await.unwrap(),
            (Tag::Output, b"out".to_vec())
        );
        peer.write_all(&[0, 0, 0, 255, 1]).await.unwrap();
        let input = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(input, Control::Stdin(bytes) if bytes == [0, 255, 1]));
        task.abort();

        let (socket, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut received) = tokio::sync::broadcast::channel(16);
        let task = tokio::spawn(client(socket, receiver, events));
        peer.write_all(&[Tag::Stderr as u8, 3, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        commands.send(Control::Stdin(b"in".to_vec())).unwrap();
        assert_eq!(
            read_frame(&mut peer).await.unwrap(),
            (Tag::Stdin, b"in".to_vec())
        );
        peer.write_all(&[0, 0, 0, 255, 1]).await.unwrap();
        let output = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(output, ExecEvent::Stderr(bytes) if bytes == [0, 255, 1]));
        task.abort();
    }
    #[tokio::test]
    async fn attach_routes_binary_streams_resize_and_real_exit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("attach.sock");
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let session = ExecSession {
            events,
            broadcast,
            control: Arc::new(RecordingControl(commands)),
        };
        let server = AttachServer::bind(&path, &session).unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        write_frame(&mut stream, Tag::Stdin, &[0, 255, 13])
            .await
            .unwrap();
        let command = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(command, Control::Stdin(bytes) if bytes == [0,255,13]));
        write_frame(&mut stream, Tag::Resize, &[80, 0, 24, 0])
            .await
            .unwrap();
        let command = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(command, Control::Resize(80, 24)));
        assert!(session
            .broadcast
            .send(ExecEvent::Stderr(vec![0, 255, 10]))
            .is_ok());
        assert!(session.broadcast.send(ExecEvent::Exited(37)).is_ok());
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            (Tag::Stderr, vec![0, 255, 10])
        );
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            (Tag::Exit, 37_i32.to_le_bytes().to_vec())
        );
        server.finish();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn lagging_attach_client_gets_failure_and_never_a_clean_exit() {
        let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (broadcast, receiver) = tokio::sync::broadcast::channel(2);
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        // Fill a two-slot channel before the client task can read it. A
        // terminal exit follows the lost protocol bytes deliberately.
        for byte in 0..8u8 {
            let _ = broadcast.send(ExecEvent::Stdout(vec![byte]));
        }
        let _ = broadcast.send(ExecEvent::Exited(0));
        let task = tokio::spawn(serve_client(
            server,
            receiver,
            Arc::new(RecordingControl(commands)),
        ));
        assert_eq!(
            read_frame(&mut peer).await.unwrap(),
            (Tag::Failed, Vec::new())
        );
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(read_frame(&mut peer).await.is_err());
    }
    #[tokio::test]
    async fn lagging_primary_frontend_reports_transport_failure_not_guest_exit() {
        let (broadcast, events) = tokio::sync::broadcast::channel(2);
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        for byte in 0..8u8 {
            let _ = broadcast.send(ExecEvent::Stdout(vec![byte]));
        }
        let _ = broadcast.send(ExecEvent::Exited(0));
        let (frontend, peer) = crate::engine::container::builtin::testing::frontend(None);
        let mut execution = {
            let mut frontend = frontend;
            let io = frontend.take_io();
            ExecBridge::new(ExecBridgeOptions {
                handle: AgentHandle {
                    id: "lagging".into(),
                    name: "lagging".into(),
                    image_tag: "test".into(),
                    started_at: chrono::Utc::now(),
                },
                session: ExecSession {
                    events,
                    broadcast,
                    control: Arc::new(RecordingControl(commands)),
                },
                cleanup: None,
                attach_path: None,
                persistent_stdin: false,
                seeded_prompt: None,
                leases: Vec::new(),
                io,
            })
            .run(frontend)
        }
        .unwrap();
        let error = execution.wait().await.unwrap_err().to_string();
        assert!(error.contains("lagged"), "{error}");
        assert!(peer.statuses.lock().unwrap().iter().any(
            |status| matches!(status, AgentStatus::Failed(message) if message.contains("lagged"))
        ));
    }
    /// Delays its own drop so a bridge that releases the frontend only after
    /// publishing the exit is caught deterministically.
    struct SlowDropFrontend(Box<dyn AgentFrontend>);
    impl Drop for SlowDropFrontend {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    impl crate::data::message::UserMessageSink for SlowDropFrontend {
        fn write_message(&mut self, message: crate::data::message::UserMessage) {
            self.0.write_message(message);
        }
        fn replay_queued(&mut self) {
            self.0.replay_queued();
        }
    }
    #[async_trait::async_trait]
    impl AgentFrontend for SlowDropFrontend {
        fn report_status(&mut self, status: AgentStatus) {
            self.0.report_status(status);
        }
        fn report_progress(
            &mut self,
            progress: crate::engine::agent_runtime::frontend::AgentProgress,
        ) {
            self.0.report_progress(progress);
        }
        fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
            self.0.take_io()
        }
        fn grace_timeout(&self) -> Duration {
            self.0.grace_timeout()
        }
        fn stuck_timeout(&self) -> Duration {
            self.0.stuck_timeout()
        }
    }
    /// The bridge releases the frontend and its I/O sinks before `wait()`
    /// resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frontend_is_released_before_wait_resolves() {
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        let _ = broadcast.send(ExecEvent::Exited(0));
        let (frontend, peer) = crate::engine::container::builtin::testing::frontend(None);
        let frontend = Box::new(SlowDropFrontend(frontend));
        let mut execution = {
            let mut frontend = frontend;
            let io = frontend.take_io();
            ExecBridge::new(ExecBridgeOptions {
                handle: AgentHandle {
                    id: "release".into(),
                    name: "release".into(),
                    image_tag: "test".into(),
                    started_at: chrono::Utc::now(),
                },
                session: ExecSession {
                    events,
                    broadcast,
                    control: Arc::new(RecordingControl(commands)),
                },
                cleanup: None,
                attach_path: None,
                persistent_stdin: false,
                seeded_prompt: None,
                leases: Vec::new(),
                io,
            })
            .run(frontend)
        }
        .unwrap();
        assert_eq!(execution.wait().await.unwrap().exit_code, 0);
        // The frontend shares `statuses` with the peer; once it is dropped the
        // peer holds the only reference. (Arc drops after the slow `Drop`.)
        assert_eq!(Arc::strong_count(&peer.statuses), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frontend_is_released_when_wait_future_is_dropped() {
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let _keepalive = broadcast.clone();
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        let (frontend, peer) = crate::engine::container::builtin::testing::frontend(None);
        let mut execution = {
            let mut frontend = Box::new(SlowDropFrontend(frontend));
            let io = frontend.take_io();
            ExecBridge::new(ExecBridgeOptions {
                handle: AgentHandle {
                    id: "dropped-wait".into(),
                    name: "dropped-wait".into(),
                    image_tag: "test".into(),
                    started_at: chrono::Utc::now(),
                },
                session: ExecSession {
                    events,
                    broadcast,
                    control: Arc::new(RecordingControl(commands)),
                },
                cleanup: None,
                attach_path: None,
                persistent_stdin: false,
                seeded_prompt: None,
                leases: Vec::new(),
                io,
            })
            .run(frontend)
        }
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), execution.wait())
                .await
                .is_err()
        );
        assert_eq!(Arc::strong_count(&peer.statuses), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_wait_keeps_event_teardown_and_removes_attach_socket_on_exit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("attach.sock");
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let _keepalive = broadcast.clone();
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        let (frontend, _) = crate::engine::container::builtin::testing::frontend(None);
        let mut execution = {
            let mut frontend = frontend;
            let io = frontend.take_io();
            ExecBridge::new(ExecBridgeOptions {
                handle: AgentHandle {
                    id: "dropped-wait-attach".into(),
                    name: "dropped-wait-attach".into(),
                    image_tag: "test".into(),
                    started_at: chrono::Utc::now(),
                },
                session: ExecSession {
                    events,
                    broadcast: broadcast.clone(),
                    control: Arc::new(RecordingControl(commands)),
                },
                cleanup: None,
                attach_path: Some(path.clone()),
                persistent_stdin: false,
                seeded_prompt: None,
                leases: Vec::new(),
                io,
            })
            .run(frontend)
        }
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), execution.wait())
                .await
                .is_err()
        );
        assert!(path.exists(), "listener must stay live until guest exit");
        let _ = broadcast.send(ExecEvent::Exited(0));
        tokio::time::timeout(Duration::from_secs(2), async {
            while path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("event task did not finish and remove attach socket");
    }
    #[tokio::test]
    async fn attach_disconnect_never_signals_target() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("attach.sock");
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let session = ExecSession {
            events,
            broadcast,
            control: Arc::new(RecordingControl(commands)),
        };
        let server = AttachServer::bind(&path, &session).unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        write_frame(&mut stream, Tag::Stdin, b"ready")
            .await
            .unwrap();
        received.recv().await.unwrap();
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), received.recv())
                .await
                .is_err()
        );
        server.finish();
    }
    #[tokio::test]
    async fn frames_preserve_binary_stderr_and_exit() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        write_frame(&mut writer, Tag::Stderr, &[0, 255, 13, 10])
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (Tag::Stderr, vec![0, 255, 13, 10])
        );
        write_frame(&mut writer, Tag::Exit, &37_i32.to_le_bytes())
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (Tag::Exit, 37_i32.to_le_bytes().to_vec())
        );
    }
    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        writer.write_u8(Tag::Output as u8).await.unwrap();
        writer
            .write_u32_le((crate::engine::container::attach_protocol::MAX_FRAME + 1) as u32)
            .await
            .unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }
    #[tokio::test]
    async fn full_attach_buffer_times_out_instead_of_hanging() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = write_frame_bounded(
            &mut writer,
            Tag::Output,
            &[42; 1024],
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}

#[cfg(test)]
mod bridge_tests;
