use super::*;
impl AgentImagePreparer {
    /// Make sure the agent Dockerfile an external-build instruction names
    /// exists, fetching it exactly as the build flow does. Best-effort: a
    /// failure is reported on the download row and the instruction is still
    /// shown. awman never builds the image itself here.
    pub(super) async fn provision_agent_dockerfile(
        &mut self,
        frontend: &mut (impl ImagePreparationSink + ?Sized),
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
        frontend.report_step(&ReadyStep::DownloadAgentDockerfile.into(), status);
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
