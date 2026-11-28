//! Move blocking local CLI work off Tokio workers and cancel it if ready is
//! dropped while the builder is running.

use std::sync::Arc;
use std::time::Duration;

use crate::data::oci_identity::OciPlatform;
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

/// A dropped preparation must finish stopping its owned builder before the
/// caller can record the run as canceled. Close the output queue first so a
/// worker blocked on progress delivery can observe cancellation too.
struct StreamCancelOnDrop<E> {
    cancel: Option<CancelToken>,
    events: tokio::sync::mpsc::Receiver<E>,
    finished: std::sync::mpsc::Receiver<()>,
}

impl<E> Drop for StreamCancelOnDrop<E> {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            self.events.close();
            cancel.cancel();
            if matches!(
                self.finished.recv_timeout(Duration::from_secs(5)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                tracing::warn!("image preparation worker did not stop after cancellation");
            }
        }
    }
}

/// Run blocking work (filesystem receipts, lock waits) off the async
/// worker, cancelling `cancel` if the awaiting future is dropped.
impl AgentImagePreparer {
    pub(crate) async fn blocking<T: Send + 'static>(
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
pub(crate) async fn stream<T: Send + 'static, E: Send + 'static>(
    cancel: CancelToken,
    work: impl FnOnce(tokio::sync::mpsc::Sender<E>) -> Result<T, EngineError> + Send + 'static,
    mut report: impl FnMut(E),
) -> Result<T, EngineError> {
    cancel.check()?;
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let result = work(tx);
        let _ = finished_tx.send(());
        result
    });
    let mut guard = StreamCancelOnDrop {
        cancel: Some(cancel),
        events: rx,
        finished: finished_rx,
    };
    while let Some(event) = guard.events.recv().await {
        report(event);
    }
    let result = worker
        .await
        .map_err(|_| EngineError::Other("image preparation worker failed".into()))?;
    guard.cancel = None;
    result
}
impl AgentImagePreparer {
    pub(super) async fn build_source(
        &mut self,
        request: BuildRequest,
        endpoint: Option<&crate::engine::oci::docker_engine::Endpoint>,
        sink: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<BuiltImageIdentity, EngineError> {
        let builder = self.builder();
        let cancel = self.cancel.clone();
        let env = self.definition_env.clone();
        let tag = request.reference.clone();
        sink.build_started(&tag);
        let result = stream(
            cancel.clone(),
            move |tx| {
                let control = crate::engine::container::BuildControl::source_build(cancel);
                let request = super::freshness::label_build(&request, &env, |reference| {
                    match builder.inspect_metadata(&request.target, reference, &control) {
                        Ok(Some(image)) => return Ok(Some(image.id)),
                        Ok(None) | Err(EngineError::NotImplemented(_)) => {}
                        Err(error) => return Err(error),
                    }
                    match builder.inspect(
                        &request.target,
                        reference,
                        &request.platform,
                        &control,
                    )? {
                        SourceImage::Present(id) => Ok(id
                            .config_digest()
                            .or(id.manifest_digest())
                            .map(ToString::to_string)),
                        _ => Ok(None),
                    }
                })?;
                builder.build(&request, &control, &mut |line| {
                    let _ = tx.blocking_send(line.to_string());
                })
            },
            |line| sink.build_output(&line),
        )
        .await;
        sink.build_finished(
            &tag,
            result
                .as_ref()
                .err()
                .map(|error| crate::engine::oci::docker_engine::failure_message_at(error, endpoint))
                .as_deref(),
        );
        result
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
