//! Filesystem locations of pulled-lease workspaces and remote URL matching.

use std::path::{Path, PathBuf};

use crate::data::config::daemon::WorkerSettings;
use crate::data::fs::data_paths::DataPaths;
use crate::data::ids::LeaseId;

/// `daemon.features.worker.pull.workRoot`, or `<data>/worker` when unset.
pub fn work_root(paths: &DataPaths, settings: &WorkerSettings) -> PathBuf {
    match settings.pull_work_root.as_deref().map(str::trim) {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => paths.root().join("worker"),
    }
}

/// `<work_root>/leases/`.
pub fn leases_dir(work_root: &Path) -> PathBuf {
    work_root.join("leases")
}

/// `<work_root>/leases/<lease_id>/`.
pub fn lease_dir(work_root: &Path, lease: &LeaseId) -> PathBuf {
    leases_dir(work_root).join(lease.as_str())
}

/// A lease owner's cross-process liveness lock. Lock files are never unlinked:
/// replacing their inode would let two owners hold different locks.
pub fn try_lease_owner(
    root: &Path,
    lease: &LeaseId,
) -> Result<Option<std::fs::File>, crate::data::error::DataError> {
    let dir = root.join("lease-owners");
    crate::data::fs::private_files::create_private_dir(&dir)?;
    crate::data::fs::private_files::lock_private_timeout(
        &dir.join(lease.as_str()),
        crate::data::fs::private_files::LockMode::Exclusive,
        std::time::Duration::ZERO,
        || Ok(()),
    )
}

/// Used by command::worker TUI/CLI hosting to offer the current checkout.
/// Normalised `host/path` of a Git remote URL: credentials, scheme, port of
/// the default transport, `.git` and trailing slashes removed; scp-style
/// `git@host:owner/repo` maps to `host/owner/repo`; the host is lowercased.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        let (authority, path) = rest.split_once('/')?;
        let host = authority.split(':').next()?;
        (host.to_string(), path.to_string())
    } else if let Some((user_host, path)) = url.split_once(':') {
        if user_host.contains('/') || path.starts_with("//") {
            return None;
        }
        let host = user_host.rsplit_once('@').map_or(user_host, |(_, h)| h);
        (host.to_string(), path.to_string())
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host.to_ascii_lowercase(), path))
}

/// Whether two remote URLs name the same repository.
pub fn same_remote(a: &str, b: &str) -> bool {
    matches!((normalize_remote(a), normalize_remote(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_root_defaults_to_data_worker_and_honours_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::at_root(dir.path());
        let mut settings = WorkerSettings {
            enabled: true,
            max_concurrent_jobs: 1,
            push_enabled: true,
            push_interaction: Default::default(),
            push_allow_override: true,
            push_work_dirs: vec![],
            push_work_dir_roots: vec![],
            pull_enabled: true,
            pull_interaction: Default::default(),
            pull_allow_override: true,
            pull_work_root: None,
            work_servers: Default::default(),
        };
        assert_eq!(work_root(&paths, &settings), dir.path().join("worker"));
        settings.pull_work_root = Some("/srv/awman-work".into());
        assert_eq!(
            work_root(&paths, &settings),
            PathBuf::from("/srv/awman-work")
        );
        let lease = LeaseId::new();
        assert_eq!(
            lease_dir(Path::new("/w"), &lease),
            PathBuf::from(format!("/w/leases/{lease}"))
        );
    }

    #[test]
    fn same_remote_matches_equivalent_spellings_only() {
        let https = "https://GitHub.com/team/app.git";
        assert!(same_remote(https, "git@github.com:team/app"));
        assert!(same_remote(https, "https://token@github.com/team/app/"));
        assert!(same_remote(https, "ssh://git@github.com:22/team/app.git"));
        assert!(!same_remote(https, "https://gitlab.com/team/app.git"));
        assert!(!same_remote(https, "https://github.com/team/app2.git"));
        assert!(!same_remote(https, "/local/path/app"));
        assert!(!same_remote("", ""));
    }
}
