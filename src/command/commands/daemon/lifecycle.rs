//! `awman daemon start | stop | status | logs`: the one daemon's lifecycle.
//!
//! The daemon is `awman daemon start` in the foreground; `--background` spawns
//! that same command under the OS service manager (systemd user unit
//! `awman-daemon`, launchd label `io.awman.daemon`). Everything else talks to
//! the running daemon over its owner-only local listener.
use super::actions::Request;
use super::client::DaemonClient;
use crate::command::commands::api_server::{
    resolve_workdir_roots, resolve_workdirs, validate_root_mount_overlap,
};
use crate::command::commands::Command;
use crate::command::daemon_api::core::StatusResponse;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::daemon_state::FeatureName;
use crate::data::fs::DataPaths;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::daemon::{DaemonSupervisor, Termination};
use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;

/// How long `start --background` waits for the daemon's local listener.
const BACKGROUND_READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `stop` waits for the daemon to drain and exit before reporting.
const STOP_WAIT: Duration = Duration::from_secs(40);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonVerb {
    Start { background: bool },
    Stop { now: bool },
    Status { json: bool },
    Logs { follow: bool },
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStartOutcome {
    pub background: bool,
    /// The daemon's PID (background: the spawned process; foreground: this one).
    pub pid: u32,
    /// Background only: whether the local listener answered before the wait
    /// ran out.
    pub ready: bool,
    pub log_file: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStopOutcome {
    /// The PID that was signalled, `None` when no daemon was running.
    pub stopped_pid: Option<u32>,
    /// `--now`: the job IDs cancelled before stopping.
    pub canceled: Vec<String>,
    /// Whether the process had exited by the time `stop` returned.
    pub exited: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatusOutcome {
    pub running: bool,
    /// `GET /v1/status`, when a daemon answered.
    #[serde(flatten)]
    pub status: Option<StatusResponse>,
    #[serde(skip)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonLogsOutcome {
    pub log_file: String,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum DaemonOutcome {
    Start(DaemonStartOutcome),
    Stop(DaemonStopOutcome),
    Status(Box<DaemonStatusOutcome>),
    Logs(DaemonLogsOutcome),
}

/// What `awman daemon` needs from a frontend.
#[async_trait]
pub trait DaemonCommandFrontend: UserMessageSink + Send + Sync {
    /// Become the daemon: bootstrap the runtime with this frontend's
    /// presentation adapters and serve its listeners until shutdown. Only the
    /// CLI hosts the daemon process; other frontends refuse.
    async fn host_daemon(
        &mut self,
        env: EnvSnapshot,
        set: crate::data::config::overrides::ConfigOverrides,
    ) -> Result<(), CommandError>;

    /// One line of the daemon log, for `logs` (stdout in the CLI).
    fn log_line(&mut self, line: &str) {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: line.to_string(),
        });
    }
}

pub struct DaemonCommand {
    verb: DaemonVerb,
    session: Session,
}

impl DaemonCommand {
    pub fn new(verb: DaemonVerb, session: Session) -> Self {
        Self { verb, session }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let verb = match ctx.caller.leaf() {
            "start" => DaemonVerb::Start {
                background: ctx.flags.bool("background"),
            },
            "stop" => DaemonVerb::Stop {
                now: ctx.flags.bool("now"),
            },
            "status" => DaemonVerb::Status {
                json: ctx.flags.bool("json"),
            },
            "logs" => DaemonVerb::Logs {
                follow: ctx.flags.bool("follow"),
            },
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(verb, ctx.session.clone()))
    }

    pub fn verb(&self) -> &DaemonVerb {
        &self.verb
    }
}

#[async_trait]
impl Command for DaemonCommand {
    type Frontend = Box<dyn DaemonCommandFrontend>;
    type Outcome = DaemonOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let outcome = match self.verb {
            DaemonVerb::Start { background } => {
                DaemonOutcome::Start(start(&self.session, background, &mut *frontend).await?)
            }
            DaemonVerb::Stop { now } => {
                DaemonOutcome::Stop(stop(self.session.env(), now, &mut *frontend).await?)
            }
            DaemonVerb::Status { json } => {
                DaemonOutcome::Status(Box::new(status(self.session.env(), json).await?))
            }
            DaemonVerb::Logs { follow } => {
                DaemonOutcome::Logs(logs(self.session.env(), follow, &mut *frontend).await?)
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn supervisor(env: &EnvSnapshot) -> Result<DaemonSupervisor, CommandError> {
    Ok(DaemonSupervisor::for_paths(
        DataPaths::from_env(env)?.daemon(),
    ))
}

/// The features `daemon.features.*.enabled` turns on.
pub fn enabled_features(session: &Session) -> Vec<FeatureName> {
    let daemon = session.effective_config().daemon();
    [
        (FeatureName::Squad, daemon.squad.enabled),
        (FeatureName::Worker, daemon.worker.enabled),
        (FeatureName::Engine, daemon.engine.enabled),
    ]
    .into_iter()
    .filter_map(|(feature, on)| on.then_some(feature))
    .collect()
}

/// Checks shared by both start modes, so `--background` reports a bad start
/// here rather than only in the log.
fn preflight(
    session: &Session,
    supervisor: &DaemonSupervisor,
    frontend: &mut dyn DaemonCommandFrontend,
) -> Result<(), CommandError> {
    if let Some(pid) = supervisor.running_pid()? {
        return Err(CommandError::DaemonAlreadyRunning { pid });
    }
    let features = enabled_features(session);
    if features.is_empty() {
        return Err(CommandError::DaemonNoFeatures);
    }
    if features.contains(&FeatureName::Worker) {
        let config = session.effective_config();
        let workdirs = resolve_workdirs(&[], &config.worker_push_work_dirs())?;
        let roots = resolve_workdir_roots(
            &[],
            &config.worker_push_work_dir_roots(),
            &crate::engine::git::GitEngine::new(),
            frontend,
        )?;
        validate_root_mount_overlap(&workdirs, &roots)?;
    }
    Ok(())
}

async fn start(
    session: &Session,
    background: bool,
    frontend: &mut dyn DaemonCommandFrontend,
) -> Result<DaemonStartOutcome, CommandError> {
    let env = session.env().clone();
    let supervisor = supervisor(&env)?;
    supervisor
        .process()
        .paths()
        .ensure_root()
        .map_err(CommandError::Data)?;
    preflight(session, &supervisor, frontend)?;
    let log_file = supervisor
        .process()
        .paths()
        .log_file()
        .display()
        .to_string();
    if !background {
        frontend
            .host_daemon(env, session.effective_config().layers().set.clone())
            .await?;
        return Ok(DaemonStartOutcome {
            background: false,
            pid: std::process::id(),
            ready: true,
            log_file,
        });
    }
    let binary = std::env::current_exe()
        .map_err(|e| CommandError::Other(format!("cannot determine the awman binary: {e}")))?;
    let args = background_args(&session.effective_config().layers().set.set_args());
    let pid = supervisor.spawn_detached(&binary, &args)?;
    let ready = wait_until_ready(&env, &supervisor, BACKGROUND_READY_TIMEOUT).await;
    let pid = supervisor.running_pid().ok().flatten().unwrap_or(pid);
    if !ready && supervisor.running_pid().ok().flatten().is_none() {
        return Err(CommandError::Other(format!(
            "the daemon exited during startup; see `awman daemon logs` ({log_file})"
        )));
    }
    Ok(DaemonStartOutcome {
        background: true,
        pid,
        ready,
        log_file,
    })
}

/// The detached child's argv: the originating `--set` overrides, then
/// `daemon start` in the foreground.
pub fn background_args(set_args: &[String]) -> Vec<String> {
    let mut args: Vec<String> = set_args
        .iter()
        .flat_map(|arg| ["--set".to_string(), arg.clone()])
        .collect();
    args.extend(["daemon".to_string(), "start".to_string()]);
    args
}

async fn wait_until_ready(
    env: &EnvSnapshot,
    supervisor: &DaemonSupervisor,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(client)) = DaemonClient::discover(env) {
            let client = client.with_timeout(Duration::from_secs(2));
            if client.call(&Request::get("/v1/health")).await.is_ok()
                || client.call(&Request::get("/v1/status")).await.is_ok()
            {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        // A child that already failed leaves no live pidfile behind; give it a
        // moment to claim one before deciding.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if tokio::time::Instant::now() + Duration::from_secs(1) >= deadline
            && supervisor.running_pid().ok().flatten().is_none()
        {
            return false;
        }
    }
}

async fn stop(
    env: &EnvSnapshot,
    now: bool,
    frontend: &mut dyn DaemonCommandFrontend,
) -> Result<DaemonStopOutcome, CommandError> {
    let supervisor = supervisor(env)?;
    let mut canceled = Vec::new();
    if now {
        // Cancel running work first, so the drain has nothing to wait for.
        match DaemonClient::discover(env)? {
            Some(client) => {
                let status: StatusResponse = client.call_as(&Request::get("/v1/status")).await?;
                if status
                    .features
                    .get("worker")
                    .is_some_and(|f| f.state != crate::data::daemon_state::FeatureState::Off)
                {
                    canceled.extend(super::api::running_jobs(&client, "worker").await?);
                    super::api::release_held_leases_for_shutdown(&client).await?;
                }
                for feature in ["worker", "squad"] {
                    let on = status
                        .features
                        .get(feature)
                        .is_some_and(|f| f.state != crate::data::daemon_state::FeatureState::Off);
                    if on {
                        let result = super::api::run_action(
                            &client,
                            "jobs.cancel",
                            &[("feature".into(), feature.into())],
                        )
                        .await?;
                        canceled.extend(
                            result["canceled"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|id| id.as_str().map(str::to_string)),
                        );
                    }
                }
            }
            None => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: "the daemon's local listener is not answering; stopping without cancelling jobs"
                        .into(),
                });
            }
        }
    }
    canceled.sort();
    canceled.dedup();
    let pid = match supervisor.terminate_running()? {
        Termination::Terminated { pid } => pid,
        Termination::NotRunning => return Err(CommandError::DaemonNotRunning),
        Termination::StalePidFile { pid } | Termination::NotAwman { pid } => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("removed a stale daemon pidfile (PID {pid} is not an awman daemon)"),
            });
            return Err(CommandError::DaemonNotRunning);
        }
    };
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    let mut exited = false;
    while !now || tokio::time::Instant::now() < deadline {
        if !DaemonSupervisor::is_process_alive(pid) || supervisor.running_pid()?.is_none() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(DaemonStopOutcome {
        stopped_pid: Some(pid),
        canceled,
        exited,
    })
}

async fn status(env: &EnvSnapshot, json: bool) -> Result<DaemonStatusOutcome, CommandError> {
    let Some(client) = DaemonClient::discover(env)? else {
        return Ok(DaemonStatusOutcome {
            running: false,
            status: None,
            json,
        });
    };
    let status: StatusResponse = client.call_as(&Request::get("/v1/status")).await?;
    Ok(DaemonStatusOutcome {
        running: true,
        status: Some(status),
        json,
    })
}

async fn logs(
    env: &EnvSnapshot,
    follow: bool,
    frontend: &mut dyn DaemonCommandFrontend,
) -> Result<DaemonLogsOutcome, CommandError> {
    let path = DataPaths::from_env(env)?.daemon().log_file();
    let log_file = path.display().to_string();
    let mut offset = match std::fs::read(&path) {
        Ok(bytes) => {
            for line in String::from_utf8_lossy(&bytes).lines() {
                frontend.log_line(line);
            }
            bytes.len() as u64
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !follow => {
            return Ok(DaemonLogsOutcome {
                log_file,
                found: false,
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(crate::data::error::DataError::io(&path, e).into()),
    };
    if follow {
        let mut pending = String::new();
        let stop = tokio::signal::ctrl_c();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
            offset = follow_once(&path, offset, &mut pending, &mut |line| {
                frontend.log_line(line)
            })?;
        }
    }
    Ok(DaemonLogsOutcome {
        log_file,
        found: true,
    })
}

/// Emit every complete line appended to `path` since `offset`; returns the new
/// offset. A truncated (rotated) file is read again from the start.
pub fn follow_once(
    path: &std::path::Path,
    offset: u64,
    pending: &mut String,
    emit: &mut dyn FnMut(&str),
) -> Result<u64, CommandError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(offset),
        Err(e) => return Err(crate::data::error::DataError::io(path, e).into()),
    };
    let len = file
        .metadata()
        .map_err(|e| crate::data::error::DataError::io(path, e))?
        .len();
    let start = if len < offset {
        pending.clear();
        0
    } else {
        offset
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|e| crate::data::error::DataError::io(path, e))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| crate::data::error::DataError::io(path, e))?;
    pending.push_str(&String::from_utf8_lossy(&bytes));
    while let Some(end) = pending.find('\n') {
        let line: String = pending.drain(..=end).collect();
        emit(line.trim_end_matches(['\n', '\r']));
    }
    Ok(start + bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::RecordingMessageSink;

    struct Fake {
        sink: RecordingMessageSink,
        hosted: bool,
        lines: Vec<String>,
    }
    impl UserMessageSink for Fake {
        fn write_message(&mut self, msg: UserMessage) {
            self.sink.write_message(msg)
        }
        fn replay_queued(&mut self) {}
    }
    #[async_trait]
    impl DaemonCommandFrontend for Fake {
        async fn host_daemon(
            &mut self,
            _env: EnvSnapshot,
            _set: crate::data::config::overrides::ConfigOverrides,
        ) -> Result<(), CommandError> {
            self.hosted = true;
            Ok(())
        }
        fn log_line(&mut self, line: &str) {
            self.lines.push(line.into())
        }
    }
    fn fake() -> Fake {
        Fake {
            sink: RecordingMessageSink::new(),
            hosted: false,
            lines: vec![],
        }
    }

    fn session(home: &std::path::Path, global: &str) -> Session {
        std::fs::write(home.join("config.json"), global).unwrap();
        Session::for_tests_with_env(
            home,
            EnvSnapshot::with_overrides([("AWMAN_HOME", home.to_str().unwrap())]),
        )
    }

    #[tokio::test]
    async fn start_refuses_when_no_feature_is_enabled() {
        let home = tempfile::tempdir().unwrap();
        let session = session(home.path(), "{}");
        assert!(enabled_features(&session).is_empty());
        let mut frontend = fake();
        let error = start(&session, false, &mut frontend).await.unwrap_err();
        assert!(matches!(error, CommandError::DaemonNoFeatures), "{error:?}");
        assert!(!frontend.hosted, "nothing may be hosted without a feature");
        assert!(error.to_string().contains("daemon.features"), "{error}");
    }

    #[tokio::test]
    async fn foreground_start_hands_the_process_to_the_frontend() {
        let home = tempfile::tempdir().unwrap();
        let session = session(
            home.path(),
            r#"{"daemon":{"features":{"engine":{"enabled":true}}}}"#,
        );
        assert_eq!(enabled_features(&session), vec![FeatureName::Engine]);
        let mut frontend = fake();
        let outcome = start(&session, false, &mut frontend).await.unwrap();
        assert!(frontend.hosted);
        assert!(!outcome.background);
        assert_eq!(outcome.pid, std::process::id());
    }

    #[test]
    fn background_reruns_daemon_start_with_the_set_overrides() {
        assert_eq!(background_args(&[]), ["daemon", "start"]);
        assert_eq!(
            background_args(&["daemon.maxConcurrentJobs=8".into()]),
            ["--set", "daemon.maxConcurrentJobs=8", "daemon", "start"]
        );
    }

    #[tokio::test]
    async fn status_without_a_daemon_reports_not_running() {
        let home = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.path().to_str().unwrap())]);
        let outcome = status(&env, true).await.unwrap();
        assert!(!outcome.running);
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            serde_json::json!({"running": false})
        );
    }

    #[tokio::test]
    async fn stop_without_a_daemon_is_daemon_not_running() {
        let home = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.path().to_str().unwrap())]);
        let error = stop(&env, false, &mut fake()).await.unwrap_err();
        assert!(matches!(error, CommandError::DaemonNotRunning));
    }

    #[tokio::test]
    async fn logs_prints_the_log_and_reports_a_missing_one() {
        let home = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.path().to_str().unwrap())]);
        let mut frontend = fake();
        let outcome = logs(&env, false, &mut frontend).await.unwrap();
        assert!(!outcome.found);
        let path = DataPaths::from_env(&env).unwrap().daemon().log_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let outcome = logs(&env, false, &mut frontend).await.unwrap();
        assert!(outcome.found);
        assert_eq!(frontend.lines, ["one", "two"]);
    }

    #[test]
    fn follow_emits_only_complete_new_lines_and_survives_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("awman.log");
        std::fs::write(&path, "old\n").unwrap();
        let mut pending = String::new();
        let mut lines = Vec::new();
        let mut offset = 4;
        std::fs::write(&path, "old\nnew\npart").unwrap();
        offset = follow_once(&path, offset, &mut pending, &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(lines, ["new"]);
        std::fs::write(&path, "old\nnew\npartial\n").unwrap();
        offset = follow_once(&path, offset, &mut pending, &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(lines, ["new", "partial"]);
        std::fs::write(&path, "rotated\n").unwrap();
        follow_once(&path, offset, &mut pending, &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(lines, ["new", "partial", "rotated"]);
    }
}
