//! Short, private roots for macOS guest control sockets.
use anyhow::{ensure, Result};
use std::path::Path;

pub(crate) const APPLE_SOCKET_LIMIT: usize = 103;

/// The longest guest-driver socket after the runner and VM state roots are
/// created. Check the canonical path because macOS resolves /tmp to /private/tmp.
pub(crate) fn guest_socket_len(root: &Path) -> Result<usize> {
    let canonical = root.canonicalize()?;
    Ok(guest_socket_len_at(&canonical))
}

fn guest_socket_len_at(canonical: &Path) -> usize {
    canonical
        .join("r-XXXXXX/tmp/g-XXXXXXXX/s/run/agent/00000000000000000000000000000000.control.sock")
        .as_os_str()
        .as_encoded_bytes()
        .len()
}

/// Kit-run supplies TMPROOT directly to the tests, so the generic runner's
/// r-*/tmp nesting is absent. Include the separator in the declared suffix.
pub(crate) const KIT_SOCKET_SUFFIX_BYTES: usize = 69;
#[cfg(unix)]
pub(crate) fn kit_guest_socket_len(root: &Path) -> Result<usize> {
    Ok(root.canonicalize()?.as_os_str().as_encoded_bytes().len() + KIT_SOCKET_SUFFIX_BYTES)
}

pub(crate) fn short_apple_root() -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("a.").rand_bytes(6);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let root = builder.tempdir_in("/tmp")?;
    let length = guest_socket_len(root.path())?;
    ensure!(
        length <= APPLE_SOCKET_LIMIT,
        "Apple test socket would be {length} bytes (limit {APPLE_SOCKET_LIMIT}) under {}; use a shorter private /tmp mount",
        root.path().canonicalize()?.display()
    );
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn short_root_has_room_for_the_canonical_guest_socket() {
        let root = short_apple_root().unwrap();
        assert!(root.path().starts_with("/tmp"));
        assert!(guest_socket_len(root.path()).unwrap() <= APPLE_SOCKET_LIMIT);
        assert_eq!(
            guest_socket_len_at(Path::new("/private/tmp/a.123456")),
            APPLE_SOCKET_LIMIT
        );
        assert!(
            guest_socket_len_at(Path::new("/private/tmp/awman-test.123456")) > APPLE_SOCKET_LIMIT
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(root.path()).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
    }
}
