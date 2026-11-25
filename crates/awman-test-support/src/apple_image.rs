//! Apple Containers image cleanup for tests that own images in the host store.

use crate::TestSandbox;

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
