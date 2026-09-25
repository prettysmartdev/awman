//! Source resolution — which explicit [`ImageSourceSpec`] an awman image tag
//! is acquired from. Pure: reads configuration, contacts nothing.
//!
//! Order: the per-tag entry in `builtin.images`, then `builtin.imageSource`.
//! There is no implicit source. A bare reference never chooses between a
//! registry and a daemon: the spec's `type` decides, so a registry name can
//! never pick up a same-named daemon image or the reverse.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::data::config::builtin_runtime::BuiltinRuntimeConfig;
use crate::data::config::image_source::ImageSourceSpec;
use crate::engine::error::EngineError;

/// The configured image sources for the builtin runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageSources {
    /// `builtin.imageSource`: used for every tag without its own entry.
    pub default: Option<ImageSourceSpec>,
    /// `builtin.images`: per-tag sources.
    pub images: BTreeMap<String, ImageSourceSpec>,
}

impl ImageSources {
    pub fn from_config(config: &BuiltinRuntimeConfig) -> Self {
        Self {
            default: config.image_source.clone(),
            images: config.images.clone(),
        }
    }
}

/// Where the image would be built from, for the "how to build it" hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildHint {
    /// The agent's Dockerfile (`.awman/Dockerfile.<agent>`).
    pub agent_dockerfile: PathBuf,
    /// The project Dockerfile the agent image builds `FROM`, and its tag.
    pub project: Option<(PathBuf, String)>,
    /// The directory the build commands run in (the Git root).
    pub context: PathBuf,
}

/// The source for `tag`, plus the reference to acquire from it.
pub fn resolve_source(
    tag: &str,
    sources: &ImageSources,
    hint: &BuildHint,
) -> Result<(ImageSourceSpec, String), EngineError> {
    let spec = sources
        .images
        .get(tag)
        .or(sources.default.as_ref())
        .cloned()
        .ok_or_else(|| EngineError::ImageSourceUnconfigured {
            tag: tag.to_string(),
            hint: external_build_hint(tag, hint),
        })?;
    spec.validate().map_err(|reason| {
        EngineError::Config(format!("builtin image source for '{tag}': {reason}"))
    })?;
    let reference = spec.reference_or(tag);
    Ok((spec, reference))
}

/// How to produce `tag` outside awman and point the builtin runtime at it.
/// The builtin runtime never builds images itself.
pub fn external_build_hint(tag: &str, hint: &BuildHint) -> String {
    let mut steps = Vec::new();
    if let Some((dockerfile, project_tag)) = &hint.project {
        steps.push(format!(
            "docker build -t {project_tag} -f {} {}",
            dockerfile.display(),
            hint.context.display()
        ));
    }
    steps.push(format!(
        "docker build -t {tag} -f {} {}",
        hint.agent_dockerfile.display(),
        hint.context.display()
    ));
    format!(
        "the builtin runtime imports images; it does not build them. Build the image with \
         Docker (or any OCI builder):\n  {}\nthen set `builtin.imageSource` in awman's config \
         to one of:\n  {{\"type\":\"docker-store\"}}  (export from the local Docker Engine)\n  \
         {{\"type\":\"archive\",\"path\":\"<file>\"}}  (after `docker save {tag} -o <file>`)\n  \
         {{\"type\":\"registry\",\"registry\":\"<host>\",\"reference\":\"<repo>:<tag>\"}}  \
         (after pushing it)\nand run `awman ready` again.",
        steps.join("\n  ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint() -> BuildHint {
        BuildHint {
            agent_dockerfile: PathBuf::from("/repo/.awman/Dockerfile.claude"),
            project: Some((
                PathBuf::from("/repo/Dockerfile.dev"),
                "awman-repo:latest".into(),
            )),
            context: PathBuf::from("/repo"),
        }
    }

    #[test]
    fn per_tag_source_wins_over_the_default() {
        let archive = ImageSourceSpec::Archive {
            path: "/x.tar".into(),
        };
        let sources = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: None,
                tls: None,
                reference: None,
            }),
            images: BTreeMap::from([("t:latest".to_string(), archive.clone())]),
        };
        assert_eq!(
            resolve_source("t:latest", &sources, &hint()).unwrap(),
            (archive, "t:latest".to_string())
        );
        let (spec, reference) = resolve_source("u:latest", &sources, &hint()).unwrap();
        assert!(matches!(spec, ImageSourceSpec::DockerStore { .. }));
        assert_eq!(reference, "u:latest");
    }

    #[test]
    fn configured_reference_is_used_verbatim() {
        let sources = ImageSources {
            default: Some(ImageSourceSpec::Registry {
                registry: Some("localhost:5000".into()),
                reference: Some("team/agent:1".into()),
            }),
            images: BTreeMap::new(),
        };
        let (_, reference) = resolve_source("awman-x:latest", &sources, &hint()).unwrap();
        assert_eq!(reference, "team/agent:1");
    }

    #[test]
    fn no_source_explains_the_external_build() {
        match resolve_source(
            "awman-repo-claude:latest",
            &ImageSources::default(),
            &hint(),
        ) {
            Err(EngineError::ImageSourceUnconfigured { tag, hint }) => {
                assert_eq!(tag, "awman-repo-claude:latest");
                assert!(hint.contains("does not build"));
                assert!(hint
                    .contains("docker build -t awman-repo:latest -f /repo/Dockerfile.dev /repo"));
                assert!(hint.contains(
                    "docker build -t awman-repo-claude:latest -f /repo/.awman/Dockerfile.claude /repo"
                ));
                assert!(hint.contains("docker-store"));
            }
            other => panic!("expected ImageSourceUnconfigured, got {other:?}"),
        }
    }

    #[test]
    fn invalid_spec_is_a_config_error() {
        let sources = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("ftp://x".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        assert!(matches!(
            resolve_source("t", &sources, &hint()),
            Err(EngineError::Config(_))
        ));
    }
}
