//! `RemoteCommand` — `remote run | session start | session kill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::chat::open_session_for_cwd;
use crate::command::commands::remote_client::RemoteClient;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

#[derive(Debug, Clone)]
pub struct RemoteRunFlags {
    pub command: Vec<String>,
    pub remote_addr: Option<String>,
    pub session: Option<String>,
    pub follow: bool,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionStartFlags {
    pub dir: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSessionKillFlags {
    pub session_id: Option<String>,
    pub remote_addr: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RemoteSubcommand {
    Run(RemoteRunFlags),
    SessionStart(RemoteSessionStartFlags),
    SessionKill(RemoteSessionKillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteRunOutcome {
    pub command_id: String,
    pub command: Vec<String>,
    pub session: String,
    pub remote_addr: String,
    pub status: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionStartOutcome {
    pub session_id: String,
    pub dir: String,
    pub remote_addr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionKillOutcome {
    pub session_id: String,
    pub remote_addr: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum RemoteOutcome {
    Run(RemoteRunOutcome),
    SessionStart(RemoteSessionStartOutcome),
    SessionKill(RemoteSessionKillOutcome),
}

pub trait RemoteCommandFrontend: UserMessageSink + Send + Sync {}

pub struct RemoteCommand {
    sub: RemoteSubcommand,
    engines: Engines,
}

impl RemoteCommand {
    pub fn new(sub: RemoteSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &RemoteSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for RemoteCommand {
    type Frontend = Box<dyn RemoteCommandFrontend>;
    type Outcome = RemoteOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = open_session_for_cwd(&self.engines)?;
        let outcome = match self.sub {
            RemoteSubcommand::Run(f) => run_remote_run(&session, f, &mut *frontend).await?,
            RemoteSubcommand::SessionStart(f) => {
                run_session_start(&session, f, &mut *frontend).await?
            }
            RemoteSubcommand::SessionKill(f) => {
                run_session_kill(&session, f, &mut *frontend).await?
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn resolve_addr(
    session: &crate::data::session::Session,
    flag: Option<&str>,
) -> Result<String, CommandError> {
    if let Some(a) = flag.filter(|s| !s.is_empty()) {
        return Ok(a.to_string());
    }
    session
        .effective_config()
        .remote_default_addr()
        .ok_or(CommandError::MissingRemoteAddress)
}

fn resolve_session_id(
    session: &crate::data::session::Session,
    flag: Option<&str>,
) -> Result<String, CommandError> {
    if let Some(s) = flag.filter(|s| !s.is_empty()) {
        return Ok(s.to_string());
    }
    session
        .effective_config()
        .remote_session()
        .ok_or_else(|| {
            CommandError::Other(
                "No session specified. Pass --session <ID> or set AMUX_REMOTE_SESSION."
                    .to_string(),
            )
        })
}

async fn run_remote_run(
    session: &crate::data::session::Session,
    flags: RemoteRunFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    if flags.command.is_empty() {
        return Err(CommandError::MissingRequiredArgument {
            command: vec!["remote".into(), "run".into()],
            argument: "command".into(),
        });
    }

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let session_id = resolve_session_id(session, flags.session.as_deref())?;
    let api_key =
        RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = RemoteClient::new(&addr, api_key.as_ref())?;

    let subcommand = &flags.command[0];
    let args: Vec<&str> = flags.command[1..].iter().map(|s| s.as_str()).collect();

    let resp = client
        .send_command(
            &["commands"],
            &[
                ("subcommand", serde_json::json!(subcommand)),
                ("args", serde_json::json!(args)),
                ("session_id", serde_json::json!(&session_id)),
            ],
        )
        .await?;

    let command_id = resp.body["command_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Command submitted: {command_id}"),
    });

    if flags.follow {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Streaming logs (waiting for command to complete)...".into(),
        });
        struct FrontendSink<'a>(&'a mut dyn UserMessageSink);
        impl crate::command::commands::remote_client::RemoteEventSink for FrontendSink<'_> {
            fn on_event(&mut self, _event_type: &str, data: &str) {
                self.0.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: data.to_string(),
                });
            }
            fn on_done(&mut self) {}
        }
        let stream_result = client
            .stream_command(
                &["commands", &command_id, "logs", "stream"],
                &[],
                &mut FrontendSink(frontend),
            )
            .await;
        if let Err(CommandError::NotImplemented(_)) = &stream_result {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "SSE streaming not yet implemented; skipping --follow".into(),
            });
        } else {
            stream_result?;
        }
    }

    let status_resp = client.get(&["commands", &command_id]).await;
    let (status, exit_code) = match status_resp {
        Ok(r) => (
            r.body["status"].as_str().map(|s| s.to_string()),
            r.body["exit_code"].as_i64(),
        ),
        Err(_) => (None, None),
    };

    Ok(RemoteOutcome::Run(RemoteRunOutcome {
        command_id,
        command: flags.command,
        session: session_id,
        remote_addr: addr,
        status,
        exit_code,
    }))
}

async fn run_session_start(
    session: &crate::data::session::Session,
    flags: RemoteSessionStartFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    let dir = flags.dir.ok_or_else(|| CommandError::MissingRequiredArgument {
        command: vec!["remote".into(), "session".into(), "start".into()],
        argument: "dir".into(),
    })?;

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key =
        RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = RemoteClient::new(&addr, api_key.as_ref())?;

    let resp = client
        .send_command(
            &["sessions"],
            &[("workdir", serde_json::json!(&dir))],
        )
        .await?;

    let session_id = resp.body["session_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("Session created: {session_id}"),
    });

    Ok(RemoteOutcome::SessionStart(RemoteSessionStartOutcome {
        session_id,
        dir,
        remote_addr: addr,
    }))
}

async fn run_session_kill(
    session: &crate::data::session::Session,
    flags: RemoteSessionKillFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    let session_id = flags.session_id.ok_or_else(|| {
        CommandError::MissingRequiredArgument {
            command: vec!["remote".into(), "session".into(), "kill".into()],
            argument: "session_id".into(),
        }
    })?;

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key =
        RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = RemoteClient::new(&addr, api_key.as_ref())?;

    client.delete(&["sessions", &session_id]).await?;

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("Session {} killed.", session_id),
    });

    Ok(RemoteOutcome::SessionKill(RemoteSessionKillOutcome {
        session_id,
        remote_addr: addr,
    }))
}
