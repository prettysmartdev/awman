//! Move blocking local CLI work off Tokio workers and cancel it if ready is
//! dropped while the builder is running.

use std::sync::Arc;

use crate::data::oci_identity::OciPlatform;
use crate::data::setup_step::SetupStep;
use crate::data::step_status::StepStatus;
use crate::engine::error::EngineError;
use crate::engine::oci::CancelToken;

use super::builder::{
    build_control, LocalBuilderTarget, LocalImageBuildRequest, LocalImageBuilder, SourceImage,
};
use super::frontend::ReadyFrontend;
use crate::data::oci_identity::BuiltImageIdentity;

/// Cancels `token` when dropped while armed, so a ready future dropped
/// mid-await (session timeout, TUI close) stops its blocking worker.
pub(super) struct CancelOnDrop(Option<CancelToken>);

impl CancelOnDrop {
    pub(super) fn arm(token: CancelToken) -> Self {
        Self(Some(token))
    }

    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

/// Run blocking work (filesystem receipts, lock waits) off the async
/// worker, cancelling `cancel` if the awaiting future is dropped.
pub(super) async fn blocking<T: Send + 'static>(
    cancel: CancelToken,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, EngineError> {
    let guard = CancelOnDrop::arm(cancel);
    let result = tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| EngineError::Other("image builder worker failed".into()));
    guard.disarm();
    result
}

pub(super) async fn inspect(
    builder: Arc<dyn LocalImageBuilder>,
    target: LocalBuilderTarget,
    reference: String,
    platform: OciPlatform,
    cancel: CancelToken,
) -> Result<SourceImage, EngineError> {
    // The builder bounds its own probe and inspection calls.
    let control = build_control(cancel.clone());
    blocking(cancel, move || {
        builder.inspect(&target, &reference, &platform, &control)
    })
    .await?
}

/// Run one build, streaming its output through the same `SetupStep::output`
/// rows ordinary runtime builds use.
pub(super) async fn run(
    builder: Arc<dyn LocalImageBuilder>,
    request: LocalImageBuildRequest,
    cancel: CancelToken,
    frontend: &mut dyn ReadyFrontend,
) -> Result<BuiltImageIdentity, EngineError> {
    let guard = CancelOnDrop::arm(cancel.clone());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let worker = tokio::task::spawn_blocking(move || {
        let control = build_control(cancel);
        builder.build(&request, &control, &mut |line| {
            let _ = tx.blocking_send(line.to_string());
        })
    });
    while let Some(line) = rx.recv().await {
        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
    }
    let result = worker
        .await
        .map_err(|_| EngineError::Other("image builder worker failed".into()))?;
    guard.disarm();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::{RecordingMessageSink, UserMessage, UserMessageSink};
    use crate::engine::container::host_cli_backend::BuildControl;
    use std::sync::mpsc;
    use std::sync::Mutex;

    fn platform() -> OciPlatform {
        OciPlatform::host_linux()
    }

    fn identity(c: char) -> BuiltImageIdentity {
        let digest = crate::data::oci_identity::Digest::parse(&format!(
            "sha256:{}",
            c.to_string().repeat(64)
        ))
        .unwrap();
        BuiltImageIdentity::new(Some(digest), None, platform()).unwrap()
    }

    // ── CancelOnDrop ───────────────────────────────────────────────────

    #[test]
    fn armed_cancel_on_drop_cancels_the_token_when_dropped() {
        let token = CancelToken::new();
        let guard = CancelOnDrop::arm(token.clone());
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(token.is_cancelled(), "an armed guard must cancel on drop");
    }

    #[test]
    fn disarmed_cancel_on_drop_leaves_the_token_untouched() {
        let token = CancelToken::new();
        let guard = CancelOnDrop::arm(token.clone());
        guard.disarm();
        assert!(
            !token.is_cancelled(),
            "disarm() must stop the guard from cancelling on drop"
        );
    }

    // ── blocking() ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn blocking_runs_the_closure_off_the_async_worker_and_returns_its_value() {
        let token = CancelToken::new();
        let result = blocking(token.clone(), || 1 + 1).await.unwrap();
        assert_eq!(result, 2);
        assert!(
            !token.is_cancelled(),
            "a closure that runs to completion must not leave the token cancelled"
        );
    }

    // ── Fake LocalImageBuilder for run()/inspect() ───────────────────────

    enum BuildBehavior {
        Immediate(Result<BuiltImageIdentity, EngineError>),
        /// Blocks until a scripted cancellation is observed through
        /// `BuildControl`, proving the real cancel path (not just a
        /// pre-baked error) stops the build.
        BlockUntilCancelled {
            started: mpsc::Sender<()>,
        },
    }

    struct FakeBuilder {
        inspect_result: Mutex<Option<Result<SourceImage, EngineError>>>,
        build_lines: Vec<String>,
        behavior: Mutex<Option<BuildBehavior>>,
    }

    impl FakeBuilder {
        fn new() -> Self {
            Self {
                inspect_result: Mutex::new(None),
                build_lines: Vec::new(),
                behavior: Mutex::new(None),
            }
        }
    }

    impl LocalImageBuilder for FakeBuilder {
        fn probe(
            &self,
            _target: &LocalBuilderTarget,
            _control: &BuildControl,
        ) -> Result<(), EngineError> {
            Ok(())
        }
        fn inspect(
            &self,
            _target: &LocalBuilderTarget,
            _reference: &str,
            _platform: &OciPlatform,
            _control: &BuildControl,
        ) -> Result<SourceImage, EngineError> {
            self.inspect_result
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Ok(SourceImage::Absent))
        }
        fn build(
            &self,
            _request: &LocalImageBuildRequest,
            control: &BuildControl,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<BuiltImageIdentity, EngineError> {
            for line in &self.build_lines {
                on_line(line);
            }
            match self.behavior.lock().unwrap().take() {
                Some(BuildBehavior::Immediate(result)) => result,
                Some(BuildBehavior::BlockUntilCancelled { started }) => {
                    let _ = started.send(());
                    loop {
                        control.check()?;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                None => Ok(identity('a')),
            }
        }
    }

    struct TestFrontend {
        sink: RecordingMessageSink,
        steps: Vec<(String, StepStatus)>,
    }

    impl TestFrontend {
        fn new() -> Self {
            Self {
                sink: RecordingMessageSink::new(),
                steps: Vec::new(),
            }
        }
    }

    impl UserMessageSink for TestFrontend {
        fn write_message(&mut self, message: UserMessage) {
            self.sink.write_message(message);
        }
        fn replay_queued(&mut self) {}
    }
    impl crate::engine::agent::AgentImageFrontend for TestFrontend {
        fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
            self.steps.push((step.to_string(), status));
        }
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            unreachable!("build::run never asks for a container frontend")
        }
    }
    impl ReadyFrontend for TestFrontend {
        fn ask_create_dockerfile(&mut self, _: &std::path::Path) -> Result<bool, EngineError> {
            Ok(false)
        }
        fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
            Ok(false)
        }
        fn report_phase(&mut self, _phase: &crate::data::ready_phase::ReadyPhase) {}
        fn report_summary(&mut self, _summary: &crate::data::ready_summary::ReadySummary) {}
    }

    fn request() -> LocalImageBuildRequest {
        LocalImageBuildRequest {
            target: LocalBuilderTarget::Apple,
            reference: "awman-repo-claude:latest".into(),
            dockerfile: "/repo/Dockerfile.dev".into(),
            context: "/repo".into(),
            platform: platform(),
            build_args: Vec::new(),
            no_cache: false,
        }
    }

    #[tokio::test]
    async fn run_streams_build_output_through_report_step_status_and_returns_the_identity() {
        let builder = Arc::new(FakeBuilder {
            build_lines: vec!["step 1/3".into(), "step 2/3".into()],
            ..FakeBuilder::new()
        });
        *builder.behavior.lock().unwrap() = Some(BuildBehavior::Immediate(Ok(identity('b'))));
        let mut frontend = TestFrontend::new();
        let result = run(builder, request(), CancelToken::new(), &mut frontend)
            .await
            .unwrap();
        assert_eq!(result, identity('b'));
        let lines: Vec<_> = frontend
            .steps
            .iter()
            .filter_map(|(label, status)| {
                matches!(status, StepStatus::Running).then_some(label.clone())
            })
            .collect();
        assert!(lines.iter().any(|l| l.contains("step 1/3")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("step 2/3")), "{lines:?}");
    }

    #[tokio::test]
    async fn run_propagates_a_build_failure() {
        let builder = Arc::new(FakeBuilder::new());
        *builder.behavior.lock().unwrap() = Some(BuildBehavior::Immediate(Err(
            EngineError::Container("no space left".into()),
        )));
        let mut frontend = TestFrontend::new();
        let err = run(builder, request(), CancelToken::new(), &mut frontend)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no space left"), "{err}");
    }

    #[tokio::test]
    async fn inspect_returns_the_builder_result() {
        let builder = Arc::new(FakeBuilder::new());
        *builder.inspect_result.lock().unwrap() = Some(Ok(SourceImage::Absent));
        let found = inspect(
            builder,
            LocalBuilderTarget::Apple,
            "awman-repo-claude:latest".into(),
            platform(),
            CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(found, SourceImage::Absent);
    }

    /// The doc comment's whole reason to exist: a ready future dropped while
    /// its build is in flight must cancel that build's `BuildControl`, not
    /// strand it. `run`'s own `CancelOnDrop` is what makes this true;
    /// without it a dropped future would leave the blocking build running
    /// forever with no way to stop it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_run_future_mid_build_cancels_the_build_control() {
        let (started_tx, started_rx) = mpsc::channel();
        let builder = Arc::new(FakeBuilder::new());
        *builder.behavior.lock().unwrap() = Some(BuildBehavior::BlockUntilCancelled {
            started: started_tx,
        });
        let cancel = CancelToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            let builder = builder.clone();
            async move {
                // `frontend` only needs to live as long as this task; a
                // `'static` owned copy sidesteps borrowing it across the
                // spawn boundary.
                let mut frontend = TestFrontend::new();
                let _ = run(builder, request(), cancel, &mut frontend).await;
            }
        });

        // Wait for the blocking build to actually start before aborting, so
        // the abort genuinely interrupts in-flight work rather than racing
        // task startup.
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("build must start");
        handle.abort();
        let _ = handle.await;

        // Give the detached blocking thread a moment to observe the
        // cancellation and exit its poll loop.
        for _ in 0..200 {
            if cancel.is_cancelled() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cancel.is_cancelled(),
            "aborting the run future must cancel the build's token"
        );
    }
}
