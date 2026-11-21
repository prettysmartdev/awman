//! First-party host pings may execute only canonical installed programs outside
//! the invoking checkout and its linked worktrees. This is deliberately separate
//! from both general host CLI lookup and explicitly approved custom pings.

use std::path::{Path, PathBuf};

use crate::data::config::env::EnvSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResolveError {
    NotInstalled,
    InsideCheckout,
    CouldNotValidate,
}

pub(super) fn resolve(
    env: &EnvSnapshot,
    name: &str,
    checkout_roots: &[PathBuf],
) -> Result<PathBuf, ResolveError> {
    // This API accepts only first-party executable names, never custom argv.
    if !crate::data::config::agent_definition::is_bare_host_binary(name) {
        return Err(ResolveError::CouldNotValidate);
    }
    let excluded =
        exclusions(env.cwd(), checkout_roots).map_err(|_| ResolveError::CouldNotValidate)?;
    if !checkout_roots.is_empty() && excluded.is_empty() {
        return Err(ResolveError::CouldNotValidate);
    }
    let mut refused = false;
    for dir in env.path_dirs().into_iter().filter(|p| p.is_absolute()) {
        // One absolute PATH entry at a time: which still supplies platform
        // executable/PATHEXT checks, but cannot search cwd or a relative entry.
        let Some(candidate) = crate::engine::host_cli::find_program(env, name, vec![dir]) else {
            continue;
        };
        let candidate = candidate
            .canonicalize()
            .map_err(|_| ResolveError::CouldNotValidate)?;
        if excluded.iter().any(|root| candidate.starts_with(root)) {
            refused = true;
            continue;
        }
        // A separate Git directory need not be named `.git`, so a linked
        // checkout's metadata alone cannot reveal its main working tree.
        // Compare inert checkout metadata at the candidate too: sharing a
        // canonical Git directory is sufficient to refuse, never to trust.
        let parent = candidate.parent().ok_or(ResolveError::CouldNotValidate)?;
        let candidate_roots =
            exclusions(parent, &[]).map_err(|_| ResolveError::CouldNotValidate)?;
        if candidate_roots.iter().any(|root| excluded.contains(root)) {
            refused = true;
            continue;
        }
        return Ok(candidate);
    }
    Err(if refused {
        ResolveError::InsideCheckout
    } else {
        ResolveError::NotInstalled
    })
}

pub(crate) fn exclusions(cwd: &Path, checkout_roots: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    // An explicit session supplies its own workdir and definition/Git roots;
    // a daemon's invocation cwd may be unrelated (or absent). Without session
    // roots, the snapshot cwd is the invoking checkout context.
    let paths = std::iter::once(cwd)
        .filter(|p| checkout_roots.is_empty() && !p.as_os_str().is_empty())
        .chain(checkout_roots.iter().map(PathBuf::as_path));
    for path in paths {
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(error)
                if !checkout_roots.is_empty() && error.kind() == std::io::ErrorKind::NotFound =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if !checkout_roots.is_empty() {
            roots.push(path.clone());
        }
        // Inspect inert filesystem metadata only. Running Git here would add
        // an unnecessary host command and repository configuration boundary.
        for ancestor in path.ancestors() {
            let marker = ancestor.join(".git");
            match std::fs::symlink_metadata(&marker) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            roots.push(ancestor.to_path_buf());
            let git_dir = if marker.is_dir() {
                marker.canonicalize()?
            } else {
                let pointer = std::fs::read_to_string(&marker)?;
                let target = pointer.trim().strip_prefix("gitdir: ").ok_or_else(|| {
                    std::io::Error::other("invalid checkout Git directory pointer")
                })?;
                ancestor.join(target).canonicalize()?
            };
            let common = match std::fs::read_to_string(git_dir.join("commondir")) {
                Ok(pointer) => git_dir.join(pointer.trim()).canonicalize()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
                Err(e) => return Err(e),
            };
            roots.push(git_dir);
            roots.push(common.clone());
            if common.file_name().is_some_and(|name| name == ".git") {
                roots.push(common.parent().unwrap().to_path_buf());
            }
            let worktrees = match std::fs::read_dir(common.join("worktrees")) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e),
            };
            for entry in worktrees {
                let entry = entry?;
                let pointer = std::fs::read_to_string(entry.path().join("gitdir"))?;
                let marker = entry.path().join(pointer.trim());
                if marker.file_name().is_none_or(|n| n != ".git") {
                    return Err(std::io::Error::other(
                        "invalid worktree Git directory pointer",
                    ));
                }
                // Stale registrations cannot supply executable candidates,
                // but an existing directory must be excluded even if its
                // administrative backlink has since been removed.
                match marker.parent().unwrap().canonicalize() {
                    Ok(root) => roots.push(root),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            break; // Stop at the nearest repository, not outer dotfiles repositories.
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Interpreters selected by env must obey the same boundary as the agent.
pub(super) fn child_path(
    env: &EnvSnapshot,
    checkout_roots: &[PathBuf],
) -> Result<std::ffi::OsString, ResolveError> {
    let excluded =
        exclusions(env.cwd(), checkout_roots).map_err(|_| ResolveError::CouldNotValidate)?;
    let mut dirs = Vec::new();
    for dir in env.path_dirs().into_iter().filter(|p| p.is_absolute()) {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if excluded.iter().any(|root| dir.starts_with(root)) {
            continue;
        }
        let metadata = exclusions(&dir, &[]).map_err(|_| ResolveError::CouldNotValidate)?;
        if metadata.iter().any(|root| excluded.contains(root)) {
            continue;
        }
        dirs.push(dir);
    }
    std::env::join_paths(dirs).map_err(|_| ResolveError::CouldNotValidate)
}
