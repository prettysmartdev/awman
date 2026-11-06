use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Core,
    Squad,
    Worker,
    Engine,
}
impl Feature {
    pub const ALL: [Self; 3] = [Self::Squad, Self::Worker, Self::Engine];
    pub fn name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Squad => "squad",
            Self::Worker => "worker",
            Self::Engine => "engine",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    On,
    Off,
    Ephemeral,
    Draining,
}
#[derive(Debug, Clone)]
pub struct FeatureSet(pub std::collections::HashMap<Feature, FeatureState>);
impl FeatureSet {
    pub fn only(feature: Feature) -> Self {
        Self([(feature, FeatureState::On)].into())
    }
    pub fn state(&self, feature: Feature) -> FeatureState {
        if feature == Feature::Core {
            FeatureState::On
        } else {
            self.0.get(&feature).copied().unwrap_or(FeatureState::Off)
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    CoreStatus,
    CoreConfigGet,
    CoreConfigReload,
    CoreFeaturesList,
    CoreFeaturesGet,
    CoreGrantsList,
    CoreGrantsDelete,
    WorkerWorkdirs,
    WorkerSessionsList,
    WorkerSessionsCreate,
    WorkerSessionsGet,
    WorkerSessionsDelete,
    WorkerSessionsQueue,
    WorkerCommandsList,
    WorkerCommandsCreate,
    WorkerCommandsGet,
    WorkerCommandsCancel,
    WorkerCommandsEvents,
    WorkerCommandsWorkflow,
    WorkerCommandsOutputs,
    WorkerCommandsOutput,
    WorkerRunsCreate,
    SquadTasksList,
    SquadTasksCreate,
    SquadTasksGet,
    SquadTasksUpdate,
    SquadTasksDelete,
    SquadTasksWorkflow,
    SquadRunsCreate,
    SquadRunsList,
    SquadRunsCancel,
    SquadEnvGet,
    SquadEnvPut,
    SquadEnvClear,
}
impl Operation {
    pub fn owner(self) -> Feature {
        match self {
            Self::CoreStatus => Feature::Core,
            Self::CoreConfigGet => Feature::Core,
            Self::CoreConfigReload => Feature::Core,
            Self::CoreFeaturesList => Feature::Core,
            Self::CoreFeaturesGet => Feature::Core,
            Self::CoreGrantsList => Feature::Core,
            Self::CoreGrantsDelete => Feature::Core,
            Self::WorkerWorkdirs => Feature::Worker,
            Self::WorkerSessionsList => Feature::Worker,
            Self::WorkerSessionsCreate => Feature::Worker,
            Self::WorkerSessionsGet => Feature::Worker,
            Self::WorkerSessionsDelete => Feature::Worker,
            Self::WorkerSessionsQueue => Feature::Worker,
            Self::WorkerCommandsList => Feature::Worker,
            Self::WorkerCommandsCreate => Feature::Worker,
            Self::WorkerCommandsGet => Feature::Worker,
            Self::WorkerCommandsCancel => Feature::Worker,
            Self::WorkerCommandsEvents => Feature::Worker,
            Self::WorkerCommandsWorkflow => Feature::Worker,
            Self::WorkerCommandsOutputs => Feature::Worker,
            Self::WorkerCommandsOutput => Feature::Worker,
            Self::WorkerRunsCreate => Feature::Worker,
            Self::SquadTasksList => Feature::Squad,
            Self::SquadTasksCreate => Feature::Squad,
            Self::SquadTasksGet => Feature::Squad,
            Self::SquadTasksUpdate => Feature::Squad,
            Self::SquadTasksDelete => Feature::Squad,
            Self::SquadTasksWorkflow => Feature::Squad,
            Self::SquadRunsCreate => Feature::Squad,
            Self::SquadRunsList => Feature::Squad,
            Self::SquadRunsCancel => Feature::Squad,
            Self::SquadEnvGet => Feature::Squad,
            Self::SquadEnvPut => Feature::Squad,
            Self::SquadEnvClear => Feature::Squad,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::CoreStatus => "core.status",
            Self::CoreConfigGet => "core.config.get",
            Self::CoreConfigReload => "core.config.reload",
            Self::CoreFeaturesList => "core.features.list",
            Self::CoreFeaturesGet => "core.features.get",
            Self::CoreGrantsList => "core.grants.list",
            Self::CoreGrantsDelete => "core.grants.delete",
            Self::WorkerWorkdirs => "worker.workdirs",
            Self::WorkerSessionsList => "worker.sessions.list",
            Self::WorkerSessionsCreate => "worker.sessions.create",
            Self::WorkerSessionsGet => "worker.sessions.get",
            Self::WorkerSessionsDelete => "worker.sessions.delete",
            Self::WorkerSessionsQueue => "worker.sessions.queue",
            Self::WorkerCommandsList => "worker.commands.list",
            Self::WorkerCommandsCreate => "worker.commands.create",
            Self::WorkerCommandsGet => "worker.commands.get",
            Self::WorkerCommandsCancel => "worker.commands.cancel",
            Self::WorkerCommandsEvents => "worker.commands.events",
            Self::WorkerCommandsWorkflow => "worker.commands.workflow",
            Self::WorkerCommandsOutputs => "worker.commands.outputs",
            Self::WorkerCommandsOutput => "worker.commands.output",
            Self::WorkerRunsCreate => "worker.runs.create",
            Self::SquadTasksList => "squad.tasks.list",
            Self::SquadTasksCreate => "squad.tasks.create",
            Self::SquadTasksGet => "squad.tasks.get",
            Self::SquadTasksUpdate => "squad.tasks.update",
            Self::SquadTasksDelete => "squad.tasks.delete",
            Self::SquadTasksWorkflow => "squad.tasks.workflow",
            Self::SquadRunsCreate => "squad.runs.create",
            Self::SquadRunsList => "squad.runs.list",
            Self::SquadRunsCancel => "squad.runs.cancel",
            Self::SquadEnvGet => "squad.env.get",
            Self::SquadEnvPut => "squad.env.put",
            Self::SquadEnvClear => "squad.env.clear",
        }
    }
    pub fn creates(self) -> bool {
        matches!(
            self,
            Self::WorkerSessionsCreate
                | Self::WorkerCommandsCreate
                | Self::WorkerRunsCreate
                | Self::SquadTasksCreate
                | Self::SquadRunsCreate
        )
    }
}
