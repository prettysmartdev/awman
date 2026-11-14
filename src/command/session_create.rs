//! Layer 2 session-creation validation and planning.
//!
//! Multi-session frontends (the API server today; desktop apps, editor
//! extensions, or k8s operators tomorrow) all need the same security-relevant
//! checks before opening a [`Session`](crate::data::session::Session):
//! the workdir must resolve onto the caller's allowlist, and a remote
//! repository URL must use an approved scheme. Per Tenet 2 of the grand
//! architecture, that logic must not live in any frontend — it lives here so
//! every frontend gets it for free and cannot drift.
//!
//! A frontend deserializes its request into a [`SessionCreateRequest`], builds
//! a [`SessionCreatePolicy`] from its configured allowlist and clone root, and
//! calls [`SessionCreateRequest::validate`]. The returned [`SessionCreatePlan`]
//! carries the resolved workdir plus (for remote sessions) the clone
//! destination — the exact tuple the API route handler used to compute inline.
//! Validation failures are typed [`CommandError`] variants so the frontend can
//! map them to transport-specific statuses (HTTP 400 vs 403) without inspecting
//! error strings.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::command::error::CommandError;
use crate::data::session::{GitRootResolver, SessionKind};

/// URL schemes accepted for `remote` session repository URLs. Bare paths and
/// `file:` URLs are intentionally excluded — a remote session must reference a
/// genuinely remote repository.
pub const DEFAULT_REPO_URL_SCHEMES: &[&str] = &["http://", "https://", "git@", "ssh://", "git://"];

/// A frontend-agnostic request to create a session. Field shapes mirror the
/// API's `CreateSessionRequest` body but carry no transport concerns.
#[derive(Debug, Clone, Default)]
pub struct SessionCreateRequest {
    pub context_key: Option<String>,
    /// `"local"` (default) or `"remote"`. Case-insensitive.
    ///
    /// Still a `String` because it is what arrives on the wire; it is parsed
    /// into a [`SessionKind`] by `validate`, once (WI 0114 F-48).
    pub session_type: Option<String>,
    /// Host workdir to mount (required for `local`).
    pub workdir: Option<String>,
    /// Repository URL to clone (required for `remote`).
    pub repo_url: Option<String>,
    /// Optional branch to check out (remote only).
    pub branch: Option<String>,
    /// Remote only. Branch, tag or sha the session branch is created from when
    /// `branch` does not yet exist on origin.
    pub base_ref: Option<String>,
    /// Remote only. Branch names merged into the session branch, in order,
    /// before the session is ready. Empty when absent.
    pub merge_refs: Vec<String>,
    /// Remote only. Push the session branch after a successful command.
    pub push: bool,
    /// Remote `https://` only (WI 0145). The caller's Git credential for
    /// this session, kept in daemon memory and never persisted.
    pub git_auth: Option<std::sync::Arc<crate::data::git_transport::GitAuthSpec>>,
}

/// The security policy a [`SessionCreateRequest`] is validated against. Carries
/// the workdir allowlist and permitted URL schemes (Tenet 3: a typed object
/// instead of loose function arguments), plus the base directory under which a
/// remote clone is planned.
#[derive(Debug, Clone)]
pub struct SessionCreatePolicy {
    /// Directories a `local` session is permitted to mount. Entries are
    /// canonicalized when checked against the requested workdir.
    pub workdirs: Vec<PathBuf>,
    /// Roots whose direct children may be used when they are independent git
    /// repositories. Resolved again per request; roots may not exist yet.
    pub workdir_roots: Vec<PathBuf>,
    /// Permitted `repo_url` scheme prefixes (matched case-insensitively).
    pub allowed_schemes: Vec<String>,
    /// Directory under which a `remote` session's clone is planned.
    pub clone_base_dir: PathBuf,
}

impl SessionCreatePolicy {
    /// Build a policy with the default accepted repo-URL scheme set.
    pub fn new(workdirs: Vec<PathBuf>, clone_base_dir: PathBuf) -> Self {
        Self {
            workdirs,
            workdir_roots: Vec::new(),
            allowed_schemes: DEFAULT_REPO_URL_SCHEMES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            clone_base_dir,
        }
    }

    /// Add roots without requiring them to exist until a request uses them.
    pub fn with_workdir_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.workdir_roots = roots;
        self
    }

    fn validate_workdir(
        &self,
        requested: &Path,
        git: &dyn GitRootResolver,
    ) -> Result<(), CommandError> {
        if self.workdirs.iter().any(|path| {
            path.canonicalize()
                .is_ok_and(|resolved| resolved == requested)
        }) {
            return Ok(());
        }

        let roots: Vec<PathBuf> = self
            .workdir_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .filter(|root| !self.workdirs.iter().any(|workdir| root.starts_with(workdir)))
            .filter(|root| match git.resolve(root) {
                Err(crate::data::error::DataError::GitRootNotFound { .. }) => true,
                _ => { tracing::warn!(root = %root.display(), "API workdir root is no longer outside git repositories, or git discovery failed"); false }
            })
            .collect();
        // A direct match takes precedence over any enclosing root, so the
        // result does not depend on the order of overlapping entries.
        let rule = if let Some(root) = roots
            .iter()
            .find(|root| requested.parent() == Some(root.as_path()))
        {
            let git_root = git
                .resolve(requested)
                .ok()
                .and_then(|p| p.canonicalize().ok());
            if git_root.as_deref() != Some(requested) {
                WorkdirRefusal::NotOwnGitRoot {
                    root: root.clone(),
                    git_root,
                }
            } else if !std::fs::symlink_metadata(requested.join(".git"))
                .is_ok_and(|metadata| metadata.is_dir())
            {
                // is_dir on symlink_metadata also rejects symlinked .git
                // directories, which could pull a mount outside the child.
                WorkdirRefusal::GitDirNotDirectory { root: root.clone() }
            } else {
                return Ok(());
            }
        } else if let Some(root) = roots
            .iter()
            .find(|root| requested != root.as_path() && requested.starts_with(root))
        {
            WorkdirRefusal::NotDirectChild { root: root.clone() }
        } else {
            WorkdirRefusal::NotListed
        };

        tracing::warn!(workdir = %requested.display(), rule = ?rule, "API workdir refused");
        Err(CommandError::SessionWorkdirNotAllowed {
            requested: requested.display().to_string(),
            rule,
            allowed: self
                .workdirs
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            roots: self
                .workdir_roots
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        })
    }
}

/// The allowlist rule a local session's canonical workdir failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirRefusal {
    /// Neither an exact entry nor under a canonical root.
    NotListed,
    /// Under the root, but deeper than one level.
    NotDirectChild { root: PathBuf },
    /// A direct child whose resolved git root is elsewhere or unavailable.
    NotOwnGitRoot {
        root: PathBuf,
        git_root: Option<PathBuf>,
    },
    /// A direct child with a pointer file, symlink or missing `.git` directory.
    GitDirNotDirectory { root: PathBuf },
}

impl fmt::Display for WorkdirRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotListed => f.write_str(
                "not an allowlisted workdir and not a direct child of an allowlisted root",
            ),
            Self::NotDirectChild { root } => write!(
                f,
                "it is below allowlisted root '{}' but is not a direct child of it",
                root.display(),
            ),
            Self::NotOwnGitRoot { .. } => f.write_str("it is a direct child of an allowlisted root but is not its own git root"),
            Self::GitDirNotDirectory { .. } => f.write_str(
                "its .git is not a real directory (linked worktrees and symlinked .git directories are not allowed)",
            ),
        }
    }
}

/// The validated, resolved plan a frontend acts on: the same tuple the API
/// route handler previously computed inline.
#[derive(Debug, Clone)]
pub struct SessionCreatePlan {
    pub context_key: Option<crate::data::context::ContextKey>,
    /// The validated session kind.
    pub kind: SessionKind,
    /// Resolved workdir. For remote sessions this equals the clone destination.
    pub resolved_workdir: PathBuf,
    /// Clone destination for remote sessions; `None` for local.
    pub cloned_path: Option<PathBuf>,
    /// Repo URL for remote sessions; `None` for local.
    pub repo_url: Option<String>,
    /// Branch for remote sessions; `None` for local (or unspecified).
    pub branch: Option<String>,
    /// Start point for a new session branch; `None` for local (or unspecified).
    pub base_ref: Option<String>,
    /// Branches merged into the session branch at setup; empty for local.
    pub merge_refs: Vec<String>,
    /// Push the session branch after a successful command; `false` for local.
    pub push: bool,
}

impl SessionCreatePlan {
    /// A bare tag and branch have the same spelling. Check advertised namespaces
    /// before accepting tag-only merge targets; missing names still fail setup.
    pub async fn validate_merge_ref_kinds(
        &self,
        git: std::sync::Arc<crate::engine::git::GitEngine>,
        transport: crate::engine::git::transport::HostGitTransport,
    ) -> Result<(), CommandError> {
        if self.merge_refs.is_empty() {
            return Ok(());
        }
        let Some(url) = self.repo_url.clone() else {
            return Ok(());
        };
        let refs = self.merge_refs.clone();
        let invalid = tokio::task::spawn_blocking(move || {
            git.tag_only_merge_ref(
                &url,
                &refs,
                &transport,
                &crate::engine::git::transport::TransportGuard::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
            )
        })
        .await
        .map_err(|e| CommandError::Other(e.to_string()))?
        .map_err(|error| match error {
            error @ crate::engine::error::EngineError::GitAuth { .. } => {
                CommandError::GitAuthFailed {
                    message: error.to_string(),
                }
            }
            error => CommandError::from(error),
        })?;
        if let Some(value) = invalid {
            return Err(CommandError::SessionRefInvalid {
                field: "merge_refs",
                value,
                reason: "a tag is not a branch on origin".into(),
            });
        }
        Ok(())
    }
}

impl SessionCreateRequest {
    /// Validate this request against `policy`, returning a resolved plan or a
    /// typed [`CommandError`].
    ///
    /// Applies the session-creation policy:
    /// - `session_type` defaults to `local`; anything other than
    ///   `local`/`remote` is [`CommandError::SessionInvalidType`].
    /// - `local` requires a `workdir` that **canonicalizes** (resolving
    ///   symlinks) onto the exact allowlist or a direct child of a root that
    ///   is its own git root and has a real `.git` directory. Roots are
    ///   canonicalized at each request before comparison.
    /// - `remote` requires a non-empty `repo_url` whose scheme is on the
    ///   policy's accepted list.
    /// - `base_ref`, `merge_refs` and `push` are remote-only
    ///   ([`CommandError::SessionFieldRemoteOnly`]) and each requires `branch`
    ///   ([`CommandError::SessionFieldRequiresBranch`]). `branch` and every
    ///   `merge_refs` entry must pass [`is_valid_branch_name`]; `base_ref` must
    ///   pass [`is_valid_ref_spelling`] ([`CommandError::SessionRefInvalid`]).
    pub fn validate(
        &self,
        policy: &SessionCreatePolicy,
        git: &dyn GitRootResolver,
    ) -> Result<SessionCreatePlan, CommandError> {
        // One parse, in Layer 2, producing the typed kind every later reader
        // uses (F-48). The `as_str()` matches this replaced ran in three
        // places against three separately-written literal sets.
        let context_key =
            crate::command::context::session_binding::plan_key(self.context_key.as_deref())?;
        let raw = self.session_type.as_deref().unwrap_or("local");
        let kind: SessionKind = raw.parse().map_err(|_| CommandError::SessionInvalidType {
            got: raw.to_string(),
        })?;

        match kind {
            SessionKind::Local => {
                // The WI 0133 fields only act on awman's own clone. `repo_url`
                // and `branch` stay silently ignored here, as they always were.
                if self.git_auth.is_some() {
                    return Err(CommandError::SessionFieldRemoteOnly { field: "git_auth" });
                }
                if self.base_ref.is_some() {
                    return Err(CommandError::SessionFieldRemoteOnly { field: "base_ref" });
                }
                if !self.merge_refs.is_empty() {
                    return Err(CommandError::SessionFieldRemoteOnly {
                        field: "merge_refs",
                    });
                }
                if self.push {
                    return Err(CommandError::SessionFieldRemoteOnly { field: "push" });
                }
                let workdir_in = self
                    .workdir
                    .as_deref()
                    .ok_or(CommandError::SessionWorkdirRequired)?;
                // Canonicalize (resolving symlinks) BEFORE the allowlist check
                // so a symlink cannot smuggle access to an off-allowlist path.
                let requested = std::fs::canonicalize(workdir_in).map_err(|_| {
                    CommandError::SessionWorkdirUnresolvable {
                        path: workdir_in.to_string(),
                    }
                })?;
                policy
                    .validate_workdir(&requested, git)
                    .map_err(|mut error| {
                        if let CommandError::SessionWorkdirNotAllowed { requested, .. } = &mut error
                        {
                            *requested = workdir_in.to_string();
                        }
                        error
                    })?;
                Ok(SessionCreatePlan {
                    context_key,
                    kind,
                    resolved_workdir: requested,
                    cloned_path: None,
                    repo_url: None,
                    branch: None,
                    base_ref: None,
                    merge_refs: Vec::new(),
                    push: false,
                })
            }
            SessionKind::Remote => {
                let repo_url = self
                    .repo_url
                    .clone()
                    .ok_or(CommandError::SessionRepoUrlRequired)?;
                if repo_url.trim().is_empty() {
                    return Err(CommandError::SessionRepoUrlEmpty);
                }
                if crate::data::git_transport::SafeRemote::url_has_credentials(&repo_url)
                    || crate::data::git_transport::SafeRemote::url_has_parameters(&repo_url)
                {
                    return Err(CommandError::SessionRefInvalid {
                        field: "repo_url",
                        value: crate::data::git_transport::SafeRemote::from_url(&repo_url)
                            .to_string(),
                        reason: "userinfo, query strings and fragments are not accepted; use git_auth or host Git credentials"
                            .into(),
                    });
                }
                let lower = repo_url.to_lowercase();
                let scheme_ok = policy
                    .allowed_schemes
                    .iter()
                    .any(|scheme| lower.starts_with(scheme.as_str()));
                if !scheme_ok {
                    return Err(CommandError::SessionRepoUrlInvalidScheme { url: repo_url });
                }
                if self.git_auth.is_some() {
                    crate::command::session_git_transport::git_auth_scope(&repo_url)?;
                }
                let branch = match self.branch.as_deref().map(str::trim) {
                    Some(b) => {
                        branch_name_problem(b).map_or(Ok(()), |reason| {
                            Err(CommandError::SessionRefInvalid {
                                field: "branch",
                                value: b.to_string(),
                                reason,
                            })
                        })?;
                        Some(b.to_string())
                    }
                    None => None,
                };
                // base_ref, merge_refs and push all act on "the session branch".
                // Without one the clone sits on the default branch, and a push
                // would go straight to it, so require it rather than document it.
                let base_ref = match self.base_ref.as_deref().map(str::trim) {
                    Some(b) => {
                        if branch.is_none() {
                            return Err(CommandError::SessionFieldRequiresBranch {
                                field: "base_ref",
                            });
                        }
                        ref_spelling_problem(b).map_or(Ok(()), |reason| {
                            Err(CommandError::SessionRefInvalid {
                                field: "base_ref",
                                value: b.to_string(),
                                reason,
                            })
                        })?;
                        Some(b.to_string())
                    }
                    None => None,
                };
                if !self.merge_refs.is_empty() && branch.is_none() {
                    return Err(CommandError::SessionFieldRequiresBranch {
                        field: "merge_refs",
                    });
                }
                let mut merge_refs = Vec::with_capacity(self.merge_refs.len());
                for entry in &self.merge_refs {
                    let r = entry.trim();
                    let problem = branch_name_problem(r).or_else(|| {
                        (r.len() >= 7 && r.len() <= 64 && r.bytes().all(|b| b.is_ascii_hexdigit()))
                            .then(|| "a commit sha is not a branch name".to_string())
                    });
                    if let Some(reason) = problem {
                        return Err(CommandError::SessionRefInvalid {
                            field: "merge_refs",
                            value: entry.clone(),
                            reason,
                        });
                    }
                    merge_refs.push(r.to_string());
                }
                if self.push && branch.is_none() {
                    return Err(CommandError::SessionFieldRequiresBranch { field: "push" });
                }
                let folder = repo_folder_from_url(&repo_url);
                let cloned = policy.clone_base_dir.join(&folder);
                Ok(SessionCreatePlan {
                    context_key,
                    kind,
                    resolved_workdir: cloned.clone(),
                    cloned_path: Some(cloned),
                    repo_url: Some(repo_url),
                    branch,
                    base_ref,
                    merge_refs,
                    push: self.push,
                })
            }
        }
    }
}

/// Derive a safe folder name for the clone target from a repo URL.
///
/// The folder is used as the on-disk repo name under `<session>/`, which in
/// turn drives the `awman-<repo>:latest` image tag (see `data::image_tags`).
/// Returning a per-repo name avoids cross-session image collisions when a
/// frontend hosts multiple remote sessions.
pub fn repo_folder_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    let safe: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        "repo".to_string()
    } else {
        safe
    }
}

/// Whether `name` is acceptable as a session `branch` or a `merge_refs` entry.
///
/// Pure and hermetic (no git spawn): `git check-ref-format --branch` restated,
/// plus three awman rules. A sha is not a branch, a `refs/...` spelling is not
/// a bare branch name, and an `origin/` prefix would double up because the
/// merge target is already `origin/<name>`. Tag-only names are checked against
/// the remote's advertisements by `validate_merge_ref_kinds` before acceptance.
pub fn is_valid_branch_name(name: &str) -> bool {
    branch_name_problem(name).is_none()
}

/// Whether `name` is acceptable as a `base_ref`: any branch, tag, sha or
/// `origin/x`, so only the spellings that could never be a ref or that would
/// inject a `git` option are refused. Git resolves it at setup.
pub fn is_valid_ref_spelling(name: &str) -> bool {
    ref_spelling_problem(name).is_none()
}

/// Shared by both ref rules: empty, control bytes, spaces, leading `-`.
fn basic_ref_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("must not be empty".to_string());
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Some("must not contain control characters".to_string());
    }
    if name.contains(' ') {
        return Some("must not contain spaces".to_string());
    }
    if name.starts_with('-') {
        return Some("must not start with '-'".to_string());
    }
    None
}

fn ref_spelling_problem(name: &str) -> Option<String> {
    basic_ref_problem(name)
}

fn branch_name_problem(name: &str) -> Option<String> {
    if let Some(problem) = basic_ref_problem(name) {
        return Some(problem);
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Some(format!("must not contain '{c}'"));
    }
    if name.contains("..") {
        return Some("must not contain '..'".to_string());
    }
    if name.contains("@{") {
        return Some("must not contain '@{'".to_string());
    }
    if name == "@" || name == "HEAD" {
        return Some("must not be '@' or 'HEAD'".to_string());
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Some("must not start or end with '/' or contain '//'".to_string());
    }
    if name.ends_with('.') {
        return Some("must not end with '.'".to_string());
    }
    if name
        .split('/')
        .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return Some("no path component may start with '.' or end with '.lock'".to_string());
    }
    if (name.len() == 40 || name.len() == 64) && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some("a commit sha is not a branch name".to_string());
    }
    if name.starts_with("refs/") {
        return Some("give the bare branch name, not a 'refs/' path".to_string());
    }
    if name.starts_with("origin/") {
        return Some("give the bare branch name; 'origin/' is implied".to_string());
    }
    None
}
