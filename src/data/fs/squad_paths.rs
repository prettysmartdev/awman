//! `SquadPaths` — storage root for the squad daemon.
//!
//! Same shape and env precedence as `ApiPaths::from_env`, rooted at
//! `$HOME/.awman/squad` with an `AWMAN_SQUAD_ROOT` override. Exposes a
//! `daemon()` accessor (key stem `squad_key`) and a validated, persistent
//! per-task context directory.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::path_guard::validate_under_root;

/// Subdirectory under the data home that hosts squad state.
pub const SQUAD_SUBDIR: &str = "squad";

/// Subdirectory holding per-task context directories.
const TASKS_SUBDIR: &str = "tasks";

/// Subdirectory holding per-task container image build logs.
const BUILDS_SUBDIR: &str = "builds";

/// Resolves every path under the squad storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadPaths {
    root: PathBuf,
}

impl SquadPaths {
    /// Build `SquadPaths` rooted at an explicit directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Resolve from a supplied env snapshot.
    ///
    /// Precedence: `AWMAN_SQUAD_ROOT` → `AWMAN_CONFIG_HOME/squad` →
    /// `XDG_DATA_HOME/awman/squad` → `$HOME/.awman/squad`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        if let Some(root) = env.squad_root() {
            return Ok(Self::from_root(root));
        }
        if let Some(home) = env.config_home() {
            return Ok(Self::from_root(home.join(SQUAD_SUBDIR)));
        }
        if let Some(xdg) = env.xdg_data_home() {
            return Ok(Self::from_root(xdg.join("awman").join(SQUAD_SUBDIR)));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::from_root(home.join(".awman").join(SQUAD_SUBDIR)))
    }

    /// The squad root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Daemon-identity paths for the squad daemon (key stem `squad_key`).
    pub fn daemon(&self) -> DaemonPaths {
        DaemonPaths::new(self.root.clone(), "squad_key")
    }

    /// Directory holding per-task context directories.
    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join(TASKS_SUBDIR)
    }

    /// The persistent workspace directory for one task:
    /// `<root>/tasks/<name>/workspace/`. The user-influenced `<name>` component
    /// is validated to stay under the tasks root (a crafted `name` cannot
    /// escape via `..`); the fixed `workspace` leaf is appended afterwards.
    ///
    /// These directories are `context(global)`-style — created once per
    /// task and never recreated per run.
    pub fn task_dir(&self, name: &str) -> Result<PathBuf, DataError> {
        let base = self.tasks_dir().join(name);
        validate_under_root(
            &self.tasks_dir(),
            &base,
            "task directory must reside under the squad tasks root",
        )?;
        Ok(base.join("workspace"))
    }

    /// The task-scoped config file for one task:
    /// `<root>/tasks/<name>/config.json`. A sibling of the task's `workspace/`
    /// directory, guarded exactly as [`task_dir`] is, and carrying the same
    /// document shape as `~/.awman/config.json` and `GITROOT/.awman/config.json`
    /// — of which only the `squad` block means anything for a task (WI 0110).
    ///
    /// The file is optional: a task without one inherits the global `squad`
    /// block unchanged.
    ///
    /// [`task_dir`]: SquadPaths::task_dir
    pub fn task_config_file(&self, name: &str) -> Result<PathBuf, DataError> {
        let base = self.tasks_dir().join(name);
        validate_under_root(
            &self.tasks_dir(),
            &base,
            "task directory must reside under the squad tasks root",
        )?;
        Ok(base.join(crate::data::config::global::GLOBAL_CONFIG_FILENAME))
    }

    /// Directory holding per-task container image build logs.
    pub fn builds_dir(&self) -> PathBuf {
        self.root.join(BUILDS_SUBDIR)
    }

    /// The image build-log directory for one task:
    /// `<root>/builds/<name>/`. Validated the same way as [`task_dir`]
    /// (a crafted `name` cannot escape via `..`).
    ///
    /// [`task_dir`]: SquadPaths::task_dir
    pub fn task_builds_dir(&self, name: &str) -> Result<PathBuf, DataError> {
        let base = self.builds_dir().join(name);
        validate_under_root(
            &self.builds_dir(),
            &base,
            "build-log directory must reside under the squad builds root",
        )?;
        Ok(base)
    }

    /// Create (idempotently) the file a task's image build streams into, and
    /// return it with its path.
    ///
    /// `seq` distinguishes the images built within one run. Layer 0 owns the
    /// name and the create, so the evaluator no longer composes a filename
    /// and calls `File::create` itself (F-47).
    pub fn build_log(
        &self,
        task: &str,
        run_id: &str,
        seq: u32,
    ) -> Result<(PathBuf, std::fs::File), DataError> {
        let dir = self.task_builds_dir(task)?;
        std::fs::create_dir_all(&dir).map_err(|error| DataError::io(&dir, error))?;
        let path = dir.join(format!("{run_id}-{seq}.log"));
        let file = std::fs::File::create(&path).map_err(|error| DataError::io(&path, error))?;
        Ok((path, file))
    }

    /// Open the daemon's log file for reading. `Ok(None)` when it does not
    /// exist yet — a daemon that has not started, or has not logged.
    pub fn open_daemon_log(&self) -> Result<Option<std::fs::File>, DataError> {
        let path = self.daemon().log_file();
        match std::fs::File::open(&path) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DataError::io(&path, error)),
        }
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }
}

// ─── Run logs ────────────────────────────────────────────────────────────────

/// Something that stopped a run-log file from being opened.
///
/// The two arms are distinguished because they mean different things to an
/// operator: a refused name is a runtime handing us something we will not
/// treat as a filename; an I/O failure is the disk.
#[derive(Debug, thiserror::Error)]
pub enum SquadRunLogError {
    #[error("refused unsafe run-log filename {name:?}")]
    UnsafeName { name: String },

    #[error(transparent)]
    Io(#[from] DataError),
}

/// The on-disk layout of one squad run's logs.
///
/// Mirrors [`CommandLogWriter`]: every filename, header line and traversal
/// guard for a run directory lives here, so no higher layer opens a run log
/// itself. The directory is created by the scheduler before evaluation is
/// dispatched; this type only writes inside it.
///
/// [`CommandLogWriter`]: crate::data::fs::api_command_log::CommandLogWriter
#[derive(Debug, Clone)]
pub struct SquadRunLogs {
    dir: PathBuf,
}

impl SquadRunLogs {
    /// Bind to a run directory. Nothing is created here.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The run directory these logs are written into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the run directory the scheduler was supposed to create exists.
    pub fn is_prepared(&self) -> bool {
        self.dir.is_dir()
    }

    /// The `<step>` part of a phase-step log filename: the step description
    /// lower-cased, runs of anything outside `[a-z0-9]` collapsed to one `-`,
    /// trimmed, capped at 40 characters. Empty when nothing survives.
    pub fn step_log_slug(description: &str) -> String {
        let mut slug = String::new();
        let mut pending_dash = false;
        for ch in description.chars().flat_map(char::to_lowercase) {
            if ch.is_ascii_alphanumeric() {
                if pending_dash && !slug.is_empty() {
                    slug.push('-');
                }
                pending_dash = false;
                slug.push(ch);
            } else {
                pending_dash = true;
            }
            if slug.len() >= 40 {
                break;
            }
        }
        slug.truncate(40);
        while slug.ends_with('-') {
            slug.pop();
        }
        slug
    }

    /// `setup-3-clone-repo.log`, or `setup-3.log` when the slug is empty.
    pub fn step_log_file_name(phase: &str, index: usize, description: &str) -> String {
        let slug = Self::step_log_slug(description);
        if slug.is_empty() {
            format!("{phase}-{index}.log")
        } else {
            format!("{phase}-{index}-{slug}.log")
        }
    }

    /// Open (creating, appending) the log for a setup/teardown step and write
    /// its two header lines.
    pub fn open_step_log(
        &self,
        phase: &str,
        index: usize,
        description: &str,
    ) -> Result<SquadRunLog, SquadRunLogError> {
        let path = self
            .dir
            .join(Self::step_log_file_name(phase, index, description));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| DataError::io(&path, error))?;
        let _ = writeln!(
            file,
            "# {phase} step {index}: {description}\n# started {}",
            chrono::Utc::now().to_rfc3339()
        );
        let _ = file.flush();
        Ok(SquadRunLog { path, file })
    }

    /// Open (creating, appending) the per-container log `<name>.log`.
    ///
    /// squad's own container names come from a validated slug helper. A name
    /// that is not a single path component is refused rather than allowed to
    /// traverse out of the run directory, because the name reaches us from a
    /// container backend.
    pub fn open_container_log(
        &self,
        container_name: &str,
    ) -> Result<SharedSquadRunLog, SquadRunLogError> {
        if Path::new(container_name)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(container_name)
        {
            return Err(SquadRunLogError::UnsafeName {
                name: container_name.to_string(),
            });
        }
        let path = self.dir.join(format!("{container_name}.log"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| DataError::io(&path, error))?;
        Ok(SharedSquadRunLog {
            path,
            file: Arc::new(Mutex::new(file)),
        })
    }
}

/// One exclusively-owned open run-log file.
///
/// Every write flushes: a daemon crash can still lose bytes in the OS page
/// cache, but nothing is lost to an application-level buffer.
#[derive(Debug)]
pub struct SquadRunLog {
    path: PathBuf,
    file: File,
}

impl SquadRunLog {
    /// Where this log is on disk, for the caller's own log lines.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. Best-effort, matching the run logger's contract that
    /// a step runs unlogged rather than not at all.
    pub fn write_line(&mut self, line: &str) {
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
    }

    /// Flush and close, returning the path that was written.
    pub fn finish(mut self) -> PathBuf {
        let _ = self.file.flush();
        self.path
    }
}

/// A run-log file shared with the tasks draining a container's output.
#[derive(Debug, Clone)]
pub struct SharedSquadRunLog {
    path: PathBuf,
    file: Arc<Mutex<File>>,
}

impl SharedSquadRunLog {
    /// Where this log is on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append raw bytes as they arrive from a container stream. Best-effort
    /// and flushed per chunk, so a reader tailing the file sees output live.
    pub fn write_bytes(&self, bytes: &[u8]) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(bytes);
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT, XDG_DATA_HOME};

    #[test]
    fn squad_root_override_wins() {
        let env = EnvSnapshot::with_overrides([
            (AWMAN_SQUAD_ROOT, "/custom/squad"),
            (XDG_DATA_HOME, "/xdg/data"),
        ]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/custom/squad"));
    }

    #[test]
    fn config_home_produces_squad_subdir() {
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/cfg")]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/cfg/squad"));
    }

    #[test]
    fn xdg_data_home_produces_awman_squad() {
        let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
        let paths = SquadPaths::from_env(&env).unwrap();
        assert_eq!(paths.root(), Path::new("/xdg/data/awman/squad"));
    }

    #[test]
    fn daemon_uses_squad_key_stem() {
        let paths = SquadPaths::from_root("/r");
        assert_eq!(paths.daemon().key_stem(), "squad_key");
        assert_eq!(
            paths.daemon().key_hash_file(),
            PathBuf::from("/r/squad_key.hash")
        );
    }

    #[test]
    fn task_dir_is_under_tasks_root() {
        let paths = SquadPaths::from_root("/r");
        assert_eq!(
            paths.task_dir("issue-triage").unwrap(),
            PathBuf::from("/r/tasks/issue-triage/workspace")
        );
    }

    #[test]
    fn task_config_file_sits_beside_the_workspace() {
        let paths = SquadPaths::from_root("/r");
        assert_eq!(
            paths.task_config_file("issue-triage").unwrap(),
            PathBuf::from("/r/tasks/issue-triage/config.json")
        );
    }

    #[test]
    fn task_config_file_rejects_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SquadPaths::from_root(tmp.path());
        std::fs::create_dir_all(paths.tasks_dir()).unwrap();
        std::fs::create_dir_all(tmp.path().join("escape")).unwrap();
        assert!(paths.task_config_file("../escape").is_err());
    }

    #[test]
    fn task_dir_rejects_escape() {
        // `..` is resolved by canonicalization only when the target exists, so
        // materialize the escape target (matching `validate_context_path`).
        let tmp = tempfile::tempdir().unwrap();
        let paths = SquadPaths::from_root(tmp.path());
        std::fs::create_dir_all(paths.tasks_dir()).unwrap();
        std::fs::create_dir_all(tmp.path().join("escape")).unwrap();
        assert!(paths.task_dir("../escape").is_err());
    }
}

#[cfg(test)]
mod run_log_tests {
    use super::*;

    #[test]
    fn step_log_slugs_are_safe_lower_case_and_capped() {
        assert_eq!(SquadRunLogs::step_log_slug("clone_repo"), "clone-repo");
        assert_eq!(
            SquadRunLogs::step_log_slug("Run shell: git   status && ls"),
            "run-shell-git-status-ls"
        );
        assert_eq!(SquadRunLogs::step_log_slug("///"), "");
        assert_eq!(SquadRunLogs::step_log_slug("-a-"), "a");
        let long = SquadRunLogs::step_log_slug(&"x".repeat(100));
        assert_eq!(long.len(), 40);
        assert_eq!(
            SquadRunLogs::step_log_file_name("setup", 2, "clone_repo"),
            "setup-2-clone-repo.log"
        );
        assert_eq!(
            SquadRunLogs::step_log_file_name("teardown", 1, "///"),
            "teardown-1.log"
        );
    }

    #[test]
    fn a_step_log_is_created_with_its_header_and_appends_on_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = SquadRunLogs::new(tmp.path());
        assert!(logs.is_prepared());

        let mut log = logs.open_step_log("setup", 1, "clone repo").unwrap();
        assert_eq!(log.path().file_name().unwrap(), "setup-1-clone-repo.log");
        log.write_line("line one");
        let path = log.finish();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("# setup step 1: clone repo\n# started "));
        assert!(contents.ends_with("line one\n"), "{contents}");

        // Reopening appends rather than truncating, so a remediation attempt
        // reads in order after the original output.
        let mut again = logs.open_step_log("setup", 1, "clone repo").unwrap();
        again.write_line("line two");
        drop(again);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("line one\n"), "{contents}");
        assert!(contents.ends_with("line two\n"), "{contents}");
    }

    #[test]
    fn a_container_log_is_named_after_the_container_and_refuses_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = SquadRunLogs::new(tmp.path());

        let log = logs.open_container_log("awman-squad-triage").unwrap();
        assert_eq!(log.path(), tmp.path().join("awman-squad-triage.log"));
        log.write_bytes(b"hello\n");
        assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "hello\n");

        // A backend-supplied name that is not a single path component is a
        // refusal, never a write outside the run directory.
        for name in ["../escape", "a/b", "..", ""] {
            assert!(
                matches!(
                    logs.open_container_log(name),
                    Err(SquadRunLogError::UnsafeName { .. })
                ),
                "{name:?} must be refused"
            );
        }
        assert!(!tmp.path().parent().unwrap().join("escape.log").exists());
    }

    #[test]
    fn an_unprepared_run_directory_is_reported_rather_than_created() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = SquadRunLogs::new(tmp.path().join("never-created"));
        assert!(!logs.is_prepared());
        assert!(logs.open_step_log("setup", 1, "x").is_err());
        assert!(!tmp.path().join("never-created").exists());
    }
}
