//! Host-side push-back of a remote session's clone after successful execution.

use std::path::Path;

use super::event_bus::EventBusSender;
use crate::command::commands::exec_workflow::parse_work_item_number;
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::data::execution_event::EventPayload;
use crate::data::fs::api_db::{PushedRef, SessionRecord};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::error::EngineError;
use crate::engine::git::{GitEngine, GitFrontend};

/// Capture publication state before execution; a workflow may push for itself.
pub(crate) struct PushBaseline {
    head: String,
    origin_sha: Option<String>,
    unpublished: bool,
}

pub(crate) fn push_baseline(
    git: &GitEngine,
    session: &SessionRecord,
) -> Result<Option<PushBaseline>, PushBackError> {
    if session.session_type != "remote" || !session.push {
        return Ok(None);
    }
    let (Some(branch), Some(clone), Some(url)) =
        (&session.branch, &session.cloned_path, &session.repo_url)
    else {
        return Ok(None);
    };
    let git = git.isolate_remote_push(Path::new(clone), url)?;
    let mut sink = crate::data::message::RecordingMessageSink::new();
    Ok(Some(PushBaseline {
        head: git.head(&mut sink)?,
        origin_sha: git.origin_sha(branch, &mut sink)?,
        unpublished: git.has_unpublished_commits(&mut sink)?,
    }))
}

/// Why a push did or did not run; projected into the command result by the worker.
#[derive(Debug)]
pub(crate) enum PushBackOutcome {
    NotConfigured,
    NothingToPush,
    Pushed(PushedRef),
}

#[derive(Debug)]
pub(crate) struct PushBackError {
    pub stage: &'static str,
    pub message: String,
}

impl PushBackError {
    pub fn code(&self) -> &'static str {
        crate::data::fs::api_db::failure_codes::PUSH_FAILED
    }
}

impl From<EngineError> for PushBackError {
    fn from(error: EngineError) -> Self {
        Self {
            stage: "push",
            message: error.to_string(),
        }
    }
}

/// Commit leftovers and push from awman's own clone using the host's git
/// credentials. Blocking: the queue worker calls this through `spawn_blocking`.
pub(crate) fn push_after_success(
    git: &GitEngine,
    session: &SessionRecord,
    subcommand: &str,
    args: &[String],
    baseline: Option<PushBaseline>,
    sink: &mut dyn GitFrontend,
) -> Result<PushBackOutcome, PushBackError> {
    if session.session_type != "remote" || !session.push {
        return Ok(PushBackOutcome::NotConfigured);
    }
    let (Some(branch), Some(clone), Some(url)) =
        (&session.branch, &session.cloned_path, &session.repo_url)
    else {
        return Ok(PushBackOutcome::NotConfigured);
    };
    let Some(baseline) = baseline else {
        return Ok(PushBackOutcome::NotConfigured);
    };
    let git = git.isolate_remote_push(Path::new(clone), url)?;
    let message = leftover_commit_message(&session.id, subcommand, args);
    git.commit_leftovers(&message, sink)?;
    let head = git.head(sink)?;
    let origin_sha = git.origin_sha(branch, sink)?;
    if origin_sha.as_ref() == Some(&head) {
        return Ok(if baseline.origin_sha == origin_sha {
            PushBackOutcome::NothingToPush
        } else {
            PushBackOutcome::Pushed(PushedRef {
                remote: "origin".into(),
                branch: branch.clone(),
                sha: head,
            })
        });
    }
    if head == baseline.head && !baseline.unpublished {
        return Ok(PushBackOutcome::NothingToPush);
    }

    // Publish command changes and unpublished setup merges.
    // The engine never force-pushes; rejection leaves the local commit in place.
    let sha = git.push(branch, sink)?;
    Ok(PushBackOutcome::Pushed(PushedRef {
        remote: "origin".into(),
        branch: branch.clone(),
        sha,
    }))
}

pub(crate) fn leftover_commit_message(
    session_id: &str,
    subcommand: &str,
    args: &[String],
) -> String {
    // Use the command parser so positional prompt text and values of other
    // flags cannot accidentally be interpreted as a work-item flag.
    let path: Vec<&str> = subcommand.split_whitespace().collect();
    let work_item = CommandCatalogue::get()
        .parse_raw_args(&path, args)
        .ok()
        .and_then(|parsed| parsed.flag_string("work-item"))
        .and_then(|value| parse_work_item_number(&value));
    match work_item {
        Some(number) => format!("Implement work item {number:04}"),
        None => format!("awman: {subcommand} in session {session_id}"),
    }
}

/// Forward git command echoes and output to the command log and SSE stream.
pub(crate) struct PushEventSink(pub EventBusSender);

impl UserMessageSink for PushEventSink {
    fn write_message(&mut self, message: UserMessage) {
        // Push output is attributed to the push phase (WI 0137 §5).
        self.0
            .with_source(crate::data::execution_event::EventSource::phase(
                crate::data::execution_event::EventSourcePhase::Push,
            ))
            .emit(EventPayload::StatusMessage {
                phase: "push".into(),
                message: message.text,
            });
    }

    fn replay_queued(&mut self) {}
}

impl GitFrontend for PushEventSink {}
