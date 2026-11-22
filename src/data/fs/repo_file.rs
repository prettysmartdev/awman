//! Read repository prompt templates without following repository-controlled links.

use std::io;
#[cfg(unix)]
use std::io::Read;
use std::path::{Component, Path};

/// Read a regular UTF-8 template below `root`. All components must be normal
/// relative names. Symlinks are refused, including links within the repository.
/// Descriptor-relative opens prevent a concurrent link swap from escaping root.
pub fn read_template(root: &Path, relative: &Path) -> io::Result<String> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt template must be a relative path without traversal",
        ));
    }
    read_beneath(root, &components)
}

#[cfg(unix)]
fn read_beneath(root: &Path, components: &[Component<'_>]) -> io::Result<String> {
    use nix::fcntl::{openat, OFlag, AT_FDCWD};
    use nix::sys::stat::Mode;
    let directory = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY;
    let mut dir = openat(AT_FDCWD, root, directory, Mode::empty())?;
    for component in &components[..components.len() - 1] {
        dir = openat(
            &dir,
            Path::new(component.as_os_str()),
            directory | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )?;
    }
    let fd = openat(
        &dir,
        Path::new(components.last().unwrap().as_os_str()),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt template must be a regular file",
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(not(unix))]
fn read_beneath(_: &Path, _: &[Component<'_>]) -> io::Result<String> {
    // Fail closed on platforms without the descriptor-relative implementation.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "custom merge-fix templates require secure descriptor-relative file access on this platform",
    ))
}
