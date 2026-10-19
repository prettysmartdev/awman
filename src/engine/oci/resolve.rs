//! Source resolution — which explicit [`ImageSourceSpec`] an awman image tag
//! is acquired from. Pure: reads configuration, contacts nothing.
//!
//! Order: the per-tag entry in `builtin.images`, then `builtin.imageSource`.
//! There is no implicit source. A bare reference never chooses between a
//! registry and a daemon: the spec's `type` decides, so a registry name can
//! never pick up a same-named daemon image or the reverse.

use crate::data::image_build_lock::BuildStoreKey;
use crate::engine::image_prep::builder::BuildTarget;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(test)]
use crate::data::config::builtin_runtime::BuiltinRuntimeConfig;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec, RegistryHostConfig};
use crate::data::config::resolved_builtin::ResolvedBuiltinConfig;
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
    /// `builtin.registries`: per-host registry settings. Carried with the
    /// sources so the import that follows a resolution pulls with the same
    /// session's registry settings.
    pub registries: BTreeMap<String, RegistryHostConfig>,
}

impl ImageSources {
    /// Adapt Layer 0's resolved acquisition inputs without re-reading or
    /// re-merging configuration in the engine.
    pub fn from_resolved(config: &ResolvedBuiltinConfig) -> Self {
        Self {
            default: config.image_source.clone(),
            images: config.images.clone(),
            registries: config.registries.clone(),
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
    pub store_key: Option<BuildStoreKey>,
    pub docker_endpoint: Option<Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderSelection {
    Local(BuildTarget),
    Incompatible(ImageSourceKind),
    Unconfigured,
    Disabled,
}

impl BuilderSelection {
    pub fn from_source(source: Option<&ResolvedImageSource>, enabled: bool) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        let Some(source) = source else {
            return Self::Unconfigured;
        };
        match (&source.source, &source.docker_endpoint) {
            // Acquisition reads Unix sockets only on Unix hosts, so only
            // there can a build and its import share one endpoint.
            (ImageSourceSpec::DockerStore { .. }, Some(Endpoint::Unix(_))) if cfg!(unix) => {
                Self::Local(BuildTarget::Docker {
                    endpoint: source.docker_endpoint.as_ref().unwrap().describe(),
                })
            }
            (ImageSourceSpec::AppleStore { .. }, _) => Self::Local(BuildTarget::Apple),
            _ => Self::Incompatible(source.source.kind()),
        }
    }
}

pub struct ImageSourceResolver<'a> {
    sources: &'a ImageSources,
    env: &'a EnvSnapshot,
    builder_enabled: bool,
}

impl<'a> ImageSourceResolver<'a> {
    pub fn new(sources: &'a ImageSources, env: &'a EnvSnapshot) -> Self {
        Self {
            sources,
            env,
            builder_enabled: false,
        }
    }

    /// Whether the session's builtin builder is on; selects the guidance an
    /// unconfigured image's error carries.
    pub fn with_builder_enabled(mut self, enabled: bool) -> Self {
        self.builder_enabled = enabled;
        self
    }

    pub fn resolve_for_image(
        &self,
        tag: &str,
        hint: &BuildHint,
        platform: OciPlatform,
    ) -> Result<ResolvedImageSource, EngineError> {
        let (mut source, reference) =
            resolve_source(tag, self.sources, hint, self.builder_enabled)?;
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
        let store_key = match (&source, &docker_endpoint) {
            (_, Some(endpoint)) => Some(BuildStoreKey::Docker {
                endpoint: endpoint.describe(),
            }),
            (ImageSourceSpec::AppleStore { .. }, _) => Some(BuildStoreKey::AppleLocal),
            _ => None,
        };
        Ok(ResolvedImageSource {
            source,
            reference,
            platform,
            store_key,
            docker_endpoint,
        })
    }
}

/// The source for `tag`, plus the reference to acquire from it. With no
/// source configured, the error's guidance follows `builder_enabled`.
pub fn resolve_source(
    tag: &str,
    sources: &ImageSources,
    hint: &BuildHint,
    builder_enabled: bool,
) -> Result<(ImageSourceSpec, String), EngineError> {
    let spec = sources
        .images
        .get(tag)
        .or(sources.default.as_ref())
        .cloned()
        .ok_or_else(|| EngineError::ImageSourceUnconfigured {
            guidance: Box::new(ImageBuildGuidance::new(
                tag,
                hint.clone(),
                BuilderSelection::from_source(None, builder_enabled),
            )),
        })?;
    spec.validate().map_err(|reason| {
        EngineError::Config(format!("builtin image source for '{tag}': {reason}"))
    })?;
    let reference = spec.reference_or(tag);
    Ok((spec, reference))
}

/// How to get `tag` into the builtin runtime when it is not available, from
/// the image's [`BuilderSelection`]: build it with `awman ready --build`
/// when the builder is on and the source is a store it can build into,
/// otherwise build it outside awman and point a source at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuildGuidance {
    pub tag: String,
    pub hint: BuildHint,
    pub selection: BuilderSelection,
}

impl ImageBuildGuidance {
    pub fn new(tag: impl Into<String>, hint: BuildHint, selection: BuilderSelection) -> Self {
        Self {
            tag: tag.into(),
            hint,
            selection,
        }
    }

    fn external_steps(&self) -> String {
        let hint = &self.hint;
        let mut steps = Vec::new();
        if let Some((dockerfile, project_tag)) = &hint.project {
            steps.push(format!(
                "docker build -t {project_tag} -f {} {}",
                dockerfile.display(),
                hint.context.display()
            ));
        }
        steps.push(format!(
            "docker build -t {} -f {} {}",
            self.tag,
            hint.agent_dockerfile.display(),
            hint.context.display()
        ));
        steps.join("\n  ")
    }
}

impl std::fmt::Display for ImageBuildGuidance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = &self.tag;
        let lead = match &self.selection {
            BuilderSelection::Local(target) => {
                return write!(
                    f,
                    "run `awman ready --build` to build {tag} in {} and import it.",
                    target.describe()
                );
            }
            BuilderSelection::Disabled => "the builtin runtime imports images; its builder is \
                 off (`builtin.builder.enabled`), so it does not build them."
                .to_string(),
            BuilderSelection::Unconfigured => format!(
                "the builtin builder is enabled, but it builds only into a `docker-local` or \
                 `apple-local` image source: set `builtin.imageSource` (or a `builtin.images` \
                 entry for {tag}) to one and run `awman ready --build`, or build the image \
                 yourself."
            ),
            BuilderSelection::Incompatible(kind) => format!(
                "the builtin builder cannot build into a `{kind}` image source; it builds only \
                 into a `docker-local` (local socket) or `apple-local` source."
            ),
        };
        write!(
            f,
            "{lead} Build the image with Docker (or any OCI builder):\n  {}\nthen set \
             `builtin.imageSource` in awman's config to one of:\n  {{\"type\":\"docker-local\"}}  \
             (export from the local Docker Engine)\n  {{\"type\":\"apple-local\"}}  (macOS: \
             export from the local Apple Containers image store)\n  \
             {{\"type\":\"archive\",\"path\":\"<file>\"}}  (after `docker save {tag} -o <file>`)\n  \
             {{\"type\":\"registry\",\"registry\":\"<host>\",\"reference\":\"<repo>:<tag>\"}}  \
             (after pushing it)\nand run `awman ready` again.",
            self.external_steps()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_from_config_carry_the_registries_the_pull_will_use() {
        let config: BuiltinRuntimeConfig = serde_json::from_str(
            r#"{"imageSource":{"type":"archive","path":"/a.tar"},
                "registries":{"localhost:5000":{"insecure":true}}}"#,
        )
        .unwrap();
        let global = crate::data::config::global::GlobalConfig {
            builtin: Some(config.clone()),
            ..Default::default()
        };
        let effective = crate::data::config::EffectiveConfig::new(
            crate::data::config::flags::FlagConfig::default(),
            crate::data::config::env::EnvSnapshot::empty(),
            crate::data::config::RepoConfig::default(),
            global,
        );
        let sources = ImageSources::from_resolved(&effective.builtin_settings().unwrap());
        assert_eq!(sources.registries, config.registries);
        assert!(sources.registries["localhost:5000"].insecure);
    }

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
            registries: Default::default(),
        };
        assert_eq!(
            resolve_source("t:latest", &sources, &hint(), false).unwrap(),
            (archive, "t:latest".to_string())
        );
        let (spec, reference) = resolve_source("u:latest", &sources, &hint(), false).unwrap();
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
            registries: Default::default(),
        };
        let (_, reference) = resolve_source("awman-x:latest", &sources, &hint(), false).unwrap();
        assert_eq!(reference, "team/agent:1");
    }

    #[test]
    fn no_source_explains_the_external_build() {
        match resolve_source(
            "awman-repo-claude:latest",
            &ImageSources::default(),
            &hint(),
            false,
        ) {
            Err(EngineError::ImageSourceUnconfigured { guidance }) => {
                assert_eq!(guidance.tag, "awman-repo-claude:latest");
                assert_eq!(guidance.selection, BuilderSelection::Disabled);
                let hint = guidance.to_string();
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

    /// WI 0126 F1: with the builder on, the guidance must not claim the
    /// builtin runtime cannot build; it names the sources it builds into,
    /// or the `ready --build` command when the source is one of them.
    #[test]
    fn build_guidance_follows_the_builder_selection() {
        let text = |selection| ImageBuildGuidance::new("t", hint(), selection).to_string();
        let unconfigured = match resolve_source("t", &ImageSources::default(), &hint(), true) {
            Err(EngineError::ImageSourceUnconfigured { guidance }) => guidance,
            other => panic!("expected ImageSourceUnconfigured, got {other:?}"),
        };
        assert_eq!(unconfigured.selection, BuilderSelection::Unconfigured);
        let unconfigured = unconfigured.to_string();
        assert!(!unconfigured.contains("does not build"), "{unconfigured}");
        assert!(unconfigured.starts_with(
            "the builtin builder is enabled, but it builds only into a `docker-local` or \
             `apple-local` image source"
        ));
        assert!(
            unconfigured.contains("awman ready --build"),
            "{unconfigured}"
        );

        let incompatible = text(BuilderSelection::Incompatible(ImageSourceKind::Registry));
        assert!(incompatible
            .starts_with("the builtin builder cannot build into a `registry` image source"));
        assert!(
            incompatible.contains("docker build -t t -f"),
            "{incompatible}"
        );

        let disabled = text(BuilderSelection::Disabled);
        assert!(disabled.starts_with(
            "the builtin runtime imports images; its builder is off (`builtin.builder.enabled`)"
        ));

        assert_eq!(
            text(BuilderSelection::Local(BuildTarget::Apple)),
            "run `awman ready --build` to build t in Apple Containers and import it."
        );
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
            registries: Default::default(),
        };
        assert!(matches!(
            resolve_source("t", &sources, &hint(), false),
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
            registries: Default::default(),
        };
        let resolved = ImageSourceResolver::new(&local, &host_env)
            .resolve_for_image("tag", &hint(), OciPlatform::host_linux())
            .unwrap();
        assert_eq!(resolved.reference, "team/a:4");
        assert!(matches!(resolved.docker_endpoint, Some(Endpoint::Unix(_))));
        assert_eq!(
            resolved.store_key.unwrap().describe(),
            "docker unix:///run/docker.sock"
        );

        let loopback = ImageSources {
            default: Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://127.0.0.1:2375".into()),
                tls: None,
                reference: None,
            }),
            images: BTreeMap::new(),
            registries: Default::default(),
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
            registries: Default::default(),
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
            registries: Default::default(),
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
            registries: Default::default(),
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
        let text = ImageBuildGuidance::new("t", hint(), BuilderSelection::Disabled).to_string();
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
            registries: Default::default(),
        };
        let error = ImageSourceResolver::new(&bad, &crate::data::config::env::EnvSnapshot::empty())
            .resolve_for_image("t", &hint(), OciPlatform::host_linux())
            .unwrap_err()
            .to_string();
        assert!(error.contains("docker-local"), "{error}");
        assert!(!error.contains("docker-store"), "{error}");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::data::config::image_source::DockerTlsConfig;
    fn host_platform() -> OciPlatform {
        OciPlatform::host_linux()
    }
    // ── BuilderSelection::from_source ────────────────────────────────────

    fn resolved(source: ImageSourceSpec, docker_endpoint: Option<Endpoint>) -> ResolvedImageSource {
        ResolvedImageSource {
            source,
            reference: "awman-repo-claude:latest".into(),
            platform: host_platform(),
            store_key: Some(crate::data::image_build_lock::BuildStoreKey::Docker {
                endpoint: "unix:///var/run/docker.sock".into(),
            }),
            docker_endpoint,
        }
    }

    #[test]
    fn no_source_is_unconfigured() {
        assert_eq!(
            BuilderSelection::from_source(None, true),
            BuilderSelection::Unconfigured
        );
    }

    #[test]
    fn docker_store_on_a_local_unix_socket_is_the_local_docker_builder() {
        let source = resolved(
            ImageSourceSpec::DockerStore {
                host: Some("unix:///var/run/docker.sock".into()),
                tls: None,
                reference: None,
            },
            Some(Endpoint::Unix("/var/run/docker.sock".into())),
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&source), true),
            BuilderSelection::Local(BuildTarget::Docker {
                endpoint: "unix:///var/run/docker.sock".into(),
            })
        );
    }

    /// Every TCP endpoint is incompatible for building, including loopback
    /// and TLS — only a local Unix socket is eligible in this item.
    #[test]
    fn docker_store_on_any_tcp_endpoint_is_incompatible() {
        for tls in [
            None,
            Some(DockerTlsConfig {
                ca: "/ca.pem".into(),
                cert: Some("/cert.pem".into()),
                key: Some("/key.pem".into()),
                verify: true,
            }),
        ] {
            let source = resolved(
                ImageSourceSpec::DockerStore {
                    host: Some("tcp://127.0.0.1:2375".into()),
                    tls: tls.clone(),
                    reference: None,
                },
                Some(Endpoint::Tcp {
                    host: "127.0.0.1".into(),
                    port: 2375,
                    tls,
                }),
            );
            assert_eq!(
                BuilderSelection::from_source(Some(&source), true),
                BuilderSelection::Incompatible(ImageSourceKind::DockerStore),
                "TCP must be incompatible even for loopback"
            );
        }
    }

    #[test]
    fn apple_store_is_always_the_local_apple_builder() {
        let source = resolved(ImageSourceSpec::AppleStore { reference: None }, None);
        assert_eq!(
            BuilderSelection::from_source(Some(&source), true),
            BuilderSelection::Local(BuildTarget::Apple)
        );
    }

    #[test]
    fn archive_and_registry_sources_are_incompatible() {
        let archive = resolved(
            ImageSourceSpec::Archive {
                path: "/exports/agent.tar".into(),
            },
            None,
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&archive), true),
            BuilderSelection::Incompatible(ImageSourceKind::Archive)
        );
        let registry = resolved(
            ImageSourceSpec::Registry {
                registry: Some("ghcr.io".into()),
                reference: None,
            },
            None,
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&registry), true),
            BuilderSelection::Incompatible(ImageSourceKind::Registry)
        );
    }

    #[test]
    fn disabled_wins_for_every_source_including_unconfigured() {
        assert_eq!(
            BuilderSelection::from_source(None, false),
            BuilderSelection::Disabled
        );
        for source in [
            ImageSourceSpec::AppleStore { reference: None },
            ImageSourceSpec::Archive {
                path: "/a.tar".into(),
            },
            ImageSourceSpec::DockerStore {
                host: Some("unix:///docker.sock".into()),
                tls: None,
                reference: None,
            },
        ] {
            let source = resolved(source, Some(Endpoint::Unix("/docker.sock".into())));
            assert_eq!(
                BuilderSelection::from_source(Some(&source), false),
                BuilderSelection::Disabled
            );
        }
    }
}
