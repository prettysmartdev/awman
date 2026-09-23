//! Typed setup-step identifiers reported by the ready, init and agent-image
//! engines (WI 0114 F-45).
//!
//! `AgentImageFrontend::report_step_status` used to take a free-form `&str`
//! key, so the set of steps a frontend could ever be asked to render was
//! knowable only by grepping three engines, and two frontends indexed their
//! own state by that string. These enums are the whole set, and their
//! [`Display`](std::fmt::Display) impls reproduce the exact strings those
//! engines passed — so a frontend that only needs the label writes
//! `step.to_string()` and its output is unchanged.
//!
//! Layer 0 because both the engines that raise them and the frontends that
//! render them must be able to name them, and because `SetupEventPayload`
//! carries the rendered label onto the API's setup event stream.

use std::fmt;
use std::path::PathBuf;

/// A step of `AgentEngine::ensure_available` — making one agent's image
/// available before a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSetupStep {
    /// Fetching the bundled `Dockerfile.<agent>` into the repo.
    DownloadingDockerfile,
    /// Building the agent image from it.
    BuildingImage,
}

impl fmt::Display for AgentSetupStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DownloadingDockerfile => f.write_str("Downloading Dockerfile"),
            Self::BuildingImage => f.write_str("Building image"),
        }
    }
}

/// A step of the `awman ready` phase machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyStep {
    /// A kit-declarative runtime prepared the agent in one operation.
    ApplyAgentKit,
    /// Confirming the project Dockerfile exists at this path.
    CheckDockerfile(PathBuf),
    /// Writing the project Dockerfile at this path.
    CreateDockerfile(PathBuf),
    /// Fetching the bundled `Dockerfile.<agent>`.
    DownloadAgentDockerfile,
    BuildBaseImage,
    BuildAgentImage,
    /// Building a *non-default* agent's image, named.
    BuildAgentImageFor(String),
    /// The roll-up over the non-default agents.
    OtherAgents,
    /// The roll-up over agents whose images are absent.
    MissingImages,
    /// The sanctioned host-side agent ping.
    CheckLocalAgent,
    RebuildingAfterAudit,
}

impl fmt::Display for ReadyStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApplyAgentKit => f.write_str("Apply agent kit"),
            Self::CheckDockerfile(path) => write!(f, "Check {}", path.display()),
            Self::CreateDockerfile(path) => write!(f, "Create {}", path.display()),
            Self::DownloadAgentDockerfile => f.write_str("Download agent Dockerfile"),
            Self::BuildBaseImage => f.write_str("Build base image"),
            Self::BuildAgentImage => f.write_str("Build agent image"),
            Self::BuildAgentImageFor(agent) => write!(f, "Build agent image: {agent}"),
            Self::OtherAgents => f.write_str("Other agents"),
            Self::MissingImages => f.write_str("Missing images"),
            Self::CheckLocalAgent => f.write_str("Check local agent"),
            Self::RebuildingAfterAudit => f.write_str("Rebuilding after audit"),
        }
    }
}

/// A step of the `awman init` phase machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitStep {
    /// The single combined build step init reports on failure.
    BuildImage,
    BuildBaseImage,
    BuildAgentImage,
    RebuildingAfterAudit,
}

impl fmt::Display for InitStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuildImage => f.write_str("Build image"),
            Self::BuildBaseImage => f.write_str("Build base image"),
            Self::BuildAgentImage => f.write_str("Build agent image"),
            Self::RebuildingAfterAudit => f.write_str("Rebuilding after audit"),
        }
    }
}

/// What `AgentImageFrontend::report_step_status` reports on.
///
/// One type because one trait method carries all three engines' steps:
/// `ReadyFrontend` and `InitFrontend` both extend `AgentImageFrontend`, which
/// the agent-image engine also drives directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    Agent(AgentSetupStep),
    Ready(ReadyStep),
    Init(InitStep),
    /// One line of runtime build output, forwarded as a transient step label.
    ///
    /// The engines stream an image build's stdout through the same method,
    /// one call per line with `StepStatus::Running`, so a frontend that draws
    /// a live build log gets it without a second channel. It is the one
    /// variant whose text is not a fixed label.
    Output(String),
}

impl fmt::Display for SetupStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(step) => step.fmt(f),
            Self::Ready(step) => step.fmt(f),
            Self::Init(step) => step.fmt(f),
            Self::Output(line) => f.write_str(line),
        }
    }
}

impl From<AgentSetupStep> for SetupStep {
    fn from(step: AgentSetupStep) -> Self {
        Self::Agent(step)
    }
}

impl From<ReadyStep> for SetupStep {
    fn from(step: ReadyStep) -> Self {
        Self::Ready(step)
    }
}

impl From<InitStep> for SetupStep {
    fn from(step: InitStep) -> Self {
        Self::Init(step)
    }
}

impl SetupStep {
    /// A line of build output, reported as a transient step.
    pub fn output(line: impl Into<String>) -> Self {
        Self::Output(line.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-45 is behaviour-preserving: every `Display` must reproduce the exact
    /// string the engines passed as a `&str` key before the change, because
    /// two frontends index their own state by it and the API's setup event
    /// stream carries it on the wire.
    #[test]
    fn display_reproduces_the_pre_f45_step_keys() {
        let cases: &[(SetupStep, &str)] = &[
            (
                AgentSetupStep::DownloadingDockerfile.into(),
                "Downloading Dockerfile",
            ),
            (AgentSetupStep::BuildingImage.into(), "Building image"),
            (ReadyStep::ApplyAgentKit.into(), "Apply agent kit"),
            (
                ReadyStep::CheckDockerfile(PathBuf::from("/repo/Dockerfile.dev")).into(),
                "Check /repo/Dockerfile.dev",
            ),
            (
                ReadyStep::CreateDockerfile(PathBuf::from("/repo/Dockerfile.dev")).into(),
                "Create /repo/Dockerfile.dev",
            ),
            (
                ReadyStep::DownloadAgentDockerfile.into(),
                "Download agent Dockerfile",
            ),
            (ReadyStep::BuildBaseImage.into(), "Build base image"),
            (ReadyStep::BuildAgentImage.into(), "Build agent image"),
            (
                ReadyStep::BuildAgentImageFor("codex".into()).into(),
                "Build agent image: codex",
            ),
            (ReadyStep::OtherAgents.into(), "Other agents"),
            (ReadyStep::MissingImages.into(), "Missing images"),
            (ReadyStep::CheckLocalAgent.into(), "Check local agent"),
            (
                ReadyStep::RebuildingAfterAudit.into(),
                "Rebuilding after audit",
            ),
            (InitStep::BuildImage.into(), "Build image"),
            (InitStep::BuildBaseImage.into(), "Build base image"),
            (InitStep::BuildAgentImage.into(), "Build agent image"),
            (
                InitStep::RebuildingAfterAudit.into(),
                "Rebuilding after audit",
            ),
            (
                SetupStep::output("#5 [2/7] RUN apt-get update"),
                "#5 [2/7] RUN apt-get update",
            ),
        ];
        for (step, want) in cases {
            assert_eq!(&step.to_string(), want, "Display for {step:?}");
        }
    }

    /// `ReadyStep::BuildAgentImage` and `BuildAgentImageFor` render
    /// differently, which matters: the ready flow reports the default agent's
    /// build and each non-default agent's build as separate steps, and a
    /// frontend that keys on the label must keep telling them apart.
    #[test]
    fn the_default_and_named_agent_image_steps_do_not_collide() {
        assert_ne!(
            ReadyStep::BuildAgentImage.to_string(),
            ReadyStep::BuildAgentImageFor("claude".into()).to_string()
        );
    }
}
