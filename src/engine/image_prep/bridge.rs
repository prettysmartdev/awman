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
    fn import_in(
        &self,
        request: &ImageImportRequest,
        _env: &crate::data::config::env::EnvSnapshot,
        cancel: &CancelToken,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.import(request, cancel, sink)
    }
    fn import(
        &self,
        request: &ImageImportRequest,
        cancel: &CancelToken,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError>;
}

pub struct RuntimeImageImporter(pub Arc<dyn AgentRuntimeEngine>);
impl ImageImporter for RuntimeImageImporter {
    fn import_in(
        &self,
        request: &ImageImportRequest,
        env: &crate::data::config::env::EnvSnapshot,
        cancel: &CancelToken,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.0.import_image_in(request, env, sink, cancel)
    }
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
        let env = self.definition_env.clone();
        let cancel = self.cancel.clone();
        stream(
            cancel.clone(),
            move |tx| importer.import_in(&request, &env, &cancel, &mut Messages(tx)),
            |message| sink.write_message(message),
        )
        .await
    }
}
