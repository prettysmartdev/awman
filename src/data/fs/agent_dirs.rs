//! Typed paths and filesystem-only discovery for custom agent definitions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::config::agent_definition::{is_first_party_name, validate_agent_file_stem};
use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;

pub const AGENTS_SUBDIR: &str = "agents";
pub const AGENT_DEFINITION_EXT: &str = "toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefinitionScope {
    Repo,
    Global,
}

impl DefinitionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentDirs {
    global_home: PathBuf,
    git_root: Option<PathBuf>,
}

impl AgentDirs {
    pub fn from_env(env: &EnvSnapshot, git_root: Option<PathBuf>) -> Result<Self, DataError> {
        Ok(Self {
            global_home: GlobalConfig::data_home_with(env)?,
            git_root,
        })
    }

    pub fn from_process_env(git_root: Option<PathBuf>) -> Result<Self, DataError> {
        Self::from_env(&Env::from_process(), git_root)
    }

    pub fn global_dir(&self) -> PathBuf {
        self.global_home.join(AGENTS_SUBDIR)
    }

    pub fn repo_dir(&self) -> Option<PathBuf> {
        self.git_root.as_deref().map(Self::repo_dir_for)
    }

    pub fn repo_dir_for(git_root: &Path) -> PathBuf {
        git_root.join(".awman").join(AGENTS_SUBDIR)
    }

    /// Pure path construction; callers validate names before writing files.
    pub fn definition_path(&self, scope: DefinitionScope, name: &str) -> Option<PathBuf> {
        self.dir(scope)
            .map(|dir| dir.join(format!("{name}.{AGENT_DEFINITION_EXT}")))
    }

    pub fn ensure_dir(&self, scope: DefinitionScope) -> Result<PathBuf, DataError> {
        let dir = self
            .dir(scope)
            .ok_or_else(|| DataError::Other("repo agent definitions require a git root".into()))?;
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(dir)
    }

    /// Write `files` (`(file name, bytes)`) into `scope`'s directory and return
    /// their paths.
    ///
    /// The repo directory is repo-controlled, so nothing below the git root is
    /// followed: a symlinked `.awman` or `agents` directory, or a symlink at a
    /// destination name (dangling or not), is refused or replaced, never
    /// written through. Without `force` publication is no-clobber: an
    /// existing entry fails before anything is written, and a file created
    /// concurrently is never replaced. With `force` an existing entry is
    /// replaced in place, inside the selected directory only. The global
    /// home itself may be a symlink (it is the user's own), its `agents`
    /// subdirectory may not.
    pub fn publish(
        &self,
        scope: DefinitionScope,
        files: &[(&str, &[u8])],
        force: bool,
    ) -> Result<Vec<PathBuf>, DataError> {
        let (base, components): (PathBuf, &[&str]) = match scope {
            DefinitionScope::Repo => (
                self.git_root.clone().ok_or_else(|| {
                    DataError::Other("repo agent definitions require a git root".into())
                })?,
                &[".awman", AGENTS_SUBDIR],
            ),
            DefinitionScope::Global => {
                std::fs::create_dir_all(&self.global_home)
                    .map_err(|e| DataError::io(&self.global_home, e))?;
                (self.global_home.clone(), &[AGENTS_SUBDIR])
            }
        };
        let dir = components.iter().fold(base.clone(), |p, c| p.join(c));
        for (name, _) in files {
            if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
                return Err(DataError::Other(format!("invalid file name {name:?}")));
            }
        }
        publish_files(&base, components, files, force)?;
        Ok(files.iter().map(|(name, _)| dir.join(name)).collect())
    }

    fn dir(&self, scope: DefinitionScope) -> Option<PathBuf> {
        match scope {
            DefinitionScope::Repo => self.repo_dir(),
            DefinitionScope::Global => Some(self.global_dir()),
        }
    }

    /// Repo entries precede global entries, sorted within each scope. Parsing and
    /// trust checking happen only when a definition is actually loaded.
    pub fn discover(&self) -> AgentDiscovery {
        let mut discovery = AgentDiscovery::default();
        for scope in [DefinitionScope::Repo, DefinitionScope::Global] {
            if let Some(dir) = self.dir(scope) {
                discover_dir(&dir, scope, &mut discovery);
            }
        }
        discovery
    }
}

fn already_exists(path: &Path) -> DataError {
    DataError::Other(format!(
        "{} already exists; pass --force to overwrite it",
        path.display()
    ))
}

fn not_a_directory(path: &Path) -> DataError {
    DataError::Other(format!(
        "{} is a symlink or not a directory; refusing to write agent definitions through it",
        path.display()
    ))
}

/// Descriptor-relative publication: every directory below `base` is opened
/// with `O_NOFOLLOW`, and files are created, linked and renamed relative to
/// that descriptor, so a concurrent symlink swap cannot redirect a write.
#[cfg(unix)]
fn publish_files(
    base: &Path,
    components: &[&str],
    files: &[(&str, &[u8])],
    force: bool,
) -> Result<(), DataError> {
    use nix::errno::Errno;
    use nix::fcntl::{openat, renameat, AtFlags, OFlag, AT_FDCWD};
    use nix::sys::stat::{fstatat, mkdirat, Mode};
    use nix::unistd::{linkat, unlinkat, UnlinkatFlags};
    use std::io::Write;

    let io = |path: &Path, e: Errno| DataError::io(path, std::io::Error::from(e));
    let read_dir = OFlag::O_DIRECTORY | OFlag::O_RDONLY | OFlag::O_CLOEXEC;
    let mut path = base.to_path_buf();
    let mut dir = openat(AT_FDCWD, base, read_dir, Mode::empty()).map_err(|e| io(base, e))?;
    for component in components {
        path.push(component);
        match mkdirat(&dir, *component, Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => return Err(io(&path, e)),
        }
        dir = openat(
            &dir,
            *component,
            read_dir | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| match e {
            Errno::ELOOP | Errno::ENOTDIR => not_a_directory(&path),
            e => io(&path, e),
        })?;
    }
    if !force {
        for (name, _) in files {
            match fstatat(&dir, *name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(_) => return Err(already_exists(&path.join(name))),
                Err(Errno::ENOENT) => {}
                Err(e) => return Err(io(&path.join(name), e)),
            }
        }
    }
    let mut created: Vec<&str> = Vec::new();
    let result = (|| {
        for (name, body) in files {
            let target = path.join(name);
            let tmp = format!(".{name}.awman-tmp-{}", std::process::id());
            let fd = openat(
                &dir,
                tmp.as_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o644),
            )
            .map_err(|e| io(&path.join(&tmp), e))?;
            let mut file = std::fs::File::from(fd);
            let written = file
                .write_all(body)
                .and_then(|()| file.sync_all())
                .map_err(|e| DataError::io(&target, e));
            let published = written.and_then(|()| {
                if force {
                    renameat(&dir, tmp.as_str(), &dir, *name).map_err(|e| io(&target, e))
                } else {
                    // link(2) never replaces an entry, symlink or not.
                    linkat(&dir, tmp.as_str(), &dir, *name, AtFlags::empty()).map_err(|e| match e {
                        Errno::EEXIST => already_exists(&target),
                        e => io(&target, e),
                    })
                }
            });
            let _ = unlinkat(&dir, tmp.as_str(), UnlinkatFlags::NoRemoveDir);
            published?;
            created.push(name);
        }
        Ok(())
    })();
    if result.is_err() && !force {
        for name in created {
            let _ = unlinkat(&dir, name, UnlinkatFlags::NoRemoveDir);
        }
    }
    result
}

/// Path-based fallback: refuses symlinked directories and existing entries,
/// then writes each file atomically.
#[cfg(not(unix))]
fn publish_files(
    base: &Path,
    components: &[&str],
    files: &[(&str, &[u8])],
    force: bool,
) -> Result<(), DataError> {
    use std::io::Write;
    let mut path = base.to_path_buf();
    for component in components {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err(not_a_directory(&path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&path).map_err(|e| DataError::io(&path, e))?
            }
            Err(e) => return Err(DataError::io(&path, e)),
        }
    }
    if !force {
        for (name, _) in files {
            if std::fs::symlink_metadata(path.join(name)).is_ok() {
                return Err(already_exists(&path.join(name)));
            }
        }
    }
    for (name, body) in files {
        let target = path.join(name);
        let mut tmp =
            tempfile::NamedTempFile::new_in(&path).map_err(|e| DataError::io(&path, e))?;
        tmp.write_all(body).map_err(|e| DataError::io(&target, e))?;
        if force {
            tmp.persist(&target)
                .map_err(|e| DataError::io(&target, e.error))?;
        } else {
            tmp.persist_noclobber(&target)
                .map_err(|_| already_exists(&target))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDefinition {
    pub name: String,
    pub scope: DefinitionScope,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDefinition {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDiscovery {
    pub found: Vec<DiscoveredDefinition>,
    pub skipped: Vec<SkippedDefinition>,
}

impl AgentDiscovery {
    pub fn lookup(&self, name: &str) -> Option<&DiscoveredDefinition> {
        self.found.iter().find(|definition| definition.name == name)
    }
}

fn discover_dir(dir: &Path, scope: DefinitionScope, discovery: &mut AgentDiscovery) {
    let skip = |path: PathBuf, reason: String| SkippedDefinition { path, reason };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            discovery
                .skipped
                .push(skip(dir.to_path_buf(), e.to_string()));
            return;
        }
    };
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(e) => {
            discovery
                .skipped
                .push(skip(dir.to_path_buf(), e.to_string()));
            return;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => discovery
                .skipped
                .push(skip(dir.to_path_buf(), e.to_string())),
        }
    }
    // Order by agent name (the stem), not the whole file name: `inside.toml`
    // must precede `inside-source.toml` even though `-` sorts before `.`.
    paths.sort_by(|a, b| a.file_stem().cmp(&b.file_stem()).then_with(|| a.cmp(b)));
    for path in paths {
        if path.extension().and_then(|ext| ext.to_str()) != Some(AGENT_DEFINITION_EXT) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) => {
                discovery.skipped.push(skip(path, e.to_string()));
                continue;
            }
        };
        if metadata.is_dir() {
            continue;
        }
        if metadata.file_type().is_symlink() {
            match std::fs::canonicalize(&path) {
                Ok(target) if !target.starts_with(&canonical_dir) => {
                    let reason = format!(
                        "{}: symlink points outside {}",
                        path.display(),
                        dir.display()
                    );
                    discovery.skipped.push(skip(path, reason));
                    continue;
                }
                Ok(target) => match std::fs::metadata(&target) {
                    Ok(metadata) if metadata.is_dir() => continue,
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => {
                        discovery
                            .skipped
                            .push(skip(path, "not a regular file".into()));
                        continue;
                    }
                    Err(e) => {
                        discovery.skipped.push(skip(path, e.to_string()));
                        continue;
                    }
                },
                Err(e) => {
                    discovery.skipped.push(skip(path, e.to_string()));
                    continue;
                }
            }
        } else if !metadata.is_file() {
            discovery
                .skipped
                .push(skip(path, "not a regular file".into()));
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if let Err(reason) = validate_agent_file_stem(stem) {
            discovery.skipped.push(skip(path, reason));
            continue;
        }
        if is_first_party_name(stem) {
            let reason =
                format!("'{stem}' is a first-party agent; custom definitions cannot replace it");
            discovery.skipped.push(skip(path, reason));
            continue;
        }
        // Opening checks readability without parsing or allocating the content.
        if let Err(e) = std::fs::File::open(&path) {
            discovery.skipped.push(skip(path, e.to_string()));
            continue;
        }
        discovery.found.push(DiscoveredDefinition {
            name: stem.to_string(),
            scope,
            path,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::AWMAN_CONFIG_HOME;

    #[test]
    fn discover_repo_before_global() {
        let tmp = tempfile::tempdir().unwrap();
        let global_home = tmp.path().join("global");
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, global_home.to_str().unwrap())]);
        let dirs = AgentDirs::from_env(&env, Some(tmp.path().join("repo"))).unwrap();
        for scope in [DefinitionScope::Global, DefinitionScope::Repo] {
            let dir = dirs.ensure_dir(scope).unwrap();
            std::fs::write(dir.join("pi.toml"), "not parsed at discovery").unwrap();
        }
        let found = dirs.discover();
        assert_eq!(found.found.len(), 2);
        assert!(found.skipped.is_empty());
        assert_eq!(found.lookup("pi").unwrap().scope, DefinitionScope::Repo);
        assert_eq!(dirs.global_dir(), global_home.join("agents"));
    }

    #[test]
    fn discovery_orders_entries_and_reports_invalid_and_reserved_stems() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(
            AWMAN_CONFIG_HOME,
            tmp.path().join("home").to_str().unwrap(),
        )]);
        let dirs = AgentDirs::from_env(&env, Some(tmp.path().join("repo"))).unwrap();
        let repo = dirs.ensure_dir(DefinitionScope::Repo).unwrap();
        let global = dirs.ensure_dir(DefinitionScope::Global).unwrap();
        for name in ["zeta", "alpha", "claude", "Bad", "_hidden"] {
            std::fs::write(repo.join(format!("{name}.toml")), "not parsed").unwrap();
        }
        std::fs::write(repo.join("ignored.TOML"), "ignored").unwrap();
        std::fs::write(repo.join("notes.txt"), "ignored").unwrap();
        std::fs::create_dir(repo.join("directory.toml")).unwrap();
        std::fs::write(global.join("beta.toml"), "not parsed").unwrap();
        let result = dirs.discover();
        assert_eq!(
            result
                .found
                .iter()
                .map(|d| (d.scope, d.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (DefinitionScope::Repo, "alpha"),
                (DefinitionScope::Repo, "zeta"),
                (DefinitionScope::Global, "beta")
            ]
        );
        assert_eq!(result.skipped.len(), 3);
        assert!(result
            .skipped
            .iter()
            .any(|s| s.reason.contains("first-party agent")));
        assert!(result
            .skipped
            .iter()
            .filter(|s| s.path.file_name().unwrap() == "Bad.toml"
                || s.path.file_name().unwrap() == "_hidden.toml")
            .all(|s| s.reason.contains("must match")));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_symlink_escape_but_allows_in_directory_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(
            AWMAN_CONFIG_HOME,
            tmp.path().join("home").to_str().unwrap(),
        )]);
        let dirs = AgentDirs::from_env(&env, Some(tmp.path().join("repo"))).unwrap();
        let repo = dirs.ensure_dir(DefinitionScope::Repo).unwrap();
        let elsewhere = tmp.path().join("outside.toml");
        std::fs::write(&elsewhere, "not parsed").unwrap();
        symlink(&elsewhere, repo.join("escape.toml")).unwrap();
        std::fs::write(repo.join("inside-source.toml"), "not parsed").unwrap();
        symlink(repo.join("inside-source.toml"), repo.join("inside.toml")).unwrap();
        let found = dirs.discover();
        assert_eq!(
            found
                .found
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["inside", "inside-source"]
        );
        assert_eq!(found.skipped.len(), 1);
        assert!(found.skipped[0].reason.contains("symlink points outside"));
    }
}
