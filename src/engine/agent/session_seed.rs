//! One-way seeding of a private session storage directory from the host
//! (`SessionStorageDir::seed_from_home`).
//!
//! opencode-v2 receives a private, per-execution copy of host
//! `~/.config/opencode` so the 2.x process can read the user's configuration
//! without ever writing to the files OpenCode 1.x reads. Rules:
//!
//! - regular files and directories only; unsafe entries fail the launch;
//!   `node_modules` and `.git` are excluded;
//! - nothing is ever copied back;
//! - a missing host directory starts the private directory empty;
//! - at most [`SEED_MAX_ENTRIES`] entries and [`SEED_MAX_BYTES`] bytes. A
//!   partial copy could drop a `deny` rule, so exceeding a cap fails the
//!   launch instead of truncating;
//! - files are created `0600`, directories `0700` (Unix).

use std::path::{Component, Path};

use crate::engine::error::EngineError;

type FileTransform<'a> = dyn Fn(&Path, Vec<u8>) -> Result<Vec<u8>, EngineError> + 'a;

/// Most entries (files plus directories) one seed may copy.
pub const SEED_MAX_ENTRIES: usize = 5_000;
/// Most file bytes one seed may copy.
pub const SEED_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(unix)]
const SKIPPED_NAMES: &[&str] = &["node_modules", ".git"];

/// Copy `<host_home>/<relative>` into `dest` (which must already exist).
pub fn seed_from_home(
    host_home: Option<&Path>,
    relative: &str,
    dest: &Path,
) -> Result<(), EngineError> {
    seed_with_data_home(
        host_home,
        None,
        relative,
        dest,
        SEED_MAX_ENTRIES,
        SEED_MAX_BYTES,
    )
}

#[cfg(all(test, unix))]
pub(crate) fn seed_with_caps(
    host_home: Option<&Path>,
    relative: &str,
    dest: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), EngineError> {
    seed_with_data_home(host_home, None, relative, dest, max_entries, max_bytes)
}

pub(crate) fn seed_with_data_home(
    host_home: Option<&Path>,
    data_home: Option<&Path>,
    relative: &str,
    dest: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), EngineError> {
    let rel = Path::new(relative);
    if relative.is_empty()
        || !rel
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || crate::data::config::agent_definition::is_deny_listed_host_dir(relative)
    {
        return Err(EngineError::Config(format!(
            "invalid session storage seed path: {relative}"
        )));
    }
    let Some(home) = host_home else {
        return Ok(());
    };
    let source = home.join(rel);
    // Validate the same real-location boundary as settings mounts, including
    // relocated protected aliases and the session's actual awman data home.
    if crate::engine::overlay::seed_overlaps_protected_root(&source, home, data_home) {
        return Err(EngineError::Config(format!(
            "unsafe session seed: {} overlaps a protected directory",
            source.display()
        )));
    }
    let source_dir = match open_seed_root(home, rel) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(EngineError::io(&source, e)),
    };
    #[cfg(unix)]
    {
        let mut budget = Budget {
            entries: 0,
            bytes: 0,
            max_entries,
            max_bytes,
            root: format!("~/{relative}"),
        };
        copy_dir(source_dir, &source, dest, &mut budget)
    }
    #[cfg(not(unix))]
    {
        let _ = (source_dir, dest, max_entries, max_bytes);
        Err(EngineError::Config(
            "secure session config copying is unavailable on this platform".into(),
        ))
    }
}

#[cfg(unix)]
struct Budget {
    entries: usize,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
    root: String,
}

#[cfg(unix)]
impl Budget {
    fn take(&mut self, bytes: u64) -> Result<(), EngineError> {
        self.entries += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if self.entries > self.max_entries {
            return Err(EngineError::Config(format!(
                "{} has more than {} entries; awman copies it into the session and refuses a \
                 partial copy. Remove unneeded files from it and retry",
                self.root, self.max_entries
            )));
        }
        if self.bytes > self.max_bytes {
            return Err(EngineError::Config(format!(
                "{} is larger than {} MiB; awman copies it into the session and refuses a \
                 partial copy. Remove unneeded files from it and retry",
                self.root,
                self.max_bytes / (1024 * 1024)
            )));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_seed_root(home: &Path, relative: &Path) -> std::io::Result<std::fs::File> {
    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::Mode;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    // Canonicalize HOME only. Never canonicalize/follow a seed component.
    let home = std::fs::canonicalize(home)?;
    let mut dir = crate::data::fs::safe_tree::platform::context_root(&home, false)?;
    for part in relative.components() {
        dir = openat(&dir, Path::new(part.as_os_str()), flags, Mode::empty())?;
    }
    Ok(std::fs::File::from(dir))
}

#[cfg(not(unix))]
fn open_seed_root(home: &Path, relative: &Path) -> std::io::Result<std::fs::File> {
    std::fs::symlink_metadata(home.join(relative))?;
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure session config copying is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn copy_dir(
    dir: std::fs::File,
    source: &Path,
    dest: &Path,
    budget: &mut Budget,
) -> Result<(), EngineError> {
    copy_selected_dir(
        dir,
        source,
        dest,
        budget,
        Path::new(""),
        &|_| true,
        &|_, bytes| Ok(bytes),
        true,
    )
}

/// Profile import/export uses the same descriptor-relative traversal as seed.
/// Selection runs before opening an excluded entry (credential sockets too).
#[cfg(unix)]
pub fn copy_profile_tree(
    source: &Path,
    dest: &Path,
    include: &dyn Fn(&Path) -> bool,
    transform: &FileTransform<'_>,
) -> Result<(), EngineError> {
    copy_profile_tree_with_owner(source, dest, include, transform, true)
}

/// Run trees may be owned by the container's user rather than the host user.
/// They still use no-follow traversal, regular-file checks and the same byte
/// and entry limits. Host-side seed and import retain the ownership check.
#[cfg(unix)]
pub fn copy_profile_tree_with_owner(
    source: &Path,
    dest: &Path,
    include: &dyn Fn(&Path) -> bool,
    transform: &FileTransform<'_>,
    require_owner: bool,
) -> Result<(), EngineError> {
    let dir = crate::data::fs::safe_tree::platform::context_root(source, false)
        .map(std::fs::File::from)
        .map_err(|e| EngineError::io(source, e))?;
    let mut budget = Budget {
        entries: 0,
        bytes: 0,
        max_entries: 50_000,
        max_bytes: 2 * 1024 * 1024 * 1024,
        root: "profile state".into(),
    };
    copy_selected_dir(
        dir,
        source,
        dest,
        &mut budget,
        Path::new(""),
        include,
        transform,
        require_owner,
    )
}
#[cfg(not(unix))]
pub fn copy_profile_tree(
    _source: &Path,
    _dest: &Path,
    _include: &dyn Fn(&Path) -> bool,
    _transform: &FileTransform<'_>,
) -> Result<(), EngineError> {
    Err(EngineError::Config(
        "secure profile copying unavailable on this platform".into(),
    ))
}
#[cfg(not(unix))]
pub fn copy_profile_tree_with_owner(
    _source: &Path,
    _dest: &Path,
    _include: &dyn Fn(&Path) -> bool,
    _transform: &FileTransform<'_>,
    _require_owner: bool,
) -> Result<(), EngineError> {
    Err(EngineError::Config(
        "secure profile copying unavailable on this platform".into(),
    ))
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn copy_selected_dir(
    dir: std::fs::File,
    source: &Path,
    dest: &Path,
    budget: &mut Budget,
    relative: &Path,
    include: &dyn Fn(&Path) -> bool,
    transform: &FileTransform<'_>,
    require_owner: bool,
) -> Result<(), EngineError> {
    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::Mode;
    use std::os::unix::ffi::OsStrExt;
    let listing = nix::dir::Dir::from_fd(
        nix::unistd::dup(&dir).map_err(|e| EngineError::io(source, e.into()))?,
    )
    .map_err(|e| EngineError::io(source, e.into()))?;
    for entry in listing {
        let entry = entry.map_err(|e| EngineError::io(source, e.into()))?;
        let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." || SKIPPED_NAMES.iter().any(|skip| name == *skip) {
            continue;
        }
        let rel = relative.join(name);
        if !include(&rel) {
            continue;
        }
        let from = source.join(name);
        let to = dest.join(name);
        // Open relative to the held directory, then inspect the opened inode.
        // A swap to a symlink fails, including between enumeration and open.
        let fd = openat(
            &dir,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| EngineError::io(&from, e.into()))?;
        let input = std::fs::File::from(fd);
        let meta = input.metadata().map_err(|e| EngineError::io(&from, e))?;
        {
            use std::os::unix::fs::MetadataExt;
            if foreign_profile_owner(meta.uid(), nix::unistd::geteuid().as_raw(), require_owner) {
                return Err(EngineError::Config("foreign profile state owner".into()));
            }
        }
        if meta.is_dir() {
            budget.take(0)?;
            create_private_dir(&to)?;
            copy_selected_dir(
                input,
                &from,
                &to,
                budget,
                &rel,
                include,
                transform,
                require_owner,
            )?;
        } else if meta.is_file() {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() != 1 {
                return Err(EngineError::Config(format!(
                    "unsafe session seed hard link: {}",
                    from.display()
                )));
            }
            if meta.len() > 512 * 1024 * 1024 {
                return Err(EngineError::Config("profile file exceeds 512 MiB".into()));
            }
            budget.take(meta.len())?;
            // Bound the actual read too, even if a file grows after metadata.
            use std::io::Read;
            let remaining = budget
                .max_bytes
                .saturating_sub(budget.bytes)
                .saturating_add(meta.len());
            let mut bytes = Vec::new();
            input
                .take(remaining + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| EngineError::io(&from, e))?;
            let copied = bytes.len() as u64;
            if copied <= remaining {
                let bytes = transform(&rel, bytes)?;
                copy_private_file(&mut bytes.as_slice(), &to)?;
            }
            if copied > remaining {
                return Err(EngineError::Config(format!(
                    "{} grew beyond the session seed byte limit",
                    budget.root
                )));
            }
            budget.bytes = budget
                .bytes
                .saturating_sub(meta.len())
                .saturating_add(copied);
        } else {
            return Err(EngineError::Config(format!(
                "unsafe session seed entry: {}",
                from.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn foreign_profile_owner(file_uid: u32, host_uid: u32, require_owner: bool) -> bool {
    require_owner && file_uid != host_uid
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<(), EngineError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| EngineError::io(path, e))
}

#[cfg(unix)]
fn copy_private_file(input: &mut impl std::io::Read, to: &Path) -> Result<u64, EngineError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(to).map_err(|e| EngineError::io(to, e))?;
    std::io::copy(input, &mut output).map_err(|e| EngineError::io(to, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh private directory inside a per-test sandbox (never the
    /// developer HOME). The sandbox is returned so it outlives the path.
    struct Dir(
        #[allow(dead_code)] awman_test_support::TestSandbox,
        std::path::PathBuf,
    );
    impl Dir {
        fn path(&self) -> &Path {
            &self.1
        }
    }
    fn tempdir() -> Dir {
        let sandbox = awman_test_support::TestSandbox::new();
        let path = sandbox.tmp().join("dir");
        std::fs::create_dir_all(&path).unwrap();
        Dir(sandbox, path)
    }

    #[cfg(unix)]
    #[test]
    fn run_tree_export_does_not_require_host_uid_ownership() {
        assert!(foreign_profile_owner(2000, 1000, true));
        assert!(!foreign_profile_owner(2000, 1000, false));
    }

    #[cfg(unix)]
    #[test]
    fn copies_regular_files_and_skips_heavy_dirs() {
        let home = tempdir();
        let dest = tempdir();
        let cfg = home.path().join(".config/opencode");
        std::fs::create_dir_all(cfg.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(cfg.join(".git")).unwrap();
        std::fs::create_dir_all(cfg.join("agent")).unwrap();
        std::fs::write(cfg.join("opencode.json"), "{\"permission\":{}}").unwrap();
        std::fs::write(cfg.join("agent/review.md"), "review").unwrap();
        std::fs::write(cfg.join("node_modules/pkg/index.js"), "x").unwrap();

        seed_from_home(Some(home.path()), ".config/opencode", dest.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.path().join("opencode.json")).unwrap(),
            "{\"permission\":{}}"
        );
        assert!(dest.path().join("agent/review.md").is_file());
        assert!(!dest.path().join("node_modules").exists());
        assert!(!dest.path().join(".git").exists());
        assert!(std::fs::symlink_metadata(dest.path().join("linked")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dest.path().join("opencode.json")), 0o600);
            assert_eq!(mode(&dest.path().join("agent")), 0o700);
        }
        // Writes into the copy never reach the host.
        std::fs::write(dest.path().join("opencode.json"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join("opencode.json")).unwrap(),
            "{\"permission\":{}}"
        );
    }

    #[test]
    fn missing_host_directory_or_home_starts_empty() {
        let home = tempdir();
        let dest = tempdir();
        seed_from_home(Some(home.path()), ".config/opencode", dest.path()).unwrap();
        seed_from_home(None, ".config/opencode", dest.path()).unwrap();
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_seed_root_is_not_followed() {
        let home = tempdir();
        let elsewhere = tempdir();
        std::fs::write(elsewhere.path().join("secret"), "x").unwrap();
        std::fs::create_dir_all(home.path().join(".config")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join(".config/opencode")).unwrap();
        let dest = tempdir();
        assert!(seed_from_home(Some(home.path()), ".config/opencode", dest.path()).is_err());
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn exceeding_a_cap_fails_closed_naming_the_directory() {
        let home = tempdir();
        let cfg = home.path().join(".config/opencode");
        std::fs::create_dir_all(&cfg).unwrap();
        for i in 0..4 {
            std::fs::write(cfg.join(format!("f{i}")), "0123456789").unwrap();
        }
        let dest = tempdir();
        let err = seed_with_caps(
            Some(home.path()),
            ".config/opencode",
            dest.path(),
            3,
            1 << 20,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("~/.config/opencode") && err.contains("3 entries"),
            "{err}"
        );
        let dest = tempdir();
        let err = seed_with_caps(Some(home.path()), ".config/opencode", dest.path(), 100, 25)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("~/.config/opencode") && err.contains("MiB"),
            "{err}"
        );
    }

    #[test]
    fn deny_listed_or_escaping_paths_are_refused() {
        let home = tempdir();
        let dest = tempdir();
        for bad in [".ssh", "../x", "/etc", "", ".config/gh"] {
            assert!(
                seed_from_home(Some(home.path()), bad, dest.path()).is_err(),
                "{bad}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn protected_aliases_and_intermediate_links_are_refused() {
        use std::os::unix::fs::symlink;
        for outside in [false, true] {
            let home = tempdir();
            let other = tempdir();
            let root = if outside {
                other.path().to_path_buf()
            } else {
                home.path().join(".ssh")
            };
            std::fs::create_dir_all(root.join("opencode")).unwrap();
            std::fs::write(root.join("opencode/private-key"), "canary").unwrap();
            symlink(&root, home.path().join(".config")).unwrap();
            let dest = tempdir();
            assert!(seed_from_home(Some(home.path()), ".config/opencode", dest.path()).is_err());
            assert!(!dest.path().join("private-key").exists());
        }
        let home = tempdir();
        std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        symlink(
            home.path().join(".config/opencode"),
            home.path().join(".awman"),
        )
        .unwrap();
        let dest = tempdir();
        assert!(seed_from_home(Some(home.path()), ".config/opencode", dest.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_policy_file_or_subdirectory_fails_closed() {
        for name in ["opencode.json", "policies"] {
            let home = tempdir();
            let other = tempdir();
            let cfg = home.path().join(".config/opencode");
            std::fs::create_dir_all(&cfg).unwrap();
            std::os::unix::fs::symlink(other.path(), cfg.join(name)).unwrap();
            let dest = tempdir();
            assert!(seed_from_home(Some(home.path()), ".config/opencode", dest.path()).is_err());
        }
    }

    #[test]
    fn actual_data_home_is_never_copied() {
        let home = tempdir();
        let cfg = home.path().join(".config/opencode");
        std::fs::create_dir_all(&cfg).unwrap();
        let dest = tempdir();
        assert!(seed_with_data_home(
            Some(home.path()),
            Some(&cfg),
            ".config/opencode",
            dest.path(),
            100,
            1000
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_link_after_directory_open_is_refused() {
        let home = tempdir();
        let cfg = home.path().join(".config/opencode");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("policy"), "old").unwrap();
        let pinned = open_seed_root(home.path(), Path::new(".config/opencode")).unwrap();
        std::fs::remove_file(cfg.join("policy")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", cfg.join("policy")).unwrap();
        let dest = tempdir();
        let mut budget = Budget {
            entries: 0,
            bytes: 0,
            max_entries: 100,
            max_bytes: 1000,
            root: "fixture".into(),
        };
        assert!(copy_dir(pinned, &cfg, dest.path(), &mut budget).is_err());
        assert!(!dest.path().join("policy").exists());
    }
}
