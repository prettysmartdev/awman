//! Live apply for `awman config set` / `unset`: after a file is written,
//! tell the running daemon to re-read its config (`POST /v1/config/reload`
//! over its owner-only local listener) and report what happened.
//!
//! There is no bearer key: the local socket or named pipe proves the caller
//! is the daemon's owner.

use serde::Serialize;

use crate::data::config::fields::ConfigKey;
use crate::data::session::Session;

/// What `config set` / `unset` reports after writing a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum LiveApply {
    /// The running daemon re-read the file and applied the key.
    Applied,
    /// No daemon is running: the next start reads the new value.
    NextStart,
    /// A daemon is running but the key is not live (or the daemon said so).
    RestartRequired,
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
                "restart the daemon to apply (awman daemon stop && awman daemon start)".into()
            }
            Self::Failed(reason) => {
                format!("takes effect on next start (the running daemon did not reload: {reason})")
            }
        }
    }
}

/// The running daemon, and where its local listener is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningDaemon {
    pub pid: u32,
    pub local_endpoint: std::path::PathBuf,
}

/// The reload response fields `config set` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ReloadReply {
    #[serde(default)]
    pub applied: Vec<String>,
    #[serde(default)]
    pub restart_required: Vec<String>,
}

/// The seam `config set` talks to the daemon through; tests substitute it.
pub trait DaemonReloader: Send + Sync {
    fn running(&self, session: &Session) -> Option<RunningDaemon>;
    fn reload(&self, daemon: &RunningDaemon) -> Result<ReloadReply, String>;
}

/// Decide the live-apply outcome for `key` (written at concrete `path`).
pub fn apply(
    reloader: &dyn DaemonReloader,
    session: &Session,
    key: &ConfigKey,
    path: &str,
) -> LiveApply {
    let Some(daemon) = reloader.running(session) else {
        return LiveApply::NextStart;
    };
    if !key.live {
        return LiveApply::RestartRequired;
    }
    match reloader.reload(&daemon) {
        Ok(reply) if reply.restart_required.iter().any(|p| covers(p, path)) => {
            LiveApply::RestartRequired
        }
        Ok(_) => LiveApply::Applied,
        Err(reason) => LiveApply::Failed(reason),
    }
}

/// Whether a reported path is `path` or one of its ancestors/descendants
/// (`unset workServers.a` reports `workServers.a.url`).
fn covers(reported: &str, path: &str) -> bool {
    reported == path
        || reported.starts_with(&format!("{path}."))
        || path.starts_with(&format!("{reported}."))
}

/// The production reloader: the daemon found through its live pidfile and
/// `server.json`, reached over its local listener.
pub struct LocalDaemons;

impl DaemonReloader for LocalDaemons {
    fn running(&self, session: &Session) -> Option<RunningDaemon> {
        let paths = crate::data::fs::DataPaths::from_env(session.env())
            .ok()?
            .daemon();
        crate::engine::remote::local::discover(&paths)
            .ok()
            .flatten()
            .map(|endpoint| RunningDaemon {
                pid: endpoint.pid,
                local_endpoint: endpoint.address,
            })
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

async fn post_reload(daemon: &RunningDaemon) -> Result<ReloadReply, String> {
    let client = crate::engine::remote::local::LocalClient::new(daemon.local_endpoint.clone());
    let response = client
        .send("POST", "/v1/config/reload", None)
        .await
        .map_err(|e| e.to_string())?;
    if response.status >= 400 {
        return Err(format!(
            "HTTP {}: {}",
            response.status,
            crate::command::commands::daemon::client::body_text(&response.body)
        ));
    }
    serde_json::from_value(response.body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::fields;
    use std::sync::Mutex;

    struct Fake {
        daemon: Option<RunningDaemon>,
        reply: Result<ReloadReply, String>,
        calls: Mutex<usize>,
    }

    impl DaemonReloader for Fake {
        fn running(&self, _: &Session) -> Option<RunningDaemon> {
            self.daemon.clone()
        }
        fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
            *self.calls.lock().unwrap() += 1;
            self.reply.clone()
        }
    }

    fn daemon() -> RunningDaemon {
        RunningDaemon {
            pid: 42,
            local_endpoint: "/tmp/awman-test/api.sock".into(),
        }
    }

    fn fake(daemon: Option<RunningDaemon>, reply: Result<ReloadReply, String>) -> Fake {
        Fake {
            daemon,
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
        let f = fake(None, Ok(ReloadReply::default()));
        let live = key("daemon.maxConcurrentJobs");
        assert_eq!(apply(&f, &session(), live, live.path), LiveApply::NextStart);
        assert_eq!(*f.calls.lock().unwrap(), 0);
        assert_eq!(LiveApply::NextStart.message(), "takes effect on next start");
    }

    #[test]
    fn a_live_key_is_applied_by_the_running_daemon() {
        for path in [
            "daemon.maxConcurrentJobs",
            "daemon.features.worker.enabled",
            "daemon.network.enabled",
            "daemon.network.tlsSans",
        ] {
            let f = fake(Some(daemon()), Ok(ReloadReply::default()));
            let live = key(path);
            assert!(live.live, "{path}");
            assert_eq!(
                apply(&f, &session(), live, path),
                LiveApply::Applied,
                "{path}"
            );
            assert_eq!(*f.calls.lock().unwrap(), 1);
        }
        assert_eq!(
            LiveApply::Applied.message(),
            "applied to the running daemon"
        );
    }

    #[test]
    fn a_non_live_key_needs_a_restart_without_asking_the_daemon() {
        let f = fake(Some(daemon()), Ok(ReloadReply::default()));
        let k = key("daemon.network.listen");
        assert!(!k.live);
        assert_eq!(apply(&f, &session(), k, k.path), LiveApply::RestartRequired);
        assert_eq!(*f.calls.lock().unwrap(), 0);
        assert_eq!(
            LiveApply::RestartRequired.message(),
            "restart the daemon to apply (awman daemon stop && awman daemon start)"
        );
    }

    #[test]
    fn the_daemon_can_report_a_restart_is_required() {
        let reply = ReloadReply {
            applied: vec![],
            restart_required: vec!["daemon.maxConcurrentJobs".into()],
        };
        let f = fake(Some(daemon()), Ok(reply));
        let live = key("daemon.maxConcurrentJobs");
        assert_eq!(
            apply(&f, &session(), live, live.path),
            LiveApply::RestartRequired
        );
    }

    #[test]
    fn a_failed_reload_falls_back_to_next_start() {
        let live = key("daemon.maxConcurrentJobs");
        let f = fake(Some(daemon()), Err("refused".into()));
        let got = apply(&f, &session(), live, live.path);
        assert_eq!(got, LiveApply::Failed("refused".into()));
        assert!(got.message().starts_with("takes effect on next start"));
    }

    #[test]
    fn the_production_reloader_finds_no_daemon_in_an_empty_home() {
        let home = tempfile::tempdir().unwrap();
        let session = Session::for_tests_isolated(home.path(), home.path());
        assert_eq!(LocalDaemons.running(&session), None);
    }
}
