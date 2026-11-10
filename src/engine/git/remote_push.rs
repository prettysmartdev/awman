//! Host push-back never runs Git against agent-controlled configuration or index.
//! Only inert objects/refs are imported into a private Git directory. Work-tree
//! operations use no system/global config; transport alone uses host credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{CommitIdentity, GitCommand, GitEngine, GitFrontend};
use crate::data::config::env::{GIT_SSH, GIT_SSH_COMMAND};
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::error::EngineError;

type Result<T> = std::result::Result<T, EngineError>;

pub struct RemotePushGit {
    env: crate::data::config::env::EnvSnapshot,
    private: tempfile::TempDir,
    repo: PathBuf,
    original: Option<(PathBuf, PathBuf)>,
    /// Lease transport (WI 0143): when set, transport calls use no global
    /// or system config and the lease credential helper, and failures are
    /// classified (`GitAuth` / `GitTransport` / `GitRejected`).
    lease: Option<super::lease_transport::LeaseTransport>,
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

// Reject symlinks and special files, including directory symlinks. In particular,
// never import objects/info/alternates, config, hooks, attributes or split indexes.
fn copy_regular(source: &Path, dest: &Path) -> Result<()> {
    crate::data::fs::git_metadata::GitMetadataCopy::copy(source, dest).map_err(io)
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
        // A sibling of the clone is outside its bind mount even when the
        // server's TMPDIR was configured inside the repository.
        let parent = repo
            .parent()
            .ok_or_else(|| EngineError::Git("clone has no parent directory".into()))?;
        let private = tempfile::Builder::new()
            .prefix("awman-push-")
            .tempdir_in(parent)
            .map_err(io)?;
        let git = RemotePushGit {
            env: self.env().clone(),
            private,
            repo: repo.to_path_buf(),
            original: None,
            lease: None,
        };
        let dir = git.private.path();
        fs::create_dir_all(dir.join("objects")).map_err(io)?;
        fs::create_dir_all(dir.join("refs")).map_err(io)?;
        copy_regular(&metadata.join("HEAD"), &dir.join("HEAD"))?;
        for name in ["refs", "packed-refs", "shallow"] {
            let source = metadata.join(name);
            if source.try_exists().map_err(io)? {
                copy_regular(&source, &dir.join(name))?;
            }
        }
        if !fs::symlink_metadata(metadata.join("objects"))
            .map_err(io)?
            .is_dir()
        {
            return Err(EngineError::Git("unsafe Git objects directory".into()));
        }
        for entry in fs::read_dir(metadata.join("objects")).map_err(io)? {
            let entry = entry.map_err(io)?;
            if entry.file_name() != "info" {
                copy_regular(&entry.path(), &dir.join("objects").join(entry.file_name()))?;
            }
        }
        // Config is authored here, never copied from the writable clone.
        fs::write(
            dir.join("config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .map_err(io)?;
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
                let output = crate::engine::host_cli::command(&self.env, "git")
                    .args(["config", "--no-includes", "--file"])
                    .arg(&local)
                    .args(["--get", key])
                    .current_dir(dir)
                    .output()
                    .map_err(io)?;
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
                }
            }
        }
        fs::remove_file(&local).map_err(io)?;
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
        self.lease = Some(transport);
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
    /// the private metadata. A lease transport call (`ls-remote`, `push`) is
    /// built by the shared lease builder,
    /// [`lease_transport_command`](super::lease_transport::lease_transport_command),
    /// exactly like the lease's clone and fetches; everything else uses the
    /// local hardening below.
    pub(super) fn command(&self, transport: bool) -> Result<Command> {
        const PUSH_SETTINGS: [&str; 4] = [
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
        ];
        let lease = self.lease.as_ref().filter(|_| transport);
        let mut command = match lease {
            Some(lease) => {
                let mut command = super::lease_transport::lease_transport_command(
                    &self.env,
                    self.private.path(),
                    "origin",
                    lease,
                )?;
                command.args(PUSH_SETTINGS);
                command
            }
            None => {
                let mut command = crate::engine::host_cli::command(&self.env, "git");
                command.args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "push.gpgSign=false",
                    "-c",
                    "maintenance.auto=false",
                    "-c",
                    "gc.auto=0",
                ]);
                command.args(PUSH_SETTINGS);
                command
            }
        };
        command
            .current_dir(self.private.path())
            .env("GIT_DIR", self.private.path())
            .env("GIT_WORK_TREE", &self.repo)
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
        // Push-session transport: the host's chosen SSH executable/options
        // remain trusted; add BatchMode when no explicit override was
        // selected, so ssh cannot prompt either. (The lease builder pins
        // `core.sshCommand` itself and runs with a scrubbed environment.)
        if transport
            && lease.is_none()
            && self.env.get_os(GIT_SSH_COMMAND).is_none()
            && self.env.get_os(GIT_SSH).is_none()
        {
            command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
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
        let display = GitCommand::new(args);
        sink.command_started(&display);
        let lease = self.lease.as_ref().filter(|_| transport);
        let mut command = self.command(transport)?;
        command.args(args);
        let trace = if lease.is_some() {
            Some(super::auth_classify::ResponseTrace::attach(&mut command)?)
        } else {
            None
        };
        let output =
            bounded_output_guarded(&mut command, std::time::Duration::from_secs(120), guard)?;
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
                    text: self
                        .lease
                        .as_ref()
                        .map_or_else(|| line.into(), |lease| lease.redact(line)),
                });
            }
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if lease.is_some() {
                let operation = args.first().copied().unwrap_or("git");
                return Err(trace
                    .as_ref()
                    .expect("lease trace")
                    .classify(&stderr, false)
                    .into_error(
                        operation,
                        self.lease
                            .as_ref()
                            .expect("lease transport")
                            .redact(&stderr),
                    ));
            }
            return Err(EngineError::Git(format!("{display} failed: {stderr}")));
        }
        Ok(output)
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
                    .env("GIT_DIR", self.private.path())
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
        // Mirror the new upstream in the clone using config-file-only operations.
        // Parsing that file does not run its hooks, helpers or includes.
        let metadata = self.repo.join(".git");
        ensure_no_links(&metadata)?;
        for (key, value) in [
            (format!("branch.{branch}.remote"), "origin".to_string()),
            (
                format!("branch.{branch}.merge"),
                format!("refs/heads/{branch}"),
            ),
        ] {
            let output = crate::engine::host_cli::command(&self.env, "git")
                .args(["config", "--no-includes", "--file"])
                .arg(metadata.join("config"))
                .args([&key, &value])
                .current_dir(self.private.path())
                .output()
                .map_err(io)?;
            if !output.status.success() {
                return Err(EngineError::Git(
                    String::from_utf8_lossy(&output.stderr).into(),
                ));
            }
        }
        let reference = format!("refs/remotes/origin/{branch}");
        copy_regular(
            &self.private.path().join(&reference),
            &metadata.join(reference),
        )?;
        self.head(sink)
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
#[cfg(test)]
fn bounded_output(command: &mut Command, timeout: std::time::Duration) -> Result<Output> {
    bounded_output_guarded(command, timeout, None)
}
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

#[cfg(test)]
mod tests {
    #[test]
    fn isolated_host_commit_preserves_sha256_object_format() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("clone");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q", "-b", "main", "--object-format=sha256"]);
        std::fs::write(repo.join("base"), "base").unwrap();
        run(&["add", "-A"]);
        run(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ]);
        let engine = super::GitEngine::new();
        let before = engine.head_sha(&repo).unwrap();
        assert_eq!(before.len(), 64);
        let git = engine
            .isolate_remote_push(&repo, "git://127.0.0.1:9/unused")
            .unwrap();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        assert_eq!(git.head(&mut sink).unwrap(), before);
        std::fs::write(repo.join("edit"), "edit").unwrap();
        git.commit_leftovers("leftovers", &mut sink).unwrap();
        let after = git.head(&mut sink).unwrap();
        assert_eq!(after.len(), 64);
        assert_ne!(before, after);
        assert_eq!(engine.head_sha(&repo).unwrap(), after);
    }

    #[test]
    fn host_git_timeout_kills_a_stalled_process() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let start = std::time::Instant::now();
        let error =
            super::bounded_output(&mut command, std::time::Duration::from_millis(30)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod workflow_review_tests {
    use super::*;
    #[test]
    fn linked_worktree_leftovers_update_pinned_admin_and_publish() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = root.path().join("worktree");
        let remote = root.path().join("remote.git");
        let run = |cwd: &Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run(
            root.path(),
            &["init", "-q", "-b", "main", repo.to_str().unwrap()],
        );
        run(
            &repo,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "lease",
                worktree.to_str().unwrap(),
            ],
        );
        run(
            root.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let engine = GitEngine::new();
        let source = engine.capture_workflow_git_source(&worktree).unwrap();
        let before = engine.head_sha(&repo).unwrap();
        let isolated = engine
            .isolate_workflow_transport(&source, remote.to_str().unwrap())
            .unwrap();
        fs::write(worktree.join("change.txt"), "lease edit").unwrap();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        isolated
            .commit_leftovers("lease changes", &mut sink)
            .unwrap();
        let head = isolated.head(&mut sink).unwrap();
        assert_eq!(engine.head_sha(&worktree).unwrap(), head);
        assert_eq!(engine.head_sha(&repo).unwrap(), before);
        assert!(run(&worktree, &["status", "--porcelain"]).is_empty());
        isolated
            .run(
                &["push", "origin", "HEAD:refs/heads/result"],
                false,
                &mut sink,
            )
            .unwrap();
        assert_eq!(run(&remote, &["rev-parse", "refs/heads/result"]), head);
        assert_eq!(
            run(&remote, &["show", "refs/heads/result:change.txt"]),
            "lease edit"
        );
    }

    #[test]
    fn isolated_workflow_transport_omits_agent_hooks_rewrites_and_pushurl() {
        let repo = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success());
        let git = GitEngine::new();
        let config = repo.path().join(".git/config");
        fs::write(&config, "[core]\nrepositoryformatversion=0\n[remote \"origin\"]\nurl=https://approved.test/o/r\npushurl=https://evil.test/o/r\n[url \"https://evil.test/\"]\ninsteadOf=https://approved.test/\n[core]\nhooksPath=hooks\nsshCommand=evil\n").unwrap();
        fs::write(repo.path().join(".git/hooks/pre-push"), "echo stolen").unwrap();
        let isolated = git
            .isolate_workflow_transport(
                &git.capture_workflow_git_source(repo.path()).unwrap(),
                "https://approved.test/o/r",
            )
            .unwrap();
        let config = fs::read_to_string(isolated.metadata_dir().join("config")).unwrap();
        assert!(config.contains("https://approved.test/o/r"));
        for needle in [
            "evil.test",
            "insteadOf",
            "hooksPath",
            "sshCommand",
            "pushurl",
        ] {
            assert!(!config.contains(needle));
        }
        assert!(!isolated.metadata_dir().join("hooks/pre-push").exists());
        assert!(!isolated.metadata_dir().starts_with(repo.path()));
    }

    #[test]
    fn pinned_workflow_metadata_ignores_rewritten_git_and_commondir_pointers() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let worktree = root.path().join("worktree");
        let foreign = root.path().join("foreign");
        for path in [&repo, &foreign] {
            assert!(Command::new("git")
                .args(["init", "-q"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        assert!(Command::new("git")
            .current_dir(&repo)
            .args([
                "-c",
                "user.name=Trusted",
                "-c",
                "user.email=trusted@test",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "add", "--detach", "-q"])
            .arg(&worktree)
            .status()
            .unwrap()
            .success());
        let git = GitEngine::new();
        let source = git.capture_workflow_git_source(&worktree).unwrap();
        let foreign_head = fs::read(foreign.join(".git/HEAD")).unwrap();
        fs::write(
            foreign.join(".git/config"),
            "[core]\nrepositoryformatversion=0\n[user]\nname=FOREIGN_CANARY\n",
        )
        .unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", foreign.join(".git").display()),
        )
        .unwrap();
        fs::write(
            source.admin.join("commondir"),
            format!("{}\n", foreign.join(".git").display()),
        )
        .unwrap();
        let isolated = git
            .isolate_workflow_transport(&source, "https://approved.test/o/r")
            .unwrap();
        assert!(!fs::read_to_string(isolated.metadata_dir().join("config"))
            .unwrap()
            .contains("FOREIGN_CANARY"));
        assert_eq!(
            fs::read(isolated.metadata_dir().join("HEAD")).unwrap(),
            fs::read(source.admin.join("HEAD")).unwrap()
        );
        isolated.refresh_workflow_metadata().unwrap();
        isolated.sync_workflow_metadata().unwrap();
        assert_eq!(fs::read(foreign.join(".git/HEAD")).unwrap(), foreign_head);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_workflow_metadata_refuses_linked_config() {
        let repo = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success());
        let git = GitEngine::new();
        let source = git.capture_workflow_git_source(repo.path()).unwrap();
        let foreign = tempfile::NamedTempFile::new().unwrap();
        fs::remove_file(repo.path().join(".git/config")).unwrap();
        std::os::unix::fs::symlink(foreign.path(), repo.path().join(".git/config")).unwrap();
        assert!(git
            .isolate_workflow_transport(&source, "https://approved.test/o/r")
            .is_err());
    }
}
