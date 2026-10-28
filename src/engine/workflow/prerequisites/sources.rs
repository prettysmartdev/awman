//! Injectable metadata and credential sources for prerequisite probes.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::fs::phase_tools_cache::PhaseToolsCache;
use crate::data::fs::ssh_keys::SshKeyInventory;
use crate::engine::auth::github_token::GitHubTokenResolver;
use crate::engine::container::phase_tools::PhaseToolsStatus;
use crate::engine::container::ContainerRuntime;
use crate::engine::git::GitEngine;

pub use crate::engine::auth::github_token::TokenProbeSource;
pub use crate::engine::git::GitProbeSource;

pub trait SshKeySource: Send + Sync {
    fn inventory(&self) -> SshKeyInventory;
}

pub struct HostSshKeys {
    pub home: PathBuf,
}

impl SshKeySource for HostSshKeys {
    fn inventory(&self) -> SshKeyInventory {
        SshKeyInventory::scan(&self.home)
    }
}

pub trait PhaseToolSource: Send + Sync {
    fn probe(&self, image: &str, tools: &[&str]) -> PhaseToolsStatus;
}

pub struct ContainerToolProbe {
    pub runtime: Option<Arc<ContainerRuntime>>,
}

impl PhaseToolSource for ContainerToolProbe {
    fn probe(&self, image: &str, tools: &[&str]) -> PhaseToolsStatus {
        let env = crate::data::config::env::Env::from_process();
        if env.test_isolation() && !env.test_docker() {
            return PhaseToolsStatus::Unknown {
                reason: "test isolation".into(),
            };
        }
        let Some(runtime) = &self.runtime else {
            return PhaseToolsStatus::Unknown {
                reason: "runtime has no phase-container probe".into(),
            };
        };
        runtime.probe_phase_tools(image, tools)
    }
}

#[derive(Clone)]
pub struct PreflightSources {
    pub git: Arc<dyn GitProbeSource>,
    pub ssh: Arc<dyn SshKeySource>,
    pub tokens: Arc<dyn TokenProbeSource>,
    pub tools: Arc<dyn PhaseToolSource>,
}

impl PreflightSources {
    pub fn production(
        git: Arc<GitEngine>,
        home: PathBuf,
        resolver: GitHubTokenResolver,
        runtime: Option<Arc<ContainerRuntime>>,
        _cache: PhaseToolsCache,
    ) -> Self {
        Self {
            git,
            ssh: Arc::new(HostSshKeys { home }),
            tokens: Arc::new(resolver),
            tools: Arc::new(ContainerToolProbe { runtime }),
        }
    }
}
