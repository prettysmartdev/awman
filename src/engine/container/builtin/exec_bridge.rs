//! Binary-clean exec and launcher-hosted attach. The attach client never owns
//! the target lifecycle. Lag/disconnect is an error, never fabricated exit 0.
use super::{driver::*, paths::BuiltinPaths};
use crate::{
    data::session::AgentHandle,
    engine::{
        agent_runtime::{
            execution::{
                AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, CancelHandle,
                ExecutionBackend,
            },
            frontend::{AgentFrontend, AgentStatus},
            output_tail::OutputTail,
        },
        container::io_bridge::spawn_stuck_detector,
        error::EngineError,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STDOUT: u8 = 0;
const INPUT: u8 = 1;
const RESIZE: u8 = 2;
const STDERR: u8 = 3;
const EXIT: u8 = 4;
const FAILED: u8 = 5;
const MAX_FRAME: usize = 1024 * 1024;
fn failed() -> EngineError {
    EngineError::Other("builtin exec stream closed or failed before reporting exit status".into())
}

struct Execution {
    done: std::sync::mpsc::Receiver<Result<AgentExitInfo, EngineError>>,
    control: Arc<dyn ExecControl>,
    input: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    owner: Option<CancelTarget>,
    _leases: Vec<crate::engine::credential_refresh::CredentialLease>,
}
pub struct RunInput {
    pub seeded_prompt: Option<String>,
    pub leases: Vec<crate::engine::credential_refresh::CredentialLease>,
}
impl ExecutionBackend for Execution {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        self.done.recv().map_err(|_| failed())?
    }
    fn cancel(&self) -> Result<(), EngineError> {
        cancel(self.control.clone(), self.owner.clone(), CANCEL_GRACE)
    }
    fn cancel_handle(&self) -> Option<CancelHandle> {
        let control = self.control.clone();
        let owner = self.owner.clone();
        Some(CancelHandle::new(move || {
            cancel(control.clone(), owner.clone(), CANCEL_GRACE)
        }))
    }
    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.input {
            tx.send(bytes.to_vec()).map_err(|_| failed())?;
            Ok(true)
        } else {
            Ok(false)
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

fn cancel(
    control: Arc<dyn ExecControl>,
    owner: Option<CancelTarget>,
    grace: Duration,
) -> Result<(), EngineError> {
    control.send(Control::Interrupt)?;
    if let Some(target) = owner {
        // Cancellation must remain usable while wait() occupies a blocking task.
        std::thread::spawn(move || {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if target.finished.load(Ordering::Acquire) {
                    // The exit path already ran finish_owned (honouring --keep).
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = control.send(Control::Kill);
            let _ = target.driver.finish_owned(
                &target.id,
                &target.token,
                Duration::from_secs(2),
                target.remove,
            );
        });
    }
    Ok(())
}

pub fn run(
    handle: AgentHandle,
    session: ExecSession,
    mut frontend: Box<dyn AgentFrontend>,
    cleanup: Option<(Arc<dyn SandboxDriver>, String, bool)>,
    attach_path: Option<PathBuf>,
    persistent_stdin: bool,
    input: RunInput,
) -> Result<AgentExecution, EngineError> {
    let mut io = frontend.take_io();
    if let Some(prompt) = input.seeded_prompt {
        // Same bytes as the Docker path: the prompt plus a terminating newline.
        io.stdin_tx
            .send(prompt.into_bytes())
            .map_err(|_| failed())?;
        io.stdin_tx.send(b"\n".to_vec()).map_err(|_| failed())?;
    }
    let injector = if persistent_stdin {
        Some(io.stdin_tx.clone())
    } else {
        None
    };
    drop(io.stdin_tx);
    let control = session.control.clone();
    let server = if let Some(path) = attach_path {
        Some(serve(&path, &session)?)
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
    let resize_control = control.clone();
    let resize_task = tokio::spawn(async move {
        if let Some(mut resize) = io.resize {
            while let Some((cols, rows)) = resize.recv().await {
                let _ = resize_control.send(Control::Resize(cols, rows));
            }
        }
    });
    let tail = Arc::new(OutputTail::with_default_capacity());
    let first = Arc::new(AtomicBool::new(false));
    let activity = Arc::new(Mutex::new(None));
    let finished = Arc::new(AtomicBool::new(false));
    let owner = cleanup
        .as_ref()
        .map(|(driver, token, remove)| CancelTarget {
            driver: driver.clone(),
            id: handle.id.clone(),
            token: token.clone(),
            remove: *remove,
            finished: finished.clone(),
        });
    let cancel_control = control.clone();
    let cancel_owner = owner.clone();
    let cancel_grace = owner.is_some().then(|| {
        Arc::new(move || {
            let _ = cancel(cancel_control.clone(), cancel_owner.clone(), CANCEL_GRACE);
        }) as crate::engine::container::io_bridge::CancelFn
    });
    let stuck = spawn_stuck_detector(
        activity.clone(),
        first.clone(),
        frontend.grace_timeout(),
        frontend.stuck_timeout(),
        Duration::ZERO,
        cancel_grace,
    );
    let (done_tx, done) = std::sync::mpsc::sync_channel(1);
    let started = chrono::Utc::now();
    let id = handle.id.clone();
    let output_tail = tail.clone();
    frontend.report_status(AgentStatus::Running {
        container_name: handle.name.clone(),
    });
    tokio::spawn(async move {
        let mut events = session.events;
        let outcome = loop {
            match events.recv().await {
                Ok(ExecEvent::Stdout(bytes)) => {
                    output_tail.push_bytes(&bytes);
                    first.store(true, Ordering::Release);
                    if let Ok(mut a) = activity.lock() {
                        *a = Some(Instant::now());
                    }
                    let _ = io.stdout.send(bytes);
                }
                Ok(ExecEvent::Stderr(bytes)) => {
                    output_tail.push_bytes(&bytes);
                    first.store(true, Ordering::Release);
                    if let Ok(mut a) = activity.lock() {
                        *a = Some(Instant::now());
                    }
                    let _ = io.stderr.send(bytes);
                }
                Ok(ExecEvent::Exited(code)) => {
                    frontend.report_status(AgentStatus::Exited(code));
                    break Ok(AgentExitInfo {
                        exit_code: code,
                        signal: None,
                        started_at: started,
                        ended_at: chrono::Utc::now(),
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // A slow consumer missed output; the agent is still running.
                    tracing::warn!(skipped, "builtin exec output lagged; frames dropped");
                }
                Ok(ExecEvent::Failed) | Err(_) => {
                    frontend
                        .report_status(AgentStatus::Failed("builtin exec stream failed".into()));
                    break Err(failed());
                }
            }
        };
        input_task.abort();
        resize_task.abort();
        // Give already connected attach clients the terminal frame before the
        // listener is closed. Their tasks hold independent event subscriptions.
        if let Some(server) = server {
            server.finish();
        }
        if let Some((driver, token, remove)) = cleanup {
            let _ = tokio::task::spawn_blocking(move || {
                driver.finish_owned(&id, &token, Duration::from_secs(2), remove)
            })
            .await;
        }
        finished.store(true, Ordering::Release);
        let _ = done_tx.send(outcome);
    });
    Ok(AgentExecution::new(
        handle,
        Box::new(Execution {
            done,
            control,
            input: injector,
            owner,
            _leases: input.leases,
        }),
        stuck,
        Some(tail),
    ))
}

struct Server {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    fn finish(self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
fn serve(path: &Path, session: &ExecSession) -> Result<Server, EngineError> {
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
    Ok(Server {
        path: path.into(),
        task,
    })
}
async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    tag: u8,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_u8(tag).await?;
    writer.write_u32_le(bytes.len() as u32).await?;
    writer.write_all(bytes).await
}
async fn read_frame(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<(u8, Vec<u8>)> {
    let tag = reader.read_u8().await?;
    let length = reader.read_u32_le().await? as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::other("attach frame exceeds bound"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok((tag, bytes))
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
                    Ok((INPUT,bytes))=>{if control.send(Control::Stdin(bytes)).is_err(){return;}},
                    Ok((RESIZE,bytes)) if bytes.len()==4=>{let cols=u16::from_le_bytes([bytes[0],bytes[1]]);let rows=u16::from_le_bytes([bytes[2],bytes[3]]);let _=control.send(Control::Resize(cols,rows));},
                    _=>return,
                } break;},
                event=events.recv()=>{
                    // A slow attach client skips frames it missed; the agent is
                    // still running, so this is never reported as a failure.
                    if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) { continue; }
                    let (tag,bytes,terminal)=match event {Ok(ExecEvent::Stdout(b))=>(STDOUT,b,false),Ok(ExecEvent::Stderr(b))=>(STDERR,b,false),Ok(ExecEvent::Exited(c))=>(EXIT,c.to_le_bytes().to_vec(),true),_=>(FAILED,Vec::new(),true)};
                    let mut error=false;
                    if bytes.is_empty(){error=write_frame(&mut writer,tag,&[]).await.is_err();}else{for chunk in bytes.chunks(MAX_FRAME){if write_frame(&mut writer,tag,chunk).await.is_err(){error=true;break;}}}
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
        let stream = std::os::unix::net::UnixStream::connect(&path)
            .map_err(|e| EngineError::io(&path, e))?;
        stream
            .set_nonblocking(true)
            .map_err(|e| EngineError::io(&path, e))?;
        let stream =
            tokio::net::UnixStream::from_std(stream).map_err(|e| EngineError::io(&path, e))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let control: Arc<dyn ExecControl> = Arc::new(AttachControl(tx));
        let (broadcast, events) = tokio::sync::broadcast::channel(4096);
        tokio::spawn(client(stream, rx, broadcast.clone()));
        run(
            self.handle,
            ExecSession {
                events,
                broadcast,
                control,
            },
            frontend,
            None,
            None,
            true,
            RunInput {
                seeded_prompt: None,
                leases: Vec::new(),
            },
        )
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
                    let (event,terminal)=match frame {Ok((STDOUT,b))=>(ExecEvent::Stdout(b),false),Ok((STDERR,b))=>(ExecEvent::Stderr(b),false),Ok((EXIT,b)) if b.len()==4=>(ExecEvent::Exited(i32::from_le_bytes(b.try_into().unwrap())),true),_=>(ExecEvent::Failed,true)};
                    let _=events.send(event);if terminal{return;} break;
                },
                command=commands.recv()=>{
                    let (tag,bytes)=match command {Some(Control::Stdin(b))=>(INPUT,b),Some(Control::Resize(c,r))=>(RESIZE,[c.to_le_bytes(),r.to_le_bytes()].concat()),Some(Control::Interrupt)=>{let _=events.send(ExecEvent::Failed);return;},Some(Control::Eof)=>continue,_=>return};
                    if write_frame(&mut writer,tag,&bytes).await.is_err(){let _=events.send(ExecEvent::Failed);return;}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct RecordingControl(tokio::sync::mpsc::UnboundedSender<Control>);
    impl ExecControl for RecordingControl {
        fn send(&self, command: Control) -> Result<(), EngineError> {
            self.0.send(command).map_err(|_| failed())
        }
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
        peer.write_all(&[INPUT, 3, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(events.send(ExecEvent::Stdout(b"out".to_vec())).is_ok());
        assert_eq!(
            read_frame(&mut peer).await.unwrap(),
            (STDOUT, b"out".to_vec())
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
        peer.write_all(&[STDERR, 3, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        commands.send(Control::Stdin(b"in".to_vec())).unwrap();
        assert_eq!(
            read_frame(&mut peer).await.unwrap(),
            (INPUT, b"in".to_vec())
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
        let server = serve(&path, &session).unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        write_frame(&mut stream, INPUT, &[0, 255, 13])
            .await
            .unwrap();
        let command = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(command, Control::Stdin(bytes) if bytes == [0,255,13]));
        write_frame(&mut stream, RESIZE, &[80, 0, 24, 0])
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
            (STDERR, vec![0, 255, 10])
        );
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            (EXIT, 37_i32.to_le_bytes().to_vec())
        );
        server.finish();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn lagging_attach_client_still_receives_the_real_exit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("attach.sock");
        let (broadcast, events) = tokio::sync::broadcast::channel(2);
        let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
        let session = ExecSession {
            events,
            broadcast,
            control: Arc::new(RecordingControl(commands)),
        };
        let server = serve(&path, &session).unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        // Let the server subscribe before flooding the 2-slot channel.
        while session.broadcast.receiver_count() < 2 {
            tokio::task::yield_now().await;
        }
        for byte in 0..8u8 {
            let _ = session.broadcast.send(ExecEvent::Stdout(vec![byte]));
        }
        let _ = session.broadcast.send(ExecEvent::Exited(0));
        let last = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (tag, bytes) = read_frame(&mut stream).await.unwrap();
                if tag != STDOUT {
                    return (tag, bytes);
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(last, (EXIT, 0_i32.to_le_bytes().to_vec()));
        server.finish();
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
        let server = serve(&path, &session).unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        write_frame(&mut stream, INPUT, b"ready").await.unwrap();
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
        write_frame(&mut writer, STDERR, &[0, 255, 13, 10])
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (STDERR, vec![0, 255, 13, 10])
        );
        write_frame(&mut writer, EXIT, &37_i32.to_le_bytes())
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (EXIT, 37_i32.to_le_bytes().to_vec())
        );
    }
    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        writer.write_u8(STDOUT).await.unwrap();
        writer.write_u32_le((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }
}

#[cfg(test)]
mod bridge_tests;
