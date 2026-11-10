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
pub use crate::data::daemon_state::FeatureState;
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
    EngineInstancesList,
    EngineInstancesCreate,
    EngineInstancesGet,
    EngineInstancesDelete,
    EngineInstancesPatch,
    EngineInstancesFrontend,
    EngineTabsList,
    EngineTabsGet,
    EngineTabsPatch,
    EngineDialogsList,
    EngineDialogsAnswer,
    EngineUiRequests,
    CoreHealth,
    CoreFeaturesPatch,
    CoreKeysList,
    CoreKeysCreate,
    CoreKeysDelete,
    CoreStatus,
    CoreConfigGet,
    CoreConfigReload,
    CoreFeaturesList,
    CoreFeaturesGet,
    CoreGrantsList,
    CoreGrantsDelete,
    CoreTerminalsList,
    CoreTerminalsGet,
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
    WorkerServersList,
    WorkerServersGet,
    WorkerQueueList,
    WorkerQueueClaim,
    WorkerLeasesList,
    WorkerLeasesGet,
    WorkerLeasesRelease,
    WorkerLeasesEvents,
    WorkerLeasesTerminal,
    WorkerLeasesResult,
    WorkerLeasesCheckout,
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
            Self::EngineInstancesList
            | Self::EngineInstancesCreate
            | Self::EngineInstancesGet
            | Self::EngineInstancesDelete
            | Self::EngineInstancesPatch
            | Self::EngineInstancesFrontend
            | Self::EngineTabsList
            | Self::EngineTabsGet
            | Self::EngineTabsPatch
            | Self::EngineDialogsList
            | Self::EngineDialogsAnswer
            | Self::EngineUiRequests => Feature::Engine,
            Self::CoreHealth
            | Self::CoreFeaturesPatch
            | Self::CoreKeysList
            | Self::CoreKeysCreate
            | Self::CoreKeysDelete => Feature::Core,
            Self::CoreStatus => Feature::Core,
            Self::CoreConfigGet => Feature::Core,
            Self::CoreConfigReload => Feature::Core,
            Self::CoreFeaturesList => Feature::Core,
            Self::CoreFeaturesGet => Feature::Core,
            Self::CoreGrantsList => Feature::Core,
            Self::CoreGrantsDelete => Feature::Core,
            // A terminal is reached through its owner's rules, not a feature gate:
            // Layer 2 (`command::terminals::AccessPolicy`) admits each viewer.
            Self::CoreTerminalsList | Self::CoreTerminalsGet => Feature::Core,
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
            Self::WorkerServersList
            | Self::WorkerServersGet
            | Self::WorkerQueueList
            | Self::WorkerQueueClaim
            | Self::WorkerLeasesList
            | Self::WorkerLeasesGet
            | Self::WorkerLeasesRelease
            | Self::WorkerLeasesEvents
            | Self::WorkerLeasesTerminal
            | Self::WorkerLeasesResult
            | Self::WorkerLeasesCheckout => Feature::Worker,
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
            Self::EngineInstancesList => "engine.instances.list",
            Self::EngineInstancesCreate => "engine.instances.create",
            Self::EngineInstancesGet => "engine.instances.get",
            Self::EngineInstancesDelete => "engine.instances.delete",
            Self::EngineInstancesPatch => "engine.instances.patch",
            Self::EngineInstancesFrontend => "engine.instances.frontend",
            Self::EngineTabsList => "engine.tabs.list",
            Self::EngineTabsGet => "engine.tabs.get",
            Self::EngineTabsPatch => "engine.tabs.patch",
            Self::EngineDialogsList => "engine.dialogs.list",
            Self::EngineDialogsAnswer => "engine.dialogs.answer",
            Self::EngineUiRequests => "engine.ui.requests",
            Self::CoreHealth => "core.health",
            Self::CoreFeaturesPatch => "core.features.patch",
            Self::CoreKeysList => "core.keys.list",
            Self::CoreKeysCreate => "core.keys.create",
            Self::CoreKeysDelete => "core.keys.delete",
            Self::CoreStatus => "core.status",
            Self::CoreConfigGet => "core.config.get",
            Self::CoreConfigReload => "core.config.reload",
            Self::CoreFeaturesList => "core.features.list",
            Self::CoreFeaturesGet => "core.features.get",
            Self::CoreGrantsList => "core.grants.list",
            Self::CoreGrantsDelete => "core.grants.delete",
            Self::CoreTerminalsList => "core.terminals.list",
            Self::CoreTerminalsGet => "core.terminals.get",
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
            Self::WorkerServersList => "worker.servers.list",
            Self::WorkerServersGet => "worker.servers.get",
            Self::WorkerQueueList => "worker.queue.list",
            Self::WorkerQueueClaim => "worker.queue.claim",
            Self::WorkerLeasesList => "worker.leases.list",
            Self::WorkerLeasesGet => "worker.leases.get",
            Self::WorkerLeasesRelease => "worker.leases.release",
            Self::WorkerLeasesEvents => "worker.leases.events",
            Self::WorkerLeasesTerminal => "worker.leases.terminal",
            Self::WorkerLeasesResult => "worker.leases.result",
            Self::WorkerLeasesCheckout => "worker.leases.checkout",
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
            Self::EngineInstancesCreate
                | Self::EngineInstancesFrontend
                | Self::WorkerSessionsCreate
                | Self::WorkerCommandsCreate
                | Self::WorkerRunsCreate
                | Self::WorkerQueueClaim
                | Self::SquadTasksCreate
                | Self::SquadRunsCreate
        )
    }
}
impl From<crate::data::daemon_state::FeatureName> for Feature {
    fn from(f: crate::data::daemon_state::FeatureName) -> Self {
        match f {
            crate::data::daemon_state::FeatureName::Squad => Self::Squad,
            crate::data::daemon_state::FeatureName::Worker => Self::Worker,
            crate::data::daemon_state::FeatureName::Engine => Self::Engine,
        }
    }
}
impl Feature {
    pub fn feature_name(self) -> Option<crate::data::daemon_state::FeatureName> {
        crate::data::daemon_state::FeatureName::parse(self.name())
    }
}
impl Operation {
    pub fn local_only(self) -> bool {
        self.owner() == Feature::Engine
            || matches!(
                self,
                Self::CoreConfigGet
                    | Self::CoreConfigReload
                    | Self::CoreFeaturesPatch
                    | Self::CoreKeysList
                    | Self::CoreKeysCreate
                    | Self::CoreKeysDelete
                    | Self::CoreGrantsList
                    | Self::CoreGrantsDelete
            )
            || self.pull()
    }
    /// The pull routes (WI 0143). Pull is exposed on the daemon's own API
    /// only: they are local-only and 404 on the network listener.
    pub fn pull(self) -> bool {
        matches!(
            self,
            Self::WorkerServersList
                | Self::WorkerServersGet
                | Self::WorkerQueueList
                | Self::WorkerQueueClaim
                | Self::WorkerLeasesList
                | Self::WorkerLeasesGet
                | Self::WorkerLeasesRelease
                | Self::WorkerLeasesEvents
                | Self::WorkerLeasesTerminal
                | Self::WorkerLeasesResult
                | Self::WorkerLeasesCheckout
        )
    }
    pub fn unauthenticated(self) -> bool {
        self == Self::CoreHealth
    }
}
