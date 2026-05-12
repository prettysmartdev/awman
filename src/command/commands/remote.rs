//! `RemoteCommand` — `remote run | session start | session kill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::remote_client::RemoteClient;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

/// Build a `RemoteClient` for `addr`, pinning the locally-stored self-signed
/// cert when (a) the target is loopback AND (b) the cert PEM is on disk.
/// For non-loopback addresses we never pin — standard webpki verification
/// stays in force, and the caller is expected to use a publicly-trusted cert.
fn build_remote_client(
    engines: &Engines,
    addr: &str,
    api_key: Option<&crate::engine::auth::ApiKey>,
) -> Result<RemoteClient, CommandError> {
    let pinned: Option<String> = if RemoteClient::is_loopback_addr(addr) {
        let cert_path = engines.auth_engine.headless_paths().tls_cert_file();
        std::fs::read_to_string(&cert_path).ok()
    } else {
        None
    };
    RemoteClient::new_with_pinned_cert(addr, api_key, pinned.as_deref())
}

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

/// Frontend hooks for the `remote` command family. Default impls return safe
/// non-interactive choices (first option / declined save) so headless dispatch
/// "just works"; CLI/TUI override to actually prompt.
pub trait RemoteCommandFrontend: UserMessageSink + Send + Sync {
    /// Choose one of multiple sessions reported by the server. Default: first.
    fn ask_session_picker(&mut self, sessions: &[String]) -> Result<String, CommandError> {
        sessions
            .first()
            .cloned()
            .ok_or(CommandError::RemoteSessionMissing)
    }

    /// Choose one of the user's saved working directories. Default: first.
    fn ask_saved_dir_picker(&mut self, dirs: &[String]) -> Result<String, CommandError> {
        dirs.first()
            .cloned()
            .ok_or_else(|| CommandError::MissingRequiredArgument {
                command: vec!["remote".into(), "session".into(), "start".into()],
                argument: "dir".into(),
            })
    }

    /// Choose which session to kill from a list. Default: first.
    fn ask_session_kill_picker(&mut self, sessions: &[String]) -> Result<String, CommandError> {
        sessions
            .first()
            .cloned()
            .ok_or(CommandError::RemoteSessionMissing)
    }

    /// Should the just-used directory be persisted to the user's saved-dirs
    /// list? Default: no — headless mode never persists side-effects without
    /// an explicit signal.
    fn confirm_save_dir(&mut self, _dir: &str) -> Result<bool, CommandError> {
        Ok(false)
    }
}

pub struct RemoteCommand {
    sub: RemoteSubcommand,
    engines: Engines,
    session: crate::data::session::Session,
}

impl RemoteCommand {
    pub fn new(
        sub: RemoteSubcommand,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            sub,
            engines,
            session,
        }
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
        let session = self.session;
        let outcome = match self.sub {
            RemoteSubcommand::Run(f) => {
                run_remote_run(&session, &self.engines, f, &mut *frontend).await?
            }
            RemoteSubcommand::SessionStart(f) => {
                run_session_start(&session, &self.engines, f, &mut *frontend).await?
            }
            RemoteSubcommand::SessionKill(f) => {
                run_session_kill(&session, &self.engines, f, &mut *frontend).await?
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
        .ok_or(CommandError::RemoteSessionMissing)
}

async fn run_remote_run(
    session: &crate::data::session::Session,
    engines: &Engines,
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
    let api_key = RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    let subcommand = &flags.command[0];
    let args: Vec<&str> = flags.command[1..].iter().map(|s| s.as_str()).collect();

    let resp = client
        .send_command_with_headers(
            &["commands"],
            &[
                ("subcommand", serde_json::json!(subcommand)),
                ("args", serde_json::json!(args)),
            ],
            &[("x-amux-session", session_id.as_str())],
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
    engines: &Engines,
    flags: RemoteSessionStartFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    let dir = flags
        .dir
        .ok_or_else(|| CommandError::MissingRequiredArgument {
            command: vec!["remote".into(), "session".into(), "start".into()],
            argument: "dir".into(),
        })?;

    // Detached-HEAD warning: surfaces a UserMessage but does not block.
    if engines.git_engine.is_detached_head(session.git_root()) {
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "detached HEAD — proceeding".into(),
        });
    }

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key = RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    let resp = client
        .send_command(&["sessions"], &[("workdir", serde_json::json!(&dir))])
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
    engines: &Engines,
    flags: RemoteSessionKillFlags,
    frontend: &mut dyn UserMessageSink,
) -> Result<RemoteOutcome, CommandError> {
    let session_id = flags
        .session_id
        .ok_or_else(|| CommandError::MissingRequiredArgument {
            command: vec!["remote".into(), "session".into(), "kill".into()],
            argument: "session_id".into(),
        })?;

    let addr = resolve_addr(session, flags.remote_addr.as_deref())?;
    let api_key = RemoteClient::resolve_api_key(session, &addr, flags.api_key.as_deref())?;
    let client = build_remote_client(engines, &addr, api_key.as_ref())?;

    match client.delete(&["sessions", &session_id]).await {
        // 200 / 204 → success
        Ok(_) => {}
        // 404 → already gone — treat as idempotent success
        Err(CommandError::RemoteHttpStatus { status: 404, .. }) => {}
        Err(CommandError::RemoteHttpStatus { status, body }) => {
            return Err(CommandError::RemoteSessionKillFailed {
                session_id,
                reason: format!("HTTP {status}: {body}"),
            });
        }
        Err(other) => {
            return Err(CommandError::RemoteSessionKillFailed {
                session_id,
                reason: other.to_string(),
            });
        }
    }

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("Session {} killed.", session_id),
    });

    Ok(RemoteOutcome::SessionKill(RemoteSessionKillOutcome {
        session_id,
        remote_addr: addr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::{Session, SessionOpenOptions};

    fn make_session_empty() -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let opts = SessionOpenOptions {
            env: Some(EnvSnapshot::empty()),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    fn make_session_with_remote_addr(addr: &str) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let config_json = format!(r#"{{"remote":{{"defaultAddr":"{addr}"}}}}"#);
        std::fs::write(tmp.path().join("config.json"), &config_json).unwrap();
        let env = EnvSnapshot::with_overrides([("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap())]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    // ─── resolve_addr ─────────────────────────────────────────────────────────

    #[test]
    fn resolve_addr_flag_wins_over_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, Some("http://flag-host:1234")).unwrap();
        assert_eq!(addr, "http://flag-host:1234");
    }

    #[test]
    fn resolve_addr_falls_back_to_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, None).unwrap();
        assert_eq!(addr, "http://config-host:9876");
    }

    #[test]
    fn resolve_addr_empty_flag_falls_back_to_config() {
        let (_tmp, session) = make_session_with_remote_addr("http://config-host:9876");
        let addr = resolve_addr(&session, Some("")).unwrap();
        assert_eq!(addr, "http://config-host:9876");
    }

    #[test]
    fn resolve_addr_errors_when_no_source_available() {
        let (_tmp, session) = make_session_empty();
        let result = resolve_addr(&session, None);
        assert!(
            matches!(result, Err(CommandError::MissingRemoteAddress)),
            "must error when no addr source: {result:?}"
        );
    }
}
