//! Bridge blocking runtime import to the asynchronous ready workflow.
//!
//! Dropping the ready future (session timeout/disconnect) cancels its operation;
//! it never strands an hour-long HTTP request on a Tokio worker. Progress uses a
//! bounded channel and is drained before returning the worker result.
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::{AgentRuntimeEngine, ImageImportRequest, ImportedImage};
use crate::engine::error::EngineError;
use crate::engine::oci::CancelToken;
use std::sync::Arc;

struct CancelOnDrop(Option<CancelToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
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

pub(super) async fn run(
    runtime: Arc<dyn AgentRuntimeEngine>,
    request: ImageImportRequest,
    cancel: CancelToken,
    sink: &mut dyn UserMessageSink,
) -> Result<ImportedImage, EngineError> {
    cancel.check()?;
    let mut guard = CancelOnDrop(Some(cancel.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let worker = tokio::task::spawn_blocking(move || {
        runtime.import_image_cancellable(&request, &mut Messages(tx), &cancel)
    });
    while let Some(message) = rx.recv().await {
        sink.write_message(message);
    }
    let result = worker
        .await
        .map_err(|_| EngineError::Other("image import worker failed".into()))?;
    guard.0 = None;
    result
}
