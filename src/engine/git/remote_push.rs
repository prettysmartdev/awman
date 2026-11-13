//! Host push-back never runs Git against agent-controlled configuration or index.
//! Only inert objects/refs are imported into a private Git directory. Work-tree
//! operations use no system/global config; transport alone uses host credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::transport::{CredentialSource, HostGitTransport, TransportGuard};
use super::{CommitIdentity, GitCommand, GitEngine, GitFrontend};
use crate::data::git_transport::{GitTransportOp, SafeRemote};
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::error::EngineError;

type Result<T> = std::result::Result<T, EngineError>;

pub struct RemotePushGit {
    env: crate::data::config::env::EnvSnapshot,
    private: tempfile::TempDir,
    repo: PathBuf,
    original: Option<(PathBuf, PathBuf)>,
    transport: HostGitTransport,
    remote: SafeRemote,
    local_guard: Option<TransportGuard>,
}

/// Checked before and throughout every host Git operation of a lease.
#[derive(Clone)]
pub struct LeaseGitGuard {
    pub cancel: tokio_util::sync::CancellationToken,
    pub expiry: Option<tokio::sync::watch::Receiver<chrono::DateTime<chrono::Utc>>>,
    pub deadline: chrono::DateTime<chrono::Utc>,
}
impl LeaseGitGuard {
    pub fn check(&self) -> Result<()> {
        let deadline = self
            .expiry
            .as_ref()
            .map_or(self.deadline, |expiry| *expiry.borrow());
        if self.cancel.is_cancelled() || chrono::Utc::now() >= deadline {
            return Err(EngineError::Git("lease Git cancelled or expired".into()));
        }
        Ok(())
    }
}

/// Repository metadata locations captured before any workflow agent runs.
/// Later transport never rediscovers them through a writable `.git` pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGitSource {
    repo: PathBuf,
    common: PathBuf,
    admin: PathBuf,
}

fn io(error: std::io::Error) -> EngineError {
    EngineError::Git(format!("isolating push-back metadata: {error}"))
}

fn io_at(path: &Path, error: std::io::Error) -> EngineError {
    EngineError::Git(format!(
        "isolating push-back metadata at {}: {error}",
        path.display()
    ))
}

// Reject symlinks and special files, including directory symlinks. In particular,
// never import objects/info/alternates, config, hooks, attributes or split indexes.
fn copy_regular(source: &Path, dest: &Path) -> Result<()> {
    crate::data::fs::git_metadata::GitMetadataCopy::copy(source, dest).map_err(|error| {
        EngineError::Git(format!(
            "isolating push-back metadata copying {} to {}: {error}",
            source.display(),
            dest.display()
        ))
    })
}

impl GitEngine {
    pub fn ensure_workflow_git_source_unchanged(&self, source: &WorkflowGitSource) -> Result<()> {
        if self.capture_workflow_git_source(&source.repo)? != *source {
            return Err(EngineError::Git(
                "workflow Git metadata layout changed after agent execution".into(),
            ));
        }
        Ok(())
    }

    pub fn capture_workflow_git_source(&self, repo: &Path) -> Result<WorkflowGitSource> {
        let common = fs::canonicalize(self.git_common_dir(repo)?).map_err(io)?;
        let output = super::host_command::command(self.env(), repo)
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(repo)
            .output()
            .map_err(io)?;
        if !output.status.success() {
            return Err(EngineError::Git("cannot locate worktree metadata".into()));
        }
        let admin = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        Ok(WorkflowGitSource {
            repo: fs::canonicalize(repo).map_err(io)?,
            common,
            admin: fs::canonicalize(admin).map_err(io)?,
        })
    }

    /// Snapshot inert metadata using the layout pinned before agents ran.
    pub fn isolate_workflow_transport(
        &self,
        source: &WorkflowGitSource,
        url: &str,
    ) -> Result<RemotePushGit> {
        let mut isolated = self.isolate_transport(&source.repo, &source.common, url)?;
        // A linked worktree has a distinct HEAD and index.
        copy_regular(
            &source.admin.join("HEAD"),
            &isolated.private.path().join("HEAD"),
        )?;
        if source.admin.join("index").exists() {
            copy_regular(
                &source.admin.join("index"),
                &isolated.private.path().join("index"),
            )?;
        }
        isolated.original = Some((source.common.clone(), source.admin.clone()));
        Ok(isolated)
    }

    pub fn isolate_remote_push(&self, repo: &Path, url: &str) -> Result<RemotePushGit> {
        let metadata = repo.join(".git");
        if !fs::symlink_metadata(&metadata).map_err(io)?.is_dir() {
            return Err(EngineError::Git(
                "remote clone must have a real .git directory".into(),
            ));
        }
        self.isolate_transport(repo, &metadata, url)
    }

    fn isolate_transport(&self, repo: &Path, metadata: &Path, url: &str) -> Result<RemotePushGit> {
        self.isolate_transport_with(
            repo,
            metadata,
            url,
            HostGitTransport::user(CredentialSource::Ambient).allow_local(),
        )
    }

    pub fn isolate_remote_push_with_transport(
        &self,
        repo: &Path,
        url: &str,
        transport: HostGitTransport,
    ) -> Result<RemotePushGit> {
        let metadata = repo.join(".git");
        if !fs::symlink_metadata(&metadata).map_err(io)?.is_dir() {
            return Err(EngineError::Git(
                "remote clone must have a real .git directory".into(),
            ));
        }
        self.isolate_transport_with(repo, &metadata, url, transport)
    }

    pub fn isolate_remote_push_guarded(
        &self,
        repo: &Path,
        url: &str,
        transport: HostGitTransport,
        guard: TransportGuard,
    ) -> Result<RemotePushGit> {
        let metadata = repo.join(".git");
        if !fs::symlink_metadata(&metadata).map_err(io)?.is_dir() {
            return Err(EngineError::Git(
                "remote clone must have a real .git directory".into(),
            ));
        }
        self.isolate_transport_with_guard(repo, &metadata, url, transport, Some(guard))
    }

    fn isolate_transport_with(
        &self,
        repo: &Path,
        metadata: &Path,
        url: &str,
        transport: HostGitTransport,
    ) -> Result<RemotePushGit> {
        self.isolate_transport_with_guard(repo, metadata, url, transport, None)
    }
    fn isolate_transport_with_guard(
        &self,
        repo: &Path,
        metadata: &Path,
        url: &str,
        transport: HostGitTransport,
        local_guard: Option<TransportGuard>,
    ) -> Result<RemotePushGit> {
        if let Some(guard) = &local_guard {
            guard.check()?;
        }
        transport.check_url(url)?;
        let repo = fs::canonicalize(repo).map_err(io)?;
        // A sibling of the clone is outside its bind mount even when the
        // server's TMPDIR was configured inside the repository.
        let parent = repo
            .parent()
            .ok_or_else(|| EngineError::Git("clone has no parent directory".into()))?;
        let private = tempfile::Builder::new()
            .prefix("awman-push-")
            .tempdir_in(parent)
            .map_err(|error| io_at(parent, error))?;
        let git = RemotePushGit {
            env: self.env().clone(),
            private,
            repo: repo.to_path_buf(),
            original: None,
            transport,
            remote: SafeRemote::from_url(url),
            local_guard,
        };
        let dir = git.private.path();
        fs::create_dir_all(dir.join("objects"))
            .map_err(|error| io_at(&dir.join("objects"), error))?;
        fs::create_dir_all(dir.join("refs")).map_err(|error| io_at(&dir.join("refs"), error))?;
        copy_regular(&metadata.join("HEAD"), &dir.join("HEAD"))?;
        for name in ["refs", "packed-refs", "shallow"] {
            let source = metadata.join(name);
            if source.try_exists().map_err(|error| io_at(&source, error))? {
                copy_regular(&source, &dir.join(name))?;
            }
        }
        if !fs::symlink_metadata(metadata.join("objects"))
            .map_err(|error| io_at(&metadata.join("objects"), error))?
            .is_dir()
        {
            return Err(EngineError::Git("unsafe Git objects directory".into()));
        }
        for entry in fs::read_dir(metadata.join("objects"))
            .map_err(|error| io_at(&metadata.join("objects"), error))?
        {
            let entry = entry.map_err(|error| io_at(&metadata.join("objects"), error))?;
            if entry.file_name() != "info" {
                copy_regular(&entry.path(), &dir.join("objects").join(entry.file_name()))?;
            }
        }
        // Config is authored here, never copied from the writable clone.
        fs::write(
            dir.join("config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .map_err(|error| io_at(&dir.join("config"), error))?;
        let mut sink = crate::data::message::RecordingMessageSink::new();
        git.run(&["config", "remote.origin.url", url], false, &mut sink)?;
        git.run(
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
            false,
            &mut sink,
        )?;
        // Preserve only identity and object-format values from local config, without includes.
        let local = dir.join("source-config");
        // Git must parse a private snapshot, never reopen an agent-swappable
        // pathname after a separate symlink check.
        copy_regular(&metadata.join("config"), &local)?;
        {
            for key in ["user.name", "user.email", "extensions.objectformat"] {
                let mut probe = crate::engine::host_cli::command(&self.env.for_lease_git(), "git");
                probe
                    .args(["config", "--no-includes", "--file"])
                    // Keep Git's path parser away from Windows verbatim paths.
                    .arg("source-config")
                    .args(["--get", key])
                    .current_dir(dir);
                let output = git
                    .local_guard
                    .clone()
                    .unwrap_or_else(|| {
                        TransportGuard::new(tokio_util::sync::CancellationToken::new())
                    })
                    .output(&mut probe)?;
                if output.status.success() {
                    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if key == "extensions.objectformat" {
                        // Object format is inert metadata needed to interpret
                        // SHA-256 repositories, not an executable setting.
                        if value != "sha1" && value != "sha256" {
                            return Err(EngineError::Git("unsupported Git object format".into()));
                        }
                        git.run(
                            &["config", "core.repositoryformatversion", "1"],
                            false,
                            &mut sink,
                        )?;
                    }
                    git.run(&["config", key, &value], false, &mut sink)?;
                } else if output.status.code() != Some(1) {
                    // Exit 1 means the key is absent. A read/parse error must
                    // not silently discard identity or the object format.
                    return Err(EngineError::Git(format!(
                        "read isolated Git config: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
            }
        }
        fs::remove_file(&local).map_err(|error| io_at(&local, error))?;
        Ok(git)
    }
}

impl RemotePushGit {
    /// Use the lease transport for this push-back's `ls-remote` and push.
    pub fn with_lease_transport(
        mut self,
        transport: super::lease_transport::LeaseTransport,
    ) -> Self {
        self.env = self.env.for_lease_git();
        self.transport = transport.host_transport();
        self
    }

    pub fn metadata_dir(&self) -> &Path {
        self.private.path()
    }

    pub fn refresh_workflow_metadata(&self) -> Result<()> {
        let Some((common, admin)) = &self.original else {
            return Ok(());
        };
        for name in ["objects", "refs", "packed-refs", "shallow"] {
            let source = common.join(name);
            if name == "objects" {
                for entry in fs::read_dir(&source).map_err(io)? {
                    let entry = entry.map_err(io)?;
                    if entry.file_name() != "info" {
                        copy_regular(
                            &entry.path(),
                            &self.private.path().join("objects").join(entry.file_name()),
                        )?;
                    }
                }
            } else if source.exists() {
                copy_regular(&source, &self.private.path().join(name))?;
            }
        }
        for name in ["HEAD", "index"] {
            let source = admin.join(name);
            if source.exists() {
                copy_regular(&source, &self.private.path().join(name))?;
            }
        }
        Ok(())
    }

    /// Publish only inert Git state after an isolated setup fetch/pull.
    pub fn sync_workflow_metadata(&self) -> Result<()> {
        let Some((common, admin)) = &self.original else {
            return Ok(());
        };
        for name in ["objects", "refs", "packed-refs", "shallow"] {
            let source = self.private.path().join(name);
            if source.exists() {
                if name == "objects" {
                    copy_new_objects(&source, &common.join(name))?;
                } else {
                    copy_regular(&source, &common.join(name))?;
                }
            }
        }
        for name in ["HEAD", "index"] {
            let source = self.private.path().join(name);
            if source.exists() {
                copy_regular(&source, &admin.join(name))?;
            }
        }
        Ok(())
    }
    fn run(&self, args: &[&str], transport: bool, sink: &mut dyn GitFrontend) -> Result<Output> {
        self.run_guarded(args, transport, sink, None)
    }

    /// The Git command (before subcommand arguments) for one operation on
    /// the private metadata. Every transport call (`ls-remote`, `push`) is
    /// built by [`HostGitTransport::command`], exactly like clone and
    /// setup fetch for that session or lease; everything else uses the
    /// local hardening below. Public so its isolation (environment, pinned
    /// settings, helper) can be inspected without running it.
    pub fn command(&self, transport: bool) -> Result<Command> {
        self.command_guarded(
            transport,
            &self
                .local_guard
                .clone()
                .unwrap_or_else(|| TransportGuard::new(tokio_util::sync::CancellationToken::new())),
        )
    }
    fn command_guarded(&self, transport: bool, guard: &TransportGuard) -> Result<Command> {
        const PUSH_SETTINGS: [&str; 4] = [
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
        ];
        let mut command = if transport {
            self.transport
                .command_guarded(&self.env, self.private.path(), "origin", guard)?
        } else {
            super::host_command::command_guarded(
                &self.env.for_lease_git(),
                self.private.path(),
                guard,
            )?
        };
        command.args(PUSH_SETTINGS);
        command
            .current_dir(self.private.path())
            // Git parses these paths itself; Rust's Windows canonical paths
            // use the verbatim prefix, which Git's repository discovery does
            // not accept here. Both locations are siblings, so relative paths
            // avoid translating or weakening the pinned filesystem paths.
            .env("GIT_DIR", ".")
            .env(
                "GIT_WORK_TREE",
                Path::new("..").join(self.repo.file_name().expect("clone has a parent")),
            )
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
        if !transport {
            command
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
        Ok(command)
    }
    fn run_guarded(
        &self,
        args: &[&str],
        transport: bool,
        sink: &mut dyn GitFrontend,
        guard: Option<&LeaseGitGuard>,
    ) -> Result<Output> {
        if let Some(guard) = guard {
            guard.check()?;
        }
        if transport {
            let mut transport_guard = guard
                .map(TransportGuard::from)
                .unwrap_or_else(|| TransportGuard::new(tokio_util::sync::CancellationToken::new()));
            transport_guard.timeout = super::push_retry::PUSH_FIRST_ATTEMPT_TIMEOUT;
            return self.run_transport(args, sink, &transport_guard);
        }
        let display = GitCommand::new(args);
        sink.command_started(&display);
        let mut command = self.command(false)?;
        command.args(args);
        let output = if let Some(local) = &self.local_guard {
            local.output(&mut command)?
        } else {
            bounded_output_guarded(&mut command, std::time::Duration::from_secs(120), guard)?
        };
        if let Some(guard) = guard {
            guard.check()?;
        }
        for line in String::from_utf8_lossy(&output.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&output.stderr).lines())
        {
            if !line.trim().is_empty() {
                sink.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line.into(),
                });
            }
        }
        if !output.status.success() {
            return Err(EngineError::Git(format!(
                "{display} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output)
    }

    /// One `ls-remote` or `push` through the shared transport runner.
    fn run_transport(
        &self,
        args: &[&str],
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<Output> {
        let op = match args.first().copied() {
            Some("ls-remote") => GitTransportOp::LsRemote,
            Some("push") => GitTransportOp::Push,
            _ => {
                return Err(EngineError::Git(
                    "unsupported push transport operation".into(),
                ))
            }
        };
        self.transport.run(
            self.command_guarded(true, guard)?,
            args,
            op,
            &self.remote,
            sink,
            guard,
        )
    }

    /// The remote identifier this push-back publishes to (never a credential).
    pub fn remote(&self) -> &SafeRemote {
        &self.remote
    }

    /// Head check: where `refs/heads/<branch>` points on the remote now,
    /// bounded by `guard` (cancellation, deadline/expiry, process timeout).
    pub fn remote_head(
        &self,
        branch: &str,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<Option<String>> {
        let name = format!("refs/heads/{branch}");
        let output = self.run_transport(&["ls-remote", "--refs", "origin", &name], sink, guard)?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| {
                let (sha, reference) = line.split_once('\t')?;
                (reference == name).then(|| sha.to_string())
            }))
    }

    /// One push of `HEAD` to `refs/heads/<branch>`: never forced, never
    /// repeated by this method. Retry and the head check between attempts
    /// belong to [`super::push_publish::PushPublisher`].
    pub fn push_head(
        &self,
        branch: &str,
        sink: &mut dyn GitFrontend,
        guard: &TransportGuard,
    ) -> Result<()> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        self.run_transport(&["push", "origin", &refspec], sink, guard)?;
        guard.check()
    }

    /// Mirror a publication of `sha` to `branch` into the clone: its
    /// upstream configuration and remote-tracking ref. Used after a push
    /// and after a head check found the push had already landed (its
    /// response was lost), so both leave the clone in the same state.
    pub fn record_published(&self, branch: &str, sha: &str) -> Result<()> {
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let reference = format!("refs/remotes/origin/{branch}");
        self.run(&["update-ref", &reference, sha], false, &mut sink)?;
        self.mirror_upstream(branch)
    }

    fn text(&self, args: &[&str], transport: bool, sink: &mut dyn GitFrontend) -> Result<String> {
        Ok(
            String::from_utf8_lossy(&self.run(args, transport, sink)?.stdout)
                .trim()
                .into(),
        )
    }

    pub fn head(&self, sink: &mut dyn GitFrontend) -> Result<String> {
        self.text(&["rev-parse", "--verify", "HEAD^{commit}"], false, sink)
    }
    pub fn head_guarded(&self, guard: &TransportGuard) -> Result<String> {
        let mut command = self.command_guarded(false, guard)?;
        command.args(["rev-parse", "--verify", "HEAD^{commit}"]);
        let output = guard.output(&mut command)?;
        if !output.status.success() {
            return Err(EngineError::Git("cannot resolve intended HEAD".into()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    }

    pub fn origin_sha(&self, branch: &str, sink: &mut dyn GitFrontend) -> Result<Option<String>> {
        let name = format!("refs/heads/{branch}");
        let output = self.text(&["ls-remote", "--refs", "origin", &name], true, sink)?;
        Ok(output.lines().find_map(|line| {
            let (sha, reference) = line.split_once('\t')?;
            (reference == name).then(|| sha.to_string())
        }))
    }

    pub fn origin_sha_guarded(
        &self,
        branch: &str,
        sink: &mut dyn GitFrontend,
        guard: &LeaseGitGuard,
    ) -> Result<Option<String>> {
        let name = format!("refs/heads/{branch}");
        let output = self.run_guarded(
            &["ls-remote", "--refs", "origin", &name],
            true,
            sink,
            Some(guard),
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| {
                let (sha, reference) = line.split_once('\t')?;
                (reference == name).then(|| sha.to_string())
            }))
    }
    pub fn push_guarded(
        &self,
        branch: &str,
        sink: &mut dyn GitFrontend,
        guard: &LeaseGitGuard,
    ) -> Result<String> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        self.run_guarded(&["push", "origin", &refspec], true, sink, Some(guard))?;
        guard.check()?;
        self.head(sink)
    }

    pub fn has_unpublished_commits(&self, sink: &mut dyn GitFrontend) -> Result<bool> {
        Ok(self.text(
            &[
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                "--remotes=origin",
                "--tags",
            ],
            false,
            sink,
        )? != "0")
    }

    pub fn commit_leftovers(&self, message: &str, sink: &mut dyn GitFrontend) -> Result<()> {
        self.commit_leftovers_impl(message, sink, None)
    }
    pub fn commit_leftovers_guarded(
        &self,
        message: &str,
        sink: &mut dyn GitFrontend,
        guard: &LeaseGitGuard,
    ) -> Result<()> {
        self.commit_leftovers_impl(message, sink, Some(guard))
    }
    fn text_guarded(
        &self,
        args: &[&str],
        transport: bool,
        sink: &mut dyn GitFrontend,
        guard: Option<&LeaseGitGuard>,
    ) -> Result<String> {
        Ok(
            String::from_utf8_lossy(&self.run_guarded(args, transport, sink, guard)?.stdout)
                .trim()
                .into(),
        )
    }
    fn commit_leftovers_impl(
        &self,
        message: &str,
        sink: &mut dyn GitFrontend,
        guard: Option<&LeaseGitGuard>,
    ) -> Result<()> {
        // Use host identity when local identity is absent, but never import other
        // global config (filters, signing, etc.) into work-tree operations.
        let mut fallback_used = false;
        for key in ["user.name", "user.email"] {
            let configured = self
                .text_guarded(&["config", "--get", key], false, sink, guard)
                .ok();
            if configured.as_deref().is_none_or(str::is_empty) {
                let host = crate::engine::host_cli::command(&self.env, "git")
                    .args(["config", "--get", key])
                    .current_dir(self.private.path())
                    .env("GIT_DIR", ".")
                    .output()
                    .map_err(io)?;
                let value = String::from_utf8_lossy(&host.stdout).trim().to_string();
                fallback_used |= value.is_empty();
                let fallback = if key == "user.name" {
                    super::FALLBACK_COMMIT_NAME
                } else {
                    super::FALLBACK_COMMIT_EMAIL
                };
                self.run_guarded(
                    &[
                        "config",
                        key,
                        if value.is_empty() { fallback } else { &value },
                    ],
                    false,
                    sink,
                    guard,
                )?;
            }
        }
        let identity = CommitIdentity {
            name: self.text_guarded(&["config", "--get", "user.name"], false, sink, guard)?,
            email: self.text_guarded(&["config", "--get", "user.email"], false, sink, guard)?,
            fallback: fallback_used,
            overrides: Vec::new(),
        };
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: identity.describe(),
        });
        // Start at HEAD so staging reflects the worktree's current state.
        self.run_guarded(&["read-tree", "HEAD"], false, sink, guard)?;
        self.run_guarded(&["add", "-A", "--", "."], false, sink, guard)?;
        // --exit-code=1 means changes, so use the name list rather than swallowing
        // failures of diff as if they meant an empty index.
        let changed =
            self.text_guarded(&["diff", "--cached", "--name-only"], false, sink, guard)?;
        if !changed.is_empty() {
            self.run_guarded(&["commit", "-m", message], false, sink, guard)?;
            self.save_commit()?;
        }
        Ok(())
    }

    fn save_commit(&self) -> Result<()> {
        // Copy inert commit objects/index back even if the subsequent push fails.
        // The original metadata is never used as a Git execution context.
        let metadata = self.repo.join(".git");
        let (common, admin) = self
            .original
            .as_ref()
            .map(|(common, admin)| (common.as_path(), admin.as_path()))
            .unwrap_or((&metadata, &metadata));
        ensure_no_links(common)?;
        ensure_no_links(admin)?;
        copy_new_objects(
            &self.private.path().join("objects"),
            &common.join("objects"),
        )?;
        copy_regular(&self.private.path().join("index"), &admin.join("index"))?;
        let head = fs::read_to_string(self.private.path().join("HEAD")).map_err(io)?;
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            if !reference.starts_with("refs/heads/")
                || reference.split('/').any(|s| s == ".." || s.is_empty())
            {
                return Err(EngineError::Git("unsafe HEAD ref".into()));
            }
            copy_regular(
                &self.private.path().join(reference),
                &common.join(reference),
            )?;
        } else {
            copy_regular(&self.private.path().join("HEAD"), &admin.join("HEAD"))?;
        }
        Ok(())
    }

    pub fn push(&self, branch: &str, sink: &mut dyn GitFrontend) -> Result<String> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        self.run(&["push", "-u", "origin", &refspec], true, sink)?;
        self.mirror_upstream(branch)?;
        self.head(sink)
    }

    fn mirror_upstream(&self, branch: &str) -> Result<()> {
        // Mirror the new upstream in the clone using config-file-only operations.
        // Parsing that file does not run its hooks, helpers or includes.
        let fallback = self.repo.join(".git");
        let metadata = self
            .original
            .as_ref()
            .map(|(common, _)| common.as_path())
            .unwrap_or(&fallback);
        ensure_no_links(metadata)?;
        for (key, value) in [
            (format!("branch.{branch}.remote"), "origin".to_string()),
            (
                format!("branch.{branch}.merge"),
                format!("refs/heads/{branch}"),
            ),
        ] {
            let mut probe = crate::engine::host_cli::command(&self.env.for_lease_git(), "git");
            probe
                .args(["config", "--no-includes", "--file"])
                .arg("config")
                .args([&key, &value])
                // This is an explicit file-only config edit, never a worktree
                // or transport operation against the original repository.
                .current_dir(metadata);
            let output = self
                .local_guard
                .clone()
                .unwrap_or_else(|| TransportGuard::new(tokio_util::sync::CancellationToken::new()))
                .output(&mut probe)?;
            if !output.status.success() {
                return Err(EngineError::Git(
                    "cannot record published branch configuration".into(),
                ));
            }
        }
        let reference = format!("refs/remotes/origin/{branch}");
        let source = self.private.path().join(&reference);
        if !source.exists() {
            // An up-to-date push can leave this ref packed. Materialize the
            // resolved hash in our private metadata before the safe copy.
            let sha = self.text(
                &["rev-parse", "--verify", &reference],
                false,
                &mut crate::data::message::RecordingMessageSink::new(),
            )?;
            fs::create_dir_all(source.parent().expect("ref parent")).map_err(io)?;
            fs::write(&source, format!("{sha}\n")).map_err(io)?;
        }
        copy_regular(&source, &metadata.join(reference))
    }
}

fn ensure_no_links(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(io)?;
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(io)? {
            ensure_no_links(&entry.map_err(io)?.path())?;
        }
    } else if !meta.is_file() {
        return Err(EngineError::Git(format!(
            "unsafe Git metadata: {}",
            path.display()
        )));
    }
    Ok(())
}

// Object files are immutable and normally mode 0444. Do not overwrite objects
// already in the clone when returning the newly created commit's objects.
fn copy_new_objects(source: &Path, dest: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(source).map_err(io)?;
    if meta.is_dir() {
        fs::create_dir_all(dest).map_err(io)?;
        for entry in fs::read_dir(source).map_err(io)? {
            let entry = entry.map_err(io)?;
            copy_new_objects(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else if !dest.try_exists().map_err(io)? {
        copy_regular(source, dest)?;
    }
    Ok(())
}

/// File-backed capture avoids hanging on descendant-held stdout/stderr pipes.
fn bounded_output_guarded(
    command: &mut Command,
    timeout: std::time::Duration,
    guard: Option<&LeaseGitGuard>,
) -> Result<Output> {
    if let Some(guard) = guard {
        guard.check()?;
    }
    use std::io::{Read, Seek};
    let mut stdout = tempfile::tempfile().map_err(io)?;
    let mut stderr = tempfile::tempfile().map_err(io)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(stdout.try_clone().map_err(io)?)
        .stderr(stderr.try_clone().map_err(io)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(io)?;
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            break status;
        }
        if start.elapsed() >= timeout || guard.is_some_and(|guard| guard.check().is_err()) {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            if let Some(guard) = guard {
                guard.check()?;
            }
            return Err(EngineError::Git(
                "host push-back Git operation timed out".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
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

// Unit tier: the lease guard decision. Every behaviour that runs `git`
// (isolation, leftovers, push, cancellation of a stalled push) is in
// `tests/component/engine/git.rs`.
