//! Durable context for one pulled lease (WI 0146).
//!
//! The work server owns the memory. A lease carries a snapshot of it
//! (`WorkItem.context`); this module stages that snapshot into a private
//! directory created for this lease alone (`context/repo/_lease/<lease>/`),
//! takes a start snapshot, and after the run diffs the directory against it
//! so the changes can travel back in the lease result.
//!
//! Safety properties (design §4):
//! - every path is validated (`validate_staged_files`) and the decoded size is
//!   bounded before anything touches the disk; a malformed snapshot leaves
//!   nothing behind;
//! - the directory is created fresh (`create_private_dir_new`), so a lease
//!   never sees another lease's files and never reads or writes a host key;
//! - the end listing never follows a symlink and skips non-regular files;
//!   inlined content is read again through a no-follow handle and capped;
//! - content over the cap is an explicit `context_too_large`, never a silent
//!   drop.
use super::error::WorkerEngineError;
use crate::data::{
    context::{
        staging::validate_staged_files, ChangeKind, ContextChange, ContextLimits, ContextRelPath,
        ContextSnapshot, RepoContextStore, StagingError,
    },
    fs::{
        context_dirs::{validate_context_path, ContextDirResolver},
        safe_tree::{SafeTree, SafeTreeError, TreeBudget},
    },
    ids::LeaseId,
};
use awman_wsp::{codes, ContextChangeItem, ContextChangeKind, ContextChanges};
use base64::Engine as _;
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// The hard ceiling on a pulled snapshot and on returned content, whatever
/// `max_bytes` the server asks for: the durable per-key total.
pub const MAX_LEASE_CONTEXT_BYTES: u64 = ContextLimits::REPO.max_total_bytes;

/// The cap that applies to one lease: `min(max_bytes, 16 MiB)` decoded bytes.
pub fn lease_cap(max_bytes: u64) -> u64 {
    max_bytes.min(MAX_LEASE_CONTEXT_BYTES)
}

fn limits(cap: u64) -> ContextLimits {
    ContextLimits {
        max_file_bytes: cap,
        max_total_bytes: cap,
        max_files: ContextLimits::REPO.max_files,
    }
}

fn invalid(message: impl Into<String>) -> WorkerEngineError {
    WorkerEngineError::Context {
        code: codes::CONTEXT_INVALID,
        message: message.into(),
    }
}

fn too_large(message: impl Into<String>) -> WorkerEngineError {
    WorkerEngineError::Context {
        code: codes::CONTEXT_TOO_LARGE,
        message: message.into(),
    }
}

/// Messages are built from relative paths and numbers only; the host path of
/// the staged directory never reaches the work server.
fn staging_error(error: StagingError) -> WorkerEngineError {
    match error {
        StagingError::TooLarge { .. } | StagingError::TooManyFiles { .. } => {
            too_large(error.to_string())
        }
        other => invalid(other.to_string()),
    }
}

pub struct LeaseContextStage {
    store: RepoContextStore,
    awman_home: PathBuf,
    start: ContextSnapshot,
    max_bytes: u64,
}

impl LeaseContextStage {
    /// Validate (Layer 0), create `context/repo/_lease/<lease>/`, write the
    /// files, snapshot. Any error → `WorkerEngineError::Context` with code
    /// `context_invalid` or `context_too_large`; nothing is left on disk.
    pub fn stage(
        resolver: &ContextDirResolver,
        lease: &LeaseId,
        context: &awman_wsp::ItemContext,
    ) -> Result<Self, WorkerEngineError> {
        let cap = lease_cap(context.max_bytes);
        let limits = limits(cap);
        // Path, encoding, duplicate, clash and decoded-size checks happen
        // before the directory exists.
        let files = validate_staged_files(
            context
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.content_base64.as_str())),
            limits,
        )
        .map_err(staging_error)?;
        let dir = resolver.lease_repo_dir(lease);
        let store = RepoContextStore::open(resolver, dir, limits).map_err(|error| {
            tracing::warn!(%error, "lease context directory is not usable");
            invalid("the worker could not prepare a private context directory")
        })?;
        // `stage_all` creates the directory with `create_private_dir_new`: an
        // existing directory (another lease's, or a planted one) is refused.
        if let Err(error) = store.stage_all(&files) {
            tracing::warn!(%error, "could not stage lease context");
            return Err(invalid(
                "the worker could not stage the context snapshot in a fresh directory",
            ));
        }
        let stage = Self {
            store,
            awman_home: resolver.awman_home().to_path_buf(),
            start: ContextSnapshot::default(),
            max_bytes: cap,
        };
        match stage.list(&AtomicBool::new(false)) {
            Ok(start) => Ok(Self { start, ..stage }),
            Err(error) => {
                let _ = stage.remove();
                Err(error)
            }
        }
    }

    /// The staged directory, mounted as the lease's `/awman/context/repo`.
    /// Never serialised.
    pub fn dir(&self) -> &Path {
        self.store.dir()
    }

    /// The decoded-byte cap for this lease's snapshot and returned content.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Lists the directory with no size caps: an agent may leave a tree over
    /// the cap and the failure report must still describe it. Symlinks and
    /// non-regular files are never listed.
    fn list(&self, cancelled: &AtomicBool) -> Result<ContextSnapshot, WorkerEngineError> {
        validate_context_path(&self.awman_home, self.dir()).map_err(|error| {
            tracing::warn!(%error, "staged lease context directory was replaced");
            invalid("the staged context directory was replaced during the run")
        })?;
        match SafeTree::new(self.dir()).list_report(TreeBudget::default(), cancelled) {
            Ok(listing) if !listing.skipped => Ok(ContextSnapshot::from_files(listing.files)),
            Ok(_) => Err(invalid("the context directory holds an invalid path")),
            Err(SafeTreeError::Incomplete) => Err(too_large(
                "the context directory could not be listed within the worker's limits",
            )),
            Err(error) => {
                tracing::warn!(%error, "could not list staged lease context");
                Err(invalid("the context directory could not be listed"))
            }
        }
    }

    fn diff(&self, cancelled: &AtomicBool) -> Result<Vec<ContextChange>, WorkerEngineError> {
        Ok(self.start.diff(&self.list(cancelled)?))
    }

    /// Diff against the start snapshot and inline content in path order.
    /// `Err(Context{code: "context_too_large"})` when the changed content's
    /// decoded bytes exceed the cap; the error carries nothing to publish.
    pub fn collect(&self, cancelled: &AtomicBool) -> Result<ContextChanges, WorkerEngineError> {
        let end = self.list(cancelled)?;
        // Validate the entire resulting namespace, including unchanged files.
        validate_staged_files(
            end.files.keys().map(|p| (p.as_str(), "")),
            limits(self.max_bytes),
        )
        .map_err(staging_error)?;
        let end_size = end
            .files
            .values()
            .map(|f| f.size_bytes)
            .fold(0, u64::saturating_add);
        if end_size > self.max_bytes {
            return Err(self.over_cap(end_size));
        }
        let changes = self.start.diff(&end);
        // Refuse from the listing before reading anything.
        let listed: u64 = changes
            .iter()
            .filter(|c| c.change != ChangeKind::Deleted)
            .map(|c| c.size_bytes.unwrap_or(0))
            .fold(0, u64::saturating_add);
        if listed > self.max_bytes {
            return Err(self.over_cap(listed));
        }
        let tree = SafeTree::new(self.dir());
        let mut total = 0u64;
        let mut items = Vec::with_capacity(changes.len());
        for change in changes {
            if change.change == ChangeKind::Deleted {
                items.push(item(change.path, ChangeKind::Deleted, None));
                continue;
            }
            let path = ContextRelPath::parse(&change.path)
                .map_err(|_| invalid("the context directory holds an invalid path"))?;
            let remaining = self.max_bytes - total;
            // The bytes, size and hash all come from one no-follow handle,
            // bounded by what is left of the cap.
            let (bytes, meta) = match tree.read(&path, remaining) {
                Ok(read) => read,
                Err(SafeTreeError::TooLarge { size_bytes }) => {
                    return Err(self.over_cap(total.saturating_add(size_bytes)))
                }
                Err(_) => {
                    return Err(invalid(format!(
                        "context file {} changed while it was collected",
                        change.path
                    )))
                }
            };
            total += bytes.len() as u64;
            let mut entry = item(change.path, change.change, None);
            entry.size_bytes = Some(meta.size_bytes);
            entry.sha256 = Some(meta.sha256);
            entry.content_base64 = Some(base64::engine::general_purpose::STANDARD.encode(&bytes));
            items.push(entry);
        }
        Ok(ContextChanges {
            items,
            next_cursor: None,
        })
    }

    /// The same diff with no content, for the failure report.
    pub fn collect_metadata(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<ContextChanges, WorkerEngineError> {
        let items = self
            .diff(cancelled)?
            .into_iter()
            .map(|c| {
                let kind = c.change;
                let mut entry = item(c.path, kind, None);
                if kind != ChangeKind::Deleted {
                    entry.size_bytes = c.size_bytes;
                    entry.sha256 = c.sha256;
                }
                entry
            })
            .collect();
        Ok(ContextChanges {
            items,
            next_cursor: None,
        })
    }

    fn over_cap(&self, size_bytes: u64) -> WorkerEngineError {
        too_large(format!(
            "changed context is at least {size_bytes} bytes; the lease allows {}",
            self.max_bytes
        ))
    }

    /// Remove the staged directory. Idempotent.
    pub fn remove(self) -> Result<(), WorkerEngineError> {
        self.store.remove_all().map_err(|error| {
            tracing::warn!(%error, "could not remove staged lease context");
            WorkerEngineError::Io("could not remove the staged lease context".into())
        })
    }
}

fn item(path: String, change: ChangeKind, content_base64: Option<String>) -> ContextChangeItem {
    ContextChangeItem {
        path,
        change: match change {
            ChangeKind::Added => ContextChangeKind::Added,
            ChangeKind::Modified => ContextChangeKind::Modified,
            ChangeKind::Deleted => ContextChangeKind::Deleted,
        },
        size_bytes: None,
        sha256: None,
        content_base64,
    }
}

/// Staged directories under `_lease/` whose lease is not in `active`. Entries
/// are removed without following symlinks (a link is unlinked, never its
/// target). Returns how many entries were removed.
pub fn remove_orphaned_lease_contexts(resolver: &ContextDirResolver, active: &[LeaseId]) -> usize {
    let root = resolver.lease_repo_root();
    if validate_context_path(resolver.awman_home(), &root).is_err() {
        tracing::warn!("lease context root is not a private directory; not sweeping it");
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let keep: std::collections::BTreeSet<String> = active.iter().map(|l| l.to_string()).collect();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| keep.contains(name)) {
            continue;
        }
        let path = entry.path();
        let outcome = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "could not remove an orphaned lease context"),
        }
    }
    removed
}
