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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::command_status::CommandStatus;
    use crate::data::fs::api_db::SqliteSessionStore;
    use crate::data::worker::JobClass;

    fn record(id: &str) -> CommandRecord {
        CommandRecord {
            allow_remote_input: false,
            listener: "local".into(),
            listener_name: None,
            trace_parent: None,
            id: id.into(),
            session_id: "ses_1".into(),
            subcommand: "exec prompt".into(),
            args: "[\"hi\"]".into(),
            status: CommandStatus::Queued,
            exit_code: None,
            started_at: None,
            finished_at: None,
            log_path: String::new(),
            worker_id: None,
            queued_at: None,
            result: None,
            attempt: 1,
        }
    }

    #[test]
    fn only_exec_prompt_and_workflow_mount_outputs() {
        assert!(command_mounts_outputs("exec prompt"));
        assert!(command_mounts_outputs("exec workflow"));
        assert!(!command_mounts_outputs("status"));
        assert!(!command_mounts_outputs("exec"));
    }

    #[test]
    fn a_pushed_command_becomes_a_push_job_once_and_never_relabels_a_pull_job() {
        let dir = tempfile::tempdir().unwrap();
        let sqlite = Arc::new(SqliteSessionStore::open(dir.path()).unwrap());
        let jobs = JobStore::open(sqlite).unwrap();
        let cmd = record("cmd_1");
        let spec = push_job_spec(&cmd, None, &["hi".into()], Interaction::Headless);
        assert_eq!(spec.command, ["exec", "prompt"]);
        assert!(record_push_job(&jobs, &cmd, &spec).unwrap());
        assert!(!record_push_job(&jobs, &cmd, &spec).unwrap());
        assert_eq!(jobs.command_ids(JobClass::Push).unwrap(), ["cmd_1"]);
        let job = jobs.job(&cmd, None).unwrap().unwrap();
        assert_eq!(job.class(), JobClass::Push);
        assert_eq!(
            job.state,
            crate::data::worker::JobState::from_command(CommandStatus::Queued, None)
        );

        let pulled = record("cmd_2");
        jobs.insert(
            "cmd_2",
            &JobOrigin::Pull {
                server: "team".into(),
                lease_id: "lease_1".into(),
                epoch: 1,
            },
            &spec,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert!(!record_push_job(&jobs, &pulled, &spec).unwrap());
        assert_eq!(jobs.command_ids(JobClass::Pull).unwrap(), ["cmd_2"]);
    }

    #[tokio::test]
    async fn every_queue_worker_of_a_daemon_shares_one_worker_share() {
        let executor = SharedExecutor::new(2);
        executor.set_share(crate::data::daemon_state::FeatureName::Worker, 2);
        let a = worker_share(&executor);
        let b = worker_share(&executor);
        assert!(Arc::ptr_eq(&a, &b));
        let other = SharedExecutor::new(1);
        assert!(!Arc::ptr_eq(&a, &worker_share(&other)));
        let permit = a.acquire(JobClass::Push).await;
        assert_eq!(b.snapshot().running_push, 1);
        drop(permit);
        assert_eq!(b.snapshot().running_push, 0);
    }
}
