use super::*;
impl AgentImagePreparer {
    /// Resolve before image work, including when an old image or Dockerfile remains.
    pub(super) fn definition(
        &mut self,
        agent: &str,
    ) -> Result<crate::engine::agent::ResolvedAgent, EngineError> {
        if let Some(def) = self.definitions.get(agent) {
            return Ok(def.clone());
        }
        let name = crate::data::session::AgentName::new(agent)?;
        let def = crate::engine::agent::AgentRegistry::resolve(
            &self.definition_env,
            &name,
            Some(&self.context.git_root),
        )?;
        self.definitions.insert(agent.into(), def.clone());
        Ok(def)
    }

    pub(super) async fn provision_agent_dockerfile(
        &mut self,
        frontend: &mut (impl ImagePreparationSink + ?Sized),
        git_root: &std::path::Path,
        agent: &str,
        _agent_dockerfile: &std::path::Path,
    ) -> Result<crate::engine::agent::DockerfilePreview, EngineError> {
        let def = self.definition(agent)?;
        let sync = crate::engine::agent::provision_agent_dockerfile(
            &def,
            &crate::data::RepoDockerfilePaths::new(git_root),
            &crate::data::image_tags::project_image_tag(git_root),
        )
        .await?;
        for warning in &sync.warnings {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: warning.clone(),
            });
        }
        Ok(sync)
    }
}
/// One spelling per local image name, using the Docker reference rules both
/// Docker and Apple Containers apply: the `docker.io` registry (and its
/// `index.docker.io` alias) and the `library/` namespace are implied, and so
/// is a `:latest` tag. Lock keys, receipts and collision checks compare this
/// form, so `foo`, `foo:latest` and `docker.io/library/foo:latest` are one
/// image.
impl AgentImagePreparer {
    pub(crate) fn normalize_build_reference(reference: &str) -> String {
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
}
