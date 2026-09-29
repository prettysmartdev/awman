//! Archive corpus: `docker save` (legacy and Docker 25+), OCI image layout
//! and Apple `container image save` shaped exports, with realistic layer
//! contents. Every archive is validated, its identity checked, projected
//! for the SDK, and the projected layers are read back to prove that
//! modes, ownership, hardlinks, symlinks, whiteouts, opaque markers and
//! xattrs are byte-for-byte what the source contained.
//!
//! The hermetic corpus is synthesised here. A corpus of real images (one per
//! shipped `templates/Dockerfile.*`, exported by Docker and, on macOS, by
//! Apple Containers) is consumed by `real_corpus_*` when
//! `AWMAN_TEST_IMAGE_CORPUS` names a directory produced by
//! `tests/oci_import/corpus/build-corpus.sh`; without it those tests report
//! SKIP (or fail under `AWMAN_TEST_REQUIRE_EXTERNAL=1`).

use std::io::Read;
use std::path::{Path, PathBuf};

use awman::data::config::image_source::ImageSourceSpec;
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::archive::prepare_runtime_archive;
use awman::engine::oci::format::SUPPORTED_LAYER_MEDIA_TYPES;
use awman::engine::oci::{
    validate_archive, AcquirePolicy, AcquireRequest, AcquiredImage, ArchiveFormat, ImageAcquirer,
};

use crate::support::*;

const TAG: &str = "awman-repo-claude:latest";

fn request(path: &Path, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: TAG.into(),
        source: ImageSourceSpec::Archive {
            path: path.to_path_buf(),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: Default::default(),
    }
}

/// Every regular-file member of a tar, by name.
fn members(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = tar::Archive::new(file);
    let mut out = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        out.push((name, data));
    }
    out
}

/// The layer blobs of a projected archive, bottom first, as raw bytes.
fn projected_layers(projected: &Path, format: ArchiveFormat) -> Vec<Vec<u8>> {
    let members = members(projected);
    let find = |name: &str| {
        members
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} missing from projection"))
            .1
            .clone()
    };
    match format {
        ArchiveFormat::DockerSave => {
            let manifest: serde_json::Value =
                serde_json::from_slice(&find("manifest.json")).unwrap();
            assert_eq!(manifest.as_array().unwrap().len(), 1, "one image only");
            manifest[0]["Layers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| find(l.as_str().unwrap()))
                .collect()
        }
        ArchiveFormat::OciLayout => {
            let index: serde_json::Value = serde_json::from_slice(&find("index.json")).unwrap();
            assert_eq!(
                index["manifests"].as_array().unwrap().len(),
                1,
                "one image only"
            );
            let digest = index["manifests"][0]["digest"].as_str().unwrap();
            let manifest: serde_json::Value = serde_json::from_slice(&find(&format!(
                "blobs/sha256/{}",
                digest.trim_start_matches("sha256:")
            )))
            .unwrap();
            manifest["layers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| {
                    find(&format!(
                        "blobs/sha256/{}",
                        l["digest"].as_str().unwrap().trim_start_matches("sha256:")
                    ))
                })
                .collect()
        }
    }
}

/// The assertions every corpus member must satisfy.
fn check_preservation(acquired: &AcquiredImage, source_layers: &[Vec<u8>]) {
    let (_dir, projected) = prepare_runtime_archive(acquired, TAG, &limits()).unwrap();
    let again = validate_archive(
        &projected,
        &OciPlatform::host_linux(),
        &[TAG.to_string()],
        &limits(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(again.config_digest, acquired.identity.config_digest);
    assert_eq!(again.platform, acquired.identity.platform);
    assert_eq!(again.config.user.as_deref(), Some("1000:1000"));
    assert_eq!(again.config.home.as_deref(), Some("/home/agent"));
    assert_eq!(again.config.working_dir.as_deref(), Some("/workspace"));

    let layers = projected_layers(&projected, acquired.archive_format);
    assert_eq!(layers.len(), source_layers.len(), "ordered layer identity");
    for (got, want) in layers.iter().zip(source_layers) {
        let got_entries = read_layer(got);
        let want_entries = read_layer(want);
        assert_eq!(got_entries, want_entries, "layer entries survive verbatim");
        // And the raw uncompressed stream is the diff_id the config names.
        let uncompressed: Vec<u8> = if got.len() >= 2 && got[..2] == [0x1f, 0x8b] {
            let mut d = flate2::read::MultiGzDecoder::new(&got[..]);
            let mut v = Vec::new();
            d.read_to_end(&mut v).unwrap();
            v
        } else {
            got.clone()
        };
        assert_eq!(hex(&uncompressed), hex(want));
    }
    let base = read_layer(&source_layers[0]);
    let entry = |name: &str| base.iter().find(|e| e.0 == name).unwrap().clone();
    assert_eq!(entry("usr/bin/agent").2, 0o755, "executable mode");
    assert_eq!(
        (
            entry("home/agent/.profile").3,
            entry("home/agent/.profile").4
        ),
        (1000, 1000)
    );
    assert_eq!(entry("home/agent/").2, 0o700);
    assert_eq!(entry("usr/bin/agent-alias").1, tar::EntryType::Link);
    assert_eq!(
        entry("usr/bin/agent-alias").5.as_deref(),
        Some("usr/bin/agent")
    );
    assert_eq!(entry("lib").5.as_deref(), Some("usr/lib"));
    assert_eq!(entry("bin/sh").5.as_deref(), Some("/usr/bin/agent"));
    assert_eq!(
        entry("usr/bin/agent").6,
        vec![("user.awman.marker".to_string(), "base".to_string())]
    );
    let upper = read_layer(&source_layers[1]);
    assert!(upper.iter().any(|e| e.0 == "etc/.wh.old.conf"));
    assert!(upper.iter().any(|e| e.0 == "opt/stale/.wh..wh..opq"));
}

fn acquire(path: &Path) -> (tempfile::TempDir, awman::engine::oci::LeasedImage) {
    let state = tempfile::tempdir().unwrap();
    let got = acquirer(state.path())
        .acquire(&request(path, AcquirePolicy::IfMissing), &mut |_| {})
        .unwrap();
    (state, got)
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn archive_real_corpus_preserves_identity_and_links() {
    let temp = tempfile::tempdir().unwrap();
    let layers = realistic_layers();
    let image = Image::new(OciPlatform::host_linux(), layers.clone(), TAG);

    // 1. Legacy docker save (uncompressed layers, RepoTags).
    let legacy = write(
        temp.path(),
        "legacy.tar",
        &docker_save(std::slice::from_ref(&image)),
    );
    let (_s1, got) = acquire(&legacy);
    assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
    check_preservation(&got, &layers);

    // 2. Docker 25+ save: manifest.json plus OCI index and blobs; the
    //    manifest digest is the real one from the index.
    let modern = docker_save_modern(std::slice::from_ref(&image));
    let modern_path = write(temp.path(), "modern.tar", &modern.bytes);
    let (_s2, got) = acquire(&modern_path);
    assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
    assert_eq!(
        got.identity.manifest_digest.as_str(),
        format!("sha256:{}", modern.manifests[0].1)
    );
    check_preservation(&got, &layers);

    // 3. OCI layout as buildx `-o type=oci` writes it, with an attestation
    //    manifest that must be skipped, never selected.
    let oci = oci_layout(std::slice::from_ref(&image), true, false);
    let oci_path = write(temp.path(), "oci.tar", &oci.bytes);
    let (_s3, got) = acquire(&oci_path);
    assert_eq!(got.archive_format, ArchiveFormat::OciLayout);
    assert_eq!(
        got.identity.manifest_digest.as_str(),
        format!("sha256:{}", oci.manifests[0].1)
    );
    assert_eq!(
        got.identity.config_digest.as_str(),
        format!("sha256:{}", oci.manifests[0].2)
    );
    check_preservation(&got, &layers);

    // 4. Apple `container image save` shape: PAX headers, directory entries,
    //    ref.name annotations, and a second platform's manifest alongside.
    let foreign = Image::new(
        other_platform(&OciPlatform::host_linux()),
        layers.clone(),
        TAG,
    );
    let apple = oci_layout(&[image.clone(), foreign], false, true);
    let apple_path = write(temp.path(), "apple.tar", &apple.bytes);
    let (_s4, got) = acquire(&apple_path);
    assert_eq!(got.identity.platform, OciPlatform::host_linux());
    assert_eq!(
        got.identity.manifest_digest.as_str(),
        format!("sha256:{}", apple.manifests[0].1)
    );
    check_preservation(&got, &layers);

    // 5. A gzip-wrapped `docker save | gzip` is staged as plain tar.
    let gz = write(temp.path(), "legacy.tar.gz", &gzip(&docker_save(&[image])));
    let (_s5, got) = acquire(&gz);
    assert_eq!(got.archive_format, ArchiveFormat::DockerSave);
    check_preservation(&got, &layers);
}

#[test]
fn archive_zstd_and_nondistributable_policy() {
    let temp = tempfile::tempdir().unwrap();
    let layers = realistic_layers();
    let reason = |err: EngineError| match err {
        EngineError::ImageArchiveRejected { reason, .. } => reason,
        other => panic!("expected ImageArchiveRejected, got {other:?}"),
    };
    let refuse = |name: &str, bytes: &[u8]| {
        let path = write(temp.path(), name, bytes);
        let state = tempfile::tempdir().unwrap();
        let err = acquirer(state.path())
            .acquire(&request(&path, AcquirePolicy::IfMissing), &mut |_| {})
            .unwrap_err();
        assert_eq!(cached_archives(state.path()), 0, "{name}: nothing cached");
        reason(err)
    };

    // zstd layer media type in an OCI manifest.
    let mut zstd = Image::new(OciPlatform::host_linux(), layers.clone(), TAG);
    zstd.layer_media_type = Some("application/vnd.oci.image.layer.v1.tar+zstd".into());
    let text = refuse("zstd-media.tar", &oci_layout(&[zstd], false, false).bytes);
    assert!(text.contains("zstd") && text.contains("gzip"), "{text}");

    // zstd bytes in a docker-save layer (no media type to go by).
    let mut fake_zstd = vec![0x28, 0xb5, 0x2f, 0xfd];
    fake_zstd.extend_from_slice(&layers[0]);
    let text = refuse(
        "zstd-magic.tar",
        &docker_save(&[Image::new(OciPlatform::host_linux(), vec![fake_zstd], TAG)]),
    );
    assert!(text.contains("zstd") && text.contains("gzip"), "{text}");

    // A zstd-compressed archive file (`docker save | zstd`).
    let mut zst_file = vec![0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x00];
    zst_file.extend_from_slice(b"not really zstd");
    let text = refuse("save.tar.zst", &zst_file);
    assert!(
        text.contains("zstd-compressed") && text.contains("Decompress"),
        "{text}"
    );
    let text = refuse("save.tar.xz", &[0xfd, b'7', b'z', b'X', b'Z', 0x00, 0, 0]);
    assert!(text.contains("xz-compressed"), "{text}");

    // Non-distributable / foreign layers are refused by policy even when
    // the archive happens to carry the bytes.
    for media in [
        "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
        "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
    ] {
        let mut foreign = Image::new(OciPlatform::host_linux(), layers.clone(), TAG);
        foreign.layer_media_type = Some(media.into());
        let text = refuse("foreign.tar", &oci_layout(&[foreign], false, false).bytes);
        assert!(
            text.contains("non-distributable") && text.contains("never fetches"),
            "{text}"
        );
    }

    // Unknown types name what is supported.
    let mut odd = Image::new(OciPlatform::host_linux(), layers, TAG);
    odd.layer_media_type = Some("application/x-squashfs".into());
    let text = refuse("odd.tar", &oci_layout(&[odd], false, false).bytes);
    assert!(text.contains(SUPPORTED_LAYER_MEDIA_TYPES[0]), "{text}");
}

#[test]
fn restrictive_symlink_rules_hold_against_realistic_layers() {
    // Merged-usr links, absolute symlinks, hardlinks to earlier entries and
    // whiteouts of symlinked paths are all accepted; writing through a
    // symlink created in a lower layer is still refused.
    let temp = tempfile::tempdir().unwrap();
    let layers = realistic_layers();
    let ok = write(
        temp.path(),
        "ok.tar",
        &oci_layout(
            &[Image::new(OciPlatform::host_linux(), layers.clone(), TAG)],
            false,
            false,
        )
        .bytes,
    );
    acquire(&ok);

    let escape = layer(&[Entry::file("lib/evil.so", b"x", 0o644)]);
    let mut bad_layers = layers;
    bad_layers.push(escape);
    let bad = write(
        temp.path(),
        "bad.tar",
        &oci_layout(
            &[Image::new(OciPlatform::host_linux(), bad_layers, TAG)],
            false,
            false,
        )
        .bytes,
    );
    let state = tempfile::tempdir().unwrap();
    let err = acquirer(state.path())
        .acquire(&request(&bad, AcquirePolicy::IfMissing), &mut |_| {})
        .unwrap_err();
    match err {
        EngineError::ImageArchiveRejected { reason, .. } => {
            assert!(reason.contains("through symlink `lib`"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(cached_archives(state.path()), 0);
}

// ── Real image corpus (gated) ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct CorpusManifest {
    entries: Vec<CorpusEntry>,
}

#[derive(serde::Deserialize)]
struct CorpusEntry {
    /// Which shipped template built this image (`claude`, `codex`, …).
    template: String,
    /// `docker-save`, `oci-layout` or `apple-export`.
    format: String,
    /// Archive path relative to the corpus directory.
    archive: String,
    /// The reference the image was tagged with when exported.
    reference: String,
    /// `linux/arm64` or `linux/amd64`.
    platform: String,
    /// Expected `sha256:` config digest recorded by the exporter.
    config_digest: String,
}

fn missing_pairs(
    templates: &[String],
    formats: &[&str],
    covered: &std::collections::BTreeSet<(String, String)>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for template in templates {
        for format in formats {
            if !covered.contains(&(template.clone(), (*format).to_string())) {
                missing.push(format!("{template}/{format}"));
            }
        }
    }
    missing
}

#[test]
fn corpus_inventory_requires_every_template_format_pair() {
    let templates = vec!["claude".into(), "codex".into()];
    let mut covered = std::collections::BTreeSet::from([
        ("claude".into(), "docker-save".into()),
        ("codex".into(), "docker-save".into()),
        ("claude".into(), "oci-layout".into()),
    ]);
    assert_eq!(
        missing_pairs(&templates, &["docker-save", "oci-layout"], &covered),
        ["codex/oci-layout"]
    );
    covered.insert(("codex".into(), "oci-layout".into()));
    assert!(missing_pairs(&templates, &["docker-save", "oci-layout"], &covered).is_empty());
    assert_eq!(
        missing_pairs(
            &templates,
            &["docker-save", "oci-layout", "apple-export"],
            &covered
        ),
        ["claude/apple-export", "codex/apple-export"]
    );
}

fn real_corpus() -> Option<PathBuf> {
    let dir = std::env::var_os("AWMAN_TEST_IMAGE_CORPUS").map(PathBuf::from);
    match dir {
        Some(dir) if dir.join("manifest.json").is_file() => Some(dir),
        _ => {
            let reason = "AWMAN_TEST_IMAGE_CORPUS does not name a directory with manifest.json \
                          (build one with tests/oci_import/corpus/build-corpus.sh)";
            eprintln!("SKIP: real_corpus: {reason}");
            if std::env::var_os("AWMAN_TEST_REQUIRE_EXTERNAL").is_some() {
                panic!("real corpus is required (AWMAN_TEST_REQUIRE_EXTERNAL=1) but: {reason}");
            }
            None
        }
    }
}

#[test]
fn real_corpus_every_shipped_template_archive_validates_and_projects() {
    let Some(dir) = real_corpus() else { return };
    let manifest: CorpusManifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let templates: Vec<String> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("templates"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_prefix("Dockerfile.")
                    .map(str::to_string)
            })
            .filter(|t| t != "project")
            .collect();
    let host = OciPlatform::host_linux().to_string();
    let mut covered = std::collections::BTreeSet::new();
    for entry in manifest.entries.iter().filter(|e| e.platform == host) {
        let path = dir.join(&entry.archive);
        let state = tempfile::tempdir().unwrap();
        let mut req = request(&path, AcquirePolicy::IfMissing);
        req.tag = entry.reference.clone();
        let production_limits = awman::engine::oci::AcquireLimits::DEFAULT;
        let got = awman::engine::oci::default_acquirer(
            state.path(),
            production_limits,
            &awman::data::config::env::EnvSnapshot::empty(),
        )
        .acquire(&req, &mut |_| {})
        .unwrap_or_else(|e| panic!("{}: {e}", entry.archive));
        assert_eq!(
            got.identity.config_digest.as_str(),
            entry.config_digest,
            "{}",
            entry.archive
        );
        let expected_format = match entry.format.as_str() {
            "docker-save" => ArchiveFormat::DockerSave,
            "oci-layout" | "apple-export" => ArchiveFormat::OciLayout,
            other => panic!("unknown corpus format {other}"),
        };
        assert_eq!(got.archive_format, expected_format, "{}", entry.archive);
        let (_dir, projected) =
            prepare_runtime_archive(&got, &entry.reference, &production_limits).unwrap();
        let again = validate_archive(
            &projected,
            &OciPlatform::host_linux(),
            std::slice::from_ref(&entry.reference),
            &production_limits,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(again.config_digest, got.identity.config_digest);
        assert_eq!(again.platform, got.identity.platform);
        assert!(
            covered.insert((entry.template.clone(), entry.format.clone())),
            "duplicate template/format pair in corpus: {}/{}",
            entry.template,
            entry.format
        );
    }
    let formats: &[&str] = if cfg!(target_os = "macos") {
        &["docker-save", "oci-layout", "apple-export"]
    } else {
        &["docker-save", "oci-layout"]
    };
    let missing = missing_pairs(&templates, formats, &covered);
    assert!(
        missing.is_empty(),
        "the corpus lacks required template/format pairs for {host}: {missing:?}"
    );
}
