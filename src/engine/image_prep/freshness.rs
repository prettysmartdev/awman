//! Content checks and the shared build-lease primitive. Callers decide when
//! to rebuild and whether a failed build may keep using an older image.
use super::*;
use crate::data::image_inputs::{InputsHash, RepoKey, INPUTS_LABEL, REPO_LABEL};
use crate::engine::container::runtime::ImageMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageFreshness {
    Fresh(ImageMetadata),
    Stale(ImageMetadata),
    Unlabelled(ImageMetadata),
    Missing,
}

impl ImageFreshness {
    pub fn from_image(image: Option<ImageMetadata>, expected: &InputsHash) -> Self {
        match image {
            None => Self::Missing,
            Some(image) => match image.labels.get(INPUTS_LABEL) {
                None => Self::Unlabelled(image),
                Some(label) if expected.matches_label(Some(label)) => Self::Fresh(image),
                Some(_) => Self::Stale(image),
            },
        }
    }
}

/// A build into the selected runtime or an explicit source store. The store
/// key must identify that endpoint; references are normalized
/// before locking so `foo` and `docker.io/library/foo:latest` share a lease.
pub struct ImageBuildSpec {
    pub request: BuildRequest,
    pub store: BuildStoreKey,
    pub inputs: InputsHash,
    pub repo: RepoKey,
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageBuildOutcome {
    Built,
    Reused,
}

/// Keep this result alive until export/import finishes, so a concurrent
/// caller cannot retag the source between the build and its import.
pub struct PreparedSourceImage {
    pub identity: BuiltImageIdentity,
    pub outcome: ImageBuildOutcome,
    _lease: ImageBuildLease,
}

impl AgentImagePreparer {
    /// Runtime builds share the session/lease lock and pin an agent's parent
    /// while that parent's tag is locked. The worker owns locks and temporary
    /// recipes until the actual CLI exits, even if its awaiting task is dropped.
    pub(crate) async fn build_runtime_coordinated(
        &self,
        request: crate::engine::container::BuildRequest,
        sink: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<(), EngineError> {
        let store = crate::engine::agent::image_freshness::runtime_build_store(
            self.runtime.as_ref(),
            &self.definition_env,
        );
        let locks = self.build_locks.clone();
        let runtime = self.runtime.clone();
        let env = self.definition_env.clone();
        let parent = crate::data::image_tags::project_image_tag(&request.context);
        let reference = crate::engine::image_prep::AgentImagePreparer::normalize_build_reference(
            &request.reference,
        );
        let parent_ref =
            crate::engine::image_prep::AgentImagePreparer::normalize_build_reference(&parent);
        let cancel = self.cancel.clone();
        crate::engine::image_prep::bridge::stream(
            cancel.clone(),
            move |tx| {
                let check = || {
                    cancel
                        .check()
                        .map_err(|e| crate::data::error::DataError::Other(e.to_string()))
                };
                let _parent_lease = locks.lock(&store, &parent_ref, check)?;
                let _agent_lease = if reference != parent_ref {
                    Some(locks.lock(&store, &reference, check)?)
                } else {
                    None
                };
                let mut request =
                    crate::engine::image_prep::freshness::label_build(&request, &env, |tag| {
                        Ok(runtime.inspect_image(tag)?.map(|image| image.id))
                    })?;
                let mut recipe = None;
                if reference != parent_ref {
                    if let Some(base) = runtime.inspect_image(&parent)? {
                        let bytes = std::fs::read(&request.dockerfile)
                            .map_err(|e| EngineError::io(&request.dockerfile, e))?;
                        let text =
                            crate::data::image_inputs::bind_project_base(&bytes, &parent, &base.id)
                                .map_err(EngineError::Config)?;
                        let pinned = tempfile::NamedTempFile::new()
                            .map_err(|e| EngineError::io(&request.dockerfile, e))?;
                        std::fs::write(pinned.path(), text)
                            .map_err(|e| EngineError::io(pinned.path(), e))?;
                        request.dockerfile = pinned.path().into();
                        recipe = Some(pinned);
                    }
                }
                let result = runtime.build_image(&request, &mut |line| {
                    let _ = tx.blocking_send(line.to_string());
                });
                drop(recipe);
                result
            },
            |line: String| sink.build_output(&line),
        )
        .await
    }

    pub fn for_runtime(
        runtime: Arc<dyn AgentRuntimeEngine>,
        data_home: PathBuf,
        context: ImageBuildContext,
    ) -> Self {
        Self::with_inputs(ImageSources::default(), false, runtime, data_home, context)
    }

    pub fn freshness(
        &self,
        tag: &str,
        expected: &InputsHash,
    ) -> Result<ImageFreshness, EngineError> {
        Ok(ImageFreshness::from_image(
            self.runtime.inspect_image(tag)?,
            expected,
        ))
    }

    /// Recheck *after* acquiring the existing cross-process build lease.
    /// The lease stays held through the build and final label verification.
    /// A failed build is returned unchanged; no warn-and-continue policy here.
    pub async fn build_if_needed(
        &self,
        build: ImageBuildSpec,
        sink: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<ImageBuildOutcome, EngineError> {
        build.request.require_runtime_build()?;
        let reference = Self::normalize_build_reference(&build.request.reference);
        let lease = self
            .build_lock(build.store, reference, sink)
            .await
            .map_err(EngineError::Other)?;
        let runtime = self.runtime.clone();
        let tag = build.request.reference.clone();
        let expected = build.inputs.clone();
        let check_tag = tag.clone();
        let check_runtime = runtime.clone();
        let expected_repo = build.repo.as_str().to_string();
        let fresh = self
            .blocking(move || {
                check_runtime.inspect_image(&check_tag).map(|image| {
                    let same_repo = image
                        .as_ref()
                        .is_some_and(|image| image.labels.get(REPO_LABEL) == Some(&expected_repo));
                    match ImageFreshness::from_image(image, &expected) {
                        ImageFreshness::Fresh(image) if !same_repo => ImageFreshness::Stale(image),
                        freshness => freshness,
                    }
                })
            })
            .await??;
        if !build.force && matches!(fresh, ImageFreshness::Fresh(_)) {
            return Ok(ImageBuildOutcome::Reused);
        }
        let request = build.request.with_inputs(&build.inputs, &build.repo);
        sink.build_started(&tag);
        let result = super::bridge::stream(
            self.cancel.clone(),
            move |tx| {
                // The blocking worker owns the lease. Dropping its awaiting
                // future must not release it while a CLI is still building.
                runtime.build_image(&request, &mut |line| {
                    let _ = tx.blocking_send(line.to_owned());
                })?;
                Ok(lease)
            },
            |line: String| sink.build_output(&line),
        )
        .await;
        sink.build_finished(
            &tag,
            result.as_ref().err().map(ToString::to_string).as_deref(),
        );
        let _lease = result?;
        let runtime = self.runtime.clone();
        let expected = build.inputs;
        self.blocking(move || {
            if matches!(
                ImageFreshness::from_image(runtime.inspect_image(&tag)?, &expected),
                ImageFreshness::Fresh(_)
            ) {
                Ok(ImageBuildOutcome::Built)
            } else {
                Err(EngineError::Container(format!(
                    "{tag}: build did not install the expected inputs label"
                )))
            }
        })
        .await?
    }

    pub async fn source_freshness(
        &mut self,
        target: builder::BuildTarget,
        reference: String,
        expected: &InputsHash,
    ) -> Result<ImageFreshness, EngineError> {
        let builder = self.builder();
        let control = crate::engine::container::BuildControl::source_build(self.cancel.clone());
        let image = self
            .blocking(move || builder.inspect_metadata(&target, &reference, &control))
            .await??;
        Ok(ImageFreshness::from_image(image, expected))
    }

    /// Source-store counterpart of `build_if_needed`. Both use the same
    /// build lock store as ready's receipt/import path.
    pub async fn build_source_if_needed(
        &mut self,
        spec: &ImageBuildSpec,
        sink: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<PreparedSourceImage, EngineError> {
        let target_store = match &spec.request.target {
            builder::BuildTarget::Docker { endpoint } => BuildStoreKey::Docker {
                endpoint: endpoint.clone(),
            },
            builder::BuildTarget::Apple => BuildStoreKey::AppleLocal,
            builder::BuildTarget::Runtime => {
                return Err(EngineError::Config(
                    "source build requires an explicit source store".into(),
                ))
            }
        };
        if target_store != spec.store {
            return Err(EngineError::Config(
                "image build lock does not match its source store".into(),
            ));
        }
        let lease = self
            .build_lock(
                spec.store.clone(),
                Self::normalize_build_reference(&spec.request.reference),
                sink,
            )
            .await
            .map_err(EngineError::Other)?;
        let cached = if spec.force {
            None
        } else {
            match self
                .source_freshness(
                    spec.request.target.clone(),
                    spec.request.reference.clone(),
                    &spec.inputs,
                )
                .await?
            {
                ImageFreshness::Fresh(image) => Some(image),
                _ => None,
            }
        };
        let cached_identity = if let Some(image) = cached {
            match self
                .inspect_source(
                    spec.request.target.clone(),
                    spec.request.reference.clone(),
                    spec.request.platform.clone(),
                )
                .await?
            {
                builder::SourceImage::Present(identity) => Some(identity),
                builder::SourceImage::Unbound
                    if spec.request.target == builder::BuildTarget::Apple =>
                {
                    // Older Apple inspection only reports an index. Rebuild
                    // to obtain a manifest receipt rather than treating that
                    // index as a selected platform's manifest.
                    image
                        .manifest_digest
                        .map(|digest| {
                            BuiltImageIdentity::new(
                                None,
                                Some(digest),
                                spec.request.platform.clone(),
                            )
                            .map_err(EngineError::Config)
                        })
                        .transpose()?
                }
                _ => {
                    return Err(EngineError::Container(
                        "source image has no single-platform build identity".into(),
                    ))
                }
            }
        } else {
            None
        };
        let (identity, outcome, lease) = if let Some(identity) = cached_identity {
            (identity, ImageBuildOutcome::Reused, lease)
        } else {
            let request = spec.request.clone().with_inputs(&spec.inputs, &spec.repo);
            let builder = self.builder();
            let control = crate::engine::container::BuildControl::source_build(self.cancel.clone());
            sink.build_started(&spec.request.reference);
            let result = super::bridge::stream(
                self.cancel.clone(),
                move |tx| {
                    let identity = builder.build(&request, &control, &mut |line| {
                        let _ = tx.blocking_send(line.to_owned());
                    })?;
                    Ok((identity, lease))
                },
                |line: String| sink.build_output(&line),
            )
            .await;
            sink.build_finished(
                &spec.request.reference,
                result.as_ref().err().map(ToString::to_string).as_deref(),
            );
            let (identity, lease) = result?;
            if !matches!(
                self.source_freshness(
                    spec.request.target.clone(),
                    spec.request.reference.clone(),
                    &spec.inputs
                )
                .await?,
                ImageFreshness::Fresh(_)
            ) {
                return Err(EngineError::Container(
                    "source build did not publish the expected inputs label".into(),
                ));
            }
            (identity, ImageBuildOutcome::Built, lease)
        };
        Ok(PreparedSourceImage {
            identity,
            outcome,
            _lease: lease,
        })
    }

    pub(super) fn repo_labels(&self) -> std::collections::BTreeMap<String, String> {
        [(
            REPO_LABEL.into(),
            repo_key(&self.context.git_root, &self.definition_env)
                .as_str()
                .into(),
        )]
        .into()
    }
}

/// Resolve the store used by the runtime CLI, including Docker's persistent
/// context selection. Ready, session refresh and lease cleanup share this key.
pub(crate) fn runtime_store_key(
    runtime: &dyn AgentRuntimeEngine,
    env: &crate::data::config::env::EnvSnapshot,
) -> Result<BuildStoreKey, EngineError> {
    match runtime.runtime_name() {
        "apple-containers" => Ok(BuildStoreKey::AppleLocal),
        "docker" => {
            if let (Some(context), _) = env.runtime_endpoint() {
                return Ok(BuildStoreKey::Docker {
                    endpoint: format!("docker-context:{context}"),
                });
            }
            // The Docker CLI also supports endpoints the builtin source
            // exporter cannot open (SSH and Windows pipes). Keep them isolated
            // using a non-secret key without requiring the exporter to run.
            if let Some(host) = env
                .docker_host()
                .filter(|host| host.starts_with("npipe:") || host.starts_with("ssh:"))
            {
                return Ok(BuildStoreKey::Docker {
                    endpoint: format!("docker-cli:{}", crate::data::fs::hash::sha256_hex(&host)),
                });
            }
            if env.docker_host().is_none() {
                let config = env
                    .get("DOCKER_CONFIG")
                    .map(std::path::PathBuf::from)
                    .or_else(|| env.home_dir().map(|home| home.join(".docker")))
                    .map(|dir| dir.join("config.json"));
                if let Some(path) = config {
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            let value: serde_json::Value =
                                serde_json::from_slice(&bytes).map_err(|_| {
                                    EngineError::Config(
                                        "cannot read Docker context configuration".into(),
                                    )
                                })?;
                            if let Some(context) = value
                                .get("currentContext")
                                .and_then(serde_json::Value::as_str)
                                .filter(|c| !c.is_empty() && *c != "default")
                            {
                                return Ok(BuildStoreKey::Docker {
                                    endpoint: format!("docker-context:{context}"),
                                });
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(EngineError::io(&path, error)),
                    }
                }
            }
            let endpoint = crate::engine::oci::docker_engine::resolve_endpoint(None, None, env)?;
            Ok(BuildStoreKey::Docker {
                endpoint: endpoint.describe(),
            })
        }
        _ => Err(EngineError::NotImplemented("lease image store")),
    }
}

/// Resolve repository attribution once using the same environment as builds.
pub fn repo_key(root: &std::path::Path, env: &crate::data::config::env::EnvSnapshot) -> RepoKey {
    let origin = crate::engine::git::GitEngine::with_env(env.clone())
        .remote_url(root, "origin")
        .ok();
    RepoKey::for_repo(origin.as_deref(), root)
}

/// Label compatibility calls into ContainerRuntime as well as the new shared
/// build entry point. Explicit hashes always take precedence (lease tags need
/// not resemble the legacy project/agent tags).
pub(crate) fn label_runtime_build(
    runtime: &crate::engine::container::ContainerRuntime,
    request: &BuildRequest,
) -> Result<BuildRequest, EngineError> {
    label_build(request, runtime.env(), |reference| {
        Ok(runtime.image_id(reference))
    })
}

pub(crate) fn label_build(
    request: &BuildRequest,
    env: &crate::data::config::env::EnvSnapshot,
    mut image_id: impl FnMut(&str) -> Result<Option<String>, EngineError>,
) -> Result<BuildRequest, EngineError> {
    let mut labelled = request.clone();
    if !labelled.labels.contains_key(REPO_LABEL) {
        labelled.labels.insert(
            REPO_LABEL.into(),
            repo_key(&request.context, env).as_str().into(),
        );
    }
    if labelled.labels.contains_key(INPUTS_LABEL) {
        return Ok(labelled);
    }
    let bytes = match std::fs::read(&request.dockerfile) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(EngineError::io(&request.dockerfile, e)),
    };
    let agent_file = request.reference
        != crate::data::image_tags::project_image_tag(&request.context)
        && request
            .dockerfile
            .parent()
            .and_then(std::path::Path::file_name)
            .is_some_and(|name| name == ".awman")
        && request
            .dockerfile
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("Dockerfile."));
    let inputs = if agent_file {
        let bytes = bytes.ok_or_else(|| {
            EngineError::Config(format!(
                "agent Dockerfile {} is unavailable",
                request.dockerfile.display()
            ))
        })?;
        let project_tag = crate::data::image_tags::project_image_tag(&request.context);
        let project_id = image_id(&project_tag)?.ok_or_else(|| {
            EngineError::Container(format!(
                "project image {project_tag} is unavailable for agent input hashing"
            ))
        })?;
        crate::data::image_inputs::agent_inputs_hash(&bytes, &project_id)
    } else {
        crate::data::image_inputs::project_inputs_at(
            bytes.as_deref(),
            &crate::data::image_inputs::DirBuildContext::new(&request.context),
            &request
                .dockerfile
                .strip_prefix(&request.context)
                .unwrap_or(&request.dockerfile)
                .to_string_lossy(),
        )
        .hash
    };
    labelled
        .labels
        .insert(INPUTS_LABEL.into(), inputs.as_str().into());
    Ok(labelled)
}
