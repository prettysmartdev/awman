//! Execution-private helper payloads. Publishing a generation is staging only.
#[cfg(unix)]
use crate::data::{automation_records::MAX_SAFE_COUNTER, secret::SecretString};
#[cfg(unix)]
use std::path::Path;
/// Guest helper: one open binds the generation header and token atomically.
/// Its stdout belongs exclusively to the provider's auth callback. This raw
/// helper discards generation metadata and cannot qualify a live route or
/// acknowledge consumption. A future adapter needs a separate correlated channel.
pub const HELPER: &str = r#"#!/bin/sh
set +x
set -eu
exec 3</awman/credentials/payload
IFS= read -r generation <&3
case "$generation" in ''|*[!0-9]*) exit 1;; esac
exec cat <&3
"#;
/// A held root prevents directory substitution during generation publication.
#[cfg(unix)]
pub struct HelperDirectory {
    root: std::fs::File,
    retired: std::sync::Mutex<bool>,
}
#[cfg(unix)]
impl HelperDirectory {
    /// Adopt an execution-private root while holding its execution ownership,
    /// before starting readers or writers. Retire crash-left payloads first.
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        use std::os::unix::fs::OpenOptionsExt;
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "private_delivery_unavailable")?;
        let meta = root
            .metadata()
            .map_err(|_| "private_delivery_unavailable")?;
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err("private_delivery_unavailable");
        }
        let directory = Self {
            root,
            retired: std::sync::Mutex::new(false),
        };
        directory.remove_payload()?;
        Ok(directory)
    }
    pub fn stage(&self, generation: u64, token: &SecretString) -> Result<(), &'static str> {
        let retired = self.retired.lock().unwrap_or_else(|p| p.into_inner());
        if *retired {
            return Err("private_delivery_unavailable");
        }
        use nix::fcntl::{openat, renameat, OFlag};
        use nix::sys::stat::{fstat, Mode};
        use std::io::Write;
        if generation == 0
            || generation > MAX_SAFE_COUNTER
            || token.expose().is_empty()
            || token.expose().len() > 8192
            || token.expose().contains(['\r', '\n', '\0'])
        {
            return Err("invalid_secrets");
        }
        // Never adopt a symlink/hardlink. A missing old payload is valid.
        match openat(
            &self.root,
            "payload",
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let stat = fstat(&fd).map_err(|_| "private_delivery_unavailable")?;
                if stat.st_nlink != 1
                    || stat.st_mode & libc::S_IFMT != libc::S_IFREG
                    || stat.st_uid != nix::unistd::geteuid().as_raw()
                    || stat.st_mode & 0o077 != 0
                {
                    return Err("private_delivery_unavailable");
                }
            }
            Err(nix::errno::Errno::ENOENT) => {}
            Err(_) => return Err("private_delivery_unavailable"),
        }
        let name = format!(".generation-{}", uuid::Uuid::new_v4());
        let fd = openat(
            &self.root,
            name.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|_| "private_delivery_unavailable")?;
        let mut file = std::fs::File::from(fd);
        let result = (|| {
            writeln!(file, "{generation}")?;
            file.write_all(token.expose().as_bytes())?;
            file.sync_all()?;
            renameat(&self.root, name.as_str(), &self.root, "payload")
                .map_err(std::io::Error::from)?;
            self.root.sync_all()
        })()
        .map_err(|_| "private_delivery_unavailable");
        if result.is_err() {
            let _ = nix::unistd::unlinkat(
                &self.root,
                name.as_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
        }
        if result.is_err() {
            let _ = self.remove_payload();
        }
        result
    }
    fn remove_payload(&self) -> Result<(), &'static str> {
        use nix::unistd::{unlinkat, UnlinkatFlags};
        match unlinkat(&self.root, "payload", UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
            Err(_) => return Err("private_delivery_unavailable"),
        }
        // Held descriptor, no traversal of a replaceable pathname.
        let mut directory = nix::dir::Dir::from_fd(
            nix::unistd::dup(&self.root).map_err(|_| "private_delivery_unavailable")?,
        )
        .map_err(|_| "private_delivery_unavailable")?;
        for entry in directory.iter() {
            let entry = entry.map_err(|_| "private_delivery_unavailable")?;
            let name = entry.file_name();
            if name.to_bytes().starts_with(b".generation-") {
                unlinkat(&self.root, name, UnlinkatFlags::NoRemoveDir)
                    .map_err(|_| "private_delivery_unavailable")?;
            }
        }
        self.root
            .sync_all()
            .map_err(|_| "private_delivery_unavailable")
    }
    /// Call only for this execution after readers/writers stop. Also used when
    /// adopting a crash-left private helper directory, before any new writer.
    pub fn retire(&self) -> Result<(), &'static str> {
        let mut retired = self.retired.lock().unwrap_or_else(|p| p.into_inner());
        *retired = true;
        self.remove_payload()
    }
}
#[cfg(unix)]
impl Drop for HelperDirectory {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
