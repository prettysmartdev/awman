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
    /// Built only for non-default agents. Like an optional agent image, its
    /// failure is reported but does not make `awman ready` fail; a required
    /// agent that needs the same base fails through its own result.
    #[serde(default)]
    pub optional: bool,
}

/// A non-default agent image that is not installed, and what `awman ready
/// --build` would do about it. Recorded per image, so the hint names the
/// right step for each image rather than one verb for the whole summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingAgentImage {
    pub agent: String,
    pub plan: ReadyImagePlan,
}

/// What `awman ready --build` would do for one missing agent image, decided
/// from the runtime and, on importing runtimes, the image's configured source
/// and builder selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadyImagePlan {
    /// The runtime builds images itself (Docker, Apple Containers).
    Build,
    /// Imported from its configured source; nothing is built.
    Import,
    /// Built into its source store, then imported from it.
    BuildAndImport,
    /// No usable source is configured, so nothing can acquire it yet.
    ConfigureSource,
    /// The selected local output is read-only or collides with another
    /// build. A writable, distinct source reference is needed before --build.
    ConfigureBuildOutput,
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
    /// The source store could not be inspected, so the installed image was
    /// kept instead of rebuilding.
    BuilderUnreachable {
        tag: String,
        /// The builder as named in messages (e.g. `Docker at unix://…`).
        builder: String,
        detail: String,
    },
    /// An earlier build's receipt names an image the source no longer
    /// exposes as one platform image; the import is refreshed instead.
    BuiltImageNotExposed { tag: String, reference: String },
    /// An earlier build's receipt names an image the source reference no
    /// longer holds; the image it holds now is imported.
    StaleBuildReceipt { tag: String, reference: String },
    /// The image was imported, but its build receipt could not be removed;
    /// a later run re-imports the same content.
    ReceiptNotCleared { tag: String, detail: String },
    /// The build succeeded and only the import failed; the receipt lets the
    /// next run retry the import without rebuilding.
    BuildOkImportFailed { tag: String },
    /// The import from a buildable source failed; rebuilding it there is
    /// the way forward.
    RebuildInSourceStore { tag: String },
    /// The audit changed `Dockerfile.dev`, but this session cannot rebuild
    /// the default image: no builder is enabled for it, or no source is
    /// configured to import a rebuilt image from.
    AuditRebuildUnavailable { source_configured: bool },
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
            Self::BuilderUnreachable {
                tag,
                builder,
                detail,
            } => write!(
                f,
                "{tag}: {builder} is unreachable; using the installed image ({detail})"
            ),
            Self::BuiltImageNotExposed { tag, reference } => write!(
                f,
                "{tag}: {reference} no longer exposes the image an earlier `awman ready` built; \
                 refreshing the source import"
            ),
            Self::StaleBuildReceipt { tag, reference } => write!(
                f,
                "{tag}: {reference} no longer holds the image an earlier `awman ready` built for \
                 it; importing the image it holds now (run `awman ready --build` to rebuild)"
            ),
            Self::ReceiptNotCleared { tag, detail } => {
                write!(f, "{tag}: could not clear its build receipt: {detail}")
            }
            Self::BuildOkImportFailed { tag } => write!(
                f,
                "{tag}: the build succeeded but the import failed; run `awman ready` again to \
                 retry the import"
            ),
            Self::RebuildInSourceStore { tag } => write!(
                f,
                "{tag}: run `awman ready --build` to rebuild it in its source store"
            ),
            Self::AuditRebuildUnavailable {
                source_configured: true,
            } => write!(
                f,
                "Dockerfile changed: rebuild the image in its source store (or enable \
                 `builtin.builder.enabled` with a `docker-local` or `apple-local` source), then \
                 run `awman ready --build` to re-import it"
            ),
            Self::AuditRebuildUnavailable {
                source_configured: false,
            } => write!(
                f,
                "Dockerfile changed: rebuild the image externally and configure \
                 `builtin.imageSource`, then run `awman ready --build` to import it"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WI 0126 F1/F3: the builder warnings that used to be free text are
    /// typed and render the same words in every frontend and in
    /// `ready --json`.
    #[test]
    fn builder_warnings_render_their_wording_and_serialize_by_kind() {
        let tag = || "awman-r-claude:latest".to_string();
        let cases = [
            (
                ReadyImageWarning::BuilderUnreachable {
                    tag: tag(),
                    builder: "Docker at unix:///d.sock".into(),
                    detail: "down".into(),
                },
                "awman-r-claude:latest: Docker at unix:///d.sock is unreachable; using the \
                 installed image (down)",
                "builder-unreachable",
            ),
            (
                ReadyImageWarning::BuiltImageNotExposed {
                    tag: tag(),
                    reference: "a:1".into(),
                },
                "awman-r-claude:latest: a:1 no longer exposes the image an earlier `awman \
                 ready` built; refreshing the source import",
                "built-image-not-exposed",
            ),
            (
                ReadyImageWarning::StaleBuildReceipt {
                    tag: tag(),
                    reference: "a:1".into(),
                },
                "awman-r-claude:latest: a:1 no longer holds the image an earlier `awman ready` \
                 built for it; importing the image it holds now (run `awman ready --build` to \
                 rebuild)",
                "stale-build-receipt",
            ),
            (
                ReadyImageWarning::ReceiptNotCleared {
                    tag: tag(),
                    detail: "denied".into(),
                },
                "awman-r-claude:latest: could not clear its build receipt: denied",
                "receipt-not-cleared",
            ),
            (
                ReadyImageWarning::BuildOkImportFailed { tag: tag() },
                "awman-r-claude:latest: the build succeeded but the import failed; run `awman \
                 ready` again to retry the import",
                "build-ok-import-failed",
            ),
            (
                ReadyImageWarning::RebuildInSourceStore { tag: tag() },
                "awman-r-claude:latest: run `awman ready --build` to rebuild it in its source \
                 store",
                "rebuild-in-source-store",
            ),
            (
                ReadyImageWarning::AuditRebuildUnavailable {
                    source_configured: true,
                },
                "Dockerfile changed: rebuild the image in its source store (or enable \
                 `builtin.builder.enabled` with a `docker-local` or `apple-local` source), then \
                 run `awman ready --build` to re-import it",
                "audit-rebuild-unavailable",
            ),
            (
                ReadyImageWarning::AuditRebuildUnavailable {
                    source_configured: false,
                },
                "Dockerfile changed: rebuild the image externally and configure \
                 `builtin.imageSource`, then run `awman ready --build` to import it",
                "audit-rebuild-unavailable",
            ),
        ];
        for (warning, text, kind) in cases {
            assert_eq!(warning.to_string(), text);
            let json = serde_json::to_value(&warning).unwrap();
            assert_eq!(json["kind"], kind);
            assert_eq!(
                serde_json::from_value::<ReadyImageWarning>(json).unwrap(),
                warning
            );
        }
    }
}
