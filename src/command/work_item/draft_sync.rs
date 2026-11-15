//! One-way draft publishing. Timing and retry policy live here, never in providers.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::data::fs::hash::sha256_hex;
use crate::data::fs::work_item_draft::{
    read_work_item_draft, DraftReadError, DraftSnapshot, WorkItemDraft,
};
use crate::data::issue::{Issue, IssueOperation, IssueSourceError};
use crate::engine::issue::IssueSource;

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const QUIET_PERIOD: Duration = Duration::from_secs(5);
pub const CONTINUOUS_EDIT_INTERVAL: Duration = Duration::from_secs(30);
pub const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
/// Ceiling for the exponential retry delay after ordinary upload failures.
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(300);

pub enum DraftSyncMode {
    Create { title: String },
    Amend { issue: Issue },
}

pub struct DraftSyncRequest {
    pub draft: WorkItemDraft,
    pub seeded_body: String,
    pub mode: DraftSyncMode,
    pub source: Arc<dyn IssueSource>,
    pub git_root: PathBuf,
}

#[derive(Debug, Clone)]
pub enum DraftSyncEvent {
    Created {
        source_id: String,
        number: Option<u32>,
        title: String,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPauseReason {
    RemoteConflict,
    Authentication,
    Permission,
    CreateOutcomeUnknown,
}

#[derive(Debug, thiserror::Error)]
pub enum DraftSyncError {
    #[error("draft sync paused ({reason:?}); draft retained at {}", draft_path.display())]
    Paused {
        reason: SyncPauseReason,
        draft_path: PathBuf,
    },
    #[error("{source}; draft retained at {}", draft_path.display())]
    Read {
        source: DraftReadError,
        draft_path: PathBuf,
    },
    #[error("{source}; draft retained at {}", draft_path.display())]
    Upload {
        source: IssueSourceError,
        draft_path: PathBuf,
    },
    #[error("cannot remove draft at {}: {source}", draft_path.display())]
    Cleanup {
        source: crate::data::error::DataError,
        draft_path: PathBuf,
    },
    #[error("draft sync task failed: {message}; draft retained at {}", draft_path.display())]
    Task {
        message: String,
        draft_path: PathBuf,
    },
}

#[derive(Debug)]
pub struct FinalDraftSyncOutcome {
    pub issue: Option<Issue>,
    pub uploaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftVersion {
    pub sha256: String,
    pub empty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadResult {
    Success,
    Retryable,
    RateLimited,
    Paused(SyncPauseReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncInput {
    Observed {
        at: Duration,
        version: DraftVersion,
    },
    Tick {
        at: Duration,
    },
    Completed {
        at: Duration,
        uploaded_hash: String,
        result: UploadResult,
    },
    Finish {
        at: Duration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    None,
    Upload { sha256: String },
    Paused(SyncPauseReason),
    Finished,
}

/// Pure monotonic timing machine. It acknowledges the version sent, even when
/// newer observations arrive during an upload. No content or clock is owned here.
pub struct DraftSyncState {
    seed: String,
    uploaded: Option<String>,
    latest: Option<DraftVersion>,
    changed_at: Duration,
    pending_since: Option<Duration>,
    last_attempt: Option<Duration>,
    retry_after: Duration,
    retrying: bool,
    /// Consecutive failed uploads; each doubles the next retry delay.
    failures: u32,
    in_flight: Option<String>,
    pause: Option<SyncPauseReason>,
}

impl DraftSyncState {
    pub fn new(seed_hash: String, last_uploaded_hash: Option<String>) -> Self {
        Self {
            seed: seed_hash,
            uploaded: last_uploaded_hash,
            latest: None,
            changed_at: Duration::ZERO,
            pending_since: None,
            last_attempt: None,
            retry_after: Duration::ZERO,
            retrying: false,
            failures: 0,
            in_flight: None,
            pause: None,
        }
    }

    fn pending(&self) -> bool {
        self.latest.as_ref().is_some_and(|v| {
            !v.empty && v.sha256 != self.seed && self.uploaded.as_ref() != Some(&v.sha256)
        })
    }

    pub fn advance(&mut self, input: SyncInput) -> SyncAction {
        let (at, finish) = match input {
            SyncInput::Observed { at, version } => {
                if self.latest.as_ref() != Some(&version) {
                    self.changed_at = at;
                    self.latest = Some(version);
                }
                if self.pending() {
                    self.pending_since.get_or_insert(at);
                } else {
                    self.pending_since = None;
                    self.last_attempt = None;
                }
                return SyncAction::None;
            }
            SyncInput::Completed {
                at,
                uploaded_hash,
                result,
            } => {
                // Ignore stale/duplicate completions; they cannot acknowledge a
                // later request or release its in-flight guard.
                if self.in_flight.as_ref() != Some(&uploaded_hash) {
                    return SyncAction::None;
                }
                self.in_flight = None;
                match result {
                    UploadResult::Success => {
                        self.uploaded = Some(uploaded_hash);
                        self.retry_after = Duration::ZERO;
                        self.retrying = false;
                        self.failures = 0;
                        if !self.pending() {
                            self.pending_since = None;
                        }
                    }
                    UploadResult::Retryable => {
                        self.failures = self.failures.saturating_add(1);
                        self.retry_after = at + retry_delay(self.failures);
                        self.retrying = true;
                    }
                    UploadResult::RateLimited => {
                        self.retry_after = at + RATE_LIMIT_BACKOFF;
                        self.retrying = true;
                    }
                    UploadResult::Paused(reason) => self.pause = Some(reason),
                }
                return self
                    .pause
                    .clone()
                    .map_or(SyncAction::None, SyncAction::Paused);
            }
            SyncInput::Tick { at } => (at, false),
            SyncInput::Finish { at } => (at, true),
        };
        if let Some(reason) = &self.pause {
            return SyncAction::Paused(reason.clone());
        }
        if self.in_flight.is_some() {
            return SyncAction::None;
        }
        if !self.pending() {
            return if finish {
                SyncAction::Finished
            } else {
                SyncAction::None
            };
        }
        let continuous_start = self.last_attempt.or(self.pending_since).unwrap_or(at);
        let quiet = at.saturating_sub(self.changed_at) >= QUIET_PERIOD;
        let continuous = at.saturating_sub(continuous_start) >= CONTINUOUS_EDIT_INTERVAL;
        if !finish && (at < self.retry_after || !(self.retrying || quiet || continuous)) {
            return SyncAction::None;
        }
        let sha256 = self
            .latest
            .as_ref()
            .expect("pending has a latest version")
            .sha256
            .clone();
        self.in_flight = Some(sha256.clone());
        self.last_attempt = Some(at);
        SyncAction::Upload { sha256 }
    }
}

/// Delay before retrying after `failures` consecutive failed uploads: one
/// poll interval, doubling each time, never more than [`MAX_RETRY_BACKOFF`],
/// so a deleted issue or a failing provider is not hit every second.
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (POLL_INTERVAL * 2u32.pow(doublings)).min(MAX_RETRY_BACKOFF)
}

pub struct DraftSync;

/// Drop requests a graceful stop. Only stop + final_upload guarantees publishing.
pub struct RunningDraftSync {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<StoppedDraftSync, DraftSyncError>>>,
    path: PathBuf,
}

pub struct StoppedDraftSync {
    draft: WorkItemDraft,
    source: Arc<dyn IssueSource>,
    git_root: PathBuf,
    title: String,
    issue: Option<Issue>,
    state: DraftSyncState,
    latest: Option<DraftSnapshot>,
    events: mpsc::UnboundedSender<DraftSyncEvent>,
    read_warning: Option<String>,
    upload_failure: bool,
    started: tokio::time::Instant,
}

impl DraftSync {
    pub fn start(
        request: DraftSyncRequest,
    ) -> (RunningDraftSync, mpsc::UnboundedReceiver<DraftSyncEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let (stop, mut stopped) = oneshot::channel();
        let path = request.draft.paths().host_path.clone();
        let task = tokio::spawn(async move {
            let seed_hash = sha256_hex(request.seeded_body.as_bytes());
            let (title, issue) = match request.mode {
                DraftSyncMode::Create { title } => (title, None),
                DraftSyncMode::Amend { issue } => (issue.title.clone(), Some(issue)),
            };
            let uploaded = issue.as_ref().map(|i| sha256_hex(i.body.as_bytes()));
            let mut sync = StoppedDraftSync {
                draft: request.draft,
                source: request.source,
                git_root: request.git_root,
                title,
                issue,
                state: DraftSyncState::new(seed_hash, uploaded),
                latest: None,
                events,
                read_warning: None,
                upload_failure: false,
                started: tokio::time::Instant::now(),
            };
            let mut ticks = tokio::time::interval(POLL_INTERVAL);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    _ = ticks.tick() => {
                        // A rejected read must suppress any cached candidate,
                        // including when that older candidate is now due.
                        if sync.observe().is_ok() {
                            let action = sync.state.advance(SyncInput::Tick { at: sync.started.elapsed() });
                            if let SyncAction::Upload { sha256 } = action {
                                // Await the whole fetch/check/update sequence.
                                // Stop cannot cancel or overtake this mutation.
                                let _ = sync.upload(sha256).await?;
                            }
                        }
                    }
                }
            }
            Ok(sync)
        });
        (
            RunningDraftSync {
                stop: Some(stop),
                task: Some(task),
                path,
            },
            receiver,
        )
    }
}

impl Drop for RunningDraftSync {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl RunningDraftSync {
    pub async fn stop(mut self) -> Result<StoppedDraftSync, DraftSyncError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task
            .take()
            .expect("running task")
            .await
            .map_err(|_| DraftSyncError::Task {
                message: "polling task panicked or was cancelled".into(),
                draft_path: self.path.clone(),
            })?
    }
}

enum PublishResult {
    Published(Issue),
    Conflict,
    Failed(IssueSourceError),
}

impl StoppedDraftSync {
    /// Always reads safely, even when metadata is unchanged: timestamp
    /// granularity cannot prove that a same-size rename save is identical.
    fn observe(&mut self) -> Result<(), DraftReadError> {
        let snapshot = self
            .draft
            .stamp()
            .and_then(|_stamp| read_work_item_draft(&self.draft));
        match snapshot {
            Ok(snapshot) => {
                self.read_warning = None;
                self.state.advance(SyncInput::Observed {
                    at: self.started.elapsed(),
                    version: DraftVersion {
                        sha256: snapshot.sha256.clone(),
                        empty: snapshot.content.trim().is_empty(),
                    },
                });
                self.latest = Some(snapshot);
                Ok(())
            }
            Err(error) => {
                let message = format!("Skipping draft read: {error}");
                if self.read_warning.as_ref() != Some(&message) {
                    let _ = self.events.send(DraftSyncEvent::Warning {
                        message: message.clone(),
                    });
                    self.read_warning = Some(message);
                }
                Err(error)
            }
        }
    }

    async fn upload(
        &mut self,
        hash: String,
    ) -> Result<Result<(), IssueSourceError>, DraftSyncError> {
        let body = self
            .latest
            .as_ref()
            .expect("observed before upload")
            .content
            .clone();
        let source = self.source.clone();
        let issue = self.issue.clone();
        let creating = issue.is_none();
        let root = self.git_root.clone();
        let title = self.title.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = if let Some(issue) = issue {
                match source.fetch_issue(&issue.source_id, &root) {
                    Ok(current)
                        if normalize_lines(&current.body) != normalize_lines(&issue.body) =>
                    {
                        return PublishResult::Conflict;
                    }
                    Ok(current) => source.update_issue(&current, &body),
                    Err(error) => return PublishResult::Failed(error),
                }
            } else {
                source.create_issue(&title, &body, &root)
            };
            match result {
                Ok(mut issue) => {
                    // Acknowledge exactly what was sent, never a newer draft.
                    issue.body = body;
                    PublishResult::Published(issue)
                }
                Err(error) => PublishResult::Failed(error),
            }
        })
        .await
        .map_err(|_| {
            let error = DraftSyncError::Task {
                message: "provider task panicked or was cancelled; check tracker before retrying"
                    .into(),
                draft_path: self.draft.paths().host_path.clone(),
            };
            let _ = self.events.send(DraftSyncEvent::Error {
                message: error.to_string(),
            });
            error
        })?;
        let (completion, failure) = match result {
            PublishResult::Published(issue) => {
                if creating {
                    let _ = self.events.send(DraftSyncEvent::Created {
                        source_id: issue.source_id.clone(),
                        number: issue.number,
                        title: issue.title.clone(),
                    });
                }
                self.issue = Some(issue);
                self.upload_failure = false;
                (UploadResult::Success, None)
            }
            PublishResult::Conflict => {
                let _ = self.events.send(DraftSyncEvent::Warning {
                    message: format!(
                        "Work item changed on the tracker; sync paused. Draft retained at {}",
                        self.draft.paths().host_path.display()
                    ),
                });
                (UploadResult::Paused(SyncPauseReason::RemoteConflict), None)
            }
            PublishResult::Failed(error) => {
                let completion = match &error {
                    IssueSourceError::Unauthorized { .. } => {
                        UploadResult::Paused(SyncPauseReason::Authentication)
                    }
                    IssueSourceError::Forbidden { .. } => {
                        UploadResult::Paused(SyncPauseReason::Permission)
                    }
                    IssueSourceError::CreateOutcomeUnknown { .. }
                    | IssueSourceError::Timeout {
                        operation: IssueOperation::Create,
                        ..
                    } => UploadResult::Paused(SyncPauseReason::CreateOutcomeUnknown),
                    IssueSourceError::RateLimited { .. } => UploadResult::RateLimited,
                    _ => UploadResult::Retryable,
                };
                if !self.upload_failure || matches!(completion, UploadResult::Paused(_)) {
                    let recovery = if completion
                        == UploadResult::Paused(SyncPauseReason::CreateOutcomeUnknown)
                    {
                        "; sync paused; check tracker before retrying"
                    } else {
                        ""
                    };
                    let _ = self.events.send(DraftSyncEvent::Warning {
                        message: format!(
                            "Draft upload failed: {error}{recovery}. Draft retained at {}",
                            self.draft.paths().host_path.display()
                        ),
                    });
                }
                self.upload_failure = true;
                (completion, Some(error))
            }
        };
        self.state.advance(SyncInput::Completed {
            at: self.started.elapsed(),
            uploaded_hash: hash,
            result: completion,
        });
        Ok(failure.map_or(Ok(()), Err))
    }

    /// Sole final publisher. Success removes the draft; every error retains it.
    /// Final events use the same receiver returned by start, which callers drain.
    pub async fn final_upload(mut self) -> Result<FinalDraftSyncOutcome, DraftSyncError> {
        let path = self.draft.paths().host_path.clone();
        self.observe().map_err(|source| DraftSyncError::Read {
            source,
            draft_path: path.clone(),
        })?;
        let mut uploaded = false;
        match self.state.advance(SyncInput::Finish {
            at: self.started.elapsed(),
        }) {
            SyncAction::Paused(reason) => {
                return Err(DraftSyncError::Paused {
                    reason,
                    draft_path: path,
                })
            }
            SyncAction::Upload { sha256 } => {
                let result = self.upload(sha256).await?;
                if let Some(reason) = self.state.pause.clone() {
                    return Err(DraftSyncError::Paused {
                        reason,
                        draft_path: path,
                    });
                }
                result.map_err(|source| DraftSyncError::Upload {
                    source,
                    draft_path: path.clone(),
                })?;
                uploaded = true;
            }
            SyncAction::Finished => {}
            SyncAction::None => {
                return Err(DraftSyncError::Task {
                    message: "final upload still in flight".into(),
                    draft_path: path,
                })
            }
        }
        self.draft
            .remove()
            .map_err(|source| DraftSyncError::Cleanup {
                source,
                draft_path: path,
            })?;
        Ok(FinalDraftSyncOutcome {
            issue: self.issue,
            uploaded,
        })
    }
}

fn normalize_lines(body: &str) -> String {
    body.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    fn at(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }
    fn observe(state: &mut DraftSyncState, seconds: u64, hash: &str) {
        assert_eq!(
            state.advance(SyncInput::Observed {
                at: at(seconds),
                version: DraftVersion {
                    sha256: hash.into(),
                    empty: hash == "empty",
                }
            }),
            SyncAction::None
        );
    }
    fn tick(state: &mut DraftSyncState, seconds: u64) -> SyncAction {
        state.advance(SyncInput::Tick { at: at(seconds) })
    }
    fn complete(state: &mut DraftSyncState, seconds: u64, hash: &str, result: UploadResult) {
        state.advance(SyncInput::Completed {
            at: at(seconds),
            uploaded_hash: hash.into(),
            result,
        });
    }
    fn upload(hash: &str) -> SyncAction {
        SyncAction::Upload {
            sha256: hash.into(),
        }
    }
    fn fresh() -> DraftSyncState {
        DraftSyncState::new("seed".into(), None)
    }

    #[test]
    fn quiet_period_and_identical_observations_table() {
        let mut state = fresh();
        observe(&mut state, 1, "edit");
        for seconds in 1..6 {
            observe(&mut state, seconds, "edit");
            assert_eq!(tick(&mut state, seconds), SyncAction::None);
        }
        assert_eq!(tick(&mut state, 6), upload("edit"));
        complete(&mut state, 6, "edit", UploadResult::Success);
        assert_eq!(tick(&mut state, 60), SyncAction::None);
        assert_eq!(
            state.advance(SyncInput::Finish { at: at(60) }),
            SyncAction::Finished
        );
    }

    #[test]
    fn a_burst_publishes_only_the_latest_version_once() {
        let mut state = fresh();
        for (seconds, hash) in [(1, "a"), (2, "b"), (3, "c")] {
            observe(&mut state, seconds, hash);
            assert_eq!(tick(&mut state, seconds), SyncAction::None);
        }
        assert_eq!(tick(&mut state, 7), SyncAction::None);
        assert_eq!(tick(&mut state, 8), upload("c"));
        assert_eq!(tick(&mut state, 40), SyncAction::None);
        complete(&mut state, 40, "c", UploadResult::Success);
        assert_eq!(tick(&mut state, 41), SyncAction::None);
    }

    #[test]
    fn continuous_edits_publish_at_thirty_second_intervals_with_latest_content() {
        let mut state = fresh();
        for seconds in 1..=61 {
            let hash = format!("edit-{seconds}");
            observe(&mut state, seconds, &hash);
            let expected = if seconds == 31 || seconds == 61 {
                upload(&hash)
            } else {
                SyncAction::None
            };
            assert_eq!(tick(&mut state, seconds), expected, "at {seconds}");
            if seconds == 31 || seconds == 61 {
                complete(&mut state, seconds, &hash, UploadResult::Success);
            }
        }
    }

    #[test]
    fn quiet_periods_can_publish_more_frequently_than_continuous_edit_interval() {
        let mut state = fresh();
        for (start, hash) in [(1, "a"), (7, "b")] {
            observe(&mut state, start, hash);
            assert_eq!(tick(&mut state, start + 5), upload(hash));
            complete(&mut state, start + 5, hash, UploadResult::Success);
        }
    }

    #[test]
    fn seed_empty_and_last_uploaded_versions_are_skipped_including_at_finish() {
        for hash in ["seed", "empty", "published"] {
            let mut state = DraftSyncState::new("seed".into(), Some("published".into()));
            observe(&mut state, 1, hash);
            assert_eq!(tick(&mut state, 90), SyncAction::None);
            assert_eq!(
                state.advance(SyncInput::Finish { at: at(90) }),
                SyncAction::Finished
            );
        }
    }

    #[test]
    fn in_flight_completions_acknowledge_only_the_sent_version() {
        let mut state = fresh();
        observe(&mut state, 1, "a");
        assert_eq!(tick(&mut state, 6), upload("a"));
        observe(&mut state, 7, "b");
        assert_eq!(tick(&mut state, 50), SyncAction::None);
        complete(&mut state, 50, "wrong", UploadResult::Success);
        assert_eq!(tick(&mut state, 50), SyncAction::None);
        complete(&mut state, 50, "a", UploadResult::Success);
        assert_eq!(tick(&mut state, 51), upload("b"));
    }

    #[test]
    fn retry_is_no_earlier_than_next_poll_and_backoff_retains_the_latest_version() {
        for (result, due) in [
            (UploadResult::Retryable, 7),
            (UploadResult::RateLimited, 66),
        ] {
            let mut state = fresh();
            observe(&mut state, 1, "a");
            assert_eq!(tick(&mut state, 6), upload("a"));
            complete(&mut state, 6, "a", result);
            assert_eq!(tick(&mut state, 6), SyncAction::None);
            if due == 66 {
                observe(&mut state, 10, "latest");
                assert_eq!(tick(&mut state, 65), SyncAction::None);
            } else {
                // Continued typing must not re-debounce a failed request.
                observe(&mut state, 7, "latest");
            }
            assert_eq!(tick(&mut state, due), upload("latest"));
        }
    }

    #[test]
    fn repeated_failures_back_off_exponentially_up_to_a_ceiling() {
        let mut state = fresh();
        observe(&mut state, 1, "a");
        let mut now = 6;
        assert_eq!(tick(&mut state, now), upload("a"));
        let mut delays = Vec::new();
        for _ in 0..12 {
            complete(&mut state, now, "a", UploadResult::Retryable);
            let start = now;
            while tick(&mut state, now) == SyncAction::None {
                now += 1;
            }
            delays.push(now - start);
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]);
        // A success resets the delay.
        complete(&mut state, now, "a", UploadResult::Success);
        observe(&mut state, now, "b");
        now += 6;
        assert_eq!(tick(&mut state, now), upload("b"));
        complete(&mut state, now, "b", UploadResult::Retryable);
        assert_eq!(tick(&mut state, now + 1), upload("b"));
    }

    #[test]
    fn finish_bypasses_debounce_and_backoff_but_never_an_in_flight_request() {
        let mut state = fresh();
        observe(&mut state, 1, "a");
        assert_eq!(state.advance(SyncInput::Finish { at: at(1) }), upload("a"));
        observe(&mut state, 2, "b");
        assert_eq!(
            state.advance(SyncInput::Finish { at: at(2) }),
            SyncAction::None
        );
        complete(&mut state, 2, "a", UploadResult::RateLimited);
        assert_eq!(state.advance(SyncInput::Finish { at: at(2) }), upload("b"));
    }

    #[test]
    fn every_terminal_pause_survives_finalization_and_new_edits() {
        for reason in [
            SyncPauseReason::Authentication,
            SyncPauseReason::Permission,
            SyncPauseReason::RemoteConflict,
            SyncPauseReason::CreateOutcomeUnknown,
        ] {
            let mut state = fresh();
            observe(&mut state, 1, "a");
            assert_eq!(tick(&mut state, 6), upload("a"));
            complete(&mut state, 6, "a", UploadResult::Paused(reason.clone()));
            observe(&mut state, 10, "b");
            assert_eq!(tick(&mut state, 1000), SyncAction::Paused(reason.clone()));
            assert_eq!(
                state.advance(SyncInput::Finish { at: at(1000) }),
                SyncAction::Paused(reason)
            );
        }
    }

    #[test]
    fn conflict_comparison_only_normalizes_line_endings() {
        assert_eq!(normalize_lines("a\rb\r\nc\n"), "a\nb\nc\n");
        assert_ne!(normalize_lines("a\n"), normalize_lines("a"));
        assert_ne!(normalize_lines("a \n"), normalize_lines("a\n"));
    }
}
