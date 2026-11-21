//! Shared atomic, mode-preserving configuration writes.

use std::io::Write;
use std::path::Path;

use crate::data::error::DataError;

fn symlink_refusal(path: &Path) -> DataError {
    DataError::Other(format!(
        "{} is a symlink; refusing to write config through it",
        path.display()
    ))
}

/// Refuse when any existing component of `path` below `root` is a symlink or
/// `path` itself exists but is not a regular file.
///
/// Repo config ships with the checkout, so `.awman/` or `config.json` may be
/// a repo-controlled symlink pointing anywhere. `root` (the git root) itself
/// is trusted. Components that do not exist yet end the walk: they will be
/// created as real directories.
pub fn refuse_symlinks_below(root: &Path, path: &Path) -> Result<(), DataError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        DataError::Other(format!(
            "{} is outside {}; refusing to use it as config",
            path.display(),
            root.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(symlink_refusal(&current));
            }
            Ok(metadata) if current == path && !metadata.is_file() => {
                return Err(DataError::Other(format!(
                    "{} is not a regular file; refusing to use it as config",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(DataError::io(&current, error)),
        }
    }
    Ok(())
}

/// Read a config file under `root` without following repo-controlled
/// symlinks. `Ok(None)` when the file does not exist.
pub fn read_config_file_within(root: &Path, path: &Path) -> Result<Option<String>, DataError> {
    refuse_symlinks_below(root, path)?;
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DataError::io(path, error)),
    }
}

/// Atomically replace a config under `root`, refusing a symlink at any
/// component below `root` (not only the file itself), so a symlinked
/// `.awman/` cannot redirect the write out of the repo.
///
/// On Unix the write is anchored to directory handles: every directory below
/// `root` is opened with `O_NOFOLLOW`, the temporary file is created and
/// renamed relative to the held handle, and the path from `root` is checked
/// to still lead to that handle immediately before the rename. Replacing an
/// ancestor during the write is refused, and the rename can never land
/// outside the directory that was verified. Elsewhere the check is
/// path-based and repeated immediately before the write.
pub fn write_config_file_within(root: &Path, path: &Path, content: &str) -> Result<(), DataError> {
    refuse_symlinks_below(root, path)?;
    #[cfg(unix)]
    {
        anchored::write_within(root, path, content, &mut || {})
    }
    #[cfg(not(unix))]
    {
        write_config_file_atomically(path, content)
    }
}

/// [`write_config_file_within`] with a hook that runs after the directory
/// handles are held and the temporary file is written, before the final
/// ancestor check and rename: the window a concurrent swap would use.
#[cfg(all(unix, test))]
pub(crate) fn write_config_file_within_racing(
    root: &Path,
    path: &Path,
    content: &str,
    mut race: impl FnMut(),
) -> Result<(), DataError> {
    refuse_symlinks_below(root, path)?;
    anchored::write_within(root, path, content, &mut race)
}

#[cfg(unix)]
mod anchored {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};

    use nix::errno::Errno;
    use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
    use nix::sys::stat::{fchmod, fstatat, mkdirat, Mode, SFlag};
    use nix::unistd::{unlinkat, UnlinkatFlags};

    use super::symlink_refusal;
    use crate::data::error::DataError;

    fn dir_flags() -> OFlag {
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY
    }

    fn errno(path: &Path, error: Errno) -> DataError {
        DataError::io(path, error.into())
    }

    fn swapped(path: &Path) -> DataError {
        DataError::Other(format!(
            "{} was replaced while the config was being written; refusing to write config \
             through it",
            path.display()
        ))
    }

    fn file_kind(mode: nix::sys::stat::mode_t) -> SFlag {
        SFlag::from_bits_truncate(mode) & SFlag::S_IFMT
    }

    /// The path from `root` through `names` still leads to `held[i + 1]`
    /// for every directory, without following a symlink.
    fn verify_chain(held: &[File], names: &[&OsStr], root: &Path) -> Result<(), DataError> {
        let mut current = root.to_path_buf();
        for (index, name) in names.iter().enumerate() {
            current.push(name);
            let stat = fstatat(&held[index], *name, AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|_| swapped(&current))?;
            let expected = held[index + 1]
                .metadata()
                .map_err(|e| DataError::io(&current, e))?;
            if file_kind(stat.st_mode) != SFlag::S_IFDIR
                || stat.st_dev as u64 != expected.dev()
                || stat.st_ino as u64 != expected.ino()
            {
                return Err(swapped(&current));
            }
        }
        Ok(())
    }

    pub(super) fn write_within(
        root: &Path,
        path: &Path,
        content: &str,
        before_commit: &mut dyn FnMut(),
    ) -> Result<(), DataError> {
        let outside = || {
            DataError::Other(format!(
                "{} is outside {}; refusing to use it as config",
                path.display(),
                root.display()
            ))
        };
        let relative = path.strip_prefix(root).map_err(|_| outside())?;
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => names.push(name),
                _ => return Err(outside()),
            }
        }
        let (leaf, directories) = names.split_last().ok_or_else(outside)?;
        // The root (the git root) itself is trusted.
        let mut held: Vec<File> = vec![open(root, dir_flags(), Mode::empty())
            .map(File::from)
            .map_err(|e| errno(root, e))?];
        let mut current = root.to_path_buf();
        for name in directories {
            current.push(name);
            let parent = held.last().expect("root is held");
            match mkdirat(parent, *name, Mode::from_bits_truncate(0o777)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(e) => return Err(errno(&current, e)),
            }
            let child = openat(parent, *name, dir_flags(), Mode::empty())
                .map(File::from)
                .map_err(|e| match e {
                    Errno::ELOOP | Errno::ENOTDIR => symlink_refusal(&current),
                    e => errno(&current, e),
                })?;
            held.push(child);
        }
        let directory = held.last().expect("root is held");
        let existing_mode = match fstatat(directory, *leaf, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                let kind = file_kind(stat.st_mode);
                if kind == SFlag::S_IFLNK {
                    return Err(symlink_refusal(path));
                }
                if kind != SFlag::S_IFREG {
                    return Err(DataError::Other(format!(
                        "{} is not a regular file; refusing to write config to it",
                        path.display()
                    )));
                }
                // Opening without truncation changes nothing; it proves the
                // file is ours to replace.
                openat(
                    directory,
                    *leaf,
                    OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| {
                    DataError::io(
                        path,
                        std::io::Error::new(
                            std::io::Error::from(e).kind(),
                            format!("config file is not writable ({e})"),
                        ),
                    )
                })?;
                Some(Mode::from_bits_truncate(stat.st_mode & 0o7777))
            }
            Err(Errno::ENOENT) => None,
            Err(e) => return Err(errno(path, e)),
        };
        let temporary = format!(
            ".{}.{}.tmp",
            leaf.to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        );
        let result = (|| {
            let file = openat(
                directory,
                temporary.as_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                // A new config gets what `fs::write` gave it (0666 less the
                // umask); a replaced one keeps its mode below.
                existing_mode.unwrap_or(Mode::from_bits_truncate(0o666)),
            )
            .map_err(|e| errno(path, e))?;
            if let Some(mode) = existing_mode {
                fchmod(&file, mode).map_err(|e| errno(path, e))?;
            }
            let mut file = File::from(file);
            file.write_all(content.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| DataError::io(path, e))?;
            before_commit();
            verify_chain(&held, directories, root)?;
            renameat(directory, temporary.as_str(), directory, *leaf).map_err(|e| errno(path, e))
        })();
        if let Err(error) = result {
            let _ = unlinkat(directory, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
            return Err(error);
        }
        let _ = directory.sync_all();
        Ok(())
    }
}

/// [`write_config_file_atomically`] for the user's own global config, which
/// may be a symlink into a dotfiles checkout: the link is followed and its
/// target is replaced atomically, so the link itself survives.
pub fn write_user_config_file(path: &Path, content: &str) -> Result<(), DataError> {
    write_config_file_atomically(&resolve_symlinks(path)?, content)
}

/// Follow `path` through any chain of symlinks, including one whose final
/// target does not exist yet (it will be created).
fn resolve_symlinks(path: &Path) -> Result<std::path::PathBuf, DataError> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target =
                    std::fs::read_link(&current).map_err(|e| DataError::io(&current, e))?;
                current = match current.parent() {
                    Some(parent) => parent.join(target),
                    None => target,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(DataError::Other(format!(
        "{} has too many levels of symlinks",
        path.display()
    )))
}

/// Replace a config using a temporary file and rename in the same directory.
/// Existing permissions are preserved and a new file gets the umask default;
/// symlinks, special files and targets the user cannot write refuse.
pub fn write_config_file_atomically(path: &Path, content: &str) -> Result<(), DataError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(symlink_refusal(path));
            }
            if !metadata.is_file() {
                return Err(DataError::Other(format!(
                    "{} is not a regular file; refusing to write config to it",
                    path.display()
                )));
            }
            if metadata.permissions().readonly() {
                return Err(DataError::io(
                    path,
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "config file is read-only",
                    ),
                ));
            }
            // The mode bits are not enough: a 0644 file owned by another user
            // is not ours to replace, so also check access. Opening without
            // truncation changes nothing. (Temp + rename still replaces hard
            // links and drops extended attributes; config files are not
            // expected to carry either.)
            if let Err(error) = std::fs::OpenOptions::new().write(true).open(path) {
                return Err(DataError::io(
                    path,
                    std::io::Error::new(
                        error.kind(),
                        format!("config file is not writable ({error})"),
                    ),
                ));
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(DataError::io(path, error)),
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Config may be the first operation creating the global awman home.
    // Create it private regardless of umask; never chmod an existing parent.
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut directories, 0o700);
    directories
        .create(parent)
        .map_err(|e| DataError::io(parent, e))?;
    if std::fs::metadata(parent)
        .map_err(|e| DataError::io(parent, e))?
        .permissions()
        .readonly()
    {
        return Err(DataError::io(
            parent,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "config directory is read-only",
            ),
        ));
    }
    let mut builder = tempfile::Builder::new();
    // Temp files are 0600 by default; a new config file gets what
    // `fs::write` gave it before (0666 less the umask).
    #[cfg(unix)]
    if metadata.is_none() {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = builder
        .tempfile_in(parent)
        .map_err(|e| DataError::io(path, e))?;
    temporary
        .write_all(content.as_bytes())
        .map_err(|e| DataError::io(path, e))?;
    if let Some(metadata) = metadata {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|e| DataError::io(path, e))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| DataError::io(path, e))?;
    #[cfg(not(windows))]
    temporary
        .persist(path)
        .map_err(|e| DataError::io(path, e.error))?;
    #[cfg(windows)]
    {
        // MoveFileEx may transiently refuse replacement while another reader
        // closes the old file. Retain the complete sibling and retry only the
        // atomic rename, never truncate the visible checkpoint.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match temporary.persist(path) {
                Ok(_) => break,
                Err(error) => {
                    if !matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                        || std::time::Instant::now() >= deadline
                    {
                        return Err(DataError::io(path, error.error));
                    }
                    temporary = error.file;
                    std::thread::park_timeout(std::time::Duration::from_millis(1));
                }
            }
        }
    }
    // Make the rename itself durable.
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    #[test]
    fn repo_write_refuses_an_ancestor_swapped_for_a_symlink_mid_write() {
        let s = TestSandbox::for_test(TestCase::component(
            "data::fs::config_file::tests::repo_write_refuses_an_ancestor_swapped_for_a_symlink_mid_write",
        ));
        let root = s.work();
        let awman = root.join(".awman");
        std::fs::create_dir_all(&awman).unwrap();
        std::fs::write(awman.join("config.json"), "{}").unwrap();
        // A host directory outside the repo, holding unrelated config.
        let outside = s.home().join("host-config");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("config.json"), "host").unwrap();
        let error = write_config_file_within_racing(
            &root,
            &awman.join("config.json"),
            "{\"new\":true}",
            || {
                std::fs::rename(&awman, root.join(".awman-moved")).unwrap();
                std::os::unix::fs::symlink(&outside, &awman).unwrap();
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("was replaced"), "{error}");
        assert_eq!(
            std::fs::read_to_string(outside.join("config.json")).unwrap(),
            "host"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            1,
            "no temporary file outside the repo"
        );
        let moved = root.join(".awman-moved");
        assert_eq!(
            std::fs::read_to_string(moved.join("config.json")).unwrap(),
            "{}"
        );
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
        s.finish();
    }

    #[test]
    fn repo_write_creates_missing_directories_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let s = TestSandbox::for_test(TestCase::component(
            "data::fs::config_file::tests::repo_write_creates_missing_directories_and_keeps_the_mode",
        ));
        let root = s.work();
        let path = root.join(".awman/config.json");
        write_config_file_within(&root, &path, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_config_file_within(&root, &path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(root.join(".awman")).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
        s.finish();
    }
}
