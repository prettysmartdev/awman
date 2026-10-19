//! Move blocking local CLI work off Tokio workers and cancel it if ready is
//! dropped while the builder is running.

use std::sync::Arc;

use crate::data::oci_identity::OciPlatform;
use crate::data::setup_step::SetupStep;
use crate::data::step_status::StepStatus;
use crate::engine::error::EngineError;
use crate::engine::oci::CancelToken;

use super::builder::{BuildRequest, BuildTarget, SourceImage};
use super::{AgentImagePreparer, ImagePreparationSink};
use crate::data::oci_identity::BuiltImageIdentity;

use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::{AgentRuntimeEngine, ImageImportRequest, ImportedImage};
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
impl AgentImagePreparer {
    pub(super) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, EngineError> {
        let guard = CancelOnDrop::arm(self.cancel.clone());
        let result = tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| EngineError::Other("image builder worker failed".into()));
        guard.disarm();
        result
    }

    pub(super) async fn inspect_source(
        &mut self,
        target: BuildTarget,
        reference: String,
        platform: OciPlatform,
    ) -> Result<SourceImage, EngineError> {
        let builder = self.builder();
        let cancel = self.cancel.clone();
        // The builder bounds its own probe and inspection calls.
        let control = crate::engine::container::BuildControl::source_build(cancel.clone());
        self.blocking(move || builder.inspect(&target, &reference, &platform, &control))
            .await?
    }
}

/// Fakeable import seam for ready. Production delegates to the selected
/// importing runtime; tests can record ordered requests and return scripted
/// identities or failures without starting a daemon.
/// Implementations must honor refresh and expected_built_identity before
/// publishing an image. Production delegates this validation to CachingAcquirer;
/// the preparer deliberately does not repeat the digest check.
pub trait ImageImporter: Send + Sync {
    fn import(
        &self,
        request: &ImageImportRequest,
        cancel: &CancelToken,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError>;
}

pub struct RuntimeImageImporter(pub Arc<dyn AgentRuntimeEngine>);
impl ImageImporter for RuntimeImageImporter {
    fn import(
        &self,
        request: &ImageImportRequest,
        cancel: &CancelToken,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.0.import_image_cancellable(request, sink, cancel)
    }
}

struct Messages(tokio::sync::mpsc::Sender<UserMessage>);
impl UserMessageSink for Messages {
    fn write_message(&mut self, message: UserMessage) {
        // A receiver dropped by cancellation releases a blocked sender.
        let _ = self.0.blocking_send(message);
    }
    fn replay_queued(&mut self) {}
}

/// The single bounded, cancellation-on-drop bridge for builds and imports.
async fn stream<T: Send + 'static, E: Send + 'static>(
    cancel: CancelToken,
    work: impl FnOnce(tokio::sync::mpsc::Sender<E>) -> Result<T, EngineError> + Send + 'static,
    mut report: impl FnMut(E),
) -> Result<T, EngineError> {
    cancel.check()?;
    let guard = CancelOnDrop::arm(cancel);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let worker = tokio::task::spawn_blocking(move || work(tx));
    while let Some(event) = rx.recv().await {
        report(event);
    }
    let result = worker
        .await
        .map_err(|_| EngineError::Other("image preparation worker failed".into()))?;
    guard.disarm();
    result
}
impl AgentImagePreparer {
    pub(super) async fn build_source(
        &mut self,
        request: BuildRequest,
        sink: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<BuiltImageIdentity, EngineError> {
        let builder = self.builder();
        let cancel = self.cancel.clone();
        stream(
            cancel.clone(),
            move |tx| {
                builder.build(
                    &request,
                    &crate::engine::container::BuildControl::source_build(cancel),
                    &mut |line| {
                        let _ = tx.blocking_send(line.to_string());
                    },
                )
            },
            |line| sink.report_step(&SetupStep::output(line), StepStatus::Running),
        )
        .await
    }
    pub(super) async fn import_source(
        &self,
        request: ImageImportRequest,
        sink: &mut (impl UserMessageSink + ?Sized),
    ) -> Result<ImportedImage, EngineError> {
        let importer = self.image_importer.clone();
        let cancel = self.cancel.clone();
        stream(
            cancel.clone(),
            move |tx| importer.import(&request, &cancel, &mut Messages(tx)),
            |message| sink.write_message(message),
        )
        .await
    }
}

#[cfg(test)]
mod build_tests {
    use super::super::builder::LocalImageBuilder;
    async fn run(
        builder: Arc<dyn LocalImageBuilder>,
        request: BuildRequest,
        cancel: CancelToken,
        sink: &mut impl ImagePreparationSink,
    ) -> Result<BuiltImageIdentity, EngineError> {
        let mut preparer = test_preparer(cancel).with_local_builder(builder);
        preparer.build_source(request, sink).await
    }

    use super::*;
    use crate::data::message::{RecordingMessageSink, UserMessage, UserMessageSink};
    use crate::engine::container::BuildControl;
    use crate::engine::ready::ReadyFrontend;
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
        let result = test_preparer(token.clone())
            .blocking(|| 1 + 1)
            .await
            .unwrap();
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
        fn probe(&self, _target: &BuildTarget, _control: &BuildControl) -> Result<(), EngineError> {
            Ok(())
        }
        fn inspect(
            &self,
            _target: &BuildTarget,
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
            _request: &BuildRequest,
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

    fn request() -> BuildRequest {
        BuildRequest {
            target: BuildTarget::Apple,
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
        let found = test_preparer(CancelToken::new())
            .with_local_builder(builder)
            .inspect_source(
                BuildTarget::Apple,
                "awman-repo-claude:latest".into(),
                platform(),
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

#[cfg(test)]
mod import_tests {
    async fn run(
        importer: Arc<dyn ImageImporter>,
        request: ImageImportRequest,
        cancel: CancelToken,
        sink: &mut impl UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        let preparer = test_preparer(cancel).with_image_importer(importer);
        preparer.import_source(request, sink).await
    }

    use super::*;
    use crate::data::config::image_source::ImageSourceSpec;
    use crate::data::message::{MessageLevel, RecordingMessageSink};
    use crate::data::oci_identity::OciPlatform;
    use std::sync::mpsc;
    use std::sync::Mutex;

    // ── CancelOnDrop ───────────────────────────────────────────────────

    #[test]
    fn armed_cancel_on_drop_cancels_the_token_when_dropped() {
        let token = CancelToken::new();
        let guard = CancelOnDrop(Some(token.clone()));
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(token.is_cancelled(), "an armed guard must cancel on drop");
    }

    #[test]
    fn cleared_cancel_on_drop_leaves_the_token_untouched() {
        let token = CancelToken::new();
        let mut guard = CancelOnDrop(Some(token.clone()));
        guard.0 = None;
        drop(guard);
        assert!(
            !token.is_cancelled(),
            "a guard cleared before drop must not cancel"
        );
    }

    // ── run() ──────────────────────────────────────────────────────────

    fn request() -> ImageImportRequest {
        ImageImportRequest {
            tag: "awman-repo-claude:latest".into(),
            source: ImageSourceSpec::Archive {
                path: "/images/agent.tar".into(),
            },
            platform: OciPlatform::host_linux(),
            refresh: false,
            expected_built_identity: None,
            immutable_export_selector: None,
            registries: Default::default(),
        }
    }

    fn imported(tag: &str) -> ImportedImage {
        let digest =
            crate::data::oci_identity::Digest::parse(&format!("sha256:{}", "a".repeat(64)))
                .unwrap();
        ImportedImage {
            tag: tag.into(),
            identity: crate::data::oci_identity::ImageIdentity {
                reference: tag.into(),
                manifest_digest: digest.clone(),
                config_digest: digest,
                platform: OciPlatform::host_linux(),
                source: crate::data::config::image_source::ImageSourceKind::Archive,
            },
            home_dir: None,
            user: None,
        }
    }

    enum Behavior {
        Immediate(Box<Result<ImportedImage, EngineError>>),
        BlockUntilCancelled { started: mpsc::Sender<()> },
    }

    struct FakeImporter {
        messages: Vec<UserMessage>,
        behavior: Mutex<Option<Behavior>>,
    }

    impl ImageImporter for FakeImporter {
        fn import(
            &self,
            _request: &ImageImportRequest,
            cancel: &CancelToken,
            sink: &mut dyn UserMessageSink,
        ) -> Result<ImportedImage, EngineError> {
            for message in &self.messages {
                sink.write_message(message.clone());
            }
            match self.behavior.lock().unwrap().take() {
                Some(Behavior::Immediate(result)) => *result,
                Some(Behavior::BlockUntilCancelled { started }) => {
                    let _ = started.send(());
                    loop {
                        cancel.check()?;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                None => Ok(imported("awman-repo-claude:latest")),
            }
        }
    }

    #[tokio::test]
    async fn run_streams_import_progress_messages_and_returns_the_identity() {
        let importer = Arc::new(FakeImporter {
            messages: vec![UserMessage {
                level: MessageLevel::Info,
                text: "importing from docker…".into(),
            }],
            behavior: Mutex::new(Some(Behavior::Immediate(Box::new(Ok(imported(
                "awman-repo-claude:latest",
            )))))),
        });
        let mut sink = RecordingMessageSink::new();
        let result = run(importer, request(), CancelToken::new(), &mut sink)
            .await
            .unwrap();
        assert_eq!(result.tag, "awman-repo-claude:latest");
        assert!(sink
            .queued()
            .iter()
            .any(|m| m.text.contains("importing from docker")));
    }

    #[tokio::test]
    async fn run_propagates_an_import_failure() {
        let importer = Arc::new(FakeImporter {
            messages: Vec::new(),
            behavior: Mutex::new(Some(Behavior::Immediate(Box::new(Err(
                EngineError::Container("digest mismatch".into()),
            ))))),
        });
        let mut sink = RecordingMessageSink::new();
        let err = run(importer, request(), CancelToken::new(), &mut sink)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("digest mismatch"), "{err}");
    }

    #[tokio::test]
    async fn run_checks_cancellation_before_starting_the_import() {
        let importer = Arc::new(FakeImporter {
            messages: Vec::new(),
            behavior: Mutex::new(Some(Behavior::Immediate(Box::new(Ok(imported(
                "awman-repo-claude:latest",
            )))))),
        });
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut sink = RecordingMessageSink::new();
        let err = run(importer, request(), cancel, &mut sink)
            .await
            .unwrap_err();
        assert!(crate::engine::oci::retry::is_cancelled(&err), "{err}");
    }

    /// Mirrors `build::dropping_the_run_future_mid_build_cancels_the_build_control`:
    /// dropping this future mid-import must cancel the token the blocking
    /// import worker observes, not strand it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_run_future_mid_import_cancels_the_token() {
        let (started_tx, started_rx) = mpsc::channel();
        let importer = Arc::new(FakeImporter {
            messages: Vec::new(),
            behavior: Mutex::new(Some(Behavior::BlockUntilCancelled {
                started: started_tx,
            })),
        });
        let cancel = CancelToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let mut sink = RecordingMessageSink::new();
                let _ = run(importer, request(), cancel, &mut sink).await;
            }
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("import must start");
        handle.abort();
        let _ = handle.await;

        for _ in 0..200 {
            if cancel.is_cancelled() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cancel.is_cancelled(),
            "aborting the run future must cancel the import's token"
        );
    }
}

#[cfg(test)]
fn test_preparer(cancel: CancelToken) -> AgentImagePreparer {
    use crate::engine::ready::tests::{archive_sources, fakes::FakeImportRuntime, import_engine};
    let (engine, _frontend, _temp) = import_engine(
        Arc::new(FakeImportRuntime::new(false)),
        false,
        archive_sources(),
    );
    let (_, mut preparer, _) = engine.into_image_test_parts();
    preparer.cancel = cancel;
    preparer
}
