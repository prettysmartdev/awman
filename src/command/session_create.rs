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

use std::path::PathBuf;

use crate::command::error::CommandError;

/// URL schemes accepted for `remote` session repository URLs. Bare paths and
/// `file:` URLs are intentionally excluded — a remote session must reference a
/// genuinely remote repository.
pub const DEFAULT_REPO_URL_SCHEMES: &[&str] =
    &["http://", "https://", "git@", "ssh://", "git://"];

/// A frontend-agnostic request to create a session. Field shapes mirror the
/// API's `CreateSessionRequest` body but carry no transport concerns.
#[derive(Debug, Clone, Default)]
pub struct SessionCreateRequest {
    /// `"local"` (default) or `"remote"`. Case-insensitive.
    pub session_type: Option<String>,
    /// Host workdir to mount (required for `local`).
    pub workdir: Option<String>,
    /// Repository URL to clone (required for `remote`).
    pub repo_url: Option<String>,
    /// Optional branch to check out (remote only).
    pub branch: Option<String>,
}

/// The security policy a [`SessionCreateRequest`] is validated against. Carries
/// the workdir allowlist and permitted URL schemes (Tenet 3: a typed object
/// instead of loose function arguments), plus the base directory under which a
/// remote clone is planned.
#[derive(Debug, Clone)]
pub struct SessionCreatePolicy {
    /// Canonicalized directories a `local` session is permitted to mount.
    pub workdirs: Vec<PathBuf>,
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
            allowed_schemes: DEFAULT_REPO_URL_SCHEMES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            clone_base_dir,
        }
    }
}

/// The validated, resolved plan a frontend acts on: the same tuple the API
/// route handler previously computed inline.
#[derive(Debug, Clone)]
pub struct SessionCreatePlan {
    /// Normalized session type: `"local"` or `"remote"`.
    pub session_type: String,
    /// Resolved workdir. For remote sessions this equals the clone destination.
    pub resolved_workdir: PathBuf,
    /// Clone destination for remote sessions; `None` for local.
    pub cloned_path: Option<PathBuf>,
    /// Repo URL for remote sessions; `None` for local.
    pub repo_url: Option<String>,
    /// Branch for remote sessions; `None` for local (or unspecified).
    pub branch: Option<String>,
}

impl SessionCreateRequest {
    /// Validate this request against `policy`, returning a resolved plan or a
    /// typed [`CommandError`].
    ///
    /// Mirrors the previous API-route validation exactly:
    /// - `session_type` defaults to `local`; anything other than
    ///   `local`/`remote` is [`CommandError::SessionInvalidType`].
    /// - `local` requires a `workdir` that **canonicalizes** (resolving
    ///   symlinks) onto the policy allowlist. Canonicalization happens before
    ///   the allowlist comparison so no frontend can bypass symlink resolution.
    /// - `remote` requires a non-empty `repo_url` whose scheme is on the
    ///   policy's accepted list.
    pub fn validate(
        &self,
        policy: &SessionCreatePolicy,
    ) -> Result<SessionCreatePlan, CommandError> {
        let session_type = self
            .session_type
            .as_deref()
            .unwrap_or("local")
            .to_lowercase();

        match session_type.as_str() {
            "local" => {
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
                if !policy.workdirs.contains(&requested) {
                    return Err(CommandError::SessionWorkdirNotAllowed {
                        requested: requested.display().to_string(),
                        allowed: policy
                            .workdirs
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect(),
                    });
                }
                Ok(SessionCreatePlan {
                    session_type,
                    resolved_workdir: requested,
                    cloned_path: None,
                    repo_url: None,
                    branch: None,
                })
            }
            "remote" => {
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
                let folder = repo_folder_from_url(&repo_url);
                let cloned = policy.clone_base_dir.join(&folder);
                Ok(SessionCreatePlan {
                    session_type,
                    resolved_workdir: cloned.clone(),
                    cloned_path: Some(cloned),
                    repo_url: Some(repo_url),
                    branch: self.branch.clone(),
                })
            }
            other => Err(CommandError::SessionInvalidType {
                got: other.to_string(),
            }),
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
        }
    }

    fn remote(repo_url: Option<&str>) -> SessionCreateRequest {
        SessionCreateRequest {
            session_type: Some("remote".to_string()),
            workdir: None,
            repo_url: repo_url.map(|s| s.to_string()),
            branch: Some("main".to_string()),
        }
    }

    // ── session_type ─────────────────────────────────────────────────────────

    #[test]
    fn unknown_session_type_is_invalid_type() {
        let req = SessionCreateRequest {
            session_type: Some("banana".to_string()),
            ..Default::default()
        };
        let err = req.validate(&policy(vec![])).unwrap_err();
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
        let err = req.validate(&policy(vec![])).unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirRequired),
            "got {err:?}"
        );
    }

    // ── local ────────────────────────────────────────────────────────────────

    #[test]
    fn local_missing_workdir_is_workdir_required() {
        let err = local(None).validate(&policy(vec![])).unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirRequired),
            "got {err:?}"
        );
    }

    #[test]
    fn local_noncanonicalizable_workdir_is_unresolvable() {
        let req = local(Some("/this/path/definitely/does/not/exist/awman-0097"));
        let err = req.validate(&policy(vec![])).unwrap_err();
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
            .validate(&policy(vec![allowed_canon]))
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
            .validate(&policy(vec![canon.clone()]))
            .expect("allowlisted workdir must validate");
        assert_eq!(plan.session_type, "local");
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
            .validate(&policy(vec![target_canon.clone()]))
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
            .validate(&policy(vec![allowed_canon]))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionWorkdirNotAllowed { .. }),
            "got {err:?}"
        );
    }

    // ── remote ───────────────────────────────────────────────────────────────

    #[test]
    fn remote_missing_repo_url_is_repo_url_required() {
        let err = remote(None).validate(&policy(vec![])).unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlRequired),
            "got {err:?}"
        );
    }

    #[test]
    fn remote_blank_repo_url_is_repo_url_empty() {
        let err = remote(Some("   ")).validate(&policy(vec![])).unwrap_err();
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
                .validate(&policy(vec![]))
                .unwrap_or_else(|e| panic!("accepted scheme {url} must validate; got {e:?}"));
            assert_eq!(plan.session_type, "remote");
            assert_eq!(plan.repo_url.as_deref(), Some(url));
            assert!(plan.cloned_path.is_some(), "remote plan must plan a clone path");
            assert_eq!(plan.branch.as_deref(), Some("main"));
        }
    }

    #[test]
    fn remote_file_scheme_is_invalid_scheme() {
        let err = remote(Some("file:///etc/passwd"))
            .validate(&policy(vec![]))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlInvalidScheme { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn remote_bare_path_is_invalid_scheme() {
        let err = remote(Some("/srv/git/local-repo"))
            .validate(&policy(vec![]))
            .unwrap_err();
        assert!(
            matches!(err, CommandError::SessionRepoUrlInvalidScheme { .. }),
            "got {err:?}"
        );
    }
}
