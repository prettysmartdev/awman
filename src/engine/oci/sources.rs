//! Per-source dispatch: the cache key each explicit source resolves to, and
//! the fetch that stages its archive.
//!
//! The key is computed without contacting the source. It combines the
//! source *kind*, a non-secret locator (registry host and repository, Docker
//! endpoint, or archive path), the reference and the platform — so the same
//! name from two different kinds of source is never the same cache entry,
//! and a registry reference can never be satisfied by a daemon's image.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::error::EngineError;
use crate::engine::oci::cache::SourceFingerprint;
use crate::engine::oci::verify::{DiskSpace, StagingSink};
use crate::engine::oci::{apple_store, archive, docker_engine, registry};
use crate::engine::oci::{AcquireLimits, AcquireProgress};

/// Progress callback usable from a worker thread.
pub(super) type Report<'a> = &'a (dyn Fn(AcquireProgress) + Sync);

/// What every adapter gets.
pub(super) struct FetchContext<'a> {
    /// Private staging directory inside the cache (same filesystem).
    pub staging_dir: &'a Path,
    pub limits: AcquireLimits,
    pub disk: &'a dyn DiskSpace,
    pub platform: &'a OciPlatform,
}

/// A staged, not yet validated archive.
#[derive(Debug)]
pub(super) struct Fetched {
    pub path: PathBuf,
    /// Names that identify the wanted image inside a multi-image archive.
    pub wanted_refs: Vec<String>,
    /// The manifest digest the source vouched for, checked after validation.
    pub expected_manifest: Option<Digest>,
    /// Archive sources: the source file's fingerprint at import.
    pub fingerprint: Option<SourceFingerprint>,
}

/// A source resolved far enough to key the cache — no I/O against it.
pub(super) enum Resolved {
    Registry {
        reference: registry::ImageReference,
        host: Option<RegistryHostConfig>,
    },
    DockerStore {
        endpoint: docker_engine::Endpoint,
    },
    AppleStore,
    Archive {
        path: PathBuf,
    },
}

impl Resolved {
    pub(super) fn resolve(
        spec: &ImageSourceSpec,
        reference: &str,
        registries: &BTreeMap<String, RegistryHostConfig>,
        env: &EnvSnapshot,
    ) -> Result<Self, EngineError> {
        spec.validate()
            .map_err(|r| EngineError::Config(format!("image source: {r}")))?;
        Ok(match spec {
            ImageSourceSpec::Registry {
                registry: forced,
                reference: configured,
            } => {
                if forced.is_none() && configured.is_none() {
                    // Pulling `docker.io/library/<awman tag>` is never what
                    // anyone meant.
                    return Err(EngineError::Config(
                        "a registry image source needs `registry` or `reference`".into(),
                    ));
                }
                let parsed = registry::ImageReference::parse(reference, forced.as_deref())
                    .map_err(|r| EngineError::Config(format!("registry image source: {r}")))?;
                let host = registry::host_config(&parsed, registries).cloned();
                Self::Registry {
                    reference: parsed,
                    host,
                }
            }
            ImageSourceSpec::DockerStore { host, tls, .. } => Self::DockerStore {
                endpoint: docker_engine::resolve_endpoint(host.as_deref(), tls.as_ref(), env)?,
            },
            ImageSourceSpec::AppleStore { .. } => Self::AppleStore,
            ImageSourceSpec::Archive { path } => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    return Err(EngineError::Config(format!(
                        "archive image source path `{}` must be absolute",
                        path.display()
                    )));
                };
                Self::Archive { path }
            }
        })
    }

    /// Non-secret description of the source.
    pub(super) fn locator(&self) -> String {
        match self {
            Self::Registry { reference, host } => {
                let scheme = if host.as_ref().is_some_and(|h| h.insecure) {
                    "http"
                } else {
                    "https"
                };
                format!(
                    "{scheme}://{}/{}",
                    reference.api_host(),
                    reference.repository
                )
            }
            Self::DockerStore { endpoint } => endpoint.describe(),
            Self::AppleStore => "apple-containers".into(),
            Self::Archive { path } => path.display().to_string(),
        }
    }

    /// The cache key for `reference` on `platform` from this source.
    pub(super) fn key(&self, kind: &str, reference: &str, platform: &OciPlatform) -> String {
        // A registry reference is keyed in its canonical, fully-qualified form.
        let reference = match self {
            Self::Registry { reference, .. } => reference.to_string(),
            _ => reference.to_string(),
        };
        format!("v1\n{kind}\n{}\n{reference}\n{platform}", self.locator())
    }

    /// Stage the archive. Network sources run here on the caller's thread;
    /// the acquirer moves them off any async runtime.
    pub(super) fn fetch(
        &self,
        awman_tag: &str,
        reference: &str,
        ctx: &FetchContext<'_>,
        report: Report<'_>,
    ) -> Result<Fetched, EngineError> {
        match self {
            Self::Registry {
                reference: parsed,
                host,
            } => {
                let lookup = |name: &str| crate::data::config::env::host_var(name);
                let docker_config = || {
                    let dir = lookup("DOCKER_CONFIG")
                        .map(PathBuf::from)
                        .or_else(|| dirs::home_dir().map(|h| h.join(".docker")))?;
                    let path = dir.join("config.json");
                    let bytes = std::fs::read(&path).ok()?;
                    Some((path, bytes))
                };
                let credentials = registry::resolve_credentials(
                    &parsed.registry,
                    host.as_ref().and_then(|h| h.auth.as_ref()),
                    &lookup,
                    &docker_config,
                )?;
                registry::pull(
                    parsed,
                    awman_tag,
                    registry::PullInputs {
                        host: host.as_ref(),
                        credentials,
                        proxy: registry::ProxySettings::from_lookup(&lookup),
                    },
                    ctx,
                    report,
                )
            }
            Self::DockerStore { endpoint } => {
                docker_engine::export(endpoint, reference, ctx, report)
            }
            Self::AppleStore => Err(apple_store::blocked(reference)),
            Self::Archive { path } => {
                let fingerprint = SourceFingerprint::of(path);
                let meta = std::fs::metadata(path).map_err(|e| EngineError::io(path, e))?;
                crate::engine::oci::verify::ensure_space(
                    ctx.disk,
                    ctx.staging_dir,
                    meta.len(),
                    ctx.limits.min_free_bytes,
                )?;
                let mut sink = StagingSink::create(
                    ctx.staging_dir.join("archive.tar"),
                    ctx.limits.max_archive_bytes,
                    ctx.limits.min_free_bytes,
                    ctx.disk,
                )?;
                archive::stage_local_archive(path, &mut sink, ctx.limits.max_archive_bytes)?;
                let (staged, _) = sink.finish()?;
                Ok(Fetched {
                    path: staged,
                    wanted_refs: vec![awman_tag.to_string()],
                    expected_manifest: None,
                    fingerprint,
                })
            }
        }
    }

    /// Whether this source does network I/O (and so must run off-runtime).
    pub(super) fn is_network(&self) -> bool {
        matches!(self, Self::Registry { .. } | Self::DockerStore { .. })
    }
}

/// Run `work` on a dedicated OS thread, forwarding its progress events to
/// `progress` on this thread. Blocking HTTP clients must not run inside an
/// async runtime's worker, and `import_image` may be called from one.
pub(super) fn run_isolated<T: Send>(
    progress: &mut dyn FnMut(AcquireProgress),
    work: impl FnOnce(Report<'_>) -> T + Send,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel::<AcquireProgress>();
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || {
            let tx = std::sync::Mutex::new(tx);
            let report = move |p: AcquireProgress| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(p);
                }
            };
            work(&report)
        });
        for event in rx {
            progress(event);
        }
        match handle.join() {
            Ok(v) => v,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_from_different_kinds_never_shares_a_key() {
        let platform = OciPlatform::host_linux();
        let env = EnvSnapshot::empty();
        let registries = BTreeMap::new();
        let reg = Resolved::resolve(
            &ImageSourceSpec::Registry {
                registry: Some("localhost:5000".into()),
                reference: Some("agent:1".into()),
            },
            "agent:1",
            &registries,
            &env,
        )
        .unwrap();
        let dock = Resolved::resolve(
            &ImageSourceSpec::DockerStore {
                host: Some("unix:///run/docker.sock".into()),
                tls: None,
                reference: Some("agent:1".into()),
            },
            "agent:1",
            &registries,
            &env,
        )
        .unwrap();
        assert_ne!(
            reg.key("registry", "agent:1", &platform),
            dock.key("docker-store", "agent:1", &platform)
        );
        assert_eq!(reg.locator(), "https://localhost:5000/agent");
        assert_eq!(dock.locator(), "unix:///run/docker.sock");
    }

    #[test]
    fn registry_without_registry_or_reference_is_refused() {
        assert!(matches!(
            Resolved::resolve(
                &ImageSourceSpec::Registry {
                    registry: None,
                    reference: None
                },
                "awman-x:latest",
                &BTreeMap::new(),
                &EnvSnapshot::empty(),
            ),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn relative_archive_paths_are_refused() {
        assert!(Resolved::resolve(
            &ImageSourceSpec::Archive {
                path: "rel.tar".into()
            },
            "t",
            &BTreeMap::new(),
            &EnvSnapshot::empty(),
        )
        .is_err());
    }

    #[test]
    fn run_isolated_forwards_progress_and_returns() {
        let mut seen = Vec::new();
        let d = crate::engine::oci::verify::sha256_digest(b"x");
        let out = run_isolated(&mut |p| seen.push(p), |report| {
            report(AcquireProgress::Verifying { digest: d.clone() });
            7
        });
        assert_eq!(out, 7);
        assert_eq!(seen.len(), 1);
    }
}
