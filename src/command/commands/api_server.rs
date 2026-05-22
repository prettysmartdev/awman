//! `ApiServerCommand` — `api start | kill | logs | status`.

use async_trait::async_trait;
use serde::Serialize;

use std::net::IpAddr;
use std::path::PathBuf;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::api_process;
use crate::engine::auth::TlsMaterial;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

/// Configuration handed from the `api start` command to Layer 3's
/// `serve_until_shutdown`. Lives in Layer 2 so the trait signature does
/// not pull Layer 3 types into the command layer.
#[derive(Debug, Clone)]
pub struct ApiServeConfig {
    pub port: u16,
    pub bind_ip: IpAddr,
    pub workdirs: Vec<PathBuf>,
    pub dangerously_skip_auth: bool,
    pub tls_material: Option<TlsMaterial>,
}

pub mod banner;

#[derive(Debug, Clone)]
pub struct ApiServerStartFlags {
    pub port: u16,
    pub workdirs: Vec<String>,
    pub background: bool,
    pub refresh_key: bool,
    pub dangerously_skip_auth: bool,
}

#[derive(Debug, Clone)]
pub struct ApiServerKillFlags {}

#[derive(Debug, Clone)]
pub struct ApiServerLogsFlags {}

#[derive(Debug, Clone)]
pub struct ApiServerStatusFlags {}

#[derive(Debug, Clone)]
pub enum ApiServerSubcommand {
    Start(ApiServerStartFlags),
    Kill(ApiServerKillFlags),
    Logs(ApiServerLogsFlags),
    Status(ApiServerStatusFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStartOutcome {
    pub port: u16,
    pub background: bool,
    pub workdirs: Vec<String>,
    pub refreshed_key: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerKillOutcome {
    pub stopped_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerLogsOutcome {
    pub log_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStatusOutcome {
    pub running: bool,
    pub pid: Option<u32>,
    /// Bound endpoint (e.g. `http://127.0.0.1:9876` or `https://...`),
    /// populated when the meta sidecar is present.
    pub bound_addr: Option<String>,
    /// Server version reported by `GET /v1/status`, when reachable.
    pub version: Option<String>,
    /// Whether the HTTP probe succeeded. `false` when the PID is alive but
    /// the server didn't respond — surfaces hung-server cases.
    pub responsive: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ApiServerOutcome {
    Start(ApiServerStartOutcome),
    Kill(ApiServerKillOutcome),
    Logs(ApiServerLogsOutcome),
    Status(ApiServerStatusOutcome),
}

/// Methods Layer 3 must provide to the api start command.
#[async_trait]
pub trait ApiServerStartCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_until_shutdown(
        &mut self,
        config: ApiServeConfig,
    ) -> Result<(), CommandError>;
}

pub trait ApiServerKillCommandFrontend: UserMessageSink + Send + Sync {}
pub trait ApiServerLogsCommandFrontend: UserMessageSink + Send + Sync {}
pub trait ApiServerStatusCommandFrontend: UserMessageSink + Send + Sync {}

/// Catch-all frontend for the umbrella `ApiServerCommand`. Includes
/// `serve_until_shutdown` so the dispatched frontend can boot the server.
#[async_trait]
pub trait ApiServerCommandFrontend: UserMessageSink + Send + Sync {
    async fn serve_until_shutdown(
        &mut self,
        config: ApiServeConfig,
    ) -> Result<(), CommandError>;
}

pub struct ApiServerCommand {
    sub: ApiServerSubcommand,
    engines: Engines,
}

impl ApiServerCommand {
    pub fn new(sub: ApiServerSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
    }

    pub fn subcommand(&self) -> &ApiServerSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for ApiServerCommand {
    type Frontend = Box<dyn ApiServerCommandFrontend>;
    type Outcome = ApiServerOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let api_paths = self.engines.auth_engine.api_paths();
        api_paths.ensure_root().map_err(CommandError::Data)?;

        let outcome = match self.sub {
            ApiServerSubcommand::Start(f) => {
                run_start(f, &self.engines, &mut *frontend, api_paths).await?
            }
            ApiServerSubcommand::Kill(_) => run_kill(api_paths, &mut *frontend)?,
            ApiServerSubcommand::Logs(_) => run_logs(api_paths, &mut *frontend)?,
            ApiServerSubcommand::Status(_) => run_status(api_paths).await?,
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

async fn run_start(
    flags: ApiServerStartFlags,
    engines: &Engines,
    frontend: &mut dyn ApiServerCommandFrontend,
    api_paths: &crate::data::fs::ApiPaths,
) -> Result<ApiServerOutcome, CommandError> {
    let pid_path = api_paths.pid_file();

    // Check if already running.
    if let Some(pid) = api_process::check_already_running(&pid_path)? {
        return Err(CommandError::ApiServerAlreadyRunning { pid });
    }

    // Resolve workdirs by merging CLI --workdirs with the global API config.
    let config_workdirs: Vec<String> = crate::data::config::global::GlobalConfig::load()
        .unwrap_or_default()
        .api
        .as_ref()
        .and_then(|h| h.work_dirs.clone())
        .unwrap_or_default();
    let workdirs = resolve_workdirs(&flags.workdirs, &config_workdirs)?;

    // --refresh-key: generate new key, print banner, exit.
    if flags.refresh_key {
        let key = engines.auth_engine.refresh_api_key()?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: banner::render_api_key_banner(key.as_str()),
        });
        return Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
            port: flags.port,
            background: false,
            workdirs: workdirs.iter().map(|p| p.display().to_string()).collect(),
            refreshed_key: true,
        }));
    }

    // Auth check: when not skipping auth, ensure an API key hash exists.
    if !flags.dangerously_skip_auth && engines.auth_engine.read_api_key_hash()?.is_none() {
        return Err(CommandError::ApiServerAuthMissing);
    }

    let workdir_strings: Vec<String> = workdirs.iter().map(|p| p.display().to_string()).collect();

    // Background mode: spawn a child process and exit.
    if flags.background {
        let binary = std::env::current_exe()
            .map_err(|e| CommandError::Other(format!("cannot determine awman binary: {e}")))?;
        let mut args = vec![
            "api".to_string(),
            "start".to_string(),
            "--port".to_string(),
            flags.port.to_string(),
        ];
        if flags.dangerously_skip_auth {
            args.push("--dangerously-skip-auth".to_string());
        }
        for w in &flags.workdirs {
            args.push("--workdirs".to_string());
            args.push(w.clone());
        }

        let log_path = api_paths.log_file();
        let child_pid = api_process::spawn_background(&binary, &args, &log_path)?;
        if child_pid > 0 {
            // Use exclusive write so a racing parallel `api start --background`
            // can't trample the PID we just spawned.
            if !api_process::write_pid_exclusive(&pid_path, child_pid)? {
                if let Some(existing) = api_process::read_pid(&pid_path)? {
                    if existing != child_pid
                        && api_process::is_process_alive(existing)
                        && api_process::pid_is_awman(existing)
                    {
                        return Err(CommandError::ApiServerAlreadyRunning { pid: existing });
                    }
                }
                // Stale or matching — overwrite.
                api_process::write_pid(&pid_path, child_pid)?;
            }
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Success,
            text: format!("API server started in background (PID {child_pid})."),
        });

        return Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
            port: flags.port,
            background: true,
            workdirs: workdir_strings,
            refreshed_key: false,
        }));
    }

    // Foreground mode: write PID race-safely, boot HTTP server, clean up on
    // exit. If the exclusive write loses the race against another fresh
    // start, surface ApiServerAlreadyRunning rather than overwriting.
    if !api_process::write_pid_exclusive(&pid_path, std::process::id())? {
        if let Some(existing) = api_process::read_pid(&pid_path)? {
            if api_process::is_process_alive(existing)
                && api_process::pid_is_awman(existing)
            {
                return Err(CommandError::ApiServerAlreadyRunning { pid: existing });
            }
        }
        // Stale file slipped through — clean up and retake.
        api_process::clear_pid(&pid_path)?;
        api_process::write_pid(&pid_path, std::process::id())?;
    }

    // TLS material: generate or load now so the bind_ip warning surfaces
    // BEFORE we hand off to serve_until_shutdown.
    let bind_ip: std::net::IpAddr = "127.0.0.1".parse().expect("static loopback ip");
    let (tls_material, regenerated) = engines.auth_engine.ensure_self_signed_tls(bind_ip)?;
    if regenerated && api_paths.tls_bind_ip_file().exists() {
        // Existing sidecar file means a previous cert was here — emit the
        // re-pin warning. (We can't reliably distinguish "first ever cert"
        // from "regenerated for new IP" without extra state, but the sidecar
        // existing post-write is good enough as a proxy.)
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text:
                "TLS cert regenerated for new bind IP — pinned remote clients will need to re-pin"
                    .into(),
        });
    }

    // Persist server metadata so `api status` and remote clients can
    // probe the right endpoint.
    let meta_path = api_paths.server_meta_file();
    let _ = api_process::write_server_meta(
        &meta_path,
        &api_process::ServerMeta {
            port: flags.port,
            bind_ip: bind_ip.to_string(),
            scheme: "https".into(),
        },
    );

    let config = ApiServeConfig {
        port: flags.port,
        bind_ip,
        workdirs,
        dangerously_skip_auth: flags.dangerously_skip_auth,
        tls_material: Some(tls_material),
    };

    let serve_result = frontend.serve_until_shutdown(config).await;

    // Always clean up PID + meta files.
    let _ = api_process::clear_pid(&pid_path);
    let _ = api_process::clear_server_meta(&meta_path);

    serve_result?;

    Ok(ApiServerOutcome::Start(ApiServerStartOutcome {
        port: flags.port,
        background: false,
        workdirs: workdir_strings,
        refreshed_key: false,
    }))
}

fn run_kill(
    api_paths: &crate::data::fs::ApiPaths,
    frontend: &mut dyn ApiServerCommandFrontend,
) -> Result<ApiServerOutcome, CommandError> {
    let pid_path = api_paths.pid_file();

    let pid = match api_process::read_pid(&pid_path)? {
        Some(pid) => pid,
        None => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "No API server is running (no PID file found).".to_string(),
            });
            return Err(CommandError::ApiServerNotRunning);
        }
    };

    if !api_process::is_process_alive(pid) {
        api_process::clear_pid(&pid_path)?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: format!("Stale PID file removed (PID {pid} was not running)."),
        });
        return Err(CommandError::ApiServerNotRunning);
    }

    if !api_process::pid_is_awman(pid) {
        api_process::clear_pid(&pid_path)?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: format!(
                "PID {pid} is alive but is not an awman server; stale PID file cleaned up."
            ),
        });
        return Err(CommandError::ApiServerNotRunning);
    }

    api_process::kill_process(pid)?;
    api_process::clear_pid(&pid_path)?;
    let _ = api_process::clear_server_meta(&api_paths.server_meta_file());

    frontend.write_message(UserMessage {
        level: MessageLevel::Success,
        text: format!("API server (PID {pid}) stopped."),
    });

    Ok(ApiServerOutcome::Kill(ApiServerKillOutcome {
        stopped_pid: Some(pid),
    }))
}

fn run_logs(
    api_paths: &crate::data::fs::ApiPaths,
    frontend: &mut dyn ApiServerCommandFrontend,
) -> Result<ApiServerOutcome, CommandError> {
    let log_path = api_paths.log_file();
    let log_str = log_path.display().to_string();

    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            for line in content.lines() {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line.to_string(),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!("Log file not found: {log_str}"),
            });
        }
        Err(e) => {
            return Err(CommandError::Data(crate::data::error::DataError::io(
                &log_path, e,
            )));
        }
    }

    Ok(ApiServerOutcome::Logs(ApiServerLogsOutcome {
        log_path: log_str,
    }))
}

async fn run_status(
    api_paths: &crate::data::fs::ApiPaths,
) -> Result<ApiServerOutcome, CommandError> {
    let pid_path = api_paths.pid_file();
    let meta_path = api_paths.server_meta_file();

    let pid = match api_process::check_already_running(&pid_path)? {
        Some(pid) => pid,
        None => {
            // Cleanup any orphan meta file when no server is running.
            let _ = api_process::clear_server_meta(&meta_path);
            return Ok(ApiServerOutcome::Status(ApiServerStatusOutcome {
                running: false,
                pid: None,
                bound_addr: None,
                version: None,
                responsive: false,
            }));
        }
    };

    let meta = api_process::read_server_meta(&meta_path)?;
    let bound_addr = meta
        .as_ref()
        .map(|m| format!("{}://{}:{}", m.scheme, m.bind_ip, m.port));

    // HTTP-probe the running server when we know its endpoint. A short
    // timeout keeps `status` snappy; a missing/timed-out probe means the
    // process is alive but the server is not responsive.
    let (responsive, version) = if let Some(m) = meta.as_ref() {
        let probe_url = format!("{}://127.0.0.1:{}/v1/status", m.scheme, m.port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .danger_accept_invalid_certs(true) // self-signed certs on loopback
            .build()
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        match client.get(&probe_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.json::<serde_json::Value>().await.ok();
                let v = body
                    .as_ref()
                    .and_then(|b| b.get("version"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (true, v)
            }
            _ => (false, None),
        }
    } else {
        (false, None)
    };

    Ok(ApiServerOutcome::Status(ApiServerStatusOutcome {
        running: true,
        pid: Some(pid),
        bound_addr,
        version,
        responsive,
    }))
}

/// Resolve the merged-and-validated workdir allowlist (per spec §6.4a).
/// Concatenate CLI-supplied workdirs and config workdirs, canonicalize,
/// deduplicate, and reject missing paths.
pub fn resolve_workdirs(
    cli: &[String],
    config: &[String],
) -> Result<Vec<std::path::PathBuf>, CommandError> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for raw in cli.iter().chain(config.iter()) {
        let path = std::path::PathBuf::from(raw);
        if !path.exists() {
            return Err(CommandError::ApiWorkdirNotFound { path });
        }
        let canon = path.canonicalize().unwrap_or(path);
        if seen.insert(canon.clone()) {
            out.push(canon);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_workdirs_dedupes_overlapping_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().to_str().unwrap().to_string();
        let merged = resolve_workdirs(std::slice::from_ref(&s), std::slice::from_ref(&s)).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn resolve_workdirs_errors_on_missing_path() {
        let err = resolve_workdirs(&["/no/such/path".into()], &[]).unwrap_err();
        assert!(matches!(err, CommandError::ApiWorkdirNotFound { .. }));
    }

    #[test]
    fn resolve_workdirs_merges_cli_and_config() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let cli = vec![tmp_a.path().to_str().unwrap().to_string()];
        let cfg = vec![tmp_b.path().to_str().unwrap().to_string()];
        let merged = resolve_workdirs(&cli, &cfg).unwrap();
        assert_eq!(merged.len(), 2, "must contain both cli and config entries");
    }

    use crate::command::dispatch::Engines;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::data::fs::api_paths::ApiPaths;
    use crate::engine::auth::AuthEngine;
    use crate::engine::message::{UserMessage, UserMessageSink};
    use std::sync::Arc;

    fn make_engines(tmp: &std::path::Path) -> Engines {
        let api_paths = ApiPaths::at_root(tmp);
        let auth_paths = AuthPathResolver::at_home(tmp);
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            auth_paths.clone(),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, api_paths));
        let workflow_state_store =
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(tmp));
        Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    struct NullFrontend {
        messages: Vec<String>,
    }
    impl UserMessageSink for NullFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg.text);
        }
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl ApiServerCommandFrontend for NullFrontend {
        async fn serve_until_shutdown(
            &mut self,
            _config: ApiServeConfig,
        ) -> Result<(), crate::command::error::CommandError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_refresh_key_short_circuits_without_checking_auth() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();

        // Ensure API root exists.
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: true,
            dangerously_skip_auth: false, // no auth configured, but refresh_key skips check
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(result.is_ok(), "refresh_key must short-circuit: {result:?}");
        if let Ok(ApiServerOutcome::Start(outcome)) = result {
            assert!(outcome.refreshed_key, "refreshed_key must be true");
        }
    }

    #[tokio::test]
    async fn start_without_auth_configured_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: false,
            dangerously_skip_auth: false,
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(
            matches!(result, Err(CommandError::ApiServerAuthMissing)),
            "missing auth hash must error with ApiServerAuthMissing: {result:?}"
        );
    }

    #[tokio::test]
    async fn start_dangerously_skip_auth_proceeds_without_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let flags = ApiServerStartFlags {
            port: 9876,
            workdirs: Vec::new(),
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
        };

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_start(flags, &engines, &mut frontend, &api_paths).await;
        assert!(
            result.is_ok(),
            "dangerously_skip_auth must bypass auth check: {result:?}"
        );
    }

    #[test]
    fn kill_no_pid_file_returns_api_not_running_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_kill(&api_paths, &mut frontend);
        assert!(
            matches!(result, Err(CommandError::ApiServerNotRunning)),
            "kill with no PID file must surface ApiServerNotRunning: {result:?}"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("No API") || m.contains("no PID")),
            "must emit a warning; got: {:?}",
            frontend.messages
        );
    }

    #[test]
    fn kill_stale_pid_file_is_cleaned_up_and_returns_api_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();
        let pid_path = api_paths.pid_file();

        // Write a PID that can't possibly be alive.
        crate::data::fs::api_process::write_pid(&pid_path, u32::MAX - 1).unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_kill(&api_paths, &mut frontend);
        assert!(
            matches!(result, Err(CommandError::ApiServerNotRunning)),
            "stale PID must surface ApiServerNotRunning: {result:?}"
        );
        assert!(
            !pid_path.exists(),
            "PID file must be removed after stale detection"
        );
    }

    #[tokio::test]
    async fn status_no_pid_file_returns_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let result = run_status(&api_paths).await;
        assert!(result.is_ok());
        if let Ok(ApiServerOutcome::Status(outcome)) = result {
            assert!(!outcome.running);
            assert!(outcome.pid.is_none());
            assert!(!outcome.responsive, "no server → not responsive");
            assert!(outcome.bound_addr.is_none());
            assert!(outcome.version.is_none());
        }
    }

    #[tokio::test]
    async fn status_with_alive_pid_but_no_meta_reports_not_responsive() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        // Write our own PID — definitely alive and "awman"-named on most CI.
        // On platforms where pid_is_awman returns false for the test binary,
        // check_already_running will treat it as stale; that's still a
        // useful signal — running=false, responsive=false.
        crate::data::fs::api_process::write_pid(
            &api_paths.pid_file(),
            std::process::id(),
        )
        .unwrap();

        let result = run_status(&api_paths).await.unwrap();
        if let ApiServerOutcome::Status(outcome) = result {
            // Either the test binary identifies as "awman" (running=true) or
            // not (running=false, stale-cleanup). In both cases responsive=false
            // because we wrote no server meta.
            assert!(!outcome.responsive, "no meta + no server → not responsive");
        }
    }

    #[test]
    fn logs_missing_log_file_emits_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_logs(&api_paths, &mut frontend);
        assert!(
            result.is_ok(),
            "missing log file must not error: {result:?}"
        );
        assert!(
            frontend
                .messages
                .iter()
                .any(|m| m.contains("not found") || m.contains("Log")),
            "must emit log-not-found warning; got: {:?}",
            frontend.messages
        );
    }

    #[test]
    fn logs_existing_log_file_streams_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let api_paths = engines.auth_engine.api_paths().clone();
        api_paths.ensure_root().unwrap();

        // Write a log file.
        let log_path = api_paths.log_file();
        std::fs::write(&log_path, "line one\nline two\nline three\n").unwrap();

        let mut frontend = NullFrontend {
            messages: Vec::new(),
        };
        let result = run_logs(&api_paths, &mut frontend);
        assert!(result.is_ok());
        assert_eq!(frontend.messages.len(), 3, "must stream all lines");
        assert_eq!(frontend.messages[0], "line one");
        assert_eq!(frontend.messages[2], "line three");
    }
}
