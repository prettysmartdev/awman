//! One host transport path for push sessions and pulled leases.
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::auth_classify::{GitTransportFailure, ResponseTrace};
use super::{host_command, GitCommand, GitEngine, GitFrontend};
use crate::data::config::env::EnvSnapshot;
use crate::data::git_transport::{GitConfigScope, GitTransportOp, GitTransportOptions, SafeRemote};
use crate::engine::error::EngineError;

type Result<T> = std::result::Result<T, EngineError>;

#[cfg(windows)]
type WindowsSpawn =
    fn(&mut Command) -> std::io::Result<(std::process::Child, Box<dyn Send + Sync>)>;
#[cfg(windows)]
static WINDOWS_SPAWN: std::sync::OnceLock<WindowsSpawn> = std::sync::OnceLock::new();
/// Install the binary's native process containment boundary.
#[cfg(windows)]
pub fn install_windows_spawn(spawn: WindowsSpawn) {
    let _ = WINDOWS_SPAWN.set(spawn);
}

fn spawn_contained(
    command: &mut Command,
) -> std::io::Result<(std::process::Child, Box<dyn Send + Sync>)> {
    #[cfg(windows)]
    {
        WINDOWS_SPAWN.get().ok_or_else(|| {
            std::io::Error::other("native Windows Git process containment is not installed")
        })?(command)
    }
    #[cfg(not(windows))]
    {
        command
            .spawn()
            .map(|child| (child, Box::new(()) as Box<dyn Send + Sync>))
    }
}

/// A helper installation contains paths/handles only, never a secret in argv or env.
pub trait CredentialHelper: Send + Sync + std::fmt::Debug {
    fn git_config_args(&self) -> Vec<String>;
    fn env(&self) -> Vec<(String, String)>;
    fn redact(&self, text: &str) -> String;
}
#[derive(Debug, Clone)]
pub enum CredentialSource {
    Ambient,
    Anonymous,
    Helper(Arc<dyn CredentialHelper>),
}
#[derive(Debug, Clone)]
pub struct HostGitTransport {
    options: GitTransportOptions,
    credentials: CredentialSource,
    private_home: Arc<std::sync::Mutex<Option<tempfile::TempDir>>>,
}
impl HostGitTransport {
    pub fn user(credentials: CredentialSource) -> Self {
        Self {
            options: GitTransportOptions {
                scope: GitConfigScope::UserGlobal,
                allow_local: false,
            },
            credentials,
            private_home: Arc::default(),
        }
    }
    pub fn isolated(credentials: CredentialSource, allow_local: bool) -> Self {
        assert!(
            !matches!(credentials, CredentialSource::Ambient),
            "isolated Git cannot use ambient credentials"
        );
        Self {
            options: GitTransportOptions {
                scope: GitConfigScope::Isolated,
                allow_local,
            },
            credentials,
            private_home: Arc::default(),
        }
    }
    /// Trusted local CLI operations and private test repositories only.
    pub fn allow_local(mut self) -> Self {
        self.options.allow_local = true;
        self
    }
    pub fn check_url(&self, url: &str) -> Result<()> {
        if SafeRemote::url_has_credentials(url) || SafeRemote::url_has_parameters(url) {
            return Err(EngineError::Config(format!(
                "remote {}: credentials in the URL are not accepted",
                SafeRemote::from_url(url)
            )));
        }
        if self.options.scope == GitConfigScope::Isolated {
            return super::lease_transport::validate_transport_url(url, self.options.allow_local);
        }
        let lower = url.to_ascii_lowercase();
        let network = ["https://", "http://", "ssh://", "git://"]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            || (!url.contains("://")
                && url
                    .split_once(':')
                    .is_some_and(|(host, _)| host.len() > 1 && !host.contains(['/', '\\'])));
        if !network && !self.options.allow_local {
            return Err(EngineError::Config(
                "local Git remotes are not permitted".into(),
            ));
        }
        Ok(())
    }
    /// The only transport command constructor; operation arguments follow.
    pub fn command(&self, env: &EnvSnapshot, cwd: &Path, remote_name: &str) -> Result<Command> {
        self.command_guarded(
            env,
            cwd,
            remote_name,
            &TransportGuard::new(CancellationToken::new()),
        )
    }
    pub fn command_guarded(
        &self,
        env: &EnvSnapshot,
        cwd: &Path,
        remote_name: &str,
        guard: &TransportGuard,
    ) -> Result<Command> {
        guard.check()?;
        let isolated;
        let env = if self.options.scope == GitConfigScope::Isolated
            || !matches!(self.credentials, CredentialSource::Ambient)
        {
            isolated = env.for_lease_git();
            &isolated
        } else {
            env
        };
        let mut command = host_command::transport_command_guarded(env, cwd, remote_name, guard)?;
        if !matches!(self.credentials, CredentialSource::Ambient) {
            let mut home = self
                .private_home
                .lock()
                .map_err(|_| EngineError::Git("private Git HOME lock failed".into()))?;
            if home.is_none() {
                *home = Some(tempfile::tempdir().map_err(|e| EngineError::Git(e.to_string()))?);
            }
            let home = home.as_ref().expect("initialized private Git HOME");
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("XDG_CONFIG_HOME", home.path());
        }
        if self.options.scope == GitConfigScope::Isolated {
            command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        }
        command
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .args(["-c", "credential.interactive=false"]);
        command.args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            "protocol.git.allow=always",
        ]);
        if !self.options.allow_local {
            command.args(["-c", "protocol.file.allow=never"]);
        } else {
            command.args(["-c", "protocol.file.allow=always"]);
        }
        match &self.credentials {
            CredentialSource::Ambient => {}
            CredentialSource::Anonymous => {
                command.args(["-c", "credential.helper="]);
            }
            CredentialSource::Helper(helper) => {
                command
                    .args(["-c", "credential.helper="])
                    .args(helper.git_config_args())
                    .envs(helper.env());
            }
        }
        // Avoid curl internally replaying an uncertain receive-pack POST on
        // a reused connection, outside the publisher's intervening head check.
        command.args([
            "-c",
            "http.version=HTTP/1.1",
            "-c",
            "http.extraHeader=Connection: close",
        ]);
        Ok(command)
    }
    /// Run once. Retry policy belongs to the publisher, never this operation.
    pub fn run(
        &self,
        mut command: Command,
        args: &[&str],
        op: GitTransportOp,
        remote: &SafeRemote,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<Output> {
        guard.check()?;
        if guard.remaining_timeout().is_zero() {
            return Err(GitTransportFailure {
                class: super::auth_classify::GitFailureClass::Transport,
                op,
                remote: remote.clone(),
                timed_out: true,
            }
            .into_error(""));
        }
        // Only the operation is echoed: neither helper configuration nor URL parameters.
        sink.command_started(&GitCommand::new(&[op.as_str()]));
        let io =
            |e: std::io::Error| EngineError::Git(format!("git {} to {remote}: {e}", op.as_str()));
        let mut stdout = tempfile::tempfile().map_err(io)?;
        let mut stderr = tempfile::tempfile().map_err(io)?;
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().map_err(io)?)
            .stderr(stderr.try_clone().map_err(io)?);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let trace = ResponseTrace::attach(&mut command)?;
        guard.check()?;
        if let Some((_, started)) = &guard.start_deadline {
            started.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let (mut child, job) = spawn_contained(&mut command).map_err(io)?;
        let mut job = Some(job);
        let started = std::time::Instant::now();
        let mut timed_out = false;
        let mut last_progress = 0;
        let mut last_activity = started;
        let mut last_report = started;
        let status = loop {
            let stopped = guard.check();
            let progress = stdout.metadata().map(|m| m.len()).unwrap_or(0)
                + stderr.metadata().map(|m| m.len()).unwrap_or(0);
            if progress != last_progress {
                last_progress = progress;
                last_activity = std::time::Instant::now();
                if guard.inactivity
                    && matches!(op, GitTransportOp::Clone | GitTransportOp::Fetch)
                    && last_report.elapsed() >= Duration::from_secs(5)
                {
                    sink.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Info,
                        text: format!("git {} to {remote}: transfer in progress", op.as_str()),
                    });
                    last_report = last_activity;
                }
            }
            let timed = if guard.inactivity {
                last_activity.elapsed() >= guard.timeout
            } else {
                started.elapsed() >= guard.timeout
            };
            if stopped.is_err() || guard.remaining_timeout().is_zero() || timed {
                #[cfg(unix)]
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = child.kill();
                drop(job.take());
                let status = child.wait().map_err(io)?;
                stopped?;
                timed_out = true;
                break status;
            }
            if let Some(status) = child.try_wait().map_err(io)? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(job);
        guard.check()?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout
            .rewind()
            .and_then(|_| stdout.read_to_end(&mut out))
            .map_err(io)?;
        stderr
            .rewind()
            .and_then(|_| stderr.read_to_end(&mut err))
            .map_err(io)?;
        if timed_out || !status.success() {
            let failure = GitTransportFailure {
                class: trace.classify(&String::from_utf8_lossy(&err), timed_out),
                op,
                remote: remote.clone(),
                timed_out,
            };
            let text = String::from_utf8_lossy(&err).to_ascii_lowercase();
            let detail = if text.contains("already exists and is not an empty directory") {
                "destination already exists"
            } else if text.contains("no space left on device") {
                "no space left on device"
            } else if text.contains("could not create work tree") {
                "cannot create work tree"
            } else if text.contains("remote branch") && text.contains("not found") {
                "remote branch not found"
            } else if text.contains("repository not found") {
                "repository not found"
            } else {
                ""
            };
            let error = failure.into_error(detail);
            sink.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: error.to_string(),
            });
            return Err(error);
        }
        // No raw transport stderr leaves this boundary, even on success.
        Ok(Output {
            status,
            stdout: out,
            stderr: Vec::new(),
        })
    }
}

#[derive(Clone)]
pub struct TransportGuard {
    pub cancel: CancellationToken,
    pub expiry: Option<tokio::sync::watch::Receiver<DateTime<Utc>>>,
    pub deadline: Option<DateTime<Utc>>,
    pub timeout: Duration,
    pub attempt_end: Option<std::time::Instant>,
    pub inactivity: bool,
    pub start_deadline: Option<(std::time::Instant, Arc<std::sync::atomic::AtomicBool>)>,
}
impl TransportGuard {
    /// Local Git and config probes share cancellation and the attempt budget.
    /// Capture to files so a full pipe cannot prevent process termination.
    pub(super) fn output(&self, command: &mut Command) -> Result<Output> {
        self.check()?;
        let io = |e| EngineError::Git(format!("local Git process: {e}"));
        let mut stdout = tempfile::tempfile().map_err(io)?;
        let mut stderr = tempfile::tempfile().map_err(io)?;
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().map_err(io)?)
            .stderr(stderr.try_clone().map_err(io)?);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let (mut child, job) = spawn_contained(command).map_err(io)?;
        let mut job = Some(job);
        let end = std::time::Instant::now() + self.remaining_timeout();
        let status = loop {
            if self.check().is_err() || std::time::Instant::now() >= end {
                #[cfg(unix)]
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = child.kill();
                drop(job.take());
                let _ = child.wait();
                self.check()?;
                return Err(EngineError::Git("local Git operation timed out".into()));
            }
            if let Some(status) = child.try_wait().map_err(io)? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(job);
        let mut out = Vec::new();
        let mut err = Vec::new();
        stdout
            .rewind()
            .and_then(|_| stdout.read_to_end(&mut out))
            .map_err(io)?;
        stderr
            .rewind()
            .and_then(|_| stderr.read_to_end(&mut err))
            .map_err(io)?;
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            expiry: None,
            deadline: None,
            timeout: host_command::NETWORK_TIMEOUT,
            attempt_end: None,
            inactivity: true,
            start_deadline: None,
        }
    }
    pub fn remaining_timeout(&self) -> Duration {
        self.attempt_end.map_or(self.timeout, |end| {
            end.saturating_duration_since(std::time::Instant::now())
                .min(self.timeout)
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled()
            || self.start_deadline.as_ref().is_some_and(|(end, started)| {
                !started.load(std::sync::atomic::Ordering::SeqCst)
                    && std::time::Instant::now() >= *end
            })
            || self.deadline.is_some_and(|d| Utc::now() >= d)
            || self
                .expiry
                .as_ref()
                .is_some_and(|e| Utc::now() >= *e.borrow())
        {
            return Err(EngineError::Git(
                "Git operation cancelled or expired".into(),
            ));
        }
        Ok(())
    }
}
impl From<&super::remote_push::LeaseGitGuard> for TransportGuard {
    fn from(guard: &super::remote_push::LeaseGitGuard) -> Self {
        // A renewed lease's receiver supersedes its initial expiry.
        Self {
            cancel: guard.cancel.clone(),
            expiry: guard.expiry.clone(),
            deadline: guard.expiry.is_none().then_some(guard.deadline),
            timeout: Duration::from_secs(120),
            attempt_end: None,
            inactivity: false,
            start_deadline: None,
        }
    }
}

impl GitEngine {
    pub fn clone_with_transport(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
        transport: &HostGitTransport,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<()> {
        transport.check_url(url)?;
        let parent = dest
            .parent()
            .ok_or_else(|| EngineError::Git("clone destination has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        let scratch = tempfile::tempdir().map_err(|e| EngineError::Git(e.to_string()))?;
        let dest_path = std::path::absolute(dest).map_err(|e| EngineError::Git(e.to_string()))?;
        let dest = dest_path
            .to_str()
            .ok_or_else(|| EngineError::Git("clone destination is not UTF-8".into()))?;
        let mut args = vec!["clone", "--progress", "--no-recurse-submodules"];
        if let Some(branch) = branch {
            args.extend(["-b", branch]);
        }
        args.extend(["--", url, dest]);
        transport.run(
            transport.command_guarded(self.env(), scratch.path(), "origin", guard)?,
            &args,
            GitTransportOp::Clone,
            &SafeRemote::from_url(url),
            sink,
            guard,
        )?;
        Ok(())
    }
    pub fn fetch_with_transport(
        &self,
        repo: &Path,
        url: &str,
        refspecs: &[String],
        transport: &HostGitTransport,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<()> {
        transport.check_url(url)?;
        let mut args = vec!["fetch", "--progress", "--no-recurse-submodules", "--", url];
        args.extend(refspecs.iter().map(String::as_str));
        transport.run(
            transport.command_guarded(self.env(), repo, "origin", guard)?,
            &args,
            GitTransportOp::Fetch,
            &SafeRemote::from_url(url),
            sink,
            guard,
        )?;
        Ok(())
    }
    pub fn ls_remote_with_transport(
        &self,
        url: &str,
        transport: &HostGitTransport,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<BTreeMap<String, String>> {
        transport.check_url(url)?;
        let scratch = tempfile::tempdir().map_err(|e| EngineError::Git(e.to_string()))?;
        let output = transport.run(
            transport.command_guarded(self.env(), scratch.path(), "origin", guard)?,
            &["ls-remote", "--heads", "--tags", "--", url],
            GitTransportOp::LsRemote,
            &SafeRemote::from_url(url),
            sink,
            guard,
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(sha, r)| (r.to_string(), sha.to_string()))
            .collect())
    }
    pub fn fetch_missing_merge_refs(
        &self,
        repo: &Path,
        url: &str,
        refs: &[String],
        transport: &HostGitTransport,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<()> {
        let missing: Vec<_> = refs
            .iter()
            .filter(|r| self.remote_ref_sha(repo, "origin", r).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let advertised = self.ls_remote_with_transport(url, transport, sink, guard)?;
        let mut refspecs = Vec::new();
        for name in missing {
            if advertised.contains_key(&format!("refs/heads/{name}")) {
                refspecs.push(format!("+refs/heads/{name}:refs/remotes/origin/{name}"));
            } else if advertised.contains_key(&format!("refs/tags/{name}")) {
                return Err(EngineError::Git(format!(
                    "merge ref '{name}' is a tag on origin; merge_refs must name branches"
                )));
            } else {
                return Err(EngineError::Git(format!(
                    "merge ref '{name}' does not exist on origin"
                )));
            }
        }
        self.fetch_with_transport(repo, url, &refspecs, transport, sink, guard)
    }
}
