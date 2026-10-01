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

/// Fakeable import seam for ready. Production delegates to the selected
/// importing runtime; tests can record ordered requests and return scripted
/// identities or failures without starting a daemon.
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
    importer: Arc<dyn ImageImporter>,
    request: ImageImportRequest,
    cancel: CancelToken,
    sink: &mut dyn UserMessageSink,
) -> Result<ImportedImage, EngineError> {
    cancel.check()?;
    let mut guard = CancelOnDrop(Some(cancel.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let worker =
        tokio::task::spawn_blocking(move || importer.import(&request, &cancel, &mut Messages(tx)));
    while let Some(message) = rx.recv().await {
        sink.write_message(message);
    }
    let result = worker
        .await
        .map_err(|_| EngineError::Other("image import worker failed".into()))?;
    guard.0 = None;
    result
}

#[cfg(test)]
mod tests {
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
