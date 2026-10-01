//! `engine::ready` — `ReadyEngine`. Multi-phase state machine for `awman ready`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::data::config::env::Env;
use crate::data::config::global::GlobalConfig;
use crate::data::image_build_lock::{BuildStoreKey, ImageBuildLease, ImageBuildLockStore};
use crate::data::image_build_state::{PendingBuildKey, PendingBuiltImage, PendingBuiltImageStore};
use crate::data::oci_identity::BuiltImageIdentity;
use crate::data::ready_image::{ReadyBaseImageResult, ReadyImageResult, ReadyImageWarning};
use crate::data::session::{AgentName, Session};
use crate::data::setup_step::{ReadyStep, SetupStep};
use crate::data::step_status::StepStatus;
use crate::engine::agent::AgentEngine;
use crate::engine::agent_runtime::{
    AgentRuntimeEngine, ImageAcquisition, ImageImportRequest, ReadyAgentOptions,
    ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::oci::{resolve_source, BuildHint, ImageSources};
use crate::engine::overlay::OverlayEngine;
use crate::engine::ready::builder::{
    BuilderSelection, HostLocalImageBuilder, LocalBuilderTarget, LocalImageBuildRequest,
    LocalImageBuilder,
};

mod build;
pub mod builder;
pub mod frontend;
pub mod host_agent;
mod import;

pub use crate::data::ready_phase::{ReadyFailure, ReadyPhase};
// Re-exported at the module root so the many call sites that name the ping
// result or the greeting table keep their import path; the host execution
// itself is reachable only through `HostAgentPinger`.
pub use crate::data::ready_summary::{ReadyImageAction, ReadySummary};
pub use frontend::ReadyFrontend;
pub use host_agent::{
    select_random_greeting, HostAgentPinger, HostRefreshOutcome, LocalAgentPingResult,
    ReadyHostPing, GREETINGS,
};
pub use import::ImageImporter;

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
    /// Where an importing runtime acquires images (`builtin.imageSource` /
    /// `builtin.images`) from this session's effective config.
    image_sources: ImageSources,
    builder_enabled: bool,
    local_builder: Arc<dyn LocalImageBuilder>,
    image_importer: Arc<dyn ImageImporter>,
    build_locks: Option<ImageBuildLockStore>,
    pending_builds: Option<PendingBuiltImageStore>,
    prepared_bases: HashMap<String, Result<(), String>>,
    /// Per-tag build-target problems found before any build; each fails
    /// only its own agent.
    prevalidation_failures: HashMap<String, String>,
    cancel: crate::engine::oci::CancelToken,
}

struct BuildPreparation {
    /// The content the import must install, when it is known: the build's
    /// receipt, or the identity the source store reports for the reference.
    expected: Option<BuiltImageIdentity>,
    force_refresh: bool,
    pending_key: Option<PendingBuildKey>,
    // The base and agent references cannot be rebuilt or retagged by
    // another awman process until the resulting archive has been imported
    // and the receipt settled.
    _leases: Vec<ImageBuildLease>,
    status: StepStatus,
    warning: Option<ReadyImageWarning>,
    /// The build succeeded but the import must not run (cancelled, or its
    /// receipt could not be recorded): the import row fails with this.
    blocked: Option<String>,
}

impl BuildPreparation {
    /// Nothing to build and nothing to bind: import exactly as with the
    /// builder disabled.
    fn import_only(status: StepStatus, warning: Option<ReadyImageWarning>) -> Self {
        Self {
            expected: None,
            force_refresh: false,
            pending_key: None,
            _leases: Vec::new(),
            status,
            warning,
            blocked: None,
        }
    }
}

impl ReadyEngine {
    /// Check every writable output in this ready request before the first
    /// builder probe or build. A problem fails only the agents it concerns
    /// (a failure is a row, not an abort): a fixed default reference already
    /// skipped for other agents, a resolver error, an unwritable reference,
    /// or two agents (or an agent and the project base) targeting the same
    /// local image. Nothing is built for a failed agent, so no conflict can
    /// overwrite another agent's output.
    fn prevalidate_build_targets(&mut self, git_root: &std::path::Path) {
        use crate::data::image_tags::{agent_image_tag, project_image_tag};
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        self.prevalidation_failures.clear();
        let default_agent = self.options.agent.as_str().to_string();
        let mut agents = vec![default_agent.clone()];
        if self.options.build {
            for (agent, _) in RepoDockerfilePaths::new(git_root).discover_agent_dockerfiles() {
                if !agents.contains(&agent) {
                    agents.push(agent);
                }
            }
        }
        let effective = self.session.effective_config();
        let env = Env::for_image_acquisition(&effective.builtin_runtime().registries);
        let resolver = crate::engine::oci::ImageSourceResolver::new(&self.image_sources, &env);
        let project_tag = project_image_tag(git_root);
        let project_ref = normalize_build_reference(&project_tag);
        let mut outputs = HashMap::<(String, String), Vec<String>>::new();
        let mut failures = HashMap::<String, String>::new();
        for agent in agents {
            let tag = agent_image_tag(git_root, &agent);
            // The import loop skips these with a warning; they are never
            // built, so they cannot conflict.
            if agent != default_agent && self.single_image_default_source(&tag).is_some() {
                continue;
            }
            let hint = BuildHint {
                agent_dockerfile: RepoDockerfilePaths::new(git_root).agent_dockerfile(&agent),
                project: Some((
                    self.session
                        .repo_config()
                        .dockerfile_path_or_default(git_root),
                    project_tag.clone(),
                )),
                context: git_root.to_path_buf(),
            };
            let resolved = match resolver.resolve_for_image(
                &tag,
                &hint,
                crate::data::oci_identity::OciPlatform::host_linux(),
            ) {
                Ok(r) => r,
                Err(EngineError::ImageSourceUnconfigured { .. }) => continue,
                Err(e) => {
                    failures.insert(tag, e.to_string());
                    continue;
                }
            };
            if !matches!(
                BuilderSelection::from_source(Some(&resolved)),
                BuilderSelection::Local(_)
            ) {
                continue;
            }
            if let Err(e) =
                crate::engine::oci::docker_engine::validate_reference(&resolved.reference)
            {
                failures.insert(tag, e.to_string());
                continue;
            }
            // Without `--build` a build happens only if the image is
            // missing; `prepare_local_image` rejects a digest then.
            if self.options.build && resolved.reference.contains('@') {
                failures.insert(
                    tag.clone(),
                    format!(
                        "{tag}: digest-addressed image references are not writable build outputs"
                    ),
                );
                continue;
            }
            let normalized = normalize_build_reference(&resolved.reference);
            if normalized == project_ref {
                failures.insert(
                    tag.clone(),
                    format!(
                        "{tag}: agent build output collides with project base image {project_tag}"
                    ),
                );
                continue;
            }
            outputs
                .entry((resolved.store, normalized))
                .or_default()
                .push(tag);
        }
        for ((_, reference), tags) in outputs {
            if tags.len() < 2 {
                continue;
            }
            let names = tags.join(", ");
            for tag in tags {
                failures.insert(
                    tag,
                    format!(
                        "{names} target the same local image reference {reference}; give each \
                         agent its own `builtin.images` entry"
                    ),
                );
            }
        }
        self.prevalidation_failures = failures;
    }

    async fn build_lock(
        &self,
        store: BuildStoreKey,
        reference: String,
    ) -> Result<ImageBuildLease, String> {
        let locks = if let Some(locks) = &self.build_locks {
            locks.clone()
        } else {
            ImageBuildLockStore::under_data_home(&data_home()?)
        };
        let cancel = self.cancel.clone();
        build::blocking(self.cancel.clone(), move || {
            locks.lock(&store, &reference, || {
                if cancel.is_cancelled() {
                    Err(crate::data::error::DataError::Other(
                        "image build cancelled".into(),
                    ))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    /// Build receipts live beside the build locks, in the process's data
    /// home, not in a session's `builtin.stateDir`: the builtin runtime's
    /// state directory is process-wide, so a repo-level `stateDir` must not
    /// move the receipts away from the cache they protect. A receipt only
    /// ever narrows what may be imported, so sharing it is safe.
    fn pending_store(&self) -> Result<PendingBuiltImageStore, String> {
        if let Some(store) = &self.pending_builds {
            return Ok(store.clone());
        }
        Ok(PendingBuiltImageStore::under_data_home(&data_home()?))
    }

    async fn load_receipt(
        &self,
        key: &PendingBuildKey,
    ) -> Result<Option<PendingBuiltImage>, String> {
        let store = self.pending_store()?;
        let key = key.clone();
        build::blocking(self.cancel.clone(), move || store.load(&key))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    async fn save_receipt(&self, receipt: PendingBuiltImage) -> Result<(), String> {
        let store = self.pending_store()?;
        let cancel = self.cancel.clone();
        build::blocking(self.cancel.clone(), move || {
            store.save(&receipt, || {
                if cancel.is_cancelled() {
                    Err(crate::data::error::DataError::Other(
                        "image build cancelled".into(),
                    ))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    /// Remove the receipt for `key` only if it still describes `installed`
    /// (compare-and-delete). Callers hold the agent's build lease, which
    /// every receipt writer also holds, so no newer receipt can be lost.
    async fn settle_receipt(
        &self,
        key: &PendingBuildKey,
        installed: &crate::data::oci_identity::ImageIdentity,
    ) -> Result<(), String> {
        let store = self.pending_store()?;
        let key = key.clone();
        let installed = installed.clone();
        build::blocking(self.cancel.clone(), move || match store.load(&key)? {
            Some(receipt) if receipt.identity.matches(&installed) => store.remove(&key),
            _ => Ok(()),
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    /// Drop a receipt whose build the source reference no longer holds.
    async fn discard_receipt(&self, key: &PendingBuildKey) -> Result<(), String> {
        let store = self.pending_store()?;
        let key = key.clone();
        build::blocking(self.cancel.clone(), move || store.remove(&key))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    async fn prepare_local_image(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        resolved: &crate::engine::oci::ResolvedImageSource,
        tag: &str,
        agent: &str,
        git_root: &std::path::Path,
    ) -> Result<BuildPreparation, String> {
        use crate::data::image_tags::project_image_tag;
        use crate::data::message::{MessageLevel, UserMessage};
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::engine::oci::SourceImage;
        let selected = BuilderSelection::from_source(Some(resolved));
        let target = match selected {
            BuilderSelection::Local(target) => target,
            BuilderSelection::Incompatible(source_kind) => {
                let attempted =
                    self.options.build || !self.runtime.image_exists(tag).unwrap_or(false);
                let warning = attempted.then(|| ReadyImageWarning::IncompatibleBuilder {
                    tag: tag.to_string(),
                    source_kind,
                    reason: "the configured source is not a local writable image store".into(),
                });
                if let Some(w) = &warning {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: w.to_string(),
                    });
                }
                return Ok(BuildPreparation::import_only(StepStatus::Skipped, warning));
            }
            BuilderSelection::Unconfigured => {
                return Ok(BuildPreparation::import_only(StepStatus::Skipped, None))
            }
        };
        crate::engine::oci::docker_engine::validate_reference(&resolved.reference)
            .map_err(|e| e.to_string())?;
        let store = match &target {
            LocalBuilderTarget::Docker { endpoint } => BuildStoreKey::Docker {
                endpoint: endpoint.clone(),
            },
            LocalBuilderTarget::Apple => BuildStoreKey::AppleLocal,
        };
        let agent_ref = normalize_build_reference(&resolved.reference);
        let project_tag = project_image_tag(git_root);
        let project_ref = normalize_build_reference(&project_tag);
        let key = PendingBuildKey {
            store: store.clone(),
            reference: agent_ref.clone(),
            platform: resolved.platform.clone(),
            install_tag: tag.to_string(),
        };
        // Leases first, base before agent in every process, held through
        // the import and the receipt update: no other awman process can
        // rebuild or retag either reference between inspection and import.
        let mut leases = vec![self.build_lock(store.clone(), project_ref.clone()).await?];
        if agent_ref != project_ref {
            leases.push(self.build_lock(store.clone(), agent_ref.clone()).await?);
        }
        let pending = self.load_receipt(&key).await?;
        if !self.options.build {
            let installed = if self.runtime.image_exists(tag).unwrap_or(false) {
                self.runtime.image_identity(tag).ok().flatten()
            } else {
                None
            };
            let inspected = build::inspect(
                self.local_builder.clone(),
                target.clone(),
                resolved.reference.clone(),
                resolved.platform.clone(),
                self.cancel.clone(),
            )
            .await;
            match inspected {
                // Nothing may need building; keep the installed image rather
                // than fail, unless an earlier build still awaits import.
                Err(e)
                    if pending.is_none()
                        && installed.is_some()
                        && !crate::engine::oci::retry::is_cancelled(&e) =>
                {
                    let note = format!(
                        "{} is unreachable; using the installed image ({e})",
                        target.describe()
                    );
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!("{tag}: {note}"),
                    });
                    return Ok(BuildPreparation::import_only(StepStatus::Warn(note), None));
                }
                Err(e) => return Err(e.to_string()),
                // Importable as the builder-disabled path imports it; the
                // store cannot vouch for one platform manifest.
                Ok(SourceImage::Unbound) => {
                    if pending.is_some() {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: format!(
                                "{tag}: {} no longer exposes the image an earlier `awman ready` built; refreshing the source import",
                                resolved.reference
                            ),
                        });
                        self.discard_receipt(&key).await?;
                        return Ok(BuildPreparation {
                            expected: None,
                            force_refresh: true,
                            pending_key: Some(key),
                            _leases: leases,
                            status: StepStatus::Skipped,
                            warning: None,
                            blocked: None,
                        });
                    }
                    let mut prepared = BuildPreparation::import_only(StepStatus::Skipped, None);
                    prepared._leases = leases;
                    return Ok(prepared);
                }
                Ok(SourceImage::Present(existing)) => {
                    if let Some(receipt) = &pending {
                        if receipt.identity != existing {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Warning,
                                text: format!(
                                    "{tag}: {} no longer holds the image an earlier `awman \
                                     ready` built for it; importing the image it holds now \
                                     (run `awman ready --build` to rebuild)",
                                    resolved.reference
                                ),
                            });
                            self.discard_receipt(&key).await?;
                        }
                    }
                    // Already installed and nothing pending: the ordinary
                    // reuse path, without re-exporting the image.
                    let current = pending.is_none()
                        && installed.as_ref().is_some_and(|i| existing.matches(i));
                    return Ok(BuildPreparation {
                        expected: (!current).then_some(existing),
                        force_refresh: !current,
                        pending_key: Some(key),
                        _leases: leases,
                        status: StepStatus::Skipped,
                        warning: None,
                        blocked: None,
                    });
                }
                Ok(SourceImage::Absent) => {}
            }
        }
        if resolved.reference.contains('@') {
            return Err(format!(
                "{tag}: digest-addressed image references are not writable build outputs"
            ));
        }
        if agent_ref == project_ref {
            return Err(format!(
                "{tag}: agent build output collides with project base image {project_tag}"
            ));
        }
        let base_key = format!("{}\n{}", store.describe(), project_ref);
        if let Some(Err(error)) = self.prepared_bases.get(&base_key) {
            return Err(error.clone());
        }
        if !self.prepared_bases.contains_key(&base_key) {
            let base_exists = if self.options.build {
                false
            } else {
                !matches!(
                    build::inspect(
                        self.local_builder.clone(),
                        target.clone(),
                        project_tag.clone(),
                        resolved.platform.clone(),
                        self.cancel.clone(),
                    )
                    .await
                    .map_err(|e| e.to_string())?,
                    SourceImage::Absent
                )
            };
            if !base_exists {
                let dockerfile = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(git_root);
                let step: SetupStep = ReadyStep::BuildBaseImageIn {
                    store: store.describe(),
                    tag: project_tag.clone(),
                }
                .into();
                frontend.report_step_status(&step, StepStatus::Running);
                let request = LocalImageBuildRequest {
                    target: target.clone(),
                    reference: project_tag.clone(),
                    dockerfile,
                    context: git_root.to_path_buf(),
                    platform: resolved.platform.clone(),
                    build_args: Vec::new(),
                    no_cache: self.options.no_cache,
                };
                let result = build::run(
                    self.local_builder.clone(),
                    request,
                    self.cancel.clone(),
                    frontend,
                )
                .await;
                let result = match result {
                    Ok(receipt) => match build::inspect(
                        self.local_builder.clone(),
                        target.clone(),
                        project_tag.clone(),
                        resolved.platform.clone(),
                        self.cancel.clone(),
                    )
                    .await
                    {
                        Ok(SourceImage::Present(current)) if current == receipt => Ok(receipt),
                        Ok(_) => Err(EngineError::Container(format!(
                            "project base image {project_tag} changed after its build"
                        ))),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                let status = match &result {
                    Ok(_) => StepStatus::Done,
                    Err(e) => StepStatus::Failed(e.to_string()),
                };
                frontend.report_step_status(&step, status.clone());
                self.summary.base_image_results.push(ReadyBaseImageResult {
                    store: store.describe(),
                    tag: project_tag.clone(),
                    build: status,
                });
                if let Err(e) = result {
                    let message = e.to_string();
                    self.prepared_bases.insert(base_key, Err(message.clone()));
                    return Err(message);
                }
            }
            self.prepared_bases.insert(base_key, Ok(()));
        }
        let dockerfile = RepoDockerfilePaths::new(git_root).agent_dockerfile(agent);
        self.provision_agent_dockerfile(frontend, git_root, agent, &dockerfile)
            .await;
        if !dockerfile.exists() {
            return Err(format!(
                "agent Dockerfile {} is unavailable",
                dockerfile.display()
            ));
        }
        let request = LocalImageBuildRequest {
            target,
            reference: resolved.reference.clone(),
            dockerfile,
            context: git_root.to_path_buf(),
            platform: resolved.platform.clone(),
            build_args: Vec::new(),
            no_cache: self.options.no_cache,
        };
        let receipt = build::run(
            self.local_builder.clone(),
            request,
            self.cancel.clone(),
            frontend,
        )
        .await
        .map_err(|e| e.to_string())?;
        // The build succeeded. Anything that stops the import from here on
        // fails the import, not the build.
        let blocked = self
            .save_receipt(PendingBuiltImage {
                key: key.clone(),
                identity: receipt.clone(),
            })
            .await
            .err()
            .map(|e| format!("the build succeeded, but its receipt was not recorded: {e}"));
        Ok(BuildPreparation {
            expected: Some(receipt),
            force_refresh: true,
            pending_key: Some(key),
            _leases: leases,
            status: StepStatus::Done,
            warning: None,
            blocked,
        })
    }
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        runtime: Arc<dyn AgentRuntimeEngine>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self {
        let runtime_name = runtime.runtime_name().to_string();
        let effective = session.effective_config();
        let builder_enabled = effective.builtin_builder_enabled();
        let image_sources = ImageSources::from_config(&effective.builtin_runtime());
        let mut summary = ReadySummary::new(runtime_name);
        summary.image_action =
            match runtime.capabilities().image_acquisition == ImageAcquisition::Import {
                true if builder_enabled => ReadyImageAction::BuildAndImport,
                true => ReadyImageAction::Import,
                false => ReadyImageAction::Build,
            };
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
            image_sources,
            builder_enabled,
            local_builder: Arc::new(HostLocalImageBuilder::default()),
            image_importer: Arc::new(import::RuntimeImageImporter(runtime.clone())),
            build_locks: None,
            pending_builds: None,
            prepared_bases: HashMap::new(),
            prevalidation_failures: HashMap::new(),
            cancel: crate::engine::oci::CancelToken::new(),
        }
    }

    /// Replace the session-derived image sources. Test-only: production
    /// sources and builder enablement both come from the session's
    /// effective config, so they cannot disagree.
    #[cfg(test)]
    pub fn with_image_sources(mut self, sources: ImageSources) -> Self {
        self.image_sources = sources;
        self
    }

    /// Inject a recording builder for daemon-free ready tests.
    pub fn with_local_builder(mut self, builder: Arc<dyn LocalImageBuilder>) -> Self {
        self.local_builder = builder;
        self
    }

    pub fn with_image_importer(mut self, importer: Arc<dyn ImageImporter>) -> Self {
        self.image_importer = importer;
        self
    }

    pub fn with_host_ping(mut self, ping: Arc<dyn ReadyHostPing>) -> Self {
        self.host_agent = ping;
        self
    }

    /// Inject private lock and receipt roots for hermetic ready tests.
    pub fn with_build_stores(
        mut self,
        locks: ImageBuildLockStore,
        receipts: PendingBuiltImageStore,
    ) -> Self {
        self.build_locks = Some(locks);
        self.pending_builds = Some(receipts);
        self
    }

    /// Cancellation handle for an in-flight ready import. Dropping an import
    /// future also cancels it, so async deadlines work on a current-thread runtime.
    pub fn cancel_token(&self) -> crate::engine::oci::CancelToken {
        self.cancel.clone()
    }

    /// Whether the runtime imports prepared images instead of building them.
    fn imports_images(&self) -> bool {
        self.runtime.capabilities().image_acquisition == ImageAcquisition::Import
    }

    /// The phase after the Dockerfile checks: build or import.
    fn image_phase(&self) -> ReadyPhase {
        if self.imports_images() {
            ReadyPhase::ImportingBaseImage
        } else {
            ReadyPhase::BuildingBaseImage
        }
    }

    pub fn phase(&self) -> &ReadyPhase {
        &self.phase
    }

    pub fn summary(&self) -> ReadySummary {
        self.summary.clone()
    }

    /// Supply command-layer credential health before the terminal summary is
    /// rendered. The ready engine itself never reads credential contents.
    pub fn set_agent_credentials(
        &mut self,
        credentials: Vec<crate::data::ready_summary::AgentCredentialHealth>,
    ) {
        self.summary.agent_credentials = credentials;
    }

    /// Advance one phase. Drives Q&A and progress through `frontend`.
    pub async fn step(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
    ) -> Result<ReadyPhase, EngineError> {
        use crate::data::image_tags::{agent_image_tag, project_image_tag};
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::data::templates;

        self.cancel.check()?;
        frontend.report_phase(&self.phase);
        let git_root = self.session.git_root().to_path_buf();
        let _ = &self.git_engine;
        let _ = &self.overlay_engine;

        // A kit-declarative runtime prepares an agent by emitting and
        // validating a kit, not by building Docker images. The whole
        // container phase machine is skipped in one step. F-40b moved this
        // branch down here from `commands/ready.rs`: it is a paradigm
        // decision, and Layer 1 is where paradigm decisions on
        // `capabilities()` belong.
        if self.runtime.capabilities().kit_declarative
            && !matches!(self.phase, ReadyPhase::Complete)
        {
            let result = self.runtime.ready_agent(
                self.options.agent.as_str(),
                ReadyAgentOptions {
                    no_cache: self.options.no_cache,
                    build: self.options.build,
                },
                frontend,
            );
            // Every container-tier step is genuinely not applicable here, so
            // the summary reports Skipped rather than a misleading Done.
            self.summary.dockerfile = StepStatus::Skipped;
            self.summary.base_image = StepStatus::Skipped;
            self.summary.local_agent = StepStatus::Skipped;
            self.summary.audit = StepStatus::Skipped;
            self.summary.agent_image = match &result {
                Ok(()) => StepStatus::Done,
                Err(e) => StepStatus::Failed(e.to_string()),
            };
            frontend.report_step_status(
                &ReadyStep::ApplyAgentKit.into(),
                self.summary.agent_image.clone(),
            );
            self.phase = ReadyPhase::Complete;
            frontend.report_summary(&self.summary);
            result?;
            return Ok(self.phase.clone());
        }

        let next = match &self.phase {
            ReadyPhase::Preflight => {
                // Issue 23: Check aspec folder and work-items config presence.
                let aspec_dir = git_root.join("aspec");
                if aspec_dir.exists() {
                    self.summary.aspec_folder = StepStatus::Done;
                } else {
                    self.summary.aspec_folder = StepStatus::Warn("aspec/ folder not found".into());
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: "aspec/ folder not found in git root; run `awman init` to create it."
                            .to_string(),
                    });
                }
                // Repo config: .awman/config.json
                let repo_config = git_root.join(".awman").join("config.json");
                if repo_config.exists() {
                    self.summary.work_items_config = StepStatus::Done;
                } else {
                    self.summary.work_items_config =
                        StepStatus::Warn(".awman/config.json not found".into());
                    frontend.write_message(crate::data::message::UserMessage {
                        level: crate::data::message::MessageLevel::Warning,
                        text: ".awman/config.json not found; run `awman init` to create it."
                            .to_string(),
                    });
                }

                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if dockerfile_path.exists() {
                    self.summary.dockerfile = StepStatus::Done;
                    frontend.report_step_status(
                        &ReadyStep::CheckDockerfile(dockerfile_path.clone()).into(),
                        StepStatus::Done,
                    );
                    self.image_phase()
                } else {
                    ReadyPhase::AwaitingDockerfileDecision
                }
            }
            ReadyPhase::AwaitingDockerfileDecision => {
                let configured_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if frontend.ask_create_dockerfile(&configured_path)? {
                    ReadyPhase::CreatingDockerfile
                } else {
                    ReadyPhase::Failed(ReadyFailure {
                        phase: "AwaitingDockerfileDecision".into(),
                        message: format!("user declined to create {}", configured_path.display()),
                    })
                }
            }
            ReadyPhase::CreatingDockerfile => {
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                std::fs::write(&dockerfile_path, templates::project_dockerfile_dev())
                    .map_err(|e| EngineError::io(dockerfile_path.clone(), e))?;
                self.summary.dockerfile = StepStatus::Done;
                frontend.report_step_status(
                    &ReadyStep::CreateDockerfile(dockerfile_path.clone()).into(),
                    StepStatus::Done,
                );
                self.image_phase()
            }
            ReadyPhase::BuildingBaseImage => {
                // Issue 22: Docker daemon pre-check — soft failure allows
                // run_to_completion to surface a summary rather than aborting.
                if !self.runtime.is_available() {
                    let runtime = self.runtime.display_name();
                    let msg = format!(
                        "{runtime} is not available. Ensure {runtime} is running and retry."
                    );
                    self.summary.base_image = StepStatus::Failed(msg.clone());
                    frontend.report_step_status(
                        &ReadyStep::BuildBaseImage.into(),
                        StepStatus::Failed(msg),
                    );
                    // Bypass the `self.phase = next` assignment below to short-
                    // circuit straight to the next phase, but DO advance self.phase
                    // first — otherwise `run_to_completion` re-enters this branch
                    // forever (the docker-unavailable case is otherwise infinite).
                    self.phase = ReadyPhase::BuildingAgentImage;
                    return Ok(self.phase.clone());
                }

                let tag = project_image_tag(&git_root);
                // Rebuild when --build was passed or when the base image is
                // missing. Otherwise skip (`awman ready` is idempotent).
                let needs_build =
                    self.options.build || !self.runtime.image_exists(&tag).unwrap_or(false);
                if !needs_build {
                    self.summary.base_image = StepStatus::Done;
                    frontend
                        .report_step_status(&ReadyStep::BuildBaseImage.into(), StepStatus::Done);
                    ReadyPhase::BuildingAgentImage
                } else {
                    frontend
                        .report_step_status(&ReadyStep::BuildBaseImage.into(), StepStatus::Running);
                    let dockerfile_path = self
                        .session
                        .repo_config()
                        .dockerfile_path_or_default(&git_root);
                    let mut sink = |line: &str| {
                        frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                    };
                    let result = self.runtime.build_image(
                        &tag,
                        &dockerfile_path,
                        &git_root,
                        self.options.no_cache,
                        &mut sink,
                    );
                    match result {
                        Ok(()) => {
                            self.summary.base_image = StepStatus::Done;
                            frontend.report_step_status(
                                &ReadyStep::BuildBaseImage.into(),
                                StepStatus::Done,
                            );
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            self.summary.base_image = StepStatus::Failed(msg.clone());
                            frontend.report_step_status(
                                &ReadyStep::BuildBaseImage.into(),
                                StepStatus::Failed(msg),
                            );
                        }
                    }
                    ReadyPhase::BuildingAgentImage
                }
            }
            ReadyPhase::BuildingAgentImage => {
                // Issue 22 (extended for WI 0078): if Docker isn't available
                // there's no point attempting to download an agent Dockerfile
                // from the network — the build would fail anyway. Mark as
                // failed and continue, so the setup task exits promptly in
                // sandboxed test environments.
                if !self.runtime.is_available() {
                    let msg = format!("{} is not available.", self.runtime.display_name());
                    self.summary.agent_image = StepStatus::Failed(msg.clone());
                    frontend.report_step_status(
                        &ReadyStep::BuildAgentImage.into(),
                        StepStatus::Failed(msg),
                    );
                    return Ok({
                        self.phase = ReadyPhase::CheckingNonDefaultAgents;
                        self.phase.clone()
                    });
                }
                let paths = RepoDockerfilePaths::new(&git_root);
                let agent_dockerfile = paths.agent_dockerfile(self.options.agent.as_str());
                let tag = agent_image_tag(&git_root, self.options.agent.as_str());
                let needs_build =
                    self.options.build || !self.runtime.image_exists(&tag).unwrap_or(false);
                if !needs_build {
                    self.summary.agent_image = StepStatus::Done;
                    frontend
                        .report_step_status(&ReadyStep::BuildAgentImage.into(), StepStatus::Done);
                    return Ok({
                        self.phase = ReadyPhase::CheckingNonDefaultAgents;
                        self.phase.clone()
                    });
                }
                frontend
                    .report_step_status(&ReadyStep::BuildAgentImage.into(), StepStatus::Running);
                if !agent_dockerfile.exists() {
                    // Try downloading the per-agent Dockerfile (best-effort).
                    let project_tag = project_image_tag(&git_root);
                    let dl = crate::engine::agent::download::download_agent_dockerfile(
                        self.options.agent.as_str(),
                        &agent_dockerfile,
                        &project_tag,
                    )
                    .await;
                    if let Err(e) = dl {
                        let msg = e.to_string();
                        self.summary.agent_image = StepStatus::Failed(msg.clone());
                        frontend.report_step_status(
                            &ReadyStep::DownloadAgentDockerfile.into(),
                            StepStatus::Failed(msg),
                        );
                        // Continue but mark agent image not built.
                        return Ok({
                            self.phase = ReadyPhase::CheckingNonDefaultAgents;
                            self.phase.clone()
                        });
                    }
                }
                let mut sink = |line: &str| {
                    frontend.report_step_status(&SetupStep::output(line), StepStatus::Running);
                };
                let result = self.runtime.build_image(
                    &tag,
                    &agent_dockerfile,
                    &git_root,
                    self.options.no_cache,
                    &mut sink,
                );
                match result {
                    Ok(()) => {
                        self.summary.agent_image = StepStatus::Done;
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImage.into(),
                            StepStatus::Done,
                        );
                    }
                    Err(e) => {
                        self.summary.agent_image = StepStatus::Failed(e.to_string());
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImage.into(),
                            StepStatus::Failed(e.to_string()),
                        );
                    }
                }

                // ENG-1: When --build is set, also build all other agent images.
                if self.options.build {
                    let all_agents = paths.discover_agent_dockerfiles();
                    let default_agent = self.options.agent.as_str();
                    for (agent_name, agent_path) in &all_agents {
                        if agent_name == default_agent {
                            continue;
                        }
                        let other_tag = agent_image_tag(&git_root, agent_name);
                        frontend.report_step_status(
                            &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                            StepStatus::Running,
                        );
                        let mut agent_sink = |line: &str| {
                            frontend
                                .report_step_status(&SetupStep::output(line), StepStatus::Running);
                        };
                        let agent_result = self.runtime.build_image(
                            &other_tag,
                            agent_path,
                            &git_root,
                            self.options.no_cache,
                            &mut agent_sink,
                        );
                        match agent_result {
                            Ok(()) => {
                                frontend.report_step_status(
                                    &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                                    StepStatus::Done,
                                );
                            }
                            Err(e) => {
                                frontend.report_step_status(
                                    &ReadyStep::BuildAgentImageFor(agent_name.to_string()).into(),
                                    StepStatus::Failed(e.to_string()),
                                );
                            }
                        }
                    }
                }

                ReadyPhase::CheckingNonDefaultAgents
            }
            ReadyPhase::CheckingNonDefaultAgents => {
                let paths = RepoDockerfilePaths::new(&git_root);
                let all_agents = paths.discover_agent_dockerfiles();
                let default_agent = self.options.agent.as_str();

                let mut missing_agents: Vec<(String, String)> = Vec::new();
                let mut all_ok = true;
                let mut count = 0usize;
                for (agent_name, _agent_path) in &all_agents {
                    if agent_name == default_agent {
                        continue;
                    }
                    count += 1;
                    let other_tag = agent_image_tag(&git_root, agent_name);
                    // An image whose build or import failed in this run is
                    // not "done" just because an older one is installed.
                    let failed_now = self
                        .summary
                        .image_results
                        .iter()
                        .any(|r| r.tag == other_tag && r.has_failure());
                    if failed_now {
                        all_ok = false;
                        missing_agents.push((format!("{agent_name} (failed)"), other_tag));
                    } else if !self.runtime.image_exists(&other_tag).unwrap_or(false) {
                        all_ok = false;
                        missing_agents.push((agent_name.clone(), other_tag));
                    }
                }

                if count > 0 {
                    if all_ok {
                        // All non-default agents have valid images → single consolidated row.
                        frontend
                            .report_step_status(&ReadyStep::OtherAgents.into(), StepStatus::Done);
                        self.summary
                            .non_default_agent_images
                            .push(("Other agents".to_string(), StepStatus::Done));
                    } else {
                        // One consolidated "Missing images" row listing the
                        // affected agents — easier to scan than one row per
                        // agent in CLI/TUI/API output.
                        let names_csv = missing_agents
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let status = StepStatus::Warn(names_csv.clone());
                        frontend
                            .report_step_status(&ReadyStep::MissingImages.into(), status.clone());
                        self.summary
                            .non_default_agent_images
                            .push(("Missing images".to_string(), status));
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text: format!("Missing agent images: {names_csv}"),
                        });
                    }
                }

                ReadyPhase::CheckingLocalAgent
            }
            // SECURITY EXCEPTION: this is the ONE sanctioned host-side agent
            // execution in awman (see `aspec/architecture/security.md`). The
            // agent binary runs on the host with a hardcoded greeting — never
            // user input, repo content, or a working directory — to verify it
            // is installed and authenticated, and to force it to refresh its
            // auth token before credentials are mounted into containerized
            // agents. Do not add other host-side agent invocations.
            ReadyPhase::CheckingLocalAgent => {
                frontend
                    .report_step_status(&ReadyStep::CheckLocalAgent.into(), StepStatus::Running);
                let agent_name = self.options.agent.as_str();
                let ping = self.host_agent.ping(&self.options.agent).await;
                // The engine reports the result; the frontend draws the
                // transcript. The default impl writes the same two `>`/`<`
                // lines this arm used to compose (F-45).
                frontend.report_ping(&ping);
                match ping {
                    LocalAgentPingResult::Ok { .. } => {
                        self.summary.local_agent = StepStatus::Done;
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Done,
                        );
                    }
                    LocalAgentPingResult::Error => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: error (check auth)"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: error (check auth)")),
                        );
                    }
                    LocalAgentPingResult::NotInstalled => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: not installed"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: not installed")),
                        );
                    }
                    LocalAgentPingResult::CouldNotRun => {
                        self.summary.local_agent =
                            StepStatus::Failed(format!("{agent_name}: could not run"));
                        frontend.report_step_status(
                            &ReadyStep::CheckLocalAgent.into(),
                            StepStatus::Failed(format!("{agent_name}: could not run")),
                        );
                    }
                }
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                self.pre_audit_dockerfile_hash = dockerfile_hash(&dockerfile_path);
                ReadyPhase::RunningAudit
            }
            ReadyPhase::RunningAudit => {
                // Issue 7: When --refresh is not set, skip the audit entirely.
                if !self.options.refresh {
                    self.summary.audit = StepStatus::Skipped;
                    self.phase = ReadyPhase::RebuildingAfterAudit;
                    return Ok(self.phase.clone());
                }
                let dockerfile_path = self
                    .session
                    .repo_config()
                    .dockerfile_path_or_default(&git_root);
                if dockerfile_path.exists() {
                    let content = std::fs::read_to_string(&dockerfile_path).unwrap_or_default();
                    if !templates::dockerfile_matches_template(&content) {
                        frontend.write_message(crate::data::message::UserMessage {
                            level: crate::data::message::MessageLevel::Warning,
                            text:
                                "Dockerfile.dev has been customised; audit may overwrite changes."
                                    .into(),
                        });
                    }
                }
                if frontend.ask_run_audit_on_template()? {
                    use crate::data::templates::ready_audit_prompt;
                    use crate::engine::agent::AgentRunOptions;

                    let run_opts = AgentRunOptions {
                        yolo: None,
                        auto: None,
                        plan: None,
                        allowed_tools: vec![],
                        disallowed_tools: vec![],
                        initial_prompt: Some(ready_audit_prompt().to_string()),
                        allow_docker: self.options.allow_docker,
                        non_interactive: self.options.non_interactive,
                        model: None,
                        env_passthrough: self.options.env_passthrough.clone(),
                        directory_overlays: vec![],
                        include_all_skills: false,
                        named_skills: vec![],
                        image_tag_override: None,
                        ..Default::default()
                    };
                    match self.agent_engine.build_options(
                        &self.session,
                        &self.options.agent,
                        &run_opts,
                    ) {
                        Err(e) => {
                            self.summary.audit = StepStatus::Failed(e.to_string());
                        }
                        Ok(options) => match ResolvedAgentOptions::container(options)
                            .and_then(|o| self.runtime.build(o))
                        {
                            Err(e) => {
                                self.summary.audit = StepStatus::Failed(e.to_string());
                            }
                            Ok(instance) => {
                                let container_fe = frontend.container_frontend();
                                match instance.run_with_frontend(container_fe) {
                                    Err(e) => {
                                        self.summary.audit = StepStatus::Failed(e.to_string());
                                    }
                                    Ok(mut exec) => match exec.wait().await {
                                        Err(e) => {
                                            self.summary.audit = StepStatus::Failed(e.to_string());
                                        }
                                        Ok(exit) => {
                                            if exit.exit_code == 0 {
                                                self.summary.audit = StepStatus::Done;
                                            } else {
                                                self.summary.audit = StepStatus::Failed(format!(
                                                    "audit exited with code {}",
                                                    exit.exit_code
                                                ));
                                            }
                                        }
                                    },
                                }
                            }
                        },
                    }
                } else {
                    self.summary.audit = StepStatus::Skipped;
                }
                ReadyPhase::RebuildingAfterAudit
            }
            ReadyPhase::RebuildingAfterAudit => {
                if matches!(self.summary.audit, StepStatus::Done) {
                    let dockerfile_path = self
                        .session
                        .repo_config()
                        .dockerfile_path_or_default(&git_root);
                    let post_hash = dockerfile_hash(&dockerfile_path);
                    let changed = match (self.pre_audit_dockerfile_hash, post_hash) {
                        (Some(pre), Some(post)) => pre != post,
                        // If we can't compute either hash, conservatively assume changed.
                        _ => true,
                    };
                    if changed
                        && self.imports_images()
                        && self.builder_enabled
                        && self.default_source_configured(&git_root)
                    {
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            StepStatus::Running,
                        );
                        let old_build = self.options.build;
                        self.options.build = true;
                        self.prevalidate_build_targets(&git_root);
                        self.prepared_bases.clear();
                        let prior = self.summary.image_results.len();
                        self.import_default_agent_image(frontend, &git_root).await;
                        self.import_other_agent_images(frontend, &git_root).await;
                        self.options.build = old_build;
                        let failure = self.summary.image_results[prior..]
                            .iter()
                            .find(|r| r.has_failure())
                            .map(|r| format!("audit rebuild of {} failed", r.tag))
                            .or_else(|| match &self.summary.agent_image {
                                StepStatus::Failed(message) => {
                                    Some(format!("audit rebuild failed: {message}"))
                                }
                                _ => None,
                            });
                        self.summary.image_rebuild = match failure {
                            Some(message) => StepStatus::Failed(message),
                            None => StepStatus::Done,
                        };
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            self.summary.image_rebuild.clone(),
                        );
                    } else if changed && self.imports_images() {
                        self.summary.image_rebuild = StepStatus::Warn(
                            "Dockerfile changed: rebuild the image externally, then run \
                             `awman ready --build` to re-import it"
                                .into(),
                        );
                    } else if changed {
                        frontend.report_step_status(
                            &ReadyStep::RebuildingAfterAudit.into(),
                            StepStatus::Running,
                        );
                        let tag = project_image_tag(&git_root);
                        let dockerfile_path_clone = dockerfile_path.clone();
                        let mut sink = |line: &str| {
                            frontend
                                .report_step_status(&SetupStep::output(line), StepStatus::Running);
                        };
                        let result = self.runtime.build_image(
                            &tag,
                            &dockerfile_path_clone,
                            &git_root,
                            self.options.no_cache,
                            &mut sink,
                        );
                        match result {
                            Ok(()) => {
                                self.summary.base_image = StepStatus::Done;
                                self.summary.image_rebuild = StepStatus::Done;
                                frontend.report_step_status(
                                    &ReadyStep::RebuildingAfterAudit.into(),
                                    StepStatus::Done,
                                );
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                self.summary.base_image = StepStatus::Failed(msg.clone());
                                self.summary.image_rebuild = StepStatus::Failed(msg.clone());
                                frontend.report_step_status(
                                    &ReadyStep::RebuildingAfterAudit.into(),
                                    StepStatus::Failed(msg),
                                );
                            }
                        }

                        // Issue 9: Also rebuild agent images that layer FROM the project base.
                        let awman_dir = git_root.join(".awman");
                        if awman_dir.exists() {
                            if let Ok(entries) = std::fs::read_dir(&awman_dir) {
                                for entry in entries.flatten() {
                                    let name = entry.file_name();
                                    let name_str = name.to_string_lossy().to_string();
                                    if name_str.starts_with("Dockerfile.") {
                                        let agent =
                                            name_str.strip_prefix("Dockerfile.").unwrap_or("");
                                        if !agent.is_empty() {
                                            let agent_tag =
                                                crate::data::image_tags::agent_image_tag(
                                                    &git_root, agent,
                                                );
                                            let mut agent_sink = |line: &str| {
                                                frontend.report_step_status(
                                                    &SetupStep::output(line),
                                                    StepStatus::Running,
                                                );
                                            };
                                            let _ = self.runtime.build_image(
                                                &agent_tag,
                                                &entry.path(),
                                                &git_root,
                                                self.options.no_cache,
                                                &mut agent_sink,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        self.summary.image_rebuild = StepStatus::Skipped;
                    }
                } else {
                    self.summary.image_rebuild = StepStatus::Skipped;
                }
                ReadyPhase::Complete
            }
            // Import-acquisition phases (`ImageAcquisition::Import`). The
            // opted-in local builder prepares images in their source stores;
            // execution remains in the importing runtime.
            //
            // The project image (`Dockerfile.dev`) is only a build input for
            // the agent images; an importing runtime runs agent images alone,
            // so there is nothing to import for it.
            ReadyPhase::ImportingBaseImage => {
                self.summary.base_image = StepStatus::Skipped;
                ReadyPhase::ImportingAgentImage
            }
            ReadyPhase::ImportingAgentImage => {
                if self.builder_enabled {
                    self.prevalidate_build_targets(&git_root);
                }
                self.import_default_agent_image(frontend, &git_root).await;
                // The import counterpart of ENG-1: `--build` also acquires
                // every other agent's image.
                if self.options.build && self.runtime.availability().is_ok() {
                    self.import_other_agent_images(frontend, &git_root).await;
                }
                ReadyPhase::CheckingNonDefaultAgents
            }
            ReadyPhase::Complete | ReadyPhase::Failed(_) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    /// `ImportingAgentImage`: import the default agent's image and record the
    /// outcome in the summary.
    async fn import_default_agent_image(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
    ) {
        let agent = self.options.agent.as_str().to_string();
        match self.import_agent_image(frontend, git_root, &agent).await {
            Ok(()) => {
                self.summary.agent_image = StepStatus::Done;
                self.summary.image_source = Some(StepStatus::Done);
            }
            Err(msg) => self.record_import_failure(msg),
        }
    }

    /// Make `agent`'s image available to an importing runtime. `Err` carries
    /// the failure for the caller's row; a failure is a row, not an abort,
    /// exactly like a failed build.
    async fn import_agent_image(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
        agent: &str,
    ) -> Result<(), String> {
        use crate::data::image_tags::{agent_image_tag, project_image_tag};
        use crate::data::message::{MessageLevel, UserMessage};
        use crate::data::oci_identity::OciPlatform;
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;

        let tag = agent_image_tag(git_root, agent);
        // Import mode keeps the runtime's concrete reason (e.g. /dev/kvm
        // permission, test isolation) instead of a bare "not available".
        if let Err(error) = self.runtime.availability() {
            return Err(format!(
                "{} is not available: {error}",
                self.runtime.display_name()
            ));
        }

        let paths = RepoDockerfilePaths::new(git_root);
        let hint = BuildHint {
            agent_dockerfile: paths.agent_dockerfile(agent),
            project: Some((
                self.session
                    .repo_config()
                    .dockerfile_path_or_default(git_root),
                project_image_tag(git_root),
            )),
            context: git_root.to_path_buf(),
        };
        let platform = OciPlatform::host_linux();
        // The acquirer's cache keys include the resolved endpoint/path and
        // platform, and detect archive replacement. ImageIdentity alone does
        // not contain that provenance, so never bypass acquisition for a
        // configured source merely because the runtime already has this tag.
        let resolved = resolve_source(&tag, &self.image_sources, &hint);

        // `--build` / `--no-cache` mean "acquire again from the source".
        let refresh = self.options.build || self.options.no_cache;
        // Progress rows reported Running here are resolved to Done/Failed
        // once the import finishes, so no row is left spinning.
        let mut pending: Vec<SetupStep> = Vec::new();
        if !refresh && self.runtime.image_exists(&tag).unwrap_or(false) {
            let identity = self.runtime.image_identity(&tag).ok().flatten();
            // Nothing configured to compare against: keep the image.
            let current = matches!(resolved, Err(EngineError::ImageSourceUnconfigured { .. }));
            if current {
                if let Some(identity) = identity {
                    frontend.report_step_status(
                        &SetupStep::output(format!(
                            "{tag}: cached {} ({}, from {})",
                            identity.manifest_digest, identity.platform, identity.source
                        )),
                        StepStatus::Done,
                    );
                }
                return Ok(());
            }
            if let (Ok((source, reference)), Some(id)) = (&resolved, &identity) {
                let step = SetupStep::output(format!(
                    "{tag}: cached from {} ({}); the configured source is {} ({reference}), \
                     importing again",
                    id.source,
                    id.reference,
                    source.kind()
                ));
                frontend.report_step_status(&step, StepStatus::Running);
                pending.push(step);
            }
        }

        let mut image_result = ReadyImageResult::new(agent.to_string(), tag.clone());
        image_result.optional = agent != self.options.agent.as_str();
        let (mut source, reference) = match resolved {
            Ok(resolved) => resolved,
            Err(e) => {
                // The error carries the external build instruction; make
                // sure the Dockerfile it names exists.
                self.provision_agent_dockerfile(frontend, git_root, agent, &hint.agent_dockerfile)
                    .await;
                let text = e.to_string();
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: text.clone(),
                });
                image_result.build = StepStatus::Skipped;
                image_result.import = StepStatus::Failed(text.clone());
                self.summary.image_results.push(image_result);
                return Err(text);
            }
        };

        image_result.source_kind = Some(source.kind());
        image_result.reference = Some(reference.clone());
        // Every import request gets a result, with the build stage Skipped
        // when the builder is off, so build and import always show apart.
        image_result.build = StepStatus::Skipped;
        let preparation = if self.builder_enabled {
            if let Some(error) = self.prevalidation_failures.get(&tag).cloned() {
                image_result.build = StepStatus::Failed(error.clone());
                image_result.import = StepStatus::Skipped;
                self.summary.image_results.push(image_result);
                for step in &pending {
                    frontend.report_step_status(step, StepStatus::Failed(error.clone()));
                }
                return Err(error);
            }
            let registries = self.session.effective_config().builtin_runtime().registries;
            let env = Env::for_image_acquisition(&registries);
            let source_resolver =
                crate::engine::oci::ImageSourceResolver::new(&self.image_sources, &env);
            let frozen = match source_resolver.resolve_for_image(&tag, &hint, platform.clone()) {
                Ok(source) => source,
                Err(e) => {
                    image_result.build = StepStatus::Failed(e.to_string());
                    image_result.import = StepStatus::Skipped;
                    self.summary.image_results.push(image_result);
                    for step in &pending {
                        frontend.report_step_status(step, StepStatus::Failed(e.to_string()));
                    }
                    return Err(e.to_string());
                }
            };
            source = frozen.source.clone();
            match self
                .prepare_local_image(frontend, &frozen, &tag, agent, git_root)
                .await
            {
                Ok(prepared) => {
                    image_result.build = prepared.status.clone();
                    if let Some(warning) = &prepared.warning {
                        image_result.warnings.push(warning.clone());
                    }
                    Some(prepared)
                }
                Err(error) => {
                    image_result.build = StepStatus::Failed(error.clone());
                    image_result.import = StepStatus::Skipped;
                    self.summary.image_results.push(image_result);
                    for step in &pending {
                        frontend.report_step_status(step, StepStatus::Failed(error.clone()));
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Some(blocked) = preparation.as_ref().and_then(|p| p.blocked.clone()) {
            image_result.import = StepStatus::Failed(blocked.clone());
            self.summary.image_results.push(image_result);
            for step in &pending {
                frontend.report_step_status(step, StepStatus::Failed(blocked.clone()));
            }
            return Err(blocked);
        }
        let built_here = preparation
            .as_ref()
            .is_some_and(|p| p.status == StepStatus::Done);
        let local_builder = preparation
            .as_ref()
            .is_some_and(|p| p.pending_key.is_some());

        let importing = SetupStep::output(format!(
            "Importing {tag} from {} ({reference})",
            source.kind()
        ));
        frontend.report_step_status(&importing, StepStatus::Running);
        pending.push(importing);
        let docker_source = matches!(
            source,
            crate::data::config::image_source::ImageSourceSpec::DockerStore { .. }
        );
        let expected = preparation.as_ref().and_then(|p| p.expected.clone());
        let request = ImageImportRequest {
            tag: tag.clone(),
            source,
            platform,
            refresh: refresh || preparation.as_ref().is_some_and(|p| p.force_refresh),
            expected_built_identity: expected.clone(),
            // Export the exact image the receipt names, by its immutable
            // Engine ID, whichever digest kind that store uses for IDs.
            immutable_export_selector: expected.as_ref().filter(|_| docker_source).and_then(|id| {
                id.config_digest()
                    .or(id.manifest_digest())
                    .map(ToString::to_string)
            }),
        };
        let imported_result = import::run(
            self.image_importer.clone(),
            request,
            self.cancel.clone(),
            frontend,
        )
        .await
        .and_then(|imported| {
            if let Some(expected) = &expected {
                if !expected.matches(&imported.identity) {
                    return Err(EngineError::ImageDigestMismatch {
                        reference: reference.clone(),
                        expected: expected
                            .config_digest()
                            .or(expected.manifest_digest())
                            .expect("validated build receipt")
                            .to_string(),
                        actual: if expected.config_digest().is_some() {
                            imported.identity.config_digest.to_string()
                        } else {
                            imported.identity.manifest_digest.to_string()
                        },
                    });
                }
            }
            Ok(imported)
        });
        match imported_result {
            Ok(imported) => {
                image_result.import = StepStatus::Done;
                self.summary.image_results.push(image_result);
                // The image is installed; a receipt that cannot be cleared
                // only means a later run re-imports the same content.
                if let Some(key) = preparation.as_ref().and_then(|p| p.pending_key.as_ref()) {
                    if let Err(e) = self.settle_receipt(key, &imported.identity).await {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: format!("{tag}: could not clear its build receipt: {e}"),
                        });
                    }
                }
                for step in &pending {
                    frontend.report_step_status(step, StepStatus::Done);
                }
                frontend.report_step_status(
                    &SetupStep::output(format!(
                        "{tag}: imported {} ({}, from {})",
                        imported.identity.manifest_digest,
                        imported.identity.platform,
                        imported.identity.source
                    )),
                    StepStatus::Done,
                );
                Ok(())
            }
            Err(e) => {
                let text = e.to_string();
                image_result.import = StepStatus::Failed(text.clone());
                self.summary.image_results.push(image_result);
                for step in &pending {
                    frontend.report_step_status(step, StepStatus::Failed(text.clone()));
                }
                if crate::engine::oci::retry::is_cancelled(&e) {
                    return Err(text);
                }
                if built_here {
                    // The new image exists in its source store; only the
                    // import failed, and its receipt lets a later run retry it.
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "{tag}: the build succeeded but the import failed; run \
                             `awman ready` again to retry the import"
                        ),
                    });
                } else if local_builder {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "{tag}: run `awman ready --build` to rebuild it in its source store"
                        ),
                    });
                } else if matches!(e, EngineError::Container(_)) {
                    // A missing source image is the common case; say how to
                    // produce it rather than only that it is missing.
                    self.provision_agent_dockerfile(
                        frontend,
                        git_root,
                        agent,
                        &hint.agent_dockerfile,
                    )
                    .await;
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: crate::engine::oci::external_build_hint(&tag, &hint),
                    });
                }
                Err(text)
            }
        }
    }

    /// `ready --build` on an importing runtime: acquire every non-default
    /// agent's image too, one row per agent. The agents are the ones with a
    /// `.awman/Dockerfile.<agent>` — the same set `CheckingNonDefaultAgents`
    /// reports as missing — so the tip it prints leads here.
    async fn import_other_agent_images(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
    ) {
        use crate::data::image_tags::agent_image_tag;
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;

        let default_agent = self.options.agent.as_str().to_string();
        let others = RepoDockerfilePaths::new(git_root).discover_agent_dockerfiles();
        for (agent, _) in &others {
            if *agent == default_agent {
                continue;
            }
            if self.cancel.check().is_err() {
                return;
            }
            let step: SetupStep = ReadyStep::ImportAgentImageFor(agent.clone()).into();
            frontend.report_step_status(&step, StepStatus::Running);
            let tag = agent_image_tag(git_root, agent);
            let status = if let Some(reason) = self.single_image_default_source(&tag) {
                // A fixed default reference cannot hold this agent's image.
                // Only a source awman could build into turns that into a
                // failed build; any other source is the import-only warning.
                if self.builder_enabled && self.default_source_is_local(git_root, agent, &tag) {
                    let mut result = ReadyImageResult::new(agent.clone(), tag.clone());
                    result.optional = true;
                    result.build = StepStatus::Failed(reason.clone());
                    result.import = StepStatus::Skipped;
                    self.summary.image_results.push(result);
                    StepStatus::Failed(reason)
                } else {
                    StepStatus::Warn(reason)
                }
            } else {
                match self.import_agent_image(frontend, git_root, agent).await {
                    Ok(()) => StepStatus::Done,
                    Err(msg) => StepStatus::Failed(msg),
                }
            };
            frontend.report_step_status(&step, status);
        }
    }

    /// Whether `builtin.imageSource` or a `builtin.images` entry applies to
    /// the default agent's image, i.e. whether a source-matched rebuild has
    /// anything to build into.
    fn default_source_configured(&self, git_root: &std::path::Path) -> bool {
        let tag = crate::data::image_tags::agent_image_tag(git_root, self.options.agent.as_str());
        self.image_sources.default.is_some() || self.image_sources.images.contains_key(&tag)
    }

    /// Whether `tag` resolves to a source the local builder can build into.
    fn default_source_is_local(&self, git_root: &std::path::Path, agent: &str, tag: &str) -> bool {
        use crate::data::image_tags::project_image_tag;
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        let registries = self.session.effective_config().builtin_runtime().registries;
        let env = Env::for_image_acquisition(&registries);
        let hint = BuildHint {
            agent_dockerfile: RepoDockerfilePaths::new(git_root).agent_dockerfile(agent),
            project: Some((
                self.session
                    .repo_config()
                    .dockerfile_path_or_default(git_root),
                project_image_tag(git_root),
            )),
            context: git_root.to_path_buf(),
        };
        crate::engine::oci::ImageSourceResolver::new(&self.image_sources, &env)
            .resolve_for_image(
                tag,
                &hint,
                crate::data::oci_identity::OciPlatform::host_linux(),
            )
            .is_ok_and(|resolved| {
                matches!(
                    BuilderSelection::from_source(Some(&resolved)),
                    BuilderSelection::Local(_)
                )
            })
    }

    /// Why `tag` must not be imported from `builtin.imageSource`, if it must
    /// not: the default source names one fixed image (an archive file or an
    /// explicit `reference`) and `tag` has no `builtin.images` entry of its
    /// own. Importing it anyway would install the default agent's image
    /// under another agent's tag.
    fn single_image_default_source(&self, tag: &str) -> Option<String> {
        use crate::data::config::image_source::ImageSourceSpec;

        if self.image_sources.images.contains_key(tag) {
            return None;
        }
        let spec = self.image_sources.default.as_ref()?;
        let pinned =
            matches!(spec, ImageSourceSpec::Archive { .. }) || spec.reference_or(tag) != tag;
        pinned.then(|| {
            format!(
                "`builtin.imageSource` ({}) names a single image; add a `builtin.images` \
                 entry for {tag} to import it",
                spec.kind()
            )
        })
    }

    /// Make sure the agent Dockerfile an external-build instruction names
    /// exists, fetching it exactly as the build flow does. Best-effort: a
    /// failure is reported on the download row and the instruction is still
    /// shown. awman never builds the image itself here.
    async fn provision_agent_dockerfile(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
        git_root: &std::path::Path,
        agent: &str,
        agent_dockerfile: &std::path::Path,
    ) {
        if agent_dockerfile.exists() {
            return;
        }
        let project_tag = crate::data::image_tags::project_image_tag(git_root);
        let downloaded = crate::engine::agent::download::download_agent_dockerfile(
            agent,
            agent_dockerfile,
            &project_tag,
        )
        .await;
        let status = match downloaded {
            Ok(()) => StepStatus::Done,
            Err(e) => StepStatus::Failed(e.to_string()),
        };
        frontend.report_step_status(&ReadyStep::DownloadAgentDockerfile.into(), status);
    }

    fn record_import_failure(&mut self, msg: String) {
        self.summary.agent_image = StepStatus::Failed(msg.clone());
        self.summary.image_source = Some(StepStatus::Failed(msg));
    }

    /// Drive to completion: advance phases in a loop until terminal.
    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ReadyFrontend,
    ) -> Result<ReadySummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            self.cancel.check()?;
            if matches!(next, ReadyPhase::Complete | ReadyPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}

/// awman's data home for build locks and receipts.
fn data_home() -> Result<std::path::PathBuf, String> {
    GlobalConfig::data_home_with(&Env::from_process()).map_err(|e| e.to_string())
}

/// One spelling per local image name, using the Docker reference rules both
/// Docker and Apple Containers apply: the `docker.io` registry (and its
/// `index.docker.io` alias) and the `library/` namespace are implied, and so
/// is a `:latest` tag. Lock keys, receipts and collision checks compare this
/// form, so `foo`, `foo:latest` and `docker.io/library/foo:latest` are one
/// image.
fn normalize_build_reference(reference: &str) -> String {
    let (name, suffix) = match reference.find('@') {
        Some(at) => (&reference[..at], &reference[at..]),
        None => {
            let last_slash = reference.rfind('/').map_or(0, |i| i + 1);
            match reference[last_slash..].rfind(':') {
                Some(colon) => (
                    &reference[..last_slash + colon],
                    &reference[last_slash + colon..],
                ),
                None => (reference, ":latest"),
            }
        }
    };
    let mut parts: Vec<&str> = name.split('/').collect();
    let first = parts[0];
    let has_registry =
        parts.len() > 1 && (first.contains('.') || first.contains(':') || first == "localhost");
    if has_registry && (first == "docker.io" || first == "index.docker.io") {
        parts.remove(0);
    } else if has_registry {
        return format!("{name}{suffix}");
    }
    if parts.len() == 2 && parts[0] == "library" {
        parts.remove(0);
    }
    format!("{}{suffix}", parts.join("/"))
}

/// Compute a simple hash of a file's contents for change detection.
/// Returns `None` when the file cannot be read.
fn dockerfile_hash(path: &std::path::Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let contents = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::message::{UserMessage, UserMessageSink};
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::data::step_status::StepStatus;
    use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
    use crate::engine::error::EngineError;
    use crate::engine::overlay::OverlayEngine;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeReadyFrontend {
        create_dockerfile: bool,
        run_audit: bool,
        phases: Vec<ReadyPhase>,
        statuses: Vec<(String, StepStatus)>,
        /// Path passed to the most recent `ask_create_dockerfile` call.
        last_dockerfile_prompt_path: Option<std::path::PathBuf>,
        /// Every `write_message` call, recorded for tests that need to
        /// assert on warning/hint text (e.g. the incompatible-builder
        /// warning, or a build-succeeded/import-failed retry hint).
        messages: Vec<UserMessage>,
    }

    struct FakeRuntimeFrontend;

    impl UserMessageSink for FakeRuntimeFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait::async_trait]
    impl AgentFrontend for FakeRuntimeFrontend {
        fn report_status(&mut self, _status: AgentStatus) {}
        fn report_progress(&mut self, _progress: AgentProgress) {}
        fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
            let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::engine::agent_runtime::frontend::AgentIo {
                stdout: stdout_tx,
                stderr: stderr_tx,
                stdin_tx,
                stdin_rx,
                resize: None,
                initial_size: None,
            }
        }
    }

    impl UserMessageSink for FakeReadyFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    impl FakeReadyFrontend {
        /// Text of every `Warning`-level message written, in order.
        fn warning_texts(&self) -> Vec<String> {
            self.messages
                .iter()
                .filter(|m| m.level == crate::data::message::MessageLevel::Warning)
                .map(|m| m.text.clone())
                .collect()
        }
    }

    impl ReadyFrontend for FakeReadyFrontend {
        fn ask_create_dockerfile(
            &mut self,
            dockerfile_path: &std::path::Path,
        ) -> Result<bool, EngineError> {
            self.last_dockerfile_prompt_path = Some(dockerfile_path.to_path_buf());
            Ok(self.create_dockerfile)
        }

        fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn report_phase(&mut self, phase: &ReadyPhase) {
            self.phases.push(phase.clone());
        }

        fn report_summary(&mut self, _summary: &ReadySummary) {}
    }

    impl crate::engine::agent::AgentImageFrontend for FakeReadyFrontend {
        fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
            self.statuses.push((step.to_string(), status));
        }

        fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
            Box::new(FakeRuntimeFrontend)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine_and_frontend(
        create_dockerfile: bool,
        run_audit: bool,
    ) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create .awman/Dockerfile.claude so the ready engine does not
        // attempt a network download during tests.
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let options = ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: true,
            no_cache: false,
            allow_docker: false,
            non_interactive: false,
            env_passthrough: None,
        };
        let engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        let frontend = FakeReadyFrontend {
            create_dockerfile,
            run_audit,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };
        (engine, frontend, tmp)
    }

    // ── Tests ────────────────────────────────────────────────────────────────
    //
    // The host-ping tests moved to `host_agent.rs` with the code they test.

    #[tokio::test]
    async fn awaiting_dockerfile_decision_false_leads_to_failed_phase() {
        let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(false, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert!(
            matches!(engine.phase(), ReadyPhase::Failed(_)),
            "expected Failed phase, got {:?}",
            engine.phase()
        );
        // Summary fields should still be Pending (nothing ran after abort).
        assert!(matches!(summary.base_image, StepStatus::Pending));
        assert!(!summary.is_ready());
    }

    #[tokio::test]
    async fn each_phase_reachable_via_step_calls() {
        let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(true, false);
        // Step through from Preflight to Awaiting* phases individually.
        assert_eq!(engine.phase(), &ReadyPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::AwaitingDockerfileDecision);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ReadyPhase::CreatingDockerfile);
    }

    #[tokio::test]
    async fn creating_dockerfile_phase_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (_engine, mut frontend, _tmp2) = make_engine_and_frontend(true, false);
        // We want to test the CreatingDockerfile phase specifically. Use a
        // dedicated tmpdir so we can check file creation.
        let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                crate::data::session::SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let options = ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: false,
            non_interactive: false,
            env_passthrough: None,
        };
        let mut engine2 = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        );
        // Step to AwaitingDockerfileDecision, then accept to move to CreatingDockerfile.
        engine2.step(&mut frontend).await.unwrap(); // Preflight → AwaitingDockerfileDecision
        engine2.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision → CreatingDockerfile
                                                    // Execute CreatingDockerfile phase.
        engine2.step(&mut frontend).await.unwrap(); // CreatingDockerfile → BuildingBaseImage
        let dockerfile = tmp.path().join("Dockerfile.dev");
        assert!(
            dockerfile.exists(),
            "CreatingDockerfile phase must write Dockerfile.dev to git root"
        );
        let content = std::fs::read_to_string(&dockerfile).unwrap();
        assert!(
            !content.is_empty(),
            "Dockerfile.dev must contain the template content"
        );
    }

    // ─── WI-0086: configured dockerfile path in ready ─────────────────────────

    /// Build a ReadyEngine rooted at `git_root` with the agent Dockerfile
    /// pre-created so no network downloads happen during tests. Mirrors
    /// `make_engine_and_frontend` but lets callers stage their own
    /// `.awman/config.json` before the Session is opened (so RepoConfig is
    /// read from disk with the test's contents).
    fn make_engine_at(git_root: &std::path::Path) -> ReadyEngine {
        let awman_dir = git_root.join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
        let resolver = StaticGitRootResolver::new(git_root);
        let session = Arc::new(
            crate::data::session::Session::open(
                git_root.to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
        ));
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            Arc::new(crate::engine::container::ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let options = ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: false,
            non_interactive: false,
            env_passthrough: None,
        };
        ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            options,
        )
    }

    #[tokio::test]
    async fn ready_with_configured_dockerfile_reports_configured_path_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Configure a non-default dockerfile path, but do not create the file.
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"dockerfile":"infra/Dockerfile.base"}"#,
        )
        .unwrap();

        let mut engine = make_engine_at(tmp.path());
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false, // decline so the engine fails fast
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };

        engine.step(&mut frontend).await.unwrap(); // Preflight
        engine.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision

        let prompted = frontend
            .last_dockerfile_prompt_path
            .as_ref()
            .expect("ask_create_dockerfile must be called when the configured path is missing");
        assert_eq!(
            prompted,
            &tmp.path().join("infra/Dockerfile.base"),
            "ready must surface the configured dockerfile path, not the default Dockerfile.dev"
        );
    }

    #[tokio::test]
    async fn ready_with_configured_dockerfile_skips_decision_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"dockerfile":"infra/Dockerfile.base"}"#,
        )
        .unwrap();
        // Create the configured Dockerfile so Preflight short-circuits past
        // AwaitingDockerfileDecision.
        let infra_dir = tmp.path().join("infra");
        std::fs::create_dir_all(&infra_dir).unwrap();
        std::fs::write(infra_dir.join("Dockerfile.base"), "FROM scratch\n").unwrap();

        let mut engine = make_engine_at(tmp.path());
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false, // sentinel — if the prompt fires the engine would fail
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };

        engine.step(&mut frontend).await.unwrap(); // Preflight → BuildingBaseImage

        assert!(
            frontend.last_dockerfile_prompt_path.is_none(),
            "ready must not prompt for Dockerfile creation when the configured path exists; \
             got: {:?}",
            frontend.last_dockerfile_prompt_path
        );
        assert!(
            matches!(engine.summary().dockerfile, StepStatus::Done),
            "dockerfile step must be Done when the configured path exists; got: {:?}",
            engine.summary().dockerfile
        );
        assert_eq!(
            engine.phase(),
            &ReadyPhase::BuildingBaseImage,
            "engine must advance directly to BuildingBaseImage after Preflight"
        );
    }

    // ── Kit-declarative (sandbox-tier) ready flow (F-40b) ────────────────
    //
    // `ReadyEngine` is runtime-agnostic: given a kit-declarative runtime it
    // skips the whole container phase machine in one step. Before F-40b this
    // branch lived in Layer 2 (`commands/ready.rs`) and keyed on
    // `engines.sandbox_runtime.is_some()`.

    /// A kit-declarative runtime that records the `ready_agent` call and
    /// refuses every image-store operation, the way the sandbox tier does.
    struct FakeKitRuntime {
        caps: crate::engine::agent_runtime::Capabilities,
        ready_calls:
            std::sync::Mutex<Vec<(String, crate::engine::agent_runtime::ReadyAgentOptions)>>,
    }

    impl FakeKitRuntime {
        fn new() -> Self {
            let mut caps = crate::engine::container::ContainerRuntime::docker()
                .capabilities()
                .clone();
            caps.kit_declarative = true;
            caps.has_image_store = false;
            Self {
                caps,
                ready_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeKitRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake-kit"
        }
        fn display_name(&self) -> &'static str {
            "Fake Kit Runtime"
        }
        fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            _: crate::engine::agent_runtime::ResolvedAgentOptions,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("FakeKitRuntime never launches an agent")
        }
        fn list_running(
            &self,
            _: &crate::data::session::Session,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn stats(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
            Ok(crate::engine::agent_runtime::AgentStats {
                name: String::new(),
                cpu_percent: 0.0,
                memory_mb: 0.0,
            })
        }
        fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(
            &self,
            _: &str,
            _: &str,
            _: &[&str],
            _: &[(&str, &str)],
        ) -> Option<Vec<String>> {
            None
        }
        fn attach(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("FakeKitRuntime never attaches")
        }
        fn list_running_with_name_prefix(
            &self,
            _: &str,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn host_cli(&self) -> Option<&'static str> {
            Some("fake-kit")
        }
        fn ready_agent(
            &self,
            agent: &str,
            opts: crate::engine::agent_runtime::ReadyAgentOptions,
            _sink: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<(), EngineError> {
            self.ready_calls
                .lock()
                .unwrap()
                .push((agent.to_string(), opts));
            Ok(())
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "local image probe",
            })
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "image HOME lookup",
            })
        }
        fn build_image(
            &self,
            _: &str,
            _: &std::path::Path,
            _: &std::path::Path,
            _: bool,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-kit",
                operation: "image build",
            })
        }
    }

    #[tokio::test]
    async fn ready_on_a_kit_declarative_runtime_applies_the_kit_and_skips_image_phases() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let kit = Arc::new(FakeKitRuntime::new());
        let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = kit.clone();
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let mut engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            agent_engine,
            ReadyEngineOptions {
                agent: AgentName::new("claude").unwrap(),
                refresh: false,
                build: true,
                no_cache: true,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        );
        let mut frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };

        let phase = engine.step(&mut frontend).await.unwrap();

        assert_eq!(
            phase,
            ReadyPhase::Complete,
            "one step, straight to Complete"
        );
        let calls = kit.ready_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "the kit is applied exactly once");
        assert_eq!(calls[0].0, "claude");
        assert!(calls[0].1.no_cache, "no_cache must reach the runtime");

        let summary = engine.summary();
        assert_eq!(summary.agent_image, StepStatus::Done);
        // Every container-tier step is not applicable, and says so rather
        // than reporting a Done it never did.
        assert_eq!(summary.dockerfile, StepStatus::Skipped);
        assert_eq!(summary.base_image, StepStatus::Skipped);
        assert_eq!(summary.local_agent, StepStatus::Skipped);
        assert_eq!(summary.audit, StepStatus::Skipped);
    }

    // ── Import-acquisition (builtin) ready flow ──────────────────────────
    //
    // An importing runtime prepares an image that was built elsewhere. It is
    // never asked to build, and a missing source explains the external build.

    struct FakeImportRuntime {
        caps: crate::engine::agent_runtime::Capabilities,
        has_image: bool,
        unavailable: Option<&'static str>,
        cached_source: crate::data::config::image_source::ImageSourceKind,
        imports: std::sync::Mutex<Vec<crate::engine::agent_runtime::ImageImportRequest>>,
        builds: std::sync::Mutex<usize>,
        wait_for_cancel: bool,
        cancel_observed: std::sync::atomic::AtomicBool,
        /// Overrides the digest `import_image` reports, regardless of the
        /// request's `expected_built_identity`. Used to script a mismatch
        /// between what a local build produced and what import installs.
        digest_override: std::sync::Mutex<Option<char>>,
        /// Number of upcoming `import_image` calls that must fail (counts
        /// down), for "build succeeded, import failed" scenarios.
        fail_next_imports: std::sync::atomic::AtomicUsize,
    }

    impl FakeImportRuntime {
        fn new(has_image: bool) -> Self {
            let mut caps = crate::engine::container::ContainerRuntime::docker()
                .capabilities()
                .clone();
            caps.image_acquisition = crate::engine::agent_runtime::ImageAcquisition::Import;
            caps.fractional_cpu = false;
            Self {
                caps,
                has_image,
                unavailable: None,
                cached_source: crate::data::config::image_source::ImageSourceKind::Archive,
                imports: std::sync::Mutex::new(Vec::new()),
                builds: std::sync::Mutex::new(0),
                wait_for_cancel: false,
                cancel_observed: std::sync::atomic::AtomicBool::new(false),
                digest_override: std::sync::Mutex::new(None),
                fail_next_imports: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// Make every subsequent `import_image` report `sha256:<c x 64>`,
        /// instead of honouring the request's expected build receipt.
        fn set_digest_override(&self, c: char) {
            *self.digest_override.lock().unwrap() = Some(c);
        }

        /// Make the next `n` `import_image` calls fail.
        fn fail_next_import(&self, n: usize) {
            self.fail_next_imports
                .store(n, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeImportRuntime {
        fn runtime_name(&self) -> &'static str {
            "fake-import"
        }
        fn display_name(&self) -> &'static str {
            "Fake Import Runtime"
        }
        fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            self.unavailable.is_none()
        }
        fn availability(&self) -> Result<(), EngineError> {
            match self.unavailable {
                Some(reason) => Err(EngineError::BuiltinRuntimeUnavailable {
                    reason: reason.into(),
                }),
                None => Ok(()),
            }
        }
        fn build(
            &self,
            _: crate::engine::agent_runtime::ResolvedAgentOptions,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!("FakeImportRuntime never launches an agent")
        }
        fn list_running(
            &self,
            _: &crate::data::session::Session,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn stats(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
            unimplemented!()
        }
        fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
            Ok(())
        }
        fn exec_args(
            &self,
            _: &str,
            _: &str,
            _: &[&str],
            _: &[(&str, &str)],
        ) -> Option<Vec<String>> {
            None
        }
        fn attach(
            &self,
            _: &crate::data::session::AgentHandle,
        ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
            unimplemented!()
        }
        fn list_running_with_name_prefix(
            &self,
            _: &str,
        ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
            Ok(vec![])
        }
        fn host_cli(&self) -> Option<&'static str> {
            None
        }
        fn ready_agent(
            &self,
            _: &str,
            _: crate::engine::agent_runtime::ReadyAgentOptions,
            _: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<(), EngineError> {
            unimplemented!("an importing runtime is not kit-declarative")
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Ok(self.has_image)
        }
        fn image_identity(
            &self,
            tag: &str,
        ) -> Result<Option<crate::data::oci_identity::ImageIdentity>, EngineError> {
            let digest =
                crate::data::oci_identity::Digest::parse(&format!("sha256:{}", "b".repeat(64)))
                    .unwrap();
            Ok(self
                .has_image
                .then(|| crate::data::oci_identity::ImageIdentity {
                    reference: tag.to_string(),
                    manifest_digest: digest.clone(),
                    config_digest: digest,
                    platform: crate::data::oci_identity::OciPlatform::host_linux(),
                    source: self.cached_source,
                }))
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Ok(None)
        }
        fn build_image(
            &self,
            _: &str,
            _: &std::path::Path,
            _: &std::path::Path,
            _: bool,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            *self.builds.lock().unwrap() += 1;
            Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake-import",
                operation: "image build",
            })
        }
        fn import_image_cancellable(
            &self,
            request: &crate::engine::agent_runtime::ImageImportRequest,
            sink: &mut dyn crate::data::message::UserMessageSink,
            cancel: &crate::engine::oci::CancelToken,
        ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
            if self.wait_for_cancel {
                self.imports.lock().unwrap().push(request.clone());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                self.cancel_observed
                    .store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
                cancel.check()?;
                return Err(EngineError::Other(
                    "ready caller did not cancel import".into(),
                ));
            }
            self.import_image(request, sink)
        }
        fn import_image(
            &self,
            request: &crate::engine::agent_runtime::ImageImportRequest,
            _sink: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
            self.imports.lock().unwrap().push(request.clone());
            if self
                .fail_next_imports
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n: usize| n.checked_sub(1),
                )
                .is_ok()
            {
                return Err(EngineError::Container("fake import failure".into()));
            }
            let hex = match *self.digest_override.lock().unwrap() {
                Some(c) => c.to_string().repeat(64),
                None => match request
                    .expected_built_identity
                    .as_ref()
                    .and_then(|id| id.config_digest().or(id.manifest_digest()))
                {
                    Some(digest) => {
                        return Ok(crate::engine::agent_runtime::ImportedImage {
                            tag: request.tag.clone(),
                            identity: crate::data::oci_identity::ImageIdentity {
                                reference: request.tag.clone(),
                                manifest_digest: digest.clone(),
                                config_digest: digest.clone(),
                                platform: request.platform.clone(),
                                source: request.source.kind(),
                            },
                            home_dir: Some("/home/agent".into()),
                            user: None,
                        })
                    }
                    None => "a".repeat(64),
                },
            };
            let digest =
                crate::data::oci_identity::Digest::parse(&format!("sha256:{hex}")).unwrap();
            Ok(crate::engine::agent_runtime::ImportedImage {
                tag: request.tag.clone(),
                identity: crate::data::oci_identity::ImageIdentity {
                    reference: request.tag.clone(),
                    manifest_digest: digest.clone(),
                    config_digest: digest,
                    platform: request.platform.clone(),
                    source: request.source.kind(),
                },
                home_dir: Some("/home/agent".into()),
                user: None,
            })
        }
    }

    fn import_engine(
        runtime: Arc<FakeImportRuntime>,
        build: bool,
        sources: ImageSources,
    ) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            dyn_runtime.clone(),
        ));
        let engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            dyn_runtime,
            agent_engine,
            ReadyEngineOptions {
                agent: AgentName::new("claude").unwrap(),
                refresh: false,
                build,
                no_cache: false,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        )
        .with_image_sources(sources);
        let frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };
        (engine, frontend, tmp)
    }

    /// Step until the image phases are done (stops before the host ping).
    async fn step_through_image_phases(engine: &mut ReadyEngine, frontend: &mut FakeReadyFrontend) {
        while !matches!(
            engine.phase(),
            ReadyPhase::CheckingNonDefaultAgents | ReadyPhase::Complete | ReadyPhase::Failed(_)
        ) {
            engine.step(frontend).await.unwrap();
        }
    }

    fn archive_sources() -> ImageSources {
        ImageSources {
            default: Some(
                crate::data::config::image_source::ImageSourceSpec::Archive {
                    path: "/exports/agent.tar".into(),
                },
            ),
            images: Default::default(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_import_timeout_cancels_worker_on_a_current_thread_runtime() {
        let mut fake = FakeImportRuntime::new(false);
        fake.wait_for_cancel = true;
        let runtime = Arc::new(fake);
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), true, archive_sources());
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            engine.import_default_agent_image(&mut frontend, tmp.path()),
        )
        .await;
        assert!(
            result.is_err(),
            "blocking import must not prevent the async timeout"
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !runtime
                .cancel_observed
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropped ready future must cancel its production worker");
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_caller_token_cancels_an_in_flight_import() {
        let mut fake = FakeImportRuntime::new(false);
        fake.wait_for_cancel = true;
        let runtime = Arc::new(fake);
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), true, archive_sources());
        let cancel = engine.cancel_token();
        let signal = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        engine
            .import_default_agent_image(&mut frontend, tmp.path())
            .await;
        signal.await.unwrap();
        assert!(runtime
            .cancel_observed
            .load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            matches!(engine.summary().agent_image, StepStatus::Failed(message) if message.contains("cancelled"))
        );
    }

    #[tokio::test]
    async fn import_runtime_preserves_the_concrete_availability_error() {
        let mut fake = FakeImportRuntime::new(true);
        fake.unavailable = Some("cannot access /dev/kvm: Permission denied");
        let runtime = Arc::new(fake);
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), false, archive_sources());
        engine
            .import_default_agent_image(&mut frontend, tmp.path())
            .await;
        assert!(
            matches!(engine.summary().agent_image, StepStatus::Failed(message) if message.contains("/dev/kvm: Permission denied"))
        );
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert_eq!(*runtime.builds.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn import_runtime_imports_the_agent_image_and_never_builds() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), false, archive_sources());
        step_through_image_phases(&mut engine, &mut frontend).await;

        assert!(frontend.phases.contains(&ReadyPhase::ImportingBaseImage));
        assert!(frontend.phases.contains(&ReadyPhase::ImportingAgentImage));
        assert!(!frontend.phases.contains(&ReadyPhase::BuildingBaseImage));
        assert_eq!(
            *runtime.builds.lock().unwrap(),
            0,
            "an importing runtime never builds"
        );
        let imports = runtime.imports.lock().unwrap().clone();
        assert_eq!(imports.len(), 1);
        assert_eq!(
            imports[0].tag,
            crate::data::image_tags::agent_image_tag(tmp.path(), "claude")
        );
        assert!(!imports[0].refresh);
        assert_eq!(
            imports[0].platform,
            crate::data::oci_identity::OciPlatform::host_linux()
        );
        let summary = engine.summary();
        assert_eq!(summary.agent_image, StepStatus::Done);
        assert_eq!(summary.image_source, Some(StepStatus::Done));
        assert_eq!(summary.base_image, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn import_runtime_checks_the_source_cache_unless_refresh_is_requested() {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), false, archive_sources());
        step_through_image_phases(&mut engine, &mut frontend).await;
        let imports = runtime.imports.lock().unwrap().clone();
        assert_eq!(
            imports.len(),
            1,
            "the acquirer checks full source provenance"
        );
        assert!(!imports[0].refresh, "cached acquisition remains offline");
        assert_eq!(engine.summary().agent_image, StepStatus::Done);

        let runtime = Arc::new(FakeImportRuntime::new(true));
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), true, archive_sources());
        step_through_image_phases(&mut engine, &mut frontend).await;
        let imports = runtime.imports.lock().unwrap().clone();
        assert_eq!(imports.len(), 1, "--build re-acquires from the source");
        assert!(imports[0].refresh);
        assert_eq!(*runtime.builds.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn import_runtime_keeps_an_installed_image_when_no_source_is_configured() {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), false, ImageSources::default());
        step_through_image_phases(&mut engine, &mut frontend).await;
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert_eq!(engine.summary().agent_image, StepStatus::Done);
    }

    #[tokio::test]
    async fn import_runtime_rechecks_changed_locators_with_the_same_kind_and_reference() {
        use crate::data::config::image_source::ImageSourceSpec;
        for sources in [
            [
                ImageSourceSpec::Archive {
                    path: "/exports/first.tar".into(),
                },
                ImageSourceSpec::Archive {
                    path: "/exports/second.tar".into(),
                },
            ],
            [
                ImageSourceSpec::DockerStore {
                    host: Some("unix:///first.sock".into()),
                    tls: None,
                    reference: None,
                },
                ImageSourceSpec::DockerStore {
                    host: Some("unix:///second.sock".into()),
                    tls: None,
                    reference: None,
                },
            ],
            [
                ImageSourceSpec::Registry {
                    registry: Some("first.example".into()),
                    reference: None,
                },
                ImageSourceSpec::Registry {
                    registry: Some("second.example".into()),
                    reference: None,
                },
            ],
        ] {
            let mut fake = FakeImportRuntime::new(true);
            fake.cached_source = sources[0].kind();
            let runtime = Arc::new(fake);
            let (mut engine, mut frontend, _tmp) = import_engine(
                runtime.clone(),
                false,
                ImageSources {
                    default: Some(sources[0].clone()),
                    images: Default::default(),
                },
            );
            let root = engine.session.git_root().to_path_buf();
            engine
                .import_default_agent_image(&mut frontend, &root)
                .await;
            engine.image_sources.default = Some(sources[1].clone());
            engine
                .import_default_agent_image(&mut frontend, &root)
                .await;
            let imports = runtime.imports.lock().unwrap();
            assert_eq!(imports.len(), 2);
            assert_eq!(imports[0].source, sources[0]);
            assert_eq!(imports[1].source, sources[1]);
            assert!(imports.iter().all(|request| !request.refresh));
        }
    }

    #[tokio::test]
    async fn import_runtime_without_a_source_explains_the_external_build() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), false, ImageSources::default());
        step_through_image_phases(&mut engine, &mut frontend).await;
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert_eq!(*runtime.builds.lock().unwrap(), 0);
        match engine.summary().agent_image {
            StepStatus::Failed(msg) => {
                assert!(msg.contains("does not build"), "{msg}");
                assert!(msg.contains("Dockerfile.claude"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(matches!(
            engine.summary().image_source,
            Some(StepStatus::Failed(_))
        ));
    }

    #[tokio::test]
    async fn import_runtime_reimports_an_image_cached_from_another_source_kind() {
        let mut fake = FakeImportRuntime::new(true);
        fake.cached_source = crate::data::config::image_source::ImageSourceKind::DockerStore;
        let runtime = Arc::new(fake);
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), false, archive_sources());
        step_through_image_phases(&mut engine, &mut frontend).await;
        let imports = runtime.imports.lock().unwrap().clone();
        assert_eq!(imports.len(), 1, "the configured archive source is used");
        assert_eq!(
            imports[0].source.kind(),
            crate::data::config::image_source::ImageSourceKind::Archive
        );
        assert!(!imports[0].refresh);
        assert_eq!(engine.summary().agent_image, StepStatus::Done);
        // No progress row is left Running once the import finished.
        let mut last = std::collections::BTreeMap::new();
        for (step, status) in &frontend.statuses {
            last.insert(step.clone(), status.clone());
        }
        assert!(
            last.keys().any(|s| s.contains("importing again")),
            "{last:?}"
        );
        assert!(
            last.values().all(|s| *s != StepStatus::Running),
            "rows left running: {last:?}"
        );
    }

    /// Write `.awman/Dockerfile.<agent>` for each agent so ready discovers it.
    fn write_agent_dockerfiles(root: &std::path::Path, agents: &[&str]) {
        let paths = crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(root);
        for agent in agents {
            let path = paths.agent_dockerfile(agent);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "FROM scratch\n").unwrap();
        }
    }

    fn docker_store_sources() -> ImageSources {
        ImageSources {
            default: Some(
                crate::data::config::image_source::ImageSourceSpec::DockerStore {
                    host: None,
                    tls: None,
                    reference: None,
                },
            ),
            images: Default::default(),
        }
    }

    fn last_status(frontend: &FakeReadyFrontend, step: &str) -> Option<StepStatus> {
        frontend
            .statuses
            .iter()
            .rev()
            .find(|(s, _)| s == step)
            .map(|(_, status)| status.clone())
    }

    #[tokio::test]
    async fn import_runtime_build_imports_every_agent_image() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), true, docker_store_sources());
        write_agent_dockerfiles(tmp.path(), &["claude", "codex", "gemini"]);
        step_through_image_phases(&mut engine, &mut frontend).await;

        let tags: Vec<String> = runtime
            .imports
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.tag.clone())
            .collect();
        let tag = |a| crate::data::image_tags::agent_image_tag(tmp.path(), a);
        assert_eq!(tags, vec![tag("claude"), tag("codex"), tag("gemini")]);
        assert!(runtime.imports.lock().unwrap().iter().all(|r| r.refresh));
        assert_eq!(*runtime.builds.lock().unwrap(), 0);
        for agent in ["codex", "gemini"] {
            assert_eq!(
                last_status(&frontend, &format!("Import agent image: {agent}")),
                Some(StepStatus::Done)
            );
        }
        assert_eq!(
            last_status(&frontend, "Import agent image: claude"),
            None,
            "the default agent keeps its own summary row"
        );
        assert_eq!(engine.summary().agent_image, StepStatus::Done);
    }

    #[tokio::test]
    async fn import_runtime_without_build_imports_only_the_default_agent() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), false, docker_store_sources());
        write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
        step_through_image_phases(&mut engine, &mut frontend).await;
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(
            imports[0].tag,
            crate::data::image_tags::agent_image_tag(tmp.path(), "claude")
        );
    }

    #[tokio::test]
    async fn import_runtime_build_never_reuses_a_single_image_default_source() {
        use crate::data::config::image_source::ImageSourceSpec;
        // An archive, or a default source pinned to one reference, holds the
        // default agent's image; importing it under codex's tag would be wrong.
        let pinned = ImageSources {
            default: Some(ImageSourceSpec::Registry {
                registry: Some("ghcr.io".into()),
                reference: Some("acme/claude:1".into()),
            }),
            images: Default::default(),
        };
        for sources in [archive_sources(), pinned] {
            let runtime = Arc::new(FakeImportRuntime::new(false));
            let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), true, sources);
            write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
            step_through_image_phases(&mut engine, &mut frontend).await;
            assert_eq!(runtime.imports.lock().unwrap().len(), 1);
            match last_status(&frontend, "Import agent image: codex") {
                Some(StepStatus::Warn(msg)) => assert!(msg.contains("builtin.images"), "{msg}"),
                other => panic!("expected Warn, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn import_runtime_build_uses_a_per_tag_source_for_other_agents() {
        use crate::data::config::image_source::ImageSourceSpec;
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), true, archive_sources());
        write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
        let codex_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "codex");
        let codex_source = ImageSourceSpec::Archive {
            path: "/exports/codex.tar".into(),
        };
        engine
            .image_sources
            .images
            .insert(codex_tag.clone(), codex_source.clone());
        step_through_image_phases(&mut engine, &mut frontend).await;
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[1].tag, codex_tag);
        assert_eq!(imports[1].source, codex_source);
        assert_eq!(
            last_status(&frontend, "Import agent image: codex"),
            Some(StepStatus::Done)
        );
    }

    #[tokio::test]
    async fn import_runtime_provisions_the_dockerfile_its_build_hint_names() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) =
            import_engine(runtime.clone(), false, ImageSources::default());
        let dockerfile = crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(tmp.path())
            .agent_dockerfile("claude");
        assert!(!dockerfile.exists());
        step_through_image_phases(&mut engine, &mut frontend).await;
        assert!(
            dockerfile.exists(),
            "the external build instruction names an existing Dockerfile"
        );
        assert_eq!(*runtime.builds.lock().unwrap(), 0);
    }

    // ── WI 0125: source-matched local builder ──────────────────────────────
    //
    // A `FakeLocalBuilder` substitutes for `HostLocalImageBuilder`: it is
    // scripted per reference string (FIFO per call), never spawns a
    // process, and records every probe/inspect/build call so these tests
    // run with no Docker/Apple daemon and no network. `builder_engine`
    // enables `builtin.builder.enabled` through a real `.awman/config.json`
    // (the only way to set the private `builder_enabled` field) and injects
    // private, per-test build-lock/receipt roots so no two tests, and no
    // real `$HOME`, ever share state.

    use crate::engine::container::host_cli_backend::BuildControl;
    use crate::engine::oci::SourceImage;

    #[derive(Default)]
    struct FakeLocalBuilder {
        probe_calls: std::sync::atomic::AtomicUsize,
        probe_err: Option<String>,
        inspect_calls: std::sync::Mutex<Vec<String>>,
        inspect_script: std::sync::Mutex<
            std::collections::HashMap<
                String,
                std::collections::VecDeque<Result<SourceImage, String>>,
            >,
        >,
        build_calls: std::sync::Mutex<Vec<LocalImageBuildRequest>>,
        build_script: std::sync::Mutex<
            std::collections::HashMap<
                String,
                std::collections::VecDeque<Result<BuiltImageIdentity, String>>,
            >,
        >,
    }

    impl FakeLocalBuilder {
        fn new() -> Self {
            Self::default()
        }

        /// Every probe/inspect/build call fails with `msg`, as if the
        /// daemon/service were unreachable.
        fn unavailable(msg: impl Into<String>) -> Self {
            Self {
                probe_err: Some(msg.into()),
                ..Self::default()
            }
        }

        fn script_inspect(&self, reference: &str, result: Result<SourceImage, String>) {
            self.inspect_script
                .lock()
                .unwrap()
                .entry(reference.to_string())
                .or_default()
                .push_back(result);
        }

        fn script_build(&self, reference: &str, result: Result<BuiltImageIdentity, String>) {
            self.build_script
                .lock()
                .unwrap()
                .entry(reference.to_string())
                .or_default()
                .push_back(result);
        }

        fn probe_count(&self) -> usize {
            self.probe_calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn inspect_count(&self) -> usize {
            self.inspect_calls.lock().unwrap().len()
        }

        fn build_calls(&self) -> Vec<LocalImageBuildRequest> {
            self.build_calls.lock().unwrap().clone()
        }
    }

    impl LocalImageBuilder for FakeLocalBuilder {
        fn probe(
            &self,
            _target: &LocalBuilderTarget,
            _control: &BuildControl,
        ) -> Result<(), EngineError> {
            self.probe_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match &self.probe_err {
                Some(msg) => Err(EngineError::Container(msg.clone())),
                None => Ok(()),
            }
        }

        fn inspect(
            &self,
            _target: &LocalBuilderTarget,
            reference: &str,
            _platform: &crate::data::oci_identity::OciPlatform,
            _control: &BuildControl,
        ) -> Result<SourceImage, EngineError> {
            self.inspect_calls
                .lock()
                .unwrap()
                .push(reference.to_string());
            if let Some(err) = &self.probe_err {
                return Err(EngineError::Container(err.clone()));
            }
            let mut script = self.inspect_script.lock().unwrap();
            match script.get_mut(reference).and_then(|q| q.pop_front()) {
                Some(Ok(image)) => Ok(image),
                Some(Err(msg)) => Err(EngineError::Container(msg)),
                None => Ok(SourceImage::Absent),
            }
        }

        fn build(
            &self,
            request: &LocalImageBuildRequest,
            _control: &BuildControl,
            _on_line: &mut dyn FnMut(&str),
        ) -> Result<BuiltImageIdentity, EngineError> {
            self.build_calls.lock().unwrap().push(request.clone());
            if let Some(err) = &self.probe_err {
                return Err(EngineError::Container(err.clone()));
            }
            let mut script = self.build_script.lock().unwrap();
            match script
                .get_mut(&request.reference)
                .and_then(|q| q.pop_front())
            {
                Some(Ok(id)) => Ok(id),
                Some(Err(msg)) => Err(EngineError::Container(msg)),
                None => Err(EngineError::Container(format!(
                    "FakeLocalBuilder: no scripted build result for {}",
                    request.reference
                ))),
            }
        }
    }

    /// A receipt naming only a config digest (Docker classic-store shape):
    /// `BuiltImageIdentity::matches` then ignores the manifest digest, so a
    /// scripted build/import pair agrees without also scripting a manifest
    /// digest on both sides.
    fn built_identity(
        hex: char,
        platform: crate::data::oci_identity::OciPlatform,
    ) -> BuiltImageIdentity {
        let digest = crate::data::oci_identity::Digest::parse(&format!(
            "sha256:{}",
            hex.to_string().repeat(64)
        ))
        .unwrap();
        BuiltImageIdentity::new(Some(digest), None, platform).unwrap()
    }

    /// A `builtin.imageSource` naming an explicit local Unix socket. Explicit
    /// (never the platform default): under `cfg(test)` the default socket is
    /// disabled unless `AWMAN_TEST_DOCKER` is set (test isolation), and an
    /// explicit `host` is the one path WI 0125 allows a build into.
    fn local_docker_sources(socket: &std::path::Path) -> ImageSources {
        ImageSources {
            default: Some(
                crate::data::config::image_source::ImageSourceSpec::DockerStore {
                    host: Some(format!("unix://{}", socket.display())),
                    tls: None,
                    reference: None,
                },
            ),
            images: Default::default(),
        }
    }

    /// A `ReadyEngine` with `builtin.builder.enabled` set (the only way to
    /// set it: it is read once, from real repo config, in `ReadyEngine::new`)
    /// and a `FakeLocalBuilder` injected in place of the host CLI builder.
    /// Build-lock and receipt roots are private per-test temp directories,
    /// passed back to the caller so a second engine can share them (retry
    /// scenarios) or each test can use its own (isolation).
    fn builder_engine(
        runtime: Arc<FakeImportRuntime>,
        build: bool,
        no_cache: bool,
        sources: ImageSources,
        local_builder: Arc<FakeLocalBuilder>,
    ) -> (
        ReadyEngine,
        FakeReadyFrontend,
        tempfile::TempDir,
        ImageBuildLockStore,
        PendingBuiltImageStore,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        let awman_dir = tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"builtin":{"builder":{"enabled":true}}}"#,
        )
        .unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            dyn_runtime.clone(),
        ));
        let locks = ImageBuildLockStore::new(tmp.path().join("locks"));
        let receipts = PendingBuiltImageStore::new(tmp.path().join("receipts"));
        let engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            dyn_runtime,
            agent_engine,
            ReadyEngineOptions {
                agent: AgentName::new("claude").unwrap(),
                refresh: false,
                build,
                no_cache,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        )
        .with_image_sources(sources)
        .with_local_builder(local_builder as Arc<dyn LocalImageBuilder>)
        .with_build_stores(locks.clone(), receipts.clone());
        let frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };
        (engine, frontend, tmp, locks, receipts)
    }

    /// Reopen a `builder_engine` at the same `git_root`/sources but with a
    /// fresh fake builder/runtime, sharing one build-lock/receipt root — a
    /// second `awman ready` invocation after the first, for retry tests.
    fn reopen_builder_engine(
        git_root: &std::path::Path,
        runtime: Arc<FakeImportRuntime>,
        build: bool,
        sources: ImageSources,
        local_builder: Arc<FakeLocalBuilder>,
        locks: ImageBuildLockStore,
        receipts: PendingBuiltImageStore,
    ) -> (ReadyEngine, FakeReadyFrontend) {
        let resolver = StaticGitRootResolver::new(git_root);
        let session = Arc::new(
            crate::data::session::Session::open(
                git_root.to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
        ));
        let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            dyn_runtime.clone(),
        ));
        let engine = ReadyEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            dyn_runtime,
            agent_engine,
            ReadyEngineOptions {
                agent: AgentName::new("claude").unwrap(),
                refresh: false,
                build,
                no_cache: false,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        )
        .with_image_sources(sources)
        .with_local_builder(local_builder as Arc<dyn LocalImageBuilder>)
        .with_build_stores(locks, receipts);
        let frontend = FakeReadyFrontend {
            create_dockerfile: false,
            run_audit: false,
            phases: Vec::new(),
            statuses: Vec::new(),
            last_dockerfile_prompt_path: None,
            messages: Vec::new(),
        };
        (engine, frontend)
    }

    /// Script a `FakeLocalBuilder` for the common "nothing built yet" path:
    /// the agent and project references are both absent from the source
    /// store, so the engine builds the base then the agent image.
    fn script_missing_image_build(
        builder: &FakeLocalBuilder,
        project_tag: &str,
        agent_tag: &str,
        base_id: BuiltImageIdentity,
        agent_id: BuiltImageIdentity,
    ) {
        builder.script_inspect(agent_tag, Ok(SourceImage::Absent));
        builder.script_inspect(project_tag, Ok(SourceImage::Absent));
        builder.script_build(project_tag, Ok(base_id.clone()));
        builder.script_inspect(project_tag, Ok(SourceImage::Present(base_id)));
        builder.script_build(agent_tag, Ok(agent_id));
    }

    /// Disabled mode (no `.awman/config.json`, so `builder_enabled` is the
    /// documented default `false`) must never probe, inspect or build, even
    /// against a source the builder would otherwise treat as local/Docker —
    /// "unchanged disabled mode".
    #[tokio::test]
    async fn disabled_builder_never_calls_the_local_builder() {
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, _tmp) =
            import_engine(runtime.clone(), false, docker_store_sources());
        engine = engine.with_local_builder(local_builder.clone() as Arc<dyn LocalImageBuilder>);
        step_through_image_phases(&mut engine, &mut frontend).await;
        assert_eq!(local_builder.probe_count(), 0);
        assert_eq!(local_builder.inspect_count(), 0);
        assert!(local_builder.build_calls().is_empty());
        // Import-only behaviour is unchanged: it still imports from the
        // configured source (docker_store_sources resolves through
        // resolve_source, not the endpoint-resolving ImageSourceResolver).
        assert_eq!(engine.summary().agent_image, StepStatus::Done);
    }

    /// Ordinary missing-image readiness: nothing is built or installed yet,
    /// so plain `ready` (no `--build`) builds the project base before the
    /// agent image — base-before-agent ordering — then imports the agent
    /// build's exact content with forced refresh.
    #[tokio::test]
    async fn builder_builds_base_then_agent_on_a_missing_image() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            ImageSources::default(),
            local_builder.clone(),
        );
        let socket = tmp.path().join("docker.sock");
        engine.image_sources = local_docker_sources(&socket);
        let project_tag = crate::data::image_tags::project_image_tag(tmp.path());
        let agent_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        let base_id = built_identity('a', platform.clone());
        let agent_id = built_identity('b', platform);
        script_missing_image_build(
            &local_builder,
            &project_tag,
            &agent_tag,
            base_id,
            agent_id.clone(),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        let builds = local_builder.build_calls();
        assert_eq!(
            builds
                .iter()
                .map(|r| r.reference.clone())
                .collect::<Vec<_>>(),
            vec![project_tag.clone(), agent_tag.clone()],
            "the project base image must be built before the agent image"
        );

        let summary = engine.summary();
        assert_eq!(summary.base_image_results.len(), 1);
        assert_eq!(summary.base_image_results[0].tag, project_tag);
        assert_eq!(summary.base_image_results[0].build, StepStatus::Done);
        assert_eq!(summary.image_results.len(), 1);
        assert_eq!(summary.image_results[0].build, StepStatus::Done);
        assert_eq!(summary.image_results[0].import, StepStatus::Done);
        assert!(summary.is_ready());
        assert_eq!(summary.outcome_line(), "awman is ready.");

        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert!(
            imports[0].refresh,
            "a fresh build must force re-acquisition"
        );
        assert_eq!(
            imports[0].expected_built_identity,
            Some(agent_id),
            "the import must be bound to the exact content the build produced"
        );
        assert_eq!(
            last_status(
                &frontend,
                &format!(
                    "Build base image {project_tag} in docker unix://{}",
                    socket.display()
                ),
            ),
            Some(StepStatus::Done),
            "the base-image build step must be reported per the ReadyStep::BuildBaseImageIn label"
        );
    }

    /// `--build` rebuilds even an already-installed image, in its source
    /// store, and the import is still refreshed with the exact content the
    /// build produced.
    #[tokio::test]
    async fn builder_forced_build_rebuilds_an_already_installed_image() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("docker.sock");
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            true,
            false,
            local_docker_sources(&socket),
            local_builder.clone(),
        );
        // `builder_engine` already wrote Dockerfile.dev/.awman inside its own
        // tmpdir; use that one instead of the throwaway `tmp` above.
        let git_root = engine.session.git_root().to_path_buf();
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        let base_id = built_identity('c', platform.clone());
        let agent_id = built_identity('d', platform);
        local_builder.script_build(&project_tag, Ok(base_id.clone()));
        local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
        local_builder.script_build(&agent_tag, Ok(agent_id.clone()));

        step_through_image_phases(&mut engine, &mut frontend).await;

        let builds = local_builder.build_calls();
        assert_eq!(
            builds
                .iter()
                .map(|r| r.reference.clone())
                .collect::<Vec<_>>(),
            vec![project_tag, agent_tag]
        );
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert!(
            imports[0].refresh,
            "--build must re-acquire even though the runtime already has the image"
        );
        assert_eq!(imports[0].expected_built_identity, Some(agent_id));
    }

    /// `--no-cache` is forwarded onto every build request it schedules
    /// (base and agent), exactly as the `build`/`no_cache` flags already
    /// behave on the Docker/Apple runtimes — it is not an independent build
    /// trigger.
    #[tokio::test]
    async fn builder_forwards_no_cache_to_every_build_request() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let tmp0 = tempfile::tempdir().unwrap();
        let socket = tmp0.path().join("docker.sock");
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            true,
            local_docker_sources(&socket),
            local_builder.clone(),
        );
        let git_root = engine.session.git_root().to_path_buf();
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        script_missing_image_build(
            &local_builder,
            &project_tag,
            &agent_tag,
            built_identity('e', platform.clone()),
            built_identity('f', platform),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        let builds = local_builder.build_calls();
        assert_eq!(builds.len(), 2);
        assert!(
            builds.iter().all(|r| r.no_cache),
            "no_cache must reach every scheduled build request: {builds:?}"
        );
    }

    /// Two non-default agents built into the same store as the default agent
    /// (`ready --build`) share one base-image build: the base is deduped per
    /// store within one ready plan, not rebuilt per agent.
    #[tokio::test]
    async fn builder_dedups_the_base_build_across_multiple_agents_in_one_store() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let tmp0 = tempfile::tempdir().unwrap();
        let socket = tmp0.path().join("docker.sock");
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            true,
            false,
            local_docker_sources(&socket),
            local_builder.clone(),
        );
        let git_root = engine.session.git_root().to_path_buf();
        write_agent_dockerfiles(&git_root, &["claude", "codex", "gemini"]);
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        // `--build` skips the `base_exists` inspect (it always rebuilds), so
        // only the post-build verification inspect is scripted.
        let base_id = built_identity('1', platform.clone());
        local_builder.script_build(&project_tag, Ok(base_id.clone()));
        local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
        for (agent, hex) in [("claude", '2'), ("codex", '3'), ("gemini", '4')] {
            let tag = crate::data::image_tags::agent_image_tag(&git_root, agent);
            local_builder.script_build(&tag, Ok(built_identity(hex, platform.clone())));
        }

        step_through_image_phases(&mut engine, &mut frontend).await;

        let builds = local_builder.build_calls();
        let base_builds = builds.iter().filter(|r| r.reference == project_tag).count();
        assert_eq!(
            base_builds, 1,
            "base must be built exactly once per store: {builds:?}"
        );
        for agent in ["claude", "codex", "gemini"] {
            let tag = crate::data::image_tags::agent_image_tag(&git_root, agent);
            assert!(
                builds.iter().any(|r| r.reference == tag),
                "{agent}'s image must have been built; builds: {builds:?}"
            );
        }
        assert_eq!(engine.summary().base_image_results.len(), 1);
    }

    /// A1: two repos (sessions) in one process, sharing the same local
    /// builder resource, must each act on only their own session's builder
    /// settings — a disabled-builder session must not leak the other
    /// session's enablement (or the reverse), regardless of construction
    /// order or which session runs first.
    #[tokio::test]
    async fn two_sessions_with_opposite_builder_settings_each_see_only_their_own() {
        use crate::data::config::image_source::ImageSourceSpec;
        let apple_sources = ImageSources {
            default: Some(ImageSourceSpec::AppleStore { reference: None }),
            images: Default::default(),
        };
        let shared_local_builder = Arc::new(FakeLocalBuilder::new());

        // Session A: no repo config.json, so the builder is disabled by
        // default. Even though it is wired to the same local-builder
        // instance session B will use, it must never call it.
        let runtime_a = Arc::new(FakeImportRuntime::new(false));
        let (mut engine_a, mut frontend_a, _tmp_a) =
            import_engine(runtime_a.clone(), false, apple_sources.clone());
        engine_a =
            engine_a.with_local_builder(shared_local_builder.clone() as Arc<dyn LocalImageBuilder>);
        step_through_image_phases(&mut engine_a, &mut frontend_a).await;
        assert_eq!(
            shared_local_builder.probe_count(),
            0,
            "session A's disabled builder must not have probed"
        );
        assert!(
            shared_local_builder.build_calls().is_empty(),
            "session A's disabled builder must not have built"
        );
        assert_eq!(engine_a.summary().agent_image, StepStatus::Done);

        // Session B: its own repo's config.json turns the builder on,
        // sharing the exact local-builder instance A just ran against with
        // zero calls. B's own enablement must now drive it.
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime_b = Arc::new(FakeImportRuntime::new(false));
        let (mut engine_b, mut frontend_b, tmp_b, _locks, _receipts) = builder_engine(
            runtime_b.clone(),
            false,
            false,
            apple_sources,
            shared_local_builder.clone(),
        );
        let project_tag = crate::data::image_tags::project_image_tag(tmp_b.path());
        let agent_tag = crate::data::image_tags::agent_image_tag(tmp_b.path(), "claude");
        script_missing_image_build(
            &shared_local_builder,
            &project_tag,
            &agent_tag,
            built_identity('a', platform.clone()),
            built_identity('b', platform),
        );
        step_through_image_phases(&mut engine_b, &mut frontend_b).await;
        let builds = shared_local_builder.build_calls();
        assert_eq!(
            builds
                .iter()
                .map(|r| r.reference.clone())
                .collect::<Vec<_>>(),
            vec![project_tag, agent_tag],
            "session B's own enabled builder must drive the shared instance \
             exactly once each, unaffected by A ever having used it"
        );
        assert!(engine_b.summary().is_ready());
    }

    /// The same ready plan can build into two different source stores (one
    /// agent on a local Docker Engine, another on the local Apple Containers
    /// store): each store gets its own base-image build, independently.
    #[tokio::test]
    async fn builder_builds_a_base_per_store_across_different_builders() {
        use crate::data::config::image_source::ImageSourceSpec;
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let tmp0 = tempfile::tempdir().unwrap();
        let socket = tmp0.path().join("docker.sock");
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            true,
            false,
            local_docker_sources(&socket),
            local_builder.clone(),
        );
        let git_root = engine.session.git_root().to_path_buf();
        write_agent_dockerfiles(&git_root, &["claude", "codex"]);
        let codex_tag = crate::data::image_tags::agent_image_tag(&git_root, "codex");
        engine.image_sources.images.insert(
            codex_tag.clone(),
            ImageSourceSpec::AppleStore { reference: None },
        );
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        // `--build` skips the `base_exists` inspect; one post-build
        // verification inspect per store (Docker, then Apple).
        for hex in ['5', '6'] {
            let base_id = built_identity(hex, platform.clone());
            local_builder.script_build(&project_tag, Ok(base_id.clone()));
            local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
        }
        let claude_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        local_builder.script_build(&claude_tag, Ok(built_identity('7', platform.clone())));
        local_builder.script_build(&codex_tag, Ok(built_identity('8', platform)));

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        assert_eq!(
            summary.base_image_results.len(),
            2,
            "each store gets its own base-image row: {:?}",
            summary.base_image_results
        );
        let builds = local_builder.build_calls();
        assert_eq!(
            builds.iter().filter(|r| r.reference == project_tag).count(),
            2,
            "the base must be built once per store: {builds:?}"
        );
    }

    /// The audit-rebuild path (a changed `Dockerfile.dev` after the sanctioned
    /// host-agent audit) uses the local builder exactly like a plan-driven
    /// build: this is isolated to `RebuildingAfterAudit` itself (private
    /// fields set directly) so it does not depend on a real agent binary.
    #[tokio::test]
    async fn builder_audit_rebuild_rebuilds_the_changed_project_image() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let tmp0 = tempfile::tempdir().unwrap();
        let socket = tmp0.path().join("docker.sock");
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            local_docker_sources(&socket),
            local_builder.clone(),
        );
        let git_root = engine.session.git_root().to_path_buf();
        let dockerfile = git_root.join("Dockerfile.dev");
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        let base_id = built_identity('9', platform.clone());
        let agent_id = built_identity('0', platform);
        // The rebuild forces `options.build = true` internally, which skips
        // both the agent-reference inspect and the `base_exists` inspect —
        // only the post-build verification inspect is scripted.
        local_builder.script_build(&project_tag, Ok(base_id.clone()));
        local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
        local_builder.script_build(&agent_tag, Ok(agent_id));

        // Drive directly to `RebuildingAfterAudit` with an audit that ran
        // and a Dockerfile that changed, bypassing the real host-agent ping
        // and the audit container the ordinary phase sequence would need.
        engine.summary.audit = StepStatus::Done;
        engine.pre_audit_dockerfile_hash = Some(0);
        std::fs::write(&dockerfile, "FROM scratch\nRUN echo changed\n").unwrap();
        engine.phase = ReadyPhase::RebuildingAfterAudit;

        engine.step(&mut frontend).await.unwrap();

        assert_eq!(engine.phase(), &ReadyPhase::Complete);
        assert_eq!(engine.summary().image_rebuild, StepStatus::Done);
        let builds = local_builder.build_calls();
        assert_eq!(
            builds
                .iter()
                .map(|r| r.reference.clone())
                .collect::<Vec<_>>(),
            vec![project_tag, agent_tag]
        );
    }

    /// An incompatible source (archive) with an attempted build (missing
    /// image) warns exactly once, invokes the local builder zero times, and
    /// still imports normally from the configured source.
    #[tokio::test]
    async fn builder_incompatible_source_warns_once_and_imports_normally() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            archive_sources(),
            local_builder.clone(),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        assert_eq!(local_builder.probe_count(), 0);
        assert_eq!(local_builder.inspect_count(), 0);
        assert!(local_builder.build_calls().is_empty());
        let summary = engine.summary();
        assert_eq!(summary.image_results.len(), 1);
        assert_eq!(summary.image_results[0].warnings.len(), 1);
        match &summary.image_results[0].warnings[0] {
            ReadyImageWarning::IncompatibleBuilder {
                tag, source_kind, ..
            } => {
                assert_eq!(
                    *source_kind,
                    crate::data::config::image_source::ImageSourceKind::Archive
                );
                assert_eq!(tag, &summary.image_results[0].tag);
            }
        }
        assert_eq!(
            frontend
                .warning_texts()
                .iter()
                .filter(|t| t.contains("cannot build Dockerfiles"))
                .count(),
            1,
            "the warning must be written to the frontend exactly once: {:?}",
            frontend.warning_texts()
        );
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
        assert_eq!(summary.image_results[0].import, StepStatus::Done);
    }

    /// With the image already installed and no `--build`, nothing attempts
    /// a build, so the incompatible-source warning is never emitted at all —
    /// the non-build-path silence half of the same requirement.
    #[tokio::test]
    async fn builder_incompatible_source_is_silent_on_a_non_build_path() {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            archive_sources(),
            local_builder.clone(),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        assert!(local_builder.build_calls().is_empty());
        let summary = engine.summary();
        assert!(
            summary.image_results[0].warnings.is_empty(),
            "no warning when no build was attempted: {:?}",
            summary.image_results[0].warnings
        );
        assert!(frontend
            .warning_texts()
            .iter()
            .all(|t| !t.contains("cannot build Dockerfiles")));
    }

    /// A compatible but unreachable builder, with nothing installed yet, is
    /// a build failure — never a silent import of stale or no content.
    #[tokio::test]
    async fn builder_unavailable_with_nothing_installed_fails_the_build() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::unavailable("daemon unreachable"));
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            ImageSources::default(),
            local_builder.clone(),
        );
        let socket = engine.session.git_root().join("docker.sock");
        engine.image_sources = local_docker_sources(&socket);

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        assert_eq!(summary.image_results.len(), 1);
        match &summary.image_results[0].build {
            StepStatus::Failed(msg) => assert!(msg.contains("daemon unreachable"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(summary.image_results[0].import, StepStatus::Skipped);
        assert!(
            runtime.imports.lock().unwrap().is_empty(),
            "an unreachable builder with nothing installed must never import"
        );
        assert!(!summary.is_ready());
        // `agent_image`/`image_source` (set by `record_import_failure`) are
        // checked ahead of the per-image `image_results` rows, so the
        // top-level stage's message is what surfaces here.
        assert!(
            summary
                .not_ready_reason()
                .unwrap()
                .contains("daemon unreachable"),
            "{:?}",
            summary.not_ready_reason()
        );
    }

    /// A compatible but unreachable builder, with an image already installed
    /// and no pending receipt, is a warning, not a failure: the installed
    /// image is reused rather than the whole ready run failing.
    #[tokio::test]
    async fn builder_unavailable_with_an_installed_image_warns_and_reuses_it() {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let local_builder = Arc::new(FakeLocalBuilder::unavailable("daemon unreachable"));
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            ImageSources::default(),
            local_builder.clone(),
        );
        let socket = engine.session.git_root().join("docker.sock");
        engine.image_sources = local_docker_sources(&socket);

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        match &summary.image_results[0].build {
            StepStatus::Warn(msg) => {
                assert!(msg.contains("unreachable"), "{msg}");
                assert!(msg.contains("installed image"), "{msg}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
        assert_eq!(summary.image_results[0].import, StepStatus::Done);
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert!(
            !imports[0].refresh,
            "an unreachable builder with an installed image must not force refresh"
        );
        assert!(
            summary.is_ready(),
            "a build Warn (not Failed) must not block readiness"
        );
    }

    /// A build failure (not an unreachable builder — a failed `docker
    /// build`) fails only its own image's build row, skips the import, and
    /// never calls the importer.
    #[tokio::test]
    async fn builder_build_failure_skips_the_import() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            ImageSources::default(),
            local_builder.clone(),
        );
        let socket = engine.session.git_root().join("docker.sock");
        engine.image_sources = local_docker_sources(&socket);
        let git_root = engine.session.git_root().to_path_buf();
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        local_builder.script_inspect(&agent_tag, Ok(SourceImage::Absent));
        local_builder.script_inspect(&project_tag, Ok(SourceImage::Absent));
        local_builder.script_build(&project_tag, Err("disk full".into()));

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        match &summary.image_results[0].build {
            StepStatus::Failed(msg) => assert!(msg.contains("disk full"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(summary.image_results[0].import, StepStatus::Skipped);
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert!(!summary.is_ready());
    }

    /// An import that returns content different from what the build
    /// produced is a digest mismatch, not a silently accepted substitution.
    #[tokio::test]
    async fn builder_import_content_mismatch_fails_closed() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        // The importer always reports digest 'f', regardless of what the
        // build receipt says it should install — simulating a retag race or
        // a broken export between the build and the import.
        runtime.set_digest_override('f');
        let local_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            ImageSources::default(),
            local_builder.clone(),
        );
        let socket = engine.session.git_root().join("docker.sock");
        engine.image_sources = local_docker_sources(&socket);
        let git_root = engine.session.git_root().to_path_buf();
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        script_missing_image_build(
            &local_builder,
            &project_tag,
            &agent_tag,
            built_identity('a', platform.clone()),
            built_identity('a', platform),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        assert_eq!(summary.image_results[0].build, StepStatus::Done);
        match &summary.image_results[0].import {
            StepStatus::Failed(msg) => assert!(
                msg.to_lowercase().contains("digest") || msg.to_lowercase().contains("mismatch"),
                "{msg}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!summary.is_ready());
    }

    /// A build that succeeds followed by an import failure is reported as
    /// build success + readiness failure; a later `ready` run (sharing the
    /// same lock/receipt roots) must retry only the import, with forced
    /// refresh, from the content the first run already verified it built —
    /// never a stale cache hit — and must settle (clear) the receipt once
    /// the retry succeeds.
    #[tokio::test]
    async fn builder_import_failure_after_a_successful_build_retries_and_settles() {
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let first_runtime = Arc::new(FakeImportRuntime::new(false));
        first_runtime.fail_next_import(1);
        let first_builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, tmp, locks, receipts) = builder_engine(
            first_runtime.clone(),
            false,
            false,
            ImageSources::default(),
            first_builder.clone(),
        );
        let socket = tmp.path().join("docker.sock");
        let sources = local_docker_sources(&socket);
        engine.image_sources = sources.clone();
        let git_root = engine.session.git_root().to_path_buf();
        let project_tag = crate::data::image_tags::project_image_tag(&git_root);
        let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
        let agent_id = built_identity('a', platform.clone());
        script_missing_image_build(
            &first_builder,
            &project_tag,
            &agent_tag,
            built_identity('a', platform.clone()),
            agent_id.clone(),
        );

        step_through_image_phases(&mut engine, &mut frontend).await;

        let summary = engine.summary();
        assert_eq!(
            summary.image_results[0].build,
            StepStatus::Done,
            "the build itself succeeded"
        );
        match &summary.image_results[0].import {
            StepStatus::Failed(msg) => assert!(msg.contains("fake import failure"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!summary.is_ready());
        assert_eq!(first_runtime.imports.lock().unwrap().len(), 1);

        // A second `awman ready`, sharing the lock/receipt roots: the
        // source still holds the build's exact content (the fake reports it
        // present), and nothing needs to be rebuilt.
        let second_runtime = Arc::new(FakeImportRuntime::new(false));
        let second_builder = Arc::new(FakeLocalBuilder::new());
        second_builder.script_inspect(&agent_tag, Ok(SourceImage::Present(agent_id.clone())));
        let (mut engine2, mut frontend2) = reopen_builder_engine(
            &git_root,
            second_runtime.clone(),
            false,
            sources,
            second_builder.clone(),
            locks,
            receipts.clone(),
        );

        step_through_image_phases(&mut engine2, &mut frontend2).await;

        assert!(
            second_builder.build_calls().is_empty(),
            "the retry must not rebuild; the source already holds the receipt's content"
        );
        let imports2 = second_runtime.imports.lock().unwrap();
        assert_eq!(imports2.len(), 1);
        assert!(
            imports2[0].refresh,
            "a pending receipt must force refresh even with no --build/--refresh flag"
        );
        assert_eq!(imports2[0].expected_built_identity, Some(agent_id));
        let summary2 = engine2.summary();
        assert_eq!(summary2.image_results[0].import, StepStatus::Done);
        assert!(summary2.is_ready());

        // The receipt was cleared on the successful retry: a third run must
        // not find anything pending and must not force a refresh on its own.
        let key = PendingBuildKey {
            store: sources_default_store(&socket),
            reference: agent_tag.clone(),
            platform: crate::data::oci_identity::OciPlatform::host_linux(),
            install_tag: agent_tag.clone(),
        };
        assert!(
            receipts.load(&key).unwrap().is_none(),
            "the receipt must be cleared once the retried import is verified installed"
        );
    }

    /// `BuildStoreKey` for the local Docker socket `local_docker_sources`
    /// configures — used only to reconstruct a `PendingBuildKey` for a
    /// direct receipt-store assertion.
    fn sources_default_store(socket: &std::path::Path) -> BuildStoreKey {
        BuildStoreKey::Docker {
            endpoint: format!("unix://{}", socket.display()),
        }
    }

    #[test]
    fn build_reference_normalization_detects_docker_equivalents() {
        for reference in [
            "agent",
            "agent:latest",
            "library/agent",
            "docker.io/library/agent:latest",
            "index.docker.io/library/agent",
        ] {
            assert_eq!(normalize_build_reference(reference), "agent:latest");
        }
        assert_eq!(
            normalize_build_reference("registry.example/team/agent"),
            "registry.example/team/agent:latest"
        );
    }

    #[test]
    fn prevalidation_marks_colliding_agent_outputs_without_probing() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, _frontend, tmp, _locks, _receipts) = builder_engine(
            runtime,
            true,
            false,
            ImageSources::default(),
            builder.clone(),
        );
        write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
        let socket = tmp.path().join("docker.sock");
        for agent in ["claude", "codex"] {
            let tag = crate::data::image_tags::agent_image_tag(tmp.path(), agent);
            engine.image_sources.images.insert(
                tag,
                crate::data::config::image_source::ImageSourceSpec::DockerStore {
                    host: Some(format!("unix://{}", socket.display())),
                    tls: None,
                    reference: Some("team/shared:latest".into()),
                },
            );
        }
        engine.prevalidate_build_targets(tmp.path());
        assert_eq!(engine.prevalidation_failures.len(), 2);
        assert!(engine
            .prevalidation_failures
            .values()
            .all(|reason| reason.contains("same local image reference")));
        assert_eq!(builder.probe_count(), 0);
    }

    #[tokio::test]
    async fn unbound_source_with_pending_receipt_forces_fresh_import() {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, tmp, _locks, receipts) = builder_engine(
            runtime,
            false,
            false,
            ImageSources::default(),
            builder.clone(),
        );
        let socket = tmp.path().join("docker.sock");
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let endpoint = crate::engine::oci::docker_engine::Endpoint::Unix(socket.clone());
        let resolved = crate::engine::oci::ResolvedImageSource {
            source: crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some(format!("unix://{}", socket.display())),
                tls: None,
                reference: None,
            },
            reference: tag.clone(),
            platform: platform.clone(),
            store: endpoint.describe(),
            docker_endpoint: Some(endpoint),
        };
        let key = PendingBuildKey {
            store: sources_default_store(&socket),
            reference: normalize_build_reference(&tag),
            platform: platform.clone(),
            install_tag: tag.clone(),
        };
        receipts
            .save(
                &PendingBuiltImage {
                    key: key.clone(),
                    identity: built_identity('a', platform),
                },
                || Ok(()),
            )
            .unwrap();
        builder.script_inspect(&tag, Ok(SourceImage::Unbound));
        let prepared = engine
            .prepare_local_image(&mut frontend, &resolved, &tag, "claude", tmp.path())
            .await
            .unwrap();
        assert!(prepared.force_refresh);
        assert!(prepared.expected.is_none());
        assert_eq!(prepared.pending_key, Some(key.clone()));
        assert!(receipts.load(&key).unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_prevalidation_closes_the_cached_image_progress_row() {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut engine, mut frontend, tmp, _locks, _receipts) =
            builder_engine(runtime, false, false, ImageSources::default(), builder);
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        engine.image_sources = local_docker_sources(&tmp.path().join("docker.sock"));
        engine
            .prevalidation_failures
            .insert(tag.clone(), "collision".into());
        assert!(engine
            .import_agent_image(&mut frontend, tmp.path(), "claude")
            .await
            .is_err());
        let cached = frontend
            .statuses
            .iter()
            .filter(|(step, _)| step.contains("cached from"));
        let statuses: Vec<_> = cached.map(|(_, status)| status).collect();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0], &StepStatus::Running);
        assert_eq!(statuses[1], &StepStatus::Failed("collision".into()));
    }
}
