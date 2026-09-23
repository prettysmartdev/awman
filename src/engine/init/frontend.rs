//! `InitFrontend` trait — defined by Layer 1, implemented by Layer 3.

use crate::data::config::repo::WorkItemsConfig;
use crate::data::prompt::Prompt;
use crate::engine::agent::AgentImageFrontend;
use crate::engine::error::EngineError;
use crate::engine::init::phase::InitPhase;
use crate::engine::init::summary::InitSummary;

/// User's choice when no project-base Dockerfile is found during init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerfileSetupDecision {
    /// Create `Dockerfile.dev` from the bundled template.
    CreateNew,
    /// Use an existing Dockerfile at this path (relative to git_root or absolute).
    UseExisting(String),
    /// Skip Dockerfile setup entirely.
    Skip,
}

/// The same three options without the path [`DockerfileSetupDecision`] carries.
///
/// A [`Prompt`](crate::data::prompt::Prompt)'s choices are fixed values, and
/// the path is not known until after the user has picked "use an existing
/// one" — so the prompt offers these, and the frontend turns the chosen one
/// into a `DockerfileSetupDecision`, collecting the path for `UseExisting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerfileSetupChoice {
    CreateNew,
    UseExisting,
    Skip,
}

impl DockerfileSetupChoice {
    /// The decision this choice means, given whatever path the frontend
    /// collected for `UseExisting` and the prompt it came from.
    ///
    /// Both frontends run the same two steps — show the choices, then ask for
    /// a path if the answer was "use an existing one" — and both used to end
    /// with their own `_ => CreateNew` arm, a default chosen in Layer 3 and
    /// asserted by Layer 3 tests (WI 0114 F-19). Naming no file is not an
    /// answer to "which existing Dockerfile?", so it means what `prompt` says
    /// a non-answer means, and neither frontend decides that.
    pub fn decide(
        self,
        prompt: &Prompt<DockerfileSetupChoice>,
        path: Option<String>,
    ) -> DockerfileSetupDecision {
        self.into_decision(path)
            .or_else(|| {
                prompt
                    .default_on_dismiss
                    .and_then(|fallback| fallback.into_decision(None))
            })
            // Only reachable if a prompt declared its own dismissal answer to
            // be "name a path", which answers nothing. Doing nothing is the
            // only safe reading.
            .unwrap_or(DockerfileSetupDecision::Skip)
    }

    fn into_decision(self, path: Option<String>) -> Option<DockerfileSetupDecision> {
        match self {
            Self::CreateNew => Some(DockerfileSetupDecision::CreateNew),
            Self::Skip => Some(DockerfileSetupDecision::Skip),
            Self::UseExisting => path
                .filter(|p| !p.is_empty())
                .map(DockerfileSetupDecision::UseExisting),
        }
    }
}

/// `report_step_status` and `container_frontend` come from
/// [`AgentImageFrontend`]: the init flow reports image-setup steps through
/// the same two methods the agent engine uses, so they are declared once.
pub trait InitFrontend: AgentImageFrontend {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;
    /// Called when no project-base Dockerfile is found during init.
    /// Returns the user's choice of how to proceed.
    ///
    /// `prompt` carries the wording, the hotkeys and — crucially — the answer
    /// a dismissal means. Both frontends used to answer `CreateNew` on a
    /// dismissal out of their own bodies, and their tests asserted it, which
    /// is a default decided and pinned in Layer 3 (WI 0114 F-19). Layer 2
    /// builds it (`command::prompts::dockerfile_setup`) and hands it down
    /// through `InitEngineOptions`.
    ///
    /// `dockerfile_path` is the path the engine looked at, already resolved
    /// from the repo config, so the frontend can show it without loading the
    /// config itself — both used to call `RepoConfig::load(git_root)` just to
    /// build that one string (WI 0114 F-31). It is a fact, not copy: a
    /// frontend displays it beside the prompt.
    fn ask_dockerfile_setup(
        &mut self,
        prompt: &crate::data::prompt::Prompt<DockerfileSetupChoice>,
        git_root: &std::path::Path,
        dockerfile_path: &str,
    ) -> Result<DockerfileSetupDecision, EngineError>;
    fn report_phase(&mut self, phase: &InitPhase);
    fn report_summary(&mut self, summary: &InitSummary);
}
