//! Prepare missing first-party recipes in a disposable pulled checkout.
use crate::command::dispatch::Engines;
use crate::data::{
    config::leader_spec::LeaderSpec,
    session::{AgentName, Session},
    RepoDockerfilePaths,
};
use crate::engine::worker::error::WorkerEngineError;
use std::collections::BTreeSet;

pub(super) async fn agents_for_lease(
    engines: &Engines,
    session: &mut Session,
    item: &awman_wsp::WorkItem,
    private_dir: &std::path::Path,
    db: std::sync::Arc<crate::data::fs::api_db::SqliteSessionStore>,
    lease: &crate::data::worker::LeaseRecord,
) -> Result<(), WorkerEngineError> {
    let invalid = |e: String| WorkerEngineError::Invalid(e);
    let paths = RepoDockerfilePaths::new(session.git_root());
    let mut names = BTreeSet::new();
    if item.exec.dynamic {
        let dynamic = session
            .effective_config()
            .dynamic_workflows()
            .map_err(|e| invalid(e.to_string()))?;
        let leader = item
            .exec
            .leader
            .as_deref()
            .or_else(|| dynamic.default_leader.as_ref().map(|v| v.value.as_str()));
        names.insert(match leader {
            Some(leader) => {
                LeaderSpec::parse(leader)
                    .map_err(|_| invalid("invalid lease leader".into()))?
                    .agent
            }
            None => session.configured_agent().to_string(),
        });
        if let Some(mapping) = dynamic.agents_to_models {
            names.extend(mapping.value.into_keys());
        }
    } else if let Some(toml) = &item.exec.workflow_toml {
        let workflow = crate::data::workflow_definition::Workflow::parse(
            toml,
            crate::data::workflow_definition::WorkflowFormat::Toml,
        )
        .map_err(|e| invalid(e.to_string()))?;
        for step in &workflow.steps {
            names.insert(
                step.agent
                    .clone()
                    .or_else(|| workflow.agent.clone())
                    .or_else(|| item.exec.agent.clone())
                    .unwrap_or_else(|| session.configured_agent().to_string()),
            );
        }
    } else {
        names.insert(
            item.exec
                .agent
                .clone()
                .unwrap_or_else(|| session.configured_agent().to_string()),
        );
    }
    // Resolve every requested agent before creating a lease image builder or
    // building the project image. A stale former-built-in name must fail here
    // even when a matching Dockerfile or agent image remains on disk.
    for name in &names {
        let agent = AgentName::new(name.as_str()).map_err(|e| invalid(e.to_string()))?;
        crate::engine::agent::AgentRegistry::resolve_for_session(session, &agent)
            .map_err(|e| invalid(e.to_string()))?;
    }
    let builder = if super::images::supported(engines.runtime.as_ref()) {
        Some(
            super::images::LeaseImageBuilder::new(
                engines.runtime.clone(),
                session,
                db.clone(),
                lease,
            )
            .map_err(|e| invalid(e.to_string()))?,
        )
    } else {
        None
    };
    let repo = crate::data::image_inputs::RepoKey::for_lease(&item.repo.url, session.git_root());
    let mut binding = crate::data::worker::lease_images::LeaseImageBinding::default();
    let mut project_id = String::new();
    if let Some(builder) = &builder {
        let dockerfile = session
            .effective_config()
            .dockerfile_path(session.git_root());
        let relative = dockerfile
            .strip_prefix(session.git_root())
            .map_err(|_| invalid("lease Dockerfile must be inside the repository".into()))?;
        let bytes = match crate::data::image_inputs::BuildContext::read(
            &crate::data::image_inputs::DirBuildContext::new(session.git_root()),
            &relative.to_string_lossy(),
        ) {
            Ok(bytes) => Some(bytes),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&dockerfile).is_err() =>
            {
                None
            }
            Err(error) => return Err(invalid(error.to_string())),
        };
        let mut inputs = crate::data::image_inputs::project_inputs_at(
            bytes.as_deref(),
            &crate::data::image_inputs::DirBuildContext::new(session.git_root()),
            &dockerfile
                .strip_prefix(session.git_root())
                .map_err(|_| invalid("lease Dockerfile must be inside the repository".into()))?
                .to_string_lossy(),
        );
        for source in &inputs.unresolved {
            tracing::warn!(source = %source.source, reason = ?source.reason, "lease image input cannot be resolved; use ready --build to refresh fetched or unresolved content");
        }
        if !inputs.unresolved.is_empty() {
            inputs.hash = inputs.hash.scoped_to_lease(lease.id.as_str());
        }
        // Repositories with no recipe share awman's default project image.
        let base_repo = if inputs.dockerfile_present {
            repo.clone()
        } else {
            crate::data::image_inputs::RepoKey::from_label("awman/default-base")
        };
        binding.project =
            crate::data::image_tags::lease_project_image_tag(&base_repo, &inputs.hash);
        let dockerfile = if bytes.is_none() {
            let path = private_dir.join("Dockerfile.project");
            crate::data::fs::config_file::write_config_file_within(
                private_dir,
                &path,
                crate::data::templates::project_dockerfile_dev(),
            )
            .map_err(|e| invalid(e.to_string()))?;
            path
        } else {
            dockerfile
        };
        project_id = builder
            .prepare(
                crate::engine::container::BuildRequest::runtime(
                    &binding.project,
                    &dockerfile,
                    session.git_root(),
                    false,
                ),
                inputs.hash,
                base_repo,
                crate::data::worker::lease_images::LeaseImageKind::Project,
            )
            .await
            .map_err(|e| invalid(e.to_string()))?;
    }
    let mut generated = Vec::new();
    for name in &names {
        let agent = AgentName::new(name.as_str()).map_err(|e| invalid(e.to_string()))?;
        if paths.agent_dockerfile(name).exists() {
            continue;
        }
        if !crate::engine::agent::first_party::is_first_party(name) {
            return Err(invalid(format!("agent '{name}' has no Dockerfile; run `awman init --agent {name}` in the repository")));
        }
        let definition = engines
            .agent_engine
            .resolve_definition(session, &agent)
            .map_err(|e| invalid(e.to_string()))?;
        crate::engine::agent::provision_agent_dockerfile(
            &definition,
            &paths,
            &if builder.is_some() {
                binding.project.clone()
            } else {
                crate::data::image_tags::project_image_tag(session.git_root())
            },
        )
        .await
        .map_err(|e| invalid(e.to_string()))?;
        generated.push(format!("/.awman/Dockerfile.{name}"));
    }
    if let Some(builder) = &builder {
        for name in &names {
            let bytes = crate::data::image_inputs::BuildContext::read(
                &crate::data::image_inputs::DirBuildContext::new(session.git_root()),
                &format!(".awman/Dockerfile.{name}"),
            )
            .map_err(|e| invalid(e.to_string()))?;
            let mut hash = crate::data::image_inputs::agent_inputs_hash(&bytes, &project_id);
            if !crate::data::image_inputs::resolve_copy_sources(&String::from_utf8_lossy(&bytes))
                .is_empty()
            {
                tracing::warn!(agent = %name, "agent recipe copies build context; image reuse is limited to this lease");
                hash = hash.scoped_to_lease(lease.id.as_str());
            }
            let tag = crate::data::image_tags::lease_agent_image_tag(&repo, &hash, name);
            let recipe = super::images::agent_recipe(
                &bytes,
                &crate::data::image_tags::project_image_tag(session.git_root()),
                &project_id,
                &private_dir.join(format!("Dockerfile.{name}")),
            )
            .map_err(|e| invalid(e.to_string()))?;
            builder
                .prepare(
                    crate::engine::container::BuildRequest::runtime(
                        &tag,
                        &recipe,
                        session.git_root(),
                        false,
                    ),
                    hash,
                    repo.clone(),
                    crate::data::worker::lease_images::LeaseImageKind::Agent {
                        agent: name.clone(),
                    },
                )
                .await
                .map_err(|e| invalid(e.to_string()))?;
            binding.agents.insert(name.clone(), tag);
        }
        session.set_lease_images(binding);
        let _ = super::images::schedule_cleanup(
            engines.runtime.clone(),
            session.env().clone(),
            db,
            super::images::budgets(&session.effective_config().daemon().worker),
        )
        .await;
    }
    if !generated.is_empty() {
        let path = private_dir.join("generated-recipes");
        let mut recipes: Vec<String> = crate::data::fs::private_files::read_private_json(&path)
            .map_err(|e| invalid(e.to_string()))?
            .unwrap_or_default();
        recipes.extend(generated.iter().cloned());
        recipes.sort();
        recipes.dedup();
        crate::data::fs::private_files::write_private_json_atomic(&path, &recipes, || Ok(()))
            .map_err(|e| invalid(e.to_string()))?;
        // Generated recipes are host preparation. A deliberate git add -f can
        // still publish one, but committing ordinary agent leftovers cannot.
        let common = engines
            .git_engine
            .git_common_dir(session.git_root())
            .map_err(|e| invalid(e.to_string()))?;
        let exclude = common.join("info/exclude");
        let mut content = std::fs::read_to_string(&exclude).unwrap_or_default();
        content.push('\n');
        content.push_str(&generated.join("\n"));
        content.push('\n');
        crate::data::fs::config_file::write_config_file_within(&common, &exclude, &content)
            .map_err(|e| invalid(e.to_string()))?;
    }
    Ok(())
}
