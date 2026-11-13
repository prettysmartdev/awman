//! Image sources — where the builtin runtime acquires an agent image it does
//! not have cached yet.
//!
//! Every source is explicit and tagged. A registry reference is only ever
//! pulled from a registry, a Docker Engine reference is only ever exported
//! from that engine's store, and an archive is only ever read from its path.
//! Nothing here falls back from one kind to another, so a registry name can
//! never silently resolve to a daemon image (or the reverse). The source kind
//! is also recorded in every [`crate::data::oci_identity::ImageIdentity`], so
//! a cached image remembers where it came from.
//!
//! Secrets are never stored here: registry credentials are named by
//! [`RegistryAuthSource`], which says where to read them at acquisition time.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One configured image source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ImageSourceSpec {
    /// Pull `<registry>/<reference>` from an OCI registry. `reference`
    /// defaults to the awman image tag; `registry` defaults to the registry
    /// named in the reference (Docker Hub when it names none).
    Registry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        registry: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Export from a Docker Engine over its API. `host` accepts `unix://`,
    /// `tcp://` and `npipe://`; `None` means the platform's default socket.
    /// `reference` defaults to the awman image tag. Spelled `docker-local`.
    #[serde(rename = "docker-local")]
    DockerStore {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tls: Option<DockerTlsConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Export from the local Apple Containers image store (macOS only).
    /// Spelled `apple-local`.
    #[serde(rename = "apple-local")]
    AppleStore {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Load a previously exported OCI-layout or `docker save` tar.
    Archive { path: PathBuf },
}

/// The kind of an [`ImageSourceSpec`], without its parameters. Recorded in
/// image identities and error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageSourceKind {
    Registry,
    #[serde(rename = "docker-local", alias = "docker-store")]
    DockerStore,
    #[serde(rename = "apple-local", alias = "apple-store")]
    AppleStore,
    Archive,
}

impl ImageSourceKind {
    /// Every kind, in display order.
    pub const ALL: [ImageSourceKind; 4] = [
        Self::Registry,
        Self::DockerStore,
        Self::AppleStore,
        Self::Archive,
    ];

    /// The canonical persisted spelling (`"registry"`, `"docker-local"`, …).
    /// Always what awman writes and displays.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::DockerStore => "docker-local",
            Self::AppleStore => "apple-local",
            Self::Archive => "archive",
        }
    }

    /// Earlier spellings of this kind that are still accepted on input and
    /// still found in records written before the rename. Never written.
    pub fn legacy_names(self) -> &'static [&'static str] {
        match self {
            Self::DockerStore => &["docker-store"],
            Self::AppleStore => &["apple-store"],
            Self::Registry | Self::Archive => &[],
        }
    }

    /// Parse a canonical or legacy spelling. Exact match only: no trimming,
    /// case folding or prefix matching.
    pub fn parse_name(name: &str) -> Option<ImageSourceKind> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name || kind.legacy_names().contains(&name))
    }
}

impl std::fmt::Display for ImageSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Docker Engine URL schemes a [`ImageSourceSpec::DockerStore`] host may use.
pub const DOCKER_HOST_SCHEMES: &[&str] = &["unix://", "tcp://", "npipe://"];

impl ImageSourceSpec {
    /// This source's kind.
    pub fn kind(&self) -> ImageSourceKind {
        match self {
            Self::Registry { .. } => ImageSourceKind::Registry,
            Self::DockerStore { .. } => ImageSourceKind::DockerStore,
            Self::AppleStore { .. } => ImageSourceKind::AppleStore,
            Self::Archive { .. } => ImageSourceKind::Archive,
        }
    }

    /// The image reference to acquire: the configured one, or `awman_tag`
    /// when none is configured. An archive has no reference of its own, so it
    /// always answers `awman_tag` (the tag the loaded image is stored under).
    pub fn reference_or(&self, awman_tag: &str) -> String {
        let configured = match self {
            Self::Registry { reference, .. }
            | Self::DockerStore { reference, .. }
            | Self::AppleStore { reference } => reference.as_deref(),
            Self::Archive { .. } => None,
        };
        configured
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or(awman_tag)
            .to_string()
    }

    /// Reject a spec that cannot mean anything: an empty string where a
    /// value is required, a Docker host with an unsupported scheme, or TLS
    /// settings on a non-TCP Docker host. Returns a human-readable reason.
    pub fn validate(&self) -> Result<(), String> {
        fn non_empty(field: &str, value: &Option<String>) -> Result<(), String> {
            match value {
                Some(v) if v.trim().is_empty() => Err(format!("`{field}` must not be empty")),
                _ => Ok(()),
            }
        }
        match self {
            Self::Registry {
                registry,
                reference,
            } => {
                non_empty("registry", registry)?;
                non_empty("reference", reference)
            }
            Self::DockerStore {
                host,
                tls,
                reference,
            } => {
                non_empty("host", host)?;
                non_empty("reference", reference)?;
                if let Some(host) = host {
                    if !DOCKER_HOST_SCHEMES.iter().any(|s| host.starts_with(s)) {
                        return Err(format!(
                            "docker host `{host}` must start with one of {}",
                            DOCKER_HOST_SCHEMES.join(", ")
                        ));
                    }
                    if tls.is_some() && !host.starts_with("tcp://") {
                        return Err(format!(
                            "`tls` applies only to a tcp:// docker host, not `{host}`"
                        ));
                    }
                } else if tls.is_some() {
                    return Err("`tls` requires an explicit tcp:// `host`".into());
                }
                Ok(())
            }
            Self::AppleStore { reference } => non_empty("reference", reference),
            Self::Archive { path } => {
                if path.as_os_str().is_empty() {
                    Err("`path` must not be empty".into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

fn default_true() -> bool {
    true
}

/// TLS material for a `tcp://` Docker Engine host. Paths only; the files are
/// read at acquisition time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DockerTlsConfig {
    /// CA bundle the engine's certificate must chain to.
    pub ca: PathBuf,
    /// Client certificate, for engines that require mutual TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<PathBuf>,
    /// Client key matching `cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<PathBuf>,
    /// Verify the engine's certificate. Default `true`.
    #[serde(default = "default_true")]
    pub verify: bool,
}

/// Per-registry-host settings, keyed by `host[:port]` in
/// [`crate::data::config::builtin_runtime::BuiltinRuntimeConfig::registries`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistryHostConfig {
    /// Plain HTTP. Only for localhost or private registries.
    #[serde(default)]
    pub insecure: bool,
    /// Extra PEM bundle (a private CA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Where to read credentials. `None` is anonymous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RegistryAuthSource>,
}

/// Where registry credentials come from. Secrets are never stored in config;
/// every variant names *where* to read them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RegistryAuthSource {
    /// No credentials.
    Anonymous,
    /// Read the username and password from these environment variables.
    #[serde(rename_all = "camelCase")]
    Env {
        username_var: String,
        password_var: String,
    },
    /// Read from awman's keychain under `service` (in-memory under test
    /// isolation).
    Keychain { service: String },
    /// Explicit opt-in to `~/.docker/config.json`, which may name a Docker
    /// credential helper that is then executed.
    DockerConfig,
}
