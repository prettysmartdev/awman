//! Image archive validation: OCI image layout and `docker save` tars.
//!
//! Every source ends here. A registry pull is written out as an OCI layout,
//! a Docker Engine export is a `docker save` tar, and a user's archive is
//! whatever they exported — all three are validated by the same code before
//! anything reaches the cache.
//!
//! Validation is two streaming passes over the staged file:
//!
//! 1. **Outer pass.** Every archive entry is checked (relative path, no `..`,
//!    no duplicates, only files, directories and contained symlinks), hashed
//!    and sized. Small JSON documents are kept in memory. Every
//!    `blobs/sha256/<hex>` entry must hash to its name.
//! 2. **Layer pass.** Each selected layer is decompressed (plain tar or
//!    gzip), capped, hashed against the config's `rootfs.diff_ids`, and every
//!    entry inside it is checked: no `..` escape, no writes through a symlink
//!    created in this or an earlier layer, hardlinks only to earlier entries,
//!    no device nodes, well-formed whiteouts.
//!
//! Cached archives retain their original bytes. The SDK import projection
//! contains only the selected image: config and layer blobs remain verbatim,
//! preserving layer order, whiteouts, ownership, modes and xattrs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::error::EngineError;
use crate::engine::oci::verify::{
    digest_from_hex, digest_hex, is_limit_exceeded, sha256_digest, HashingReader,
};
use crate::engine::oci::{AcquireLimits, AcquireProgress, ArchiveFormat};

/// Largest JSON document (index, manifest, config) kept in memory.
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Total bytes of JSON documents kept in memory for one archive.
const MAX_JSON_TOTAL: u64 = 64 * 1024 * 1024;
/// Entries in the outer archive.
const MAX_OUTER_ENTRIES: usize = 100_000;
/// Entries in one layer.
const MAX_LAYER_ENTRIES: usize = 2_000_000;
/// Nested image indexes followed before giving up.
const MAX_INDEX_DEPTH: usize = 4;

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// What validation learned about the selected image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedArchive {
    pub format: ArchiveFormat,
    /// The selected manifest's digest. For a legacy `docker save` archive
    /// with no matching OCI index this hashes a synthetic OCI manifest of
    /// the config and ordered layer content digests and sizes.
    pub manifest_digest: Digest,
    pub config_digest: Digest,
    /// The platform the image config declares.
    pub platform: OciPlatform,
    pub config: ImageConfigSummary,
    /// Layer digests (uncompressed `diff_id`s), bottom first.
    pub diff_ids: Vec<Digest>,
}

/// The non-secret image defaults the runtime needs before first boot.
/// Image `Env` is deliberately not kept (it may carry baked-in secrets);
/// only `HOME` is extracted from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfigSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Validate the archive at `path` and select the image for `platform`.
///
/// `wanted_refs` disambiguates archives that hold several images (a name
/// from `docker save`'s `RepoTags` or an OCI `ref.name` annotation); it
/// never selects an image of the wrong platform.
pub fn validate_archive(
    path: &Path,
    platform: &OciPlatform,
    wanted_refs: &[String],
    limits: &AcquireLimits,
    progress: &mut dyn FnMut(AcquireProgress),
) -> Result<ValidatedArchive, EngineError> {
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let size = std::fs::metadata(path)
        .map_err(|e| EngineError::io(path, e))?
        .len();
    if size > limits.max_archive_bytes {
        return Err(reject(format!(
            "archive is {size} bytes; the limit is {}",
            limits.max_archive_bytes
        )));
    }

    let outer = scan_outer(path, limits)?;
    let selected = if outer.entries.contains_key("manifest.json") {
        select_docker_save(path, &outer, platform, wanted_refs)?
    } else if outer.entries.contains_key("oci-layout") && outer.entries.contains_key("index.json") {
        check_oci_layout_marker(path, &outer)?;
        select_oci_layout(path, &outer, platform, wanted_refs)?
    } else {
        return Err(reject(
            "neither a docker-save archive (manifest.json) nor an OCI image layout \
             (oci-layout + index.json)"
                .into(),
        ));
    };

    if selected.layers.len() > limits.max_layers {
        return Err(reject(format!(
            "image has {} layers; the limit is {}",
            selected.layers.len(),
            limits.max_layers
        )));
    }
    if selected.layers.len() != selected.diff_ids.len() {
        return Err(reject(format!(
            "image config lists {} diff_ids for {} layers",
            selected.diff_ids.len(),
            selected.layers.len()
        )));
    }

    scan_layers(path, &outer, &selected, limits)?;
    progress(AcquireProgress::Verifying {
        digest: selected.manifest_digest.clone(),
    });

    Ok(ValidatedArchive {
        format: selected.format,
        manifest_digest: selected.manifest_digest,
        config_digest: selected.config_digest,
        platform: selected.platform,
        config: selected.config,
        diff_ids: selected.diff_ids,
    })
}

/// Project an acquired archive onto exactly the validated image before SDK
/// import. Microsandbox loads *all* archive images and tags the first; passing
/// the original multi-image archive would cross the validation boundary.
/// Only outer metadata is rewritten; config and layer contents are unchanged.
pub fn prepare_runtime_archive(
    acquired: &crate::engine::oci::AcquiredImage,
    tag: &str,
    limits: &AcquireLimits,
) -> Result<(tempfile::TempDir, std::path::PathBuf), EngineError> {
    let path = &acquired.archive;
    let refs = [tag.to_string(), acquired.identity.reference.clone()];
    let validated = validate_archive(
        path,
        &acquired.identity.platform,
        &refs,
        limits,
        &mut |_| {},
    )?;
    if validated.manifest_digest != acquired.identity.manifest_digest
        || validated.config_digest != acquired.identity.config_digest
    {
        return Err(EngineError::ImageArchiveRejected {
            path: path.clone(),
            reason: "runtime image selection differs from acquired identity".into(),
        });
    }
    let outer = scan_outer(path, limits)?;
    let selected = match validated.format {
        ArchiveFormat::DockerSave => select_docker_save(path, &outer, &validated.platform, &refs)?,
        ArchiveFormat::OciLayout => select_oci_layout(path, &outer, &validated.platform, &refs)?,
    };
    let dir = tempfile::Builder::new()
        .prefix("runtime-")
        .tempdir_in(
            path.parent()
                .ok_or_else(|| EngineError::Config("archive has no parent".into()))?,
        )
        .map_err(|e| EngineError::io(path, e))?;
    let output = dir.path().join("selected.tar");
    let mut sink = crate::engine::oci::verify::StagingSink::create(
        output.clone(),
        limits.max_archive_bytes,
        limits.min_free_bytes,
        &crate::engine::oci::verify::HostDiskSpace,
    )?;
    let write_result = (|| -> Result<(), EngineError> {
        let mut builder = tar::Builder::new(&mut sink);
        let io_error = |e| EngineError::io(&output, e);
        let mut copies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut include = |name: &str, destination: &str| -> Result<(), EngineError> {
            let (real, _, _) =
                outer
                    .resolve_file(name)
                    .ok_or_else(|| EngineError::ImageArchiveRejected {
                        path: path.clone(),
                        reason: "selected archive member disappeared".into(),
                    })?;
            copies
                .entry(real.to_string())
                .or_default()
                .insert(destination.to_string());
            Ok(())
        };
        include(&selected.config_entry, &selected.config_entry)?;
        for layer in &selected.layers {
            include(&layer.entry, &layer.entry)?;
        }
        let metadata = match selected.format {
            ArchiveFormat::DockerSave => {
                let manifest = serde_json::json!([{
                    "Config": selected.config_entry, "RepoTags": [tag],
                    "Layers": selected.layers.iter().map(|layer| &layer.entry).collect::<Vec<_>>()
                }]);
                vec![("manifest.json", manifest.to_string().into_bytes())]
            }
            ArchiveFormat::OciLayout => {
                let manifest = blob_name(&selected.manifest_digest);
                include(&manifest, &manifest)?;
                let index = serde_json::json!({
                    "schemaVersion": 2, "mediaType": OCI_INDEX,
                    "manifests": [{ "mediaType": OCI_MANIFEST,
                        "digest": selected.manifest_digest, "size": outer.resolve_file(&manifest).unwrap().1,
                        "platform": selected.platform,
                        "annotations": { "org.opencontainers.image.ref.name": tag }
                    }]
                });
                vec![
                    ("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec()),
                    ("index.json", index.to_string().into_bytes()),
                ]
            }
        };
        for (name, bytes) in metadata {
            append_runtime_member(&mut builder, name, bytes.len() as u64, &bytes[..])
                .map_err(&io_error)?;
        }
        // Seek back for aliases of a legacy docker-save layer. No blob-sized
        // allocation and no extraction through archive paths or symlinks.
        for entry in open_archive(path)?.entries().map_err(&io_error)? {
            let mut entry = entry.map_err(&io_error)?;
            let Some(name) = normalize_outer_name(&entry.path_bytes()).map_err(|reason| {
                EngineError::ImageArchiveRejected {
                    path: path.clone(),
                    reason,
                }
            })?
            else {
                continue;
            };
            if let Some(destinations) = copies.remove(&name) {
                let offset = entry.raw_file_position();
                let size = entry.size();
                for destination in destinations {
                    use std::io::{Seek, SeekFrom};
                    let mut input = File::open(path).map_err(&io_error)?;
                    input.seek(SeekFrom::Start(offset)).map_err(&io_error)?;
                    append_runtime_member(&mut builder, &destination, size, input.take(size))
                        .map_err(&io_error)?;
                }
                std::io::copy(&mut entry, &mut std::io::sink()).map_err(&io_error)?;
            }
        }
        if !copies.is_empty() {
            return Err(EngineError::ImageArchiveRejected {
                path: path.clone(),
                reason: "selected archive members are missing".into(),
            });
        }
        builder.finish().map_err(&io_error)?;
        Ok(())
    })();
    if let Some(error) = sink.take_failure() {
        return Err(error);
    }
    write_result?;
    sink.finish()?;
    Ok((dir, output))
}

fn append_runtime_member<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    size: u64,
    reader: impl Read,
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o600);
    header.set_cksum();
    builder.append_data(&mut header, name, reader)
}

// ── Outer pass ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum OuterKind {
    File {
        size: u64,
        sha256: String,
    },
    Dir,
    /// A symlink, already resolved to the in-archive path it names.
    Symlink {
        target: String,
    },
}

struct OuterScan {
    entries: BTreeMap<String, OuterKind>,
    /// Small JSON-ish documents, by entry name.
    docs: HashMap<String, Vec<u8>>,
}

impl OuterScan {
    /// The regular-file entry `name` names, following at most one contained
    /// symlink (legacy `docker save` links duplicate layers).
    fn resolve_file<'a>(&'a self, name: &'a str) -> Option<(&'a str, u64, &'a str)> {
        match self.entries.get(name)? {
            OuterKind::File { size, sha256 } => Some((name, *size, sha256)),
            OuterKind::Symlink { target } => match self.entries.get(target.as_str())? {
                OuterKind::File { size, sha256 } => Some((target.as_str(), *size, sha256)),
                _ => None,
            },
            OuterKind::Dir => None,
        }
    }

    fn doc(&self, name: &str) -> Option<&[u8]> {
        let (real, _, _) = self.resolve_file(name)?;
        self.docs.get(real).map(Vec::as_slice)
    }
}

fn open_archive(path: &Path) -> Result<tar::Archive<BufReader<File>>, EngineError> {
    let file = File::open(path).map_err(|e| EngineError::io(path, e))?;
    Ok(tar::Archive::new(BufReader::new(file)))
}

fn scan_outer(path: &Path, limits: &AcquireLimits) -> Result<OuterScan, EngineError> {
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let mut archive = open_archive(path)?;
    let mut entries = BTreeMap::new();
    let mut docs = HashMap::new();
    let mut doc_total = 0u64;
    let iter = archive
        .entries()
        .map_err(|e| reject(format!("unreadable tar: {e}")))?;
    for entry in iter {
        let mut entry = entry.map_err(|e| reject(format!("malformed or truncated tar: {e}")))?;
        if entries.len() >= MAX_OUTER_ENTRIES {
            return Err(reject(format!(
                "more than {MAX_OUTER_ENTRIES} archive entries"
            )));
        }
        let raw = entry.path_bytes().into_owned();
        let name = normalize_outer_name(&raw).map_err(reject)?;
        let Some(name) = name else { continue };
        if entries.contains_key(&name) {
            // Duplicate names are resolved "last wins" by some readers and
            // "first wins" by others; refuse the ambiguity.
            return Err(reject(format!("duplicate archive entry `{name}`")));
        }
        let kind = match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let declared = entry
                    .header()
                    .size()
                    .map_err(|e| reject(format!("entry `{name}` has a bad size: {e}")))?;
                if declared > limits.max_layer_bytes {
                    return Err(reject(format!(
                        "entry `{name}` is {declared} bytes; the per-blob limit is {}",
                        limits.max_layer_bytes
                    )));
                }
                let mut hashing = HashingReader::new(&mut entry, declared);
                let mut kept = Vec::new();
                let mut keep = is_document_candidate(&name, declared)
                    && doc_total + declared <= MAX_JSON_TOTAL;
                if keep {
                    // Only JSON documents are kept: a small layer blob must not
                    // use up the budget a config or manifest needs.
                    let mut first = [0u8; 1];
                    let n = hashing
                        .read(&mut first)
                        .map_err(|e| reject(format!("truncated entry `{name}`: {e}")))?;
                    kept.extend_from_slice(&first[..n]);
                    keep = n == 1 && matches!(first[0], b'{' | b'[');
                }
                if keep {
                    hashing
                        .read_to_end(&mut kept)
                        .map_err(|e| reject(format!("truncated entry `{name}`: {e}")))?;
                    doc_total += kept.len() as u64;
                }
                let (sha256, size) = hashing
                    .finish()
                    .map_err(|e| reject(format!("truncated entry `{name}`: {e}")))?;
                if size != declared {
                    return Err(reject(format!(
                        "truncated entry `{name}`: {size} of {declared} bytes"
                    )));
                }
                if keep {
                    docs.insert(name.clone(), kept);
                }
                OuterKind::File { size, sha256 }
            }
            tar::EntryType::Directory => OuterKind::Dir,
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name_bytes()
                    .ok_or_else(|| reject(format!("symlink `{name}` has no target")))?
                    .into_owned();
                let target = resolve_outer_symlink(&name, &link).ok_or_else(|| {
                    reject(format!(
                        "symlink `{name}` points outside the archive: `{}`",
                        String::from_utf8_lossy(&link)
                    ))
                })?;
                OuterKind::Symlink { target }
            }
            other => {
                return Err(reject(format!(
                    "archive entry `{name}` has unsupported type {other:?} (hardlinks, \
                     devices and FIFOs are not allowed at the archive level)"
                )))
            }
        };
        entries.insert(name, kind);
    }

    // Contained symlinks must name a regular file that actually exists.
    for (name, kind) in &entries {
        if let OuterKind::Symlink { target } = kind {
            if !matches!(entries.get(target), Some(OuterKind::File { .. })) {
                return Err(reject(format!(
                    "symlink `{name}` does not point at a file in the archive"
                )));
            }
        }
    }

    // Content-addressed blobs must hash to their names.
    for (name, kind) in &entries {
        if let Some(rest) = name.strip_prefix("blobs/") {
            let OuterKind::File { sha256, .. } = kind else {
                if matches!(kind, OuterKind::Dir) {
                    continue;
                }
                return Err(reject(format!("blob `{name}` is not a regular file")));
            };
            let Some(hex) = rest.strip_prefix("sha256/") else {
                return Err(reject(format!(
                    "blob `{name}` uses an unsupported digest algorithm"
                )));
            };
            let claimed = Digest::parse(&format!("sha256:{hex}"))
                .map_err(|e| reject(format!("blob `{name}`: {e}")))?;
            if digest_hex(&claimed) != sha256 {
                return Err(EngineError::ImageDigestMismatch {
                    reference: format!("{}:{name}", path.display()),
                    expected: claimed.to_string(),
                    actual: format!("sha256:{sha256}"),
                });
            }
        }
    }
    Ok(OuterScan { entries, docs })
}

/// Normalise an outer entry name: strip `./`, refuse absolute paths, `..`
/// and non-UTF-8. `Ok(None)` for the archive root itself.
fn normalize_outer_name(raw: &[u8]) -> Result<Option<String>, String> {
    let name = std::str::from_utf8(raw)
        .map_err(|_| format!("entry name is not UTF-8: {}", String::from_utf8_lossy(raw)))?;
    if name.starts_with('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("entry `{name}` is not a relative archive path"));
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("entry `{name}` escapes the archive with `..`")),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("/")))
}

/// Resolve a symlink target relative to the link's directory; `None` when it
/// is absolute or climbs out of the archive.
fn resolve_outer_symlink(name: &str, link: &[u8]) -> Option<String> {
    let link = std::str::from_utf8(link).ok()?;
    if link.starts_with('/') || link.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = name.split('/').collect();
    parts.pop();
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn is_document_candidate(name: &str, size: u64) -> bool {
    if size > MAX_JSON_BYTES {
        return false;
    }
    matches!(
        name,
        "manifest.json" | "index.json" | "oci-layout" | "repositories"
    ) || name.ends_with(".json")
        || name.starts_with("blobs/")
}

fn check_oci_layout_marker(path: &Path, outer: &OuterScan) -> Result<(), EngineError> {
    #[derive(Deserialize)]
    struct Layout {
        #[serde(rename = "imageLayoutVersion")]
        version: String,
    }
    let bytes = outer.doc("oci-layout").unwrap_or_default();
    let layout: Layout =
        serde_json::from_slice(bytes).map_err(|e| EngineError::ImageArchiveRejected {
            path: path.to_path_buf(),
            reason: format!("oci-layout is not valid: {e}"),
        })?;
    if !layout.version.starts_with("1.") {
        return Err(EngineError::ImageArchiveRejected {
            path: path.to_path_buf(),
            reason: format!("unsupported OCI layout version {}", layout.version),
        });
    }
    Ok(())
}

// ── Image selection ────────────────────────────────────────────────────────

/// A layer to scan: which archive entry holds it and how it is compressed.
#[derive(Debug, Clone)]
struct LayerRef {
    entry: String,
    /// Expected size of the (possibly compressed) blob, when known.
    size: Option<u64>,
}

struct Selected {
    format: ArchiveFormat,
    manifest_digest: Digest,
    config_digest: Digest,
    config_entry: String,
    platform: OciPlatform,
    config: ImageConfigSummary,
    layers: Vec<LayerRef>,
    diff_ids: Vec<Digest>,
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<DescriptorPlatform>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct DescriptorPlatform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IndexDoc {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ManifestDoc {
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    os: String,
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    config: Option<ContainerConfig>,
    rootfs: RootFs,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerConfig {
    #[serde(rename = "User", default)]
    user: Option<String>,
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RootFs {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    diff_ids: Vec<String>,
}

fn parse_config(
    path: &Path,
    bytes: &[u8],
) -> Result<(ConfigDoc, OciPlatform, ImageConfigSummary, Vec<Digest>), EngineError> {
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let config: ConfigDoc =
        serde_json::from_slice(bytes).map_err(|e| reject(format!("image config: {e}")))?;
    if config.rootfs.kind != "layers" {
        return Err(reject(format!(
            "image config rootfs type `{}` is not `layers`",
            config.rootfs.kind
        )));
    }
    if config.os.is_empty() || config.architecture.is_empty() {
        return Err(reject(
            "image config does not declare its os and architecture".into(),
        ));
    }
    let diff_ids = config
        .rootfs
        .diff_ids
        .iter()
        .map(|d| Digest::parse(d).map_err(|e| reject(format!("diff_id: {e}"))))
        .collect::<Result<Vec<_>, _>>()?;
    let platform = OciPlatform {
        os: config.os.clone(),
        architecture: config.architecture.clone(),
        variant: config.variant.clone().filter(|v| !v.is_empty()),
    };
    let container = config.config.as_ref();
    let summary = ImageConfigSummary {
        user: container
            .and_then(|c| c.user.clone())
            .filter(|u| !u.is_empty()),
        home: container
            .and_then(|c| c.env.as_ref())
            .and_then(|env| env.iter().rev().find_map(|kv| kv.strip_prefix("HOME=")))
            .filter(|h| !h.is_empty())
            .map(str::to_string),
        working_dir: container
            .and_then(|c| c.working_dir.clone())
            .filter(|w| !w.is_empty()),
    };
    Ok((config, platform, summary, diff_ids))
}

fn select_docker_save(
    path: &Path,
    outer: &OuterScan,
    wanted: &OciPlatform,
    wanted_refs: &[String],
) -> Result<Selected, EngineError> {
    #[derive(Debug, Deserialize, Serialize)]
    struct Entry {
        #[serde(rename = "Config")]
        config: String,
        #[serde(rename = "RepoTags", default)]
        repo_tags: Option<Vec<String>>,
        #[serde(rename = "Layers")]
        layers: Vec<String>,
    }
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let manifest: Vec<Entry> =
        serde_json::from_slice(outer.doc("manifest.json").unwrap_or_default())
            .map_err(|e| reject(format!("manifest.json: {e}")))?;
    if manifest.is_empty() {
        return Err(reject("manifest.json lists no images".into()));
    }

    let mut candidates = Vec::new();
    let mut found_platforms = Vec::new();
    for entry in &manifest {
        let config_name = clean_ref_path(path, &entry.config)?;
        let (_, config_size, sha) = outer
            .resolve_file(&config_name)
            .ok_or_else(|| reject(format!("config `{}` is missing", entry.config)))?;
        let config_digest = digest_from_hex(sha);
        let bytes = outer
            .doc(&config_name)
            .ok_or_else(|| reject(format!("config `{}` is too large", entry.config)))?;
        // A config named by its digest (`<hex>.json` or `blobs/sha256/<hex>`)
        // must hash to that name.
        let stem = config_name
            .rsplit('/')
            .next()
            .unwrap_or(&config_name)
            .trim_end_matches(".json");
        if stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()) && stem != sha {
            return Err(EngineError::ImageDigestMismatch {
                reference: format!("{}:{}", path.display(), entry.config),
                expected: format!("sha256:{stem}"),
                actual: config_digest.to_string(),
            });
        }
        let (_, platform, summary, diff_ids) = parse_config(path, bytes)?;
        found_platforms.push(platform.to_string());
        if !wanted.matches(&platform) {
            continue;
        }
        let layers = entry
            .layers
            .iter()
            .map(|l| {
                let name = clean_ref_path(path, l)?;
                outer
                    .resolve_file(&name)
                    .ok_or_else(|| reject(format!("layer `{l}` is missing")))?;
                Ok(LayerRef {
                    entry: name,
                    size: None,
                })
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        // Without an OCI index the archive has no manifest digest. Synthesise
        // one from a canonical OCI manifest over the actual config and layer
        // bytes (their hashes and sizes), so two archives share an identity
        // only when their content is the same.
        let layer_descs = layers
            .iter()
            .map(|l| {
                let (_, size, sha) = outer
                    .resolve_file(&l.entry)
                    .expect("layers were resolved above");
                serde_json::json!({
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": format!("sha256:{sha}"),
                    "size": size,
                })
            })
            .collect::<Vec<_>>();
        let canonical = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest.as_str(),
                "size": config_size,
            },
            "layers": layer_descs,
        }))
        .map_err(|e| reject(e.to_string()))?;
        candidates.push((
            entry.repo_tags.clone().unwrap_or_default(),
            Selected {
                format: ArchiveFormat::DockerSave,
                manifest_digest: sha256_digest(&canonical),
                config_digest,
                config_entry: config_name,
                platform,
                config: summary,
                layers,
                diff_ids,
            },
        ));
    }
    let mut selected = pick_one(path, candidates, wanted, wanted_refs, &found_platforms)?;

    // Docker 25+ exports carry an OCI index next to manifest.json; when it
    // holds exactly one manifest for this config *and* these ordered layer
    // blobs, report that real manifest digest.
    if outer.entries.contains_key("index.json") {
        if let Some(digest) = find_manifest_for(outer, &selected.config_digest, &selected.layers) {
            selected.manifest_digest = digest;
        }
    }
    Ok(selected)
}

/// `docker save` paths are relative to the archive root.
fn clean_ref_path(path: &Path, raw: &str) -> Result<String, EngineError> {
    normalize_outer_name(raw.as_bytes())
        .and_then(|n| n.ok_or_else(|| format!("empty path `{raw}`")))
        .map_err(|reason| EngineError::ImageArchiveRejected {
            path: path.to_path_buf(),
            reason,
        })
}

fn find_manifest_for(outer: &OuterScan, config: &Digest, layers: &[LayerRef]) -> Option<Digest> {
    let real = |name: &str| outer.resolve_file(name).map(|(r, _, _)| r.to_string());
    let wanted: Option<Vec<String>> = layers.iter().map(|l| real(&l.entry)).collect();
    let wanted = wanted?;
    let mut found: Option<Digest> = None;
    let index: IndexDoc = serde_json::from_slice(outer.doc("index.json")?).ok()?;
    let mut queue: Vec<(Descriptor, usize)> = index.manifests.into_iter().map(|d| (d, 0)).collect();
    while let Some((desc, depth)) = queue.pop() {
        // Indexes routinely reference other platforms whose blobs were not
        // exported; skip what is absent rather than give up.
        let Ok(digest) = Digest::parse(&desc.digest) else {
            continue;
        };
        let Some(bytes) = outer.doc(&blob_name(&digest)) else {
            continue;
        };
        if is_index_type(&desc.media_type) && depth < MAX_INDEX_DEPTH {
            let Ok(nested) = serde_json::from_slice::<IndexDoc>(bytes) else {
                continue;
            };
            queue.extend(nested.manifests.into_iter().map(|d| (d, depth + 1)));
        } else if let Ok(manifest) = serde_json::from_slice::<ManifestDoc>(bytes) {
            if manifest.config.digest != config.as_str() {
                continue;
            }
            let same_layers = manifest.layers.len() == wanted.len()
                && manifest.layers.iter().zip(&wanted).all(|(d, w)| {
                    Digest::parse(&d.digest)
                        .ok()
                        .and_then(|d| real(&blob_name(&d)))
                        .is_some_and(|r| r == *w)
                });
            if !same_layers {
                continue;
            }
            match &found {
                // Two different manifests claim the same content: ambiguous.
                Some(other) if *other != digest => return None,
                _ => found = Some(digest),
            }
        }
    }
    found
}

fn blob_name(digest: &Digest) -> String {
    format!("blobs/sha256/{}", digest_hex(digest))
}

fn is_index_type(media_type: &str) -> bool {
    media_type == OCI_INDEX || media_type == DOCKER_LIST
}

fn select_oci_layout(
    path: &Path,
    outer: &OuterScan,
    wanted: &OciPlatform,
    wanted_refs: &[String],
) -> Result<Selected, EngineError> {
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let index: IndexDoc = serde_json::from_slice(outer.doc("index.json").unwrap_or_default())
        .map_err(|e| reject(format!("index.json: {e}")))?;
    if let Some(mt) = &index.media_type {
        if !mt.is_empty() && mt != OCI_INDEX {
            return Err(reject(format!("index.json has media type `{mt}`")));
        }
    }

    // Walk the index tree, carrying the nearest `ref.name`-style annotation.
    let mut queue: Vec<(Descriptor, usize, Vec<String>)> = index
        .manifests
        .into_iter()
        .map(|d| {
            let names = ref_names(&d.annotations);
            (d, 0, names)
        })
        .collect();
    let mut candidates = Vec::new();
    let mut found_platforms = Vec::new();
    while let Some((desc, depth, names)) = queue.pop() {
        let digest =
            Digest::parse(&desc.digest).map_err(|e| reject(format!("index descriptor: {e}")))?;
        let blob = blob_name(&digest);
        let (_, size, _) = outer
            .resolve_file(&blob)
            .ok_or_else(|| reject(format!("manifest blob {digest} is missing")))?;
        if desc.size != 0 && desc.size != size {
            return Err(reject(format!(
                "manifest blob {digest} is {size} bytes; its descriptor says {}",
                desc.size
            )));
        }
        let bytes = outer
            .doc(&blob)
            .ok_or_else(|| reject(format!("manifest blob {digest} is too large")))?;
        if is_index_type(&desc.media_type) {
            if depth >= MAX_INDEX_DEPTH {
                return Err(reject("image indexes are nested too deeply".into()));
            }
            let nested: IndexDoc = serde_json::from_slice(bytes)
                .map_err(|e| reject(format!("nested index {digest}: {e}")))?;
            for d in nested.manifests {
                let mut n = ref_names(&d.annotations);
                n.extend(names.iter().cloned());
                queue.push((d, depth + 1, n));
            }
            continue;
        }
        if !desc.media_type.is_empty()
            && desc.media_type != OCI_MANIFEST
            && desc.media_type != DOCKER_MANIFEST
        {
            // Attestations and other artifacts share the index; skip them.
            continue;
        }
        // An attestation manifest carries platform unknown/unknown.
        if let Some(p) = &desc.platform {
            if p.os == "unknown" {
                continue;
            }
            let declared = OciPlatform {
                os: p.os.clone(),
                architecture: p.architecture.clone(),
                variant: p.variant.clone(),
            };
            if !wanted.matches(&declared) {
                found_platforms.push(declared.to_string());
                continue;
            }
        }
        let manifest: ManifestDoc =
            serde_json::from_slice(bytes).map_err(|e| reject(format!("manifest {digest}: {e}")))?;
        if let Some(mt) = &manifest.media_type {
            if mt != OCI_MANIFEST && mt != DOCKER_MANIFEST {
                return Err(reject(format!("manifest {digest} has media type `{mt}`")));
            }
        }
        let config_digest = Digest::parse(&manifest.config.digest)
            .map_err(|e| reject(format!("config descriptor: {e}")))?;
        let config_blob = blob_name(&config_digest);
        let (_, config_size, _) = outer
            .resolve_file(&config_blob)
            .ok_or_else(|| reject(format!("config blob {config_digest} is missing")))?;
        if manifest.config.size != 0 && manifest.config.size != config_size {
            return Err(reject(format!(
                "config blob {config_digest} size does not match its descriptor"
            )));
        }
        let config_bytes = outer
            .doc(&config_blob)
            .ok_or_else(|| reject(format!("config blob {config_digest} is too large")))?;
        let (_, platform, summary, diff_ids) = parse_config(path, config_bytes)?;
        found_platforms.push(platform.to_string());
        if !wanted.matches(&platform) {
            continue;
        }
        let layers = manifest
            .layers
            .iter()
            .map(|l| {
                let d = Digest::parse(&l.digest)
                    .map_err(|e| reject(format!("layer descriptor: {e}")))?;
                check_layer_media_type(path, &l.media_type)?;
                let entry = blob_name(&d);
                let (_, size, _) = outer.resolve_file(&entry).ok_or_else(|| {
                    reject(format!(
                        "layer blob {d} is missing (non-distributable layers are not supported)"
                    ))
                })?;
                if l.size != size {
                    return Err(reject(format!(
                        "layer blob {d} is {size} bytes; its descriptor says {}",
                        l.size
                    )));
                }
                Ok(LayerRef {
                    entry,
                    size: Some(size),
                })
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        candidates.push((
            names,
            Selected {
                format: ArchiveFormat::OciLayout,
                manifest_digest: digest,
                config_digest,
                config_entry: config_blob,
                platform,
                config: summary,
                layers,
                diff_ids,
            },
        ));
    }
    pick_one(path, candidates, wanted, wanted_refs, &found_platforms)
}

fn ref_names(annotations: &BTreeMap<String, String>) -> Vec<String> {
    [
        "io.containerd.image.name",
        "org.opencontainers.image.ref.name",
    ]
    .iter()
    .filter_map(|k| annotations.get(*k).cloned())
    .collect()
}

fn check_layer_media_type(path: &Path, media_type: &str) -> Result<(), EngineError> {
    const OK: &[&str] = &[
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.diff.tar.gzip",
        "application/vnd.docker.image.rootfs.diff.tar",
    ];
    if OK.contains(&media_type) {
        return Ok(());
    }
    let reason = if media_type.contains("zstd") {
        format!(
            "layer media type `{media_type}` (zstd) is not supported; re-export with gzip layers"
        )
    } else if media_type.contains("nondistributable") || media_type.contains("foreign") {
        format!("non-distributable layer `{media_type}` is not supported")
    } else {
        format!("unknown layer media type `{media_type}`")
    };
    Err(EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    })
}

/// Choose exactly one platform-matching image. More than one distinct
/// match is narrowed by `wanted_refs`; if it is still ambiguous the archive
/// is rejected rather than guessed at.
fn pick_one(
    path: &Path,
    candidates: Vec<(Vec<String>, Selected)>,
    wanted: &OciPlatform,
    wanted_refs: &[String],
    found_platforms: &[String],
) -> Result<Selected, EngineError> {
    let mut unique: Vec<(Vec<String>, Selected)> = Vec::new();
    for (names, sel) in candidates {
        if let Some(existing) = unique
            .iter_mut()
            .find(|(_, s)| s.manifest_digest == sel.manifest_digest)
        {
            existing.0.extend(names);
        } else {
            unique.push((names, sel));
        }
    }
    if unique.is_empty() {
        let mut found: Vec<&String> = found_platforms.iter().collect();
        found.sort();
        found.dedup();
        return Err(EngineError::ImagePlatformMismatch {
            reference: path.display().to_string(),
            wanted: wanted.to_string(),
            found: if found.is_empty() {
                "no images".into()
            } else {
                found.into_iter().cloned().collect::<Vec<_>>().join(", ")
            },
        });
    }
    if unique.len() > 1 && !wanted_refs.is_empty() {
        let matching: Vec<usize> = unique
            .iter()
            .enumerate()
            .filter(|(_, (names, _))| names.iter().any(|n| ref_matches(n, wanted_refs)))
            .map(|(i, _)| i)
            .collect();
        if matching.len() == 1 {
            return Ok(unique.swap_remove(matching[0]).1);
        }
    }
    if unique.len() > 1 {
        return Err(EngineError::ImageArchiveRejected {
            path: path.to_path_buf(),
            reason: format!(
                "archive holds {} different images for {wanted}; export only the one to use",
                unique.len()
            ),
        });
    }
    Ok(unique.pop().expect("one candidate").1)
}

/// `docker save` records `name:tag`; Docker Hub names may be written with or
/// without `docker.io/library/`.
fn ref_matches(name: &str, wanted: &[String]) -> bool {
    let strip = |s: &str| {
        s.trim_start_matches("docker.io/")
            .trim_start_matches("library/")
            .to_string()
    };
    wanted.iter().any(|w| strip(w) == strip(name))
}

// ── Layer pass ─────────────────────────────────────────────────────────────

fn scan_layers(
    path: &Path,
    outer: &OuterScan,
    selected: &Selected,
    limits: &AcquireLimits,
) -> Result<(), EngineError> {
    // Which real entries hold layers, and which diff_ids each must produce.
    let mut wanted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (layer, diff_id) in selected.layers.iter().zip(&selected.diff_ids) {
        let (real, size, _) = outer
            .resolve_file(&layer.entry)
            .expect("layers were resolved during selection");
        if let Some(expected) = layer.size {
            debug_assert_eq!(expected, size);
        }
        wanted
            .entry(real.to_string())
            .or_default()
            .insert(digest_hex(diff_id).to_string());
    }

    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason,
    };
    let mut archive = open_archive(path)?;
    let iter = archive
        .entries()
        .map_err(|e| reject(format!("unreadable tar: {e}")))?;
    let mut seen = 0usize;
    let mut summaries: BTreeMap<String, Vec<PathOp>> = BTreeMap::new();
    for entry in iter {
        let entry = entry.map_err(|e| reject(format!("malformed or truncated tar: {e}")))?;
        let raw = entry.path_bytes().into_owned();
        let Ok(Some(name)) = normalize_outer_name(&raw) else {
            continue;
        };
        let Some(diff_ids) = wanted.get(&name) else {
            continue;
        };
        let (actual, ops) = scan_layer(path, &name, entry, limits.max_layer_bytes)?;
        summaries.insert(name.clone(), ops);
        // Several layers may share one blob, but one blob has one diff_id.
        if diff_ids.len() != 1 || !diff_ids.contains(&actual) {
            return Err(EngineError::ImageDigestMismatch {
                reference: format!("{}:{name}", path.display()),
                expected: diff_ids
                    .iter()
                    .map(|d| format!("sha256:{d}"))
                    .collect::<Vec<_>>()
                    .join(" or "),
                actual: format!("sha256:{actual}"),
            });
        }
        seen += 1;
    }
    if seen != wanted.len() {
        return Err(reject("archive ended before every layer was read".into()));
    }
    // Layers stream in archive order; replay them in image order so a
    // symlink made by a lower layer is seen by every upper one.
    let mut graph = SymlinkGraph::default();
    for layer in &selected.layers {
        let (real, _, _) = outer
            .resolve_file(&layer.entry)
            .expect("layers were resolved during selection");
        let ops = summaries.get(real).map(Vec::as_slice).unwrap_or_default();
        graph
            .apply(ops)
            .map_err(|reason| reject(format!("layer `{}`: {reason}", layer.entry)))?;
    }
    Ok(())
}

/// Scan one layer; returns the hex SHA-256 of its uncompressed tar stream and
/// the path operations it performs, for the cross-layer symlink replay.
fn scan_layer(
    path: &Path,
    name: &str,
    mut entry: impl Read,
    max_layer_bytes: u64,
) -> Result<(String, Vec<PathOp>), EngineError> {
    let reject = |reason: String| EngineError::ImageArchiveRejected {
        path: path.to_path_buf(),
        reason: format!("layer `{name}`: {reason}"),
    };
    let mut magic = [0u8; 4];
    let mut got = 0;
    while got < magic.len() {
        match entry.read(&mut magic[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return Err(reject(format!("unreadable: {e}"))),
        }
    }
    let head = std::io::Cursor::new(magic[..got].to_vec());
    let chained = head.chain(entry);
    let decompressed: Box<dyn Read> = if got >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::MultiGzDecoder::new(chained))
    } else if got == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(reject(
            "zstd-compressed layers are not supported; re-export with gzip layers".into(),
        ));
    } else {
        Box::new(chained)
    };
    let hashing = HashingReader::new(decompressed, max_layer_bytes);
    let mut layer = tar::Archive::new(hashing);
    let ops = {
        let iter = layer
            .entries()
            .map_err(|e| reject(format!("not a tar stream: {e}")))?;
        let mut checker = LayerChecker::default();
        for inner in iter {
            let inner = inner.map_err(|e| {
                if is_limit_exceeded(&e) {
                    reject(format!("uncompressed size {e}"))
                } else {
                    reject(format!("malformed or truncated: {e}"))
                }
            })?;
            checker.check(&inner).map_err(reject)?;
        }
        checker.ops
    };
    let (sha, _) = layer.into_inner().finish().map_err(|e| {
        if is_limit_exceeded(&e) {
            reject(format!("uncompressed size {e}"))
        } else {
            reject(format!("truncated: {e}"))
        }
    })?;
    Ok((sha, ops))
}

/// What one layer entry does to the rootfs path graph, recorded for the
/// cross-layer replay in [`SymlinkGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathOp {
    /// Creates (or replaces) a symlink at the path.
    Symlink(String),
    /// Creates (or replaces) a non-symlink entry at the path.
    Plain(String),
    /// `.wh.<name>`: removes the lower-layer path and everything below it.
    Whiteout(String),
    /// `.wh..wh..opq`: hides every lower-layer entry below the directory
    /// (`""` is the root).
    Opaque(String),
}

/// Per-layer entry checks.
#[derive(Default)]
struct LayerChecker {
    count: usize,
    /// Paths created as symlinks in this layer.
    symlinks: BTreeSet<String>,
    /// Paths whose current entry in this layer can be a hardlink target
    /// (regular files, fifos, symlinks — the last rejected separately).
    files: BTreeSet<String>,
    /// This layer's effect on the path graph, in entry order.
    ops: Vec<PathOp>,
}

impl LayerChecker {
    fn check<R: Read>(&mut self, entry: &tar::Entry<'_, R>) -> Result<(), String> {
        self.count += 1;
        if self.count > MAX_LAYER_ENTRIES {
            return Err(format!("more than {MAX_LAYER_ENTRIES} entries"));
        }
        let raw = entry.path_bytes();
        let Some(path) = normalize_layer_path(&raw)? else {
            return Ok(());
        };

        // Writing beneath a symlink this layer just created is the classic
        // extraction escape (`a -> /etc`, then `a/passwd`). Symlinks from
        // lower layers are checked by the image-order replay.
        if let Some(link) = symlink_prefix(&self.symlinks, &path) {
            return Err(format!("`{path}` is written through symlink `{link}`"));
        }

        let (parent, base) = match path.rsplit_once('/') {
            Some((parent, base)) => (parent, base),
            None => ("", path.as_str()),
        };
        let kind = entry.header().entry_type();
        if let Some(rest) = base.strip_prefix(".wh.") {
            if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Continuous) {
                return Err(format!("whiteout `{path}` is not a regular file"));
            }
            if rest == ".wh..opq" {
                self.ops.push(PathOp::Opaque(parent.to_string()));
                return Ok(());
            }
            if rest.is_empty() || rest == "." || rest == ".." || rest.starts_with(".wh.") {
                return Err(format!("malformed whiteout `{path}`"));
            }
            let hidden = if parent.is_empty() {
                rest.to_string()
            } else {
                format!("{parent}/{rest}")
            };
            self.ops.push(PathOp::Whiteout(hidden));
            return Ok(());
        }

        match kind {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                self.symlinks.remove(&path);
                self.files.insert(path.clone());
                self.ops.push(PathOp::Plain(path));
            }
            tar::EntryType::Directory => {
                // A directory replaces whatever was there: it is neither a
                // symlink nor a hardlink target any more.
                self.symlinks.remove(&path);
                self.files.remove(&path);
                self.ops.push(PathOp::Plain(path));
            }
            tar::EntryType::Symlink => {
                if entry.link_name_bytes().is_none() {
                    return Err(format!("symlink `{path}` has no target"));
                }
                self.files.insert(path.clone());
                self.symlinks.insert(path.clone());
                self.ops.push(PathOp::Symlink(path));
            }
            tar::EntryType::Link => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| format!("hardlink `{path}` has no target"))?;
                let target = normalize_layer_path(&target)?
                    .ok_or_else(|| format!("hardlink `{path}` targets the root"))?;
                if !self.files.contains(&target) {
                    return Err(format!(
                        "hardlink `{path}` targets `{target}`, which is not an earlier entry \
                         of this layer (or no longer a file)"
                    ));
                }
                if self.symlinks.contains(&target)
                    || symlink_prefix(&self.symlinks, &target).is_some()
                {
                    return Err(format!("hardlink `{path}` targets symlink `{target}`"));
                }
                self.symlinks.remove(&path);
                self.files.insert(path.clone());
                self.ops.push(PathOp::Plain(path));
            }
            tar::EntryType::Fifo => {
                self.symlinks.remove(&path);
                self.files.insert(path.clone());
                self.ops.push(PathOp::Plain(path));
            }
            tar::EntryType::Char | tar::EntryType::Block => {
                return Err(format!("device node `{path}` is not allowed"));
            }
            other => return Err(format!("`{path}` has unsupported entry type {other:?}")),
        }
        Ok(())
    }
}

/// The nearest proper ancestor of `path` that is in `symlinks`.
fn symlink_prefix<'a>(symlinks: &BTreeSet<String>, path: &'a str) -> Option<&'a str> {
    path.match_indices('/')
        .map(|(i, _)| &path[..i])
        .find(|p| symlinks.contains(*p))
}

/// Remove `path` and everything below it from `set`.
fn remove_tree(set: &mut BTreeSet<String>, path: &str) {
    set.remove(path);
    let below = format!("{path}/");
    let doomed: Vec<String> = set
        .range(below.clone()..)
        .take_while(|p| p.starts_with(&below))
        .cloned()
        .collect();
    for p in doomed {
        set.remove(&p);
    }
}

/// The live symlinks of the rootfs as layers are applied in image order.
/// Any entry written beneath a live symlink — made by this layer or any
/// lower one — is rejected, so no extractor can be steered outside the
/// root however it resolves links.
#[derive(Default)]
struct SymlinkGraph {
    symlinks: BTreeSet<String>,
}

impl SymlinkGraph {
    fn apply(&mut self, ops: &[PathOp]) -> Result<(), String> {
        // Whiteouts act on the lower layers, before this layer's entries.
        for op in ops {
            match op {
                PathOp::Whiteout(p) => remove_tree(&mut self.symlinks, p),
                PathOp::Opaque(dir) if dir.is_empty() => self.symlinks.clear(),
                PathOp::Opaque(dir) => {
                    let keep = self.symlinks.contains(dir);
                    remove_tree(&mut self.symlinks, dir);
                    if keep {
                        self.symlinks.insert(dir.clone());
                    }
                }
                _ => {}
            }
        }
        for op in ops {
            let path = match op {
                PathOp::Symlink(p) | PathOp::Plain(p) | PathOp::Whiteout(p) => p.clone(),
                PathOp::Opaque(dir) if dir.is_empty() => continue,
                PathOp::Opaque(dir) => format!("{dir}/.wh..wh..opq"),
            };
            if let Some(link) = symlink_prefix(&self.symlinks, &path) {
                return Err(format!("`{path}` is written through symlink `{link}`"));
            }
            match op {
                PathOp::Symlink(p) => {
                    self.symlinks.insert(p.clone());
                }
                PathOp::Plain(p) => {
                    self.symlinks.remove(p);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Layer paths are rootfs-relative; a leading `./` names the root. An
/// absolute name (leading `/`) is refused, as is `..` anywhere, rather than
/// being reinterpreted: every extractor must see the same path the
/// validator checked.
fn normalize_layer_path(raw: &[u8]) -> Result<Option<String>, String> {
    let s = String::from_utf8_lossy(raw);
    if s.contains('\0') {
        return Err("entry name contains NUL".into());
    }
    if s.starts_with('/') {
        return Err(format!("entry `{s}` is an absolute path"));
    }
    let mut parts = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("entry `{s}` escapes the root with `..`")),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("/")))
}

/// Stage a user-supplied archive into `sink`, transparently decompressing a
/// gzip-wrapped tar (`docker save | gzip`). The cache always stores a plain
/// tar.
pub(super) fn stage_local_archive(
    source: &Path,
    sink: &mut crate::engine::oci::verify::StagingSink<'_>,
    max_bytes: u64,
) -> Result<(), EngineError> {
    let mut file = File::open(source).map_err(|e| EngineError::io(source, e))?;
    let meta = file.metadata().map_err(|e| EngineError::io(source, e))?;
    if !meta.is_file() {
        return Err(EngineError::ImageArchiveRejected {
            path: source.to_path_buf(),
            reason: "not a regular file".into(),
        });
    }
    let mut magic = [0u8; 2];
    let n = file
        .read(&mut magic)
        .map_err(|e| EngineError::io(source, e))?;
    let head = std::io::Cursor::new(magic[..n].to_vec());
    let chained = head.chain(file);
    let mut reader: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(HashingReader::new(
            flate2::read::MultiGzDecoder::new(chained),
            max_bytes,
        ))
    } else {
        Box::new(chained)
    };
    sink.copy_from(&mut reader).map_err(|e| match e {
        EngineError::Network(msg) => EngineError::ImageArchiveRejected {
            path: source.to_path_buf(),
            reason: msg,
        },
        other => other,
    })?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    //! Unit tests build archives in memory with the `tar` crate. The
    //! corpus-level tests (real `docker save` fixtures, every attack shape)
    //! live in `tests/oci_import/`.
    use super::*;
    use flate2::write::GzEncoder;
    use std::io::Write;

    pub(crate) fn limits() -> AcquireLimits {
        AcquireLimits {
            max_archive_bytes: 64 * 1024 * 1024,
            max_layer_bytes: 16 * 1024 * 1024,
            max_layers: 16,
            min_free_bytes: 0,
        }
    }

    pub(crate) fn arm64() -> OciPlatform {
        OciPlatform {
            os: "linux".into(),
            architecture: "arm64".into(),
            variant: None,
        }
    }

    pub(crate) fn amd64() -> OciPlatform {
        OciPlatform {
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        }
    }

    /// One layer entry for [`layer_tar`].
    pub(crate) enum L<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Char(&'a str),
    }

    /// Build an uncompressed layer tar. Paths are written raw so malicious
    /// names reach the validator unchanged.
    pub(crate) fn layer_tar(entries: &[L<'_>]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for e in entries {
            let mut h = tar::Header::new_gnu();
            let (name, data): (&str, &[u8]) = match e {
                L::File(n, d, mode) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(*mode);
                    (n, d)
                }
                L::Dir(n) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    (n, &[])
                }
                L::Symlink(n, t) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    set_raw_link(&mut h, t);
                    (n, &[])
                }
                L::Hardlink(n, t) => {
                    h.set_entry_type(tar::EntryType::Link);
                    set_raw_link(&mut h, t);
                    (n, &[])
                }
                L::Char(n) => {
                    h.set_entry_type(tar::EntryType::Char);
                    (n, &[])
                }
            };
            set_raw_name(&mut h, name);
            h.set_size(data.len() as u64);
            h.set_uid(1000);
            h.set_gid(1000);
            h.set_cksum();
            b.append(&h, data).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn set_raw_name(h: &mut tar::Header, name: &str) {
        let gnu = h.as_gnu_mut().unwrap();
        gnu.name = [0; 100];
        gnu.name[..name.len()].copy_from_slice(name.as_bytes());
    }

    fn set_raw_link(h: &mut tar::Header, target: &str) {
        let gnu = h.as_gnu_mut().unwrap();
        gnu.linkname = [0; 100];
        gnu.linkname[..target.len()].copy_from_slice(target.as_bytes());
    }

    pub(crate) fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    pub(crate) fn config_json(platform: &OciPlatform, diff_ids: &[String]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "os": platform.os,
            "architecture": platform.architecture,
            "config": {
                "User": "1000:1000",
                "Env": ["PATH=/usr/bin", "HOME=/home/agent", "SECRET=do-not-keep"],
                "WorkingDir": "/workspace",
                "Entrypoint": ["/bin/sh"],
            },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
        }))
        .unwrap()
    }

    /// Outer archive builder.
    pub(crate) struct Outer(tar::Builder<Vec<u8>>);

    impl Outer {
        pub(crate) fn new() -> Self {
            Self(tar::Builder::new(Vec::new()))
        }
        pub(crate) fn file(mut self, name: &str, data: &[u8]) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            set_raw_name(&mut h, name);
            h.set_size(data.len() as u64);
            h.set_cksum();
            self.0.append(&h, data).unwrap();
            self
        }
        pub(crate) fn symlink(mut self, name: &str, target: &str) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            set_raw_name(&mut h, name);
            set_raw_link(&mut h, target);
            h.set_size(0);
            h.set_cksum();
            self.0.append(&h, &[][..]).unwrap();
            self
        }
        pub(crate) fn blob(self, data: &[u8]) -> (Self, String) {
            let hex = crate::engine::oci::verify::sha256_hex(data);
            (self.file(&format!("blobs/sha256/{hex}"), data), hex)
        }
        pub(crate) fn finish(self) -> Vec<u8> {
            self.0.into_inner().unwrap()
        }
    }

    /// A one-image OCI layout for `platform` with gzip layers.
    pub(crate) fn oci_archive(platform: &OciPlatform, layers: &[Vec<u8>]) -> (Vec<u8>, String) {
        let mut outer = Outer::new().file("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#);
        let mut descs = Vec::new();
        let mut diff_ids = Vec::new();
        for l in layers {
            diff_ids.push(format!(
                "sha256:{}",
                crate::engine::oci::verify::sha256_hex(l)
            ));
            let gz = gzip(l);
            let (o, hex) = outer.blob(&gz);
            outer = o;
            descs.push(serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": format!("sha256:{hex}"), "size": gz.len(),
            }));
        }
        let config = config_json(platform, &diff_ids);
        let (o, config_hex) = outer.blob(&config);
        outer = o;
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": OCI_MANIFEST,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{config_hex}"), "size": config.len()},
            "layers": descs,
        }))
        .unwrap();
        let (o, manifest_hex) = outer.blob(&manifest);
        outer = o;
        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2, "mediaType": OCI_INDEX,
            "manifests": [{"mediaType": OCI_MANIFEST, "digest": format!("sha256:{manifest_hex}"),
                           "size": manifest.len(),
                           "annotations": {"org.opencontainers.image.ref.name": "latest"}}],
        }))
        .unwrap();
        (outer.file("index.json", &index).finish(), manifest_hex)
    }

    /// A legacy `docker save` archive with uncompressed layers.
    pub(crate) fn docker_save_archive(platform: &OciPlatform, layers: &[Vec<u8>]) -> Vec<u8> {
        let mut outer = Outer::new();
        let mut paths = Vec::new();
        let mut diff_ids = Vec::new();
        for (i, l) in layers.iter().enumerate() {
            let p = format!("layer{i}/layer.tar");
            outer = outer.file(&p, l);
            paths.push(p);
            diff_ids.push(format!(
                "sha256:{}",
                crate::engine::oci::verify::sha256_hex(l)
            ));
        }
        let config = config_json(platform, &diff_ids);
        let config_name = format!("{}.json", crate::engine::oci::verify::sha256_hex(&config));
        outer = outer.file(&config_name, &config);
        let manifest = serde_json::to_vec(&serde_json::json!([
            {"Config": config_name, "RepoTags": ["awman-x-claude:latest"], "Layers": paths}
        ]))
        .unwrap();
        outer.file("manifest.json", &manifest).finish()
    }

    fn validate_bytes(
        bytes: &[u8],
        platform: &OciPlatform,
    ) -> Result<ValidatedArchive, EngineError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        std::fs::write(&path, bytes).unwrap();
        validate_archive(&path, platform, &[], &limits(), &mut |_| {})
    }

    fn base_layer() -> Vec<u8> {
        layer_tar(&[
            L::Dir("home/agent"),
            L::File("usr/bin/tool", b"#!/bin/sh\n", 0o755),
            L::Symlink("usr/bin/alias", "/usr/bin/tool"),
            L::Hardlink("usr/bin/tool2", "usr/bin/tool"),
        ])
    }

    fn rejected_reason(result: Result<ValidatedArchive, EngineError>) -> String {
        match result {
            Err(EngineError::ImageArchiveRejected { reason, .. }) => reason,
            other => panic!("expected ImageArchiveRejected, got {other:?}"),
        }
    }

    #[test]
    fn oci_layout_is_validated_and_identified() {
        let upper = layer_tar(&[
            L::File("etc/.wh.old", b"", 0o644),
            L::File("opt/.wh..wh..opq", b"", 0o644),
        ]);
        let (bytes, manifest_hex) = oci_archive(&arm64(), &[base_layer(), upper]);
        let v = validate_bytes(&bytes, &arm64()).unwrap();
        assert_eq!(v.format, ArchiveFormat::OciLayout);
        assert_eq!(v.manifest_digest.as_str(), format!("sha256:{manifest_hex}"));
        assert_eq!(v.diff_ids.len(), 2);
        assert_eq!(v.platform, arm64());
        assert_eq!(v.config.user.as_deref(), Some("1000:1000"));
        assert_eq!(v.config.home.as_deref(), Some("/home/agent"));
        assert_eq!(v.config.working_dir.as_deref(), Some("/workspace"));
    }

    #[test]
    fn save_archive_is_validated_and_identified() {
        let bytes = docker_save_archive(&amd64(), &[base_layer()]);
        let v = validate_bytes(&bytes, &amd64()).unwrap();
        assert_eq!(v.format, ArchiveFormat::DockerSave);
        assert_eq!(v.diff_ids.len(), 1);
    }

    #[test]
    fn wrong_platform_is_a_platform_mismatch() {
        let (bytes, _) = oci_archive(&amd64(), &[base_layer()]);
        match validate_bytes(&bytes, &arm64()) {
            Err(EngineError::ImagePlatformMismatch { wanted, found, .. }) => {
                assert_eq!(wanted, "linux/arm64");
                assert_eq!(found, "linux/amd64");
            }
            other => panic!("expected platform mismatch, got {other:?}"),
        }
        let bytes = docker_save_archive(&amd64(), &[base_layer()]);
        assert!(matches!(
            validate_bytes(&bytes, &arm64()),
            Err(EngineError::ImagePlatformMismatch { .. })
        ));
    }

    #[test]
    fn tampered_blob_is_a_digest_mismatch() {
        let (mut bytes, _) = oci_archive(&arm64(), &[base_layer()]);
        // Flip one byte inside the first blob's data (the first file after
        // `oci-layout` begins at offset 1024).
        bytes[1024 + 512 + 20] ^= 0xff;
        assert!(matches!(
            validate_bytes(&bytes, &arm64()),
            Err(EngineError::ImageDigestMismatch { .. } | EngineError::ImageArchiveRejected { .. })
        ));
    }

    #[test]
    fn diff_id_mismatch_is_rejected() {
        let layer = base_layer();
        let mut outer = Outer::new();
        outer = outer.file("l.tar", &layer);
        let config = config_json(&arm64(), &[format!("sha256:{}", "0".repeat(64))]);
        outer = outer.file("c.json", &config);
        let manifest = br#"[{"Config":"c.json","RepoTags":null,"Layers":["l.tar"]}]"#;
        let bytes = outer.file("manifest.json", manifest).finish();
        assert!(matches!(
            validate_bytes(&bytes, &arm64()),
            Err(EngineError::ImageDigestMismatch { .. })
        ));
    }

    #[test]
    fn truncated_archive_is_rejected() {
        let bytes = docker_save_archive(&arm64(), &[base_layer()]);
        let cut = &bytes[..bytes.len() / 2];
        assert!(validate_bytes(cut, &arm64()).is_err());
    }

    #[test]
    fn outer_traversal_and_escaping_symlinks_are_rejected() {
        let bytes = Outer::new().file("../evil", b"x").finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains(".."));
        let bytes = Outer::new().file("/abs", b"x").finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains("relative"));
        let bytes = Outer::new().symlink("a", "../../etc/passwd").finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains("outside"));
        let bytes = Outer::new().symlink("a", "/etc/passwd").finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains("outside"));
    }

    #[test]
    fn duplicate_outer_entries_are_rejected() {
        let bytes = Outer::new()
            .file("manifest.json", b"[]")
            .file("manifest.json", b"[]")
            .finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains("duplicate"));
    }

    #[test]
    fn layer_attacks_are_rejected() {
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (layer_tar(&[L::File("../../etc/passwd", b"x", 0o644)]), ".."),
            (
                layer_tar(&[
                    L::Symlink("etc", "/host/etc"),
                    L::File("etc/passwd", b"x", 0o644),
                ]),
                "through symlink",
            ),
            (layer_tar(&[L::Hardlink("x", "../../etc/shadow")]), ".."),
            (layer_tar(&[L::Hardlink("x", "/etc/shadow")]), "absolute"),
            (
                layer_tar(&[
                    L::File("a/file", b"x", 0o644),
                    L::Symlink("a", "/host"),
                    L::Hardlink("b", "a/file"),
                ]),
                "symlink",
            ),
            (
                layer_tar(&[L::Hardlink("x", "etc/shadow")]),
                "not an earlier entry",
            ),
            (layer_tar(&[L::Char("dev/kmem")]), "device node"),
            (
                layer_tar(&[L::File("a/.wh.", b"", 0o644)]),
                "malformed whiteout",
            ),
            (
                layer_tar(&[L::File("a/.wh..wh.plnk", b"", 0o644)]),
                "malformed whiteout",
            ),
            (layer_tar(&[L::Dir("a/.wh.b")]), "whiteout"),
            (
                layer_tar(&[L::File("/etc/passwd", b"x", 0o644)]),
                "absolute",
            ),
            (
                layer_tar(&[
                    L::File("a", b"x", 0o644),
                    L::Dir("a"),
                    L::Hardlink("b", "a"),
                ]),
                "not an earlier entry",
            ),
        ];
        for (layer, needle) in cases {
            let (bytes, _) = oci_archive(&arm64(), &[layer]);
            let reason = rejected_reason(validate_bytes(&bytes, &arm64()));
            assert!(
                reason.contains(needle),
                "{reason:?} should mention {needle:?}"
            );
        }
    }

    #[test]
    fn a_lower_layer_symlink_cannot_be_written_through() {
        let lower = layer_tar(&[L::Symlink("a", "/outside")]);
        let upper = layer_tar(&[L::File("a/file", b"x", 0o644)]);
        let (bytes, _) = oci_archive(&arm64(), &[lower.clone(), upper]);
        let reason = rejected_reason(validate_bytes(&bytes, &arm64()));
        assert!(reason.contains("through symlink `a`"), "{reason}");

        // Whiting the symlink out (or replacing it with a directory) in the
        // upper layer makes the path a plain directory again.
        let replaced = layer_tar(&[
            L::File(".wh.a", b"", 0o644),
            L::Dir("a"),
            L::File("a/file", b"x", 0o644),
        ]);
        let (bytes, _) = oci_archive(&arm64(), &[lower, replaced]);
        validate_bytes(&bytes, &arm64()).unwrap();

        // Merged-usr style links are fine when upper layers write the real
        // path.
        let usr = layer_tar(&[L::Dir("usr/lib"), L::Symlink("lib", "usr/lib")]);
        let pkg = layer_tar(&[L::File("usr/lib/libx.so", b"x", 0o644)]);
        let (bytes, _) = oci_archive(&arm64(), &[usr, pkg]);
        validate_bytes(&bytes, &arm64()).unwrap();
    }

    #[test]
    fn decompression_bomb_is_capped() {
        let big = layer_tar(&[L::File("zeros", &vec![0u8; 2 * 1024 * 1024], 0o644)]);
        let (bytes, _) = oci_archive(&arm64(), &[big]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        std::fs::write(&path, &bytes).unwrap();
        let mut small = limits();
        small.max_layer_bytes = 1024 * 1024;
        let reason = rejected_reason(validate_archive(&path, &arm64(), &[], &small, &mut |_| {}));
        assert!(reason.contains("limit"), "{reason}");
    }

    #[test]
    fn oversized_archive_and_too_many_layers_are_rejected() {
        let second = layer_tar(&[L::File("second", b"2", 0o644)]);
        let (bytes, _) = oci_archive(&arm64(), &[base_layer(), second]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        std::fs::write(&path, &bytes).unwrap();
        let mut l = limits();
        l.max_archive_bytes = 100;
        assert!(
            rejected_reason(validate_archive(&path, &arm64(), &[], &l, &mut |_| {}))
                .contains("limit")
        );
        let mut l = limits();
        l.max_layers = 1;
        assert!(
            rejected_reason(validate_archive(&path, &arm64(), &[], &l, &mut |_| {}))
                .contains("layers")
        );
    }

    #[test]
    fn legacy_save_identity_depends_on_content_not_entry_names() {
        let build = |layer: Vec<u8>| {
            let diff = format!("sha256:{}", crate::engine::oci::verify::sha256_hex(&layer));
            let config = config_json(&arm64(), &[diff]);
            Outer::new()
                .file("l/layer.tar", &layer)
                .file("config.json", &config)
                .file(
                    "manifest.json",
                    br#"[{"Config":"config.json","RepoTags":["a:1"],"Layers":["l/layer.tar"]}]"#,
                )
                .finish()
        };
        let one = validate_bytes(&build(base_layer()), &arm64()).unwrap();
        let other = layer_tar(&[L::File("different", b"x", 0o644)]);
        let two = validate_bytes(&build(other), &arm64()).unwrap();
        assert_ne!(one.manifest_digest, two.manifest_digest);
    }

    #[test]
    fn modern_save_manifest_digest_must_match_config_and_layers() {
        let l1 = base_layer();
        let l2 = layer_tar(&[L::File("other", b"x", 0o644)]);
        let diff = format!("sha256:{}", crate::engine::oci::verify::sha256_hex(&l1));
        let config = config_json(&arm64(), &[diff]);
        let hex = crate::engine::oci::verify::sha256_hex;
        let (l1_hex, l2_hex, config_hex) = (hex(&l1), hex(&l2), hex(&config));
        let manifest_with = |layer_hex: &str| {
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2, "mediaType": OCI_MANIFEST,
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                           "digest": format!("sha256:{config_hex}"), "size": config.len()},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar",
                            "digest": format!("sha256:{layer_hex}"), "size": 1}],
            }))
            .unwrap()
        };
        let (wrong, right) = (manifest_with(&l2_hex), manifest_with(&l1_hex));
        let (wrong_hex, right_hex) = (hex(&wrong), hex(&right));
        let desc = |hex: &str| {
            serde_json::json!({"mediaType": OCI_MANIFEST,
                                                  "digest": format!("sha256:{hex}"), "size": 1})
        };
        let save = |manifests: Vec<serde_json::Value>| {
            let index = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests,
            }))
            .unwrap();
            let entry = serde_json::to_vec(&serde_json::json!([{
                "Config": format!("blobs/sha256/{config_hex}"),
                "RepoTags": ["a:1"],
                "Layers": [format!("blobs/sha256/{l1_hex}")],
            }]))
            .unwrap();
            let mut outer = Outer::new();
            for blob in [&l1, &l2, &config, &wrong, &right] {
                outer = outer.blob(blob).0;
            }
            outer
                .file("index.json", &index)
                .file("manifest.json", &entry)
                .finish()
        };
        // The manifest sharing the config but listing other layers is not
        // this image, whichever order the index lists them in.
        for manifests in [
            vec![desc(&wrong_hex), desc(&right_hex)],
            vec![desc(&right_hex), desc(&wrong_hex)],
        ] {
            let v = validate_bytes(&save(manifests), &arm64()).unwrap();
            assert_eq!(v.manifest_digest.as_str(), format!("sha256:{right_hex}"));
        }
        // Only the inconsistent manifest: fall back to the content identity.
        let v = validate_bytes(&save(vec![desc(&wrong_hex)]), &arm64()).unwrap();
        assert_ne!(v.manifest_digest.as_str(), format!("sha256:{wrong_hex}"));
    }

    #[test]
    fn legacy_save_archive_symlinked_layers_are_followed() {
        let layer = base_layer();
        let diff = format!("sha256:{}", crate::engine::oci::verify::sha256_hex(&layer));
        let config = config_json(&arm64(), &[diff.clone(), diff]);
        let bytes = Outer::new()
            .file("a/layer.tar", &layer)
            .symlink("b/layer.tar", "../a/layer.tar")
            .file("c.json", &config)
            .file(
                "manifest.json",
                br#"[{"Config":"c.json","RepoTags":["x:latest"],"Layers":["a/layer.tar","b/layer.tar"]}]"#,
            )
            .finish();
        let v = validate_bytes(&bytes, &arm64()).unwrap();
        assert_eq!(v.diff_ids.len(), 2);
    }

    #[test]
    fn ambiguous_multi_image_archive_is_narrowed_by_reference_or_rejected() {
        let one = base_layer();
        let two = layer_tar(&[L::File("other", b"2", 0o644)]);
        let c1 = config_json(
            &arm64(),
            &[format!(
                "sha256:{}",
                crate::engine::oci::verify::sha256_hex(&one)
            )],
        );
        let c2 = config_json(
            &arm64(),
            &[format!(
                "sha256:{}",
                crate::engine::oci::verify::sha256_hex(&two)
            )],
        );
        let bytes = Outer::new()
            .file("1.tar", &one)
            .file("2.tar", &two)
            .file("c1.json", &c1)
            .file("c2.json", &c2)
            .file(
                "manifest.json",
                br#"[{"Config":"c1.json","RepoTags":["one:latest"],"Layers":["1.tar"]},
                    {"Config":"c2.json","RepoTags":["two:latest"],"Layers":["2.tar"]}]"#,
            )
            .finish();
        assert!(rejected_reason(validate_bytes(&bytes, &arm64())).contains("2 different images"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        std::fs::write(&path, &bytes).unwrap();
        let v = validate_archive(
            &path,
            &arm64(),
            &["two:latest".into()],
            &limits(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(v.config_digest, sha256_digest(&c2));
    }

    #[test]
    fn runtime_projection_excludes_unselected_images_and_preserves_selected_blobs() {
        let platform = OciPlatform::host_linux();
        let bad = layer_tar(&[L::File("../escape", b"bad", 0o777)]);
        let good = layer_tar(&[L::File("bin/tool", b"good", 0o755)]);
        let bad_config = config_json(&platform, &[sha256_digest(&bad).to_string()]);
        let good_config = config_json(&platform, &[sha256_digest(&good).to_string()]);
        let docker = Outer::new().file("bad.tar", &bad).file("good.tar", &good)
            .file("bad.json", &bad_config).file("good.json", &good_config)
            .file("manifest.json", br#"[
                {"Config":"bad.json","RepoTags":["other:latest"],"Layers":["bad.tar"]},
                {"Config":"good.json","RepoTags":["selected:latest","unrelated:latest"],"Layers":["good.tar"]}
            ]"#).finish();
        let mut members = BTreeMap::new();
        let mut manifests = Vec::new();
        for (reference, layer) in [("other:latest", &bad), ("selected:latest", &good)] {
            let (bytes, _) = oci_archive(&platform, std::slice::from_ref(layer));
            for entry in tar::Archive::new(&bytes[..]).entries().unwrap() {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                if name == "index.json" {
                    let index: serde_json::Value = serde_json::from_slice(&content).unwrap();
                    let mut descriptor = index["manifests"][0].clone();
                    descriptor["annotations"] =
                        serde_json::json!({"org.opencontainers.image.ref.name": reference});
                    manifests.push(descriptor);
                } else {
                    members.insert(name, content);
                }
            }
        }
        members.insert(
            "index.json".into(),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests,
            }))
            .unwrap(),
        );
        let mut combined = Outer::new();
        for (name, content) in members {
            combined = combined.file(&name, &content);
        }
        let oci = combined.finish();
        for bytes in [docker, oci] {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("input.tar");
            std::fs::write(&input, bytes).unwrap();
            let checked = validate_archive(
                &input,
                &platform,
                &["selected:latest".into()],
                &limits(),
                &mut |_| {},
            )
            .unwrap();
            let acquired = crate::engine::oci::AcquiredImage {
                identity: crate::data::oci_identity::ImageIdentity {
                    reference: "selected:latest".into(),
                    manifest_digest: checked.manifest_digest.clone(),
                    config_digest: checked.config_digest.clone(),
                    platform: platform.clone(),
                    source: crate::data::config::image_source::ImageSourceKind::Archive,
                },
                archive: input.clone(),
                archive_format: checked.format,
                bytes: std::fs::metadata(&input).unwrap().len(),
            };
            let (_projected, output) =
                prepare_runtime_archive(&acquired, "awman-test:latest", &limits()).unwrap();
            let result = validate_archive(&output, &platform, &[], &limits(), &mut |_| {}).unwrap();
            assert_eq!(result.config_digest, checked.config_digest);
            assert_eq!(result.diff_ids, checked.diff_ids);
            let before = scan_outer(&input, &limits()).unwrap();
            let after = scan_outer(&output, &limits()).unwrap();
            assert!(!after.entries.contains_key("bad.tar"));
            assert!(!after.entries.contains_key("bad.json"));
            assert!(!after
                .entries
                .contains_key(&blob_name(&sha256_digest(&bad_config))));
            for (name, kind) in &after.entries {
                if matches!(name.as_str(), "manifest.json" | "index.json" | "oci-layout") {
                    continue;
                }
                if let OuterKind::File { size, sha256 } = kind {
                    let (_, original_size, original_sha) = before.resolve_file(name).unwrap();
                    assert_eq!(*size, original_size);
                    assert_eq!(sha256, original_sha);
                }
            }
            let doc = after
                .doc(if checked.format == ArchiveFormat::DockerSave {
                    "manifest.json"
                } else {
                    "index.json"
                })
                .unwrap();
            let text = std::str::from_utf8(doc).unwrap();
            assert!(text.contains("awman-test:latest"));
            assert!(!text.contains("unrelated:latest"));
            #[cfg(awman_builtin)]
            {
                let cache = dir.path().join("sdk-cache");
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let loaded = runtime
                    .block_on(microsandbox_image::load_archive(
                        &cache,
                        &output,
                        microsandbox_image::ImageLoadOptions {
                            tags: vec!["awman-test:latest".into()],
                            progress: None,
                        },
                    ))
                    .expect("actual SDK must import the projected image without a VM");
                assert!(!loaded.is_empty());
                assert!(
                    loaded
                        .iter()
                        .all(|image| image.metadata.config_digest
                            == checked.config_digest.to_string())
                );
                assert!(loaded
                    .iter()
                    .all(|image| image.reference.contains("awman-test")));
            }
        }
    }

    #[test]
    fn zstd_layers_are_rejected_with_a_reason() {
        assert!(check_layer_media_type(
            Path::new("x"),
            "application/vnd.oci.image.layer.v1.tar+zstd"
        )
        .is_err());
    }

    #[test]
    fn gzip_wrapped_archives_are_staged_as_plain_tar() {
        use crate::engine::oci::verify::{tests::FixedDisk, StagingSink};
        let bytes = docker_save_archive(&arm64(), &[base_layer()]);
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.tar.gz");
        std::fs::write(&src, gzip(&bytes)).unwrap();
        let disk = FixedDisk(None);
        let mut sink = StagingSink::create(dir.path().join("out"), u64::MAX, 0, &disk).unwrap();
        stage_local_archive(&src, &mut sink, u64::MAX).unwrap();
        let (out, n) = sink.finish().unwrap();
        assert_eq!(n as usize, bytes.len());
        assert_eq!(std::fs::read(out).unwrap(), bytes);
    }
}
