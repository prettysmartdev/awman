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
    fn remove_stopped(&self, _: &str) -> Result<(), EngineError> {
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
