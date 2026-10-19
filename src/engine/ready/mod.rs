//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `awman ready`.

use crate::data::config::resolved_builtin::ResolvedBuiltinConfig;
#[cfg(test)]
use crate::data::image_build_lock::ImageBuildLockStore;
#[cfg(test)]
use crate::data::image_build_state::PendingBuiltImageStore;
use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{ReadyStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{
    AgentRuntimeEngine, ImageAcquisition, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
#[cfg(test)]
use crate::engine::image_prep::builder::LocalImageBuilder;
#[cfg(test)]
use crate::engine::oci::ImageSources;
use crate::engine::overlay::OverlayEngine;
use std::sync::Arc;
pub mod frontend;
pub mod host_agent;
mod phases;

pub use crate::data::ready_phase::{ReadyFailure, ReadyPhase};
// Re-exported at the module root so the many call sites that name the ping
// result or the greeting table keep their import path; the host execution
// itself is reachable only through `HostAgentPinger`.
pub use crate::data::ready_summary::{ReadyImageAction, ReadySummary};
pub use crate::engine::image_prep::ImageImporter;
pub use frontend::ReadyFrontend;
pub use host_agent::{
    select_random_greeting, HostAgentPinger, HostRefreshOutcome, LocalAgentPingResult,
    ReadyHostPing, GREETINGS,
};

#[derive(Debug, Clone)]
pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
    pub non_interactive: bool,
    /// Env-passthrough list for audit container runs.
    pub env_passthrough: Option<Vec<String>>,
}

pub struct ReadyEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    agent_engine: Arc<AgentEngine>,
    options: ReadyEngineOptions,
    phase: ReadyPhase,
    summary: ReadySummary,
    /// Hash of `Dockerfile.dev` captured just before the audit runs, so we can
    /// detect modifications made by the agent and trigger a rebuild.
    pre_audit_dockerfile_hash: Option<u64>,
    /// The one handle permitted to execute an agent binary on the host
    /// (F-38). `awman ready`'s local-agent check is authorized trigger (a)
    /// in `security.md`.
    host_agent: Arc<dyn ReadyHostPing>,
    preparer: crate::engine::image_prep::AgentImagePreparer,
    image_policy: crate::engine::image_prep::BuildPolicy,
    cancel: crate::engine::oci::CancelToken,
}

impl ReadyEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolved: ResolvedBuiltinConfig,
        data_home: std::path::PathBuf,
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let runtime_name = runtime.runtime_name().to_string();
        let builder_enabled = resolved.builder_enabled;
        let preparer = crate::engine::image_prep::AgentImagePreparer::new(
            resolved,
            runtime.clone(),
            data_home,
            crate::engine::image_prep::ImageBuildContext::for_session(
                &session,
                options.agent.as_str(),
            ),
        );
        let image_policy = crate::engine::image_prep::BuildPolicy::Ready {
            build: options.build,
            no_cache: options.no_cache,
            refresh: options.refresh,
        };
        let mut summary = ReadySummary::new(runtime_name);
        summary.image_action =
            match runtime.capabilities().image_acquisition == ImageAcquisition::Import {
                true if builder_enabled => ReadyImageAction::BuildAndImport,
                true => ReadyImageAction::Import,
                false => ReadyImageAction::Build,
            };
        let cancel = preparer.cancel_token();
        Self {
            session,
            git_engine,
            overlay_engine,
            runtime: runtime.clone(),
            agent_engine,
            options,
            phase: ReadyPhase::Preflight,
            summary,
            pre_audit_dockerfile_hash: None,
            host_agent: Arc::new(HostAgentPinger::new()),
            preparer,
            image_policy,
            cancel,
        }
    }

    /// API setup builds missing source images without forcing a rebuild.
    #[allow(clippy::too_many_arguments)]
    pub fn for_session_setup(
        resolved: ResolvedBuiltinConfig,
        data_home: std::path::PathBuf,
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let mut engine = Self::new(
            resolved,
            data_home,
            session,
            git_engine,
            overlay_engine,
            runtime,
            agent_engine,
            options,
        );
        engine.image_policy = crate::engine::image_prep::BuildPolicy::SessionSetup;
        engine
    }

    /// Replace the session-derived image sources. Test-only: production
    /// sources and builder enablement both come from the session's
    /// effective config, so they cannot disagree.
    #[cfg(test)]
    pub fn with_image_sources(mut self, sources: ImageSources) -> Self {
        self.preparer.replace_sources(sources);
        self
    }

    /// Inject a recording builder for daemon-free ready tests.
    #[cfg(test)]
    pub fn with_local_builder(mut self, builder: Arc<dyn LocalImageBuilder>) -> Self {
        self.preparer = self.preparer.with_local_builder(builder);
        self
    }

    #[cfg(test)]
    pub fn with_image_importer(mut self, importer: Arc<dyn ImageImporter>) -> Self {
        self.preparer = self.preparer.with_image_importer(importer);
        self
    }

    #[cfg(test)]
    pub fn with_host_ping(mut self, ping: Arc<dyn ReadyHostPing>) -> Self {
        self.host_agent = ping;
        self
    }

    /// Inject private lock and receipt roots for hermetic ready tests.
    #[cfg(test)]
    pub fn with_build_stores(
        mut self,
        locks: ImageBuildLockStore,
        receipts: PendingBuiltImageStore,
    ) -> Self {
        self.preparer = self.preparer.with_build_stores(locks, receipts);
        self
    }

    /// Cancellation handle for an in-flight ready import. Dropping an import
    /// future also cancels it, so async deadlines work on a current-thread runtime.
    pub fn cancel_token(&self) -> crate::engine::oci::CancelToken {
        self.preparer.cancel_token()
    }
}
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
impl ReadyEngine {
    pub(crate) fn into_image_test_parts(
        self,
    ) -> (
        Arc<Session>,
        crate::engine::image_prep::AgentImagePreparer,
        crate::engine::image_prep::BuildPolicy,
    ) {
        (self.session, self.preparer, self.image_policy)
    }
}
