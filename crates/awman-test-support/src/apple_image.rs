//! Apple Containers image cleanup for tests that own images in the host store.

use crate::TestSandbox;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Give a verified OCI fixture an attempt-owned reference without changing
/// its content-addressed manifest or blobs. Apple `image load` has no tag
/// override, and the published archive's fixed tag may belong to another run.
pub fn private_fixture(sandbox: &TestSandbox, archive: &Path) -> (String, PathBuf) {
    let owner = sandbox
        .root()
        .file_name()
        .expect("sandbox root name")
        .to_string_lossy()
        .to_ascii_lowercase();
    let tag = format!("awman-test/guest:fixture-{owner}");
    let output = sandbox.work().join("private-guest-oci.tar");
    retag_oci_archive(archive, &output, &tag).expect("retag verified OCI fixture");
    (tag, output)
}

/// Refuse to claim a tag unless the Apple store confirms it is absent.
pub fn assert_absent(sandbox: &TestSandbox, cli: impl AsRef<std::ffi::OsStr>, tag: &str) {
    let inspection = sandbox
        .command(cli.as_ref())
        .args(["image", "inspect", tag])
        .run();
    assert!(
        !inspection.exit.success()
            && (inspection.stderr_text().contains("Image not found")
                || inspection.stdout_text().contains("Image not found")),
        "BLOCKED[fixture-ownership]: private Apple fixture tag is present or could not be inspected: {tag}: {inspection:?}"
    );
}

fn retag_oci_archive(source: &Path, output: &Path, tag: &str) -> std::io::Result<()> {
    let input = std::fs::File::open(source)?;
    let mut writer = tar::Builder::new(std::fs::File::create(output)?);
    let mut indexes = 0;
    for entry in tar::Archive::new(input).entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut header = entry.header().clone();
        if path == Path::new("index.json") {
            indexes += 1;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            let mut index: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            let manifests = index["manifests"]
                .as_array_mut()
                .filter(|items| items.len() == 1)
                .ok_or_else(|| std::io::Error::other("fixture must have one OCI manifest"))?;
            let reference = manifests[0]["annotations"]["org.opencontainers.image.ref.name"]
                .as_str()
                .ok_or_else(|| std::io::Error::other("fixture has no OCI reference"))?;
            if reference != "awman-test/guest:fixture" {
                return Err(std::io::Error::other("unexpected fixture OCI reference"));
            }
            manifests[0]["annotations"]["org.opencontainers.image.ref.name"] = tag.into();
            let changed = serde_json::to_vec(&index).map_err(std::io::Error::other)?;
            header.set_size(changed.len() as u64);
            header.set_cksum();
            writer.append_data(&mut header, &path, changed.as_slice())?;
        } else {
            writer.append_data(&mut header, &path, &mut entry)?;
        }
    }
    if indexes != 1 {
        return Err(std::io::Error::other("fixture must have one OCI index"));
    }
    writer.finish()?;
    writer.into_inner()?.flush()
}

/// Delete an owned image and verify that its tag is gone. Apple Containers
/// 0.12 can report a missing content digest after it has removed the tag.
/// Only that specific error is accepted, and only when inspection confirms
/// the image is absent. During panic unwinding, cleanup remains best effort.
pub fn delete_and_verify(sandbox: &TestSandbox, cli: impl AsRef<std::ffi::OsStr>, tag: &str) {
    let deletion = sandbox
        .command(cli.as_ref())
        .args(["image", "delete", tag])
        .run();
    if std::thread::panicking() {
        return;
    }
    let inspection = sandbox
        .command(cli.as_ref())
        .args(["image", "inspect", tag])
        .run();
    assert!(
        !inspection.exit.success()
            && (inspection.stderr_text().contains("Image not found")
                || inspection.stdout_text().contains("Image not found")),
        "Apple image {tag} remains or inspection failed: {inspection:?}"
    );
    assert!(
        deletion.exit.success()
            || deletion
                .stderr_text()
                .contains("Error: content with digest sha256:")
            || deletion
                .stdout_text()
                .contains("Error: content with digest sha256:"),
        "Apple image {tag} deletion failed unexpectedly: {deletion:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{OciImage, OciImageBuilder};

    #[test]
    fn private_fixture_changes_only_the_oci_reference() {
        let sandbox = TestSandbox::new();
        let source = sandbox.work().join("published.tar");
        let original = OciImageBuilder::new()
            .ref_name("awman-test/guest:fixture")
            .layer(&[("etc/probe", b"ok")])
            .build();
        original.write_archive(&source);
        let source_hash = crate::fixtures::sha256_file(&source).unwrap();

        let (tag, private) = private_fixture(&sandbox, &source);
        let retagged = OciImage::read_archive(&private);
        let index: serde_json::Value = serde_json::from_slice(&retagged.index).unwrap();
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            tag
        );
        assert_eq!(retagged.manifest, original.manifest);
        assert_eq!(retagged.config, original.config);
        assert_eq!(retagged.layers, original.layers);
        assert_eq!(crate::fixtures::sha256_file(&source).unwrap(), source_hash);
    }
}
