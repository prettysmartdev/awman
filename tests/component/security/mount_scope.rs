use awman::command::commands::mount_scope::{MountScope, MountScopeDecision, MountScopeFrontend};
use awman::command::error::CommandError;
use awman::command::session_create::{SessionCreatePolicy, SessionCreateRequest};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::engine::git::GitEngine;
use awman_test_support::fixtures::RepoBuilder;
use std::path::{Path, PathBuf};

struct Choice {
    decision: MountScopeDecision,
    asked: Vec<(PathBuf, PathBuf)>,
}
impl UserMessageSink for Choice {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Choice {
    fn ask_mount_scope(
        &mut self,
        root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        self.asked.push((root.into(), cwd.into()));
        Ok(self.decision)
    }
}

#[test]
fn widening_to_the_parent_git_root_requires_a_decision_and_dismissal_aborts() {
    let s = crate::support::sandbox("security::mount_scope::widening_to_the_parent_git_root_requires_a_decision_and_dismissal_aborts");
    let repo = RepoBuilder::new(&s, "app")
        .file("nested/file", "fixture")
        .init();
    let root = repo.path().canonicalize().unwrap();
    let cwd = root.join("nested");
    for decision in [
        MountScopeDecision::MountCurrentDirOnly,
        MountScopeDecision::MountGitRoot,
        MountScopeDecision::Abort,
    ] {
        let mut f = Choice {
            decision,
            asked: vec![],
        };
        let result = MountScope::resolve(&cwd, &root, &mut f);
        assert_eq!(f.asked, [(root.clone(), cwd.clone())]);
        match decision {
            MountScopeDecision::MountCurrentDirOnly => assert_eq!(result.unwrap(), cwd),
            MountScopeDecision::MountGitRoot => assert_eq!(result.unwrap(), root),
            MountScopeDecision::Abort => assert!(matches!(result, Err(CommandError::Aborted))),
        }
    }
    let mut f = Choice {
        decision: MountScopeDecision::Abort,
        asked: vec![],
    };
    assert_eq!(
        MountScope::resolve(&root.join("nested/.."), &root, &mut f).unwrap(),
        root
    );
    assert!(f.asked.is_empty());
    s.finish();
}

#[test]
fn admission_allows_only_exact_mounts_or_independent_direct_child_repositories() {
    let s = crate::support::sandbox("security::mount_scope::admission_allows_only_exact_mounts_or_independent_direct_child_repositories");
    let repo = RepoBuilder::new(&s, "repos/app")
        .file("nested/file", "fixture")
        .init();
    let root = repo.path().parent().unwrap();
    let git = GitEngine::with_env(crate::support::env_snapshot(&s));
    let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"))
        .with_workdir_roots(vec![root.into()]);
    let request = |p: &Path| SessionCreateRequest {
        context_key: None,
        workdir: Some(p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    assert!(request(repo.path()).validate(&policy, &git).is_ok());
    for path in [root.to_path_buf(), repo.path().join("nested"), s.home()] {
        let error = request(&path).validate(&policy, &git).unwrap_err();
        assert!(
            matches!(error, CommandError::SessionWorkdirNotAllowed { .. }),
            "{error:?}"
        );
        assert_eq!(error.http_status(), 403);
    }
    s.finish();
}

#[test]
fn remote_plans_reject_local_transport_and_option_injection_before_clone() {
    let s = crate::support::sandbox("security::mount_scope::remote_plans_reject_local_transport_and_option_injection_before_clone");
    let git = GitEngine::with_env(crate::support::env_snapshot(&s));
    let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"));
    for url in ["file:///etc", "/etc", "../repo", "ext::codex", "", "  "] {
        let request = SessionCreateRequest {
            context_key: None,
            session_type: Some("remote".into()),
            repo_url: Some(url.into()),
            ..Default::default()
        };
        assert!(request.validate(&policy, &git).is_err(), "{url}");
    }
    for branch in [
        "--upload-pack=codex",
        "../escape",
        "a..b",
        "refs/heads/no",
        "x\nexec",
    ] {
        let request = SessionCreateRequest {
            context_key: None,
            session_type: Some("remote".into()),
            repo_url: Some("https://example.invalid/r.git".into()),
            branch: Some(branch.into()),
            ..Default::default()
        };
        assert!(
            matches!(
                request.validate(&policy, &git),
                Err(CommandError::SessionRefInvalid { .. })
            ),
            "{branch}"
        );
    }
    assert!(!policy.clone_base_dir.exists());
    s.finish();
}

#[cfg(unix)]
#[test]
fn symlink_escapes_and_linked_git_directories_cannot_expand_admission() {
    #[cfg(unix)]
    {
        let s = crate::support::sandbox(
        "security::mount_scope::symlink_escapes_and_linked_git_directories_cannot_expand_admission",
    );
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("file", "fixture")
            .init();
        let git = GitEngine::with_env(crate::support::env_snapshot(&s));
        let policy = SessionCreatePolicy::new(vec![repo.path().into()], s.tmp().join("clones"));
        let link = repo.path().join("escape");
        std::os::unix::fs::symlink(s.home(), &link).unwrap();
        let request = SessionCreateRequest {
            context_key: None,
            workdir: Some(link.to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert!(matches!(
            request.validate(&policy, &git),
            Err(CommandError::SessionWorkdirNotAllowed { .. })
        ));
        let worktree = repo.worktree("linked", "linked-branch");
        let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"))
            .with_workdir_roots(vec![worktree.as_path().parent().unwrap().into()]);
        let request = SessionCreateRequest {
            context_key: None,
            workdir: Some(worktree.as_path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert!(request.validate(&policy, &git).is_err());
        s.finish();
    }
}
