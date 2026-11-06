//! Read-only squad health projection over the unified daemon.
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::task_store::{RunStatus, Task};
use crate::engine::daemon::DaemonSupervisor;
use crate::engine::error::EngineError;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquadHealth {
    Unknown,
    NotRunning,
    Unreachable,
    Failed,
    EnvUnmet,
    Running,
    Healthy,
}
pub struct SquadSupervisor {
    process: DaemonSupervisor,
}
impl SquadSupervisor {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, EngineError> {
        Ok(Self {
            process: DaemonSupervisor::for_paths(
                crate::data::fs::DataPaths::from_env(env)?.daemon(),
            ),
        })
    }
    pub fn health(&self, tasks: Option<&[Task]>) -> SquadHealth {
        if self.process.running_pid().ok().flatten().is_none() {
            SquadHealth::NotRunning
        } else {
            Self::health_of_running(tasks)
        }
    }
    pub fn health_of_running(tasks: Option<&[Task]>) -> SquadHealth {
        let Some(tasks) = tasks else {
            return SquadHealth::Unreachable;
        };
        if tasks
            .iter()
            .any(|t| t.last_run_status == Some(RunStatus::Failed))
        {
            SquadHealth::Failed
        } else if tasks.iter().any(|t| !t.unmet_env.is_empty()) {
            SquadHealth::EnvUnmet
        } else if tasks
            .iter()
            .any(|t| t.last_run_status == Some(RunStatus::Running))
        {
            SquadHealth::Running
        } else {
            SquadHealth::Healthy
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn health_is_read_only_and_distinguishes_empty_from_unreachable() {
        assert_eq!(
            SquadSupervisor::health_of_running(None),
            SquadHealth::Unreachable
        );
        assert_eq!(
            SquadSupervisor::health_of_running(Some(&[])),
            SquadHealth::Healthy
        );
        let root = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([("AWMAN_HOME", root.path().to_str().unwrap())]);
        assert_eq!(
            SquadSupervisor::from_env(&env).unwrap().health(None),
            SquadHealth::NotRunning
        );
        assert!(!root.path().join("data/daemon").exists());
    }
}
