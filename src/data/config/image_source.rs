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
    /// `reference` defaults to the awman image tag.
    DockerStore {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tls: Option<DockerTlsConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Export from the local Apple Containers image store (macOS only).
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
    DockerStore,
    AppleStore,
    Archive,
}

impl ImageSourceKind {
    /// The persisted spelling (`"registry"`, `"docker-store"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::DockerStore => "docker-store",
            Self::AppleStore => "apple-store",
            Self::Archive => "archive",
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_tagged_and_distinct() {
        let registry: ImageSourceSpec =
            serde_json::from_str(r#"{"type":"registry","reference":"ghcr.io/x/y:1"}"#).unwrap();
        assert_eq!(registry.kind(), ImageSourceKind::Registry);
        let docker: ImageSourceSpec =
            serde_json::from_str(r#"{"type":"docker-store","reference":"ghcr.io/x/y:1"}"#).unwrap();
        assert_eq!(docker.kind(), ImageSourceKind::DockerStore);
        // Same reference, different source: never equal.
        assert_ne!(registry, docker);
        let apple: ImageSourceSpec = serde_json::from_str(r#"{"type":"apple-store"}"#).unwrap();
        assert_eq!(apple.kind(), ImageSourceKind::AppleStore);
        let archive: ImageSourceSpec =
            serde_json::from_str(r#"{"type":"archive","path":"/tmp/a.tar"}"#).unwrap();
        assert_eq!(archive.kind(), ImageSourceKind::Archive);
    }

    #[test]
    fn untagged_or_unknown_sources_are_rejected() {
        assert!(serde_json::from_str::<ImageSourceSpec>(r#"{"reference":"x"}"#).is_err());
        assert!(serde_json::from_str::<ImageSourceSpec>(r#"{"type":"daemon"}"#).is_err());
        // A registry spec cannot smuggle a docker host.
        assert!(serde_json::from_str::<ImageSourceSpec>(
            r#"{"type":"registry","host":"unix:///var/run/docker.sock"}"#
        )
        .is_err());
        // An archive requires its path.
        assert!(serde_json::from_str::<ImageSourceSpec>(r#"{"type":"archive"}"#).is_err());
    }

    #[test]
    fn serialization_round_trips() {
        let specs = [
            ImageSourceSpec::Registry {
                registry: Some("localhost:5000".into()),
                reference: None,
            },
            ImageSourceSpec::DockerStore {
                host: Some("tcp://10.0.0.2:2376".into()),
                tls: Some(DockerTlsConfig {
                    ca: "/ca.pem".into(),
                    cert: None,
                    key: None,
                    verify: true,
                }),
                reference: Some("awman-x:latest".into()),
            },
            ImageSourceSpec::AppleStore { reference: None },
            ImageSourceSpec::Archive {
                path: "/tmp/img.tar".into(),
            },
        ];
        for spec in specs {
            let json = serde_json::to_string(&spec).unwrap();
            let back: ImageSourceSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(back, spec, "{json}");
        }
    }

    #[test]
    fn reference_defaults_to_the_awman_tag() {
        let tag = "awman-repo-claude:latest";
        assert_eq!(
            ImageSourceSpec::Registry {
                registry: None,
                reference: None
            }
            .reference_or(tag),
            tag
        );
        assert_eq!(
            ImageSourceSpec::DockerStore {
                host: None,
                tls: None,
                reference: Some("other:1".into())
            }
            .reference_or(tag),
            "other:1"
        );
        assert_eq!(
            ImageSourceSpec::Archive {
                path: "/a.tar".into()
            }
            .reference_or(tag),
            tag
        );
    }

    #[test]
    fn validate_rejects_meaningless_specs() {
        assert!(ImageSourceSpec::Registry {
            registry: Some(" ".into()),
            reference: None
        }
        .validate()
        .is_err());
        assert!(ImageSourceSpec::DockerStore {
            host: Some("/var/run/docker.sock".into()),
            tls: None,
            reference: None
        }
        .validate()
        .is_err());
        let tls = DockerTlsConfig {
            ca: "/ca.pem".into(),
            cert: None,
            key: None,
            verify: true,
        };
        assert!(ImageSourceSpec::DockerStore {
            host: Some("unix:///var/run/docker.sock".into()),
            tls: Some(tls.clone()),
            reference: None
        }
        .validate()
        .is_err());
        assert!(ImageSourceSpec::DockerStore {
            host: None,
            tls: Some(tls.clone()),
            reference: None
        }
        .validate()
        .is_err());
        assert!(ImageSourceSpec::DockerStore {
            host: Some("tcp://h:2376".into()),
            tls: Some(tls),
            reference: None
        }
        .validate()
        .is_ok());
        assert!(ImageSourceSpec::Archive { path: "".into() }
            .validate()
            .is_err());
    }

    #[test]
    fn docker_tls_verify_defaults_to_true() {
        let tls: DockerTlsConfig = serde_json::from_str(r#"{"ca":"/ca.pem"}"#).unwrap();
        assert!(tls.verify);
    }

    #[test]
    fn registry_auth_names_a_source_never_a_secret() {
        let host: RegistryHostConfig = serde_json::from_str(
            r#"{"insecure":true,"auth":{"type":"env","usernameVar":"U","passwordVar":"P"}}"#,
        )
        .unwrap();
        assert!(host.insecure);
        assert_eq!(
            host.auth,
            Some(RegistryAuthSource::Env {
                username_var: "U".into(),
                password_var: "P".into()
            })
        );
        // There is no field that could hold a password.
        assert!(serde_json::from_str::<RegistryHostConfig>(r#"{"password":"hunter2"}"#).is_err());
        assert!(serde_json::from_str::<RegistryAuthSource>(
            r#"{"type":"env","usernameVar":"U","passwordVar":"P","password":"x"}"#
        )
        .is_err());
    }
}
