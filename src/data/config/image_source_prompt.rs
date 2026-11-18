//! The typed builtin image-source choice that `awman ready` may put to a
//! person (WI 0158).
//!
//! These are plain data: Layer 2 (`ReadyCommand`) builds the request and
//! decides what an answer means, Layer 0
//! ([`crate::data::config::image_source_setup`]) validates and writes the
//! chosen source, and a Layer 3 frontend only displays the request and
//! returns the raw answer. Every type is serializable because the TUI and the
//! relayed CLI carry it across the instance boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::data::config::effective::Layer;
use crate::data::config::fields::FileKind;
use crate::data::config::image_source::ImageSourceKind;

/// The one typed source-choice request. One interaction, two stages, each
/// asked at most once per `ready` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", content = "request", rename_all = "snake_case")]
pub enum ImageSourcePrompt {
    /// Stage 1: pick a source kind, its fields, the scope, and the builder
    /// opt-in.
    Choose(ImageSourceChoiceRequest),
    /// Stage 2: Layer 0's preview of the exact write; save it or not.
    Confirm(ImageSourcePreview),
}

/// Stage 1 of [`ImageSourcePrompt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourceChoiceRequest {
    /// The image the choice is for.
    pub subject: ImageSubject,
    /// Other images of this run that also lack a source (informational).
    pub also_missing: Vec<ImageSubject>,
    /// Why the subject has no source.
    pub reason: MissingSourceReason,
    /// Offered kinds, in display order. "Leave unconfigured" is not in this
    /// list: every frontend always offers it as the last (and default) entry.
    pub choices: Vec<ImageSourceChoice>,
    /// Scopes the write may go to; the first is the recommended default.
    /// Never empty.
    pub scopes: Vec<ImageSourceScopeOption>,
    /// Sentences to show above the choices, one per entry, plain text.
    pub notes: Vec<String>,
}

/// One awman image a `ready` run prepares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSubject {
    pub agent: String,
    pub tag: String,
    /// The image of the agent `ready` was invoked for.
    pub is_default: bool,
    /// An image for this tag is already installed in the runtime and keeps
    /// working if the user leaves the source unconfigured.
    pub installed: bool,
}

/// Why an image has no usable source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum MissingSourceReason {
    /// Neither `runtime.builtin.images[tag]` nor `runtime.builtin.imageSource`.
    Unconfigured,
    /// `runtime.builtin.imageSource` exists but names one image.
    DefaultPinned { kind: ImageSourceKind },
}

/// One offered source kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourceChoice {
    pub kind: ImageSourceKind,
    /// One-line description ("Export from the local Docker Engine").
    pub description: String,
    /// Text fields to gather, in order.
    pub fields: Vec<ImageSourceField>,
    /// `Some(explanation)` when the selected local store can build and the
    /// builder is not already enabled: the frontend asks a separate yes/no
    /// whose default is **no**. `None`: do not ask.
    pub builder_offer: Option<String>,
}

/// One text field of a choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourceField {
    pub id: ImageSourceFieldId,
    /// "Archive path".
    pub label: String,
    /// One sentence on what to enter.
    pub help: String,
    /// Blank input is refused by Layer 2 when true.
    pub required: bool,
}

/// Identifies a gathered field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourceFieldId {
    Host,
    Reference,
    Registry,
    Path,
}

/// The config file a chosen source is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourceScope {
    /// `<git root>/.awman/config.json`.
    Repo,
    /// `<AWMAN_HOME>/config.json`.
    Global,
}

impl ImageSourceScope {
    pub fn file_kind(self) -> FileKind {
        match self {
            Self::Repo => FileKind::Repo,
            Self::Global => FileKind::Global,
        }
    }

    pub fn layer(self) -> Layer {
        match self {
            Self::Repo => Layer::Repo,
            Self::Global => Layer::Global,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Global => "global",
        }
    }
}

/// A scope and the file it would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourceScopeOption {
    pub scope: ImageSourceScope,
    pub path: PathBuf,
}

/// What the person picked in stage 1. Raw, unvalidated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourceSelection {
    pub kind: ImageSourceKind,
    /// Trimmed text per field id. A blank optional field is omitted.
    pub fields: BTreeMap<ImageSourceFieldId, String>,
    pub scope: ImageSourceScope,
    /// The separate, explicit builder answer. `false` whenever the chosen
    /// choice's `builder_offer` was `None`.
    pub enable_builder: bool,
}

/// A frontend's answer to an [`ImageSourcePrompt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", content = "selection", rename_all = "snake_case")]
pub enum ImageSourceAnswer {
    /// Stage 1 answer.
    Selected(ImageSourceSelection),
    /// Stage 2 answer: write it.
    Confirmed,
    /// The person chose "leave unconfigured" (stage 1) or "don't save"
    /// (stage 2).
    LeaveUnconfigured,
    /// Esc / Ctrl-C / dialog dismissed / command interrupted.
    Cancelled,
    /// Input ended (closed stdin, Ctrl-D) before an answer.
    Eof,
    /// This frontend cannot ask a person. The default.
    Unavailable,
}

/// Stage-2 payload: exactly what will be written, and what it means
/// afterwards. Built by
/// [`crate::data::config::image_source_setup::plan_image_source_edit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSourcePreview {
    pub scope: ImageSourceScope,
    /// The file that will be written.
    pub path: PathBuf,
    /// `runtime.builtin.imageSource` or `runtime.builtin.images["<tag>"]`.
    pub field: String,
    /// The exact JSON this edit sets, nested from the document root and
    /// pretty-printed. Contains only the keys the edit writes.
    pub fragment: String,
    /// After the write: the effective source of every image of this run.
    pub effective: Vec<EffectiveImageSource>,
    pub builder: BuilderPreview,
    /// Extra sentences.
    pub notes: Vec<String>,
}

/// The effective source of one image after a planned write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveImageSource {
    pub agent: String,
    pub tag: String,
    /// `None`: still no source after this write.
    pub source: Option<EffectiveSource>,
}

/// Where an image's source comes from after a planned write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSource {
    pub kind: ImageSourceKind,
    /// The spec as compact JSON (`{"type":"docker-local"}`).
    pub json: String,
    /// Which key provides it.
    pub field: String,
    /// `"global"`, `"repo"` or `"task"`.
    pub scope: String,
    /// This write is what provides it.
    pub from_this_write: bool,
}

/// What a planned write does to `runtime.builtin.builder.enabled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BuilderPreview {
    /// Not asked for; `effective` is the value after the write.
    Unchanged { effective: bool },
    /// This write sets `runtime.builtin.builder.enabled` to true and it takes
    /// effect.
    Enabled,
    /// Asked for, but a closer layer sets it to false; nothing is written
    /// for it.
    Shadowed { by: String },
}

impl ImageSourcePreview {
    /// One sentence per image, then one builder line: the wording every
    /// frontend shows under the JSON fragment.
    pub fn effective_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .effective
            .iter()
            .map(|image| match &image.source {
                Some(source) => format!(
                    "{} → {} from {} ({} config{})",
                    image.tag,
                    source.kind,
                    source.field,
                    source.scope,
                    if source.from_this_write {
                        ", this change"
                    } else {
                        ""
                    }
                ),
                None => format!("{} → still no source", image.tag),
            })
            .collect();
        lines.push(match &self.builder {
            BuilderPreview::Unchanged { effective: false } => "builder: stays off".into(),
            BuilderPreview::Unchanged { effective: true } => "builder: already on".into(),
            BuilderPreview::Enabled => "builder: enabled by this change".into(),
            BuilderPreview::Shadowed { by } => format!(
                "builder: not enabled — the {by} config sets {} to false",
                crate::data::config::builtin_runtime::BUILDER_ENABLED_FIELD
            ),
        });
        lines
    }
}
