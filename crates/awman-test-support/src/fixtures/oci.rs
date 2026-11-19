//! OCI images built in memory with deterministic digests: layers (tar with
//! fixed mtime/owner/order, optionally gzip), config, manifest and index,
//! written as an OCI layout directory or a tar archive of it.

use super::sha256_hex;
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// One content-addressed blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Media type.
    pub media_type: String,
    /// `sha256:<hex>`.
    pub digest: String,
    /// Contents.
    pub bytes: Vec<u8>,
}

impl Blob {
    fn new(media_type: &str, bytes: Vec<u8>) -> Blob {
        Blob {
            media_type: media_type.to_string(),
            digest: format!("sha256:{}", sha256_hex(&bytes)),
            bytes,
        }
    }

    fn descriptor(&self) -> Descriptor {
        Descriptor {
            media_type: self.media_type.clone(),
            digest: self.digest.clone(),
            size: self.bytes.len() as u64,
            annotations: None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    media_type: &'static str,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    schema_version: u32,
    media_type: &'static str,
    manifests: Vec<Descriptor>,
}

#[derive(Serialize)]
struct Config {
    architecture: String,
    os: String,
    config: RunConfig,
    rootfs: RootFs,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RunConfig {
    env: Vec<String>,
    cmd: Vec<String>,
    working_dir: String,
}

#[derive(Serialize)]
struct RootFs {
    #[serde(rename = "type")]
    kind: &'static str,
    diff_ids: Vec<String>,
}

/// A built image.
#[derive(Debug, Clone)]
pub struct OciImage {
    /// Layer blobs, bottom first.
    pub layers: Vec<Blob>,
    /// The config blob.
    pub config: Blob,
    /// The manifest blob.
    pub manifest: Blob,
    /// `index.json` bytes (references the manifest, tagged `ref_name`).
    pub index: Vec<u8>,
}

impl OciImage {
    /// Read one image from a verified fixture archive without extracting it.
    /// Descriptor digests and sizes are checked before any layer is reused.
    pub fn read_archive(path: &Path) -> Self {
        let mut files = std::collections::BTreeMap::new();
        for entry in tar::Archive::new(std::fs::File::open(path).expect("fixture archive"))
            .entries()
            .expect("archive entries")
        {
            let mut entry = entry.expect("archive entry");
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = entry
                .path()
                .expect("archive path")
                .to_string_lossy()
                .trim_start_matches("./")
                .to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).expect("archive body");
            assert!(
                files.insert(name, bytes).is_none(),
                "duplicate archive member"
            );
        }
        let index = files["index.json"].clone();
        let parsed: serde_json::Value = serde_json::from_slice(&index).expect("fixture index");
        assert_eq!(
            parsed["manifests"].as_array().unwrap().len(),
            1,
            "fixture must select one image"
        );
        let blob = |descriptor: &serde_json::Value| {
            let digest = descriptor["digest"].as_str().expect("descriptor digest");
            let hex = digest.strip_prefix("sha256:").expect("sha256 fixture blob");
            let blob = Blob::new(
                descriptor["mediaType"].as_str().expect("media type"),
                files[&format!("blobs/sha256/{hex}")].clone(),
            );
            assert_eq!(blob.digest, digest);
            assert_eq!(
                blob.bytes.len() as u64,
                descriptor["size"].as_u64().unwrap()
            );
            blob
        };
        let manifest = blob(&parsed["manifests"][0]);
        let parsed: serde_json::Value =
            serde_json::from_slice(&manifest.bytes).expect("fixture manifest");
        Self {
            config: blob(&parsed["config"]),
            layers: parsed["layers"]
                .as_array()
                .unwrap()
                .iter()
                .map(blob)
                .collect(),
            manifest,
            index,
        }
    }

    /// Derive a private fixture by appending regular files while retaining its
    /// architecture, image user, environment and original layer identities.
    pub fn with_files(mut self, files: &[(&str, &[u8], u32)]) -> Self {
        let layer = Blob::new(LAYER_TAR, tar_bytes(files.iter().copied()));
        let mut config: serde_json::Value = serde_json::from_slice(&self.config.bytes).unwrap();
        config["rootfs"]["diff_ids"]
            .as_array_mut()
            .unwrap()
            .push(layer.digest.clone().into());
        if let Some(history) = config
            .get_mut("history")
            .and_then(serde_json::Value::as_array_mut)
        {
            history.push(serde_json::json!({"created_by":"awman synthetic guest probe"}));
        }
        self.config = Blob::new(CONFIG_TYPE, serde_json::to_vec(&config).unwrap());
        self.layers.push(layer);
        let mut manifest: serde_json::Value = serde_json::from_slice(&self.manifest.bytes).unwrap();
        manifest["config"] = serde_json::to_value(self.config.descriptor()).unwrap();
        manifest["layers"] =
            serde_json::to_value(self.layers.iter().map(Blob::descriptor).collect::<Vec<_>>())
                .unwrap();
        self.manifest = Blob::new(MANIFEST_TYPE, serde_json::to_vec(&manifest).unwrap());
        let mut index: serde_json::Value = serde_json::from_slice(&self.index).unwrap();
        index["manifests"][0]["digest"] = self.manifest.digest.clone().into();
        index["manifests"][0]["size"] = self.manifest.bytes.len().into();
        self.index = serde_json::to_vec(&index).unwrap();
        self
    }

    /// Write an OCI image layout directory; returns `dir`.
    pub fn write_layout(&self, dir: &Path) -> PathBuf {
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).expect("create layout");
        std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)
            .expect("oci-layout");
        std::fs::write(dir.join("index.json"), &self.index).expect("index.json");
        for blob in self.blobs() {
            let hex = blob.digest.trim_start_matches("sha256:");
            std::fs::write(blobs.join(hex), &blob.bytes).expect("blob");
        }
        dir.to_path_buf()
    }

    /// Write the layout as a deterministic tar archive; returns `path`.
    pub fn write_archive(&self, path: &Path) -> PathBuf {
        let mut entries: Vec<(String, Vec<u8>)> = vec![
            (
                "oci-layout".into(),
                br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
            ),
            ("index.json".into(), self.index.clone()),
        ];
        for blob in self.blobs() {
            let hex = blob.digest.trim_start_matches("sha256:").to_string();
            entries.push((format!("blobs/sha256/{hex}"), blob.bytes.clone()));
        }
        entries.sort();
        entries.dedup();
        let bytes = tar_bytes(
            entries
                .iter()
                .map(|(p, b)| (p.as_str(), b.as_slice(), 0o644)),
        );
        std::fs::write(path, bytes).expect("write archive");
        path.to_path_buf()
    }

    /// Every blob (layers, config, manifest).
    pub fn blobs(&self) -> Vec<&Blob> {
        self.layers
            .iter()
            .chain([&self.config, &self.manifest])
            .collect()
    }
}

/// Builds an [`OciImage`].
///
/// ```
/// use awman_test_support::fixtures::OciImageBuilder;
/// let a = OciImageBuilder::new().layer(&[("etc/hello", b"hi")]).build();
/// let b = OciImageBuilder::new().layer(&[("etc/hello", b"hi")]).build();
/// assert_eq!(a.manifest.digest, b.manifest.digest);
/// ```
#[derive(Debug, Clone)]
pub struct OciImageBuilder {
    layers: Vec<Vec<(String, Vec<u8>, u32)>>,
    env: Vec<String>,
    cmd: Vec<String>,
    working_dir: String,
    architecture: String,
    gzip: bool,
    ref_name: String,
}

impl Default for OciImageBuilder {
    fn default() -> Self {
        OciImageBuilder::new()
    }
}

impl OciImageBuilder {
    /// A linux image for the host architecture with no layers.
    pub fn new() -> OciImageBuilder {
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        OciImageBuilder {
            layers: Vec::new(),
            env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
            cmd: vec!["/bin/sh".into()],
            working_dir: "/".into(),
            architecture: architecture.into(),
            gzip: false,
            ref_name: "latest".into(),
        }
    }

    /// A layer of regular files (mode 0644).
    pub fn layer(mut self, files: &[(&str, &[u8])]) -> Self {
        self.layers.push(
            files
                .iter()
                .map(|(p, b)| (p.to_string(), b.to_vec(), 0o644))
                .collect(),
        );
        self
    }

    /// A layer with explicit modes (e.g. 0o755 executables).
    pub fn layer_with_modes(mut self, files: &[(&str, &[u8], u32)]) -> Self {
        self.layers.push(
            files
                .iter()
                .map(|(p, b, m)| (p.to_string(), b.to_vec(), *m))
                .collect(),
        );
        self
    }

    /// An `ENV` entry `KEY=VALUE`.
    pub fn env(mut self, entry: &str) -> Self {
        self.env.push(entry.into());
        self
    }

    /// The default command.
    pub fn cmd(mut self, cmd: &[&str]) -> Self {
        self.cmd = cmd.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Image architecture (`amd64`, `arm64`).
    pub fn architecture(mut self, arch: &str) -> Self {
        self.architecture = arch.into();
        self
    }

    /// Compress layers with gzip (deterministic header).
    pub fn gzip(mut self, on: bool) -> Self {
        self.gzip = on;
        self
    }

    /// `org.opencontainers.image.ref.name` in the index (default `latest`).
    pub fn ref_name(mut self, name: &str) -> Self {
        self.ref_name = name.into();
        self
    }

    /// Build the blobs.
    pub fn build(&self) -> OciImage {
        let mut layers = Vec::new();
        let mut diff_ids = Vec::new();
        for files in &self.layers {
            let mut files = files.clone();
            files.sort();
            let tar = tar_bytes(files.iter().map(|(p, b, m)| (p.as_str(), b.as_slice(), *m)));
            diff_ids.push(format!("sha256:{}", sha256_hex(&tar)));
            if self.gzip {
                let mut gz = flate2::GzBuilder::new()
                    .mtime(0)
                    .write(Vec::new(), flate2::Compression::default());
                gz.write_all(&tar).expect("gzip");
                layers.push(Blob::new(LAYER_GZIP, gz.finish().expect("gzip")));
            } else {
                layers.push(Blob::new(LAYER_TAR, tar));
            }
        }
        let config = Config {
            architecture: self.architecture.clone(),
            os: "linux".into(),
            config: RunConfig {
                env: self.env.clone(),
                cmd: self.cmd.clone(),
                working_dir: self.working_dir.clone(),
            },
            rootfs: RootFs {
                kind: "layers",
                diff_ids,
            },
        };
        let config = Blob::new(CONFIG_TYPE, serde_json::to_vec(&config).expect("config"));
        let manifest = Manifest {
            schema_version: 2,
            media_type: MANIFEST_TYPE,
            config: config.descriptor(),
            layers: layers.iter().map(Blob::descriptor).collect(),
        };
        let manifest = Blob::new(
            MANIFEST_TYPE,
            serde_json::to_vec(&manifest).expect("manifest"),
        );
        let mut descriptor = manifest.descriptor();
        descriptor.annotations = Some(
            [(
                "org.opencontainers.image.ref.name".to_string(),
                self.ref_name.clone(),
            )]
            .into_iter()
            .collect(),
        );
        let index = Index {
            schema_version: 2,
            media_type: INDEX_TYPE,
            manifests: vec![descriptor],
        };
        OciImage {
            layers,
            config,
            manifest,
            index: serde_json::to_vec(&index).expect("index"),
        }
    }
}

/// A tar stream with fixed metadata, entries in the given order.
fn tar_bytes<'a>(files: impl Iterator<Item = (&'a str, &'a [u8], u32)>) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.mode(tar::HeaderMode::Deterministic);
    for (path, bytes, mode) in files {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, path, bytes)
            .expect("tar entry");
    }
    builder.into_inner().expect("tar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_deterministic_and_order_independent() {
        let a = OciImageBuilder::new()
            .layer(&[("b", b"2"), ("a", b"1")])
            .gzip(true)
            .build();
        let b = OciImageBuilder::new()
            .layer(&[("a", b"1"), ("b", b"2")])
            .gzip(true)
            .build();
        assert_eq!(a.manifest.digest, b.manifest.digest);
        assert_eq!(a.layers[0].media_type, LAYER_GZIP);
    }

    #[test]
    fn manifest_references_config_and_layers() {
        let image = OciImageBuilder::new().layer(&[("x", b"y")]).build();
        let manifest: serde_json::Value = serde_json::from_slice(&image.manifest.bytes).unwrap();
        assert_eq!(manifest["config"]["digest"], image.config.digest.as_str());
        assert_eq!(
            manifest["layers"][0]["digest"],
            image.layers[0].digest.as_str()
        );
        let config: serde_json::Value = serde_json::from_slice(&image.config.bytes).unwrap();
        assert_eq!(config["rootfs"]["diff_ids"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn layout_and_archive_contain_every_blob() {
        let dir = tempfile::tempdir().unwrap();
        let image = OciImageBuilder::new().layer(&[("x", b"y")]).build();
        let layout = image.write_layout(&dir.path().join("layout"));
        for blob in image.blobs() {
            let hex = blob.digest.trim_start_matches("sha256:");
            assert_eq!(
                std::fs::read(layout.join("blobs/sha256").join(hex)).unwrap(),
                blob.bytes
            );
        }
        let a = std::fs::read(image.write_archive(&dir.path().join("a.tar"))).unwrap();
        let b = std::fs::read(image.write_archive(&dir.path().join("b.tar"))).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn derived_archive_preserves_base_metadata_and_adds_verified_content() {
        let dir = tempfile::tempdir().unwrap();
        let base = OciImageBuilder::new()
            .architecture("arm64")
            .env("HOME=/home/probe")
            .layer(&[("base", b"base")])
            .build();
        let path = base.write_archive(&dir.path().join("base.tar"));
        let derived = OciImage::read_archive(&path).with_files(&[(
            "usr/bin/probe",
            b"#!/bin/sh\necho 2.0.0\n",
            0o755,
        )]);
        assert_eq!(derived.layers[0], base.layers[0]);
        let config: serde_json::Value = serde_json::from_slice(&derived.config.bytes).unwrap();
        let base_config: serde_json::Value = serde_json::from_slice(&base.config.bytes).unwrap();
        assert_eq!(config["architecture"], "arm64");
        assert_eq!(config["config"], base_config["config"]);
        assert_eq!(config["rootfs"]["diff_ids"].as_array().unwrap().len(), 2);
        let reloaded =
            OciImage::read_archive(&derived.write_archive(&dir.path().join("derived.tar")));
        assert_eq!(reloaded.manifest, derived.manifest);
        let mut archive = tar::Archive::new(reloaded.layers[1].bytes.as_slice());
        let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str(), Some("usr/bin/probe"));
        assert_eq!(entry.header().mode().unwrap(), 0o755);
        let mut contents = String::new();
        entry.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "#!/bin/sh\necho 2.0.0\n");
    }
}
