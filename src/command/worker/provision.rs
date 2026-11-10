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
    session: &Session,
    item: &awman_wsp::WorkItem,
    private_dir: &std::path::Path,
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
    let mut generated = Vec::new();
    for name in names {
        let agent = AgentName::new(&name).map_err(|e| invalid(e.to_string()))?;
        if paths.agent_dockerfile(&name).exists() {
            continue;
        }
        if !crate::engine::agent::first_party::is_first_party(&name) {
            return Err(invalid(format!("agent '{name}' has no Dockerfile; run `awman init --agent {name}` in the repository")));
        }
        let definition = engines
            .agent_engine
            .resolve_definition(session, &agent)
            .map_err(|e| invalid(e.to_string()))?;
        crate::engine::agent::provision_agent_dockerfile(
            &definition,
            &paths,
            &crate::data::image_tags::project_image_tag(session.git_root()),
        )
        .await
        .map_err(|e| invalid(e.to_string()))?;
        generated.push(format!("/.awman/Dockerfile.{name}"));
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
