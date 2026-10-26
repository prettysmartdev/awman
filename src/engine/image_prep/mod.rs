//! Session-scoped, source-matched image preparation for importing runtimes.
//! Builds and imports have separate outcomes. The policy is supplied by the
//! caller; source selection, receipts and locks are owned here.
use crate::data::config::image_source::ImageSourceKind;
use crate::data::config::{env::Env, resolved_builtin::ResolvedBuiltinConfig};
use crate::data::image_build_lock::{BuildStoreKey, ImageBuildLease, ImageBuildLockStore};
use crate::data::image_build_state::{PendingBuildKey, PendingBuiltImage, PendingBuiltImageStore};
use crate::data::oci_identity::BuiltImageIdentity;
use crate::data::ready_image::{
    ReadyBaseImageResult, ReadyImagePlan, ReadyImageResult, ReadyImageWarning,
};
use crate::data::setup_step::{ReadyStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ImageImportRequest};
use crate::engine::error::EngineError;
use crate::engine::oci::resolve::BuilderSelection;
use crate::engine::oci::{BuildHint, ImageSourceResolver, ImageSources, ResolvedImageSource};
use build::BuildPreparation;
use builder::{BuildRequest, HostLocalImageBuilder, LocalImageBuilder};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

mod bridge;
mod build;
pub mod builder;
pub(crate) mod helpers;
pub use bridge::ImageImporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildPolicy {
    Ready {
        build: bool,
        no_cache: bool,
        refresh: bool,
    },
    SessionSetup,
    ImportOnly,
    CheckOnly,
}
impl BuildPolicy {
    fn allows_build(self) -> bool {
        matches!(self, Self::Ready { .. } | Self::SessionSetup)
    }
    fn force_build(self) -> bool {
        matches!(self, Self::Ready { build: true, .. })
    }
    fn no_cache(self) -> bool {
        matches!(self, Self::Ready { no_cache: true, .. })
    }
    // WI 0125: --refresh refreshes templates, not image acquisition. Only
    // --build/--no-cache force acquisition when the builder is disabled.
    fn refresh_import(self) -> bool {
        self.force_build() || self.no_cache()
    }
    pub(crate) fn audit_rebuild(self) -> Self {
        match self {
            Self::Ready {
                no_cache, refresh, ..
            } => Self::Ready {
                build: true,
                no_cache,
                refresh,
            },
            other => other,
        }
    }
}

pub struct AgentImageRequest<'a> {
    pub tag: &'a str,
    pub agent: &'a str,
    pub policy: BuildPolicy,
}

/// The one Dockerfile rule for builds, provenance hints and check-only errors.
#[derive(Debug, Clone)]
pub struct ImageBuildContext {
    pub git_root: PathBuf,
    pub project_dockerfile: PathBuf,
    pub default_agent: String,
}
impl ImageBuildContext {
    pub fn for_session(session: &crate::data::session::Session, default_agent: &str) -> Self {
        Self::for_repo(session.git_root(), session.repo_config(), default_agent)
    }
    pub fn for_repo(
        git_root: &std::path::Path,
        config: &crate::data::config::RepoConfig,
        default_agent: &str,
    ) -> Self {
        Self {
            git_root: git_root.to_path_buf(),
            project_dockerfile: config.dockerfile_path_or_default(git_root),
            default_agent: default_agent.to_string(),
        }
    }
    pub fn hint(&self, agent: &str) -> BuildHint {
        BuildHint {
            agent_dockerfile: crate::data::RepoDockerfilePaths::new(&self.git_root)
                .agent_dockerfile(agent),
            project: Some((
                self.project_dockerfile.clone(),
                crate::data::image_tags::project_image_tag(&self.git_root),
            )),
            context: self.git_root.clone(),
        }
    }
}

pub trait ImagePreparationSink: crate::data::message::UserMessageSink {
    fn report_step(&mut self, step: &SetupStep, status: StepStatus);
}
impl<T: crate::engine::agent::AgentImageFrontend + ?Sized> ImagePreparationSink for T {
    fn report_step(&mut self, step: &SetupStep, status: StepStatus) {
        self.report_step_status(step, status);
    }
}
/// Message-only callers do not expose ready's phase or execution frontend.
pub struct MessageSink<'a>(pub &'a mut dyn crate::data::message::UserMessageSink);
impl crate::data::message::UserMessageSink for MessageSink<'_> {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.0.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.0.replay_queued();
    }
}
impl ImagePreparationSink for MessageSink<'_> {
    fn report_step(&mut self, _: &SetupStep, _: StepStatus) {}
}

#[derive(Clone)]
struct ImagePlan {
    source: Option<ResolvedImageSource>,
    // Keep display metadata even if freezing the endpoint fails. Layer 0
    // has already validated these source specifications.
    source_description: Option<(ImageSourceKind, String)>,
    selection: BuilderSelection,
    error: Option<String>,
    pinned_default: Option<String>,
}

pub struct AgentImagePreparer {
    definition_env: crate::data::config::env::EnvSnapshot,
    definitions: HashMap<String, crate::engine::agent::ResolvedAgent>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    context: ImageBuildContext,
    image_sources: ImageSources,
    builder_enabled: bool,
    local_builder: Option<Arc<dyn LocalImageBuilder>>,
    image_importer: Arc<dyn ImageImporter>,
    build_locks: ImageBuildLockStore,
    pending_builds: PendingBuiltImageStore,
    prepared_bases: HashMap<(BuildStoreKey, String), Result<(), String>>,
    base_results: Vec<ReadyBaseImageResult>,
    prevalidation_failures: HashMap<String, String>,
    validated_policy: Option<BuildPolicy>,
    plans: HashMap<(String, String), ImagePlan>,
    warned_tags: HashSet<String>,
    cancel: crate::engine::oci::CancelToken,
}
impl AgentImagePreparer {
    pub fn new(
        resolved: ResolvedBuiltinConfig,
        runtime: Arc<dyn AgentRuntimeEngine>,
        data_home: PathBuf,
        context: ImageBuildContext,
    ) -> Self {
        Self::with_inputs(
            ImageSources::from_resolved(&resolved),
            resolved.builder_enabled,
            runtime,
            data_home,
            context,
        )
    }
    fn with_inputs(
        image_sources: ImageSources,
        builder_enabled: bool,
        runtime: Arc<dyn AgentRuntimeEngine>,
        data_home: PathBuf,
        context: ImageBuildContext,
    ) -> Self {
        Self {
            definition_env: Env::from_process(),
            definitions: HashMap::new(),
            image_sources,
            builder_enabled,
            image_importer: Arc::new(bridge::RuntimeImageImporter(runtime.clone())),
            runtime,
            context,
            local_builder: None,
            build_locks: ImageBuildLockStore::under_data_home(&data_home),
            pending_builds: PendingBuiltImageStore::under_data_home(&data_home),
            prepared_bases: HashMap::new(),
            base_results: Vec::new(),
            prevalidation_failures: HashMap::new(),
            validated_policy: None,
            plans: HashMap::new(),
            warned_tags: HashSet::new(),
            cancel: crate::engine::oci::CancelToken::new(),
        }
    }
    pub fn with_definition_env(mut self, env: crate::data::config::env::EnvSnapshot) -> Self {
        self.definition_env = env;
        self
    }
    pub fn with_definition(mut self, def: crate::engine::agent::ResolvedAgent) -> Self {
        self.set_definition(def);
        self
    }
    pub(crate) fn set_definition(&mut self, def: crate::engine::agent::ResolvedAgent) {
        self.definitions.insert(def.name.clone(), def);
    }
    /// A preparer for a runtime that builds its own images (Docker, Apple
    /// Containers). It owns only the Dockerfile-path rule and the build
    /// bookkeeping; it has no image sources and no builder, because builtin
    /// configuration does not apply to that runtime and is never read.
    pub fn for_building_runtime(
        runtime: Arc<dyn AgentRuntimeEngine>,
        data_home: PathBuf,
        context: ImageBuildContext,
    ) -> Self {
        Self::with_inputs(ImageSources::default(), false, runtime, data_home, context)
    }
    #[cfg(test)]
    pub(crate) fn replace_sources(&mut self, sources: ImageSources) {
        self.image_sources = sources;
        self.plans.clear();
        self.validated_policy = None;
    }
    #[cfg(test)]
    pub(crate) fn image_sources(&self) -> &ImageSources {
        &self.image_sources
    }
    #[cfg(test)]
    pub(crate) fn with_build_stores(
        mut self,
        locks: ImageBuildLockStore,
        receipts: PendingBuiltImageStore,
    ) -> Self {
        self.build_locks = locks;
        self.pending_builds = receipts;
        self
    }
    #[cfg(test)]
    pub(crate) fn with_local_builder(mut self, builder: Arc<dyn LocalImageBuilder>) -> Self {
        self.local_builder = Some(builder);
        self
    }
    #[cfg(test)]
    pub(crate) fn with_image_importer(mut self, importer: Arc<dyn ImageImporter>) -> Self {
        self.image_importer = importer;
        self
    }
    pub fn cancel_token(&self) -> crate::engine::oci::CancelToken {
        self.cancel.clone()
    }
    pub fn take_base_results(&mut self) -> Vec<ReadyBaseImageResult> {
        std::mem::take(&mut self.base_results)
    }
    pub(crate) fn reset_bases(&mut self) {
        self.prepared_bases.clear();
        self.validated_policy = None;
    }
    pub(crate) fn project_dockerfile(&self) -> &std::path::Path {
        &self.context.project_dockerfile
    }
    fn builder(&mut self) -> Arc<dyn LocalImageBuilder> {
        self.local_builder
            .get_or_insert_with(|| Arc::new(HostLocalImageBuilder::default()))
            .clone()
    }
    fn plan(&mut self, tag: &str, agent: &str) -> ImagePlan {
        self.plans
            .entry((tag.to_string(), agent.to_string()))
            .or_insert_with(|| {
                let source_description = self
                    .image_sources
                    .images
                    .get(tag)
                    .or(self.image_sources.default.as_ref())
                    .map(|spec| (spec.kind(), spec.reference_or(tag)));
                // The same acquisition env the backend pulls with
                // (`BuiltinBackend` import, `Env::for_image_acquisition`): source
                // resolution and the pull must see one Docker endpoint, proxy and
                // credential set, so neither reads the session snapshot alone.
                let env = Env::for_image_acquisition(&self.image_sources.registries);
                let resolved = ImageSourceResolver::new(&self.image_sources, &env)
                    .with_builder_enabled(self.builder_enabled)
                    .resolve_for_image(
                        tag,
                        &self.context.hint(agent),
                        crate::data::oci_identity::OciPlatform::host_linux(),
                    );
                let error = match &resolved {
                    Err(EngineError::ImageSourceUnconfigured { .. }) => None,
                    Err(e) => Some(e.to_string()),
                    _ => None,
                };
                let source = resolved.ok();
                let selection =
                    BuilderSelection::from_source(source.as_ref(), self.builder_enabled);
                let pinned_default = if agent != self.context.default_agent
                    && !self.image_sources.images.contains_key(tag)
                {
                    self.image_sources.default.as_ref().and_then(|spec| {
                        let single_image = matches!(
                            spec,
                            crate::data::config::image_source::ImageSourceSpec::Archive { .. }
                        ) || spec.reference_or(tag) != tag;
                        if single_image {
                            Some(format!(
                                "`builtin.imageSource` ({}) names a single image; add a \
                                 `builtin.images` entry for {tag} to import it",
                                spec.kind()
                            ))
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                ImagePlan {
                    source,
                    source_description,
                    selection,
                    error,
                    pinned_default,
                }
            })
            .clone()
    }
    /// What `awman ready --build` would do for `tag` if it is missing,
    /// from this session's source and builder selection for that image.
    /// Never probes a builder or contacts a source.
    pub fn missing_image_plan(&mut self, tag: &str, agent: &str) -> ReadyImagePlan {
        self.prevalidate_build_targets(BuildPolicy::Ready {
            build: true,
            no_cache: false,
            refresh: false,
        });
        let plan = self.plan(tag, agent);
        if plan.pinned_default.is_some() || plan.error.is_some() || plan.source.is_none() {
            return ReadyImagePlan::ConfigureSource;
        }
        match plan.selection {
            BuilderSelection::Local(_)
                if self.prevalidation_failures.contains_key(tag)
                    || plan
                        .source
                        .as_ref()
                        .is_some_and(|source| source.reference.contains('@')) =>
            {
                ReadyImagePlan::ConfigureBuildOutput
            }
            BuilderSelection::Local(_) => ReadyImagePlan::BuildAndImport,
            _ => ReadyImagePlan::Import,
        }
    }

    /// Whether a source is configured for the default agent image (the
    /// `builtin.images` entry, else `builtin.imageSource`).
    pub(crate) fn default_image_has_source(&self) -> bool {
        let tag = crate::data::image_tags::agent_image_tag(
            &self.context.git_root,
            &self.context.default_agent,
        );
        self.image_sources.default.is_some() || self.image_sources.images.contains_key(&tag)
    }
    pub(crate) fn can_rebuild_default(&mut self) -> bool {
        let agent = self.context.default_agent.clone();
        let tag = crate::data::image_tags::agent_image_tag(&self.context.git_root, &agent);
        let plan = self.plan(&tag, &agent);
        self.default_image_has_source() && !matches!(plan.selection, BuilderSelection::Disabled)
    }

    /// Check all outputs before the first write, using the same cached plans
    /// preparation and the audit pass will consume. Never probes a builder.
    pub(crate) fn prevalidate_build_targets(&mut self, policy: BuildPolicy) {
        if !policy.allows_build() || self.validated_policy == Some(policy) {
            return;
        }
        self.validated_policy = Some(policy);
        self.prevalidation_failures.clear();
        let mut agents = vec![self.context.default_agent.clone()];
        if policy.force_build() {
            for (agent, _) in crate::data::RepoDockerfilePaths::new(&self.context.git_root)
                .discover_agent_dockerfiles()
            {
                if !agents.contains(&agent) {
                    agents.push(agent);
                }
            }
        }
        let project_tag = crate::data::image_tags::project_image_tag(&self.context.git_root);
        let project_ref = AgentImagePreparer::normalize_build_reference(&project_tag);
        let mut outputs = HashMap::<(BuildStoreKey, String), Vec<String>>::new();
        for agent in agents {
            let tag = crate::data::image_tags::agent_image_tag(&self.context.git_root, &agent);
            let plan = self.plan(&tag, &agent);
            if plan.pinned_default.is_some() {
                continue;
            }
            if let Some(error) = plan.error {
                self.prevalidation_failures.insert(tag, error);
                continue;
            }
            if !matches!(plan.selection, BuilderSelection::Local(_)) {
                continue;
            }
            let resolved = plan.source.expect("local selection has a source");
            let error = crate::engine::oci::docker_engine::validate_reference(&resolved.reference)
                .err()
                .map(|e| e.to_string())
                .or_else(|| {
                    if policy.force_build() && resolved.reference.contains('@') {
                        Some(format!(
                            "{tag}: digest-addressed image references are not writable build outputs"
                        ))
                    } else if AgentImagePreparer::normalize_build_reference(&resolved.reference) == project_ref {
                        Some(format!(
                            "{tag}: agent build output collides with project base image {project_tag}"
                        ))
                    } else {
                        None
                    }
                });
            if let Some(error) = error {
                self.prevalidation_failures.insert(tag, error);
                continue;
            }
            outputs
                .entry((
                    resolved.store_key.expect("local store"),
                    AgentImagePreparer::normalize_build_reference(&resolved.reference),
                ))
                .or_default()
                .push(tag);
        }
        for ((_, reference), tags) in outputs {
            if tags.len() < 2 {
                continue;
            }
            let names = tags.join(", ");
            for tag in tags {
                let error = format!(
                    "{names} target the same local image reference {reference}; \
                     give each agent its own `builtin.images` entry"
                );
                self.prevalidation_failures.insert(tag, error);
            }
        }
    }
}

mod prepare;

#[cfg(test)]
mod tests;
