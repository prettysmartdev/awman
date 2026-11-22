//! Repo-context change tracking for push commands
//! (`GET /v1/worker/commands/{id}/context_changes`).
//!
//! `begin` snapshots the session's repo context directory (the one
//! `RepoContextScope::resolve(None, origin)` yields: the session key's
//! directory or the inferred one) and persists it as the attempt's `Start`
//! record beside the command's logs. `finish` snapshots again on every
//! terminal path and persists the diff as `Final`. While the command runs, the
//! route diffs `Start` against the directory as it is now.
//!
//! Agents write the directory from a container outside any awman lock, so
//! two commands sharing one key race: the last writer wins on disk and each
//! command's listing shows what changed between its own start and end. A
//! client running parallel jobs on one key should mount it `:ro`.
//!
//! Tracking never fails a command: an unreadable directory records
//! `Unavailable`, which the route reports as `503 unavailable`.
use super::{scope::RepoContextScope, ContextApiError};
use crate::{
    data::{
        context::{CommandContextRecord, ContextChange, ContextSnapshot},
        fs::{
            api_db::CommandRecord,
            api_paths::ApiPaths,
            context_dirs::{validate_context_path, ContextDirResolver},
            safe_tree::{SafeTree, SafeTreeError, TreeBudget},
        },
        session::{RepoContextBinding, Session},
    },
    engine::git::GitEngine,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Mutex, OnceLock},
};

/// Directories of commands whose tracker is between `begin` and `finish`,
/// by command id: the route's live diff reads them. In-process only; after a
/// restart the command is re-queued under a new attempt.
fn live_dirs() -> &'static Mutex<HashMap<String, (u32, PathBuf)>> {
    static LIVE: OnceLock<Mutex<HashMap<String, (u32, PathBuf)>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// The directory a running command's tracker watches, if any.
pub fn live_dir(command_id: &str) -> Option<PathBuf> {
    live_dirs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(command_id)
        .map(|(_, dir)| dir.clone())
}

pub struct ContextChangeTracker {
    paths: ApiPaths,
    session_id: String,
    command_id: String,
    attempt: u32,
    /// `None` when nothing is tracked (staged lease session, or `begin`
    /// could not snapshot).
    tracked: Option<Tracked>,
}

struct Tracked {
    dir: PathBuf,
    start: ContextSnapshot,
}

impl ContextChangeTracker {
    /// Resolve the session's repo dir with `RepoContextScope::resolve(None,
    /// origin)`, snapshot it and write `CommandContextRecord::Start`. A
    /// `Staged` session is skipped (the work server receives those changes).
    pub fn begin(
        paths: &ApiPaths,
        session: &Session,
        git: &GitEngine,
        session_id: &str,
        command_id: &str,
        attempt: u32,
    ) -> Self {
        let mut tracker = Self {
            paths: paths.clone(),
            session_id: session_id.to_owned(),
            command_id: command_id.to_owned(),
            attempt,
            tracked: None,
        };
        if matches!(session.repo_context(), RepoContextBinding::Staged(_)) {
            return tracker;
        }
        let dir = match resolve_dir(session, git) {
            Ok(dir) => dir,
            Err(reason) => {
                tracker.write_unavailable(reason);
                return tracker;
            }
        };
        match snapshot(&dir, &AtomicBool::new(false)) {
            Ok(start) => {
                let record = CommandContextRecord::Start {
                    attempt,
                    snapshot: start.clone(),
                };
                if let Err(error) =
                    CommandContextRecord::write_start(paths, session_id, command_id, &record)
                {
                    tracing::warn!(%error, command_id, "Failed to persist context start snapshot");
                    return tracker;
                }
                live_dirs()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(command_id.to_owned())
                    .and_modify(|live| {
                        if live.0 <= attempt {
                            *live = (attempt, dir.clone());
                        }
                    })
                    .or_insert_with(|| (attempt, dir.clone()));
                tracker.tracked = Some(Tracked { dir, start });
            }
            Err(reason) => tracker.write_unavailable(reason),
        }
        tracker
    }

    /// Snapshot again, diff, write `CommandContextRecord::Final`. Called on
    /// every terminal path (done, error, cancelled, panic).
    pub fn finish(self) {
        let Some(tracked) = &self.tracked else {
            return;
        };
        let record = match snapshot(&tracked.dir, &AtomicBool::new(false)) {
            Ok(end) => CommandContextRecord::Final {
                attempt: self.attempt,
                changes: tracked.start.diff(&end),
            },
            Err(reason) => CommandContextRecord::Unavailable {
                attempt: self.attempt,
                reason: reason.into(),
            },
        };
        if let Err(error) = CommandContextRecord::write_final(
            &self.paths,
            &self.session_id,
            &self.command_id,
            &record,
        ) {
            tracing::warn!(%error, command_id = %self.command_id, "Failed to persist context changes");
        }
        let mut live = live_dirs().lock().unwrap_or_else(|e| e.into_inner());
        if live
            .get(&self.command_id)
            .is_some_and(|(attempt, _)| *attempt == self.attempt)
        {
            live.remove(&self.command_id);
        }
    }

    /// For the route: `Final` if present for the row's attempt; else a live
    /// diff of `Start` against `live_dir` now; else an empty list (a command
    /// that has not started, an older attempt, or a staged lease session).
    pub fn changes_for(
        paths: &ApiPaths,
        resolver: &ContextDirResolver,
        command: &CommandRecord,
        live_dir: Option<&Path>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ContextChange>, ContextApiError> {
        let (session_id, command_id) = (&command.session_id, &command.id);
        match CommandContextRecord::read_final(paths, session_id, command_id).map_err(storage)? {
            Some(CommandContextRecord::Final { attempt, changes })
                if attempt == command.attempt =>
            {
                return Ok(changes)
            }
            Some(CommandContextRecord::Unavailable { attempt, .. })
                if attempt == command.attempt =>
            {
                return Err(unavailable());
            }
            _ => {}
        }
        let start = match CommandContextRecord::read_start(paths, session_id, command_id)
            .map_err(storage)?
        {
            Some(CommandContextRecord::Start { attempt, snapshot })
                if attempt == command.attempt =>
            {
                snapshot
            }
            Some(CommandContextRecord::Unavailable { attempt, .. })
                if attempt == command.attempt =>
            {
                return Err(unavailable());
            }
            _ => return Ok(vec![]),
        };
        let Some(dir) = live_dir else {
            // Started but neither finished nor running here (the daemon
            // stopped mid-command): nothing trustworthy to compare against.
            return Ok(vec![]);
        };
        validate_context_path(resolver.awman_home(), dir).map_err(|error| {
            tracing::warn!(%error, "Tracked context directory refused");
            unavailable()
        })?;
        let now = snapshot(dir, cancelled).map_err(|_| unavailable())?;
        Ok(start.diff(&now))
    }

    fn write_unavailable(&self, reason: &str) {
        let record = CommandContextRecord::Unavailable {
            attempt: self.attempt,
            reason: reason.into(),
        };
        if let Err(error) = CommandContextRecord::write_start(
            &self.paths,
            &self.session_id,
            &self.command_id,
            &record,
        ) {
            tracing::warn!(%error, command_id = %self.command_id, "Failed to persist context start record");
        }
    }
}

/// The session's repo context directory, validated under `~/.awman/context`.
/// The error is a fixed, path-free reason.
fn resolve_dir(session: &Session, git: &GitEngine) -> Result<PathBuf, &'static str> {
    let resolver = ContextDirResolver::from_env(session.env()).map_err(|error| {
        tracing::warn!(%error, "Context dir resolver unavailable");
        "the context directory could not be resolved"
    })?;
    // `origin` only matters for the inferred directory, and reading it
    // shells out to git.
    let remote = match session.repo_context() {
        RepoContextBinding::Inferred => git.remote_effective_url(session.git_root(), "origin"),
        _ => None,
    };
    let dir = RepoContextScope::new(&resolver, session)
        .resolve(None, remote.as_deref())
        .host_dir;
    validate_context_path(resolver.awman_home(), &dir).map_err(|error| {
        tracing::warn!(%error, "Repo context directory refused");
        "the context directory is not usable"
    })?;
    Ok(dir)
}

/// Regular files only, no symlink followed; a missing directory is empty.
/// Unlike the keyed API's listing, no size cap applies: an agent may leave
/// an oversized tree and the diff must still describe it.
fn snapshot(dir: &Path, cancelled: &AtomicBool) -> Result<ContextSnapshot, &'static str> {
    match SafeTree::new(dir).list(TreeBudget::default(), cancelled) {
        Ok(files) => Ok(ContextSnapshot::from_files(files)),
        Err(SafeTreeError::Incomplete) => Err("the context directory could not be listed in time"),
        Err(error) => {
            tracing::warn!(%error, "Context snapshot failed");
            Err("the context directory could not be read")
        }
    }
}

fn unavailable() -> ContextApiError {
    ContextApiError::Unavailable("context changes are unavailable for this command".into())
}

fn storage(error: crate::data::error::DataError) -> ContextApiError {
    tracing::error!(%error, "Failed to read command context record");
    ContextApiError::Data(error)
}
