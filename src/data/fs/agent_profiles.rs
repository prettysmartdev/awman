//! Owner-private profile records. The manifest is the only mutable durable
//! record; a free OS lease never implies that an execution stopped.
use super::{private_files::*, DataPaths};
use crate::data::error::DataError;
use crate::data::ids::ExecutionId;
use crate::data::session::{AgentName, Session};
use crate::data::settings_profile::{NativeSessionId, ProfileState, SettingsProfileName};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_NATIVE_SESSIONS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRepository {
    pub key: String,
    pub root_sha256: String,
}
#[derive(Debug, Clone)]
pub struct ProfileRepoBinding {
    pub root: PathBuf,
    pub repository: ProfileRepository,
}
impl ProfileRepoBinding {
    pub fn from_session(session: &Session) -> Result<Self, DataError> {
        Self::new(session.definition_root())
    }
    pub fn new(root: &Path) -> Result<Self, DataError> {
        let root = std::fs::canonicalize(root).map_err(|e| DataError::io(root, e))?;
        let hash = super::hash::sha256_hex(root.to_string_lossy().as_bytes());
        Ok(Self {
            root,
            repository: ProfileRepository {
                key: hash[..16].into(),
                root_sha256: hash,
            },
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateAtExit {
    Complete,
    Interrupted,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeSessionRecord {
    pub id: NativeSessionId,
    pub first_generation: u64,
    pub last_generation: u64,
    pub created_at: String,
    pub last_used_at: String,
    pub execution_id: ExecutionId,
    pub turn_state: TurnStateAtExit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStatus {
    Published,
    RetainedPrevious,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilePublication {
    pub generation: u64,
    pub at: String,
    pub status: PublicationStatus,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupReason {
    Running,
    Publishing,
    OwnerLost,
    LivenessUnknown,
    RuntimeUnavailable,
    SnapshotRejected,
    SnapshotIo,
    CleanupTimeout,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileCleanup {
    pub reason: CleanupReason,
    pub since: String,
    pub last_attempt: Option<String>,
    pub last_error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileOwner {
    /// Lost comparison material after a crash forbids publishing new history.
    #[serde(default)]
    pub redaction_required: bool,
    /// A crash during import must retain the previous complete generation.
    #[serde(default)]
    pub run_initialized: bool,
    #[serde(default)]
    pub native_identity_rejected: bool,
    pub execution_id: ExecutionId,
    pub session_id: String,
    pub command_id: Option<String>,
    pub runtime: String,
    pub resource_name: String,
    pub resource_label: String,
    pub owner_pid: u32,
    pub started_at: String,
    pub cancel_timeout: String,
    pub output_drain_timeout: String,
    pub base_generation: u64,
    pub agent_session: Option<NativeSessionId>,
    pub observed_native_sessions: Vec<NativeSessionId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileManifest {
    pub schema: u32,
    pub repository: ProfileRepository,
    pub provider: String,
    #[serde(default)]
    pub state_layout: String,
    pub name: SettingsProfileName,
    pub created_at: String,
    pub state: ProfileState,
    pub current_generation: u64,
    pub owner: Option<ProfileOwner>,
    pub cleanup: Option<ProfileCleanup>,
    pub last_publication: Option<ProfilePublication>,
    pub native_sessions: Vec<NativeSessionRecord>,
    #[serde(default)]
    pub history_unavailable: Option<String>,
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
impl ProfileManifest {
    pub fn new(repo: &ProfileRepoBinding, agent: &AgentName, name: &SettingsProfileName) -> Self {
        Self {
            schema: 1,
            repository: repo.repository.clone(),
            provider: agent.as_str().into(),
            state_layout: String::new(),
            name: name.clone(),
            created_at: now(),
            state: ProfileState::Inactive,
            current_generation: 0,
            owner: None,
            cleanup: None,
            last_publication: None,
            native_sessions: vec![],
            history_unavailable: None,
        }
    }
}
#[derive(Debug, Clone)]
pub struct ProfileLayout {
    root: PathBuf,
    repository: ProfileRepository,
    agent: String,
    name: SettingsProfileName,
    boundary: PathBuf,
}
impl ProfileLayout {
    pub fn new(
        paths: &DataPaths,
        repo: &ProfileRepoBinding,
        agent: &AgentName,
        name: &SettingsProfileName,
    ) -> Self {
        Self {
            root: paths
                .agent_profiles_dir(&repo.root)
                .join(agent.as_str())
                .join(name.as_str()),
            repository: repo.repository.clone(),
            agent: agent.as_str().into(),
            name: name.clone(),
            boundary: paths.agent_profiles_dir(&repo.root),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn exists(&self) -> bool {
        self.root.symlink_metadata().is_ok()
    }
    pub fn try_lock(&self, wait: Duration) -> Result<Option<ProfileLease>, DataError> {
        // Validate each existing component rather than following an alias inside
        // the store. Data home's ancestors may legitimately be shared (e.g. /tmp).
        let parent = self
            .root
            .parent()
            .ok_or_else(|| DataError::Other("profile parent absent".into()))?;
        private_chain(&self.boundary, parent)?;
        let Some(_namespace) = lock_private_timeout(
            &parent.join(format!(".{}.namespace.lock", self.name.as_str())),
            LockMode::Exclusive,
            Duration::ZERO,
            || Ok(()),
        )?
        else {
            return Ok(None);
        };
        private_chain(&self.boundary, &self.root)?;
        let lock = self.root.join("profile.lock");
        let Some(file) = lock_private_timeout(&lock, LockMode::Exclusive, wait, || Ok(()))? else {
            return Ok(None);
        };
        // A delete may have unlinked the inode while this caller waited. Never
        // authorize a new writer using a lease on that old inode.
        let current = open_private_file(&lock)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = file.metadata().map_err(|e| DataError::io(&lock, e))?;
            let b = current.metadata().map_err(|e| DataError::io(&lock, e))?;
            if a.ino() != b.ino() || a.dev() != b.dev() {
                return Err(DataError::Other("profile was deleted; retry".into()));
            }
        }
        for child in ["generations", "staging", "runs"] {
            create_private_dir(&self.root.join(child))?;
        }
        Ok(Some(ProfileLease {
            layout: self.clone(),
            _file: file,
        }))
    }
}
fn private_chain(boundary: &Path, root: &Path) -> Result<(), DataError> {
    let data_home = boundary.ancestors().nth(4).unwrap_or(boundary);
    // Configuration may legitimately place the data home below a symlinked
    // system path (for example macOS /var). Enforce the private-chain policy
    // only from the profile store boundary down.
    let mut path = PathBuf::new();
    for part in root.components() {
        path.push(part);
        if path.starts_with(data_home) {
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                check_store_parent(&path, data_home, &meta)?;
            }
        }
        if !path.starts_with(boundary) {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            check_store_parent(&path, data_home, &meta)?;
            if meta.file_type().is_symlink() {
                return Err(DataError::Other("profile path contains a symlink".into()));
            }
        }
        if path.starts_with(boundary) {
            create_private_dir(&path)?;
        }
    }
    Ok(())
}
pub struct ProfileLease {
    layout: ProfileLayout,
    _file: File,
}
impl ProfileLease {
    pub fn layout(&self) -> &ProfileLayout {
        &self.layout
    }
    pub fn read_manifest(&self) -> Result<Option<ProfileManifest>, DataError> {
        let m = read_manifest(&self.layout.root.join("manifest.json"))?;
        if let Some(m) = &m {
            self.validate(m)?;
        }
        Ok(m)
    }
    fn validate(&self, m: &ProfileManifest) -> Result<(), DataError> {
        if m.schema > 1 {
            return Err(DataError::Other("profile created by a newer awman".into()));
        }
        if m.schema != 1
            || m.repository != self.layout.repository
            || m.provider != self.layout.agent
            || m.name != self.layout.name
        {
            return Err(DataError::Other(
                "profile directory belongs to a different repository or agent".into(),
            ));
        }
        if m.native_sessions.len() > MAX_NATIVE_SESSIONS
            || (m.state.admits_writer() && (m.owner.is_some() || m.cleanup.is_some()))
            || (!m.state.admits_writer() && m.owner.is_none())
        {
            return Err(DataError::Other(
                "inconsistent profile ownership record".into(),
            ));
        }
        Ok(())
    }
    pub fn write_manifest(&self, m: &ProfileManifest) -> Result<(), DataError> {
        self.validate(m)?;
        if serde_json::to_vec_pretty(m)
            .map_err(|source| DataError::ConfigSerialize { source })?
            .len() as u64
            > MAX_MANIFEST_BYTES
        {
            return Err(DataError::Other("profile manifest exceeds 1 MiB".into()));
        }
        write_private_json_atomic(&self.layout.root.join("manifest.json"), m, || Ok(()))
    }
    pub fn run_dir(&self, id: &ExecutionId) -> PathBuf {
        self.layout.root.join("runs").join(id.as_str())
    }
    pub fn create_run_dir(&self, id: &ExecutionId) -> Result<PathBuf, DataError> {
        let p = self.run_dir(id);
        create_private_dir_new(&p)?;
        Ok(p)
    }
    pub fn generation_tree(&self, n: u64) -> PathBuf {
        self.generation_dir(n).join("tree")
    }
    fn generation_dir(&self, n: u64) -> PathBuf {
        self.layout
            .root
            .join("generations")
            .join(format!("g-{n:08}"))
    }
    pub fn begin_staging(&self) -> Result<PathBuf, DataError> {
        let p = self.layout.root.join("staging/snapshot");
        remove_tree(&p)?;
        create_private_dir_new(&p)?;
        create_private_dir_new(&p.join("tree"))?;
        Ok(p)
    }
    pub fn publish(&self, staging: PathBuf, next: &ProfileManifest) -> Result<(), DataError> {
        self.validate(next)?;
        if staging != self.layout.root.join("staging/snapshot") {
            return Err(DataError::Other("foreign profile staging path".into()));
        }
        let old = self.read_manifest()?.map_or(0, |m| m.current_generation);
        if next.current_generation != old + 1 {
            return Err(DataError::Other("stale profile generation".into()));
        }
        let target = self.generation_dir(next.current_generation);
        // Only an uncommitted generation can occupy the next name after a crash.
        remove_tree(&target)?;
        let snapshot = staging.join("snapshot.json");
        let mut metadata = read_private_json::<serde_json::Value>(&snapshot)?
            .ok_or_else(|| DataError::Other("profile snapshot metadata absent".into()))?;
        metadata["generation"] = serde_json::json!(next.current_generation);
        write_private_json_atomic(&snapshot, &metadata, || Ok(()))?;
        sync_tree(&staging)?;
        std::fs::rename(&staging, &target).map_err(|e| DataError::io(&target, e))?;
        sync_private_dir(&self.layout.root.join("generations"));
        self.write_manifest(next)
    }
    pub fn remove_run_dir(&self, id: &ExecutionId) -> Result<(), DataError> {
        let runs = self.layout.root.join("runs");
        let live = self.run_dir(id);
        let tombstone = runs.join(format!(".{}.removing", id.as_str()));
        // A previous interrupted removal leaves only a hidden tombstone. Never
        // make that partially removed tree visible at the live run path.
        remove_tree(&tombstone)?;
        match std::fs::rename(&live, &tombstone) {
            Ok(()) => sync_private_dir(&runs),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(DataError::io(&live, e)),
        }
        remove_tree(&tombstone)
    }
    pub fn prune_generations(&self, keep: &[u64]) -> Result<(), DataError> {
        let dir = self.layout.root.join("generations");
        for e in std::fs::read_dir(&dir).map_err(|e| DataError::io(&dir, e))? {
            let p = e.map_err(|e| DataError::io(&dir, e))?.path();
            if !keep.iter().any(|n| p == self.generation_dir(*n)) {
                remove_tree(&p)?;
            }
        }
        Ok(())
    }
    pub fn remove_uninitialized(self) -> Result<(), DataError> {
        if self.read_manifest()?.is_some() {
            return Err(DataError::Other("profile is initialized".into()));
        }
        let parent = self.layout.root.parent().unwrap();
        let _namespace = lock_private_timeout(
            &parent.join(format!(".{}.namespace.lock", self.layout.name.as_str())),
            LockMode::Exclusive,
            Duration::ZERO,
            || Ok(()),
        )?
        .ok_or(DataError::ProfileLockBusy)?;
        remove_tree(&self.layout.root)
    }
    pub fn remove_profile(self) -> Result<(), DataError> {
        let parent = self.layout.root.parent().unwrap();
        let _namespace = lock_private_timeout(
            &parent.join(format!(".{}.namespace.lock", self.layout.name.as_str())),
            LockMode::Exclusive,
            Duration::ZERO,
            || Ok(()),
        )?
        .ok_or(DataError::ProfileLockBusy)?;
        let m = self
            .read_manifest()?
            .ok_or_else(|| DataError::Other("profile not found".into()))?;
        if !m.state.admits_writer() {
            return Err(DataError::ProfileLockBusy);
        }
        let root = &self.layout.root;
        let parent = root.parent().unwrap();
        let tombstone = parent.join(format!(
            ".{}.deleting-{}",
            self.layout.name,
            uuid::Uuid::new_v4()
        ));
        std::fs::rename(root, &tombstone).map_err(|e| DataError::io(root, e))?;
        sync_private_dir(parent);
        remove_tree(&tombstone)?;
        Ok(())
    }
}
fn read_manifest(path: &Path) -> Result<Option<ProfileManifest>, DataError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DataError::io(path, e)),
    }
    let file = open_private_file(path)?;
    let mut bytes = vec![];
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| DataError::io(path, e))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(DataError::Other("profile manifest exceeds 1 MiB".into()));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| DataError::Other(format!("invalid profile manifest: {e}")))
}
/// Removal never follows a symlink, including links planted by a guest.
pub fn remove_tree(path: &Path) -> Result<(), DataError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(DataError::io(path, e)),
    }
    .map_err(|e| DataError::io(path, e))
}
fn sync_tree(path: &Path) -> Result<(), DataError> {
    let m = std::fs::symlink_metadata(path).map_err(|e| DataError::io(path, e))?;
    if m.is_dir() {
        create_private_dir(path)?;
        for e in std::fs::read_dir(path).map_err(|e| DataError::io(path, e))? {
            sync_tree(&e.map_err(|e| DataError::io(path, e))?.path())?;
        }
    } else if !m.is_file() || m.file_type().is_symlink() {
        return Err(DataError::Other("unsafe profile snapshot entry".into()));
    }
    let f = if m.is_file() {
        open_private_file(path)?
    } else {
        File::open(path).map_err(|e| DataError::io(path, e))?
    };
    f.sync_all().map_err(|e| DataError::io(path, e))
}
#[derive(Debug, Clone, Serialize)]
pub struct ProfileListing {
    pub agent: String,
    pub name: String,
    pub state: ProfileState,
    pub created_at: Option<String>,
    pub current_generation: u64,
    pub native_session_count: usize,
    pub execution_id: Option<ExecutionId>,
    pub cleanup_reason: Option<String>,
    pub last_publication: Option<ProfilePublication>,
}
pub fn list_profiles(
    paths: &DataPaths,
    repo: &ProfileRepoBinding,
    agent: Option<&AgentName>,
) -> Result<Vec<ProfileListing>, DataError> {
    let root = paths.agent_profiles_dir(&repo.root);
    let mut out = vec![];
    if std::fs::symlink_metadata(&root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(out);
    }
    check_existing_chain(&root, &root)?;
    for a in std::fs::read_dir(&root).map_err(|e| DataError::io(&root, e))? {
        let a = a.map_err(|e| DataError::io(&root, e))?;
        let Some(an) = a.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if AgentName::new(&an).is_err() || agent.is_some_and(|v| v.as_str() != an) {
            continue;
        }
        if check_existing_chain(&root, &a.path()).is_err() {
            // An unsafe/foreign provider directory must not block unrelated
            // profiles in this repository.
            continue;
        }
        let entries = match std::fs::read_dir(a.path()) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for n in entries {
            let n = n.map_err(|e| DataError::io(a.path(), e))?;
            let name = n.file_name().to_string_lossy().into_owned();
            if SettingsProfileName::parse(&name).is_err() {
                continue;
            }
            let m = check_existing_chain(&root, &n.path())
                .and_then(|_| read_manifest(&n.path().join("manifest.json")));
            let mut row = ProfileListing {
                agent: an.clone(),
                name: name.clone(),
                state: ProfileState::Closing,
                created_at: None,
                current_generation: 0,
                native_session_count: 0,
                execution_id: None,
                cleanup_reason: Some("manifest unreadable".into()),
                last_publication: None,
            };
            if let Ok(Some(m)) = m {
                if m.schema == 1
                    && m.repository == repo.repository
                    && m.provider == an
                    && m.name.as_str() == name
                {
                    row.state = m.state;
                    row.created_at = Some(m.created_at);
                    row.current_generation = m.current_generation;
                    row.native_session_count = m.native_sessions.len();
                    row.execution_id = m.owner.map(|o| o.execution_id);
                    row.cleanup_reason = m.cleanup.map(|c| {
                        serde_json::to_value(c.reason)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    });
                    row.last_publication = m.last_publication;
                }
            }
            out.push(row);
        }
    }
    out.sort_by(|a, b| (&a.agent, &a.name).cmp(&(&b.agent, &b.name)));
    Ok(out)
}

pub fn record_command_owner(
    paths: &super::ApiPaths,
    session: &str,
    command: &str,
    owner: bool,
) -> Result<(), DataError> {
    write_private_json_atomic(
        &paths
            .command_dir(session, command)
            .join("profile-owner.json"),
        &owner,
        || Ok(()),
    )
}
pub fn command_owner(
    paths: &super::ApiPaths,
    session: &str,
    command: &str,
) -> Result<bool, DataError> {
    Ok(read_private_json::<bool>(
        &paths
            .command_dir(session, command)
            .join("profile-owner.json"),
    )?
    .unwrap_or(false))
}

fn check_existing_chain(boundary: &Path, root: &Path) -> Result<(), DataError> {
    let data_home = boundary.ancestors().nth(4).unwrap_or(boundary);
    let mut path = PathBuf::new();
    for part in root.components() {
        path.push(part);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| DataError::io(&path, e))?;
        check_store_parent(&path, data_home, &meta)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(DataError::Other("unsafe profile directory".into()));
        }
        #[cfg(unix)]
        if path.starts_with(boundary) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != nix::unistd::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
                return Err(DataError::Other(
                    "profile directory is not owner-private".into(),
                ));
            }
        }
    }
    Ok(())
}

fn check_store_parent(
    path: &Path,
    data_home: &Path,
    meta: &std::fs::Metadata,
) -> Result<(), DataError> {
    #[cfg(unix)]
    if path.starts_with(data_home) {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() || meta.mode() & 0o022 != 0 {
            return Err(DataError::Other(
                "profile data parents must be owned by this user and not writable by others".into(),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, data_home, meta);
    Ok(())
}
