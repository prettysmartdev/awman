//! Copy inert Git metadata without following source or destination links.
use std::path::Path;

pub struct GitMetadataCopy;
impl GitMetadataCopy {
    pub fn copy(source: &Path, destination: &Path) -> std::io::Result<()> {
        copy(source, destination)
    }
}

#[cfg(unix)]
fn copy(source: &Path, destination: &Path) -> std::io::Result<()> {
    use nix::fcntl::{openat, OFlag, AT_FDCWD};
    use nix::sys::stat::{mkdirat, Mode};
    use std::os::fd::{AsFd, OwnedFd};
    use std::path::{Component, PathBuf};
    fn parent(path: &Path, create: bool) -> std::io::Result<(OwnedFd, std::ffi::OsString)> {
        let path = if path.is_absolute() {
            path.into()
        } else {
            std::env::current_dir()?.join(path)
        };
        // These are operating-system aliases on macOS, outside any checkout.
        let path: PathBuf = if let Ok(rest) = path.strip_prefix("/var") {
            std::fs::canonicalize("/var")?.join(rest)
        } else if let Ok(rest) = path.strip_prefix("/tmp") {
            std::fs::canonicalize("/tmp")?.join(rest)
        } else {
            path
        };
        let names: Vec<_> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n),
                _ => None,
            })
            .collect();
        let (name, parents) = names
            .split_last()
            .ok_or_else(|| std::io::Error::other("missing metadata filename"))?;
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let mut fd = openat(AT_FDCWD, Path::new("/"), flags, Mode::empty())?;
        for name in parents {
            if create {
                match mkdirat(&fd, *name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            fd = openat(&fd, *name, flags, Mode::empty())?;
        }
        Ok((fd, name.to_os_string()))
    }
    fn tree(
        source_parent: &OwnedFd,
        source_name: &std::ffi::OsStr,
        dest_parent: &OwnedFd,
        dest_name: &std::ffi::OsStr,
    ) -> std::io::Result<()> {
        let fd = openat(
            source_parent,
            source_name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let mut input = std::fs::File::from(fd);
        let meta = input.metadata()?;
        if meta.is_dir() {
            match mkdirat(dest_parent, dest_name, Mode::from_bits_truncate(0o700)) {
                Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                Err(e) => return Err(e.into()),
            }
            let dest = openat(
                dest_parent,
                dest_name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
            let mut listing = nix::dir::Dir::from_fd(nix::unistd::dup(input.as_fd())?)?;
            for entry in listing.iter() {
                let entry = entry?;
                if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." {
                    continue;
                }
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
                tree(&nix::unistd::dup(input.as_fd())?, name, &dest, name)?;
            }
        } else if meta.is_file() {
            let fd = openat(
                dest_parent,
                dest_name,
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_NONBLOCK
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )?;
            let mut output = std::fs::File::from(fd);
            use std::os::unix::fs::MetadataExt;
            if !output.metadata()?.is_file() || output.metadata()?.nlink() != 1 {
                return Err(std::io::Error::other("unsafe metadata destination"));
            }
            output.set_len(0)?;
            std::io::copy(&mut input, &mut output)?;
        } else {
            return Err(std::io::Error::other("unsafe Git metadata"));
        }
        Ok(())
    }
    let (src, src_name) = parent(source, false)?;
    let (dst, dst_name) = parent(destination, true)?;
    tree(&src, &src_name, &dst, &dst_name)
}

#[cfg(not(unix))]
fn copy(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "secure Git metadata copying is unsupported on this platform",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn source_links_and_destination_links_cannot_escape_the_metadata_tree() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(source.join("ref"), "new").unwrap();
        std::fs::write(temp.path().join("outside"), "untouched").unwrap();
        symlink(temp.path().join("outside"), dest.join("ref")).unwrap();
        assert!(GitMetadataCopy::copy(&source, &dest).is_err());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("outside")).unwrap(),
            "untouched"
        );
        std::fs::remove_file(dest.join("ref")).unwrap();
        symlink(temp.path().join("outside"), source.join("link")).unwrap();
        assert!(GitMetadataCopy::copy(&source, &dest).is_err());
    }
}
