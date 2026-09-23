//! `engine::overlay` — `OverlayEngine`.
//!
//! Consolidates overlay construction and management. Layer 0 *resolves* host
//! paths; this layer *builds* the resolved overlay specs that
//! `ContainerOption::Overlay` accepts. Replaces `oldsrc/overlays/` and the
//! agent-settings-passthrough bits of `oldsrc/passthrough.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::fs::overlay_paths::OverlayPathResolver;
use crate::data::fs::skill_library::read_library_meta;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::agent_matrix::{matrix_for, SettingsMount};
use crate::engine::auth::credential::{CredentialFile, CredentialFingerprint};
use crate::engine::container::options::{OverlayPermission, OverlaySpec};
use crate::engine::error::EngineError;

/// Top-level entries in `~/.claude/` that the legacy code excludes when
/// preparing a sanitized overlay copy. Single source of truth.
pub const CLAUDE_DENYLIST: &[&str] = &[
    "projects",
    "sessions",
    "session-env",
    "debug",
    "file-history",
    "history.jsonl",
    "telemetry",
    "downloads",
    "ide",
    "shell-snapshots",
    "paste-cache",
    // The host copy contains the refresh token.  Containers receive only the
    // awman-authored, refresh-token-free replacement planted below. The
    // case-insensitive, every-depth guard in `is_denied_credential_name` is the
    // real enforcement (INV-2); this entry keeps the exact top-level name in the
    // single-source list.
    ".credentials.json",
];

/// Credential filenames that must NEVER be copied into a staged Claude settings
/// overlay at ANY recursion depth, matched case-insensitively so
/// `.Credentials.json` (or any other case variant) cannot smuggle a copy of the
/// host refresh token into the read-write `~/.claude` bind mount (INV-2).
const CLAUDE_CREDENTIAL_DENYLIST: &[&str] = &[".credentials.json"];

/// True when `name` is a host-credential filename we must never mount. Compared
/// with `eq_ignore_ascii_case`, so case variants are rejected too.
fn is_denied_credential_name(name: &str) -> bool {
    CLAUDE_CREDENTIAL_DENYLIST
        .iter()
        .any(|denied| name.eq_ignore_ascii_case(denied))
}

/// Opaque filesystem identity used to reject a hard link or alias that points
/// at the very same inode as the host `.credentials.json`, regardless of the
/// name it wears. On unix this is `(dev, ino)`; elsewhere the canonical path.
#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = PathBuf;

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    // `metadata` follows symlinks intentionally: an alias pointing at the host
    // credential resolves to the same (dev, ino) as the credential itself.
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    std::fs::canonicalize(path).ok()
}

// `ContextScope` and `DirectorySpec` are Layer 0: the overlay grammar in
// `data::config::overlays` parses both, and a Layer 0 parser cannot name a
// Layer 1 type (WI 0114 F-27). Re-exported here for one release so existing
// `engine::overlay::{ContextScope, DirectorySpec}` paths still compile.
pub use crate::data::config::overlays::{ContextScope, DirectorySpec};

/// A resolved context-directory overlay (host path already ensured-to-exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOverlay {
    pub scope: ContextScope,
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

/// Description of "overlays I want for this command, with these flags".
#[derive(Debug, Default, Clone)]
pub struct OverlayRequest {
    /// Inline directory specs (host:container[:perm]).
    pub directories: Vec<DirectorySpec>,
    /// When true, mount all skill directories.
    pub include_all_skills: bool,
    /// Named skills to mount (when `include_all_skills` is false).
    pub named_skills: Vec<String>,
    /// Whether to include agent-settings overlays for `agent`. When `Some`
    /// the engine prepares per-agent host configs (e.g. `~/.claude.json`).
    pub agent: Option<AgentName>,
    /// When `true`, write `skipDangerousModePermissionPrompt: true` into the
    /// prepared Claude `settings.json` (Yolo mode).
    pub yolo: bool,
    /// Override container `$HOME` (defaults to `/root`).
    pub container_home: Option<String>,
    /// Context-directory overlays (global/repo/workflow).
    pub context_overlays: Vec<ContextOverlay>,
    /// Plant refreshable credential files into the staged agent-settings
    /// overlay. This is enabled only for file-delivered container credentials;
    /// passthrough/none auth modes must never cause host credentials to be
    /// copied into a mount.
    pub materialize_credentials: bool,
}

/// Resolved directory overlay (after canonicalization + tilde expansion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryOverlay {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: OverlayPermission,
}

/// Pluggable provider for per-agent file-form keychain artifacts. The
/// production binding shells out to the host OS keychain
/// (`engine::auth::keychain::agent_keychain_files`); tests inject a stub so
/// they don't accidentally read the dev's real macOS keychain.
pub type AgentSecretFilesProvider = std::sync::Arc<
    dyn Fn(&AgentName) -> Vec<crate::engine::auth::keychain::AgentSecretFile> + Send + Sync,
>;

/// Test-injectable source of refreshable credential files. The production
/// implementation reads the descriptor's host source and materializes its
/// refresh-token-free container file; tests can replace it without touching a
/// developer's keychain or host credential file.
pub type AgentCredentialFileProvider =
    std::sync::Arc<dyn Fn(&AgentName) -> Option<CredentialFile> + Send + Sync>;

/// A credential file planted in one retained staged settings directory.
/// Contains no secret material: only the path and a non-reversible fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedCredentialFile {
    pub agent: AgentName,
    pub path: PathBuf,
    pub root: PathBuf,
    pub fingerprint: CredentialFingerprint,
}

pub struct OverlayEngine {
    auth_resolver: AuthPathResolver,
    /// Source of file-form host-keychain artifacts to plant into agent
    /// settings overlays (e.g. `~/.gemini/antigravity-cli/...`). Injectable
    /// for testability; defaults to the real host-keychain reader.
    secret_files_provider: AgentSecretFilesProvider,
    credential_provider: AgentCredentialFileProvider,
    /// Sanitized temp directories that back agent-settings overlays. Held
    /// here so the directories live as long as this engine instance and are
    /// removed on `Drop` (RAII via `tempfile::TempDir`). This prevents the
    /// sanitized `~/.claude.json` and copied `~/.claude/` contents from
    /// leaking to `/tmp` after process exit.
    sanitized: std::sync::Mutex<Vec<tempfile::TempDir>>,
}

impl std::fmt::Debug for OverlayEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayEngine")
            .field("auth_resolver", &self.auth_resolver)
            .field("sanitized", &"<TempDir guard>")
            .finish_non_exhaustive()
    }
}

impl OverlayEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_resolver = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        let credential_provider = default_credential_provider(auth_resolver.clone());
        Ok(Self {
            auth_resolver,
            secret_files_provider: default_secret_files_provider(),
            credential_provider,
            sanitized: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn with_auth_resolver(auth_resolver: AuthPathResolver) -> Self {
        let credential_provider = default_credential_provider(auth_resolver.clone());
        Self {
            auth_resolver,
            secret_files_provider: default_secret_files_provider(),
            credential_provider,
            sanitized: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Replace the keychain provider. Used in tests to substitute a stub for
    /// the OS-keychain reader so the test suite stays deterministic and never
    /// reads a developer's real credentials.
    pub fn with_secret_files_provider(mut self, provider: AgentSecretFilesProvider) -> Self {
        self.secret_files_provider = provider;
        self
    }

    /// Replace the refreshable-credential source. Tests use this to avoid any
    /// host credential read while exercising staging behaviour.
    pub fn with_credential_provider(mut self, provider: AgentCredentialFileProvider) -> Self {
        self.credential_provider = provider;
        self
    }

    /// Track a sanitized tempdir so its cleanup is deferred until this
    /// engine is dropped.
    fn retain_tempdir(&self, dir: tempfile::TempDir) -> PathBuf {
        let path = dir.path().to_path_buf();
        if let Ok(mut guard) = self.sanitized.lock() {
            guard.push(dir);
        }
        path
    }

    /// Build the resolved overlay set for a request. Deduplicated by
    /// canonicalized host path; most restrictive permission wins.
    pub fn build_overlays(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError> {
        self.build_overlays_with_credentials(session, request)
            .map(|(overlays, _)| overlays)
    }

    /// Build overlays and report the refreshable credential files planted in
    /// staged settings directories. `build_overlays` remains the compatible
    /// convenience wrapper for callers that do not need the file metadata.
    pub fn build_overlays_with_credentials(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<(Vec<OverlaySpec>, Vec<StagedCredentialFile>), EngineError> {
        let mut by_key: HashMap<String, OverlaySpec> = HashMap::new();
        let mut staged_credentials = Vec::new();

        // 1. User-supplied directory overlays.
        for spec in &request.directories {
            let resolved = self.resolve_user_overlay(
                spec,
                session.working_dir(),
                request.container_home.as_deref(),
            )?;
            let key = OverlayPathResolver::conflict_key(&resolved.host_path);
            insert_or_merge(&mut by_key, key, resolved);
        }

        // 2. Agent settings overlays. Forward the yolo flag so Claude's
        //    settings sanitization can inject the bypass-permissions overlay,
        //    and the request's container_home so settings paths agree with
        //    user-supplied overlays.
        if let Some(agent) = &request.agent {
            let (agent_overlays, staged) = self.agent_settings_overlays_with_credentials(
                agent,
                request.yolo,
                session.git_root(),
                request.container_home.as_deref(),
                request.materialize_credentials,
            )?;
            staged_credentials.extend(staged);
            for spec in agent_overlays {
                let key = OverlayPathResolver::conflict_key(&spec.host_path);
                insert_or_merge(&mut by_key, key, spec);
            }
        }

        // 3. Skills overlay (mount ~/.awman/skills/ read-only into agent's native path).
        if request.include_all_skills || !request.named_skills.is_empty() {
            if let Some(agent) = &request.agent {
                for spec in self.skill_overlays(
                    agent,
                    request.include_all_skills,
                    &request.named_skills,
                    &request.container_home,
                    session.git_root(),
                )? {
                    let key = OverlayPathResolver::conflict_key(&spec.host_path);
                    insert_or_merge(&mut by_key, key, spec);
                }
            }
        }

        // 4. Context-directory overlays.
        for ctx in &request.context_overlays {
            let spec = OverlaySpec {
                host_path: ctx.host_path.clone(),
                container_path: ctx.container_path.clone(),
                permission: ctx.permission,
            };
            let key = OverlayPathResolver::conflict_key(&spec.host_path);
            insert_or_merge(&mut by_key, key, spec);
        }

        let mut out: Vec<OverlaySpec> = by_key.into_values().collect();
        out.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        Ok((out, staged_credentials))
    }

    /// Resolve a single user-supplied overlay spec into its canonical form.
    ///
    /// Relative host paths are resolved against `cwd` (the session's working
    /// directory), not the process's current directory.
    ///
    /// Fails fast when the host path does not exist on disk. Without this
    /// guard, Docker would auto-create an empty bind-mount source at run
    /// time and silently break tools that expect real content there
    /// (e.g. `ssh()` against a missing `~/.ssh`).
    pub fn resolve_user_overlay(
        &self,
        spec: &DirectorySpec,
        cwd: &Path,
        container_home: Option<&str>,
    ) -> Result<OverlaySpec, EngineError> {
        // Allow container paths starting with ~/ (expanded below).
        if !Path::new(&spec.container).is_absolute() && !spec.container.starts_with("~/") {
            return Err(EngineError::Other(format!(
                "overlay container path '{}' must be absolute",
                spec.container
            )));
        }
        let host_abs = OverlayPathResolver::make_absolute_with_cwd(&spec.host, cwd);
        let host_canon = OverlayPathResolver::canonicalize_lossy(&host_abs);
        if !host_canon.exists() {
            return Err(EngineError::Other(format!(
                "overlay host path '{}' does not exist (resolved to '{}')",
                spec.host,
                host_canon.display()
            )));
        }
        // Expand ~/ in container path to the container home directory.
        let container_path = if spec.container.starts_with("~/") {
            let home = container_home.unwrap_or("/root");
            format!("{}{}", home, &spec.container[1..])
        } else {
            spec.container.clone()
        };
        Ok(OverlaySpec {
            host_path: host_canon,
            container_path: PathBuf::from(container_path),
            permission: spec.permission,
        })
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let target = dst.join(entry.file_name());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                copy_dir_all(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
    }
    Ok(())
}

/// Recursively copy a subtree of the host `~/.claude` into a staged overlay
/// while applying the credential guard at EVERY depth (INV-2, BLOCKING-1):
/// files whose name matches the credential denylist (case-insensitively),
/// symlinks and other non-regular entries, and any file sharing the host
/// credential's inode identity are all skipped. Unlike `copy_dir_all` this
/// never follows a symlink and never copies a nested `.credentials.json`.
fn copy_claude_tree_secure(
    src: &Path,
    dst: &Path,
    host_credential: &Option<FileIdentity>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_denied_credential_name(&name_str) {
                continue;
            }
            let src_path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&src_path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let target = dst.join(&name);
            if meta.is_dir() {
                copy_claude_tree_secure(&src_path, &target, host_credential)?;
            } else if meta.is_file() {
                let identity = file_identity(&src_path);
                if identity.is_some() && identity == *host_credential {
                    continue;
                }
                std::fs::copy(&src_path, &target)?;
            }
            // Any other file type is never mounted.
        }
    }
    Ok(())
}

/// Parse a Dockerfile for the last non-root `USER` directive and return
/// `/home/<name>`. Returns `None` when the file doesn't exist, can't be read,
/// or only uses root.
pub(crate) fn detect_home_from_dockerfile(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut result: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        let upper = trimmed.to_uppercase();
        if let Some(rest) = upper.strip_prefix("USER ") {
            let name = rest.split_whitespace().next().unwrap_or("").trim();
            if !name.is_empty() && name != "ROOT" && name != "0" {
                let orig_rest = &trimmed[5..]; // skip "USER "
                let orig_name = orig_rest.split_whitespace().next().unwrap_or("root");
                result = Some(format!("/home/{orig_name}"));
            } else {
                // Switched back to root — reset.
                result = None;
            }
        }
    }
    result
}

/// Detect the container home directory by inspecting `Dockerfile.<agent>`.
///
/// Looks for a `USER <name>` directive (where `<name>` is not "root" or "0")
/// in `Dockerfile.<agent>` files under `<git_root>/.awman/` and `<home>/.awman/`.
/// Returns `Some("/home/<name>")` when found, `None` otherwise.
pub(crate) fn detect_container_home(home: &Path, agent: &str, git_root: &Path) -> Option<String> {
    let dockerfile_name = format!("Dockerfile.{agent}");
    let search_dirs: Vec<PathBuf> = [git_root.join(".awman"), home.join(".awman")]
        .into_iter()
        .collect();

    for dir in &search_dirs {
        let path = dir.join(&dockerfile_name);
        if let Some(home) = detect_home_from_dockerfile(&path) {
            return Some(home);
        }
    }
    None
}

/// Validate one segment of a `skill(...)` reference (a plain skill name, a
/// library name, or a skill name inside a library) as a single, contained
/// path component.
///
/// The overlay parser applies the same rule, but named skills also reach this
/// function from config files and the API, so containment is re-checked here:
/// an empty, `.`, or `..` segment would otherwise be joined onto a host path
/// and resolve to a directory the reference was never meant to name (e.g.
/// `skill(lib/..)` mounting the whole managed clone, `.git/` included).
fn validate_skill_reference_segment(segment: &str, name: &str) -> Result<(), EngineError> {
    let mut components = Path::new(segment).components();
    let first = components.next();
    let contained =
        matches!(first, Some(std::path::Component::Normal(_))) && components.next().is_none();
    if !contained {
        return Err(EngineError::Other(format!(
            "named skill '{name}' has an invalid path segment '{segment}'; segments must not be \
             empty, '.', '..', or contain a path separator"
        )));
    }
    Ok(())
}

/// Validate a persisted library `subdir` as a relative path *inside* the
/// managed clone and return its normalized form. Rejects empty values and any
/// absolute/root/prefix, `.`, or `..` component so a crafted `.awman.json`
/// (or `--subdir` value that produced it) can never turn a library mount into
/// a host path outside `skill_dirs.library_dir(<slug>)`. Mirrors the
/// containment rule applied by the command-layer pull orchestration.
fn validate_library_subdir(subdir: &str) -> Result<PathBuf, EngineError> {
    let mut normalized = PathBuf::new();
    let mut components = 0;
    for component in Path::new(subdir).components() {
        match component {
            std::path::Component::Normal(part) => {
                normalized.push(part);
                components += 1;
            }
            _ => {
                return Err(EngineError::Other(format!(
                    "skill library subdir '{subdir}' must be a relative path inside the \
                     library (no absolute, '.', or '..' components)"
                )));
            }
        }
    }
    if components == 0 {
        return Err(EngineError::Other(
            "skill library subdir must not be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn insert_or_merge(map: &mut HashMap<String, OverlaySpec>, key: String, spec: OverlaySpec) {
    use std::collections::hash_map::Entry;
    match map.entry(key) {
        Entry::Occupied(mut e) => {
            // Most restrictive permission wins.
            let existing = e.get_mut();
            if matches!(spec.permission, OverlayPermission::ReadOnly)
                && matches!(existing.permission, OverlayPermission::ReadWrite)
            {
                existing.permission = OverlayPermission::ReadOnly;
            }
            // Keep the existing container path; first writer wins for clarity.
        }
        Entry::Vacant(e) => {
            e.insert(spec);
        }
    }
}

// ─── Module layout (WI 0114 F-51) ───────────────────────────────────────────
//
// The engine keeps its types and its entry points here; the staging work is
// split by what is being staged. All private: `OverlayEngine` is declared in
// this file, so the module's public surface is unchanged.
mod agent_settings;
mod claude;
mod credential_file;
mod skills;

#[cfg(test)]
mod tests;

pub(crate) use claude::*;
pub(crate) use credential_file::*;
