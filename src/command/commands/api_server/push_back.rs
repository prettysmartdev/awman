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
        self.0.emit(EventPayload::StatusMessage {
            phase: "push".into(),
            message: message.text,
        });
    }

    fn replay_queued(&mut self) {}
}

impl GitFrontend for PushEventSink {}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(session_type: &str, push: bool) -> SessionRecord {
        SessionRecord {
            id: "session".into(),
            workdir: "/missing".into(),
            created_at: "now".into(),
            status: "active".into(),
            closed_at: None,
            setup_status: "ready".into(),
            session_type: session_type.into(),
            cloned_path: Some("/path/that/does/not/exist".into()),
            repo_url: Some("git://example/repo".into()),
            branch: Some("feature".into()),
            base_ref: None,
            merge_refs: Vec::new(),
            push,
        }
    }

    #[test]
    fn push_hook_skips_local_sessions_and_remote_sessions_without_push_enabled() {
        let git = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        for session in [session("remote", false), session("local", true)] {
            assert!(matches!(
                push_after_success(&git, &session, "exec prompt", &[], None, &mut sink).unwrap(),
                PushBackOutcome::NotConfigured
            ));
        }
    }

    #[test]
    fn missing_push_configuration_never_invokes_git() {
        let git = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        for missing in ["branch", "cloned_path", "repo_url"] {
            let mut session = session("remote", true);
            match missing {
                "branch" => session.branch = None,
                "cloned_path" => session.cloned_path = None,
                _ => session.repo_url = None,
            }
            assert!(push_baseline(&git, &session).unwrap().is_none());
            assert!(matches!(
                push_after_success(&git, &session, "exec prompt", &[], None, &mut sink).unwrap(),
                PushBackOutcome::NotConfigured
            ));
        }
    }

    #[test]
    fn leftover_message_uses_the_workflow_work_item_number() {
        for args in [
            vec!["build.toml", "--work-item", "WI-133"],
            vec!["--dynamic", "--work-item=0133-feature"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert_eq!(
                leftover_commit_message("session", "exec workflow", &args),
                "Implement work item 0133"
            );
        }
    }

    #[test]
    fn leftover_message_uses_command_and_session_without_a_work_item() {
        for (command, args) in [
            ("exec workflow", vec!["build.toml"]),
            ("exec workflow", vec!["build.toml", "--work-item=unknown"]),
            ("exec prompt", vec!["fix it"]),
            ("exec prompt", vec!["--", "--work-item", "42"]),
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert_eq!(
                leftover_commit_message("session", command, &args),
                format!("awman: {command} in session session")
            );
        }
    }
}
