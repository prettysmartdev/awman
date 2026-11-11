//! Pushed commands as jobs of the worker feature's one job model (WI 0143).
//!
//! A pushed command stays an ordinary command row; this module records its
//! [`JobOrigin::Push`] and [`JobSpec`] beside the row (`JobStore`), records
//! whether `/awman/output` was mounted, and hands out the daemon's one
//! [`WorkerExecutorShare`] so pushed and pulled jobs alternate on the same
//! `daemon.features.worker.maxConcurrentJobs` share.

use std::sync::{Arc, Mutex, Weak};

use crate::data::config::daemon::Interaction;
use crate::data::error::DataError;
use crate::data::fs::api_db::{CommandRecord, SessionRecord};
use crate::data::worker::job_store::JobStore;
use crate::data::worker::{JobOrigin, JobSpec, JobWorkspace};
use crate::engine::daemon::executor::SharedExecutor;
use crate::engine::worker::executor_share::WorkerExecutorShare;

/// The catalogue commands that mount the command's `/awman/output`.
pub fn command_mounts_outputs(subcommand: &str) -> bool {
    let parts: Vec<&str> = subcommand.split_whitespace().collect();
    matches!(
        parts.as_slice(),
        ["exec", "prompt", ..] | ["exec", "workflow", ..]
    )
}

/// The scheduling description of a pushed command.
pub fn push_job_spec(
    cmd: &CommandRecord,
    session: Option<&SessionRecord>,
    args: &[String],
    interaction: Interaction,
) -> JobSpec {
    let workspace = match session {
        Some(s) if s.session_type == "remote" => JobWorkspace::Clone {
            repo_url: s.repo_url.clone().unwrap_or_default(),
            base_ref: s.base_ref.clone(),
            branch: s.branch.clone().unwrap_or_default(),
            merge_refs: s.merge_refs.clone(),
            push_branch: s.push.then(|| s.branch.clone()).flatten(),
            read_only: false,
            work_root: s
                .cloned_path
                .clone()
                .unwrap_or_else(|| s.workdir.clone())
                .into(),
        },
        Some(s) => JobWorkspace::Local {
            workdir: s.workdir.clone().into(),
        },
        None => JobWorkspace::Local {
            workdir: Default::default(),
        },
    };
    JobSpec {
        command: cmd
            .subcommand
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        args: args.to_vec(),
        interaction,
        workspace,
    }
}

/// Record `cmd` as a push-origin job unless it already has an origin (a
/// pulled lease's command row is recorded by the lease runner first and is
/// never re-labelled as pushed).
pub fn record_push_job(
    jobs: &JobStore,
    cmd: &CommandRecord,
    spec: &JobSpec,
) -> Result<bool, DataError> {
    if jobs.get(&cmd.id)?.is_some() {
        return Ok(false);
    }
    jobs.insert(
        &cmd.id,
        &JobOrigin::Push {
            session_id: cmd.session_id.clone(),
            command_id: cmd.id.clone(),
        },
        spec,
        &chrono::Utc::now().to_rfc3339(),
    )?;
    Ok(true)
}

type ShareRegistry = Mutex<Vec<(Weak<SharedExecutor>, Weak<WorkerExecutorShare>)>>;
static SHARES: ShareRegistry = Mutex::new(Vec::new());

/// The one worker share over `executor`. Every queue worker of a daemon
/// (and the pull runner) gets the same share, so push/pull alternation holds
/// across them; a share lives as long as someone holds it.
pub fn worker_share(executor: &Arc<SharedExecutor>) -> Arc<WorkerExecutorShare> {
    let mut shares = SHARES.lock().unwrap_or_else(|p| p.into_inner());
    shares.retain(|(e, s)| e.strong_count() > 0 && s.strong_count() > 0);
    if let Some(share) = shares
        .iter()
        .find(|(e, _)| e.upgrade().is_some_and(|e| Arc::ptr_eq(&e, executor)))
        .and_then(|(_, s)| s.upgrade())
    {
        return share;
    }
    let share = WorkerExecutorShare::new(executor.clone());
    shares.push((Arc::downgrade(executor), Arc::downgrade(&share)));
    share
}
