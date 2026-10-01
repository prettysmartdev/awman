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
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use crate::data::oci_identity::OciPlatform;
use crate::engine::error::EngineError;
use crate::engine::oci::docker_engine::{resolve_endpoint, Endpoint};

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

/// A single launch's source, with any ambient Docker endpoint frozen before a
/// build or an import starts. The reference remains the configured logical
/// name; only the endpoint is materialized into the import specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImageSource {
    pub source: ImageSourceSpec,
    pub reference: String,
    pub platform: OciPlatform,
    pub store: String,
    pub docker_endpoint: Option<Endpoint>,
}

pub struct ImageSourceResolver<'a> {
    sources: &'a ImageSources,
    env: &'a EnvSnapshot,
}

impl<'a> ImageSourceResolver<'a> {
    pub fn new(sources: &'a ImageSources, env: &'a EnvSnapshot) -> Self {
        Self { sources, env }
    }

    pub fn resolve_for_image(
        &self,
        tag: &str,
        hint: &BuildHint,
        platform: OciPlatform,
    ) -> Result<ResolvedImageSource, EngineError> {
        let (mut source, reference) = resolve_source(tag, self.sources, hint)?;
        let docker_endpoint = match &mut source {
            ImageSourceSpec::DockerStore { host, tls, .. } => {
                let endpoint = resolve_endpoint(host.as_deref(), tls.as_ref(), self.env)?;
                if let Endpoint::Unix(path) = &endpoint {
                    *host = Some(format!("unix://{}", path.display()));
                    *tls = None;
                } else if let Endpoint::Tcp {
                    host: name,
                    port,
                    tls: resolved_tls,
                } = &endpoint
                {
                    let authority = if name.contains(':') {
                        format!("[{name}]")
                    } else {
                        name.clone()
                    };
                    *host = Some(format!("tcp://{authority}:{port}"));
                    *tls = resolved_tls.clone();
                }
                Some(endpoint)
            }
            _ => None,
        };
        let store = match (&source, &docker_endpoint) {
            (_, Some(endpoint)) => endpoint.describe(),
            (ImageSourceSpec::AppleStore { .. }, _) => "apple-containers".into(),
            (ImageSourceSpec::Archive { path }, _) => path.display().to_string(),
            (ImageSourceSpec::Registry { .. }, _) => ImageSourceKind::Registry.as_str().into(),
            _ => unreachable!(),
        };
        Ok(ResolvedImageSource {
            source,
            reference,
            platform,
            store,
            docker_endpoint,
        })
    }
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
         to one of:\n  {{\"type\":\"docker-local\"}}  (export from the local Docker Engine)\n  \
         {{\"type\":\"apple-local\"}}  (macOS: export from the local Apple Containers image store)\n  \
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
                assert!(hint.contains("docker-local"));
                assert!(hint.contains(r#"{"type":"apple-local"}"#));
                assert!(hint.contains("archive"));
                assert!(hint.contains("registry"));
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

    #[test]
    fn docker_resolution_distinguishes_local_remote_and_ignores_ambient_drift() {
        use crate::data::config::env::EnvSnapshot;
        let host_env = EnvSnapshot::with_overrides([("DOCKER_HOST", "tcp://192.0.2.7:2375")]);
        let local = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("unix:///run/docker.sock".into()),
                tls: None,
                reference: Some("team/a:4".into()),
            }),
            images: BTreeMap::new(),
        };
        let resolved = ImageSourceResolver::new(&local, &host_env)
            .resolve_for_image("tag", &hint(), OciPlatform::host_linux())
            .unwrap();
        assert_eq!(resolved.reference, "team/a:4");
        assert!(matches!(resolved.docker_endpoint, Some(Endpoint::Unix(_))));
        assert_eq!(resolved.store, "unix:///run/docker.sock");

        let loopback = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://127.0.0.1:2375".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        assert!(
            matches!(ImageSourceResolver::new(&loopback, &EnvSnapshot::empty())
            .resolve_for_image("tag", &hint(), OciPlatform::host_linux()).unwrap().docker_endpoint,
            Some(Endpoint::Tcp { host, port: 2375, .. }) if host == "127.0.0.1")
        );

        let remote = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://192.0.2.7:2375".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        assert!(matches!(
            ImageSourceResolver::new(&remote, &EnvSnapshot::empty()).resolve_for_image(
                "tag",
                &hint(),
                OciPlatform::host_linux()
            ),
            Err(EngineError::UnsupportedImageSource { .. })
        ));

        // An explicit endpoint is frozen into the resolved source even if
        // ambient DOCKER_HOST points elsewhere at acquisition time.
        let explicit = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://127.0.0.1:2376".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        let endpoint = ImageSourceResolver::new(&explicit, &host_env)
            .resolve_for_image("tag", &hint(), OciPlatform::host_linux())
            .unwrap()
            .docker_endpoint
            .unwrap();
        assert_eq!(
            endpoint,
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 2376,
                tls: None
            }
        );
    }

    #[test]
    fn ipv6_loopback_endpoint_survives_source_freezing() {
        let sources = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://[::1]:2375".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        let env = EnvSnapshot::empty();
        let resolved = ImageSourceResolver::new(&sources, &env)
            .resolve_for_image("tag", &hint(), OciPlatform::host_linux())
            .unwrap();
        let ImageSourceSpec::DockerStore { host, tls, .. } = resolved.source else {
            panic!("expected Docker source")
        };
        assert_eq!(host.as_deref(), Some("tcp://[::1]:2375"));
        assert_eq!(
            resolve_endpoint(host.as_deref(), tls.as_ref(), &env).unwrap(),
            resolved.docker_endpoint.unwrap()
        );
    }

    #[test]
    fn canonical_and_alias_source_spellings_resolve_to_typed_kinds() {
        for spelling in ["docker-local", "docker-store", "apple-local", "apple-store"] {
            let raw = if spelling.starts_with("docker") {
                format!(
                    r#"{{"type":"{spelling}","host":"unix:///run/docker.sock","reference":"team/a:4"}}"#
                )
            } else {
                format!(r#"{{"type":"{spelling}","reference":"team/a:4"}}"#)
            };
            let spec: ImageSourceSpec = serde_json::from_str(&raw).unwrap();
            assert_eq!(spec.reference_or("tag"), "team/a:4");
            let expected = if spelling.starts_with("docker") {
                ImageSourceKind::DockerStore
            } else {
                ImageSourceKind::AppleStore
            };
            assert_eq!(spec.kind(), expected);
            assert_eq!(
                serde_json::to_value(spec).unwrap()["type"],
                expected.as_str()
            );
        }
        for source in [
            ImageSourceSpec::Archive {
                path: "/tmp/a.tar".into(),
            },
            ImageSourceSpec::Registry {
                registry: Some("registry.example".into()),
                reference: Some("team/a:4".into()),
            },
        ] {
            assert_eq!(
                source.reference_or("fallback"),
                if source.kind() == ImageSourceKind::Archive {
                    "fallback"
                } else {
                    "team/a:4"
                }
            );
        }
    }

    #[test]
    fn errors_and_external_hints_use_canonical_source_names() {
        let text = external_build_hint("t", &hint());
        for canonical in ["docker-local", "apple-local", "archive", "registry"] {
            assert!(text.contains(canonical));
        }
        assert!(!text.contains("docker-store"));
        assert!(!text.contains("apple-store"));
        let bad = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://192.0.2.9:2375".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
        };
        let error = ImageSourceResolver::new(&bad, &crate::data::config::env::EnvSnapshot::empty())
            .resolve_for_image("t", &hint(), OciPlatform::host_linux())
            .unwrap_err()
            .to_string();
        assert!(error.contains("docker-local"), "{error}");
        assert!(!error.contains("docker-store"), "{error}");
    }
}
