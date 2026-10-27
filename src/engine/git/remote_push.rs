//! Host push-back never runs Git against agent-controlled configuration or index.
//! Only inert objects/refs are imported into a private Git directory. Work-tree
//! operations use no system/global config; transport alone uses host credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{CommitIdentity, GitCommand, GitEngine, GitFrontend, AWMAN_BOOKKEEPING_EXCLUDES};
use crate::data::config::env::{host_var_os, GIT_SSH, GIT_SSH_COMMAND};
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::error::EngineError;

type Result<T> = std::result::Result<T, EngineError>;

pub struct RemotePushGit {
    private: tempfile::TempDir,
    repo: PathBuf,
}

fn io(error: std::io::Error) -> EngineError {
    EngineError::Git(format!("isolating push-back metadata: {error}"))
}

// Reject symlinks and special files, including directory symlinks. In particular,
// never import objects/info/alternates, config, hooks, attributes or split indexes.
fn copy_regular(source: &Path, dest: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(source).map_err(io)?;
    if meta.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        fs::copy(source, dest).map_err(io)?;
    } else if meta.is_dir() {
        fs::create_dir_all(dest).map_err(io)?;
        for entry in fs::read_dir(source).map_err(io)? {
            let entry = entry.map_err(io)?;
            copy_regular(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        return Err(EngineError::Git(format!(
            "unsafe Git metadata: {}",
            source.display()
        )));
    }
    Ok(())
}

impl GitEngine {
    pub fn isolate_remote_push(&self, repo: &Path, url: &str) -> Result<RemotePushGit> {
        let metadata = repo.join(".git");
        if !fs::symlink_metadata(&metadata).map_err(io)?.is_dir() {
            return Err(EngineError::Git(
                "remote clone must have a real .git directory".into(),
            ));
        }
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
            private,
            repo: repo.to_path_buf(),
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
        let local = metadata.join("config");
        if fs::symlink_metadata(&local).map_err(io)?.is_file() {
            for key in ["user.name", "user.email", "extensions.objectformat"] {
                let output = Command::new("git")
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
        Ok(git)
    }
}

impl RemotePushGit {
    fn run(&self, args: &[&str], transport: bool, sink: &mut dyn GitFrontend) -> Result<Output> {
        let display = GitCommand::new(args);
        sink.command_started(&display);
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "push.gpgSign=false",
                "-c",
                "push.followTags=false",
                "-c",
                "push.recurseSubmodules=no",
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
            ])
            .args(args)
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
        // The host's chosen SSH executable/options remain trusted; add BatchMode
        // when no explicit override was selected, so ssh cannot prompt either.
        if transport && host_var_os(GIT_SSH_COMMAND).is_none() && host_var_os(GIT_SSH).is_none() {
            command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        let output = bounded_output(&mut command, std::time::Duration::from_secs(120))?;
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
        // Use host identity when local identity is absent, but never import other
        // global config (filters, signing, etc.) into work-tree operations.
        let mut fallback_used = false;
        for key in ["user.name", "user.email"] {
            let configured = self.text(&["config", "--get", key], false, sink).ok();
            if configured.as_deref().is_none_or(str::is_empty) {
                let host = Command::new("git")
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
                self.run(
                    &[
                        "config",
                        key,
                        if value.is_empty() { fallback } else { &value },
                    ],
                    false,
                    sink,
                )?;
            }
        }
        let identity = CommitIdentity {
            name: self.text(&["config", "--get", "user.name"], false, sink)?,
            email: self.text(&["config", "--get", "user.email"], false, sink)?,
            fallback: fallback_used,
            overrides: Vec::new(),
        };
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: identity.describe(),
        });
        // Start at HEAD: previously staged bookkeeping can never enter this index.
        self.run(&["read-tree", "HEAD"], false, sink)?;
        let mut add = vec!["add", "-A", "--", "."];
        add.extend_from_slice(AWMAN_BOOKKEEPING_EXCLUDES);
        self.run(&add, false, sink)?;
        // --exit-code=1 means changes, so use the name list rather than swallowing
        // failures of diff as if they meant an empty index.
        let changed = self.text(&["diff", "--cached", "--name-only"], false, sink)?;
        if !changed.is_empty() {
            self.run(&["commit", "-m", message], false, sink)?;
            self.save_commit()?;
        }
        Ok(())
    }

    fn save_commit(&self) -> Result<()> {
        // Copy inert commit objects/index back even if the subsequent push fails.
        // The original metadata is never used as a Git execution context.
        let metadata = self.repo.join(".git");
        ensure_no_links(&metadata)?;
        copy_new_objects(
            &self.private.path().join("objects"),
            &metadata.join("objects"),
        )?;
        copy_regular(&self.private.path().join("index"), &metadata.join("index"))?;
        let head = fs::read_to_string(self.private.path().join("HEAD")).map_err(io)?;
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            if !reference.starts_with("refs/heads/")
                || reference.split('/').any(|s| s == ".." || s.is_empty())
            {
                return Err(EngineError::Git("unsafe HEAD ref".into()));
            }
            copy_regular(
                &self.private.path().join(reference),
                &metadata.join(reference),
            )?;
        } else {
            copy_regular(&self.private.path().join("HEAD"), &metadata.join("HEAD"))?;
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
            let output = Command::new("git")
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
fn bounded_output(command: &mut Command, timeout: std::time::Duration) -> Result<Output> {
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
        if start.elapsed() >= timeout {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
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
