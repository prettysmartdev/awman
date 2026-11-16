//! Lease workspaces under `<workRoot>/leases/<lease_id>/`, the private
//! inline work-item source, and what a runtime can enforce for a lease.
//!
//! Layout under the work root (all owner-only, 0700/0600):
//!
//! ```text
//! <workRoot>/leases/<lease_id>/      the fresh clone (mounted into the run)
//! <workRoot>/private/<lease_id>/     never mounted: credential helper dir,
//!                                    inline work-item body
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use super::error::WorkerEngineError;
use crate::data::ids::LeaseId;
use crate::data::worker::paths::{lease_dir, leases_dir};
use crate::engine::agent_runtime::Capabilities;

fn io(path: &Path, e: impl std::fmt::Display) -> WorkerEngineError {
    WorkerEngineError::Io(format!("{}: {e}", path.display()))
}

fn private_dir(path: &Path) -> Result<(), WorkerEngineError> {
    crate::data::fs::private_files::create_private_dir(path).map_err(|e| io(path, e))
}

/// One lease's workspace. Removed by [`LeaseWorkspace::remove`] (or on drop).
#[derive(Debug)]
pub struct LeaseWorkspace {
    lease: LeaseId,
    clone_dir: PathBuf,
    private_dir: PathBuf,
    removed: bool,
    _owner: std::fs::File,
}

impl LeaseWorkspace {
    /// Create `<work_root>/leases/<lease>/` (empty, 0700) and its private
    /// sibling. An existing lease directory is refused: lease IDs are fresh.
    pub fn prepare(work_root: &Path, lease: &LeaseId) -> Result<Self, WorkerEngineError> {
        private_dir(work_root)?;
        let owner = crate::data::worker::paths::try_lease_owner(work_root, lease)
            .map_err(|e| io(work_root, e))?
            .ok_or_else(|| io(work_root, "lease workspace has a live owner"))?;
        private_dir(&leases_dir(work_root))?;
        private_dir(&work_root.join("private"))?;
        let clone_dir = lease_dir(work_root, lease);
        let private = work_root.join("private").join(lease.as_str());
        crate::data::fs::private_files::create_private_dir_new(&clone_dir)
            .map_err(|e| io(&clone_dir, e))?;
        if let Err(e) = crate::data::fs::private_files::create_private_dir_new(&private) {
            let _ = std::fs::remove_dir_all(&clone_dir);
            return Err(io(&private, e));
        }
        Ok(Self {
            lease: lease.clone(),
            clone_dir,
            private_dir: private,
            removed: false,
            _owner: owner,
        })
    }

    pub fn lease(&self) -> &LeaseId {
        &self.lease
    }

    /// Where the repository is cloned (empty until the clone runs).
    pub fn clone_dir(&self) -> &Path {
        &self.clone_dir
    }

    /// Owner-only directory outside every mount (credential helper parent).
    pub fn private_dir(&self) -> &Path {
        &self.private_dir
    }

    /// Write the item `body` into a private, owner-only temporary source
    /// outside the clone. It is never committed; it goes with the workspace.
    pub fn inline_work_item(
        &self,
        item_id: &str,
        body: &str,
    ) -> Result<InlineWorkItem, WorkerEngineError> {
        let dir = tempfile::Builder::new()
            .prefix("inline-")
            .tempdir_in(&self.private_dir)
            .map_err(|e| io(&self.private_dir, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io(dir.path(), e))?;
        }
        let safe: String = item_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let path = dir.path().join(format!("work-item-{safe}.md"));
        let mut file = crate::data::fs::private_files::create_private_file_new(&path)
            .map_err(|e| io(&path, e))?;
        file.write_all(body.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| io(&path, e))?;
        Ok(InlineWorkItem { _dir: dir, path })
    }

    /// Delete the clone and the private directory.
    pub fn remove(mut self) -> Result<(), WorkerEngineError> {
        self.removed = true;
        remove_tree(&self.clone_dir)?;
        remove_tree(&self.private_dir)
    }
}

impl Drop for LeaseWorkspace {
    fn drop(&mut self) {
        if !self.removed {
            let _ = remove_tree(&self.clone_dir);
            let _ = remove_tree(&self.private_dir);
        }
    }
}

fn remove_tree(path: &Path) -> Result<(), WorkerEngineError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
    }
}

/// The private inline source of a lease's work item. Removed on drop.
#[derive(Debug)]
pub struct InlineWorkItem {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl InlineWorkItem {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Lease directories left by an earlier process that are not in `active`
/// (start-up cleanup), both clones and private siblings.
pub fn orphaned_lease_dirs(work_root: &Path, active: &[LeaseId]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for parent in [leases_dir(work_root), work_root.join("private")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(id) = LeaseId::parse(name) else {
                continue;
            };
            if !active.contains(&id) && entry.file_type().is_ok_and(|t| t.is_dir()) {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// Remove [`orphaned_lease_dirs`]; returns how many were removed.
pub fn remove_orphaned_lease_dirs(work_root: &Path, active: &[LeaseId]) -> usize {
    orphaned_lease_dirs(work_root, active)
        .into_iter()
        .filter(|dir| {
            let Some(id) = dir
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| LeaseId::parse(s).ok())
            else {
                return false;
            };
            // A foreground owner may have begun after the recovery snapshot.
            let Ok(Some(_owner)) = crate::data::worker::paths::try_lease_owner(work_root, &id)
            else {
                return false;
            };
            remove_tree(dir).is_ok()
        })
        .count()
}

/// What the run's runtime can honour for a lease, read off its capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRuntimeSupport {
    /// `exec.read_only` (`lease_read_only`).
    pub read_only_workspace: bool,
    /// `/awman/output` is mounted (`outputs`).
    pub outputs_mount: bool,
}

/// Why a lease was refused before any work started (`release(ineligible)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ineligible(pub String);

impl LeaseRuntimeSupport {
    pub fn from_capabilities(caps: &Capabilities) -> Self {
        Self {
            read_only_workspace: caps.read_only_workspace(),
            outputs_mount: caps.mounts_outputs(true),
        }
    }

    /// Fail closed on every item requirement this runtime cannot honour.
    pub fn check(&self, item: &awman_wsp::WorkItem, runtime: &str) -> Result<(), Ineligible> {
        if item.exec.read_only && !self.read_only_workspace {
            return Err(Ineligible(format!(
                "the {runtime} runtime cannot mount the workspace read-only (exec.read_only)"
            )));
        }
        if item.outputs.is_some() && !self.outputs_mount {
            return Err(Ineligible(format!(
                "the {runtime} runtime cannot mount /awman/output (outputs)"
            )));
        }
        Ok(())
    }

    /// The WSP registration features this runtime support adds.
    pub fn features(&self) -> Vec<&'static str> {
        let mut features = Vec::new();
        if self.read_only_workspace {
            features.push(awman_wsp::features::LEASE_READ_ONLY);
        }
        if self.outputs_mount {
            features.push(awman_wsp::features::OUTPUTS);
        }
        features
    }
}

/// What [`LeaseCheckout::run`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutOutcome {
    /// `true` when `branch` already existed on the remote and was checked out.
    pub resumed_branch: bool,
    /// The start point `branch` was cut from (when it was created).
    pub start_point: Option<String>,
    pub merged: Vec<(String, crate::engine::git::MergeRefOutcome)>,
    pub head: String,
}

/// Prepare a lease's fresh clone with 0133's Git machinery, every network
/// operation going through the one lease transport: clone `repo.url`, check
/// out `repo.branch` (the remote branch when it exists, else cut from
/// `repo.base_ref`), then merge each `repo.merge_refs` entry.
pub struct LeaseCheckout<'a> {
    git: &'a crate::engine::git::GitEngine,
    transport: &'a crate::engine::git::lease_transport::LeaseTransport,
}

impl<'a> LeaseCheckout<'a> {
    pub fn new(
        git: &'a crate::engine::git::GitEngine,
        transport: &'a crate::engine::git::lease_transport::LeaseTransport,
    ) -> Self {
        Self { git, transport }
    }

    pub fn run(
        &self,
        workspace: &LeaseWorkspace,
        repo: &awman_wsp::Repo,
        sink: &mut dyn crate::engine::git::GitFrontend,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<CheckoutOutcome, crate::engine::error::EngineError> {
        let dir = workspace.clone_dir();
        self.git
            .clone_with_lease_transport(&repo.url, None, dir, self.transport, sink, cancel)?;
        self.git.fetch_missing_merge_refs(
            dir,
            &repo.url,
            &repo.merge_refs,
            &self.transport.host_transport(),
            sink,
            &crate::engine::git::transport::TransportGuard::new(cancel.clone()),
        )?;
        let (resumed_branch, start_point) = if self
            .git
            .remote_ref_sha(dir, "origin", &repo.branch)
            .is_some()
        {
            self.git
                .checkout_remote_branch_logged(dir, &repo.branch, sink)?;
            (true, None)
        } else {
            let start = self
                .git
                .resolve_base_ref_logged(dir, &repo.base_ref, sink)?;
            self.git
                .create_branch_from_logged(dir, &repo.branch, &start, sink)?;
            (false, Some(start))
        };
        let identity = self.git.commit_identity(dir)?;
        let mut merged = Vec::new();
        for merge_ref in &repo.merge_refs {
            merged.push((
                merge_ref.clone(),
                self.git.merge_ref_logged(dir, merge_ref, &identity, sink)?,
            ));
        }
        let outcome = CheckoutOutcome {
            resumed_branch,
            start_point,
            merged,
            head: self.git.head_sha(dir)?,
        };
        sink.write_message(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Info,
            text: outcome.summary(&repo.branch),
        });
        Ok(outcome)
    }
}

impl CheckoutOutcome {
    /// One log line describing the checkout (written to the run's sink).
    pub fn summary(&self, branch: &str) -> String {
        let from = if self.resumed_branch {
            "resumed from origin".to_string()
        } else {
            format!(
                "created from {}",
                self.start_point.as_deref().unwrap_or("base_ref")
            )
        };
        let merged = if self.merged.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = self.merged.iter().map(|(name, _)| name.as_str()).collect();
            format!(", merged {}", names.join(", "))
        };
        format!("lease branch {branch} {from}{merged}; head {}", self.head)
    }
}

// Shared by lease and command launches: the workspace access intent is enforced
// after all overlays have been collected, independent of its source.
use crate::engine::container::options::ContainerOption;
use crate::engine::error::EngineError;

pub(crate) fn read_only_unsupported(runtime: &str) -> EngineError {
    EngineError::Config(format!(
        "the {runtime} runtime cannot mount the workspace read-only; this run requires a read-only workspace"
    ))
}

/// Include linked-worktree administrative state, which can live outside the
/// checkout and is often mounted as a sidecar so Git works in the guest.
fn workspace_git_dirs(workspace: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let dot_git = workspace.join(".git");
    let metadata = match std::fs::metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(EngineError::io(&dot_git, e)),
    };
    let git_dir = if metadata.is_dir() {
        dot_git.clone()
    } else {
        let content =
            std::fs::read_to_string(&dot_git).map_err(|e| EngineError::io(&dot_git, e))?;
        let path = content
            .trim()
            .strip_prefix("gitdir: ")
            .filter(|path| !path.is_empty())
            .ok_or_else(|| EngineError::Config("invalid workspace .git file".into()))?;
        workspace.join(path)
    };
    let git_dir = git_dir
        .canonicalize()
        .map_err(|e| EngineError::io(&git_dir, e))?;
    let common_file = git_dir.join("commondir");
    let mut dirs = vec![git_dir.clone()];
    match std::fs::read_to_string(&common_file) {
        Ok(content) => {
            if content.trim().is_empty() {
                return Err(EngineError::Config(
                    "invalid workspace Git commondir".into(),
                ));
            }
            let common = git_dir.join(content.trim());
            dirs.push(
                common
                    .canonicalize()
                    .map_err(|e| EngineError::io(&common, e))?,
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(EngineError::io(&common_file, e)),
    }
    Ok(dirs)
}

/// Runs after all overlays are assembled and before any runtime sees them.
pub(crate) fn validate_workspace_mounts(
    options: &mut [ContainerOption],
    workspace: &Path,
    read_only: bool,
    lease_scope: Option<&Path>,
    inline_file: Option<&Path>,
    command_mounts: &[crate::engine::container::options::OverlaySpec],
) -> Result<(), EngineError> {
    use crate::engine::container::options::OverlayPermission;
    let workspace = workspace
        .canonicalize()
        .map_err(|e| EngineError::io(workspace, e))?;
    let git_dirs = if read_only {
        workspace_git_dirs(&workspace)?
    } else {
        Vec::new()
    };
    let inline_file = inline_file.and_then(|path| path.canonicalize().ok());
    let lease_root = lease_scope
        .map(|scope| scope.canonicalize())
        .transpose()
        .map_err(|e| EngineError::Config(format!("lease workspace: {e}")))?;
    for option in options {
        let ContainerOption::Overlay(spec) = option else {
            continue;
        };
        let host = spec
            .host_path
            .canonicalize()
            .map_err(|e| EngineError::io(&spec.host_path, e))?;
        if let Some(clone) = &lease_root {
            if !host.starts_with(clone)
                && inline_file.as_deref() != Some(host.as_path())
                && !command_mounts.iter().any(|allowed| {
                    // Compare resolved paths: on macOS a temporary directory is
                    // reached through a symlink (/var -> /private/var).
                    allowed
                        .host_path
                        .canonicalize()
                        .unwrap_or_else(|_| allowed.host_path.clone())
                        == host
                        && allowed.container_path == spec.container_path
                        && allowed.permission == spec.permission
                })
            {
                return Err(EngineError::Config(
                    "overlay exposes a path outside the lease workspace".into(),
                ));
            }
        }
        let guest = &spec.container_path;
        if !guest.is_absolute()
            || guest.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(EngineError::Config(
                "overlay guest path must be normalized".into(),
            ));
        }
        let guest_workspace = Path::new("/workspace");
        if guest_workspace.starts_with(guest) && (guest != guest_workspace || host != workspace) {
            return Err(EngineError::Config(
                "overlay overlaps the workspace mount".into(),
            ));
        }
        if read_only
            && (host.starts_with(&workspace)
                || workspace.starts_with(&host)
                || git_dirs
                    .iter()
                    .any(|dir| host.starts_with(dir) || dir.starts_with(&host))
                || guest.starts_with(guest_workspace))
        {
            spec.permission = OverlayPermission::ReadOnly;
        }
        if inline_file.as_deref() == Some(host.as_path()) {
            spec.permission = OverlayPermission::ReadOnly;
        }
        spec.host_path = host;
    }
    Ok(())
}

#[cfg(test)]
mod workspace_access_tests {
    use super::*;
    use crate::engine::container::options::{OverlayPermission, OverlaySpec};
    use awman_test_support::{TestCase, TestSandbox};

    #[test]
    fn linked_worktree_git_sidecars_are_read_only_and_external_writes_stay_writable() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::worker::lease_workspace::workspace_access_tests::linked_worktree_git_sidecars_are_read_only_and_external_writes_stay_writable"));
        let common = sandbox.root().join("main/.git");
        let admin = common.join("worktrees/review");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(
            sandbox.work().join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let mut options: Vec<_> = [
            (sandbox.work(), "/workspace"),
            (admin, "/awman/git-admin"),
            (common, "/awman/git-common"),
            (sandbox.root().join("main"), "/awman/git-parent"),
            (sandbox.home(), "/awman/output"),
            (sandbox.tmp(), "/awman/context/repo"),
        ]
        .into_iter()
        .map(|(host_path, guest)| {
            ContainerOption::Overlay(OverlaySpec {
                host_path,
                container_path: guest.into(),
                permission: OverlayPermission::ReadWrite,
            })
        })
        .collect();
        validate_workspace_mounts(&mut options, &sandbox.work(), true, None, None, &[]).unwrap();
        let permissions: Vec<_> = options
            .iter()
            .map(|o| match o {
                ContainerOption::Overlay(o) => o.permission,
                _ => panic!("expected overlay"),
            })
            .collect();
        assert_eq!(
            permissions,
            [
                OverlayPermission::ReadOnly,
                OverlayPermission::ReadOnly,
                OverlayPermission::ReadOnly,
                OverlayPermission::ReadOnly,
                OverlayPermission::ReadWrite,
                OverlayPermission::ReadWrite
            ]
        );
        let mut resolved =
            crate::engine::container::options::ResolvedContainerOptions::resolve(options).unwrap();
        resolved.read_only_workspace = true;
        resolved.tmpfs = vec!["/tmp".into()];
        resolved.validate_read_only_workspace().unwrap();
        resolved.overlays[1].permission = OverlayPermission::ReadWrite;
        assert!(resolved.validate_read_only_workspace().is_err());
    }
}
