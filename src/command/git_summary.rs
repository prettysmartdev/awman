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

#[cfg(test)]
mod tests {
    use super::*;

    fn stale_summary() -> Arc<Mutex<Option<GitDiffSummary>>> {
        Arc::new(Mutex::new(Some(GitDiffSummary {
            branch: Some("stale".to_string()),
            files: Vec::new(),
            added: 0,
            removed: 0,
        })))
    }

    async fn wait_for(
        summary: &Mutex<Option<GitDiffSummary>>,
        done: impl Fn(&Option<GitDiffSummary>) -> bool,
    ) {
        for _ in 0..500 {
            if done(&summary.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the summary never reached the expected state");
    }

    #[tokio::test]
    async fn a_root_that_is_not_a_repo_replaces_the_summary_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let summary = stale_summary();
        let cancel = CancellationToken::new();
        let handle =
            GitSummaryPoller::default().start(dir.path().into(), summary.clone(), cancel.clone());

        wait_for(&summary, Option::is_none).await;
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_repo_root_publishes_its_summary() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();

        let summary = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let handle =
            GitSummaryPoller::default().start(dir.path().into(), summary.clone(), cancel.clone());

        wait_for(&summary, |s| {
            s.as_ref()
                .is_some_and(|s| s.files.iter().any(|f| f.path == "new.txt"))
        })
        .await;
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_wakes_the_task_from_its_sleep_and_stops_refreshing() {
        let dir = tempfile::tempdir().unwrap();
        let summary = stale_summary();
        let cancel = CancellationToken::new();
        let handle =
            GitSummaryPoller::default().start(dir.path().into(), summary.clone(), cancel.clone());

        wait_for(&summary, Option::is_none).await;
        cancel.cancel();
        handle.await.unwrap();

        *summary.lock().unwrap() = Some(GitDiffSummary {
            branch: None,
            files: Vec::new(),
            added: 0,
            removed: 0,
        });
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(summary.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn a_task_cancelled_before_it_runs_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let summary = stale_summary();
        let cancel = CancellationToken::new();
        cancel.cancel();
        GitSummaryPoller::default()
            .start(dir.path().into(), summary.clone(), cancel)
            .await
            .unwrap();
        assert!(summary.lock().unwrap().is_some());
    }
}
