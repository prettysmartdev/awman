//! Private, stable runtime roots. Nothing is silently moved to shared /tmp.
use crate::engine::error::EngineError;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub const SOCKET_PATH_LIMIT: usize = 103;

#[derive(Clone)]
pub struct BuiltinPaths {
    pub home: PathBuf,
}

impl BuiltinPaths {
    pub fn resolve(root: &Path) -> Result<Self, EngineError> {
        // Resolve symlinks in the parent chain (e.g. macOS `/tmp` ->
        // `/private/tmp`); `secure_create` then validates every canonical
        // component. The state directory itself must not be a symlink.
        let root = &canonical_parent(root)?;
        check_socket_budget(root)?;
        secure_create(root)?;
        let home = root.canonicalize().map_err(|e| EngineError::io(root, e))?;
        check_socket_budget(&home)?;
        for sub in [
            "run",
            "cache",
            "logs",
            "sandboxes",
            "volumes",
            "snapshots",
            "secrets",
            "attach",
            "db",
        ] {
            secure_create(&home.join(sub))?;
        }
        Ok(Self { home })
    }

    /// Short catalog lock (version marker, image identities). Never held
    /// across VM boot or stop; waits a bounded time, then reports "busy".
    pub fn lock(&self) -> Result<File, EngineError> {
        self.lock_named("awman.lock", LOCK_WAIT)
    }

    /// Serialises image imports, which can take minutes, without blocking
    /// catalog readers such as `awman status`.
    #[cfg(test)]
    pub fn import_lock(&self) -> Result<File, EngineError> {
        self.lock_named("awman-import.lock", IMPORT_LOCK_WAIT)
    }

    pub fn import_lock_cancellable(
        &self,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<File, EngineError> {
        self.lock_controlled("awman-import.lock", IMPORT_LOCK_WAIT, false, cancel)
    }

    /// Protect the interval before the SDK publishes a sandbox's rootfs
    /// reference. Afterwards its transactional ImageInUse check owns retention.
    /// Mutations fail promptly rather than waiting for a planned launch.
    pub fn image_lease(&self, image: &str, exclusive: bool) -> Result<File, EngineError> {
        use sha2::{Digest, Sha256};
        let digest: String = Sha256::digest(image.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("image-{digest}.lock");
        self.lock_controlled(&name, Duration::ZERO, !exclusive, &Default::default())
    }

    fn lock_named(&self, name: &str, wait: Duration) -> Result<File, EngineError> {
        self.lock_controlled(name, wait, false, &Default::default())
    }

    fn lock_controlled(
        &self,
        name: &str,
        wait: Duration,
        shared: bool,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<File, EngineError> {
        let path = self.home.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| EngineError::io(&path, e))?;
        let metadata = file.metadata().map_err(|e| EngineError::io(&path, e))?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(EngineError::Config(
                "builtin lock must be a private regular file".into(),
            ));
        }
        let deadline = Instant::now() + wait;
        loop {
            cancel.check()?;
            match if shared { file.try_lock_shared() } else { file.try_lock() } {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(EngineError::BuiltinRuntimeUnavailable {
                        reason: format!(
                            "builtin runtime is busy: another awman process held {name} for {}s; retry shortly",
                            wait.as_secs()
                        ),
                    })
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(EngineError::io(&path, e)),
            }
        }
    }

    pub fn attach(&self, id: &str, owner: &str) -> PathBuf {
        self.home.join("attach").join(format!(
            "{}.sock",
            super::naming::sandbox_name_for(&format!("{id}/{owner}"))
        ))
    }
}

const LOCK_WAIT: Duration = Duration::from_secs(30);
const IMPORT_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);

/// Canonicalise the longest existing ancestor of `root`'s parent and re-append
/// the missing components. The final component is kept as given, so a
/// symlinked state directory is still refused by `secure_create`.
fn canonical_parent(root: &Path) -> Result<PathBuf, EngineError> {
    let (Some(parent), Some(leaf)) = (root.parent(), root.file_name()) else {
        return Ok(root.to_path_buf());
    };
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        // secure_create reports the precise error.
        return Ok(root.to_path_buf());
    }
    let mut existing = parent.to_path_buf();
    let mut missing = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(up), Some(name)) => {
                missing.push(name.to_os_string());
                existing = up.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| EngineError::io(&existing, e))?;
    resolved.extend(missing.into_iter().rev());
    resolved.push(leaf);
    Ok(resolved)
}

pub fn check_socket_budget(home: &Path) -> Result<(), EngineError> {
    // SDK hashes names again: canonical uses 24 hex characters, compatibility
    // uses 32. The latter control endpoint is the longest derived socket.
    let path = home.join("run/agent/00000000000000000000000000000000.control.sock");
    if path.as_os_str().as_encoded_bytes().len() > SOCKET_PATH_LIMIT {
        return Err(EngineError::SocketPathTooLong {
            path,
            limit: SOCKET_PATH_LIMIT,
        });
    }
    Ok(())
}

pub fn secure_create(path: &Path) -> Result<(), EngineError> {
    use nix::fcntl::{open, openat, OFlag};
    use nix::sys::stat::{mkdirat, Mode};
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(EngineError::Config(
            "builtin state directory must be an absolute path without '..'".into(),
        ));
    }
    let uid = nix::unistd::geteuid().as_raw();
    let flags = OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY;
    let mut directory: File = open("/", flags, Mode::empty())
        .map_err(|e| EngineError::io(path, e.into()))?
        .into();
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| {
            if let Component::Normal(n) = c {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    if parts.is_empty() {
        return Err(EngineError::Config(
            "builtin state directory cannot be /".into(),
        ));
    }
    for (index, component) in parts.iter().enumerate() {
        match mkdirat(
            &directory,
            Path::new(component),
            Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => (),
            Err(e) => return Err(EngineError::io(path, e.into())),
        }
        directory = openat(&directory, Path::new(component), flags, Mode::empty())
            .map_err(|e| match e {
                nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR => {
                    EngineError::Config(format!(
                        "builtin state directory {} contains a symlink or non-directory component; use a real directory path",
                        path.display()
                    ))
                }
                e => EngineError::io(path, e.into()),
            })?
            .into();
        let metadata = directory.metadata().map_err(|e| EngineError::io(path, e))?;
        let last = index + 1 == parts.len();
        let trusted_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0 && !last;
        if (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && !trusted_sticky)
            || (last && (metadata.uid() != uid || metadata.mode() & 0o077 != 0))
        {
            return Err(EngineError::Config(
                "builtin state directory requires private ownership (0700) and trusted parents"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_leases_exclude_mutation_across_processes_but_not_other_images() {
        const CHILD: &str = "AWMAN_TEST_IMAGE_LEASE_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let paths = BuiltinPaths::resolve(Path::new(&root)).unwrap();
            assert!(paths.image_lease("agent:1", true).is_err());
            assert!(paths.image_lease("agent:1", false).is_ok());
            assert!(paths.image_lease("other:1", true).is_ok());
            return;
        }
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths = BuiltinPaths::resolve(&temp.path().join("s")).unwrap();
        let lease = paths.image_lease("agent:1", false).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "engine::container::builtin::paths::tests::image_leases_exclude_mutation_across_processes_but_not_other_images", "--nocapture"])
            .env(CHILD, &paths.home).output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(lease);
        assert!(paths.image_lease("agent:1", true).is_ok());
    }

    #[test]
    fn import_lock_wait_observes_cancellation() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths = BuiltinPaths::resolve(&temp.path().join("s")).unwrap();
        let held = paths.import_lock().unwrap();
        let token = crate::engine::oci::CancelToken::new();
        let signal = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            signal.cancel();
        });
        let start = Instant::now();
        let err = paths.import_lock_cancellable(&token).unwrap_err();
        canceller.join().unwrap();
        assert!(crate::engine::oci::retry::is_cancelled(&err));
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        assert!(paths.import_lock().is_ok());
    }

    #[test]
    fn full_socket_budget_counts_utf8_bytes() {
        assert!(check_socket_budget(Path::new("/short")).is_ok());
        assert!(check_socket_budget(Path::new(&format!("/{}", "é".repeat(35)))).is_err());
    }
    #[test]
    fn socket_budget_includes_the_longest_sdk_compatibility_endpoint() {
        let suffix = "/run/agent/00000000000000000000000000000000.control.sock";
        let root = format!("/{}", "a".repeat(SOCKET_PATH_LIMIT - suffix.len() - 1));
        assert!(check_socket_budget(Path::new(&root)).is_ok());
        assert!(check_socket_budget(Path::new(&(root + "a"))).is_err());
        let paths = BuiltinPaths {
            home: "/private".into(),
        };
        assert_ne!(
            paths.attach("same-name", "old-owner"),
            paths.attach("same-name", "new-owner")
        );
        assert_eq!(
            paths.attach("../escape", "owner").parent().unwrap(),
            Path::new("/private/attach")
        );
    }
    #[test]
    fn refuses_symlinks_and_public_roots() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let root = temp.path().canonicalize().unwrap().join("private");
        secure_create(&root).unwrap();
        let link = temp.path().canonicalize().unwrap().join("link");
        symlink(&root, &link).unwrap();
        assert!(secure_create(&link).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(secure_create(&root).is_err());
    }
    #[test]
    fn symlinked_parent_is_resolved_but_symlinked_leaf_is_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let real = temp.path().canonicalize().unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = temp.path().canonicalize().unwrap().join("alias");
        symlink(&real, &alias).unwrap();
        let paths = BuiltinPaths::resolve(&alias.join("state")).unwrap();
        assert_eq!(paths.home, real.join("state"));
        let leaf = temp.path().canonicalize().unwrap().join("leaf");
        symlink(real.join("state"), &leaf).unwrap();
        assert!(matches!(
            BuiltinPaths::resolve(&leaf),
            Err(EngineError::Config(message)) if message.contains("symlink")
        ));
    }
    #[test]
    fn busy_lock_times_out_with_a_clear_error() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        let _held = paths.lock().unwrap();
        assert!(matches!(
            paths.lock_named("awman.lock", Duration::from_millis(100)),
            Err(EngineError::BuiltinRuntimeUnavailable { reason }) if reason.contains("busy")
        ));
        // The import lock is independent of the catalog lock.
        assert!(paths.import_lock().is_ok());
    }
    #[test]
    fn concurrent_creation_is_safe() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let root = temp.path().canonicalize().unwrap().join("private");
        std::thread::scope(|scope| {
            let a = scope.spawn(|| secure_create(&root));
            let b = scope.spawn(|| secure_create(&root));
            a.join().unwrap().unwrap();
            b.join().unwrap().unwrap();
        });
    }
}

#[cfg(test)]
mod hardening_tests;
