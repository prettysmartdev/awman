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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::ImageAcquisition;

    fn sandbox_caps() -> Capabilities {
        Capabilities {
            arbitrary_host_mounts: false,
            kit_declarative: true,
            has_image_store: false,
            image_acquisition: ImageAcquisition::Kit,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        }
    }

    #[test]
    fn read_only_unsupported_runtime_is_ineligible() {
        let mut item = awman_wsp::conformance::work_item("01J");
        item.exec.read_only = true;
        let sandbox = LeaseRuntimeSupport::from_capabilities(&sandbox_caps());
        assert!(sandbox
            .check(&item, "sbx")
            .unwrap_err()
            .0
            .contains("read-only"));
        assert!(sandbox.features().is_empty());
        let docker = crate::engine::container::ContainerRuntime::docker();
        let docker = LeaseRuntimeSupport::from_capabilities(docker.capabilities());
        assert!(docker.check(&item, "docker").is_ok());
        assert_eq!(docker.features(), vec!["lease_read_only", "outputs"]);
        item.exec.read_only = false;
        item.outputs = Some(awman_wsp::Outputs { max_bytes: 10 });
        assert!(sandbox
            .check(&item, "sbx")
            .unwrap_err()
            .0
            .contains("/awman/output"));
    }

    #[test]
    fn lease_workspace_is_owner_only_holds_a_private_inline_body_and_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let work_root = root.path().join("worker");
        let lease = LeaseId::new();
        let ws = LeaseWorkspace::prepare(&work_root, &lease).unwrap();
        assert_eq!(
            ws.clone_dir(),
            work_root.join("leases").join(lease.as_str())
        );
        assert!(!ws.private_dir().starts_with(ws.clone_dir()));
        let inline = ws
            .inline_work_item("01J/../x", "# Work Item\nbody")
            .unwrap();
        assert!(
            !inline.path().starts_with(ws.clone_dir()),
            "never inside the clone"
        );
        assert_eq!(
            std::fs::read_to_string(inline.path()).unwrap(),
            "# Work Item\nbody"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(inline.path()), 0o600);
            assert_eq!(mode(inline.path().parent().unwrap()), 0o700);
            assert_eq!(mode(ws.clone_dir()), 0o700);
            assert_eq!(mode(&work_root.join("leases")), 0o700);
        }
        assert!(
            LeaseWorkspace::prepare(&work_root, &lease).is_err(),
            "lease dirs are fresh"
        );
        let (clone, private) = (ws.clone_dir().to_path_buf(), ws.private_dir().to_path_buf());
        drop(inline);
        ws.remove().unwrap();
        assert!(!clone.exists() && !private.exists());
    }

    #[test]
    fn orphaned_lease_dirs_excludes_active_leases() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("w");
        let root = root.as_path();
        let (a, b) = (LeaseId::new(), LeaseId::new());
        let keep = LeaseWorkspace::prepare(root, &a).unwrap();
        let mut orphan = LeaseWorkspace::prepare(root, &b).unwrap();
        orphan.removed = true; // simulate process exit: keep dirs, release lock
        drop(orphan);
        std::fs::create_dir_all(root.join("leases").join("not-a-lease")).unwrap();
        let found = orphaned_lease_dirs(root, std::slice::from_ref(&a));
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.ends_with(b.as_str())));
        assert_eq!(remove_orphaned_lease_dirs(root, &[a]), 2);
        assert!(keep.clone_dir().exists());
        assert!(orphaned_lease_dirs(root, &[]).len() == 2);
        assert_eq!(
            remove_orphaned_lease_dirs(root, &[]),
            0,
            "cleanup must preserve a live owner absent from its snapshot"
        );
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
        // Setup fetch: any merge ref the clone did not bring (it fetches all
        // heads, so this only runs for refs created since).
        let missing: Vec<&String> = repo
            .merge_refs
            .iter()
            .filter(|r| self.git.remote_ref_sha(dir, "origin", r).is_none())
            .collect();
        if !missing.is_empty() {
            // Merge refs the clone did not bring: ask the remote (merge-ref
            // `ls-remote`, same lease transport) and fetch only branches.
            let advertised =
                self.git
                    .ls_remote_with_lease_transport(&repo.url, self.transport, sink, cancel)?;
            let mut refspecs = Vec::new();
            for name in missing {
                if advertised.contains_key(&format!("refs/heads/{name}")) {
                    refspecs.push(format!("+refs/heads/{name}:refs/remotes/origin/{name}"));
                } else if advertised.contains_key(&format!("refs/tags/{name}")) {
                    return Err(crate::engine::error::EngineError::Git(format!(
                        "merge ref '{name}' is a tag on origin; merge_refs must name branches"
                    )));
                } else {
                    return Err(crate::engine::error::EngineError::Git(format!(
                        "merge ref '{name}' does not exist on origin"
                    )));
                }
            }
            self.git
                .fetch_with_lease_transport(dir, &refspecs, self.transport, sink, cancel)?;
        }
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

#[cfg(test)]
mod checkout_tests {
    use super::*;
    use crate::engine::git::lease_transport::{LeaseTransport, LeaseUrlPolicy};
    use crate::engine::git::{GitEngine, MergeRefOutcome};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn real_git_lease_checkout_cuts_branch_from_base_ref_and_merges_merge_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q", "-b", "main"]);
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("a"), "a").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "a"]);
        git(&seed, &["checkout", "-qb", "wsp/dep"]);
        std::fs::write(seed.join("dep"), "dep").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "dep"]);
        git(
            &seed,
            &["push", "-q", bare.to_str().unwrap(), "main", "wsp/dep"],
        );

        let work_root = tmp.path().join("worker");
        let ws = LeaseWorkspace::prepare(&work_root, &LeaseId::new()).unwrap();
        let repo = awman_wsp::Repo {
            url: bare.to_str().unwrap().into(),
            base_ref: "main".into(),
            branch: "wsp/item".into(),
            merge_refs: vec!["wsp/dep".into()],
            credentials: None,
        };
        let engine = GitEngine::new();
        let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let outcome = LeaseCheckout::new(&engine, &transport)
            .run(
                &ws,
                &repo,
                &mut sink,
                &tokio_util::sync::CancellationToken::new(),
            )
            .unwrap();
        assert!(!outcome.resumed_branch);
        assert!(outcome.start_point.is_some());
        assert!(matches!(
            outcome.merged[0].1,
            MergeRefOutcome::Merged { .. }
        ));
        assert_eq!(
            git(ws.clone_dir(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "wsp/item"
        );
        assert!(ws.clone_dir().join("dep").exists(), "merge ref merged");
        assert_eq!(outcome.head, git(ws.clone_dir(), &["rev-parse", "HEAD"]));
        let logged = format!("{sink:?}");
        assert!(
            logged.contains("lease branch wsp/item created from")
                && logged.contains("merged wsp/dep"),
            "{logged}"
        );
    }

    #[test]
    fn real_git_lease_checkout_resolves_missing_merge_refs_with_ls_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "-q", "-b", "main"]);
        let seed = tmp.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("a"), "a").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "a"]);
        git(&seed, &["tag", "v1"]);
        git(&seed, &["push", "-q", bare.to_str().unwrap(), "main", "v1"]);
        let engine = GitEngine::new();
        let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
        for (merge_ref, expected) in [
            ("v1", "is a tag on origin"),
            ("wsp/missing", "does not exist on origin"),
        ] {
            let ws = LeaseWorkspace::prepare(&tmp.path().join("worker"), &LeaseId::new()).unwrap();
            let repo = awman_wsp::Repo {
                url: bare.to_str().unwrap().into(),
                base_ref: "main".into(),
                branch: "wsp/item".into(),
                merge_refs: vec![merge_ref.into()],
                credentials: None,
            };
            let mut sink = crate::data::message::RecordingMessageSink::new();
            let error = LeaseCheckout::new(&engine, &transport)
                .run(
                    &ws,
                    &repo,
                    &mut sink,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{merge_ref}: {error}");
            assert!(
                format!("{sink:?}").contains("ls-remote"),
                "the remote was asked through the lease transport"
            );
        }
    }
}
