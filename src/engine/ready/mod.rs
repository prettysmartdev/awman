//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `awman ready`.

use crate::data::config::resolved_builtin::ResolvedBuiltinConfig;

use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{ReadyStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{
    AgentRuntimeEngine, ImageAcquisition, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

use crate::engine::overlay::OverlayEngine;
use std::sync::Arc;
#[cfg(test)]
mod contracts;
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
    /// `options.agent` resolved once, on the first step, and read by every
    /// later phase (its host ping included) so one `ready` run never mixes
    /// two versions of a definition.
    definition: Option<crate::engine::agent::ResolvedAgent>,
    preparer: crate::engine::image_prep::AgentImagePreparer,
    image_policy: crate::engine::image_prep::BuildPolicy,
    cancel: crate::engine::oci::CancelToken,
}

impl ReadyEngine {
    /// The builtin settings a `ReadyEngine` for `runtime` needs: resolved and
    /// validated for a runtime that imports images, `None` for one that builds
    /// them (Docker, Apple Containers). A `builtin` block is never read, so an
    /// invalid or widening one cannot fail `ready` on a runtime that ignores it.
    pub fn builtin_inputs(
        session: &Session,
        runtime: &dyn AgentRuntimeEngine,
    ) -> Result<Option<ResolvedBuiltinConfig>, crate::data::error::DataError> {
        if runtime.capabilities().image_acquisition == ImageAcquisition::Import {
            session.effective_config().builtin_settings().map(Some)
        } else {
            Ok(None)
        }
    }

    /// `resolved` comes from [`ReadyEngine::builtin_inputs`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolved: Option<ResolvedBuiltinConfig>,
        data_home: std::path::PathBuf,
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let runtime_name = runtime.runtime_name().to_string();
        let imports = runtime.capabilities().image_acquisition == ImageAcquisition::Import;
        debug_assert!(
            imports == resolved.is_some(),
            "builtin inputs must come from ReadyEngine::builtin_inputs"
        );
        let builder_enabled = resolved.as_ref().is_some_and(|r| r.builder_enabled);
        let context = crate::engine::image_prep::ImageBuildContext::for_session(
            &session,
            options.agent.as_str(),
        );
        let preparer = match resolved {
            Some(resolved) => crate::engine::image_prep::AgentImagePreparer::new(
                resolved,
                runtime.clone(),
                data_home,
                context,
            ),
            None => crate::engine::image_prep::AgentImagePreparer::for_building_runtime(
                runtime.clone(),
                data_home,
                context,
            ),
        }
        .with_definition_env(session.env().clone());
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
        let host_env = session.env().clone();
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
            host_agent: Arc::new(HostAgentPinger::with_env(host_env)),
            definition: None,
            preparer,
            image_policy,
            cancel,
        }
    }

    /// API setup builds missing source images without forcing a rebuild.
    #[allow(clippy::too_many_arguments)]
    pub fn for_session_setup(
        resolved: Option<ResolvedBuiltinConfig>,
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

    /// Cancellation handle for an in-flight ready import. Dropping an import
    /// future also cancels it, so async deadlines work on a current-thread runtime.
    pub fn cancel_token(&self) -> crate::engine::oci::CancelToken {
        self.preparer.cancel_token()
    }
}
