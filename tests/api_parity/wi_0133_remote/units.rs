//! `push_branch` translation and session-request validation (WI 0133).

use std::path::PathBuf;

use awman::command::error::CommandError;
use awman::command::session_create::{
    is_valid_branch_name, SessionCreatePolicy, SessionCreateRequest,
};
use awman::data::workflow_definition::TeardownStep;
use awman::engine::workflow::step_commands::teardown_step_to_shell;

fn push_branch(remote: Option<&str>, branch: Option<&str>) -> TeardownStep {
    TeardownStep::PushBranch {
        remote: remote.map(str::to_string),
        branch: branch.map(str::to_string),
    }
}

#[test]
fn push_branch_translation_covers_all_four_remote_branch_combinations() {
    for (remote, branch, expected) in [
        (
            Some("upstream"),
            Some("topic"),
            "git push -u -- 'upstream' 'topic'",
        ),
        (None, None, "git push -u -- 'origin' 'HEAD'"),
        (Some("upstream"), None, "git push -u -- 'upstream' 'HEAD'"),
        (None, Some("topic"), "git push -u -- 'origin' 'topic'"),
    ] {
        let (command, env) = teardown_step_to_shell(&push_branch(remote, branch));
        assert_eq!(command, expected, "remote={remote:?} branch={branch:?}");
        // Never a bare `git push`, which fails on a branch with no upstream.
        assert!(command.starts_with("git push -u "), "{command}");
        let env = env.expect("push_branch keeps its ssh override");
        assert_eq!(
            env.get("GIT_SSH_COMMAND").map(String::as_str),
            Some("ssh -o StrictHostKeyChecking=accept-new")
        );
    }
}

#[test]
fn push_branch_translation_quotes_hostile_names() {
    let (command, _) = teardown_step_to_shell(&push_branch(Some("o r"), Some("a'b")));
    assert_eq!(command, "git push -u -- 'o r' 'a'\\''b'");
}

fn policy() -> SessionCreatePolicy {
    let base = std::env::temp_dir().join("awman-wi0133-clones");
    SessionCreatePolicy::new(vec![], base)
}

fn remote(branch: Option<&str>) -> SessionCreateRequest {
    SessionCreateRequest {
        session_type: Some("remote".into()),
        repo_url: Some("git://127.0.0.1:9/origin.git".into()),
        branch: branch.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn session_request_rejects_remote_only_fields_on_a_local_session() {
    let local = || SessionCreateRequest {
        session_type: Some("local".into()),
        workdir: Some("/nonexistent".into()),
        ..Default::default()
    };
    let cases = [
        (
            SessionCreateRequest {
                base_ref: Some("main".into()),
                ..local()
            },
            "base_ref",
        ),
        (
            SessionCreateRequest {
                merge_refs: vec!["feature".into()],
                ..local()
            },
            "merge_refs",
        ),
        (
            SessionCreateRequest {
                push: true,
                ..local()
            },
            "push",
        ),
    ];
    for (request, expected) in cases {
        let error = request.validate(&policy()).unwrap_err();
        assert!(
            matches!(error, CommandError::SessionFieldRemoteOnly { field } if field == expected),
            "{expected}: {error:?}"
        );
        assert_eq!(error.http_status(), 400);
        assert_eq!(
            error.to_string(),
            format!("{expected} is only valid when session_type is 'remote'")
        );
    }
}

#[test]
fn session_request_requires_a_branch_for_base_ref_merge_refs_and_push() {
    let cases = [
        (
            SessionCreateRequest {
                base_ref: Some("main".into()),
                ..remote(None)
            },
            "base_ref",
        ),
        (
            SessionCreateRequest {
                merge_refs: vec!["feature".into()],
                ..remote(None)
            },
            "merge_refs",
        ),
        (
            SessionCreateRequest {
                push: true,
                ..remote(None)
            },
            "push",
        ),
    ];
    for (request, expected) in cases {
        let error = request.validate(&policy()).unwrap_err();
        assert!(
            matches!(error, CommandError::SessionFieldRequiresBranch { field } if field == expected),
            "{expected}: {error:?}"
        );
        assert_eq!(error.http_status(), 400);
    }
}

#[test]
fn session_request_merge_refs_must_be_branch_names() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for bad in [
        sha,
        "refs/heads/feature",
        "refs/tags/v1",
        "origin/feature",
        "-feature",
        "a..b",
        "a b",
        "feature.lock",
        "",
        "@",
        "x@{1}",
    ] {
        let request = SessionCreateRequest {
            merge_refs: vec!["ok".into(), bad.into()],
            ..remote(Some("work"))
        };
        let error = request.validate(&policy()).unwrap_err();
        assert!(
            matches!(
                &error,
                CommandError::SessionRefInvalid { field: "merge_refs", value, .. } if value == bad
            ),
            "{bad:?}: {error:?}"
        );
        assert_eq!(error.http_status(), 400);
        assert!(!is_valid_branch_name(bad), "{bad:?}");
    }
}

#[test]
fn session_request_accepts_and_trims_remote_fields() {
    let request = SessionCreateRequest {
        base_ref: Some(" v1.0 ".into()),
        merge_refs: vec![" feature/a ".into(), "feature-b".into(), "feature-b".into()],
        push: true,
        ..remote(Some(" work "))
    };
    let plan = request.validate(&policy()).expect("valid remote request");
    assert_eq!(plan.branch.as_deref(), Some("work"));
    assert_eq!(plan.base_ref.as_deref(), Some("v1.0"));
    assert_eq!(plan.merge_refs, ["feature/a", "feature-b", "feature-b"]);
    assert!(plan.push);
    assert!(plan.cloned_path.is_some());
}

#[test]
fn session_request_base_ref_may_be_a_sha_tag_or_remote_branch_but_not_an_option() {
    for good in [
        "0123456789abcdef0123456789abcdef01234567",
        "v1",
        "origin/main",
    ] {
        let request = SessionCreateRequest {
            base_ref: Some(good.into()),
            ..remote(Some("work"))
        };
        assert!(request.validate(&policy()).is_ok(), "{good}");
    }
    let request = SessionCreateRequest {
        base_ref: Some("--upload-pack=evil".into()),
        ..remote(Some("work"))
    };
    let error = request.validate(&policy()).unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::SessionRefInvalid {
                field: "base_ref",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn session_request_invalid_branch_is_rejected() {
    let error = remote(Some("-x")).validate(&policy()).unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::SessionRefInvalid {
                field: "branch",
                ..
            }
        ),
        "{error:?}"
    );
}

/// `file://` is not a default scheme; a test policy can opt into it, and the
/// WI 0133 fields validate the same way under it.
#[test]
fn session_request_file_scheme_only_with_a_test_policy() {
    let request = SessionCreateRequest {
        repo_url: Some("file:///srv/origin.git".into()),
        push: true,
        ..remote(Some("work"))
    };
    assert!(matches!(
        request.validate(&policy()).unwrap_err(),
        CommandError::SessionRepoUrlInvalidScheme { .. }
    ));
    let mut permissive = policy();
    permissive.allowed_schemes.push("file://".into());
    let plan = request
        .validate(&permissive)
        .expect("file:// allowed by policy");
    assert!(plan.push);
    assert_eq!(
        plan.cloned_path,
        Some(PathBuf::from(&permissive.clone_base_dir).join("origin"))
    );
}
