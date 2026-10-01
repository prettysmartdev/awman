//! Per-image results of `awman ready` for runtimes that import images
//! (builtin), with the build and import stages recorded separately.
//!
//! With the source-matched local builder enabled, one image can be built into
//! its source's store and then imported from it. The two stages succeed or
//! fail independently ("built, import failed" is a different state from
//! "build failed"), so each result keeps both, plus any warnings raised while
//! planning it. Frontends render these; they never compose warning text of
//! their own.

use serde::{Deserialize, Serialize};

use crate::data::config::image_source::ImageSourceKind;
use crate::data::step_status::StepStatus;

/// The build and import outcome for one agent image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyImageResult {
    /// Agent the image belongs to.
    pub agent: String,
    /// awman image tag the runtime installs it under.
    pub tag: String,
    /// Kind of the configured source, canonical spelling. `None` when no
    /// source is configured for this image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<ImageSourceKind>,
    /// Effective source reference (non-secret). `None` with no source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Local build into the source's store. `Skipped` when no build was
    /// needed, the builder is off, or the source cannot be built into.
    pub build: StepStatus,
    /// Acquisition from the source and install into the runtime.
    pub import: StepStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadyImageWarning>,
    /// A non-default agent's image, prepared by `ready --build`. Its
    /// failure is reported, but (as for every non-default image) does not
    /// make `awman ready` fail.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl ReadyImageResult {
    /// A result for `agent`/`tag` with both stages pending.
    pub fn new(agent: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            tag: tag.into(),
            source_kind: None,
            reference: None,
            build: StepStatus::Pending,
            import: StepStatus::Pending,
            warnings: Vec::new(),
            optional: false,
        }
    }

    /// Whether either stage failed.
    pub fn has_failure(&self) -> bool {
        matches!(self.build, StepStatus::Failed(_)) || matches!(self.import, StepStatus::Failed(_))
    }

    /// Whether either stage has not reached a terminal status.
    pub fn is_unfinished(&self) -> bool {
        !self.build.is_terminal() || !self.import.is_terminal()
    }
}

/// The project base image built into one store, as a build input for the
/// agent images built into that store. It is never installed into the
/// builtin runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyBaseImageResult {
    /// Non-secret description of the store (e.g. the Docker endpoint, or
    /// `apple-containers`).
    pub store: String,
    /// Project image tag built there.
    pub tag: String,
    pub build: StepStatus,
}

/// A non-fatal condition raised while preparing one image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ReadyImageWarning {
    /// A build was wanted, but the image's source is not a local store awman
    /// can build into, so the build was skipped and the image is imported
    /// from the source as configured.
    IncompatibleBuilder {
        tag: String,
        source_kind: ImageSourceKind,
        /// Why that source cannot be built into (non-secret).
        reason: String,
    },
}

impl std::fmt::Display for ReadyImageWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleBuilder {
                tag,
                source_kind,
                reason,
            } => write!(
                f,
                "builtin runtime builder is enabled, but image '{tag}' uses source \
                 '{source_kind}', which cannot build Dockerfiles ({reason}); skipping build \
                 and importing from the configured source"
            ),
        }
    }
}
