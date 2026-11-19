//! Layer 0 support for choosing a builtin image source (WI 0158).
//!
//! Everything here is decided from configuration alone: whether an awman
//! image has a source ([`ImageSourceStatus`]), where a chosen source must be
//! written ([`slot_for`], [`image_source_scopes`]), what the write would look
//! like and mean afterwards ([`plan_image_source_edit`]), and the write
//! itself ([`apply_image_source_plan`]). A source that exists in config is
//! *configured* whatever happens when it is contacted, so a failing source is
//! never mistaken for a missing one.
//!
//! The write reuses [`ConfigFile::load`] and [`ConfigFile::save`], the same
//! validator and atomic writer `awman config set` uses: unrelated keys are
//! preserved by the typed round trip, file permissions by the writer.

use std::path::{Path, PathBuf};

use crate::data::config::builtin_runtime::{BuiltinBuilderConfig, BUILDER_ENABLED_FIELD};
use crate::data::config::effective::{EffectiveConfig, Layer};
use crate::data::config::fields::FileKind;
use crate::data::config::file::{global_config_path, repo_config_path, ConfigFile};
use crate::data::config::image_source::ImageSourceSpec;
use crate::data::config::image_source_prompt::{
    BuilderPreview, EffectiveImageSource, EffectiveSource, ImageSourcePreview, ImageSourceScope,
    ImageSourceScopeOption,
};
use crate::data::error::DataError;

/// Dotted name of the default source.
pub const IMAGE_SOURCE_FIELD: &str = "runtime.builtin.imageSource";

/// Dotted name of the per-image source map.
pub const IMAGES_FIELD: &str = "runtime.builtin.images";

/// Which config key holds a source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "slot", content = "tag", rename_all = "snake_case")]
pub enum ImageSourceSlot {
    /// `runtime.builtin.imageSource`
    Default,
    /// `runtime.builtin.images["<tag>"]`
    Image(String),
}

impl ImageSourceSlot {
    /// `runtime.builtin.imageSource` or `runtime.builtin.images["<tag>"]`.
    pub fn field(&self) -> String {
        match self {
            Self::Default => IMAGE_SOURCE_FIELD.to_string(),
            Self::Image(tag) => images_entry_field(tag),
        }
    }
}

/// `runtime.builtin.images["<tag>"]`.
pub fn images_entry_field(tag: &str) -> String {
    format!("{IMAGES_FIELD}[\"{tag}\"]")
}

/// What configuration says about the source of one awman image tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSourceStatus {
    /// No `images[tag]` entry and no `imageSource`.
    Unconfigured,
    /// A source applies to this tag. Never offered for replacement, even if
    /// it fails when contacted.
    Configured {
        slot: ImageSourceSlot,
        layer: Layer,
        spec: ImageSourceSpec,
    },
    /// `imageSource` exists but names a single image (an archive, or a fixed
    /// `reference`) and this tag belongs to a non-default agent, so it has
    /// no usable source.
    DefaultPinned { layer: Layer, spec: ImageSourceSpec },
}

impl ImageSourceStatus {
    /// `Unconfigured` or `DefaultPinned`.
    pub fn needs_source(&self) -> bool {
        !matches!(self, Self::Configured { .. })
    }
}

impl EffectiveConfig {
    /// Classify the source of `tag`. `is_default_agent` is whether `tag` is
    /// the image of the agent this `ready` run was invoked for.
    ///
    /// Reads the same merged view `builtin_settings()` hands the engine, so
    /// it cannot disagree with source resolution: a matching
    /// `runtime.builtin.images` entry beats `runtime.builtin.imageSource`,
    /// and a closer layer beats a farther one.
    pub fn image_source_status(&self, tag: &str, is_default_agent: bool) -> ImageSourceStatus {
        let builtin = self.builtin_runtime();
        if let Some(spec) = builtin.images.get(tag) {
            return ImageSourceStatus::Configured {
                slot: ImageSourceSlot::Image(tag.to_string()),
                // The map replaces as a whole, so provenance is recorded at
                // the map path (tags contain `.` and `:`).
                layer: self.source_of(IMAGES_FIELD),
                spec: spec.clone(),
            };
        }
        match builtin.image_source {
            None => ImageSourceStatus::Unconfigured,
            Some(spec) if !is_default_agent && spec.names_single_image(tag) => {
                ImageSourceStatus::DefaultPinned {
                    layer: self.source_of(IMAGE_SOURCE_FIELD),
                    spec,
                }
            }
            Some(spec) => ImageSourceStatus::Configured {
                slot: ImageSourceSlot::Default,
                layer: self.source_of(IMAGE_SOURCE_FIELD),
                spec,
            },
        }
    }
}

/// Where a source chosen for `tag` must be written.
///
/// Only an unconfigured image with a source that follows the tag becomes the
/// default. A source fixed to one image (an archive or any explicit
/// `reference`, even one equal to `tag`) would later be applied to other
/// agents' tags, and a pinned default already serves other agents, so both
/// get a `runtime.builtin.images` entry instead.
pub fn slot_for(status: &ImageSourceStatus, spec: &ImageSourceSpec, tag: &str) -> ImageSourceSlot {
    match status {
        ImageSourceStatus::Unconfigured if !spec.is_fixed_to_one_image() => {
            ImageSourceSlot::Default
        }
        _ => ImageSourceSlot::Image(tag.to_string()),
    }
}

/// The scopes a source for one tag may be written to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSourceScopes {
    /// Recommended first.
    pub offered: Vec<ImageSourceScopeOption>,
    /// Scopes left out because the write would be overridden, with the
    /// winning layer.
    pub shadowed: Vec<(ImageSourceScopeOption, Layer)>,
}

/// The scopes a source for `tag` may be written to, recommended first. A
/// scope is offered only when a write to every slot `status` can lead to
/// would take effect.
pub fn image_source_scopes(
    config: &EffectiveConfig,
    git_root: Option<&Path>,
    status: &ImageSourceStatus,
    tag: &str,
    is_default_agent: bool,
) -> Result<ImageSourceScopes, DataError> {
    let mut candidates = vec![ImageSourceScopeOption {
        scope: ImageSourceScope::Global,
        path: global_config_path(config.env())?,
    }];
    if let Some(root) = git_root {
        let repo = ImageSourceScopeOption {
            scope: ImageSourceScope::Repo,
            path: repo_config_path(root),
        };
        let repo_first = match status {
            ImageSourceStatus::DefaultPinned { .. } => true,
            _ => config.source_of("runtime.kind") == Layer::Repo,
        };
        if repo_first {
            candidates.insert(0, repo);
        } else {
            candidates.push(repo);
        }
    }
    let slots = match status {
        ImageSourceStatus::Unconfigured => vec![
            (
                ImageSourceSlot::Default,
                ImageSourceSpec::DockerStore {
                    host: None,
                    tls: None,
                    reference: None,
                },
            ),
            (
                ImageSourceSlot::Image(tag.to_string()),
                ImageSourceSpec::Archive {
                    path: PathBuf::from("/probe.tar"),
                },
            ),
        ],
        _ => vec![(
            ImageSourceSlot::Image(tag.to_string()),
            ImageSourceSpec::Archive {
                path: PathBuf::from("/probe.tar"),
            },
        )],
    };
    let mut scopes = ImageSourceScopes {
        offered: Vec::new(),
        shadowed: Vec::new(),
    };
    'candidates: for candidate in candidates {
        for (slot, spec) in &slots {
            let edit = ImageSourceEdit {
                scope: candidate.scope,
                slot: slot.clone(),
                spec: spec.clone(),
                enable_builder: false,
            };
            let simulated = match simulate(config, git_root, &edit) {
                Ok(simulation) => simulation.simulated,
                Err(ImageSourcePlanError::Io(e)) => return Err(DataError::Other(e)),
                // A target file that does not validate today cannot take a
                // write either: do not offer it.
                Err(_) => {
                    let layer = candidate.scope.layer();
                    scopes.shadowed.push((candidate, layer));
                    continue 'candidates;
                }
            };
            match simulated.image_source_status(tag, is_default_agent) {
                ImageSourceStatus::Configured {
                    slot: got, layer, ..
                } if got == *slot && layer == candidate.scope.layer() => {}
                status => {
                    let layer = winner(&simulated, &edit, &status);
                    scopes.shadowed.push((candidate, layer));
                    continue 'candidates;
                }
            }
        }
        scopes.offered.push(candidate);
    }
    Ok(scopes)
}

/// A source to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSourceEdit {
    pub scope: ImageSourceScope,
    pub slot: ImageSourceSlot,
    pub spec: ImageSourceSpec,
    /// Also set `runtime.builtin.builder.enabled` to true.
    pub enable_builder: bool,
}

/// One image whose effective source the preview reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSubject {
    pub agent: String,
    pub tag: String,
    pub is_default: bool,
}

/// A validated, not yet written edit.
#[derive(Debug, Clone)]
pub struct ImageSourcePlan {
    edit: ImageSourceEdit,
    path: PathBuf,
    kind: FileKind,
    git_root: Option<PathBuf>,
    /// The global config path, re-read before the write.
    global_path: PathBuf,
    /// The repo layer used when there is no Git root to re-read it from.
    fallback_repo: ConfigFile,
    /// The global and repo files as read from disk for this plan, unedited.
    /// The write is refused unless both still read the same.
    baseline: FileLayers,
    /// The edited target file, already validated.
    file: ConfigFile,

    pub preview: ImageSourcePreview,
}

impl ImageSourcePlan {
    /// The edit this plan writes (after any builder shadowing was applied).
    pub fn edit(&self) -> &ImageSourceEdit {
        &self.edit
    }

    /// The file this plan writes.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Why a source cannot be planned. Nothing is written for any of these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageSourcePlanError {
    /// The spec, the edited file or the merged result failed validation.
    #[error("{0}")]
    Invalid(String),
    /// Repo scope without a Git repository.
    #[error("the repo config needs a Git repository")]
    NoRepo,
    /// The write would not become the effective source of the subject.
    #[error(
        "{field} in the {scope} config would not take effect: the {winner} config overrides it"
    )]
    Shadowed {
        field: String,
        scope: &'static str,
        winner: &'static str,
    },
    /// The target file could not be read.
    #[error("{0}")]
    Io(String),
}

/// Validate `edit`, compute its destination, and preview its effect on every
/// image in `subjects` through the real merge. `subjects[0]` is the image the
/// edit is for. Reads the target file from disk; writes nothing.
pub fn plan_image_source_edit(
    config: &EffectiveConfig,
    git_root: Option<&Path>,
    edit: ImageSourceEdit,
    subjects: &[PreviewSubject],
) -> Result<ImageSourcePlan, ImageSourcePlanError> {
    edit.spec.validate().map_err(|reason| {
        ImageSourcePlanError::Invalid(format!("{}: {reason}", edit.slot.field()))
    })?;
    let subject = subjects
        .first()
        .ok_or_else(|| ImageSourcePlanError::Invalid("no image to configure".into()))?;
    let mut edit = edit;
    let mut notes = Vec::new();
    let mut simulation = simulate(config, git_root, &edit)?;
    check_effective(&simulation.simulated, &edit, subject)?;
    let builder = if edit.enable_builder {
        if simulation.simulated.builtin_runtime().builder_enabled() {
            BuilderPreview::Enabled
        } else {
            let by = simulation
                .simulated
                .source_of(BUILDER_ENABLED_FIELD)
                .as_str()
                .to_string();
            edit.enable_builder = false;
            simulation = simulate(config, git_root, &edit)?;
            check_effective(&simulation.simulated, &edit, subject)?;
            notes.push(format!(
                "The builder was not enabled: the {by} config sets {BUILDER_ENABLED_FIELD} to false."
            ));
            BuilderPreview::Shadowed { by }
        }
    } else {
        BuilderPreview::Unchanged {
            effective: simulation.simulated.builtin_runtime().builder_enabled(),
        }
    };
    let effective = subjects
        .iter()
        .map(|s| EffectiveImageSource {
            agent: s.agent.clone(),
            tag: s.tag.clone(),
            source: match simulation
                .simulated
                .image_source_status(&s.tag, s.is_default)
            {
                ImageSourceStatus::Configured { slot, layer, spec } => Some(EffectiveSource {
                    kind: spec.kind(),
                    json: serde_json::to_string(&spec).unwrap_or_default(),
                    field: slot.field(),
                    scope: layer.as_str().to_string(),
                    from_this_write: slot == edit.slot && layer == edit.scope.layer(),
                }),
                _ => None,
            },
        })
        .collect();
    let preview = ImageSourcePreview {
        scope: edit.scope,
        path: simulation.path.clone(),
        field: edit.slot.field(),
        fragment: fragment(&edit),
        effective,
        builder,
        notes,
    };
    Ok(ImageSourcePlan {
        edit,
        path: simulation.path,
        kind: simulation.kind,
        git_root: git_root.map(Path::to_path_buf),
        global_path: simulation.global_path,
        fallback_repo: config.layers().repo.clone(),
        baseline: simulation.baseline,
        file: simulation.file,
        preview,
    })
}

/// The subject's simulated status must be exactly this write.
fn check_effective(
    simulated: &EffectiveConfig,
    edit: &ImageSourceEdit,
    subject: &PreviewSubject,
) -> Result<(), ImageSourcePlanError> {
    match simulated.image_source_status(&subject.tag, subject.is_default) {
        ImageSourceStatus::Configured { slot, layer, spec }
            if slot == edit.slot && layer == edit.scope.layer() && spec == edit.spec =>
        {
            Ok(())
        }
        status => Err(ImageSourcePlanError::Shadowed {
            field: edit.slot.field(),
            scope: edit.scope.as_str(),
            winner: winner(simulated, edit, &status).as_str(),
        }),
    }
}

/// The layer that keeps `edit` from taking effect, given the simulated
/// `status` of its subject. An `images` entry lost because a closer file's
/// map replaced the whole map names that file, not the fallback source.
fn winner(
    simulated: &EffectiveConfig,
    edit: &ImageSourceEdit,
    status: &ImageSourceStatus,
) -> Layer {
    let images_layer = simulated.source_of(IMAGES_FIELD);
    match (&edit.slot, status) {
        (ImageSourceSlot::Image(_), _) if images_layer != edit.scope.layer() => images_layer,
        (_, ImageSourceStatus::Configured { layer, .. })
        | (_, ImageSourceStatus::DefaultPinned { layer, .. }) => *layer,
        (_, ImageSourceStatus::Unconfigured) => Layer::Default,
    }
}

/// The exact JSON this edit sets, nested from the document root.
fn fragment(edit: &ImageSourceEdit) -> String {
    let spec = serde_json::to_value(&edit.spec).unwrap_or_default();
    let mut builtin = serde_json::Map::new();
    if edit.enable_builder {
        builtin.insert("builder".into(), serde_json::json!({ "enabled": true }));
    }
    match &edit.slot {
        ImageSourceSlot::Default => {
            builtin.insert("imageSource".into(), spec);
        }
        ImageSourceSlot::Image(tag) => {
            let mut images = serde_json::Map::new();
            images.insert(tag.clone(), spec);
            builtin.insert("images".into(), serde_json::Value::Object(images));
        }
    }
    let doc = serde_json::json!({ "runtime": { "builtin": serde_json::Value::Object(builtin) } });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// The destination of `scope`.
fn destination(
    config: &EffectiveConfig,
    git_root: Option<&Path>,
    scope: ImageSourceScope,
) -> Result<PathBuf, ImageSourcePlanError> {
    match scope {
        ImageSourceScope::Global => {
            global_config_path(config.env()).map_err(|e| ImageSourcePlanError::Io(e.to_string()))
        }
        ImageSourceScope::Repo => git_root
            .map(repo_config_path)
            .ok_or(ImageSourcePlanError::NoRepo),
    }
}

/// Read the target file from disk now (an edit made since the session opened
/// is not clobbered). Repo files refuse symlinks below the Git root.
fn load_target(
    path: &Path,
    kind: FileKind,
    git_root: Option<&Path>,
) -> Result<ConfigFile, ImageSourcePlanError> {
    if kind == FileKind::Repo {
        let root = git_root.ok_or(ImageSourcePlanError::NoRepo)?;
        crate::data::fs::config_file::refuse_symlinks_below(root, path)
            .map_err(|e| ImageSourcePlanError::Invalid(e.to_string()))?;
    }
    ConfigFile::load(path, kind).map_err(|e| match e {
        DataError::Io { .. } => ImageSourcePlanError::Io(e.to_string()),
        other => ImageSourcePlanError::Invalid(other.to_string()),
    })
}

/// The typed edit on one file. Nothing else in the file is touched.
fn apply_edit(file: &mut ConfigFile, edit: &ImageSourceEdit) {
    let builtin = file
        .runtime
        .get_or_insert_with(Default::default)
        .builtin
        .get_or_insert_with(Default::default);
    match &edit.slot {
        ImageSourceSlot::Default => builtin.image_source = Some(edit.spec.clone()),
        ImageSourceSlot::Image(tag) => {
            builtin.images.insert(tag.clone(), edit.spec.clone());
        }
    }
    if edit.enable_builder {
        builtin
            .builder
            .get_or_insert_with(BuiltinBuilderConfig::default)
            .enabled = Some(true);
    }
}

/// The two file layers a source choice depends on, as read from disk.
#[derive(Debug, Clone, PartialEq)]
struct FileLayers {
    global: ConfigFile,
    repo: ConfigFile,
}

/// The global file and (with a Git root) the repo file as they are on disk
/// now. The session snapshot may be stale: another process may have added a
/// closer source or a builder override since the session opened, and the
/// preview must describe what this run will actually resolve.
fn read_file_layers(
    global_path: &Path,
    git_root: Option<&Path>,
    fallback_repo: &ConfigFile,
) -> Result<FileLayers, ImageSourcePlanError> {
    let global = load_target(global_path, FileKind::Global, git_root)?;
    let repo = match git_root {
        Some(root) => load_target(&repo_config_path(root), FileKind::Repo, git_root)?,
        None => fallback_repo.clone(),
    };
    Ok(FileLayers { global, repo })
}

/// The result of simulating one edit.
struct Simulation {
    path: PathBuf,
    kind: FileKind,
    global_path: PathBuf,
    /// Both file layers before the edit.
    baseline: FileLayers,
    /// The edited, validated target file.
    file: ConfigFile,
    /// The effective config after the write, through the real merge.
    simulated: EffectiveConfig,
}

/// Destination, edited and validated file, and the effective config after
/// the write, through the real merge over the file layers as they are on
/// disk now.
fn simulate(
    config: &EffectiveConfig,
    git_root: Option<&Path>,
    edit: &ImageSourceEdit,
) -> Result<Simulation, ImageSourcePlanError> {
    let path = destination(config, git_root, edit.scope)?;
    let kind = edit.scope.file_kind();
    let global_path = destination(config, git_root, ImageSourceScope::Global)?;
    let baseline = read_file_layers(&global_path, git_root, &config.layers().repo)?;
    let mut file = match edit.scope {
        ImageSourceScope::Global => baseline.global.clone(),
        ImageSourceScope::Repo => baseline.repo.clone(),
    };
    apply_edit(&mut file, edit);
    // Exactly the validation `awman config set` applies before a write.
    let file = ConfigFile::from_value(file.to_value(), kind)
        .map_err(|e| ImageSourcePlanError::Invalid(e.to_string()))?;
    let mut layers = config.layers().clone();
    layers.global = baseline.global.clone();
    layers.repo = baseline.repo.clone();
    match edit.scope {
        ImageSourceScope::Global => layers.global = file.clone(),
        ImageSourceScope::Repo => layers.repo = file.clone(),
    }
    let simulated = EffectiveConfig::resolve(layers, config.flags().clone(), config.env().clone())
        .map_err(|e| ImageSourcePlanError::Invalid(e.to_string()))?;
    simulated
        .builtin_settings()
        .map_err(|e| ImageSourcePlanError::Invalid(e.to_string()))?;
    Ok(Simulation {
        path,
        kind,
        global_path,
        baseline,
        file,
        simulated,
    })
}

/// Write the planned edit atomically. Re-reads both file layers and refuses
/// if either changed since the plan was previewed (including a concurrent
/// write of the same field); nothing is written then. The remaining window is
/// between this re-read and the rename.
pub fn apply_image_source_plan(plan: &ImageSourcePlan) -> Result<(), DataError> {
    let current = read_file_layers(
        &plan.global_path,
        plan.git_root.as_deref(),
        &plan.fallback_repo,
    )
    .map_err(|e| DataError::Other(e.to_string()))?;
    if current != plan.baseline {
        let changed = if current.global != plan.baseline.global {
            plan.global_path.clone()
        } else {
            plan.git_root
                .as_deref()
                .map(repo_config_path)
                .unwrap_or_else(|| plan.path.clone())
        };
        return Err(DataError::Other(format!(
            "{} changed while the image source was being chosen; nothing was written",
            changed.display()
        )));
    }
    plan.file.save(&plan.path, plan.kind)
}

/// An image without a usable source, for [`missing_image_source_message`].
#[derive(Debug, Clone, Copy)]
pub struct MissingImageSource<'a> {
    pub tag: &'a str,
    pub agent: &'a str,
    /// `Unconfigured` or `DefaultPinned`.
    pub reason: &'a ImageSourceStatus,
    pub repo_config: Option<&'a Path>,
    pub global_config: &'a Path,
}

const ACCEPTED_SOURCES: &str = "{\"type\":\"docker-local\"}, {\"type\":\"apple-local\"} (macOS), \
{\"type\":\"archive\",\"path\":\"<file>\"} or \
{\"type\":\"registry\",\"registry\":\"<host>\",\"reference\":\"<repo>:<tag>\"}";

/// The single-line error for an image without a source: names the tag, the
/// config keys and files that fix it, and the accepted values. Its lead is
/// the engine's `no image source is configured for '<tag>'` wording.
pub fn missing_image_source_message(missing: &MissingImageSource<'_>) -> String {
    let files = match missing.repo_config {
        Some(repo) => format!("{} or {}", repo.display(), missing.global_config.display()),
        None => missing.global_config.display().to_string(),
    };
    let tag = missing.tag;
    let lead = format!(
        "no image source is configured for '{tag}' (agent {})",
        missing.agent
    );
    let entry = images_entry_field(tag);
    match missing.reason {
        ImageSourceStatus::DefaultPinned { spec, .. } => format!(
            "{lead}: `{IMAGE_SOURCE_FIELD}` ({}) names a single image and cannot provide it. \
             Set `{entry}` in {files} to one of {ACCEPTED_SOURCES}, then run `awman ready --build` \
             again. Run `awman ready --build` in a terminal to choose a source interactively.",
            spec.kind()
        ),
        _ => format!(
            "{lead}: the builtin runtime imports images only from an explicit source. \
             Set `{IMAGE_SOURCE_FIELD}` (default for every agent image) or `{entry}` (this image \
             only) in {files} to one of {ACCEPTED_SOURCES}, then run `awman ready` again. \
             To let awman build the image into a docker-local or apple-local source, also set \
             `{BUILDER_ENABLED_FIELD}` to true. Run `awman ready` in a terminal to choose a \
             source interactively."
        ),
    }
}

/// [`missing_image_source_message`] for `tag` in `session`, or `None` when
/// configuration gives it a source (however that source fails when
/// contacted). `is_default_agent` is whether a pinned default may serve
/// `tag`. Callers decide whether their runtime imports images at all.
pub fn missing_image_source_guidance(
    session: &crate::data::session::Session,
    tag: &str,
    agent: &str,
    is_default_agent: bool,
) -> Option<String> {
    let status = session
        .effective_config()
        .image_source_status(tag, is_default_agent);
    if !status.needs_source() {
        return None;
    }
    let global = global_config_path(session.env()).ok()?;
    let repo = session
        .is_git_repo()
        .then(|| repo_config_path(session.git_root()));
    Some(missing_image_source_message(&MissingImageSource {
        tag,
        agent,
        reason: &status,
        repo_config: repo.as_deref(),
        global_config: &global,
    }))
}

#[cfg(test)]
#[path = "image_source_setup_tests.rs"]
mod tests;
