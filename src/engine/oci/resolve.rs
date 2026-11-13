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
