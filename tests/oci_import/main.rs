//! Public-contract tests for OCI image acquisition and the verified image
//! cache.
//!
//! Every fixture is assembled in memory or served by a loopback double built
//! in this process; nothing invokes Docker, reads user credentials, or uses a
//! per-user cache. The opt-in real-service tier is `real_stores`, gated by
//! `AWMAN_TEST_IMAGE_STORES=1` plus the specific service gates it documents.
//!
//! Modules:
//! * `support` — fixture builders, Docker Engine / registry / proxy doubles,
//!   TLS material;
//! * `docker_store` — Docker Engine export over Unix and TLS, API versions,
//!   missing images, disconnects, bounded retry, cancellation;
//! * `registry_store` — registry pulls over TLS with a private CA, token
//!   realms, native credentials, proxy/no-proxy and secret redaction;
//! * `corpus` — docker-save / OCI / Apple-shaped archives, layer preservation,
//!   the zstd / non-distributable format policy, real corpus (gated);
//! * `cache_invariants` — cached-execution invariants of the archive cache;
//! * `real_stores` — disposable real services (gated).

mod cache_invariants;
mod cancellation;
mod corpus;
mod docker_store;
mod real_stores;
mod registry_store;
mod support;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use awman::data::config::env::EnvSnapshot;
use awman::data::config::image_source::ImageSourceSpec;
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::DiskSpace;
use awman::engine::oci::{
    cached_image_config, AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer,
    ImageAcquirer,
};
use sha2::{Digest as _, Sha256};

struct PlentyOfSpace;

impl DiskSpace for PlentyOfSpace {
    fn available(&self, _path: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn tar_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

fn layer_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    tar_file(&mut builder, "etc/os-release", b"ID=awman-test\n");
    builder.into_inner().unwrap()
}

/// Build a small, valid single-image OCI layout, including real layer/config/
/// manifest digests. No checked-in or downloaded image fixture is required.
fn oci_layout(platform: &OciPlatform) -> Vec<u8> {
    let layer = layer_tar();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&layer).unwrap();
    let compressed_layer = encoder.finish().unwrap();
    let layer_digest = hex(&compressed_layer);
    let config = serde_json::to_vec(&serde_json::json!({
        "os": platform.os,
        "architecture": platform.architecture,
        "config": {
            "User": "1000:1000",
            "Env": ["HOME=/home/agent", "PRIVATE_VALUE=not-for-cache-metadata"],
            "WorkingDir": "/workspace"
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{}", hex(&layer))]
        }
    }))
    .unwrap();
    let config_digest = hex(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": compressed_layer.len()
        }]
    }))
    .unwrap();
    let manifest_digest = hex(&manifest);

    let mut archive = tar::Builder::new(Vec::new());
    tar_file(
        &mut archive,
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    );
    tar_file(
        &mut archive,
        &format!("blobs/sha256/{layer_digest}"),
        &compressed_layer,
    );
    tar_file(
        &mut archive,
        &format!("blobs/sha256/{config_digest}"),
        &config,
    );
    tar_file(
        &mut archive,
        &format!("blobs/sha256/{manifest_digest}"),
        &manifest,
    );
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest.len(),
            "annotations": {"org.opencontainers.image.ref.name": "fixture:latest"}
        }]
    }))
    .unwrap();
    tar_file(&mut archive, "index.json", &index);
    archive.into_inner().unwrap()
}

fn acquirer(state: &Path) -> CachingAcquirer {
    let mut limits = AcquireLimits::DEFAULT;
    limits.min_free_bytes = 0;
    CachingAcquirer::new(state, limits, EnvSnapshot::empty(), Arc::new(PlentyOfSpace))
}

fn request(path: &Path, platform: OciPlatform, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: "awman-fixture:latest".into(),
        source: ImageSourceSpec::Archive {
            path: path.to_path_buf(),
        },
        platform,
        policy,
        registries: Default::default(),
    }
}

fn assert_cache_empty(state: &Path) {
    let root = state.join("oci-cache");
    for dir in [root.join("images"), root.join("refs"), root.join("tmp")] {
        if dir.exists() {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
    }
}

#[test]
fn archive_import_persists_identity_and_cached_only_reuse_needs_no_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fixture.oci.tar");
    std::fs::write(&source, oci_layout(&OciPlatform::host_linux())).unwrap();
    let state = temp.path().join("private-state");
    let acquirer = acquirer(&state);

    let imported = acquirer
        .acquire(
            &request(&source, OciPlatform::host_linux(), AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(imported.identity.source.as_str(), "archive");
    assert_eq!(imported.identity.reference, "awman-fixture:latest");
    let config = cached_image_config(&state, &imported.archive)
        .unwrap()
        .expect("validated config summary is recorded");
    assert_eq!(config.user.as_deref(), Some("1000:1000"));
    assert_eq!(config.home.as_deref(), Some("/home/agent"));
    assert_eq!(config.working_dir.as_deref(), Some("/workspace"));
    let cache_record = std::fs::read_to_string(imported.archive.with_extension("json")).unwrap();
    assert!(!cache_record.contains("not-for-cache-metadata"));

    std::fs::remove_file(&source).unwrap();
    let hit = acquirer
        .acquire(
            &request(
                &source,
                OciPlatform::host_linux(),
                AcquirePolicy::CachedOnly,
            ),
            &mut |_| {},
        )
        .expect("cached-only resolves without reading the absent archive");
    assert_eq!(hit.identity, imported.identity);
    assert_eq!(hit.archive, imported.archive);
}

#[test]
fn wrong_platform_and_malformed_transfer_leave_no_visible_cache_state() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("private-state");
    let source = temp.path().join("foreign.oci.tar");
    let foreign = OciPlatform {
        os: "linux".into(),
        architecture: if OciPlatform::host_linux().architecture == "amd64" {
            "arm64".into()
        } else {
            "amd64".into()
        },
        variant: None,
    };
    std::fs::write(&source, oci_layout(&foreign)).unwrap();
    assert!(acquirer(&state)
        .acquire(
            &request(&source, OciPlatform::host_linux(), AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .is_err());
    assert_cache_empty(&state);

    std::fs::write(&source, b"not an OCI or docker-save archive").unwrap();
    assert!(acquirer(&state)
        .acquire(
            &request(&source, OciPlatform::host_linux(), AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .is_err());
    assert_cache_empty(&state);
}

#[test]
fn apple_store_is_a_typed_blocker_with_dated_evidence() {
    use awman::engine::error::EngineError;
    use awman::engine::oci::apple_store::{feasibility, Blocker, Feasibility, CONTRACT};
    let temp = tempfile::tempdir().unwrap();
    let req = AcquireRequest {
        tag: "awman-fixture:latest".into(),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: Default::default(),
    };
    match acquirer(&temp.path().join("s")).acquire(&req, &mut |_| {}) {
        Err(EngineError::ImageSourceBlocked { reason, .. }) => {
            assert!(reason.contains("container image save awman-fixture:latest"));
            assert!(reason.contains("apple/container 1.4.1"));
        }
        other => panic!("expected a blocked source, got {other:?}"),
    }
    match feasibility() {
        Feasibility::Blocked(blockers) => {
            assert!(blockers.contains(&Blocker::UnversionedProtocol));
            assert!(blockers.contains(&Blocker::RequiresUnsafeFfi));
        }
        Feasibility::Available { .. } => panic!("no bridge exists"),
    }
    let contract = CONTRACT;
    assert!(!contract.protocol_versioned);
    assert_cache_empty(&temp.path().join("s"));
}
