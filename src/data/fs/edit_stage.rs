//! Staged editing of artifacts that live outside the mounted checkout (Layer 0).
//!
//! A global or library artifact is never mounted into the editing container.
//! Instead [`EditStage::create`] copies exactly the resolved bundle into a
//! fresh private directory under the repository context
//! (`<repo context>/scratch/edit/<uuid>/`, mounted at
//! `/awman/context/repo/scratch/edit/<uuid>/`), and [`EditStage::publish`]
//! copies the result back on the host.
//!
//! The stage directory is agent-controlled, so everything read back from it is
//! untrusted: reads go through the bounded no-follow reader, the bundle is
//! validated from a private snapshot (never executed), every destination is
//! compared with its baseline before anything is written, and each file is
//! published atomically on its own (temp file + fsync + rename, descriptor
//! relative on Unix). Publication of a multi-file bundle is *not*
//! transactional: a write failure part-way is reported with what was and was
//! not written, and the draft is kept.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::edit_target::{
    check_bundle_limits, join_rel, normal_relative, probe_dockerfile_ref, read_bounded,
    validate_artifact, walk_bundle, ArtifactFileRole, ArtifactKind, ArtifactTarget, ReadFailure,
    WalkEntry, MAX_ARTIFACT_FILE_BYTES,
};
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::private_files::create_private_file_new;
#[cfg(not(unix))]
use crate::data::fs::private_files::{create_private_dir, create_private_dir_new};

/// Stage parent below the resolved repo context directory.
pub const EDIT_STAGE_SUBDIR: &str = "scratch/edit";
/// The same directory inside the container (`context(repo)` overlay).
pub const EDIT_STAGE_CONTAINER_ROOT: &str = "/awman/context/repo/scratch/edit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagePaths {
    pub host_dir: PathBuf,
    pub container_dir: String,
}

/// `<repo_context_host_dir>/scratch/edit/<id>` and the matching container path.
pub fn stage_paths(repo_context_host_dir: &Path, id: uuid::Uuid) -> StagePaths {
    let mut host_dir = repo_context_host_dir.to_path_buf();
    host_dir.extend(EDIT_STAGE_SUBDIR.split('/'));
    host_dir.push(id.to_string());
    StagePaths {
        host_dir,
        container_dir: format!("{EDIT_STAGE_CONTAINER_ROOT}/{id}"),
    }
}

/// A private working copy of one artifact bundle.
#[derive(Debug)]
pub struct EditStage {
    target: ArtifactTarget,
    paths: StagePaths,
    /// `host_dir` as resolved at creation; never re-resolved later, so a
    /// directory the agent swaps for a symlink fails closed.
    canonical_dir: PathBuf,
    /// Device and inode of the stage directory created here.
    #[cfg(unix)]
    identity: anchor::Identity,
}

#[derive(Debug)]
pub struct PublishReport {
    /// Destinations written (empty for a no-change session).
    pub published: Vec<PathBuf>,
    /// Staged entries that were not part of the permitted bundle and were left alone.
    pub ignored: Vec<String>,
}

#[derive(Debug)]
pub enum PublishFailureKind {
    /// The stage could not be read safely (symlink, special file, too large, not UTF-8 where required).
    Unreadable(String),
    /// The staged bundle does not validate.
    Invalid(Vec<String>),
    /// A destination no longer matches the baseline.
    Conflict(Vec<PathBuf>),
    /// A write failed part-way.
    Write { path: PathBuf, reason: String },
}

#[derive(Debug)]
pub struct PublishFailure {
    pub kind: PublishFailureKind,
    /// The retained draft directory.
    pub stage_dir: PathBuf,
    /// Destinations already replaced (non-empty only for `Write`).
    pub published: Vec<PathBuf>,
    /// Changed files not yet published.
    pub pending: Vec<PathBuf>,
}

impl fmt::Display for PublishFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| format!("\n  {}", p.display()))
                .collect::<String>()
        };
        match &self.kind {
            PublishFailureKind::Unreadable(reason) => {
                write!(f, "the draft could not be read safely: {reason}")?
            }
            PublishFailureKind::Invalid(problems) => {
                write!(f, "the edited bundle is invalid; nothing was saved:")?;
                for p in problems {
                    write!(f, "\n  {p}")?;
                }
            }
            PublishFailureKind::Conflict(paths) => write!(
                f,
                "these files changed while the agent was editing; nothing was saved:{}",
                list(paths)
            )?,
            PublishFailureKind::Write { path, reason } => {
                write!(f, "writing {} failed: {reason}", path.display())?;
                if !self.published.is_empty() {
                    write!(
                        f,
                        "\nalready saved (partial publication):{}",
                        list(&self.published)
                    )?;
                }
            }
        }
        if !self.pending.is_empty() {
            write!(f, "\nnot saved:{}", list(&self.pending))?;
        }
        write!(f, "\nthe draft was kept at {}", self.stage_dir.display())
    }
}

impl std::error::Error for PublishFailure {}

/// Writes one copied bundle member into the new stage.
type StageWriter<'a> = dyn FnMut(&str, &[u8]) -> Result<(), DataError> + 'a;

/// One file the publication will write.
struct Planned {
    rel: String,
    bytes: Vec<u8>,
    /// Baseline hash for an existing destination; `None` for a new file.
    baseline: Option<String>,
    role: ArtifactFileRole,
}

impl EditStage {
    /// Create the private stage directory and copy exactly `target.files`
    /// into it, preserving `rel`. Each source is re-read with the bounded safe
    /// reader and must still hash to its baseline, otherwise this fails and
    /// removes the stage.
    ///
    /// The repo context is agent-writable, so on Unix the stage is created
    /// through directory descriptors opened without following links (the
    /// context itself, then `scratch`, `edit` and the new stage), and its
    /// identity is recorded: a swapped ancestor makes creation fail before
    /// anything is written outside the context, and removal later refuses a
    /// directory that is not the one created here.
    pub fn create(target: &ArtifactTarget, paths: StagePaths) -> Result<Self, DataError> {
        let invalid = |reason: &str| DataError::InvalidPath {
            path: paths.host_dir.clone(),
            reason: reason.into(),
        };
        let context = paths
            .host_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or_else(|| invalid("stage directory is not below a repo context"))?;
        let name = paths
            .host_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| invalid("stage directory has no name"))?
            .to_string();
        if stage_paths(context, uuid::Uuid::nil()).host_dir.parent() != paths.host_dir.parent() {
            return Err(invalid("stage directory is not below a repo context"));
        }
        // The context directory itself is resolved by awman (it is the mount
        // point, not something the agent can replace). It may not exist yet.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(context)
            .map_err(|e| DataError::io(context, e))?;
        let context = std::fs::canonicalize(context).map_err(|e| DataError::io(context, e))?;
        let mut canonical = context.join(EDIT_STAGE_SUBDIR);
        canonical.push(&name);
        let copy = |write: &mut StageWriter<'_>| {
            for file in &target.files {
                let (bytes, sha) = read_bounded(&target.root, &file.rel, MAX_ARTIFACT_FILE_BYTES)
                    .map_err(|e| DataError::InvalidPath {
                    path: target.host_path(file),
                    reason: read_failure_text(&e),
                })?;
                if sha != file.sha256 {
                    return Err(DataError::InvalidPath {
                        path: target.host_path(file),
                        reason: "changed since it was resolved; run the edit again".into(),
                    });
                }
                write(&file.rel, &bytes)?;
            }
            Ok(())
        };
        #[cfg(unix)]
        {
            let created = anchor::create(&context, &name).map_err(|e| DataError::InvalidPath {
                path: paths.host_dir.clone(),
                reason: format!("cannot create the stage safely: {e}"),
            })?;
            let result = copy(&mut |rel, bytes| {
                anchor::write_file(&created.stage, rel, bytes)
                    .map_err(|e| DataError::io(join_rel(&canonical, rel), e))
            });
            if let Err(e) = result {
                let _ = anchor::remove_in(&created.parent, &name, created.identity);
                return Err(e);
            }
            Ok(Self {
                target: target.clone(),
                paths,
                canonical_dir: canonical,
                identity: created.identity,
            })
        }
        #[cfg(not(unix))]
        {
            let parent = canonical.parent().ok_or_else(|| invalid("no parent"))?;
            create_private_dir(parent)?;
            if std::fs::canonicalize(parent).ok().as_deref() != Some(parent) {
                return Err(invalid(
                    "the stage path passes through a symlink; refusing to stage",
                ));
            }
            create_private_dir_new(&canonical)?;
            let result = copy(&mut |rel, bytes| write_stage_file(&canonical, rel, bytes));
            if let Err(e) = result {
                let _ = std::fs::remove_dir_all(&canonical);
                return Err(e);
            }
            Ok(Self {
                target: target.clone(),
                paths,
                canonical_dir: canonical,
            })
        }
    }

    pub fn paths(&self) -> &StagePaths {
        &self.paths
    }

    pub fn target(&self) -> &ArtifactTarget {
        &self.target
    }

    /// `<container_dir>/<rel>`.
    pub fn container_path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.paths.container_dir)
    }

    /// Remove the stage. Only for "the agent never ran".
    pub fn discard(self) -> Result<(), DataError> {
        self.remove_stage()
    }

    /// Remove the stage created by [`EditStage::create`], never anything
    /// else: ancestors are re-opened without following links, a stage that
    /// was swapped for a symlink loses only the link, and a directory with a
    /// different identity is refused.
    #[cfg(unix)]
    fn remove_stage(&self) -> Result<(), DataError> {
        let dir = &self.canonical_dir;
        let (Some(parent), Some(name)) = (dir.parent(), dir.file_name().and_then(|n| n.to_str()))
        else {
            return Err(DataError::InvalidPath {
                path: dir.clone(),
                reason: "not a stage directory".into(),
            });
        };
        anchor::remove(parent, name, self.identity).map_err(|e| DataError::InvalidPath {
            path: dir.clone(),
            reason: format!("the draft was not removed: {e}"),
        })
    }

    #[cfg(not(unix))]
    fn remove_stage(&self) -> Result<(), DataError> {
        let dir = &self.canonical_dir;
        let parent = dir.parent().unwrap_or(dir);
        if std::fs::canonicalize(parent).ok().as_deref() != Some(parent) {
            return Err(DataError::InvalidPath {
                path: dir.clone(),
                reason: "an ancestor of the draft was replaced; it was not removed".into(),
            });
        }
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                std::fs::remove_file(dir).map_err(|e| DataError::io(dir, e))
            }
            Ok(_) => std::fs::remove_dir_all(dir).map_err(|e| DataError::io(dir, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(dir, e)),
        }
    }

    fn failure(
        &self,
        kind: PublishFailureKind,
        published: Vec<PathBuf>,
        pending: Vec<PathBuf>,
    ) -> PublishFailure {
        PublishFailure {
            kind,
            stage_dir: self.paths.host_dir.clone(),
            published,
            pending,
        }
    }

    /// Read stage → validate whole bundle → compare every destination with its
    /// baseline → publish changed files one by one, atomically → remove stage.
    /// On any failure the stage is kept and nothing further is written.
    pub fn publish(self) -> Result<PublishReport, PublishFailure> {
        let target = &self.target;
        // 1. Snapshot the stage.
        let snapshot = match self.snapshot() {
            Ok(s) => s,
            Err(reason) => {
                return Err(self.failure(PublishFailureKind::Unreadable(reason), vec![], vec![]))
            }
        };
        // 2. Decide what would be written.
        let mut ignored = Vec::new();
        let mut plan = Vec::new();
        let baseline: BTreeSet<&str> = target.files.iter().map(|f| f.rel.as_str()).collect();
        for file in &target.files {
            match snapshot.get(&file.rel) {
                Some(bytes) if sha256_hex(bytes) != file.sha256 => plan.push(Planned {
                    rel: file.rel.clone(),
                    bytes: bytes.clone(),
                    baseline: Some(file.sha256.clone()),
                    role: file.role,
                }),
                Some(_) => {}
                None if file.role == ArtifactFileRole::Primary => {
                    let problem = format!("{} was removed from the draft", file.rel);
                    return Err(self.failure(
                        PublishFailureKind::Invalid(vec![problem]),
                        vec![],
                        vec![],
                    ));
                }
                None => ignored.push(format!(
                    "{} (deleted in the draft; the original was kept)",
                    file.rel
                )),
            }
        }
        let new_companion = (target.kind == ArtifactKind::Agent)
            .then(|| snapshot.get(&target.primary().rel))
            .flatten()
            .and_then(|bytes| probe_dockerfile_ref(bytes))
            .filter(|r| normal_relative(r));
        for (rel, bytes) in &snapshot {
            if baseline.contains(rel.as_str()) {
                continue;
            }
            let role = if target.directory_bundle {
                Some(ArtifactFileRole::Supporting)
            } else if new_companion.as_deref() == Some(rel.as_str()) {
                Some(ArtifactFileRole::Companion)
            } else {
                None
            };
            match role {
                Some(role) => plan.push(Planned {
                    rel: rel.clone(),
                    bytes: bytes.clone(),
                    baseline: None,
                    role,
                }),
                None => ignored.push(rel.clone()),
            }
        }
        if plan.is_empty() {
            if let Err(e) = self.remove_stage() {
                tracing::warn!("could not remove edit stage: {e}");
            }
            return Ok(PublishReport {
                published: Vec::new(),
                ignored,
            });
        }
        let dest = |p: &Planned| join_rel(&target.root, &p.rel);
        let all_pending: Vec<PathBuf> = plan.iter().map(dest).collect();
        // A member deleted in the draft is kept at the destination, so the
        // bundle that will be on disk uses the destination's copy, which must
        // still be the baseline.
        let mut retained: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut conflicts = Vec::new();
        for file in target
            .files
            .iter()
            .filter(|f| !snapshot.contains_key(&f.rel))
        {
            match read_bounded(&target.root, &file.rel, MAX_ARTIFACT_FILE_BYTES) {
                Ok((bytes, sha)) if sha == file.sha256 => {
                    retained.insert(file.rel.clone(), bytes);
                }
                _ => conflicts.push(target.host_path(file)),
            }
        }
        if !conflicts.is_empty() {
            return Err(self.failure(PublishFailureKind::Conflict(conflicts), vec![], all_pending));
        }
        // 3. Validate the complete resulting bundle from a private copy.
        let bundle: BTreeMap<&str, &[u8]> = snapshot
            .iter()
            .filter(|(rel, _)| {
                baseline.contains(rel.as_str()) || plan.iter().any(|p| &p.rel == *rel)
            })
            .chain(retained.iter())
            .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice()))
            .collect();
        match validate_snapshot(target, &bundle) {
            Ok(problems) if problems.is_empty() => {}
            Ok(problems) => {
                return Err(self.failure(
                    PublishFailureKind::Invalid(problems),
                    vec![],
                    all_pending,
                ))
            }
            Err(reason) => {
                return Err(self.failure(
                    PublishFailureKind::Invalid(vec![reason]),
                    vec![],
                    all_pending,
                ))
            }
        }
        // 4. Every original member (changed or not: the validated bundle
        // depends on all of them) must still match its baseline, and every new
        // destination must still be absent. Best effort: no lock is held
        // between this check and the writes.
        for file in &target.files {
            let unchanged = matches!(
                read_bounded(&target.root, &file.rel, MAX_ARTIFACT_FILE_BYTES),
                Ok((_, current)) if current == file.sha256
            );
            if !unchanged {
                conflicts.push(target.host_path(file));
            }
        }
        for p in plan.iter().filter(|p| p.baseline.is_none()) {
            let absent = matches!(
                std::fs::symlink_metadata(dest(p)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            );
            if !absent {
                conflicts.push(dest(p));
            }
        }
        if !conflicts.is_empty() {
            return Err(self.failure(PublishFailureKind::Conflict(conflicts), vec![], all_pending));
        }
        // 5. Companions and supporting files first, the primary last.
        plan.sort_by_key(|p| (p.role == ArtifactFileRole::Primary, p.rel.clone()));
        let mut published = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            if let Err(reason) = injected_write_failure(&p.rel).map_or_else(
                || publish_file(&target.root, &p.rel, &p.bytes, p.baseline.is_some()),
                Err,
            ) {
                let pending = plan[i..].iter().map(dest).collect();
                return Err(self.failure(
                    PublishFailureKind::Write {
                        path: dest(p),
                        reason,
                    },
                    published,
                    pending,
                ));
            }
            published.push(dest(p));
        }
        // 6. Success: the draft is no longer needed.
        if let Err(e) = self.remove_stage() {
            tracing::warn!("could not remove edit stage: {e}");
        }
        Ok(PublishReport { published, ignored })
    }

    /// Every regular file in the stage, read with the bounded no-follow
    /// reader. Any symlink, special file or oversize entry fails the read.
    fn snapshot(&self) -> Result<BTreeMap<String, Vec<u8>>, String> {
        let dir = &self.canonical_dir;
        match std::fs::symlink_metadata(dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err(format!("{} was replaced by a non-directory", dir.display())),
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
        let mut files = Vec::new();
        for entry in walk_bundle(dir).map_err(|e| e.to_string())? {
            match entry {
                WalkEntry::File { rel, len } => files.push((rel, len)),
                WalkEntry::Skipped { rel, reason } => {
                    return Err(format!("{}: {reason}", join_rel(dir, &rel).display()))
                }
            }
        }
        check_bundle_limits(dir, &files).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for (rel, _) in files {
            let (bytes, _) = read_bounded(dir, &rel, MAX_ARTIFACT_FILE_BYTES).map_err(|e| {
                format!(
                    "{}: {}",
                    join_rel(dir, &rel).display(),
                    read_failure_text(&e)
                )
            })?;
            out.insert(rel, bytes);
        }
        Ok(out)
    }
}

fn read_failure_text(failure: &ReadFailure) -> String {
    match failure {
        ReadFailure::Missing => "is missing".into(),
        ReadFailure::TooLarge => format!("exceeds {MAX_ARTIFACT_FILE_BYTES} bytes"),
        ReadFailure::Unsafe(reason) => reason.clone(),
    }
}

/// Lay the snapshot out in a private host directory nobody else can write
/// and validate it there with the normal parsers. Nothing is executed.
pub(crate) fn validate_snapshot(
    target: &ArtifactTarget,
    bundle: &BTreeMap<&str, &[u8]>,
) -> Result<Vec<String>, String> {
    let dir = tempfile::Builder::new()
        .prefix("awman-edit-validate-")
        .tempdir()
        .map_err(|e| format!("cannot create a private validation directory: {e}"))?;
    let root = std::fs::canonicalize(dir.path()).map_err(|e| e.to_string())?;
    for (rel, bytes) in bundle {
        write_stage_file(&root, rel, bytes).map_err(|e| e.to_string())?;
    }
    if target.kind == ArtifactKind::Workflow {
        let content = bundle
            .get(target.primary().rel.as_str())
            .copied()
            .unwrap_or_default();
        if std::str::from_utf8(content).is_err() {
            return Ok(vec![format!(
                "{}: is not valid UTF-8",
                target.primary().rel
            )]);
        }
    }
    let problems = validate_artifact(
        target.kind,
        &target.scope,
        &root,
        &target.primary().rel,
        target.format,
    );
    // Report the real destination rather than the throwaway directory.
    let shown = target.root.display().to_string();
    let root_text = root.display().to_string();
    Ok(problems
        .into_iter()
        .map(|p| p.replace(&root_text, &shown))
        .collect())
}

/// Write `bytes` to `<dir>/<rel>` (owner-only), creating private parents.
/// Used for the stage and the validation copy, which awman just created.
fn write_stage_file(dir: &Path, rel: &str, bytes: &[u8]) -> Result<(), DataError> {
    let path = join_rel(dir, rel);
    if let Some(parent) = path.parent() {
        if parent != dir {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(parent)
                .map_err(|e| DataError::io(parent, e))?;
        }
    }
    let mut file = create_private_file_new(&path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| DataError::io(&path, e))
}

/// Atomically publish one file below the canonical bundle `root`.
/// `replace` → the destination must be a regular file and is replaced by
/// rename (keeping its permission bits); otherwise it is created no-clobber.
#[cfg(unix)]
fn publish_file(root: &Path, rel: &str, bytes: &[u8], replace: bool) -> Result<(), String> {
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
    use nix::sys::stat::{fchmod, fstatat, mkdirat, Mode, SFlag};
    use nix::unistd::{linkat, unlinkat, UnlinkatFlags};
    use std::os::fd::OwnedFd;
    use std::path::Component;

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let err = |what: &str, e: Errno| format!("{what}: {e}");
    // Open the canonical root one component at a time without following links.
    let mut dir: OwnedFd = open("/", dir_flags, Mode::empty()).map_err(|e| err("open /", e))?;
    for part in root.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                dir = openat(&dir, name, dir_flags, Mode::empty())
                    .map_err(|e| err(&format!("open {}", root.display()), e))?;
            }
            _ => return Err(format!("{} is not a canonical path", root.display())),
        }
    }
    let mut segments: Vec<&str> = rel.split('/').collect();
    let name = segments.pop().ok_or("empty destination name")?;
    for segment in segments {
        if !replace {
            match mkdirat(&dir, segment, Mode::from_bits_truncate(0o755)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(e) => return Err(err(&format!("create directory {segment}"), e)),
            }
        }
        dir = openat(&dir, segment, dir_flags, Mode::empty()).map_err(|e| match e {
            Errno::ELOOP | Errno::ENOTDIR => format!("{segment} is a symlink or not a directory"),
            e => err(&format!("open directory {segment}"), e),
        })?;
    }
    let mode = if replace {
        let stat = fstatat(&dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(|e| err("inspect destination", e))?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return Err("the destination is no longer a regular file".into());
        }
        Mode::from_bits_truncate(stat.st_mode & 0o7777)
    } else {
        Mode::from_bits_truncate(0o644)
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = format!(".{name}.awman-edit-{}-{nonce}", std::process::id());
    let fd = openat(
        &dir,
        tmp.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|e| err("create temporary file", e))?;
    let mut file = std::fs::File::from(fd);
    let written = file
        .write_all(bytes)
        .map_err(|e| format!("write: {e}"))
        .and_then(|()| fchmod(&file, mode).map_err(|e| err("set permissions", e)))
        .and_then(|()| file.sync_all().map_err(|e| format!("sync: {e}")));
    let result = written.and_then(|()| {
        if replace {
            renameat(&dir, tmp.as_str(), &dir, name).map_err(|e| err("replace", e))
        } else {
            // link(2) never replaces an existing entry, symlink or not.
            linkat(&dir, tmp.as_str(), &dir, name, AtFlags::empty()).map_err(|e| match e {
                Errno::EEXIST => "the destination appeared while saving".to_string(),
                e => err("create", e),
            })
        }
    });
    let _ = unlinkat(&dir, tmp.as_str(), UnlinkatFlags::NoRemoveDir);
    result?;
    std::fs::File::from(dir)
        .sync_all()
        .map_err(|e| format!("sync directory: {e}"))
}

/// Path-based fallback: refuses symlinked components, then writes atomically.
#[cfg(not(unix))]
fn publish_file(root: &Path, rel: &str, bytes: &[u8], replace: bool) -> Result<(), String> {
    let mut dir = root.to_path_buf();
    let mut segments: Vec<&str> = rel.split('/').collect();
    let name = segments.pop().ok_or("empty destination name")?;
    for segment in segments {
        dir.push(segment);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err(format!("{segment} is a symlink or not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !replace => {
                std::fs::create_dir(&dir).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let target = dir.join(name);
    if replace {
        match std::fs::symlink_metadata(&target) {
            Ok(m) if m.is_file() && !m.file_type().is_symlink() => {}
            _ => return Err("the destination is no longer a regular file".into()),
        }
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| e.to_string())?;
    tmp.write_all(bytes).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    if replace {
        tmp.persist(&target).map_err(|e| e.error.to_string())?;
    } else {
        tmp.persist_noclobber(&target)
            .map_err(|_| "the destination appeared while saving".to_string())?;
    }
    Ok(())
}

/// Descriptor-anchored creation and removal of a stage below the
/// agent-writable repo context. Every directory is opened with
/// `O_NOFOLLOW` relative to an already-open parent, so a component the
/// agent swaps for a symlink fails the operation instead of redirecting it.
#[cfg(unix)]
mod anchor {
    use std::ffi::OsStr;
    use std::io::{self, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use nix::errno::Errno;
    use nix::fcntl::{openat, AtFlags, OFlag};
    use nix::sys::stat::{fstat, fstatat, mkdirat, FileStat, Mode, SFlag};
    use nix::unistd::{unlinkat, UnlinkatFlags};

    use crate::data::fs::safe_tree::platform::context_root;

    /// `(st_dev, st_ino)` of the stage directory.
    pub(super) type Identity = (u64, u64);

    /// Deeper trees are refused rather than recursed into without bound.
    const MAX_REMOVE_DEPTH: usize = 64;

    fn dir_flags() -> OFlag {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    }

    #[allow(clippy::unnecessary_cast)]
    fn identity(stat: &FileStat) -> Identity {
        (stat.st_dev as u64, stat.st_ino as u64)
    }

    fn kind(stat: &FileStat) -> SFlag {
        SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT
    }

    fn mkdir_open(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        match mkdirat(dir, name, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(openat(dir, name, dir_flags(), Mode::empty())?)
    }

    pub(super) struct Created {
        /// `<context>/scratch/edit`.
        pub parent: OwnedFd,
        pub stage: OwnedFd,
        pub identity: Identity,
    }

    /// Open the canonical `context`, then create (or reuse real directories
    /// for) each `EDIT_STAGE_SUBDIR` component and a new stage `name`.
    pub(super) fn create(context: &Path, name: &str) -> io::Result<Created> {
        let mut dir = context_root(context, false)?;
        for part in super::EDIT_STAGE_SUBDIR.split('/') {
            dir = mkdir_open(&dir, OsStr::new(part))?;
        }
        let stat = fstat(&dir)?;
        if stat.st_uid != nix::unistd::geteuid().as_raw() {
            return Err(io::Error::other(
                "the stage parent is owned by another user",
            ));
        }
        if stat.st_mode & 0o077 != 0 {
            return Err(io::Error::other(
                "the stage parent is accessible to other users",
            ));
        }
        mkdirat(&dir, name, Mode::from_bits_truncate(0o700))?;
        let stage = openat(&dir, name, dir_flags(), Mode::empty())?;
        let identity = identity(&fstat(&stage)?);
        Ok(Created {
            parent: dir,
            stage,
            identity,
        })
    }

    /// Write `bytes` to `<stage>/<rel>` (owner-only) through descriptors.
    pub(super) fn write_file(stage: &OwnedFd, rel: &str, bytes: &[u8]) -> io::Result<()> {
        let mut segments: Vec<&str> = rel.split('/').collect();
        let name = segments.pop().ok_or(io::ErrorKind::InvalidInput)?;
        let mut owned: Option<OwnedFd> = None;
        for segment in segments {
            let next = mkdir_open(owned.as_ref().unwrap_or(stage), OsStr::new(segment))?;
            owned = Some(next);
        }
        let fd = openat(
            owned.as_ref().unwrap_or(stage),
            name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )?;
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()
    }

    /// Remove the stage `name` below the canonical `parent` path. A parent
    /// that no longer opens without following links is refused; a missing
    /// parent or stage means there is nothing left to remove.
    pub(super) fn remove(parent: &Path, name: &str, expected: Identity) -> io::Result<()> {
        let dir = match context_root(parent, false) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(io::Error::other(format!(
                    "an ancestor of the draft was replaced or cannot be opened safely ({e})"
                )))
            }
        };
        remove_in(&dir, name, expected)
    }

    /// Remove `<dir>/<name>` only if it is the stage with `expected`
    /// identity; a symlink in its place is unlinked without being followed.
    pub(super) fn remove_in(dir: &OwnedFd, name: &str, expected: Identity) -> io::Result<()> {
        let stat = match fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if kind(&stat) == SFlag::S_IFLNK {
            return Ok(unlinkat(dir, name, UnlinkatFlags::NoRemoveDir)?);
        }
        if kind(&stat) != SFlag::S_IFDIR || identity(&stat) != expected {
            return Err(io::Error::other(
                "the draft directory was replaced; refusing to remove it",
            ));
        }
        remove_tree(dir, OsStr::new(name), 0)
    }

    fn remove_tree(parent: &OwnedFd, name: &OsStr, depth: usize) -> io::Result<()> {
        if depth > MAX_REMOVE_DEPTH {
            return Err(io::Error::other("the draft is nested too deeply to remove"));
        }
        let dir = openat(parent, name, dir_flags(), Mode::empty())?;
        let mut children = Vec::new();
        for entry in nix::dir::Dir::from_fd(nix::unistd::dup(&dir)?)?.into_iter() {
            let entry = entry?;
            let child = entry.file_name().to_bytes();
            if child != b"." && child != b".." {
                children.push(child.to_vec());
            }
        }
        for child in &children {
            let child = OsStr::from_bytes(child);
            match fstatat(&dir, child, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) if kind(&stat) == SFlag::S_IFDIR => remove_tree(&dir, child, depth + 1)?,
                Ok(_) => unlinkat(&dir, child, UnlinkatFlags::NoRemoveDir)?,
                Err(Errno::ENOENT) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(unlinkat(parent, name, UnlinkatFlags::RemoveDir)?)
    }
}

#[cfg(test)]
#[path = "edit_stage_tests.rs"]
mod tests;

#[cfg(test)]
thread_local! {
    /// Test seam: fail the write of this staged `rel` (permission tricks are
    /// not deterministic when tests run as root).
    pub(crate) static FAIL_WRITE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn injected_write_failure(rel: &str) -> Option<String> {
    FAIL_WRITE.with(|f| (f.borrow().as_deref() == Some(rel)).then(|| "injected failure".into()))
}

#[cfg(not(test))]
fn injected_write_failure(_rel: &str) -> Option<String> {
    None
}
