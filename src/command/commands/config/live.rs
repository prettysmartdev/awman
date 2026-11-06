//! Live apply for `awman config set` / `unset`: after a file is written,
//! tell a running daemon to re-read its config (`POST /v1/config/reload` on
//! the local listener) and report what happened.
//!
//! Each running daemon is reached through its owner-authenticated local
//! socket or named pipe. Reload never reads or requires a stored bearer key.

use serde::Serialize;

use crate::data::config::fields::ConfigKey;
use crate::data::session::Session;

/// What `config set` / `unset` reports after writing a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum LiveApply {
    /// A running daemon re-read the file and applied the key.
    Applied,
    /// No daemon is running: the next start reads the new value.
    NextStart,
    /// A daemon is running but the key is not live (or the daemon said so).
    RestartRequired,
    /// A daemon is running and the key is live, but this process holds no
    /// credential for it.
    NoCredential,
    /// The reload request failed; the value still applies on the next start.
    Failed(String),
}

impl LiveApply {
    /// The line printed after `wrote <path>`.
    pub fn message(&self) -> String {
        match self {
            Self::Applied => "applied to the running daemon".into(),
            Self::NextStart => "takes effect on next start".into(),
            Self::RestartRequired => {
                "restart the daemon to apply (awman api kill && awman api start)".into()
            }
            Self::NoCredential => {
                "takes effect on next start (no local credential for the running daemon; see \
                 docs/07-configuration.md)"
                    .into()
            }
            Self::Failed(reason) => {
                format!("takes effect on next start (the running daemon did not reload: {reason})")
            }
        }
    }
}

/// A daemon this process found running, and how to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningDaemon {
    pub name: &'static str,
    pub base_url: String,
    pub local_endpoint: Option<std::path::PathBuf>,
    pub bearer: Option<String>,
    pub pinned_cert_pem: Option<String>,
    /// `false` when the daemon requires auth and no credential was found.
    pub reachable: bool,
}

/// The reload response fields `config set` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ReloadReply {
    #[serde(default)]
    pub applied: Vec<String>,
    #[serde(default)]
    pub restart_required: Vec<String>,
}

/// The seam `config set` talks to daemons through; tests substitute it.
pub trait DaemonReloader: Send + Sync {
    fn running(&self, session: &Session) -> Vec<RunningDaemon>;
    fn reload(&self, daemon: &RunningDaemon) -> Result<ReloadReply, String>;
}

/// Decide the live-apply outcome for `key` (written at concrete `path`).
pub fn apply(
    reloader: &dyn DaemonReloader,
    session: &Session,
    key: &ConfigKey,
    path: &str,
) -> LiveApply {
    let daemons = reloader.running(session);
    if daemons.is_empty() {
        return LiveApply::NextStart;
    }
    if !key.live {
        return LiveApply::RestartRequired;
    }
    let mut outcome: Option<LiveApply> = None;
    for daemon in &daemons {
        let this = if !daemon.reachable {
            LiveApply::NoCredential
        } else {
            match reloader.reload(daemon) {
                Ok(reply) if reply.restart_required.iter().any(|p| covers(p, path)) => {
                    LiveApply::RestartRequired
                }
                Ok(_) => LiveApply::Applied,
                Err(reason) => LiveApply::Failed(reason),
            }
        };
        // Report the least-applied outcome across daemons.
        outcome = Some(match (outcome, this) {
            (None, this) => this,
            (Some(LiveApply::Applied), this) => this,
            (Some(prev), _) => prev,
        });
    }
    outcome.unwrap_or(LiveApply::NextStart)
}

/// Whether a reported path is `path` or one of its ancestors/descendants
/// (`unset workServers.a` reports `workServers.a.url`).
fn covers(reported: &str, path: &str) -> bool {
    reported == path
        || reported.starts_with(&format!("{path}."))
        || path.starts_with(&format!("{reported}."))
}

/// The production reloader: finds the API server and the squad daemon by
/// their pidfiles and `server.json`, and posts to their loopback listener.
pub struct LocalDaemons;

impl LocalDaemons {
    fn probe(
        name: &'static str,
        paths: crate::data::fs::daemon_paths::DaemonPaths,
        unit: &'static str,
        label: &'static str,
    ) -> Option<(crate::data::fs::daemon_process::ServerMeta, &'static str)> {
        let process = crate::data::fs::daemon_process::DaemonProcess::new(paths, unit, label);
        let pid =
            crate::engine::daemon::supervisor::check_already_running(&process.paths().pid_file())
                .ok()
                .flatten()?;
        let _ = pid;
        process.read_meta().ok().flatten().map(|meta| (meta, name))
    }

    fn base_url(meta: &crate::data::fs::daemon_process::ServerMeta) -> String {
        let host = match meta.bind_ip.as_str() {
            "0.0.0.0" | "" => "127.0.0.1".to_string(),
            "::" => "[::1]".to_string(),
            ip if ip.contains(':') => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        format!("{}://{}:{}", meta.scheme, host, meta.port)
    }
}

impl DaemonReloader for LocalDaemons {
    fn running(&self, session: &Session) -> Vec<RunningDaemon> {
        use crate::data::fs::daemon_process::{
            API_PLIST_LABEL, API_UNIT_NAME, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
        };
        let env = session.env();
        let mut out = Vec::new();
        if let Ok(api) = crate::data::fs::api_paths::ApiPaths::from_env(env) {
            if let Some((meta, name)) =
                Self::probe("api", api.daemon(), API_UNIT_NAME, API_PLIST_LABEL)
            {
                let base_url = Self::base_url(&meta);

                out.push(RunningDaemon {
                    name,
                    reachable: true,
                    local_endpoint: Some(api.daemon().local_endpoint()),
                    base_url,
                    bearer: None,
                    pinned_cert_pem: None,
                });
            }
        }
        if let Ok(squad) = crate::data::fs::squad_paths::SquadPaths::from_env(env) {
            if let Some((meta, name)) =
                Self::probe("squad", squad.daemon(), SQUAD_UNIT_NAME, SQUAD_PLIST_LABEL)
            {
                out.push(RunningDaemon {
                    name,
                    reachable: true,
                    local_endpoint: Some(squad.daemon().local_endpoint()),
                    base_url: Self::base_url(&meta),
                    bearer: None,
                    pinned_cert_pem: None,
                });
            }
        }
        out
    }

    fn reload(&self, daemon: &RunningDaemon) -> Result<ReloadReply, String> {
        let daemon = daemon.clone();
        // `config` runs inside the CLI's tokio runtime; the request is one
        // short call, so it runs on a scoped thread with its own runtime
        // rather than threading `async` through the edit loop.
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| e.to_string())?;
                    runtime.block_on(post_reload(&daemon))
                })
                .join()
                .map_err(|_| "reload request panicked".to_string())?
        })
    }
}

/// Whether a configured target URL names this daemon's local listener.
#[cfg(test)]
fn same_endpoint(url: &str, meta: &crate::data::fs::daemon_process::ServerMeta) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let loopback = parsed.host_str().is_some_and(|h| {
        crate::engine::remote::http_core::HttpCore::is_loopback_addr(h) || h == meta.bind_ip
    });
    loopback && parsed.port_or_known_default() == Some(meta.port)
}

async fn post_reload(daemon: &RunningDaemon) -> Result<ReloadReply, String> {
    if let Some(endpoint) = &daemon.local_endpoint {
        let client =
            crate::command::commands::daemon::api::DaemonApiClient::local(endpoint.clone());
        let body = client
            .action("config.reload", &[])
            .await
            .map_err(|e| e.to_string())?;
        return serde_json::from_value(body).map_err(|e| e.to_string());
    }
    Err("owner-authenticated local endpoint is unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::fields;
    use std::sync::Mutex;

    struct Fake {
        daemons: Vec<RunningDaemon>,
        reply: Result<ReloadReply, String>,
        calls: Mutex<usize>,
    }

    impl DaemonReloader for Fake {
        fn running(&self, _: &Session) -> Vec<RunningDaemon> {
            self.daemons.clone()
        }
        fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
            *self.calls.lock().unwrap() += 1;
            self.reply.clone()
        }
    }

    fn daemon(reachable: bool) -> RunningDaemon {
        RunningDaemon {
            name: "api",
            base_url: "https://127.0.0.1:9876".into(),
            local_endpoint: None,
            bearer: reachable.then(|| "k".into()),
            pinned_cert_pem: None,
            reachable,
        }
    }

    fn fake(daemons: Vec<RunningDaemon>, reply: Result<ReloadReply, String>) -> Fake {
        Fake {
            daemons,
            reply,
            calls: Mutex::new(0),
        }
    }

    fn session() -> Session {
        Session::for_tests(std::path::Path::new("/tmp"))
    }

    fn key(path: &str) -> &'static ConfigKey {
        fields::resolve(path).unwrap().key
    }

    #[test]
    fn no_daemon_means_next_start() {
        let f = fake(vec![], Ok(ReloadReply::default()));
        let live = key("daemon.maxConcurrentJobs");
        assert_eq!(apply(&f, &session(), live, live.path), LiveApply::NextStart);
        assert_eq!(*f.calls.lock().unwrap(), 0);
    }

    #[test]
    fn a_live_key_is_applied_by_a_running_daemon() {
        let f = fake(vec![daemon(true)], Ok(ReloadReply::default()));
        let live = key("daemon.maxConcurrentJobs");
        assert_eq!(apply(&f, &session(), live, live.path), LiveApply::Applied);
        assert_eq!(
            LiveApply::Applied.message(),
            "applied to the running daemon"
        );
    }

    #[test]
    fn a_non_live_key_needs_a_restart_without_asking_the_daemon() {
        let f = fake(vec![daemon(true)], Ok(ReloadReply::default()));
        let k = key("daemon.network.listen");
        assert!(!k.live);
        assert_eq!(apply(&f, &session(), k, k.path), LiveApply::RestartRequired);
        assert_eq!(*f.calls.lock().unwrap(), 0);
        assert!(LiveApply::RestartRequired
            .message()
            .starts_with("restart the daemon to apply"));
    }

    #[test]
    fn the_daemon_can_report_a_restart_is_required() {
        let reply = ReloadReply {
            applied: vec![],
            restart_required: vec!["daemon.maxConcurrentJobs".into()],
        };
        let f = fake(vec![daemon(true)], Ok(reply));
        let live = key("daemon.maxConcurrentJobs");
        assert_eq!(
            apply(&f, &session(), live, live.path),
            LiveApply::RestartRequired
        );
    }

    #[test]
    fn no_credential_and_failures_fall_back_to_next_start() {
        let live = key("daemon.maxConcurrentJobs");
        let f = fake(vec![daemon(false)], Ok(ReloadReply::default()));
        assert_eq!(
            apply(&f, &session(), live, live.path),
            LiveApply::NoCredential
        );
        let f = fake(vec![daemon(true)], Err("refused".into()));
        let got = apply(&f, &session(), live, live.path);
        assert_eq!(got, LiveApply::Failed("refused".into()));
        assert!(got.message().starts_with("takes effect on next start"));
    }

    #[test]
    fn endpoint_matching_is_by_loopback_and_port() {
        let meta = crate::data::fs::daemon_process::ServerMeta {
            port: 9876,
            bind_ip: "127.0.0.1".into(),
            scheme: "https".into(),
            auth_disabled: false,
        };
        assert!(same_endpoint("https://127.0.0.1:9876", &meta));
        assert!(same_endpoint("https://localhost:9876/", &meta));
        assert!(!same_endpoint("https://127.0.0.1:9877", &meta));
        assert!(!same_endpoint("https://example.com:9876", &meta));
        assert_eq!(LocalDaemons::base_url(&meta), "https://127.0.0.1:9876");
    }
}
