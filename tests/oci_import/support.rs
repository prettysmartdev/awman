//! Shared fixtures for the OCI import tests: in-memory image archives with
//! realistic layer contents, disposable loopback services (a Docker Engine
//! double over a Unix socket or TLS, a registry double over TLS, an HTTP
//! CONNECT proxy) and throwaway TLS material.
//!
//! Everything here is built inside the test process. Nothing contacts a
//! real daemon, registry, credential store or the developer's HOME.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use awman::data::config::env::EnvSnapshot;
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{AcquireLimits, CachingAcquirer, DiskSpace, RetryPolicy};
use sha2::{Digest as _, Sha256};

// ── Small helpers ───────────────────────────────────────────────────────────

pub struct PlentyOfSpace;

impl DiskSpace for PlentyOfSpace {
    fn available(&self, _path: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

pub fn limits() -> AcquireLimits {
    AcquireLimits {
        max_archive_bytes: 256 * 1024 * 1024,
        max_layer_bytes: 64 * 1024 * 1024,
        max_layers: 64,
        min_free_bytes: 0,
    }
}

/// An acquirer with no backoff sleeps and a short deadline, so retry tests
/// finish quickly.
pub fn acquirer(state: &Path) -> CachingAcquirer {
    acquirer_with_env(state, EnvSnapshot::empty())
}

pub fn acquirer_with_env(state: &Path, env: EnvSnapshot) -> CachingAcquirer {
    CachingAcquirer::new(state, limits(), env, Arc::new(PlentyOfSpace))
        .with_retry(RetryPolicy {
            max_attempts: 3,
            deadline: Duration::from_secs(60),
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
        })
        .with_sleeper(Arc::new(|_| {}))
}

pub fn arm64() -> OciPlatform {
    OciPlatform {
        os: "linux".into(),
        architecture: "arm64".into(),
        variant: None,
    }
}

pub fn amd64() -> OciPlatform {
    OciPlatform {
        os: "linux".into(),
        architecture: "amd64".into(),
        variant: None,
    }
}

pub fn other_platform(than: &OciPlatform) -> OciPlatform {
    if than.architecture == "amd64" {
        arm64()
    } else {
        amd64()
    }
}

pub fn cache_dir(state: &Path) -> PathBuf {
    state.join("oci-cache")
}

pub fn cached_archives(state: &Path) -> usize {
    std::fs::read_dir(cache_dir(state).join("images"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .ok()
                    .and_then(|e| e.path().extension().map(|x| x == "tar"))
                    .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0)
}

pub fn staging_dirs(state: &Path) -> usize {
    std::fs::read_dir(cache_dir(state).join("tmp"))
        .map(|d| d.count())
        .unwrap_or(0)
}

// ── Layer builder ───────────────────────────────────────────────────────────

/// One entry of a synthetic layer tar. Ownership, modes, links, whiteouts
/// and xattrs are all expressible so the corpus tests can check that they
/// survive acquisition untouched.
#[derive(Clone, Debug)]
pub enum Entry {
    File {
        path: String,
        data: Vec<u8>,
        mode: u32,
        uid: u64,
        gid: u64,
        xattrs: Vec<(String, String)>,
    },
    Dir {
        path: String,
        mode: u32,
        uid: u64,
        gid: u64,
    },
    Symlink {
        path: String,
        target: String,
    },
    Hardlink {
        path: String,
        target: String,
    },
    Whiteout {
        /// The hidden path (`.wh.` is inserted for you).
        path: String,
    },
    Opaque {
        dir: String,
    },
}

impl Entry {
    pub fn file(path: &str, data: &[u8], mode: u32) -> Self {
        Self::File {
            path: path.into(),
            data: data.to_vec(),
            mode,
            uid: 0,
            gid: 0,
            xattrs: Vec::new(),
        }
    }
    pub fn owned(path: &str, data: &[u8], mode: u32, uid: u64, gid: u64) -> Self {
        Self::File {
            path: path.into(),
            data: data.to_vec(),
            mode,
            uid,
            gid,
            xattrs: Vec::new(),
        }
    }
    pub fn with_xattr(mut self, key: &str, value: &str) -> Self {
        if let Self::File { xattrs, .. } = &mut self {
            xattrs.push((key.into(), value.into()));
        }
        self
    }
    pub fn dir(path: &str, mode: u32) -> Self {
        Self::Dir {
            path: path.into(),
            mode,
            uid: 0,
            gid: 0,
        }
    }
    pub fn symlink(path: &str, target: &str) -> Self {
        Self::Symlink {
            path: path.into(),
            target: target.into(),
        }
    }
    pub fn hardlink(path: &str, target: &str) -> Self {
        Self::Hardlink {
            path: path.into(),
            target: target.into(),
        }
    }
    pub fn whiteout(path: &str) -> Self {
        Self::Whiteout { path: path.into() }
    }
    pub fn opaque(dir: &str) -> Self {
        Self::Opaque { dir: dir.into() }
    }
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    // "%d %s=%s\n" where %d is the record's own length.
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let digits = len.to_string().len();
        if digits + body.len() == len {
            break;
        }
        len = digits + body.len();
    }
    format!("{len}{body}").into_bytes()
}

/// Build an uncompressed layer tar (USTAR/PAX as real builders emit).
pub fn layer(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        match entry {
            Entry::File {
                path,
                data,
                mode,
                uid,
                gid,
                xattrs,
            } => {
                if !xattrs.is_empty() {
                    let mut body = Vec::new();
                    for (k, v) in xattrs {
                        body.extend(pax_record(&format!("SCHILY.xattr.{k}"), v));
                    }
                    let mut header = tar::Header::new_ustar();
                    header.set_entry_type(tar::EntryType::XHeader);
                    header.set_size(body.len() as u64);
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_mode(0o644);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, "PaxHeaders/x", &body[..])
                        .unwrap();
                }
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_uid(*uid);
                header.set_gid(*gid);
                header.set_size(data.len() as u64);
                header.set_mtime(1_700_000_000);
                header.set_cksum();
                builder.append_data(&mut header, path, &data[..]).unwrap();
            }
            Entry::Dir {
                path,
                mode,
                uid,
                gid,
            } => {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                header.set_uid(*uid);
                header.set_gid(*gid);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("{path}/"), &[][..])
                    .unwrap();
            }
            Entry::Symlink { path, target } => {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_cksum();
                builder.append_link(&mut header, path, target).unwrap();
            }
            Entry::Hardlink { path, target } => {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Link);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mode(0o644);
                header.set_size(0);
                header.set_cksum();
                builder.append_link(&mut header, path, target).unwrap();
            }
            Entry::Whiteout { path } => {
                let (parent, base) = match path.rsplit_once('/') {
                    Some((p, b)) => (format!("{p}/"), b.to_string()),
                    None => (String::new(), path.clone()),
                };
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mode(0o644);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("{parent}.wh.{base}"), &[][..])
                    .unwrap();
            }
            Entry::Opaque { dir } => {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mode(0o644);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("{dir}/.wh..wh..opq"), &[][..])
                    .unwrap();
            }
        }
    }
    builder.into_inner().unwrap()
}

/// The entries of a layer tar as `(path, type, mode, uid, gid, link, xattrs)`.
pub type LayerEntry = (
    String,
    tar::EntryType,
    u32,
    u64,
    u64,
    Option<String>,
    Vec<(String, String)>,
);

pub fn read_layer(bytes: &[u8]) -> Vec<LayerEntry> {
    let reader: Box<dyn Read> = if bytes.len() >= 2 && bytes[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let header = entry.header().clone();
        let link = header
            .link_name()
            .unwrap()
            .map(|l| l.to_string_lossy().into_owned());
        let mut xattrs = Vec::new();
        if let Some(pax) = entry.pax_extensions().unwrap() {
            for ext in pax.flatten() {
                if let (Ok(k), Ok(v)) = (ext.key(), ext.value()) {
                    if let Some(name) = k.strip_prefix("SCHILY.xattr.") {
                        xattrs.push((name.to_string(), v.to_string()));
                    }
                }
            }
        }
        out.push((
            path,
            header.entry_type(),
            header.mode().unwrap(),
            header.uid().unwrap(),
            header.gid().unwrap(),
            link,
            xattrs,
        ));
    }
    out
}

// ── Image and archive builders ──────────────────────────────────────────────

/// One image inside an archive.
#[derive(Clone)]
pub struct Image {
    pub platform: OciPlatform,
    /// Uncompressed layer tars, bottom first.
    pub layers: Vec<Vec<u8>>,
    /// Names for `RepoTags` / `ref.name`.
    pub names: Vec<String>,
    pub user: Option<String>,
    pub home: Option<String>,
    pub workdir: Option<String>,
    /// OCI layouts: compress layers with gzip (else plain).
    pub gzip_layers: bool,
    /// OCI layouts: media type override for every layer (format tests).
    pub layer_media_type: Option<String>,
}

impl Image {
    pub fn new(platform: OciPlatform, layers: Vec<Vec<u8>>, name: &str) -> Self {
        Self {
            platform,
            layers,
            names: vec![name.to_string()],
            user: Some("1000:1000".into()),
            home: Some("/home/agent".into()),
            workdir: Some("/workspace".into()),
            gzip_layers: true,
            layer_media_type: None,
        }
    }

    pub fn config(&self) -> Vec<u8> {
        let mut env = vec!["PATH=/usr/local/bin:/usr/bin:/bin".to_string()];
        if let Some(home) = &self.home {
            env.push(format!("HOME={home}"));
        }
        env.push("PRIVATE_VALUE=not-for-cache-metadata".into());
        serde_json::to_vec(&serde_json::json!({
            "created": "2026-01-01T00:00:00Z",
            "os": self.platform.os,
            "architecture": self.platform.architecture,
            "variant": self.platform.variant,
            "config": {
                "User": self.user,
                "Env": env,
                "WorkingDir": self.workdir,
                "Entrypoint": ["/bin/sh", "-c"],
                "Cmd": ["echo entrypoint-marker"],
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": self.layers.iter().map(|l| format!("sha256:{}", hex(l))).collect::<Vec<_>>()
            },
            "history": self.layers.iter().map(|_| serde_json::json!({"created_by": "fixture"})).collect::<Vec<_>>()
        }))
        .unwrap()
    }
}

pub struct OuterTar(tar::Builder<Vec<u8>>);

impl OuterTar {
    pub fn new() -> Self {
        Self(tar::Builder::new(Vec::new()))
    }
    pub fn file(&mut self, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        self.0.append_data(&mut header, path, bytes).unwrap();
    }
    pub fn pax_file(&mut self, path: &str, bytes: &[u8]) {
        // Apple's writer emits PAX; same content, ustar-style header.
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        self.0.append_data(&mut header, path, bytes).unwrap();
    }
    pub fn dir(&mut self, path: &str) {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        self.0
            .append_data(&mut header, format!("{path}/"), &[][..])
            .unwrap();
    }
    pub fn finish(self) -> Vec<u8> {
        self.0.into_inner().unwrap()
    }
}

/// What an OCI-layout build produced, for assertions.
pub struct BuiltOci {
    pub bytes: Vec<u8>,
    /// `(image index, manifest hex, config hex)`.
    pub manifests: Vec<(usize, String, String)>,
}

/// Build an OCI image layout tar holding `images`. `attestation` adds an
/// `unknown/unknown` attestation manifest (as buildx does); `apple_shape`
/// writes PAX headers and directory entries the way `container image save`
/// does, with `org.opencontainers.image.ref.name` per image.
pub fn oci_layout(images: &[Image], attestation: bool, apple_shape: bool) -> BuiltOci {
    let mut outer = OuterTar::new();
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut manifests = Vec::new();
    let mut descriptors = Vec::new();
    for (i, image) in images.iter().enumerate() {
        let mut layer_descs = Vec::new();
        for raw in &image.layers {
            let (bytes, media) = if image.gzip_layers {
                (
                    gzip(raw),
                    "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                )
            } else {
                (
                    raw.clone(),
                    "application/vnd.oci.image.layer.v1.tar".to_string(),
                )
            };
            let media = image.layer_media_type.clone().unwrap_or(media);
            let digest = hex(&bytes);
            layer_descs.push(serde_json::json!({
                "mediaType": media, "digest": format!("sha256:{digest}"), "size": bytes.len(),
            }));
            blobs.insert(digest, bytes);
        }
        let config = image.config();
        let config_hex = hex(&config);
        blobs.insert(config_hex.clone(), config.clone());
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{config_hex}"), "size": config.len()},
            "layers": layer_descs,
        }))
        .unwrap();
        let manifest_hex = hex(&manifest);
        blobs.insert(manifest_hex.clone(), manifest.clone());
        let mut annotations = serde_json::Map::new();
        if let Some(name) = image.names.first() {
            annotations.insert(
                "org.opencontainers.image.ref.name".into(),
                serde_json::Value::String(name.clone()),
            );
            annotations.insert(
                "io.containerd.image.name".into(),
                serde_json::Value::String(name.clone()),
            );
        }
        descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_hex}"), "size": manifest.len(),
            "platform": {"os": image.platform.os, "architecture": image.platform.architecture},
            "annotations": annotations,
        }));
        if attestation {
            let att = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.in-toto+json", "digest": format!("sha256:{}", "0".repeat(64)), "size": 2},
                "layers": [{"mediaType": "application/vnd.in-toto+json", "digest": format!("sha256:{}", "1".repeat(64)), "size": 2}],
            }))
            .unwrap();
            let att_hex = hex(&att);
            let att_len = att.len();
            blobs.insert(att_hex.clone(), att);
            descriptors.push(serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{att_hex}"), "size": att_len,
                "platform": {"os": "unknown", "architecture": "unknown"},
                "annotations": {"vnd.docker.reference.digest": format!("sha256:{manifest_hex}"),
                                "vnd.docker.reference.type": "attestation-manifest"},
            }));
        }
        manifests.push((i, manifest_hex, config_hex));
    }
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": descriptors,
    }))
    .unwrap();
    if apple_shape {
        outer.dir("blobs");
        outer.dir("blobs/sha256");
        outer.pax_file("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#);
        for (digest, bytes) in &blobs {
            outer.pax_file(&format!("blobs/sha256/{digest}"), bytes);
        }
        outer.pax_file("index.json", &index);
    } else {
        outer.file("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#);
        for (digest, bytes) in &blobs {
            outer.file(&format!("blobs/sha256/{digest}"), bytes);
        }
        outer.file("index.json", &index);
    }
    BuiltOci {
        bytes: outer.finish(),
        manifests,
    }
}

/// A legacy `docker save` archive (`<id>/layer.tar`, `<config>.json`,
/// `manifest.json`, `repositories`). Layers are uncompressed as Docker
/// writes them.
pub fn docker_save(images: &[Image]) -> Vec<u8> {
    let mut outer = OuterTar::new();
    let mut manifest = Vec::new();
    let mut repositories = serde_json::Map::new();
    for image in images {
        let mut paths = Vec::new();
        for (i, raw) in image.layers.iter().enumerate() {
            let id = hex(&[hex(raw).as_bytes(), &[i as u8]].concat());
            let path = format!("{id}/layer.tar");
            outer.file(&format!("{id}/VERSION"), b"1.0");
            outer.file(&format!("{id}/json"), br#"{"id":"x"}"#);
            outer.file(&path, raw);
            paths.push(path);
        }
        let config = image.config();
        let config_name = format!("{}.json", hex(&config));
        outer.file(&config_name, &config);
        manifest.push(serde_json::json!({
            "Config": config_name, "RepoTags": image.names, "Layers": paths,
        }));
        for name in &image.names {
            if let Some((repo, tag)) = name.rsplit_once(':') {
                repositories
                    .entry(repo.to_string())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .unwrap()
                    .insert(tag.into(), serde_json::Value::String("x".into()));
            }
        }
    }
    outer.file("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    outer.file(
        "repositories",
        &serde_json::to_vec(&serde_json::Value::Object(repositories)).unwrap(),
    );
    outer.finish()
}

/// A Docker 25+ `docker save` archive: `manifest.json` plus an OCI index and
/// `blobs/sha256/*` (config, layers and the real manifest).
pub fn docker_save_modern(images: &[Image]) -> BuiltOci {
    let built = oci_layout(images, false, false);
    // Re-open the layout and add manifest.json entries that point at the
    // blobs by their `blobs/sha256/<hex>` names.
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in tar::Archive::new(&built.bytes[..]).entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        members.push((name, data));
    }
    let mut manifest = Vec::new();
    for (i, image) in images.iter().enumerate() {
        let (_, _, config_hex) = &built.manifests[i];
        let layers: Vec<String> = image
            .layers
            .iter()
            .map(|raw| {
                let bytes = if image.gzip_layers {
                    gzip(raw)
                } else {
                    raw.clone()
                };
                format!("blobs/sha256/{}", hex(&bytes))
            })
            .collect();
        manifest.push(serde_json::json!({
            "Config": format!("blobs/sha256/{config_hex}"),
            "RepoTags": image.names,
            "Layers": layers,
        }));
    }
    let mut outer = OuterTar::new();
    for (name, data) in members {
        outer.file(&name, &data);
    }
    outer.file("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    BuiltOci {
        bytes: outer.finish(),
        manifests: built.manifests,
    }
}

/// The realistic layer set the corpus tests use: an agent image with a
/// non-root account, executable tools, a hardlink, symlinks (including a
/// merged-usr style one), xattrs, a whiteout and an opaque directory.
pub fn realistic_layers() -> Vec<Vec<u8>> {
    let base = layer(&[
        Entry::dir("bin", 0o755),
        Entry::dir("usr", 0o755),
        Entry::dir("usr/bin", 0o755),
        Entry::dir("usr/lib", 0o755),
        Entry::symlink("lib", "usr/lib"),
        Entry::owned("usr/bin/agent", b"#!/bin/sh\necho agent\n", 0o755, 0, 0)
            .with_xattr("user.awman.marker", "base"),
        Entry::hardlink("usr/bin/agent-alias", "usr/bin/agent"),
        Entry::symlink("bin/sh", "/usr/bin/agent"),
        Entry::dir("etc", 0o755),
        Entry::owned(
            "etc/passwd",
            b"root:x:0:0::/root:/bin/sh\nagent:x:1000:1000::/home/agent:/bin/sh\n",
            0o644,
            0,
            0,
        ),
        Entry::owned(
            "etc/group",
            b"root:x:0:\nagent:x:1000:\ndev:x:2000:agent\n",
            0o644,
            0,
            0,
        ),
        Entry::owned("etc/old.conf", b"old", 0o600, 0, 0),
        Entry::dir("opt", 0o755),
        Entry::dir("opt/stale", 0o755),
        Entry::owned("opt/stale/file", b"stale", 0o644, 0, 0),
        Entry::dir("home", 0o755),
        Entry::Dir {
            path: "home/agent".into(),
            mode: 0o700,
            uid: 1000,
            gid: 1000,
        },
        Entry::owned("home/agent/.profile", b"export X=1\n", 0o600, 1000, 1000),
    ]);
    let upper = layer(&[
        Entry::whiteout("etc/old.conf"),
        Entry::opaque("opt/stale"),
        Entry::dir("opt/stale", 0o755),
        Entry::owned("opt/stale/fresh", b"fresh", 0o644, 0, 0),
        Entry::owned("usr/lib/libx.so", b"\x7fELF", 0o755, 0, 0),
        Entry::owned("home/agent/.config", b"", 0o600, 1000, 1000)
            .with_xattr("user.awman.marker", "upper"),
    ]);
    vec![base, upper]
}

// ── Docker Engine double over a Unix socket ────────────────────────────────

/// How the fake Engine answers one request.
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// Send only this many body bytes, then close (disconnect mid-transfer).
    pub truncate_to: Option<usize>,
    /// Send this many bytes, then hold the connection open for `stall`.
    pub stall_after: Option<(usize, Duration)>,
    /// Close without sending any response at all.
    pub drop_connection: bool,
}

impl Reply {
    pub fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            body,
            truncate_to: None,
            stall_after: None,
            drop_connection: false,
        }
    }
    pub fn status(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            truncate_to: None,
            stall_after: None,
            drop_connection: false,
        }
    }
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self::status(status, &serde_json::to_vec(&value).unwrap())
    }
    pub fn truncated(body: Vec<u8>, keep: usize) -> Self {
        Self {
            truncate_to: Some(keep),
            ..Self::ok(body)
        }
    }
    pub fn stalled(body: Vec<u8>, after: usize, stall: Duration) -> Self {
        Self {
            stall_after: Some((after, stall)),
            ..Self::ok(body)
        }
    }
    pub fn dropped() -> Self {
        Self {
            drop_connection: true,
            ..Self::ok(Vec::new())
        }
    }
}

/// `(request number, path) -> reply`. The request number counts every
/// request the daemon ever saw, so a script can fail the first export and
/// succeed on the second.
pub type Handler = dyn Fn(usize, &str) -> Reply + Send + Sync;

pub struct FakeEngine {
    pub socket: PathBuf,
    pub seen: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl FakeEngine {
    /// Start a minimal HTTP/1.1 Engine on a Unix socket in `dir`.
    pub fn start(dir: &Path, handler: Arc<Handler>) -> Self {
        let socket = dir.join("docker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (log, count, halt) = (seen.clone(), counter.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let handler = handler.clone();
                let (log, count) = (log.clone(), count.clone());
                std::thread::spawn(move || serve_connection(stream, handler, log, count));
            }
        });
        Self {
            socket,
            seen,
            counter,
            stop,
        }
    }

    pub fn host(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    pub fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn exports(&self) -> usize {
        self.requests()
            .iter()
            .filter(|p| p.contains("/get"))
            .count()
    }

    /// Stop accepting connections and remove the socket: the daemon is gone.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket);
        // Wake the accept loop once so it observes the flag.
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
    }
}

fn serve_connection<S>(
    stream: S,
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<String>>>,
    count: Arc<AtomicUsize>,
) where
    S: Read + Write + Send + TryCloneStream + 'static,
{
    let mut reader = BufReader::new(stream.try_clone_stream());
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
        let number = count.fetch_add(1, Ordering::SeqCst) + 1;
        log.lock().unwrap().push(path.clone());
        let reply = handler(number, &path);
        if reply.drop_connection {
            return;
        }
        let head = format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            reply.status,
            reply.body.len()
        );
        if out.write_all(head.as_bytes()).is_err() {
            return;
        }
        if let Some(n) = reply.truncate_to {
            let _ = out.write_all(&reply.body[..n.min(reply.body.len())]);
            let _ = out.flush();
            return;
        }
        if let Some((n, stall)) = reply.stall_after {
            let _ = out.write_all(&reply.body[..n.min(reply.body.len())]);
            let _ = out.flush();
            std::thread::sleep(stall);
            let _ = out.write_all(&reply.body[n.min(reply.body.len())..]);
            let _ = out.flush();
            return;
        }
        if out.write_all(&reply.body).is_err() {
            return;
        }
        let _ = out.flush();
    }
}

pub trait TryCloneStream {
    fn try_clone_stream(&self) -> Self;
}

impl TryCloneStream for std::os::unix::net::UnixStream {
    fn try_clone_stream(&self) -> Self {
        self.try_clone().unwrap()
    }
}

impl TryCloneStream for std::net::TcpStream {
    fn try_clone_stream(&self) -> Self {
        self.try_clone().unwrap()
    }
}

/// The standard Engine script: ping, version `api`, inspect answering
/// `os/arch`, export answering `archive`. `export_reply` overrides the
/// export answer per request number.
pub fn engine_script(
    api: &'static str,
    os_arch: (&'static str, &'static str),
    reference: &'static str,
    archive: Vec<u8>,
    export_reply: impl Fn(usize, Vec<u8>) -> Reply + Send + Sync + 'static,
) -> Arc<Handler> {
    Arc::new(move |number: usize, path: &str| {
        let p = path.split('?').next().unwrap_or("");
        if p == "/_ping" {
            return Reply::status(200, b"OK");
        }
        if p == "/version" {
            return Reply::json(200, serde_json::json!({"ApiVersion": api}));
        }
        if p.ends_with(&format!("/images/{reference}/json")) {
            return Reply::json(
                200,
                serde_json::json!({"Os": os_arch.0, "Architecture": os_arch.1, "Size": archive.len()}),
            );
        }
        if p.ends_with(&format!("/images/{reference}/get")) {
            return export_reply(number, archive.clone());
        }
        Reply::json(404, serde_json::json!({"message": "No such image"}))
    })
}

// ── TLS material ────────────────────────────────────────────────────────────

pub struct TlsMaterial {
    pub dir: tempfile::TempDir,
    pub ca_pem: PathBuf,
    pub server_cert_pem: Vec<u8>,
    pub server_key_pem: Vec<u8>,
    /// A client certificate signed by the same CA, for mutual TLS.
    pub client_cert_pem: PathBuf,
    pub client_key_pem: PathBuf,
    /// A CA that signed nothing the server presents.
    pub other_ca_pem: PathBuf,
}

/// Disposable CA, server leaf for `server_names` (and 127.0.0.1) and a
/// client leaf. `expired` backdates the server leaf so it is invalid now.
pub fn tls_material(server_names: &[&str], expired: bool) -> TlsMaterial {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    let dir = tempfile::tempdir().unwrap();
    let ca = |name: &str| {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
    };
    let issuer = ca("awman test CA");
    let other = ca("awman other CA");

    let mut names: Vec<String> = server_names.iter().map(|s| s.to_string()).collect();
    names.push("127.0.0.1".into());
    let mut server = CertificateParams::new(names).unwrap();
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    if expired {
        server.not_before = rcgen::date_time_ymd(2020, 1, 1);
        server.not_after = rcgen::date_time_ymd(2021, 1, 1);
    }
    let server_key = KeyPair::generate().unwrap();
    let server_cert = server.signed_by(&server_key, &issuer).unwrap();

    let mut client = CertificateParams::new(vec!["awman-client".to_string()]).unwrap();
    client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_key = KeyPair::generate().unwrap();
    let client_cert = client.signed_by(&client_key, &issuer).unwrap();

    let write = |name: &str, bytes: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let ca_pem = write("ca.pem", &issuer.pem());
    let other_ca_pem = write("other-ca.pem", &other.pem());
    let client_cert_pem = write("client-cert.pem", &client_cert.pem());
    let client_key_pem = write("client-key.pem", &client_key.serialize_pem());
    TlsMaterial {
        server_cert_pem: server_cert.pem().into_bytes(),
        server_key_pem: server_key.serialize_pem().into_bytes(),
        ca_pem,
        client_cert_pem,
        client_key_pem,
        other_ca_pem,
        dir,
    }
}

// ── Loopback HTTPS services (axum on a private runtime thread) ─────────────

/// A running loopback service and the runtime that drives it. Dropping it
/// stops the service.
pub struct Service {
    pub addr: SocketAddr,
    pub seen: Seen,
    handle: axum_server::Handle<std::net::SocketAddr>,
    _runtime: tokio::runtime::Runtime,
}

impl Service {
    pub fn requests(&self) -> Vec<(String, BTreeMap<String, String>)> {
        self.seen.lock().unwrap().clone()
    }
    pub fn stop(&self) {
        self.handle.shutdown();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

/// Every request's path and lower-cased headers, recorded by middleware.
pub type Seen = Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>;

fn record_requests(app: axum::Router, seen: Seen) -> axum::Router {
    use axum::{extract::Request, middleware::Next};
    app.layer(axum::middleware::from_fn(
        move |req: Request, next: Next| {
            let seen = seen.clone();
            async move {
                let path = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let headers = req
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_ascii_lowercase(),
                            v.to_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect();
                seen.lock().unwrap().push((path, headers));
                next.run(req).await
            }
        },
    ))
}

/// Serve `app` over HTTPS with `tls` on a loopback port.
pub fn serve_https(app: axum::Router, tls: &TlsMaterial) -> Service {
    serve(
        app,
        Some((tls.server_cert_pem.clone(), tls.server_key_pem.clone())),
        0,
    )
}

/// Serve `app` over HTTPS on a specific loopback port (for services whose
/// own URL must be known before they start, such as a token realm).
pub fn serve_https_port(app: axum::Router, tls: &TlsMaterial, port: u16) -> Service {
    serve(
        app,
        Some((tls.server_cert_pem.clone(), tls.server_key_pem.clone())),
        port,
    )
}

/// A free loopback port, released again so a service can bind it.
pub fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

/// Serve `app` over plain HTTP on a loopback port.
pub fn serve_http(app: axum::Router) -> Service {
    serve(app, None, 0)
}

fn serve(app: axum::Router, tls: Option<(Vec<u8>, Vec<u8>)>, port: u16) -> Service {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let app = record_requests(app, seen.clone());
    let handle: axum_server::Handle<std::net::SocketAddr> = axum_server::Handle::new();
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let h = handle.clone();
    runtime.spawn(async move {
        match tls {
            Some((cert, key)) => {
                // The builtin SDK enables ring alongside axum's aws-lc. Pick
                // this fixture's provider explicitly, without changing a
                // process-global default or relying on feature unification.
                use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
                let certificates = CertificateDer::pem_slice_iter(&cert)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let key = PrivateKeyDer::from_pem_slice(&key).unwrap();
                let server = rustls::ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certificates, key)
                .unwrap();
                let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server));
                axum_server::from_tcp_rustls(listener, config)
                    .unwrap()
                    .handle(h)
                    .serve(app.into_make_service())
                    .await
                    .unwrap();
            }
            None => {
                axum_server::from_tcp(listener)
                    .unwrap()
                    .handle(h)
                    .serve(app.into_make_service())
                    .await
                    .unwrap();
            }
        }
    });
    Service {
        addr,
        seen,
        handle,
        _runtime: runtime,
    }
}

/// A Docker Engine double as an axum app (the same script as the Unix one,
/// minus stalls/truncation).
pub fn engine_app(
    api: &'static str,
    os_arch: (&'static str, &'static str),
    reference: &'static str,
    archive: Vec<u8>,
) -> axum::Router {
    use axum::{
        extract::Path as AxPath, http::StatusCode, response::IntoResponse, routing::get, Router,
    };
    let archive = Arc::new(archive);
    let size = archive.len();
    let inspect = move |AxPath((_, name)): AxPath<(String, String)>| async move {
        if name == reference {
            (
                StatusCode::OK,
                serde_json::json!({"Os": os_arch.0, "Architecture": os_arch.1, "Size": size})
                    .to_string(),
            )
        } else {
            (
                StatusCode::NOT_FOUND,
                serde_json::json!({"message": "No such image"}).to_string(),
            )
        }
    };
    let export = move |AxPath((_, name)): AxPath<(String, String)>| {
        let archive = archive.clone();
        async move {
            if name == reference {
                (StatusCode::OK, archive.as_ref().clone()).into_response()
            } else {
                (StatusCode::NOT_FOUND, "no such image").into_response()
            }
        }
    };
    Router::new()
        .route("/_ping", get(|| async { "OK" }))
        .route(
            "/version",
            get(move || async move { serde_json::json!({"ApiVersion": api}).to_string() }),
        )
        .route("/{version}/images/{name}/json", get(inspect))
        .route("/{version}/images/{name}/get", get(export))
}

/// A registry double as an axum app. Unauthenticated requests get a Bearer
/// challenge with `realm`; the token endpoint accepts `user:password` Basic
/// auth and issues `token`. `Serves` the OCI layout `built` image `image`.
pub struct RegistryImage {
    pub index: Vec<u8>,
    pub manifest: Vec<u8>,
    pub manifest_hex: String,
    pub config: Vec<u8>,
    pub config_hex: String,
    pub layers: Vec<(String, Vec<u8>)>,
}

pub fn registry_image(image: &Image) -> RegistryImage {
    let mut layer_descs = Vec::new();
    let mut layers = Vec::new();
    for raw in &image.layers {
        let bytes = gzip(raw);
        let digest = hex(&bytes);
        layer_descs.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{digest}"), "size": bytes.len(),
        }));
        layers.push((digest, bytes));
    }
    let config = image.config();
    let config_hex = hex(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": format!("sha256:{config_hex}"), "size": config.len()},
        "layers": layer_descs,
    }))
    .unwrap();
    let manifest_hex = hex(&manifest);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json",
             "digest": format!("sha256:{}", "f".repeat(64)), "size": 1,
             "platform": {"os": "linux", "architecture": "s390x"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json",
             "digest": format!("sha256:{manifest_hex}"), "size": manifest.len(),
             "platform": {"os": image.platform.os, "architecture": image.platform.architecture}},
        ],
    }))
    .unwrap();
    RegistryImage {
        index,
        manifest,
        manifest_hex,
        config,
        config_hex,
        layers,
    }
}

pub struct RegistryAuth {
    /// `realm` sent in the challenge (absolute URL).
    pub realm: String,
    pub username: String,
    pub password: String,
    pub token: String,
}

pub fn registry_app(
    repo: &'static str,
    image: RegistryImage,
    auth: Option<RegistryAuth>,
) -> axum::Router {
    use axum::{
        extract::{Path as AxPath, Request},
        http::{header, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    let image = Arc::new(image);
    let auth = Arc::new(auth);
    let base = format!("/v2/{repo}");

    let guard = {
        let auth = auth.clone();
        move |req: &Request| -> Option<Response> {
            let Some(auth) = auth.as_ref() else {
                return None;
            };
            let got = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if got == format!("Bearer {}", auth.token) {
                return None;
            }
            Some(
                (
                    StatusCode::UNAUTHORIZED,
                    [(
                        header::WWW_AUTHENTICATE,
                        format!(r#"Bearer realm="{}",service="awman-test""#, auth.realm),
                    )],
                    "unauthorized",
                )
                    .into_response(),
            )
        }
    };
    let token = {
        let auth = auth.clone();
        move |req: Request| {
            let auth = auth.clone();
            async move {
                let Some(auth) = auth.as_ref() else {
                    return (StatusCode::NOT_FOUND, "no token endpoint".to_string())
                        .into_response();
                };
                let expected = format!(
                    "Basic {}",
                    base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        format!("{}:{}", auth.username, auth.password)
                    )
                );
                let got = req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if got == expected {
                    (
                        StatusCode::OK,
                        serde_json::json!({"token": auth.token}).to_string(),
                    )
                        .into_response()
                } else {
                    (StatusCode::UNAUTHORIZED, "bad credentials".to_string()).into_response()
                }
            }
        }
    };
    let manifests = {
        let image = image.clone();
        let guard = guard.clone();
        move |AxPath(reference): AxPath<String>, req: Request| {
            let image = image.clone();
            let guard = guard.clone();
            async move {
                if let Some(challenge) = guard(&req) {
                    return challenge;
                }
                let (body, media) = if reference == format!("sha256:{}", image.manifest_hex) {
                    (
                        image.manifest.clone(),
                        "application/vnd.oci.image.manifest.v1+json",
                    )
                } else if reference.starts_with("sha256:") {
                    return (StatusCode::NOT_FOUND, "no such manifest").into_response();
                } else {
                    (
                        image.index.clone(),
                        "application/vnd.oci.image.index.v1+json",
                    )
                };
                (StatusCode::OK, [(header::CONTENT_TYPE, media)], body).into_response()
            }
        }
    };
    let blobs = {
        let image = image.clone();
        move |AxPath(digest): AxPath<String>, req: Request| {
            let image = image.clone();
            let guard = guard.clone();
            async move {
                if let Some(challenge) = guard(&req) {
                    return challenge;
                }
                let hex = digest.trim_start_matches("sha256:");
                if hex == image.config_hex {
                    return (StatusCode::OK, image.config.clone()).into_response();
                }
                if let Some((_, bytes)) = image.layers.iter().find(|(h, _)| h == hex) {
                    return (StatusCode::OK, bytes.clone()).into_response();
                }
                (StatusCode::NOT_FOUND, "no such blob").into_response()
            }
        }
    };
    Router::new()
        .route("/token", get(token))
        .route(&format!("{base}/manifests/{{reference}}"), get(manifests))
        .route(&format!("{base}/blobs/{{digest}}"), get(blobs))
}

// ── HTTP CONNECT proxy ──────────────────────────────────────────────────────

/// `CONNECT` targets seen, plus the Proxy-Authorization header if any.
pub type Tunnels = Arc<Mutex<Vec<(String, Option<String>)>>>;

pub struct Proxy {
    pub addr: SocketAddr,
    pub seen: Tunnels,
    _runtime: tokio::runtime::Runtime,
}

impl Proxy {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn url_with_credentials(&self, user: &str, password: &str) -> String {
        format!("http://{user}:{password}@{}", self.addr)
    }
    pub fn connects(&self) -> Vec<(String, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

/// A loopback HTTP CONNECT proxy. With `require_auth`, tunnels are refused
/// (407) unless a `Proxy-Authorization` header is present.
pub fn connect_proxy(require_auth: bool) -> Proxy {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TBufReader};
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Tunnels = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = TBufReader::new(read_half);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let mut auth = None;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    if let Some(v) = h
                        .strip_prefix("Proxy-Authorization:")
                        .or_else(|| h.strip_prefix("proxy-authorization:"))
                    {
                        auth = Some(v.trim().to_string());
                    }
                }
                log.lock().unwrap().push((target.clone(), auth.clone()));
                if method != "CONNECT" {
                    let _ = write_half
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                if require_auth && auth.is_none() {
                    let _ = write_half
                        .write_all(
                            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    return;
                }
                let Ok(mut upstream) = tokio::net::TcpStream::connect(&target).await else {
                    let _ = write_half
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };
                if write_half
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let mut client_read = reader.into_inner();
                let (mut up_read, mut up_write) = upstream.split();
                let a = tokio::io::copy(&mut client_read, &mut up_write);
                let b = tokio::io::copy(&mut up_read, &mut write_half);
                let _ = tokio::join!(a, b);
            });
        }
    });
    Proxy {
        addr,
        seen,
        _runtime: runtime,
    }
}
