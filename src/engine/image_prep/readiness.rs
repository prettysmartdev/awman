//! Read-only image preparation decisions for the workflow prerequisite review.
//! These run inside the preflight's bounded probe workers. They never provision
//! recipes, take build leases, acquire images or alter pending-build receipts.
use super::*;
use crate::data::image_inputs::{
    agent_inputs_hash, project_inputs_at, DirBuildContext, InputsHash,
};
use crate::data::image_tags::project_image_tag;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::{preview_agent_dockerfile, AgentRegistry};
use crate::engine::agent_runtime::ImageAcquisition;
use crate::engine::container::BuildControl;
use crate::engine::oci::SourceImage;
use std::path::Path;

/// The same project hash used by review and refresh, including configured
/// Dockerfile paths, copied inputs and Dockerfile-specific ignore files.
pub(crate) fn project_inputs_from_disk(
    root: &Path,
    dockerfile: &Path,
) -> Result<Option<InputsHash>, EngineError> {
    let bytes = match std::fs::read(dockerfile) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(EngineError::io(dockerfile, e)),
    };
    Ok(Some(
        project_inputs_at(
            Some(&bytes),
            &DirBuildContext::new(root),
            &dockerfile
                .strip_prefix(root)
                .unwrap_or(dockerfile)
                .to_string_lossy(),
        )
        .hash,
    ))
}

/// `None` means no preparation is needed; `Some` explains the pending work.
pub(crate) fn workflow_image_preparation(
    runtime: Arc<dyn AgentRuntimeEngine>,
    session: &Session,
    agent: &str,
    tag: &str,
) -> Result<Option<String>, EngineError> {
    if runtime.capabilities().image_acquisition == ImageAcquisition::Kit {
        return Ok(None);
    }
    if !runtime.image_exists(tag)? {
        return Ok(Some(format!(
            "image {tag} must be prepared before the workflow starts"
        )));
    }
    // Lease provisioning already chose immutable content-keyed tags. Do not
    // compare them to the checkout's ordinary, folder-named images.
    if session.lease_images().is_some() {
        return Ok(None);
    }
    let config = session.effective_config();
    let context = ImageBuildContext::for_repo(
        session.git_root(),
        &config,
        session.configured_agent().as_str(),
    );
    let home = crate::data::fs::data_paths::data_home(session.env())?;
    let definition = AgentRegistry::resolve_for_session(session, &AgentName::new(agent)?)?;
    let mut preparer = if runtime.capabilities().image_acquisition == ImageAcquisition::Import {
        AgentImagePreparer::new(config.builtin_settings()?, runtime, home, context)
    } else {
        AgentImagePreparer::for_runtime(runtime, home, context)
    }
    .with_definition_env(session.env().clone())
    .with_definition(definition);
    preparer.preparation_reason(tag, agent)
}

impl ImageFreshness {
    fn preparation_reason(&self, tag: &str) -> Option<String> {
        match self {
            Self::Fresh(_) => None,
            Self::Stale(_) => Some(format!(
                "image {tag} is out of date with its build inputs; rebuild required"
            )),
            Self::Unlabelled(_) => {
                Some(format!("image {tag} has no inputs label; rebuild required"))
            }
            Self::Missing => Some(format!("image {tag} is missing; preparation required")),
        }
    }
}

impl AgentImagePreparer {
    pub(crate) fn preparation_reason(
        &mut self,
        tag: &str,
        agent: &str,
    ) -> Result<Option<String>, EngineError> {
        let root = self.context.git_root.clone();
        let project_tag = project_image_tag(&root);
        if self.runtime.capabilities().image_acquisition == ImageAcquisition::Build {
            let project = self.runtime.inspect_image(&project_tag)?;
            let Some(project) = project else {
                return Ok(Some(format!(
                    "project image {project_tag} is missing; run awman ready"
                )));
            };
            if let Some(expected) =
                project_inputs_from_disk(&root, &self.context.project_dockerfile)?
            {
                if let Some(reason) = ImageFreshness::from_image(Some(project.clone()), &expected)
                    .preparation_reason(&project_tag)
                {
                    return Ok(Some(format!(
                        "{reason}; agent image {tag} must be refreshed after it"
                    )));
                }
            }
            let Some(bytes) = self.preview_recipe(agent, &project_tag)? else {
                return Ok(Some(format!("image {tag} requires its agent Dockerfile to be downloaded before freshness can be checked")));
            };
            let expected = agent_inputs_hash(&bytes, &project.id);
            return Ok(self.freshness(tag, &expected)?.preparation_reason(tag));
        }

        let plan = self.plan(tag, agent);
        if let Some(error) = plan.error.or(plan.pinned_default) {
            return Err(EngineError::Other(error));
        }
        let Some(source) = plan.source else {
            return Ok(None); // Preparation also retains an installed image without a source.
        };
        if let Some(store) = &source.store_key {
            let key = PendingBuildKey {
                store: store.clone(),
                reference: Self::normalize_build_reference(&source.reference),
                platform: source.platform.clone(),
                install_tag: tag.into(),
            };
            if self.pending_builds.load(&key)?.is_some() {
                return Ok(Some(format!(
                    "image {tag} has a successful build awaiting import"
                )));
            }
        }
        let mut expected_built_identity = None;
        if let BuilderSelection::Local(target) = plan.selection {
            let builder = self.builder();
            let control = BuildControl::source_build(self.cancel.clone());
            // Source builds use the default project recipe when none exists.
            let expected = project_inputs_from_disk(&root, &self.context.project_dockerfile)?
                .unwrap_or_else(|| {
                    project_inputs_at(
                        None,
                        &DirBuildContext::new(&root),
                        &self
                            .context
                            .project_dockerfile
                            .strip_prefix(&root)
                            .unwrap_or(&self.context.project_dockerfile)
                            .to_string_lossy(),
                    )
                    .hash
                });
            let parent = ImageFreshness::from_image(
                builder.inspect_metadata(&target, &project_tag, &control)?,
                &expected,
            );
            if let Some(reason) = parent.preparation_reason(&project_tag) {
                return Ok(Some(format!(
                    "{reason}; agent image {tag} must be prepared and imported after it"
                )));
            }
            let ImageFreshness::Fresh(parent) = parent else {
                unreachable!()
            };
            let Some(bytes) = self.preview_recipe(agent, &project_tag)? else {
                return Ok(Some(format!("image {tag} requires its agent Dockerfile before source freshness can be checked")));
            };
            let expected = agent_inputs_hash(&bytes, &parent.id);
            let image = ImageFreshness::from_image(
                builder.inspect_metadata(&target, &source.reference, &control)?,
                &expected,
            );
            if let Some(reason) = image.preparation_reason(&source.reference) {
                return Ok(Some(reason));
            }
            match builder.inspect(&target, &source.reference, &source.platform, &control)? {
                SourceImage::Present(identity) => expected_built_identity = Some(identity),
                SourceImage::Absent => {
                    return Ok(Some(format!(
                        "source image for {tag} must be rebuilt and imported"
                    )))
                }
                SourceImage::Unbound => {}
            }
        }
        let request = ImageImportRequest {
            labels: Default::default(),
            tag: tag.into(),
            source: source.source,
            platform: source.platform,
            refresh: false,
            expected_built_identity,
            immutable_export_selector: None,
            registries: self.image_sources.registries.clone(),
        };
        Ok((!self.runtime.image_matches_source(&request)?)
            .then(|| format!("image {tag} must be imported from the configured source")))
    }

    fn preview_recipe(
        &mut self,
        agent: &str,
        project_tag: &str,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let definition = self.definition(agent)?;
        let paths = crate::data::RepoDockerfilePaths::new(&self.context.git_root);
        preview_agent_dockerfile(&definition, &paths, project_tag)?
            .effective_bytes(&paths.agent_dockerfile(agent))
    }
}
