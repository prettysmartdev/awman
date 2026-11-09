//! Lossless, ordered output for a relayed CLI. Separate from TUI log lines.
use crate::data::{
    frontend_protocol::OutputStream,
    message::{MessageLevel, UserMessage},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
pub enum OutputEvent {
    Bytes(OutputStream, Vec<u8>),
    Message(MessageLevel, String),
}
#[derive(Default)]
struct State {
    #[cfg(any(test, feature = "test-support"))]
    trace: Option<Arc<super::trace::OutcomeTrace>>,
    pending: Vec<OutputEvent>,
    tail: Vec<OutputEvent>,
    drains: usize,
    defer_tail: bool,
}
#[derive(Clone, Default)]
pub struct CommandBytes {
    state: Arc<Mutex<State>>,
    finished: Arc<tokio::sync::Notify>,
    enabled: Arc<AtomicBool>,
}
impl CommandBytes {
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_trace(&self, trace: Arc<super::trace::OutcomeTrace>) {
        self.state.lock().unwrap().trace = Some(trace);
    }
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    pub fn drain(&self) -> Vec<OutputEvent> {
        std::mem::take(&mut self.state.lock().unwrap().pending)
    }
    pub fn message(&self, message: &UserMessage) {
        let mut state = self.state.lock().unwrap();
        let event = OutputEvent::Message(message.level, message.text.clone());
        if state.defer_tail && state.drains > 0 {
            state.tail.push(event);
        } else {
            state.pending.push(event);
        }
    }
    /// AgentExecution has finished: status written now follows its final bytes.
    pub fn ended(&self) {
        self.state.lock().unwrap().defer_tail = true;
    }
    pub fn started(&self) {
        self.state.lock().unwrap().defer_tail = false;
    }
    pub async fn wait_drained(&self) {
        loop {
            let notified = self.finished.notified();
            if self.state.lock().unwrap().drains == 0 {
                return;
            }
            notified.await;
        }
    }
    pub fn pipe(
        &self,
        wake: Option<Arc<tokio::sync::Notify>>,
    ) -> crate::engine::agent_runtime::frontend::AgentIo {
        let (stdout, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        for (stream, mut rx) in [
            (OutputStream::Stdout, stdout_rx),
            (OutputStream::Stderr, stderr_rx),
        ] {
            let output = self.clone();
            let wake = wake.clone();
            self.state.lock().unwrap().drains += 1;
            tokio::spawn(async move {
                while let Some(bytes) = rx.recv().await {
                    #[cfg(any(test, feature = "test-support"))]
                    if let Some(trace) = &output.state.lock().unwrap().trace {
                        trace.record(
                            match stream {
                                OutputStream::Stdout => "stdout",
                                OutputStream::Stderr => "stderr",
                            },
                            &bytes,
                        );
                    }
                    output
                        .state
                        .lock()
                        .unwrap()
                        .pending
                        .push(OutputEvent::Bytes(stream, bytes));
                    if let Some(w) = &wake {
                        w.notify_one();
                    }
                }
                {
                    let mut state = output.state.lock().unwrap();
                    state.drains -= 1;
                    if state.drains == 0 {
                        let mut tail = std::mem::take(&mut state.tail);
                        state.pending.append(&mut tail);
                    }
                }
                output.finished.notify_one();
                if let Some(w) = wake {
                    w.notify_one();
                }
            });
        }
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}
