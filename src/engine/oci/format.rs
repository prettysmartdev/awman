//! The image-format contract: which archive containers, layer media types
//! and layer compressions the builtin runtime imports, and the exact,
//! actionable refusal for everything else.
//!
//! One place decides, so every source (registry pull, Docker Engine export,
//! Apple export, user archive) gets the same answer and the same message.
//!
//! | Element | Supported | Excluded (refused with a reason) |
//! |---|---|---|
//! | Archive container | plain tar; gzip-wrapped tar (`docker save \| gzip`) | zstd-, xz- or bzip2-wrapped tars |
//! | Archive layout | OCI image layout (`oci-layout` + `index.json`), including Apple `container image save` output; `docker save` (legacy and Docker 25+ with OCI index) | anything else |
//! | Layer media type | OCI/Docker `tar` and `tar+gzip` | `tar+zstd`, non-distributable/foreign layers, unknown types |
//! | Layer compression (by magic) | none, gzip | zstd |
//! | Manifest media type | OCI manifest, Docker manifest v2 | attestations/other artifacts are skipped, not selected |
//!
//! **zstd** is excluded on purpose rather than by accident: the embedded SDK
//! can read zstd layers, but awman validates every layer entry itself
//! before anything reaches the SDK store, and that validator has no zstd
//! decoder. An unvalidated compression would be a hole in the security
//! boundary, so zstd stays refused until the validator can decode it; the
//! refusal names the re-export that works. **Non-distributable** layers are
//! excluded because their bytes are, by definition, not in the archive and
//! awman never fetches from a URL a manifest names.

use std::path::Path;

use crate::engine::error::EngineError;

/// Layer media types the validator accepts.
pub const SUPPORTED_LAYER_MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
    "application/vnd.docker.image.rootfs.diff.tar",
];

/// Compression of a layer blob, detected from its media type or its first
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCompression {
    None,
    Gzip,
    /// Recognised, deliberately unsupported.
    Zstd,
}

/// Compression wrapping a whole user-supplied archive file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveContainer {
    PlainTar,
    GzipTar,
    /// Recognised, deliberately unsupported.
    ZstdTar,
    XzTar,
    Bzip2Tar,
}

/// Why a format element is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatRefusal {
    ZstdLayer { media_type: Option<String> },
    NonDistributableLayer { media_type: String },
    UnknownLayerMediaType { media_type: String },
    CompressedArchive { container: ArchiveContainer },
}

impl FormatRefusal {
    /// The actionable reason shown to the user.
    pub fn reason(&self) -> String {
        match self {
            Self::ZstdLayer { media_type } => {
                let named = media_type
                    .as_deref()
                    .map(|m| format!("layer media type `{m}` (zstd)"))
                    .unwrap_or_else(|| "zstd-compressed layer".to_string());
                format!(
                    "{named} is not supported: awman validates every layer before the SDK sees \
                     it and has no zstd decoder. Re-export with gzip layers (Docker: build \
                     without `--output type=...,compression=zstd` or `docker save` an image \
                     whose layers are gzip/uncompressed; buildx: `compression=gzip`)"
                )
            }
            Self::NonDistributableLayer { media_type } => format!(
                "non-distributable layer `{media_type}` is not supported: its bytes are not in \
                 the archive and awman never fetches layer content from URLs named by a manifest. \
                 Rebuild the image so every layer is distributable"
            ),
            Self::UnknownLayerMediaType { media_type } => format!(
                "unknown layer media type `{media_type}`; supported types are {}",
                SUPPORTED_LAYER_MEDIA_TYPES.join(", ")
            ),
            Self::CompressedArchive { container } => {
                let name = match container {
                    ArchiveContainer::ZstdTar => "zstd",
                    ArchiveContainer::XzTar => "xz",
                    ArchiveContainer::Bzip2Tar => "bzip2",
                    ArchiveContainer::PlainTar | ArchiveContainer::GzipTar => "",
                };
                format!(
                    "the archive is {name}-compressed; awman reads plain and gzip-compressed \
                     tars only. Decompress it first (e.g. `zstd -d`/`xz -d`/`bzip2 -d`) or \
                     export it again without compression"
                )
            }
        }
    }

    /// As the archive-rejection error for `path`.
    pub fn into_error(self, path: &Path) -> EngineError {
        EngineError::ImageArchiveRejected {
            path: path.to_path_buf(),
            reason: self.reason(),
        }
    }
}

/// Classify a manifest layer descriptor's media type.
pub fn classify_layer_media_type(media_type: &str) -> Result<LayerCompression, FormatRefusal> {
    if SUPPORTED_LAYER_MEDIA_TYPES.contains(&media_type) {
        return Ok(if media_type.ends_with("gzip") {
            LayerCompression::Gzip
        } else {
            LayerCompression::None
        });
    }
    let lower = media_type.to_ascii_lowercase();
    if lower.contains("nondistributable") || lower.contains("foreign") {
        return Err(FormatRefusal::NonDistributableLayer {
            media_type: media_type.to_string(),
        });
    }
    if lower.contains("zstd") {
        return Err(FormatRefusal::ZstdLayer {
            media_type: Some(media_type.to_string()),
        });
    }
    Err(FormatRefusal::UnknownLayerMediaType {
        media_type: media_type.to_string(),
    })
}

/// Classify a layer blob by its first bytes (used for `docker save` layers,
/// which carry no media type, and as a cross-check for OCI blobs).
pub fn classify_layer_magic(head: &[u8]) -> Result<LayerCompression, FormatRefusal> {
    if head.len() >= 2 && head[..2] == [0x1f, 0x8b] {
        return Ok(LayerCompression::Gzip);
    }
    if head.len() >= 4 && head[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(FormatRefusal::ZstdLayer { media_type: None });
    }
    Ok(LayerCompression::None)
}

/// Classify a user-supplied archive file by its first bytes.
pub fn classify_archive_magic(head: &[u8]) -> Result<ArchiveContainer, FormatRefusal> {
    let refuse = |container| Err(FormatRefusal::CompressedArchive { container });
    if head.len() >= 2 && head[..2] == [0x1f, 0x8b] {
        return Ok(ArchiveContainer::GzipTar);
    }
    if head.len() >= 4 && head[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return refuse(ArchiveContainer::ZstdTar);
    }
    if head.len() >= 6 && head[..6] == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        return refuse(ArchiveContainer::XzTar);
    }
    if head.len() >= 3 && &head[..3] == b"BZh" {
        return refuse(ArchiveContainer::Bzip2Tar);
    }
    Ok(ArchiveContainer::PlainTar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_media_types_classify_by_compression() {
        assert_eq!(
            classify_layer_media_type("application/vnd.oci.image.layer.v1.tar").unwrap(),
            LayerCompression::None
        );
        assert_eq!(
            classify_layer_media_type("application/vnd.oci.image.layer.v1.tar+gzip").unwrap(),
            LayerCompression::Gzip
        );
        assert_eq!(
            classify_layer_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip").unwrap(),
            LayerCompression::Gzip
        );
    }

    #[test]
    fn zstd_nondistributable_and_unknown_are_refused_with_actionable_reasons() {
        let zstd =
            classify_layer_media_type("application/vnd.oci.image.layer.v1.tar+zstd").unwrap_err();
        assert!(matches!(zstd, FormatRefusal::ZstdLayer { .. }));
        assert!(zstd.reason().contains("gzip"), "{}", zstd.reason());

        let foreign =
            classify_layer_media_type("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip")
                .unwrap_err();
        assert!(matches!(
            foreign,
            FormatRefusal::NonDistributableLayer { .. }
        ));
        assert!(foreign.reason().contains("never fetches"));
        let nondist = classify_layer_media_type(
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
        )
        .unwrap_err();
        assert!(matches!(
            nondist,
            FormatRefusal::NonDistributableLayer { .. }
        ));

        let unknown = classify_layer_media_type("application/x-what").unwrap_err();
        assert!(matches!(
            unknown,
            FormatRefusal::UnknownLayerMediaType { .. }
        ));
        assert!(unknown.reason().contains(SUPPORTED_LAYER_MEDIA_TYPES[0]));
    }

    #[test]
    fn magic_bytes_classify_layers_and_archives() {
        assert_eq!(
            classify_layer_magic(&[0x1f, 0x8b, 0x08, 0]).unwrap(),
            LayerCompression::Gzip
        );
        assert_eq!(
            classify_layer_magic(b"ustar").unwrap(),
            LayerCompression::None
        );
        assert!(matches!(
            classify_layer_magic(&[0x28, 0xb5, 0x2f, 0xfd]),
            Err(FormatRefusal::ZstdLayer { media_type: None })
        ));
        assert_eq!(
            classify_archive_magic(&[0x1f, 0x8b]).unwrap(),
            ArchiveContainer::GzipTar
        );
        assert_eq!(
            classify_archive_magic(b"").unwrap(),
            ArchiveContainer::PlainTar
        );
        for (magic, container) in [
            (
                &[0x28u8, 0xb5, 0x2f, 0xfd, 0, 0][..],
                ArchiveContainer::ZstdTar,
            ),
            (
                &[0xfd, b'7', b'z', b'X', b'Z', 0x00][..],
                ArchiveContainer::XzTar,
            ),
            (b"BZh91AY", ArchiveContainer::Bzip2Tar),
        ] {
            match classify_archive_magic(magic) {
                Err(FormatRefusal::CompressedArchive { container: c }) => {
                    assert_eq!(c, container);
                    let text = FormatRefusal::CompressedArchive { container: c }.reason();
                    assert!(text.contains("Decompress"), "{text}");
                }
                other => panic!("{magic:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn refusals_become_archive_rejections() {
        let err = FormatRefusal::ZstdLayer { media_type: None }.into_error(Path::new("/a.tar"));
        assert!(matches!(err, EngineError::ImageArchiveRejected { .. }));
        assert!(err.to_string().contains("/a.tar"));
    }
}
