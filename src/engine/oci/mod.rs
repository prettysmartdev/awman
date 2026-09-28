//! `engine::oci` — acquiring OCI images for runtimes that import rather than
//! build (`ImageAcquisition::Import`).
//!
//! An [`ImageAcquirer`] resolves an explicit [`ImageSourceSpec`] to content
//! identity, fetches and verifies the content, and leaves a verified archive
//! in awman's cache. The builtin runtime loads that archive into its own
//! store and records the [`ImageIdentity`].
//!
//! Sources never substitute for one another: a registry reference is only
//! pulled from a registry, a Docker Engine reference is only exported from
//! that engine, and an archive is only read from its path. The Apple
//! Containers store is modelled but blocked (see [`apple_store`]).
//!
//! Every acquisition follows the same path:
//!
//! 1. resolve the source and its cache key without contacting it
//!    ([`sources`]); a cache hit is returned unless the policy is `Refresh`;
//! 2. stage the archive in a private directory inside the cache, enforcing
//!    the byte cap and the free-space floor as it arrives;
//! 3. validate it completely ([`archive`]): structure, every digest, the
//!    platform, every layer entry;
//! 4. commit it atomically ([`cache`]).
//!
//! Nothing is visible in the cache until step 4, so a wrong-platform,
//! malformed, oversized, tampered or truncated image — or a full disk —
//! leaves no partial state behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec, RegistryHostConfig};
use crate::data::oci_identity::{Digest, ImageIdentity, OciPlatform};
use crate::engine::error::EngineError;

pub mod apple_store;
pub mod archive;
pub mod cache;
pub mod docker_engine;
pub mod registry;
pub mod resolve;
pub mod sources;
pub mod verify;

pub use archive::{validate_archive, ImageConfigSummary, ValidatedArchive};
pub use cache::{CacheRecord, OciCache, PruneReport};
pub use resolve::{external_build_hint, resolve_source, BuildHint, ImageSources};
pub use verify::DiskSpace;

/// Acquires images from explicit sources into awman's verified archive cache.
pub trait ImageAcquirer: Send + Sync {
    /// Resolve `request.source` to content identity, fetch and verify it, and
    /// produce an OCI archive in awman's cache.
    ///
    /// Never contacts a source daemon or registry when `request.policy` is
    /// [`AcquirePolicy::CachedOnly`] and the identity is already cached.
    fn acquire(
        &self,
        request: &AcquireRequest,
        progress: &mut dyn FnMut(AcquireProgress),
    ) -> Result<AcquiredImage, EngineError>;
}

/// One acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    /// The awman tag the image will be stored under.
    pub tag: String,
    /// The explicit source to acquire from.
    pub source: ImageSourceSpec,
    /// The platform the selected manifest must match.
    pub platform: OciPlatform,
    pub policy: AcquirePolicy,
    /// Registry host settings keyed by `host[:port]`.
    pub registries: BTreeMap<String, RegistryHostConfig>,
}

/// When an acquisition may contact its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquirePolicy {
    /// Use the cache only; fail if the image is not cached.
    CachedOnly,
    /// Contact the source only when the image is not cached.
    IfMissing,
    /// Always re-resolve against the source.
    Refresh,
}

/// A verified image in awman's cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredImage {
    pub identity: ImageIdentity,
    /// The archive, digest-named, under `<state_dir>/oci-cache`.
    pub archive: PathBuf,
    pub archive_format: ArchiveFormat,
    /// Archive size in bytes.
    pub bytes: u64,
}

/// Layout of an archive on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// OCI image layout (`oci-layout`, `index.json`, `blobs/`).
    OciLayout,
    /// `docker save` layout (`manifest.json`, per-layer tars).
    DockerSave,
}

/// Progress reported while acquiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireProgress {
    Resolving {
        source: ImageSourceKind,
        reference: String,
    },
    Layer {
        digest: Digest,
        done: u64,
        total: Option<u64>,
    },
    Verifying {
        digest: Digest,
    },
    Cached {
        identity: ImageIdentity,
    },
}

/// Bounds every acquisition enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireLimits {
    pub max_archive_bytes: u64,
    pub max_layer_bytes: u64,
    pub max_layers: usize,
    /// Free space that must remain on the cache's filesystem.
    pub min_free_bytes: u64,
}

impl AcquireLimits {
    /// 64 GiB archive, 32 GiB per layer (compressed and uncompressed), 256
    /// layers, 2 GiB left free.
    pub const DEFAULT: Self = Self {
        max_archive_bytes: 64 << 30,
        max_layer_bytes: 32 << 30,
        max_layers: 256,
        min_free_bytes: 2 << 30,
    };
}

impl Default for AcquireLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The acquirer for `state_dir` (cache under `<state_dir>/oci-cache`).
pub fn default_acquirer(
    state_dir: &Path,
    limits: AcquireLimits,
    env: &EnvSnapshot,
) -> Box<dyn ImageAcquirer> {
    Box::new(CachingAcquirer::new(
        state_dir,
        limits,
        env.clone(),
        Arc::new(verify::HostDiskSpace),
    ))
}

/// The non-secret image defaults recorded when `archive` (an
/// [`AcquiredImage::archive`] path) was cached: `USER`, `HOME`, `WORKDIR`.
pub fn cached_image_config(
    state_dir: &Path,
    archive: &Path,
) -> Result<Option<ImageConfigSummary>, EngineError> {
    let cache = OciCache::open(state_dir)?;
    Ok(cache.record_for_archive(archive)?.map(|r| r.config))
}

/// Remove cached archives whose manifest digest is not in `keep`.
pub fn prune_cache(state_dir: &Path, keep: &[Digest]) -> Result<PruneReport, EngineError> {
    OciCache::open(state_dir)?.prune(keep)
}

/// The cache-backed acquirer. The disk probe is injectable so the
/// insufficient-space path is testable.
pub struct CachingAcquirer {
    state_dir: PathBuf,
    limits: AcquireLimits,
    env: EnvSnapshot,
    disk: Arc<dyn DiskSpace>,
}

impl CachingAcquirer {
    pub fn new(
        state_dir: &Path,
        limits: AcquireLimits,
        env: EnvSnapshot,
        disk: Arc<dyn DiskSpace>,
    ) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            limits,
            env,
            disk,
        }
    }
}

impl ImageAcquirer for CachingAcquirer {
    fn acquire(
        &self,
        request: &AcquireRequest,
        progress: &mut dyn FnMut(AcquireProgress),
    ) -> Result<AcquiredImage, EngineError> {
        let kind = request.source.kind();
        let reference = request.source.reference_or(&request.tag);
        let resolved = sources::Resolved::resolve(
            &request.source,
            &reference,
            &request.registries,
            &self.env,
        )?;
        let key = resolved.key(kind.as_str(), &reference, &request.platform);
        progress(AcquireProgress::Resolving {
            source: kind,
            reference: reference.clone(),
        });

        let cache = OciCache::open(&self.state_dir)?;
        if request.policy != AcquirePolicy::Refresh {
            if let Some(hit) = cache.lookup(&key)? {
                let archive_changed = match (&resolved, &hit.record.source_fingerprint) {
                    // An archive re-exported at the same path is re-imported
                    // (only when the source is allowed to be read at all).
                    (sources::Resolved::Archive { path }, Some(fp))
                        if request.policy == AcquirePolicy::IfMissing =>
                    {
                        cache::SourceFingerprint::of(path).is_some_and(|now| now != *fp)
                    }
                    _ => false,
                };
                if !archive_changed && request.platform.matches(&hit.image.identity.platform) {
                    progress(AcquireProgress::Cached {
                        identity: hit.image.identity.clone(),
                    });
                    return Ok(hit.image);
                }
            }
            if request.policy == AcquirePolicy::CachedOnly {
                return Err(EngineError::Container(format!(
                    "image '{}' is not in the builtin image cache ({} {}); run `awman ready` \
                     to import it",
                    request.tag,
                    kind,
                    resolved.locator()
                )));
            }
        }

        let staging = cache.staging()?;
        verify::ensure_space(
            self.disk.as_ref(),
            staging.path(),
            0,
            self.limits.min_free_bytes,
        )?;
        let ctx = sources::FetchContext {
            staging_dir: staging.path(),
            limits: self.limits,
            disk: self.disk.as_ref(),
            platform: &request.platform,
        };
        let fetch = |report: sources::Report<'_>| {
            let fetched = resolved.fetch(&request.tag, &reference, &ctx, report)?;
            let validated = archive::validate_archive(
                &fetched.path,
                &request.platform,
                &fetched.wanted_refs,
                &self.limits,
                &mut |p| report(p),
            )?;
            Ok::<_, EngineError>((fetched, validated))
        };
        let (fetched, validated) = if resolved.is_network() {
            sources::run_isolated(progress, fetch)?
        } else {
            let events = std::sync::Mutex::new(Vec::new());
            let out = fetch(&|p| {
                if let Ok(mut e) = events.lock() {
                    e.push(p)
                }
            });
            for p in events.into_inner().unwrap_or_default() {
                progress(p);
            }
            out?
        };

        if let Some(expected) = &fetched.expected_manifest {
            if *expected != validated.manifest_digest {
                return Err(EngineError::ImageDigestMismatch {
                    reference: reference.clone(),
                    expected: expected.to_string(),
                    actual: validated.manifest_digest.to_string(),
                });
            }
        }
        let identity = ImageIdentity {
            reference: reference.clone(),
            manifest_digest: validated.manifest_digest.clone(),
            config_digest: validated.config_digest.clone(),
            platform: validated.platform.clone(),
            source: kind,
        };
        let committed = cache.commit(
            &fetched.path,
            &key,
            identity,
            &validated,
            resolved.locator(),
            fetched.fingerprint,
        )?;
        drop(staging);
        progress(AcquireProgress::Cached {
            identity: committed.image.identity.clone(),
        });
        Ok(committed.image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::oci::archive::tests::{
        arm64, docker_save_archive, layer_tar, oci_archive, L,
    };
    use crate::engine::oci::verify::tests::FixedDisk;

    fn acquirer(state: &Path, disk: Option<u64>) -> CachingAcquirer {
        CachingAcquirer::new(
            state,
            archive::tests::limits(),
            EnvSnapshot::empty(),
            Arc::new(FixedDisk(disk)),
        )
    }

    fn request(path: &Path, policy: AcquirePolicy) -> AcquireRequest {
        AcquireRequest {
            tag: "awman-x-claude:latest".into(),
            source: ImageSourceSpec::Archive {
                path: path.to_path_buf(),
            },
            platform: arm64(),
            policy,
            registries: BTreeMap::new(),
        }
    }

    fn layer() -> Vec<u8> {
        layer_tar(&[L::File("bin/tool", b"x", 0o755)])
    }

    fn cache_entries(state: &Path) -> usize {
        std::fs::read_dir(state.join(cache::CACHE_DIR).join("images"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn archive_source_is_imported_then_served_from_cache() {
        let state = tempfile::tempdir().unwrap();
        let src = state.path().join("export.tar");
        let (bytes, manifest_hex) = oci_archive(&arm64(), &[layer()]);
        std::fs::write(&src, &bytes).unwrap();
        let acq = acquirer(state.path(), None);

        let mut events = Vec::new();
        let first = acq
            .acquire(&request(&src, AcquirePolicy::IfMissing), &mut |p| {
                events.push(p)
            })
            .unwrap();
        assert_eq!(first.identity.source, ImageSourceKind::Archive);
        assert_eq!(
            first.identity.manifest_digest.as_str(),
            format!("sha256:{manifest_hex}")
        );
        assert_eq!(first.archive_format, ArchiveFormat::OciLayout);
        assert!(matches!(
            events.first(),
            Some(AcquireProgress::Resolving { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(AcquireProgress::Cached { .. })
        ));

        // Cached use needs no source at all.
        std::fs::remove_file(&src).unwrap();
        let again = acq
            .acquire(&request(&src, AcquirePolicy::CachedOnly), &mut |_| {})
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(
            cached_image_config(state.path(), &first.archive)
                .unwrap()
                .unwrap()
                .home
                .as_deref(),
            Some("/home/agent")
        );
    }

    #[test]
    fn cached_only_miss_never_reads_the_source() {
        let state = tempfile::tempdir().unwrap();
        let src = state.path().join("export.tar");
        std::fs::write(&src, docker_save_archive(&arm64(), &[layer()])).unwrap();
        let err = acquirer(state.path(), None)
            .acquire(&request(&src, AcquirePolicy::CachedOnly), &mut |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("awman ready"), "{err}");
        assert_eq!(cache_entries(state.path()), 0);
    }

    #[test]
    fn rejected_archives_leave_no_cache_state() {
        let state = tempfile::tempdir().unwrap();
        let src = state.path().join("export.tar");
        let evil = layer_tar(&[L::File("../../etc/passwd", b"x", 0o644)]);
        std::fs::write(&src, oci_archive(&arm64(), &[evil]).0).unwrap();
        let acq = acquirer(state.path(), None);
        assert!(matches!(
            acq.acquire(&request(&src, AcquirePolicy::IfMissing), &mut |_| {}),
            Err(EngineError::ImageArchiveRejected { .. })
        ));
        let (wrong, _) = oci_archive(&archive::tests::amd64(), &[layer()]);
        std::fs::write(&src, wrong).unwrap();
        assert!(matches!(
            acq.acquire(&request(&src, AcquirePolicy::IfMissing), &mut |_| {}),
            Err(EngineError::ImagePlatformMismatch { .. })
        ));
        assert_eq!(cache_entries(state.path()), 0);
        let tmp = std::fs::read_dir(state.path().join(cache::CACHE_DIR).join("tmp"))
            .unwrap()
            .count();
        assert_eq!(tmp, 0, "staging is removed on failure");
    }

    #[test]
    fn insufficient_disk_space_is_reported_before_staging() {
        let state = tempfile::tempdir().unwrap();
        let src = state.path().join("export.tar");
        std::fs::write(&src, oci_archive(&arm64(), &[layer()]).0).unwrap();
        let mut limits = archive::tests::limits();
        limits.min_free_bytes = 1 << 30;
        let acq = CachingAcquirer::new(
            state.path(),
            limits,
            EnvSnapshot::empty(),
            Arc::new(FixedDisk(Some(1024))),
        );
        assert!(matches!(
            acq.acquire(&request(&src, AcquirePolicy::IfMissing), &mut |_| {}),
            Err(EngineError::InsufficientDiskSpace { .. })
        ));
        assert_eq!(cache_entries(state.path()), 0);
    }

    #[test]
    fn a_re_exported_archive_is_re_imported() {
        let state = tempfile::tempdir().unwrap();
        let src = state.path().join("export.tar");
        std::fs::write(&src, oci_archive(&arm64(), &[layer()]).0).unwrap();
        let acq = acquirer(state.path(), None);
        let first = acq
            .acquire(&request(&src, AcquirePolicy::IfMissing), &mut |_| {})
            .unwrap();
        let other = layer_tar(&[L::File("bin/other", b"y", 0o755)]);
        std::fs::write(&src, oci_archive(&arm64(), &[layer(), other]).0).unwrap();
        let second = acq
            .acquire(&request(&src, AcquirePolicy::IfMissing), &mut |_| {})
            .unwrap();
        assert_ne!(
            first.identity.manifest_digest,
            second.identity.manifest_digest
        );
    }

    #[test]
    fn apple_store_is_blocked_without_running_anything() {
        let state = tempfile::tempdir().unwrap();
        let req = AcquireRequest {
            source: ImageSourceSpec::AppleStore { reference: None },
            ..request(Path::new("/unused"), AcquirePolicy::IfMissing)
        };
        assert!(matches!(
            acquirer(state.path(), None).acquire(&req, &mut |_| {}),
            Err(EngineError::ImageSourceBlocked {
                source_kind: ImageSourceKind::AppleStore,
                ..
            })
        ));
    }

    mod engine_store {
        //! Docker Engine source tests. Kept out of any module or function named
        //! `docker` so `make test-fast` (which skips `docker` tests, meaning "needs
        //! a real daemon") still runs these hermetic ones.
        use std::path::PathBuf;

        use crate::data::config::env::EnvSnapshot;
        use crate::data::config::image_source::DockerTlsConfig;
        use crate::engine::error::EngineError;
        use crate::engine::oci::docker_engine::*;

        fn env(pairs: &[(&str, &str)]) -> EnvSnapshot {
            EnvSnapshot::with_overrides(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        }

        #[test]
        fn explicit_hosts_resolve() {
            assert_eq!(
                resolve_endpoint(Some("unix:///run/d.sock"), None, &EnvSnapshot::empty()).unwrap(),
                Endpoint::Unix("/run/d.sock".into())
            );
            assert_eq!(
                resolve_endpoint(Some("tcp://127.0.0.1:2375"), None, &EnvSnapshot::empty())
                    .unwrap(),
                Endpoint::Tcp {
                    host: "127.0.0.1".into(),
                    port: 2375,
                    tls: None
                }
            );
            let tls = DockerTlsConfig {
                ca: "/ca.pem".into(),
                cert: None,
                key: None,
                verify: true,
            };
            assert_eq!(
                resolve_endpoint(
                    Some("tcp://docker.example"),
                    Some(&tls),
                    &EnvSnapshot::empty()
                )
                .unwrap(),
                Endpoint::Tcp {
                    host: "docker.example".into(),
                    port: 2376,
                    tls: Some(tls)
                }
            );
        }

        #[test]
        fn remote_plain_tcp_ssh_npipe_and_userinfo_are_refused() {
            let e = EnvSnapshot::empty();
            assert!(matches!(
                resolve_endpoint(Some("tcp://10.0.0.5:2375"), None, &e),
                Err(EngineError::UnsupportedImageSource { .. })
            ));
            assert!(matches!(
                resolve_endpoint(Some("ssh://me@host"), None, &e),
                Err(EngineError::UnsupportedImageSource { .. })
            ));
            assert!(matches!(
                resolve_endpoint(Some("npipe:////./pipe/docker_engine"), None, &e),
                Err(EngineError::UnsupportedImageSource { .. })
            ));
            assert!(matches!(
                resolve_endpoint(Some("tcp://u:p@127.0.0.1:1"), None, &e),
                Err(EngineError::Config(_))
            ));
        }

        #[test]
        fn engine_host_and_tls_env_are_honoured() {
            let e = env(&[
                ("DOCKER_HOST", "tcp://docker.example:2376"),
                ("DOCKER_TLS_VERIFY", "1"),
                ("DOCKER_CERT_PATH", "/certs"),
            ]);
            match resolve_endpoint(None, None, &e).unwrap() {
                Endpoint::Tcp {
                    host,
                    tls: Some(tls),
                    ..
                } => {
                    assert_eq!(host, "docker.example");
                    assert_eq!(tls.ca, PathBuf::from("/certs/ca.pem"));
                    assert_eq!(tls.key, Some(PathBuf::from("/certs/key.pem")));
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        #[test]
        fn default_socket_is_off_limits_under_test_isolation() {
            assert!(matches!(
                resolve_endpoint(None, None, &EnvSnapshot::empty()),
                Err(EngineError::UnsupportedImageSource { .. })
            ));
        }

        #[test]
        fn references_that_could_escape_the_api_path_are_refused() {
            for bad in [
                "",
                "../x",
                "x?all=1",
                "x#y",
                "/x",
                "a b",
                "x/../../containers",
            ] {
                assert!(validate_reference(bad).is_err(), "{bad:?}");
            }
            for good in [
                "awman-x-claude:latest",
                "localhost:5000/a/b:1",
                "x@sha256:abc",
            ] {
                assert!(validate_reference(good).is_ok(), "{good:?}");
            }
        }

        // ── Hermetic exports against a fake Engine on a Unix socket ────────────

        #[cfg(unix)]
        mod fake_daemon {
            use std::collections::BTreeMap;
            use std::io::{BufRead, BufReader, Write};
            use std::os::unix::net::UnixListener;
            use std::path::{Path, PathBuf};
            use std::sync::{Arc, Mutex};

            use crate::data::config::env::EnvSnapshot;
            use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
            use crate::engine::error::EngineError;
            use crate::engine::oci::archive::tests::{
                arm64, docker_save_archive, layer_tar, limits, L,
            };
            use crate::engine::oci::verify::tests::FixedDisk;
            use crate::engine::oci::{
                AcquirePolicy, AcquireRequest, ArchiveFormat, CachingAcquirer, ImageAcquirer,
            };

            /// `(status, body, truncate_to)` for a request path.
            type Handler = dyn Fn(&str) -> (u16, Vec<u8>, Option<usize>) + Send + Sync;

            /// A minimal HTTP/1.1 Engine on `sock`. Records every request path.
            fn spawn(sock: &Path, handler: Arc<Handler>) -> Arc<Mutex<Vec<String>>> {
                let listener = UnixListener::bind(sock).unwrap();
                let seen = Arc::new(Mutex::new(Vec::new()));
                let log = seen.clone();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { break };
                        let handler = handler.clone();
                        let log = log.clone();
                        std::thread::spawn(move || {
                            let mut reader = BufReader::new(stream.try_clone().unwrap());
                            let mut out = stream;
                            loop {
                                let mut line = String::new();
                                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                    return;
                                }
                                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                                loop {
                                    let mut h = String::new();
                                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                                        break;
                                    }
                                }
                                log.lock().unwrap().push(path.clone());
                                let (status, body, truncate) = handler(&path);
                                let head = format!(
                                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n",
                                    body.len()
                                );
                                let _ = out.write_all(head.as_bytes());
                                match truncate {
                                    Some(n) => {
                                        let _ = out.write_all(&body[..n]);
                                        return;
                                    }
                                    None => {
                                        let _ = out.write_all(&body);
                                    }
                                }
                            }
                        });
                    }
                });
                seen
            }

            fn standard(
                api: &'static str,
                os_arch: (&'static str, &'static str),
                archive: Vec<u8>,
                truncate: bool,
            ) -> Arc<Handler> {
                Arc::new(move |path: &str| {
                    let p = path.split('?').next().unwrap_or("");
                    match p {
                        "/_ping" => (200, b"OK".to_vec(), None),
                        "/version" => (
                            200,
                            format!(r#"{{"ApiVersion":"{api}"}}"#).into_bytes(),
                            None,
                        ),
                        _ if p.ends_with("/images/awman-x-claude:latest/json") => (
                            200,
                            format!(
                                r#"{{"Os":"{}","Architecture":"{}","Size":1000}}"#,
                                os_arch.0, os_arch.1
                            )
                            .into_bytes(),
                            None,
                        ),
                        _ if p.ends_with("/images/awman-x-claude:latest/get") => {
                            let n = archive.len() / 2;
                            (200, archive.clone(), truncate.then_some(n))
                        }
                        _ => (404, br#"{"message":"No such image"}"#.to_vec(), None),
                    }
                })
            }

            fn request(sock: &Path, policy: AcquirePolicy) -> AcquireRequest {
                AcquireRequest {
                    tag: "awman-x-claude:latest".into(),
                    source: ImageSourceSpec::DockerStore {
                        host: Some(format!("unix://{}", sock.display())),
                        tls: None,
                        reference: None,
                    },
                    platform: arm64(),
                    policy,
                    registries: BTreeMap::new(),
                }
            }

            fn acquire(
                state: &Path,
                req: &AcquireRequest,
            ) -> Result<crate::engine::oci::AcquiredImage, EngineError> {
                CachingAcquirer::new(
                    state,
                    limits(),
                    EnvSnapshot::empty(),
                    Arc::new(FixedDisk(None)),
                )
                .acquire(req, &mut |_| {})
            }

            fn archive() -> Vec<u8> {
                docker_save_archive(&arm64(), &[layer_tar(&[L::File("bin/agent", b"x", 0o755)])])
            }

            fn setup() -> (tempfile::TempDir, PathBuf) {
                let dir = tempfile::tempdir().unwrap();
                let sock = dir.path().join("d.sock");
                (dir, sock)
            }

            #[test]
            fn export_is_validated_cached_and_reused_without_the_daemon() {
                let (dir, sock) = setup();
                let seen = spawn(
                    &sock,
                    standard("1.45", ("linux", "arm64"), archive(), false),
                );
                let got = acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap();
                assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
                assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
                assert_eq!(got.identity.reference, "awman-x-claude:latest");
                let paths = seen.lock().unwrap().clone();
                assert!(
                    paths.contains(&"/v1.41/images/awman-x-claude:latest/get".to_string()),
                    "{paths:?}"
                );

                // The daemon disappears; running the cached image does not care.
                std::fs::remove_file(&sock).unwrap();
                let cached =
                    acquire(dir.path(), &request(&sock, AcquirePolicy::CachedOnly)).unwrap();
                assert_eq!(cached, got);
            }

            #[test]
            fn new_engines_are_asked_for_the_native_platform() {
                let (dir, sock) = setup();
                let seen = spawn(
                    &sock,
                    standard("1.49", ("linux", "arm64"), archive(), false),
                );
                acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap();
                let paths = seen.lock().unwrap().clone();
                let get = paths.iter().find(|p| p.contains("/get")).unwrap();
                assert!(
                    get.starts_with("/v1.48/images/awman-x-claude:latest/get?platform="),
                    "{get}"
                );
                assert!(get.contains("arm64"), "{get}");
            }

            #[test]
            fn a_default_variant_mismatch_still_exports_the_native_platform() {
                // The unqualified inspect reports amd64, but a 1.48+ engine
                // exports the requested arm64 variant, which validates.
                let (dir, sock) = setup();
                let seen = spawn(
                    &sock,
                    standard("1.49", ("linux", "amd64"), archive(), false),
                );
                acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap();
                assert!(seen.lock().unwrap().iter().any(|p| p.contains("/get")));
            }

            #[test]
            fn wrong_platform_is_refused_before_export() {
                let (dir, sock) = setup();
                let seen = spawn(
                    &sock,
                    standard("1.45", ("linux", "amd64"), archive(), false),
                );
                let err =
                    acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap_err();
                assert!(
                    matches!(err, EngineError::ImagePlatformMismatch { .. }),
                    "{err:?}"
                );
                assert!(!seen.lock().unwrap().iter().any(|p| p.contains("/get")));
            }

            #[test]
            fn missing_image_names_the_engine() {
                let (dir, sock) = setup();
                spawn(
                    &sock,
                    standard("1.45", ("linux", "arm64"), archive(), false),
                );
                let mut req = request(&sock, AcquirePolicy::IfMissing);
                req.source = ImageSourceSpec::DockerStore {
                    host: Some(format!("unix://{}", sock.display())),
                    tls: None,
                    reference: Some("nope:1".into()),
                };
                let err = acquire(dir.path(), &req).unwrap_err();
                let text = err.to_string();
                assert!(
                    text.contains("nope:1") && text.contains("unix://"),
                    "{text}"
                );
            }

            #[test]
            fn truncated_export_leaves_no_cache_state() {
                let (dir, sock) = setup();
                spawn(&sock, standard("1.45", ("linux", "arm64"), archive(), true));
                let err =
                    acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap_err();
                assert!(
                    matches!(
                        err,
                        EngineError::Network(_) | EngineError::ImageArchiveRejected { .. }
                    ),
                    "{err:?}"
                );
                let images = dir.path().join("oci-cache/images");
                assert_eq!(std::fs::read_dir(images).unwrap().count(), 0);
            }

            #[test]
            fn unreachable_engine_is_a_network_error() {
                let (dir, sock) = setup();
                let err =
                    acquire(dir.path(), &request(&sock, AcquirePolicy::IfMissing)).unwrap_err();
                assert!(
                    matches!(err, EngineError::Network(ref m) if m.contains("not reachable")),
                    "{err:?}"
                );
            }
        }
    }
}
