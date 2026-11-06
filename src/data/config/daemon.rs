//! `daemon` section: settings of the long-running awman daemon process(es).
//!
//! Every field is optional (absent = not set in this file). The resolved,
//! defaults-applied view is [`DaemonSettings`], built from the merged config
//! by `EffectiveConfig::daemon()`.

use serde::{Deserialize, Serialize};

use crate::data::config::units::DurationStr;
use crate::data::fs::daemon_env::EnvPersistenceSetting;

/// How a job interacts with its user: an attachable terminal session, or
/// headless (no prompts; defaults answer them).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interaction {
    #[default]
    Interactive,
    Headless,
}

impl Interaction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Headless => "headless",
        }
    }

    pub fn is_headless(self) -> bool {
        self == Self::Headless
    }
}

macro_rules! section {
    ($(#[$meta:meta])* $name:ident { $($(#[$fmeta:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name {
            $(
                $(#[$fmeta])*
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }
    };
}

section!(
    /// The `daemon` block.
    DaemonConfig {
        max_concurrent_jobs: usize,
        verbose_setup: bool,
        terminal: DaemonTerminalConfig,
        network: DaemonNetworkConfig,
        features: FeaturesConfig,
    }
);

section!(DaemonTerminalConfig {
    cols: u16,
    rows: u16,
    remote_input: bool
});

section!(DaemonNetworkConfig {
    enabled: bool,
    listen: String,
    tls_sans: Vec<String>,
    features: Vec<String>,
});

section!(FeaturesConfig {
    squad: SquadFeatureConfig,
    worker: WorkerFeatureConfig,
    engine: EngineFeatureConfig,
});

section!(SquadFeatureConfig {
    enabled: bool,
    interaction: Interaction,
    max_concurrent_jobs: usize,
    max_concurrent_evaluations: usize,
    env_persistence: EnvPersistenceSetting,
});

section!(WorkerFeatureConfig {
    enabled: bool,
    max_concurrent_jobs: usize,
    push: WorkerPushConfig,
    pull: WorkerPullConfig,
});

section!(WorkerPushConfig {
    enabled: bool,
    interaction: Interaction,
    allow_override: bool,
    work_dirs: WorkDirsConfig,
});

section!(WorkDirsConfig { allow: Vec<String>, roots: Vec<String> });

section!(WorkerPullConfig {
    enabled: bool,
    interaction: Interaction,
    allow_override: bool,
    work_root: String,
});

section!(EngineFeatureConfig {
    enabled: bool,
    allow_ephemeral: bool,
    idle_timeout: DurationStr,
    max_concurrent_jobs: usize,
});

/// Every `daemon` leaf with defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSettings {
    pub max_concurrent_jobs: usize,
    pub verbose_setup: bool,
    pub terminal: TerminalSettings,
    pub network: NetworkSettings,
    pub squad: SquadSettings,
    pub worker: WorkerSettings,
    pub engine: EngineSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSettings {
    pub cols: u16,
    pub rows: u16,
    pub remote_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSettings {
    pub enabled: bool,
    pub listen: String,
    pub tls_sans: Vec<String>,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadSettings {
    pub enabled: bool,
    pub interaction: Interaction,
    pub max_concurrent_jobs: usize,
    pub max_concurrent_evaluations: usize,
    pub env_persistence: EnvPersistenceSetting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSettings {
    pub enabled: bool,
    pub max_concurrent_jobs: usize,
    pub push_enabled: bool,
    pub push_interaction: Interaction,
    pub push_allow_override: bool,
    pub push_work_dirs: Vec<String>,
    pub push_work_dir_roots: Vec<String>,
    pub pull_enabled: bool,
    pub pull_interaction: Interaction,
    pub pull_allow_override: bool,
    pub pull_work_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSettings {
    pub enabled: bool,
    pub allow_ephemeral: bool,
    pub idle_timeout: std::time::Duration,
    pub max_concurrent_jobs: usize,
}

impl DaemonSettings {
    /// Resolve from a merged `daemon` block whose defaults are already
    /// filled in. A leaf still unset (only possible for a block that did not
    /// come through the merge) falls back to the same built-in default.
    pub fn from_merged(config: &DaemonConfig) -> Self {
        let terminal = config.terminal.clone().unwrap_or_default();
        let network = config.network.clone().unwrap_or_default();
        let features = config.features.clone().unwrap_or_default();
        let squad = features.squad.unwrap_or_default();
        let worker = features.worker.unwrap_or_default();
        let push = worker.push.clone().unwrap_or_default();
        let pull = worker.pull.clone().unwrap_or_default();
        let engine = features.engine.unwrap_or_default();
        Self {
            max_concurrent_jobs: config.max_concurrent_jobs.unwrap_or(2),
            verbose_setup: config.verbose_setup.unwrap_or(false),
            terminal: TerminalSettings {
                cols: terminal.cols.unwrap_or(120),
                rows: terminal.rows.unwrap_or(40),
                remote_input: terminal.remote_input.unwrap_or(false),
            },
            network: NetworkSettings {
                enabled: network.enabled.unwrap_or(false),
                listen: network.listen.unwrap_or_else(|| "127.0.0.1:9876".into()),
                tls_sans: network.tls_sans.unwrap_or_default(),
                features: network.features.unwrap_or_else(|| vec!["worker".into()]),
            },
            squad: SquadSettings {
                enabled: squad.enabled.unwrap_or(false),
                interaction: squad.interaction.unwrap_or(Interaction::Interactive),
                max_concurrent_jobs: squad.max_concurrent_jobs.unwrap_or(1),
                max_concurrent_evaluations: squad.max_concurrent_evaluations.unwrap_or(2),
                env_persistence: squad.env_persistence.unwrap_or_default(),
            },
            worker: WorkerSettings {
                enabled: worker.enabled.unwrap_or(false),
                max_concurrent_jobs: worker.max_concurrent_jobs.unwrap_or(2),
                push_enabled: push.enabled.unwrap_or(true),
                push_interaction: push.interaction.unwrap_or(Interaction::Headless),
                push_allow_override: push.allow_override.unwrap_or(true),
                push_work_dirs: push
                    .work_dirs
                    .as_ref()
                    .and_then(|w| w.allow.clone())
                    .unwrap_or_default(),
                push_work_dir_roots: push
                    .work_dirs
                    .as_ref()
                    .and_then(|w| w.roots.clone())
                    .unwrap_or_default(),
                pull_enabled: pull.enabled.unwrap_or(true),
                pull_interaction: pull.interaction.unwrap_or(Interaction::Interactive),
                pull_allow_override: pull.allow_override.unwrap_or(true),
                pull_work_root: pull.work_root,
            },
            engine: EngineSettings {
                enabled: engine.enabled.unwrap_or(false),
                allow_ephemeral: engine.allow_ephemeral.unwrap_or(true),
                idle_timeout: engine
                    .idle_timeout
                    .map_or(std::time::Duration::from_secs(15 * 60), |d| d.to_duration()),
                max_concurrent_jobs: engine.max_concurrent_jobs.unwrap_or(4),
            },
        }
    }
}
