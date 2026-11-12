//! [`GitSummaryPoller`] — the Layer 2 source of a frontend's live git summary.
//!
//! A frontend schedules nothing against `GitEngine` itself: it hands over the
//! root to watch and the slot to publish into, and renders whatever appears.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::git::{GitDiffSummary, GitEngine};

/// Refreshes a shared [`GitDiffSummary`] in the background.
#[derive(Clone)]
pub struct GitSummaryPoller {
    git: Arc<GitEngine>,
}

impl Default for GitSummaryPoller {
    fn default() -> Self {
        Self::new(Arc::new(GitEngine::new()))
    }
}

impl GitSummaryPoller {
    pub fn new(git: Arc<GitEngine>) -> Self {
        Self { git }
    }

    /// Spawn the background diff-polling task.
    ///
    /// Every ~2 seconds the task asks [`GitEngine::diff_summary`] for a snapshot
    /// and replaces the shared value. On failure (not a repo, git missing) it
    /// stores `None`. The engine owns all git process and file I/O; this method
    /// only schedules the refresh and publishes the result.
    pub fn start(
        &self,
        root: PathBuf,
        summary: Arc<Mutex<Option<GitDiffSummary>>>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let git = self.git.clone();
        tokio::spawn(async move {
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let next = git.diff_summary(&root).ok();
                if let Ok(mut guard) = summary.lock() {
                    *guard = next;
                }

                // Sleep ~2s, but wake immediately on cancellation.
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        })
    }
}
