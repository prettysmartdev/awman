//! Deterministic fixture builders (WI 3.6).
//!
//! Each builder writes plain files into a sandbox and returns paths; none
//! returns awman types. Fixtures belong to the sandbox they were built into
//! and are removed with it. Identical inputs produce identical bytes
//! (fixed authors, dates, modes and ordering).

mod config_tree;
mod credentials;
mod oci;
mod repo;
mod work_item;
mod workflow;

pub use config_tree::{ConfigPaths, ConfigTree};
pub use credentials::{CredentialFiles, WrittenCredential};
pub use oci::{Blob, OciImage, OciImageBuilder};
pub use repo::{Repo, RepoBuilder};
pub use work_item::WorkItemBuilder;
pub use workflow::{WorkflowBuilder, WorkflowStep};

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Lowercase hex SHA-256 of a file's contents.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The fixture store: `AWMAN_TEST_FIXTURES`, default
/// `<workspace>/target/awman-fixtures`. A fixture `<id>` is the file
/// `<store>/<id>`.
pub fn store() -> PathBuf {
    crate::runtime_env::get()
        .fixtures
        .clone()
        .unwrap_or_else(|| crate::config::workspace_root().join("target/awman-fixtures"))
}

/// An external, content-addressed fixture, SHA-256 verified against
/// `[fixtures.<id>] sha256` of the gate manifest before first use in this
/// process. Missing, unpinned or mismatching → panic `BLOCKED[fixture:<id>]: …`.
/// Prefer [`crate::TestSandbox::fixture`], which also records the digest in
/// the failure evidence.
pub fn external(id: &str) -> PathBuf {
    external_checked(id).unwrap_or_else(|reason| panic!("BLOCKED[fixture:{id}]: {reason}"))
}

pub(crate) fn external_checked(id: &str) -> Result<PathBuf, String> {
    let spec = crate::config::run_config()
        .fixtures
        .get(id)
        .cloned()
        .ok_or_else(|| format!("`{id}` is not declared in [fixtures] of the gate manifest"))?;
    pinned_file(id, &spec, &store())
}

/// Resolve and verify one pinned input in `dir`. A `pending` pin or a
/// missing digest blocks; the bytes are re-hashed whenever the file's size
/// or modification time changed since the last verification in this process.
pub(crate) fn pinned_file(
    id: &str,
    spec: &crate::config::FixtureSpec,
    dir: &Path,
) -> Result<PathBuf, String> {
    type Seen = BTreeMap<PathBuf, (u64, Option<std::time::SystemTime>)>;
    static VERIFIED: OnceLock<Mutex<Seen>> = OnceLock::new();
    if let Some(pending) = &spec.pending {
        return Err(format!("`{id}` is pending: {pending}"));
    }
    let expected = spec
        .sha256
        .as_deref()
        .ok_or_else(|| format!("`{id}` has no sha256 pin in the gate manifest"))?;
    let path = dir.join(spec.file.as_deref().unwrap_or(id));
    let stamp = std::fs::symlink_metadata(&path)
        .map(|m| (m.len(), m.modified().ok()))
        .map_err(|e| format!("{}: {e} (run `cargo xtask fixtures fetch`)", path.display()))?;
    let verified = VERIFIED.get_or_init(|| Mutex::new(BTreeMap::new()));
    if verified.lock().unwrap().get(&path) == Some(&stamp) {
        return Ok(path);
    }
    verify(&path, expected)?;
    verified.lock().unwrap().insert(path.clone(), stamp);
    Ok(path)
}

/// Verify `path` is a regular file (not a symlink) with SHA-256 `expected`.
pub(crate) fn verify(path: &Path, expected: &str) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("{}: {e} (run `cargo xtask fixtures fetch`)", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let actual = sha256_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if actual != expected.to_lowercase() {
        return Err(format!(
            "{}: sha256 {actual}, manifest pins {expected}",
            path.display()
        ));
    }
    Ok(())
}

impl crate::TestSandbox {
    /// [`external`] plus recording the fixture digest in this test's evidence.
    pub fn fixture(&self, id: &str) -> PathBuf {
        let path = external(id);
        if let Some(digest) = crate::config::run_config()
            .fixtures
            .get(id)
            .and_then(|f| f.sha256.as_deref())
        {
            self.record_fixture(id, digest);
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_known_input() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_rejects_mismatch_missing_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"abc").unwrap();
        assert!(verify(&file, &sha256_hex(b"abc")).is_ok());
        assert!(verify(&file, &sha256_hex(b"abd"))
            .unwrap_err()
            .contains("manifest pins"));
        assert!(verify(&dir.path().join("missing"), "00").is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(verify(&link, &sha256_hex(b"abc"))
                .unwrap_err()
                .contains("not a regular file"));
        }
    }

    #[test]
    fn pending_or_digestless_pins_block_and_valid_pins_resolve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("base.tar"), b"abc").unwrap();
        let mut spec = crate::config::FixtureSpec {
            sha256: Some(sha256_hex(b"abc")),
            file: Some("base.tar".into()),
            pending: Some("P7: not published".into()),
            ..Default::default()
        };
        let err = pinned_file("guest-oci-base", &spec, dir.path()).unwrap_err();
        assert!(err.contains("pending: P7"), "{err}");
        spec.pending = None;
        assert_eq!(
            pinned_file("guest-oci-base", &spec, dir.path()).unwrap(),
            dir.path().join("base.tar")
        );
        // Changing the bytes is noticed even after a successful verification.
        std::fs::write(dir.path().join("base.tar"), b"abcd").unwrap();
        assert!(pinned_file("guest-oci-base", &spec, dir.path()).is_err());
        spec.sha256 = None;
        let err = pinned_file("guest-oci-base", &spec, dir.path()).unwrap_err();
        assert!(err.contains("no sha256 pin"), "{err}");
    }

    #[test]
    fn unpinned_fixture_blocks() {
        let err = std::panic::catch_unwind(|| external("no-such-fixture")).unwrap_err();
        let text = crate::panic_text(&err);
        assert!(
            text.starts_with("BLOCKED[fixture:no-such-fixture]: "),
            "{text}"
        );
    }
}
