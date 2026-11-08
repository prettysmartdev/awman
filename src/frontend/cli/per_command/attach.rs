//! CLI presentation for `awman attach`.
use async_trait::async_trait;
use std::io::{IsTerminal, Write};
use tokio_util::sync::CancellationToken;

use crate::command::commands::attach::{initial_client_frames, AttachCommandFrontend};
use crate::command::error::CommandError;
use crate::data::ids::TerminalId;
use crate::data::terminal::{DetachReason, HostOutputPolicy, TerminalClientFrame, TerminalFrame};
use crate::engine::remote::terminal_client::TerminalStream;
use crate::frontend::cli::command_frontend::RawModeGuard;
use crate::frontend::cli::CliFrontend;
use crate::frontend::terminal_output_filter::HostOutputFilter;

fn output_filter(tty: bool, policy: HostOutputPolicy) -> Option<HostOutputFilter> {
    tty.then(|| HostOutputFilter::new(policy))
}
fn safe_output(filter: &mut Option<HostOutputFilter>, bytes: &[u8]) -> Vec<u8> {
    let mut safe = Vec::new();
    if let Some(filter) = filter {
        filter.feed(bytes, &mut safe);
    } else {
        safe.extend_from_slice(bytes);
    }
    safe
}

// Ctrl-C remains a viewer control even for a read-only session. Preserve the
// input preceding it; no byte following the detach key is sent to the agent.
fn input_before_detach(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.iter().position(|b| *b == 3) {
        Some(at) => (&bytes[..at], true),
        None => (bytes, false),
    }
}

#[cfg(unix)]
fn spawn_input(
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    stop: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::io::Read;
        use std::os::fd::AsFd;
        let mut stdin = std::io::stdin();
        let mut buffer = [0; 1024];
        while !stop.is_cancelled() {
            // A bounded poll avoids Tokio's permanently blocked stdin worker
            // keeping the process alive after the viewer detaches.
            let ready = {
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
                poll(&mut fds, PollTimeout::from(50u16)).unwrap_or(0)
            };
            if ready < 0 {
                break;
            }
            if ready == 0 {
                continue;
            }
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => loop {
                    if stop.is_cancelled() {
                        return;
                    }
                    match tx.try_send(buffer[..n].to_vec()) {
                        Ok(()) => break,
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            std::thread::park_timeout(std::time::Duration::from_millis(10))
                        }
                    }
                },
            }
        }
    })
}

#[cfg(not(unix))]
fn spawn_input(
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    stop: CancellationToken,
) -> std::thread::JoinHandle<()> {
    // A blocked OS reader is detached at teardown; unlike tokio::io::stdin
    // it cannot keep the async runtime waiting for its blocking pool to exit.
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buffer = [0; 1024];
        while !stop.is_cancelled() {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stop.is_cancelled() || tx.blocking_send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[async_trait]
impl AttachCommandFrontend for CliFrontend {
    fn begin_attach(&mut self, _: &TerminalId) -> CancellationToken {
        let cancel = CancellationToken::new();
        self.attach_cancel = Some(cancel.clone());
        self.attach_hangup = CancellationToken::new();
        self.attach_exit_code
            .store(0, std::sync::atomic::Ordering::Relaxed);
        cancel
    }
    fn hangup(&self) -> CancellationToken {
        self.attach_hangup.clone()
    }
    fn bind_terminal(
        &mut self,
        mut stream: TerminalStream,
        read_only: bool,
        policy: HostOutputPolicy,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError> {
        let tty = std::io::stdout().is_terminal();
        if std::io::stdin().is_terminal() {
            self.raw_mode_guard =
                Some(RawModeGuard::enable().map_err(|e| CommandError::Other(e.to_string()))?);
        }
        let cancel = self.attach_cancel.clone().unwrap_or_default();
        let exit_code = self.attach_exit_code.clone();
        let hangup = self.attach_hangup.clone();
        Ok(tokio::spawn(async move {
            let mut filter = output_filter(tty, policy);
            let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            let stop = CancellationToken::new();
            let input = spawn_input(input_tx, stop.clone());
            let hup = hangup_signal();
            tokio::pin!(hup);
            let mut resize_tick = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut writable = !read_only;
            let mut last_size = crossterm::terminal::size().ok();
            for frame in initial_client_frames(read_only, last_size) {
                if stream.send(frame).await.is_err() {
                    hangup.cancel();
                }
            }
            let mut reason = DetachReason::User;
            loop {
                tokio::select! {
                    biased;
                    _ = hangup.cancelled() => { reason = DetachReason::Hangup; break; }
                    _ = cancel.cancelled() => break,
                    _ = tokio::signal::ctrl_c() => break,
                    _ = &mut hup => { reason = DetachReason::Hangup; break; }
                    _ = resize_tick.tick(), if writable => {
                        let size = crossterm::terminal::size().ok();
                        if size != last_size {
                            if let Some((cols, rows)) = size {
                                if stream.send(TerminalClientFrame::Resize { cols, rows }).await.is_err() { reason = DetachReason::Hangup; break; }
                            }
                            last_size = size;
                        }
                    }
                    Some(bytes) = input_rx.recv() => {
                        let (bytes, detach) = input_before_detach(&bytes);
                        if writable && !bytes.is_empty() && stream.send(TerminalClientFrame::Input { bytes: bytes.to_vec() }).await.is_err() { reason = DetachReason::Hangup; break; }
                        if detach { break; }
                    }
                    frame = stream.next() => match frame {
                        Ok(Some(TerminalFrame::Snapshot { screen, .. } | TerminalFrame::Output { bytes: screen })) => {
                            let safe = safe_output(&mut filter, &screen);
                            let mut out = std::io::stdout().lock();
                            if out.write_all(&safe).and_then(|_| out.flush()).is_err() { reason = DetachReason::Hangup; break; }
                        }
                        Ok(Some(TerminalFrame::Exit { code })) => { exit_code.store(code, std::sync::atomic::Ordering::Relaxed); break; }
                        Ok(Some(TerminalFrame::Error { code, .. })) => {
                            if writable && matches!(code.as_str(), "control_held" | "input_not_allowed") {
                                writable = false;
                                eprintln!("awman: terminal control unavailable; watching read-only");
                            }
                        }
                        Ok(Some(TerminalFrame::Resize { .. } | TerminalFrame::Lagged)) => {}
                        Ok(None) | Err(_) => { reason = DetachReason::Hangup; break; }
                    }
                }
            }
            stop.cancel();
            drop(input_rx);
            #[cfg(unix)]
            let _ = input.join();
            #[cfg(not(unix))]
            drop(input);
            if let Some(filter) = filter.as_mut() {
                let mut safe = Vec::new();
                filter.finish(&mut safe);
                let _ = std::io::stdout().lock().write_all(&safe);
            }
            let _ = stream.send(TerminalClientFrame::Detached { reason }).await;
            let _ = stream.close().await;
            if reason == DetachReason::Hangup {
                hangup.cancel();
            } else {
                cancel.cancel();
            }
        }))
    }
    fn end_attach(&mut self) {
        self.raw_mode_guard.take();
        self.attach_cancel = None;
    }
    fn terminal_exit_code(&self) -> i32 {
        self.attach_exit_code
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

async fn hangup_signal() {
    #[cfg(unix)]
    if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        signal.recv().await;
        return;
    }
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attach_writer_filters_only_when_stdout_is_a_tty() {
        let bytes = b"before\x1b]52;c;Zm9v\x07after";
        assert_eq!(
            safe_output(&mut output_filter(true, HostOutputPolicy::default()), bytes),
            b"beforeafter"
        );
        assert_eq!(
            safe_output(
                &mut output_filter(false, HostOutputPolicy::default()),
                bytes
            ),
            bytes
        );
    }
    #[test]
    fn detach_key_preserves_preceding_input_including_read_only() {
        assert_eq!(
            input_before_detach(b"ls\r\x03ignored"),
            (b"ls\r".as_slice(), true)
        );
        assert_eq!(input_before_detach(b"\x03"), (b"".as_slice(), true));
    }
}

#[cfg(all(test, unix))]
mod session_regression {
    use super::*;
    use crate::engine::remote::terminal_client::TerminalClientOptions;
    use futures_util::{SinkExt, StreamExt};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    #[allow(clippy::result_large_err)] // tungstenite's fixed handshake callback type
    async fn refused_control_and_readonly_viewers_finish_the_user_detach_before_teardown() {
        for read_only in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = temp.path().join("daemon.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let (ready_tx, ready) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                use tokio_tungstenite::tungstenite::{
                    handshake::server::{Request, Response},
                    Message,
                };
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    socket,
                    |_: &Request, mut response: Response| {
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            crate::data::terminal::TERMINAL_SUBPROTOCOL.parse().unwrap(),
                        );
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                if !read_only {
                    let Message::Text(frame) = socket.next().await.unwrap().unwrap() else {
                        panic!()
                    };
                    assert!(matches!(
                        serde_json::from_str::<TerminalClientFrame>(&frame).unwrap(),
                        TerminalClientFrame::TakeControl
                    ));
                    socket
                        .send(Message::Text(
                            r#"{"type":"error","code":"control_held","error":"busy"}"#.into(),
                        ))
                        .await
                        .unwrap();
                }
                ready_tx.send(()).unwrap();
                loop {
                    match socket.next().await.unwrap().unwrap() {
                        Message::Text(text) => {
                            match serde_json::from_str::<TerminalClientFrame>(&text).unwrap() {
                                TerminalClientFrame::Detached { reason } => {
                                    assert_eq!(reason, DetachReason::User);
                                    break;
                                }
                                TerminalClientFrame::Resize { .. } if !read_only => {}
                                frame => panic!("unexpected client frame: {frame:?}"),
                            }
                        }
                        frame => panic!("closed before declaring the detach reason: {frame:?}"),
                    }
                }
                let _ = socket.next().await;
            });
            let id = TerminalId::new();
            let matches = crate::command::dispatch::catalogue::CommandCatalogue::get()
                .build_clap_command()
                .get_matches_from(["awman", "attach", id.as_str()]);
            let mut frontend = CliFrontend::new(matches);
            let cancel = frontend.begin_attach(&id);
            let stream = TerminalStream::connect_local(
                &path,
                &id,
                &TerminalClientOptions {
                    read_only,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let task = frontend
                .bind_terminal(stream, read_only, HostOutputPolicy::default())
                .unwrap();
            ready.await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            assert!(
                !frontend.hangup().is_cancelled(),
                "a refusal must keep the viewer alive"
            );
            cancel.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            frontend.end_attach();
            server.await.unwrap();
        }
    }
}
