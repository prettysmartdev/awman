//! Bounded, read-only workflow artifact collection for pulled leases.
use crate::{
    data::{context::ContextLimits, error::DataError, fs::context_dirs::ContextDirResolver},
    engine::workflow::context_files::WorkflowContextReader,
};
use awman_wsp::{WorkflowContextItem, WorkflowContextResult};
use base64::Engine as _;
use std::sync::atomic::AtomicBool;

pub struct WorkflowArtifactCollector {
    reader: WorkflowContextReader,
    max_bytes: u64,
}

impl WorkflowArtifactCollector {
    pub fn for_invocation(
        resolver: &ContextDirResolver,
        invocation: uuid::Uuid,
        max_bytes: u64,
    ) -> Result<Self, DataError> {
        Ok(Self {
            reader: WorkflowContextReader::for_invocation(resolver, invocation)?,
            max_bytes: max_bytes.min(ContextLimits::REPO.max_total_bytes),
        })
    }

    /// Return metadata for every safe file. Content is read through the
    /// existing no-follow, bounded Layer 1 reader and only inlined while the
    /// request's decoded-byte budget remains. A failed/oversized read only
    /// omits that file's content; it never changes the lease outcome.
    pub fn collect(&self, cancelled: &AtomicBool) -> WorkflowContextResult {
        let metadata = match self.reader.list_for_collection(cancelled) {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(%error, "could not list workflow artifacts for lease result");
                return WorkflowContextResult {
                    collection_error: Some(awman_wsp::WorkflowContextCollectionError::Unavailable),
                    items: vec![],
                    next_cursor: None,
                    truncated: true,
                };
            }
        };

        let mut remaining = self.max_bytes;
        let mut truncated = metadata.skipped;
        let mut items = Vec::with_capacity(metadata.files.len());
        for meta in metadata.files {
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                truncated = true;
                items.push(WorkflowContextItem {
                    path: meta.path,
                    size_bytes: meta.size_bytes,
                    sha256: meta.sha256,
                    modified_at: meta.modified_at,
                    content_base64: None,
                });
                continue;
            }
            let mut item = WorkflowContextItem {
                path: meta.path.clone(),
                size_bytes: meta.size_bytes,
                sha256: meta.sha256,
                modified_at: meta.modified_at,
                content_base64: None,
            };
            if meta.size_bytes <= remaining {
                match self.reader.read(&meta.path) {
                    Ok((bytes, read_meta)) if bytes.len() as u64 <= remaining => {
                        remaining -= bytes.len() as u64;
                        item.size_bytes = read_meta.size_bytes;
                        item.sha256 = read_meta.sha256;
                        item.modified_at = read_meta.modified_at;
                        item.content_base64 =
                            Some(base64::engine::general_purpose::STANDARD.encode(bytes));
                    }
                    Ok(_) => truncated = true,
                    Err(error) => {
                        tracing::debug!(path = %meta.path, %error, "omitting workflow artifact content");
                        truncated = true;
                    }
                }
            } else {
                truncated = true;
            }
            items.push(item);
        }
        WorkflowContextResult {
            collection_error: None,
            items,
            next_cursor: None,
            truncated,
        }
    }
}
