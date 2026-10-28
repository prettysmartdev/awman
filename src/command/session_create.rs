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
}

/// The security policy a [`SessionCreateRequest`] is validated against. Carries
/// the workdir allowlist and permitted URL schemes (Tenet 3: a typed object
/// instead of loose function arguments), plus the base directory under which a
/// remote clone is planned.
#[derive(Debug, Clone)]
pub struct SessionCreatePolicy {
    /// Canonicalized directories a `local` session is permitted to mount.
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
        if self.workdirs.iter().any(|path| path == requested) {
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
    ) -> Result<(), CommandError> {
        if self.merge_refs.is_empty() {
            return Ok(());
        }
        let Some(url) = self.repo_url.clone() else {
            return Ok(());
        };
        let refs = self.merge_refs.clone();
        let invalid = tokio::task::spawn_blocking(move || git.tag_only_merge_ref(&url, &refs))
            .await
            .map_err(|e| CommandError::Other(e.to_string()))??;
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
        let raw = self.session_type.as_deref().unwrap_or("local");
        let kind: SessionKind = raw.parse().map_err(|_| CommandError::SessionInvalidType {
            got: raw.to_string(),
        })?;

        match kind {
            SessionKind::Local => {
                // The WI 0133 fields only act on awman's own clone. `repo_url`
                // and `branch` stay silently ignored here, as they always were.
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
                let lower = repo_url.to_lowercase();
                let scheme_ok = policy
                    .allowed_schemes
                    .iter()
                    .any(|scheme| lower.starts_with(scheme.as_str()));
                if !scheme_ok {
                    return Err(CommandError::SessionRepoUrlInvalidScheme { url: repo_url });
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

#[cfg(test)]
mod repo_folder_tests {
    use super::repo_folder_from_url;

    #[test]
    fn https_url_with_dot_git() {
        assert_eq!(
            repo_folder_from_url("https://github.com/cohix/somerepo.git"),
            "somerepo"
        );
    }

    #[test]
    fn https_url_without_dot_git() {
        assert_eq!(
            repo_folder_from_url("https://github.com/cohix/somerepo"),
            "somerepo"
        );
    }

    #[test]
    fn scp_style_ssh_url() {
        assert_eq!(
            repo_folder_from_url("git@github.com:cohix/somerepo.git"),
            "somerepo"
        );
    }

    #[test]
    fn ssh_url() {
        assert_eq!(
            repo_folder_from_url("ssh://git@github.com/cohix/somerepo.git"),
            "somerepo"
        );
    }

    #[test]
    fn trailing_slash_stripped() {
        assert_eq!(
            repo_folder_from_url("https://github.com/cohix/somerepo/"),
            "somerepo"
        );
    }

    #[test]
    fn empty_falls_back_to_repo() {
        assert_eq!(repo_folder_from_url(""), "repo");
    }

    #[test]
    fn unsafe_chars_filtered() {
        assert_eq!(
            repo_folder_from_url("https://example.com/group/my repo!.git"),
            "myrepo"
        );
    }
}

#[cfg(test)]
mod validate_tests {
    //! Table-driven coverage of [`SessionCreateRequest::validate`] — the Layer 2
    //! session-creation policy that the API frontend delegates to. Each case
    //! asserts the exact typed [`CommandError`] variant (or `Ok` plan) so a
    //! frontend can map failures to transport statuses without string matching.

    use super::*;
    use crate::data::session::StaticGitRootResolver;

    type RequestEdit = Box<dyn Fn(&mut SessionCreateRequest)>;

    /// A policy whose allowlist is exactly `workdirs`; clone base is a throwaway
    /// temp dir (only exercised by remote cases).
    fn policy(workdirs: Vec<PathBuf>) -> SessionCreatePolicy {
        let base = std::env::temp_dir().join("awman-0097-clone-base");
        SessionCreatePolicy::new(workdirs, base)
    }

    fn local(workdir: Option<&str>) -> SessionCreateRequest {
        SessionCreateRequest {
            session_type: Some("local".to_string()),
            workdir: workdir.map(|s| s.to_string()),
            repo_url: None,
            branch: None,
            ..Default::default()
        }
    }

    fn remote(repo_url: Option<&str>) -> SessionCreateRequest {
        SessionCreateRequest {
            session_type: Some("remote".to_string()),
            workdir: None,
            repo_url: repo_url.map(|s| s.to_string()),
            branch: Some("main".to_string()),
            ..Default::default()
        }
    }

    // ── session_type ─────────────────────────────────────────────────────────

    #[test]
    fn unknown_session_type_is_invalid_type() {
        let req = SessionCreateRequest {
            session_type: Some("banana".to_string()),
            ..Default::default()
        };
        let err = req
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionInvalidType { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn missing_session_type_defaults_to_local() {
        // No session_type → treated as local → fails on the missing workdir,
        // proving the default path is `local` (not an invalid-type error).
        let req = SessionCreateRequest::default();
        let err = req
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirRequired),
            "got {err:?}"
        );
    }

    // ── local ────────────────────────────────────────────────────────────────

    #[test]
    fn local_missing_workdir_is_workdir_required() {
        let err = local(None)
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirRequired),
            "got {err:?}"
        );
    }

    #[test]
    fn local_noncanonicalizable_workdir_is_unresolvable() {
        let req = local(Some("/this/path/definitely/does/not/exist/awman-0097"));
        let err = req
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirUnresolvable { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn local_offallowlist_workdir_is_not_allowed() {
        let allowed = tempfile::tempdir().unwrap();
        let allowed_canon = allowed.path().canonicalize().unwrap();
        let requested = tempfile::tempdir().unwrap();
        let requested_str = requested.path().to_str().unwrap().to_string();

        let err = local(Some(&requested_str))
            .validate(
                &policy(vec![allowed_canon]),
                &StaticGitRootResolver::new("/unused"),
            )
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirNotAllowed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn local_allowlisted_workdir_produces_plan() {
        let workdir = tempfile::tempdir().unwrap();
        let canon = workdir.path().canonicalize().unwrap();

        let plan = local(Some(workdir.path().to_str().unwrap()))
            .validate(
                &policy(vec![canon.clone()]),
                &StaticGitRootResolver::new("/unused"),
            )
            .expect("allowlisted workdir must validate");
        assert_eq!(plan.kind, SessionKind::Local);
        assert_eq!(plan.resolved_workdir, canon);
        assert!(plan.cloned_path.is_none());
        assert!(plan.repo_url.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn local_symlink_resolving_onto_allowlist_is_allowed() {
        // Canonicalization resolves symlinks BEFORE the allowlist check, so a
        // symlink pointing at an allowlisted dir must be accepted and resolve to
        // the real (allowlisted) target.
        let target = tempfile::tempdir().unwrap();
        let target_canon = target.path().canonicalize().unwrap();

        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("link-to-allowed");
        std::os::unix::fs::symlink(&target_canon, &link).unwrap();

        let plan = local(Some(link.to_str().unwrap()))
            .validate(
                &policy(vec![target_canon.clone()]),
                &StaticGitRootResolver::new("/unused"),
            )
            .expect("symlink onto allowlist must validate");
        assert_eq!(plan.resolved_workdir, target_canon);
    }

    #[cfg(unix)]
    #[test]
    fn local_symlink_resolving_off_allowlist_is_not_allowed() {
        // A symlink cannot smuggle access to an off-allowlist directory: the
        // resolved target is compared, not the symlink path.
        let allowed = tempfile::tempdir().unwrap();
        let allowed_canon = allowed.path().canonicalize().unwrap();

        let off = tempfile::tempdir().unwrap();
        let off_canon = off.path().canonicalize().unwrap();

        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("link-to-off");
        std::os::unix::fs::symlink(&off_canon, &link).unwrap();

        let err = local(Some(link.to_str().unwrap()))
            .validate(
                &policy(vec![allowed_canon]),
                &StaticGitRootResolver::new("/unused"),
            )
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirNotAllowed { .. }),
            "got {err:?}"
        );
    }

    // ── remote ───────────────────────────────────────────────────────────────

    #[test]
    fn remote_missing_repo_url_is_repo_url_required() {
        let err = remote(None)
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlRequired),
            "got {err:?}"
        );
    }

    #[test]
    fn remote_blank_repo_url_is_repo_url_empty() {
        let err = remote(Some("   "))
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlEmpty),
            "got {err:?}"
        );
    }

    #[test]
    fn remote_each_accepted_scheme_produces_plan() {
        let accepted = [
            "http://host.invalid/org/repo.git",
            "https://host.invalid/org/repo.git",
            "git@host.invalid:org/repo.git",
            "ssh://git@host.invalid/org/repo.git",
            "git://host.invalid/org/repo.git",
        ];
        for url in accepted {
            let plan = remote(Some(url))
                .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
                .unwrap_or_else(|e| panic!("accepted scheme {url} must validate; got {e:?}"));
            assert_eq!(plan.kind, SessionKind::Remote);
            assert_eq!(plan.repo_url.as_deref(), Some(url));
            assert!(
                plan.cloned_path.is_some(),
                "remote plan must plan a clone path"
            );
            assert_eq!(plan.branch.as_deref(), Some("main"));
        }
    }

    #[test]
    fn remote_file_scheme_is_invalid_scheme() {
        let err = remote(Some("file:///etc/passwd"))
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlInvalidScheme { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn remote_bare_path_is_invalid_scheme() {
        let err = remote(Some("/srv/git/local-repo"))
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlInvalidScheme { .. }),
            "got {err:?}"
        );
    }

    // ── WI 0133: base_ref / merge_refs / push ────────────────────────────────

    fn remote_with(edit: impl FnOnce(&mut SessionCreateRequest)) -> SessionCreateRequest {
        let mut req = remote(Some("https://host.invalid/org/repo.git"));
        edit(&mut req);
        req
    }

    #[test]
    fn remote_only_fields_on_local_name_the_field() {
        let workdir = tempfile::tempdir().unwrap();
        let canon = workdir.path().canonicalize().unwrap();
        let path = workdir.path().to_str().unwrap();
        let cases: [(&str, RequestEdit); 3] = [
            ("base_ref", Box::new(|r| r.base_ref = Some("main".into()))),
            (
                "merge_refs",
                Box::new(|r| r.merge_refs = vec!["feat".into()]),
            ),
            ("push", Box::new(|r| r.push = true)),
        ];
        for (name, edit) in cases {
            let mut req = local(Some(path));
            edit(&mut req);
            let err = req
                .validate(
                    &policy(vec![canon.clone()]),
                    &StaticGitRootResolver::new("/unused"),
                )
                .unwrap_err();
            assert!(
                matches!(err, CommandError::SessionFieldRemoteOnly { field } if field == name),
                "{name}: got {err:?}"
            );
            assert_eq!(err.http_status(), 400);
        }
    }

    #[test]
    fn remote_fields_require_branch() {
        let cases: [(&str, RequestEdit); 3] = [
            ("base_ref", Box::new(|r| r.base_ref = Some("main".into()))),
            (
                "merge_refs",
                Box::new(|r| r.merge_refs = vec!["feat".into()]),
            ),
            ("push", Box::new(|r| r.push = true)),
        ];
        for (name, edit) in cases {
            let req = remote_with(|r| {
                r.branch = None;
                edit(r);
            });
            let err = req
                .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
                .unwrap_err();
            assert!(
                matches!(err, CommandError::SessionFieldRequiresBranch { field } if field == name),
                "{name}: got {err:?}"
            );
            assert_eq!(err.http_status(), 400);
        }
    }

    #[test]
    fn remote_plan_carries_the_new_fields() {
        let plan = remote_with(|r| {
            r.branch = Some(" feature/x ".into());
            r.base_ref = Some("v1.2.0".into());
            r.merge_refs = vec!["a".into(), " b ".into()];
            r.push = true;
        })
        .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
        .unwrap();
        assert_eq!(plan.branch.as_deref(), Some("feature/x"));
        assert_eq!(plan.base_ref.as_deref(), Some("v1.2.0"));
        assert_eq!(plan.merge_refs, vec!["a".to_string(), "b".to_string()]);
        assert!(plan.push);
    }

    #[test]
    fn merge_refs_refuse_shas_refs_and_origin_prefixes() {
        for bad in [
            "0123456789abcdef0123456789abcdef01234567",
            "refs/tags/v1",
            "refs/heads/main",
            "origin/main",
            "-x",
            "a..b",
            "a b",
            "",
        ] {
            let err = remote_with(|r| r.merge_refs = vec![bad.into()])
                .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
                .unwrap_err();
            assert!(
                matches!(&err, CommandError::SessionRefInvalid { field: "merge_refs", value, .. } if value == bad),
                "{bad:?}: got {err:?}"
            );
            assert_eq!(err.http_status(), 400);
        }
    }

    #[test]
    fn invalid_branch_and_base_ref_are_refused() {
        let err = remote_with(|r| r.branch = Some("--foo".into()))
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                CommandError::SessionRefInvalid {
                    field: "branch",
                    ..
                }
            ),
            "got {err:?}"
        );
        let err = remote_with(|r| r.base_ref = Some("-b".into()))
            .validate(&policy(vec![]), &StaticGitRootResolver::new("/unused"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                CommandError::SessionRefInvalid {
                    field: "base_ref",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn branch_name_rule() {
        for ok in ["main", "feature/x", "release-1.2", "a@b", "deadbeef"] {
            assert!(is_valid_branch_name(ok), "{ok} should be valid");
        }
        for bad in [
            "", "@", "a~b", "a^b", "a:b", "a?b", "a*b", "a[b", "a\\b", "a@{b", "/a", "a/", "a//b",
            "a.", ".a", "a/.b", "a.lock", "a/b.lock", "refs/x", "origin/x", "-a", "a\tb",
        ] {
            assert!(!is_valid_branch_name(bad), "{bad:?} should be invalid");
        }
        let sha256 = "a".repeat(64);
        assert!(!is_valid_branch_name(&sha256));
    }

    #[test]
    fn ref_spelling_rule() {
        for ok in [
            "main",
            "origin/main",
            "v1.0",
            "0123456789abcdef0123456789abcdef01234567",
            "HEAD~2",
        ] {
            assert!(is_valid_ref_spelling(ok), "{ok} should be valid");
        }
        for bad in ["", "-x", "a b", "a\nb"] {
            assert!(!is_valid_ref_spelling(bad), "{bad:?} should be invalid");
        }
    }
}

#[cfg(test)]
mod workdir_root_tests {
    //! The allowlist-root rule (WI 0134): a direct child of a root is allowed
    //! only when it is its own git root with a real `.git` directory. These
    //! use real `git init` and the real `GitEngine` probe, because the rule is
    //! about what `git rev-parse --show-toplevel` reports.

    use super::*;
    use crate::engine::git::GitEngine;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
    }

    fn policy_with_roots(workdirs: Vec<PathBuf>, roots: Vec<PathBuf>) -> SessionCreatePolicy {
        SessionCreatePolicy::new(workdirs, std::env::temp_dir().join("awman-0134-clone-base"))
            .with_workdir_roots(roots)
    }

    fn request(path: &Path) -> SessionCreateRequest {
        SessionCreateRequest {
            session_type: Some("local".to_string()),
            workdir: Some(path.to_str().unwrap().to_string()),
            ..Default::default()
        }
    }

    fn validate(
        path: &Path,
        policy: &SessionCreatePolicy,
    ) -> Result<SessionCreatePlan, CommandError> {
        request(path).validate(policy, &GitEngine::new())
    }

    fn refusal(result: Result<SessionCreatePlan, CommandError>) -> (WorkdirRefusal, String) {
        match result.expect_err("must be refused") {
            err @ CommandError::SessionWorkdirNotAllowed { .. } => {
                let text = err.to_string();
                let CommandError::SessionWorkdirNotAllowed { rule, .. } = err else {
                    unreachable!()
                };
                (rule, text)
            }
            other => panic!("expected SessionWorkdirNotAllowed, got {other:?}"),
        }
    }

    #[test]
    fn direct_child_git_root_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("repo");
        init_repo(&child);

        let plan = validate(&child, &policy_with_roots(vec![], vec![root])).unwrap();
        assert_eq!(plan.resolved_workdir, child);
        assert_eq!(plan.kind, SessionKind::Local);
    }

    #[test]
    fn direct_child_that_is_not_a_git_root_is_refused() {
        // The root lives inside a repository, so the plain child's toplevel is
        // the enclosing repository, not the child.
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().canonicalize().unwrap().join("outer");
        init_repo(&outer);
        let root = outer.join("roots");
        let child = root.join("plain");
        std::fs::create_dir_all(&child).unwrap();

        let (rule, text) = refusal(validate(&child, &policy_with_roots(vec![], vec![root])));
        assert_eq!(rule, WorkdirRefusal::NotListed);
        assert!(text.contains("not an allowlisted workdir"));
    }

    #[test]
    fn direct_child_outside_any_repo_is_refused_as_not_own_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("plain");
        std::fs::create_dir_all(&child).unwrap();

        let (rule, _) = refusal(validate(&child, &policy_with_roots(vec![], vec![root])));
        assert!(
            matches!(rule, WorkdirRefusal::NotOwnGitRoot { .. }),
            "{rule:?}"
        );
    }

    #[test]
    fn direct_child_with_git_file_linked_worktree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let main = base.join("main-repo");
        init_repo(&main);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let root = base.join("roots");
        std::fs::create_dir_all(&root).unwrap();
        let linked = root.join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        assert!(
            linked.join(".git").is_file(),
            "fixture: .git must be a file"
        );

        let (rule, text) = refusal(validate(&linked, &policy_with_roots(vec![], vec![root])));
        assert!(
            matches!(rule, WorkdirRefusal::GitDirNotDirectory { .. }),
            "{rule:?}"
        );
        assert!(text.contains(".git is not a real directory"), "{text}");
    }

    #[test]
    fn grandchild_of_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("repo");
        init_repo(&child);
        let grandchild = child.join("src");
        std::fs::create_dir_all(&grandchild).unwrap();
        // Also a grandchild that is itself a repository.
        let nested = root.join("group").join("nested");
        init_repo(&nested);

        for path in [&grandchild, &nested] {
            let (rule, text) = refusal(validate(
                path,
                &policy_with_roots(vec![], vec![root.clone()]),
            ));
            assert!(
                matches!(rule, WorkdirRefusal::NotDirectChild { .. }),
                "{rule:?}"
            );
            assert!(text.contains("not a direct child"), "{text}");
        }
    }

    #[test]
    fn root_itself_is_not_allowed_by_the_root_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (rule, _) = refusal(validate(
            &root,
            &policy_with_roots(vec![], vec![root.clone()]),
        ));
        assert_eq!(rule, WorkdirRefusal::NotListed);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_direct_child_of_root_is_allowed_after_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("roots");
        let child = root.join("repo");
        init_repo(&child);
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("alias");
        std::os::unix::fs::symlink(&child, &link).unwrap();

        let plan = validate(&link, &policy_with_roots(vec![], vec![root])).unwrap();
        assert_eq!(plan.resolved_workdir, child);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_pointing_outside_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("roots");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().canonicalize().unwrap().join("repo");
        init_repo(&target);
        let link = root.join("escape");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (rule, _) = refusal(validate(&link, &policy_with_roots(vec![], vec![root])));
        assert_eq!(rule, WorkdirRefusal::NotListed);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_grandchild_of_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("roots");
        let nested = root.join("group").join("repo");
        init_repo(&nested);
        let link = root.join("shortcut");
        std::os::unix::fs::symlink(&nested, &link).unwrap();

        let (rule, _) = refusal(validate(&link, &policy_with_roots(vec![], vec![root])));
        assert!(
            matches!(rule, WorkdirRefusal::NotDirectChild { .. }),
            "{rule:?}"
        );
    }

    #[test]
    fn root_missing_at_validation_time_allows_nothing_then_works_once_created() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("later");
        let policy = policy_with_roots(vec![], vec![root.clone()]);

        // A repository elsewhere is not allowed while the root is missing.
        let other = base.join("other");
        init_repo(&other);
        let (rule, _) = refusal(validate(&other, &policy));
        assert_eq!(rule, WorkdirRefusal::NotListed);

        // Creating the root and a repository in it needs no policy rebuild.
        let child = root.join("repo");
        init_repo(&child);
        let plan = validate(&child, &policy).expect("allowed once the root exists");
        assert_eq!(plan.resolved_workdir, child);
    }

    #[test]
    fn exact_workdir_entry_still_allows_non_repo_and_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let plan = validate(&dir, &policy_with_roots(vec![dir.clone()], vec![])).unwrap();
        assert_eq!(plan.resolved_workdir, dir);

        // An exact entry that is also under a root is not subject to root rules.
        let child = dir.join("plain");
        std::fs::create_dir_all(&child).unwrap();
        let policy = policy_with_roots(vec![child.clone()], vec![dir]);
        assert_eq!(validate(&child, &policy).unwrap().resolved_workdir, child);
    }

    #[test]
    fn stranger_path_is_not_listed_and_body_names_rule() {
        let root = tempfile::tempdir().unwrap();
        let stranger = tempfile::tempdir().unwrap();
        let policy = policy_with_roots(vec![], vec![root.path().canonicalize().unwrap()]);
        let (rule, text) = refusal(validate(&stranger.path().canonicalize().unwrap(), &policy));
        assert_eq!(rule, WorkdirRefusal::NotListed);
        assert!(
            text.contains("not a direct child of an allowlisted root"),
            "{text}"
        );
    }

    #[test]
    fn overlapping_roots_allow_if_any_root_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().canonicalize().unwrap();
        let inner = outer.join("inner");
        let child = inner.join("repo");
        init_repo(&child);

        for roots in [
            vec![outer.clone(), inner.clone()],
            vec![inner.clone(), outer.clone()],
        ] {
            let plan = validate(&child, &policy_with_roots(vec![], roots)).unwrap();
            assert_eq!(plan.resolved_workdir, child);
        }
    }
    #[test]
    fn missing_root_later_created_inside_repository_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        init_repo(&outer);
        let root = outer.join("missing");
        let policy = policy_with_roots(vec![], vec![root.clone()]);
        let child = root.join("child");
        init_repo(&child);
        assert_eq!(
            refusal(validate(&child, &policy)).0,
            WorkdirRefusal::NotListed
        );
    }

    #[test]
    fn existing_root_later_initialized_as_repository_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("child");
        init_repo(&child);
        let policy = policy_with_roots(vec![], vec![root.clone()]);
        assert!(validate(&child, &policy).is_ok());
        init_repo(&root);
        assert_eq!(
            refusal(validate(&child, &policy)).0,
            WorkdirRefusal::NotListed
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_git_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("child");
        init_repo(&child);
        let metadata = root.join("elsewhere.git");
        std::fs::rename(child.join(".git"), &metadata).unwrap();
        std::os::unix::fs::symlink(&metadata, child.join(".git")).unwrap();
        assert!(matches!(
            refusal(validate(&child, &policy_with_roots(vec![], vec![root]))).0,
            WorkdirRefusal::GitDirNotDirectory { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refusal_echoes_client_spelling_without_disclosing_symlink_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let target = tmp.path().join("private-target");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&target);
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (_, text) = refusal(validate(&link, &policy_with_roots(vec![], vec![root])));
        assert!(text.contains(&link.display().to_string()));
        assert!(!text.contains(&target.display().to_string()));
        assert!(!text.contains("Allowed workdirs"));
    }
    #[cfg(unix)]
    #[test]
    fn root_retargeted_inside_an_exact_mount_is_refused_at_request_time() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tmp.path().canonicalize().unwrap().join("exact");
        std::fs::create_dir(&mount).unwrap();
        let child = mount.join("repo");
        init_repo(&child);
        let root = tmp.path().join("root");
        // Models a missing configured root that later appears as a symlink.
        let policy = policy_with_roots(vec![mount.clone()], vec![root.clone()]);
        std::os::unix::fs::symlink(&mount, &root).unwrap();
        assert_eq!(
            refusal(validate(&child, &policy)).0,
            WorkdirRefusal::NotListed
        );
    }
}
