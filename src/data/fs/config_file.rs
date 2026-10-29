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
                return Err(symlink_refusal(&current))
            }
            Ok(metadata) if current == path && !metadata.is_file() => {
                return Err(DataError::Other(format!(
                    "{} is not a regular file; refusing to use it as config",
                    path.display()
                )))
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

/// [`write_config_file_atomically`] for a config under `root`, refusing a
/// symlink at any component below `root` (not only the file itself), so a
/// symlinked `.awman/` cannot redirect the write out of the repo.
///
/// The check is path-based, so it is repeated immediately before the write;
/// what remains is the window between that check and the rename.
pub fn write_config_file_within(root: &Path, path: &Path, content: &str) -> Result<(), DataError> {
    refuse_symlinks_below(root, path)?;
    write_config_file_atomically(path, content)
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
    std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
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
    temporary
        .persist(path)
        .map_err(|e| DataError::io(path, e.error))?;
    // Make the rename itself durable.
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_is_refused_and_nothing_is_written_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("config.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".awman")).unwrap();
        let path = root.join(".awman").join("config.json");

        let err = write_config_file_within(&root, &path, "{\"agent\":\"x\"}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(err.contains(".awman"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("config.json")).unwrap(),
            "{}"
        );
        let err = read_config_file_within(&root, &path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a symlink"), "{err}");
    }

    #[test]
    fn missing_components_are_allowed_and_paths_outside_root_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".awman").join("config.json");
        assert_eq!(read_config_file_within(tmp.path(), &path).unwrap(), None);
        write_config_file_within(tmp.path(), &path, "{}").unwrap();
        assert_eq!(
            read_config_file_within(tmp.path(), &path)
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        let other = tempfile::tempdir().unwrap();
        assert!(refuse_symlinks_below(other.path(), &path).is_err());
    }

    #[test]
    fn a_directory_at_the_config_path_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".awman").join("config.json");
        std::fs::create_dir_all(&path).unwrap();
        let err = read_config_file_within(tmp.path(), &path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(write_config_file_atomically(&path, "{}").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_umask_default_and_an_existing_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        write_config_file_atomically(&path, "{}").unwrap();
        // Whatever the umask, the same mode `fs::write` gives a new file.
        let sibling = tmp.path().join("plain.json");
        std::fs::write(&sibling, "{}").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), mode(&sibling));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_config_file_atomically(&path, "{\"a\":1}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_user_config_symlink_is_followed_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("awman.json"), "{}").unwrap();
        let link = tmp.path().join("config.json");
        std::os::unix::fs::symlink("dotfiles/awman.json", &link).unwrap();

        write_user_config_file(&link, "{\"a\":1}").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("awman.json")).unwrap(),
            "{\"a\":1}"
        );

        // A dangling link creates its target, as a plain write would.
        std::fs::remove_file(dotfiles.join("awman.json")).unwrap();
        write_user_config_file(&link, "{}").unwrap();
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("awman.json")).unwrap(),
            "{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_existing_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let err = write_config_file_atomically(&path, "{\"a\":1}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("read-only"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }
}
