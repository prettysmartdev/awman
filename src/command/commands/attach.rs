//! `awman attach <terminal-id | command-id | task-id> [--step NAME]
//! [--read-only] [--annotation k=v]...` (WI 0141).
//!
//! Replaces `agents attach` and folds in `squad attach`'s resolver:
//! `awman squad attach <task>` is a shortcut for `awman attach task_…`. The
//! command resolves the target through Layer 2's `TerminalOperations`, opens
//! the terminal over the daemon's local socket (`engine::remote::
//! terminal_client`) and hands the frame stream to its frontend. The
//! attaching process never owns the agent: detaching ends only the viewer.

use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::DataPaths;
use crate::data::ids::{TaskId, TerminalId};
use crate::data::message::UserMessageSink;
use crate::data::terminal::{
    parse_annotations, Annotations, AttachTarget, DetachReason, HostOutputPolicy,
    TerminalClientFrame, TerminalRecord, TerminalState,
};
use crate::data::wire::List;
use crate::engine::remote::local::{discover, LocalClient};
use crate::engine::remote::terminal_client::{TerminalClientOptions, TerminalStream};

/// Flag and argument names in the catalogue (`catalogue::attach`).
pub const ARG_ID: &str = "id";
pub const FLAG_STEP: &str = "step";
pub const FLAG_READ_ONLY: &str = "read-only";
pub const FLAG_ANNOTATION: &str = "annotation";

#[derive(Debug, Clone, Serialize)]
pub struct AttachOutcome {
    pub terminal_id: TerminalId,
    /// `0` after a normal detach; the agent's exit code when the terminal
    /// exited while attached.
    pub exit_code: i32,
    pub detach_reason: DetachReason,
}

/// What the frontend provides: raw terminal I/O and the hang-up signal.
/// Presentation only — the filter policy and grant rules are the command's.
#[async_trait]
pub trait AttachCommandFrontend: UserMessageSink + Send {
    /// Start the attach session. The returned token ends it on Ctrl-C (a
    /// `user` detach); the TUI cancels it when the tab closes.
    fn begin_attach(&mut self, terminal_id: &TerminalId) -> CancellationToken;

    /// Fires when the attach loses its own terminal (SIGHUP, stdout closed),
    /// so the command can say `detached{reason: hangup}` before exiting.
    fn hangup(&self) -> CancellationToken;

    /// Bind the terminal stream to the frontend's I/O. `read_only` drops
    /// stdin, never requests the grant and never resizes. `policy` is applied
    /// through `HostOutputFilter` whenever stdout is a TTY; there is no flag.
    fn bind_terminal(
        &mut self,
        stream: TerminalStream,
        read_only: bool,
        policy: HostOutputPolicy,
    ) -> Result<tokio::task::JoinHandle<()>, CommandError>;

    /// The attach session ended; restore whatever `bind_terminal` changed.
    fn end_attach(&mut self);

    /// Exit code when the terminal itself closed during this viewer session.
    fn terminal_exit_code(&self) -> i32 {
        0
    }
}

pub struct AttachCommand {
    target: AttachTarget,
    step: Option<String>,
    read_only: bool,
    annotations: Annotations,
    host_output_policy: HostOutputPolicy,
    task_name: Option<String>,
    task_gateway: Option<Arc<dyn TaskGateway>>,
    /// The invoking session's environment, which locates the daemon.
    env: EnvSnapshot,
}

impl AttachCommand {
    pub fn new(
        target: AttachTarget,
        step: Option<String>,
        read_only: bool,
        annotations: Annotations,
        env: EnvSnapshot,
    ) -> Self {
        Self {
            target,
            step,
            read_only,
            annotations,
            host_output_policy: HostOutputPolicy::default(),
            task_name: None,
            task_gateway: None,
            env,
        }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let target = AttachTarget::parse(&ctx.args.require(ARG_ID)?)
            .map_err(|error| CommandError::Other(error.to_string()))?;
        Self::from_parts(ctx, target)
    }

    pub fn from_task_input(ctx: &BuildContext, task: &str) -> Result<Self, CommandError> {
        let (target, task_name) = if task.starts_with("task_") {
            (
                AttachTarget::parse(task)
                    .map_err(|error| CommandError::Other(error.to_string()))?,
                None,
            )
        } else {
            (AttachTarget::Task(TaskId::new()), Some(task.to_owned()))
        };
        let mut command = Self::from_parts(ctx, target)?;
        command.task_name = task_name;
        command.task_gateway = ctx.gateway.clone();
        Ok(command)
    }

    fn from_parts(ctx: &BuildContext, target: AttachTarget) -> Result<Self, CommandError> {
        validate_step_target(&target, ctx.flags.string(FLAG_STEP).as_deref())?;
        let annotations = parse_annotations(ctx.flags.strs(FLAG_ANNOTATION))
            .map_err(|error| CommandError::Other(format!("invalid --annotation: {error}")))?;
        let host_output_policy = crate::command::host_output::resolve_host_output_policy(
            &ctx.session.effective_config(),
        );
        Ok(Self::new(
            target,
            ctx.flags.string(FLAG_STEP),
            ctx.flags.bool(FLAG_READ_ONLY),
            annotations,
            ctx.session.env().clone(),
        )
        .with_policy(host_output_policy))
    }

    fn with_policy(mut self, policy: HostOutputPolicy) -> Self {
        self.host_output_policy = policy;
        self
    }

    pub fn target(&self) -> &AttachTarget {
        &self.target
    }

    pub fn step(&self) -> Option<&str> {
        self.step.as_deref()
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn annotations(&self) -> &Annotations {
        &self.annotations
    }
}

#[async_trait]
impl Command for AttachCommand {
    type Frontend = Box<dyn AttachCommandFrontend>;
    type Outcome = AttachOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        validate_step_target(&self.target, self.step())?;
        let paths = DataPaths::from_env(&self.env)?;
        let endpoint = discover(&paths.daemon())?.ok_or(CommandError::DaemonNotRunning)?;
        let resolved_task = match (&self.task_name, &self.task_gateway) {
            (Some(name), Some(gateway)) => Some(gateway.get(name).await?.id),
            (Some(_), None) => {
                return Err(CommandError::Other(
                    "squad task lookup is unavailable".into(),
                ));
            }
            (None, _) => None,
        };
        let target = resolved_task
            .map(AttachTarget::Task)
            .unwrap_or_else(|| self.target.clone());
        let terminal_id = match &target {
            AttachTarget::Terminal(id) => id.clone(),
            target => {
                let owner_id = match target {
                    AttachTarget::Command(id) => id.as_str().to_owned(),
                    AttachTarget::Task(id) => id.as_str().to_owned(),
                    AttachTarget::Terminal(_) => unreachable!(),
                };
                let client = LocalClient::new(endpoint.address.clone())
                    .with_timeout(Duration::from_secs(10));
                resolve_owner_terminal(&client, &owner_id, self.step()).await?
            }
        };
        let cancel = frontend.begin_attach(&terminal_id);
        let hangup = frontend.hangup();
        let stream = TerminalStream::connect_local(
            &endpoint.address,
            &terminal_id,
            &TerminalClientOptions {
                annotations: self.annotations.clone(),
                actor: None,
                read_only: self.read_only,
            },
        )
        .await?;
        let session = frontend.bind_terminal(stream, self.read_only, self.host_output_policy)?;
        let (detach_reason, _) = wait_attach(cancel, hangup).await;
        // The viewer sends Detached and closes before its frontend or runtime
        // is torn down. Bound a dead peer's close handshake.
        let _ = tokio::time::timeout(Duration::from_secs(2), session).await;
        frontend.end_attach();
        let exit_code = frontend.terminal_exit_code();
        Ok(AttachOutcome {
            terminal_id,
            exit_code,
            detach_reason,
        })
    }
}

async fn resolve_owner_terminal(
    client: &LocalClient,
    owner_id: &str,
    step: Option<&str>,
) -> Result<TerminalId, CommandError> {
    let mut records = Vec::new();
    let mut cursor = None;
    loop {
        let mut path = format!("/v1/terminals?owner_id={owner_id}");
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={cursor}"));
        }
        let response = client.send("GET", &path, None).await?;
        if response.status >= 400 {
            return Err(CommandError::Other(format!(
                "daemon returned HTTP {} resolving terminal",
                response.status
            )));
        }
        let page: List<TerminalRecord> = serde_json::from_value(response.body)
            .map_err(|error| CommandError::Other(format!("invalid terminal list: {error}")))?;
        records.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let record = select_terminal(records, step)
        .ok_or_else(|| CommandError::Other(format!("no terminal is available for {owner_id}")))?;
    Ok(record.id)
}

fn validate_step_target(target: &AttachTarget, step: Option<&str>) -> Result<(), CommandError> {
    if matches!(target, AttachTarget::Terminal(_)) && step.is_some() {
        return Err(CommandError::Other(
            "--step applies to command and task targets".into(),
        ));
    }
    Ok(())
}

fn select_terminal(records: Vec<TerminalRecord>, step: Option<&str>) -> Option<TerminalRecord> {
    let mut candidates: Vec<_> = records
        .into_iter()
        .filter(|record| {
            step.is_none_or(|step| {
                record
                    .source
                    .as_ref()
                    .and_then(|source| source.step_name.as_deref())
                    == Some(step)
            })
        })
        .collect();
    let selected = candidates
        .iter()
        .position(|record| record.state == TerminalState::Open)
        .unwrap_or(0);
    (!candidates.is_empty()).then(|| candidates.remove(selected))
}

/// Frames sent at attach time. A read-only viewer never requests control and
/// never resizes the agent's PTY.
pub fn initial_client_frames(
    read_only: bool,
    size: Option<(u16, u16)>,
) -> Vec<TerminalClientFrame> {
    if read_only {
        return Vec::new();
    }
    let mut frames = vec![TerminalClientFrame::TakeControl];
    if let Some((cols, rows)) = size {
        frames.push(TerminalClientFrame::Resize { cols, rows });
    }
    frames
}

async fn wait_attach(cancel: CancellationToken, hangup: CancellationToken) -> (DetachReason, i32) {
    tokio::select! {
        _ = hangup.cancelled() => (DetachReason::Hangup, 0),
        _ = cancel.cancelled() => (DetachReason::User, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_targets_parse_each_supported_prefix() {
        use crate::data::ids::{CommandId, TaskId, TerminalId};
        assert!(matches!(
            AttachTarget::parse(&TerminalId::new().to_string()),
            Ok(AttachTarget::Terminal(_))
        ));
        assert!(matches!(
            AttachTarget::parse(&CommandId::new().to_string()),
            Ok(AttachTarget::Command(_))
        ));
        assert!(matches!(
            AttachTarget::parse(&TaskId::new().to_string()),
            Ok(AttachTarget::Task(_))
        ));
    }

    #[test]
    fn read_only_attach_has_no_input_grant_or_resize_frames() {
        assert!(initial_client_frames(true, Some((120, 40))).is_empty());
        assert_eq!(initial_client_frames(false, Some((120, 40))).len(), 2);
    }

    #[test]
    fn command_resolution_honours_step_and_prefers_open_terminal() {
        use crate::data::daemon_state::FeatureName;
        use crate::data::execution_event::{EventSource, EventSourcePhase};
        use crate::data::ids::CommandId;
        use crate::data::terminal::TerminalOwner;
        use chrono::Utc;

        let make = |step: &str, state| TerminalRecord {
            id: TerminalId::new(),
            owner_id: TerminalOwner::Command(CommandId::new()),
            feature: FeatureName::Worker,
            source: Some(EventSource {
                phase: EventSourcePhase::Main,
                step_name: Some(step.to_owned()),
                step_index: None,
                container: None,
                step_attempt: Some(1),
            }),
            agent: "agent".into(),
            model: None,
            attempt: 1,
            cols: 120,
            rows: 40,
            state,
            exit_code: None,
            created_at: Utc::now(),
            allow_remote_input: false,
        };
        let old = make("plan", TerminalState::Exited);
        let active = make("implement", TerminalState::Open);
        let active_id = active.id.clone();
        assert_eq!(
            select_terminal(vec![old, active], None).unwrap().id,
            active_id
        );
        assert_eq!(
            select_terminal(
                vec![
                    make("plan", TerminalState::Exited),
                    make("implement", TerminalState::Open)
                ],
                Some("plan")
            )
            .unwrap()
            .source
            .unwrap()
            .step_name
            .as_deref(),
            Some("plan")
        );
    }

    #[tokio::test]
    async fn detach_tokens_distinguish_hangup_from_user() {
        let user = CancellationToken::new();
        user.cancel();
        assert_eq!(
            wait_attach(user, CancellationToken::new()).await.0,
            DetachReason::User
        );
        let hangup = CancellationToken::new();
        hangup.cancel();
        assert_eq!(
            wait_attach(CancellationToken::new(), hangup).await.0,
            DetachReason::Hangup
        );
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    #[test]
    fn a_terminal_id_cannot_silently_ignore_a_step_selector() {
        assert!(validate_step_target(
            &AttachTarget::Terminal(TerminalId::new()),
            Some("implement")
        )
        .is_err());
        assert!(validate_step_target(
            &AttachTarget::Command(crate::data::ids::CommandId::new()),
            Some("implement")
        )
        .is_ok());
    }
}

#[cfg(all(test, unix))]
mod pagination_regression {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn owner_resolution_follows_all_pages_before_selecting_a_step() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let id = TerminalId::new();
        let expected = id.clone();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                }
                assert_eq!(
                    String::from_utf8_lossy(&header).contains("cursor=next"),
                    index == 1
                );
                let items = if index == 0 {
                    Vec::new()
                } else {
                    vec![TerminalRecord {
                        id: id.clone(),
                        owner_id: crate::data::terminal::TerminalOwner::Command(
                            crate::data::ids::CommandId::new(),
                        ),
                        feature: crate::data::daemon_state::FeatureName::Worker,
                        source: Some(crate::data::execution_event::EventSource::step(
                            crate::data::execution_event::EventSourcePhase::Main,
                            "implement",
                            0,
                        )),
                        agent: "test".into(),
                        model: None,
                        attempt: 1,
                        cols: 80,
                        rows: 24,
                        state: TerminalState::Open,
                        exit_code: None,
                        created_at: chrono::Utc::now(),
                        allow_remote_input: false,
                    }]
                };
                let body = serde_json::to_string(&List {
                    items,
                    next_cursor: (index == 0).then(|| "next".to_owned()),
                })
                .unwrap();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let actual = resolve_owner_terminal(
            &LocalClient::new(path),
            "cmd_00000000000000000000000000000001",
            Some("implement"),
        )
        .await
        .unwrap();
        assert_eq!(actual, expected);
        server.await.unwrap();
    }
}
