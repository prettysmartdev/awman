use super::*;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::{frontend::AgentProgress, AgentStats};
use crate::engine::auth::{credential::SecretString, redaction::RedactionSet};
use awman_test_support::{TestCase, TestSandbox};

struct Driver {
    stop_succeeds: bool,
    stopped: AtomicBool,
}
impl SandboxDriver for Driver {
    fn create(&self, _: SandboxSpec) -> Result<(), EngineError> {
        unreachable!()
    }
    fn get(&self, _: &str) -> Result<SandboxSummary, EngineError> {
        unreachable!()
    }
    fn list(&self) -> Result<Vec<SandboxSummary>, EngineError> {
        unreachable!()
    }
    fn exec(&self, _: &str, _: ExecRequest) -> Result<ExecSession, EngineError> {
        unreachable!()
    }
    fn finish_owned(
        &self,
        id: &str,
        owner: &str,
        _: Duration,
        remove: bool,
    ) -> Result<(), EngineError> {
        assert_eq!(id, "step");
        assert_eq!(owner, "launch-owner");
        assert!(remove);
        self.stopped.store(true, Ordering::Release);
        if self.stop_succeeds {
            Ok(())
        } else {
            Err(EngineError::Other("stop failed".into()))
        }
    }
    fn remove_stopped(&self, _: &str, _: &str) -> Result<(), EngineError> {
        unreachable!()
    }
    fn stats(&self, _: &str) -> Result<AgentStats, EngineError> {
        unreachable!()
    }
    fn image_config(&self, _: &str) -> Result<Option<SandboxImageConfig>, EngineError> {
        unreachable!()
    }
    fn import_archive(&self, _: PathBuf, _: String) -> Result<(), EngineError> {
        unreachable!()
    }
    fn remove_image(&self, _: &str) -> Result<(), EngineError> {
        unreachable!()
    }
}
struct ControlSink;
impl ExecControl for ControlSink {
    fn send(&self, _: Control) -> Result<(), EngineError> {
        Ok(())
    }
}
struct Frontend(RedactionSet);
impl UserMessageSink for Frontend {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl AgentFrontend for Frontend {
    fn output_redaction(&self) -> RedactionSet {
        self.0.clone()
    }
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        unreachable!()
    }
}

enum Failure {
    Guest,
    Lagged,
    Closed,
}

async fn failed_execution(stop_succeeds: bool, failure: Failure) -> EngineError {
    let driver = Arc::new(Driver {
        stop_succeeds,
        stopped: AtomicBool::new(false),
    });
    let (broadcast, events) = tokio::sync::broadcast::channel(2);
    if matches!(failure, Failure::Lagged) {
        // Overflow before starting the observer, so this cannot depend on task timing.
        for _ in 0..3 {
            assert!(broadcast
                .send(ExecEvent::Stdout(b"output".to_vec()))
                .is_ok());
        }
    } else if matches!(failure, Failure::Guest) {
        assert!(broadcast
            .send(ExecEvent::Failed("worker lost secret-token".into()))
            .is_ok());
    }
    let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let redaction = RedactionSet::new();
    redaction.add(SecretString::new("secret-token"));
    let mut execution = ExecBridge::new(ExecBridgeOptions {
        handle: AgentHandle {
            id: "step".into(),
            name: "step".into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        },
        session: ExecSession {
            events,
            broadcast,
            control: Arc::new(ControlSink),
        },
        cleanup: Some(CleanupTarget {
            driver: driver.clone(),
            owner: "launch-owner".into(),
            remove_on_exit: true,
        }),
        attach_path: None,
        persistent_stdin: false,
        seeded_prompt: None,
        leases: vec![],
        prompt_files: vec![],
        io: AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        cancel_grace: DEFAULT_CANCEL_GRACE,
        startup_grace: None,
        output_tail: Arc::new(OutputTail::with_default_capacity()),
    })
    .run(Box::new(Frontend(redaction)))
    .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), execution.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        driver.stopped.load(Ordering::Acquire),
        "wait must observe cleanup before allowing a retry"
    );
    error
}

#[tokio::test]
async fn stream_failure_preserves_redacted_reason_and_stop_confirmation() {
    let _sandbox = TestSandbox::for_test(TestCase::component(
        "engine::container::builtin::exec_bridge::contracts::stream_failure_preserves_redacted_reason_and_stop_confirmation",
    ));
    for stop_succeeds in [true, false] {
        let error = failed_execution(stop_succeeds, Failure::Guest).await;
        assert!(
            matches!(&error, EngineError::ExecutionStreamFailed { stopped, .. } if *stopped == stop_succeeds)
        );
        assert!(error.to_string().contains("worker lost ***"));
        assert!(!error.to_string().contains("secret-token"));
    }
}

#[tokio::test]
async fn lagged_output_is_a_recoverable_stream_failure_only_after_stop() {
    let _sandbox = TestSandbox::for_test(TestCase::component(
        "engine::container::builtin::exec_bridge::contracts::lagged_output_is_a_recoverable_stream_failure_only_after_stop",
    ));
    let error = failed_execution(true, Failure::Lagged).await;
    assert!(matches!(
        &error,
        EngineError::ExecutionStreamFailed { stopped: true, .. }
    ));
    assert!(error.to_string().contains("lost 1 events"));
}

#[tokio::test]
async fn closed_channel_reports_unknown_exit_after_cleanup() {
    let _sandbox = TestSandbox::for_test(TestCase::component(
        "engine::container::builtin::exec_bridge::contracts::closed_channel_reports_unknown_exit_after_cleanup",
    ));
    let error = failed_execution(true, Failure::Closed).await;
    assert!(matches!(
        &error,
        EngineError::ExecutionStreamFailed { stopped: true, .. }
    ));
    assert!(error
        .to_string()
        .contains("exec event channel: channel closed"));
}

#[tokio::test(start_paused = true)]
async fn resolved_phase_options_preserve_quiet_start_and_ordered_redacted_failure_output() {
    use std::io::Read;
    struct PhaseFrontend;
    impl UserMessageSink for PhaseFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl AgentFrontend for PhaseFrontend {
        fn report_status(&mut self, _: AgentStatus) {}
        fn report_progress(&mut self, _: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            unreachable!()
        }
        fn grace_timeout(&self) -> Duration {
            Duration::ZERO
        }
        fn output_redaction(&self) -> RedactionSet {
            let set = RedactionSet::new();
            set.add(SecretString::new("phase-secret"));
            set
        }
    }
    let s = TestSandbox::new();
    let driver = Arc::new(Driver {
        stop_succeeds: true,
        stopped: AtomicBool::new(false),
    });
    let (broadcast, events) = tokio::sync::broadcast::channel(1024);
    let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let tail = Arc::new(OutputTail::spooled(&s.awman_home()).unwrap());
    let mut execution = ExecBridge::new(ExecBridgeOptions {
        handle: AgentHandle {
            id: "step".into(),
            name: "step".into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        },
        session: ExecSession {
            events,
            broadcast: broadcast.clone(),
            control: Arc::new(ControlSink),
        },
        cleanup: Some(CleanupTarget {
            driver: driver.clone(),
            owner: "launch-owner".into(),
            remove_on_exit: true,
        }),
        attach_path: None,
        persistent_stdin: true,
        seeded_prompt: None,
        leases: vec![],
        prompt_files: vec![],
        io: AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        cancel_grace: Duration::ZERO,
        startup_grace: Some(Duration::MAX),
        output_tail: tail.clone(),
    })
    .run(Box::new(PhaseFrontend))
    .unwrap();
    let mut startup_events = execution.subscribe_stuck();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        startup_events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(!driver.stopped.load(Ordering::Acquire));
    // Alternate the streams in one event sequence. Finish each line with
    // nonsecret trailing bytes so redaction never needs a cross-event hold.
    for n in 0..250 {
        let bytes =
            format!("line-{n:03}: phase-secret trailing-padding-xxxxxxxxxxxxxxxx\n").into_bytes();
        assert!(broadcast
            .send(if n % 2 == 0 {
                ExecEvent::Stdout(bytes)
            } else {
                ExecEvent::Stderr(bytes)
            })
            .is_ok());
    }
    assert!(broadcast.send(ExecEvent::Exited(37)).is_ok());
    assert_eq!(execution.wait().await.unwrap().exit_code, 37);
    let mut transcript = String::new();
    tail.transcript_reader()
        .unwrap()
        .read_to_string(&mut transcript)
        .unwrap();
    assert!(!transcript.contains("phase-secret"));
    let lines = transcript.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 250);
    for (n, line) in lines.iter().enumerate() {
        assert!(line.starts_with(&format!("line-{n:03}: ")), "{line}");
    }
    assert_eq!(tail.snapshot().len(), 100);
    assert!(driver.stopped.load(Ordering::Acquire));
}

#[tokio::test]
async fn quiet_phase_deadline_cancels_with_startup_grace_disabled() {
    struct ExitOnInterrupt(tokio::sync::broadcast::Sender<ExecEvent>);
    impl ExecControl for ExitOnInterrupt {
        fn send(&self, command: Control) -> Result<(), EngineError> {
            if matches!(command, Control::Interrupt) {
                let _ = self.0.send(ExecEvent::Exited(130));
            }
            Ok(())
        }
    }
    let s = TestSandbox::new();
    let driver = Arc::new(Driver {
        stop_succeeds: true,
        stopped: AtomicBool::new(false),
    });
    let (broadcast, events) = tokio::sync::broadcast::channel(16);
    let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut execution = ExecBridge::new(ExecBridgeOptions {
        handle: AgentHandle {
            id: "step".into(),
            name: "step".into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        },
        session: ExecSession {
            events,
            broadcast: broadcast.clone(),
            control: Arc::new(ExitOnInterrupt(broadcast)),
        },
        cleanup: Some(CleanupTarget {
            driver: driver.clone(),
            owner: "launch-owner".into(),
            remove_on_exit: true,
        }),
        attach_path: None,
        persistent_stdin: true,
        seeded_prompt: None,
        leases: vec![],
        prompt_files: vec![],
        io: AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        cancel_grace: Duration::ZERO,
        startup_grace: Some(Duration::MAX),
        output_tail: Arc::new(OutputTail::spooled(&s.awman_home()).unwrap()),
    })
    .run(Box::new(Frontend(RedactionSet::new())))
    .unwrap();
    // An overall deadline owns cancellation independently of the first-byte
    // detector. Dropping wait must still interrupt this entirely quiet phase.
    assert!(tokio::time::timeout(Duration::ZERO, execution.wait())
        .await
        .is_err());
    awman_test_support::wait_for_async(
        "quiet phase cancellation cleanup",
        Duration::from_millis(500),
        || async { driver.stopped.load(Ordering::Acquire).then_some(()) },
    )
    .await;
    assert!(driver.stopped.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropped_wait_releases_warning_sender_before_guest_shutdown() {
    let s = TestSandbox::for_test(TestCase::component("engine::container::builtin::exec_bridge::contracts::dropped_wait_releases_warning_sender_before_guest_shutdown"));
    let (broadcast, events) = tokio::sync::broadcast::channel(16);
    let (stdout, mut out) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, mut err) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let tail = Arc::new(OutputTail::spooled(&s.awman_home()).unwrap());
    let mut execution = ExecBridge::new(ExecBridgeOptions {
        handle: AgentHandle {
            id: "step".into(),
            name: "step".into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        },
        session: ExecSession {
            events,
            broadcast: broadcast.clone(),
            control: Arc::new(ControlSink),
        },
        cleanup: None,
        attach_path: None,
        persistent_stdin: true,
        seeded_prompt: None,
        leases: vec![],
        prompt_files: vec![],
        io: AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        cancel_grace: Duration::from_secs(10),
        startup_grace: Some(Duration::MAX),
        output_tail: tail.clone(),
    })
    .run(Box::new(Frontend(RedactionSet::new())))
    .unwrap();
    assert!(tokio::time::timeout(Duration::ZERO, execution.wait())
        .await
        .is_err());
    tokio::time::timeout(Duration::from_millis(200), async {
        assert!(out.recv().await.is_none());
        assert!(err.recv().await.is_none());
    })
    .await
    .expect("both drains close while guest and transcript are retained");
    assert!(broadcast.send(ExecEvent::Exited(0)).is_ok());
    drop(execution);
    drop(tail);
    drop(broadcast);
    s.finish();
}
