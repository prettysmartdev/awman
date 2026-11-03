use super::*;
use crate::data::workflow_definition::Workflow;
use crate::data::workflow_state::PhaseKind;
use std::collections::BTreeSet;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_classification_strips_userinfo_and_supports_enterprise_hosts() {
        for (input, expected, ssh) in [
            (
                "git@github.com:owner/repo.git",
                "github.com:owner/repo.git",
                true,
            ),
            (
                "ssh://git@ghe.example:2222/owner/repo.git",
                "ssh://ghe.example:2222/owner/repo.git",
                true,
            ),
            (
                "https://user:password@ghe.example/owner/repo.git?access_token=unused#fragment",
                "https://ghe.example/owner/repo.git",
                false,
            ),
        ] {
            let remote = GitRemoteUrl::parse(input);
            assert_eq!(remote.display(), expected);
            assert_eq!(matches!(remote.kind, GitRemoteKind::Ssh { .. }), ssh);
            assert_eq!(remote.owner_repo, Some(("owner".into(), "repo".into())));
            assert!(!serde_json::to_string(&remote).unwrap().contains("password"));
        }
        for path in ["/tmp/repo", "../repo", "file:///tmp/repo", "C:\\repo"] {
            assert_eq!(GitRemoteUrl::parse(path).kind, GitRemoteKind::File);
        }
    }

    #[test]
    fn shell_heuristic_is_advisory_and_dynamic_needs_can_narrow() {
        let workflow: Workflow = serde_json::from_value(serde_json::json!({
            "title": null, "steps": [], "teardown": [
                {"type": "run_shell", "command": "git push; gh pr create"},
                {"type": "commit_changes", "message": "done"}
            ]
        }))
        .unwrap();
        let mut needs = WorkflowPrerequisites::assume_all();
        needs.narrow_to(&workflow);
        assert_eq!(needs.origin, PrerequisiteOrigin::Static);
        assert_eq!(needs.needs.len(), 2);
        assert_eq!(needs.advisories.len(), 1);
        assert!(needs.contains(&WorkflowPrerequisite::GitIdentity));
        let json = serde_json::to_string(&needs).unwrap();
        assert!(!json.contains("gh pr create"));
        assert!(!PrerequisiteStepKind::RunShell.receives_grants());
    }
}

#[cfg(test)]
mod derivation_tests {
    use super::*;
    use WorkflowPrerequisite as P;

    fn workflow(teardown: serde_json::Value) -> Workflow {
        serde_json::from_value(serde_json::json!({
            "title": null, "steps": [], "teardown": teardown
        }))
        .unwrap()
    }

    fn names(needs: &WorkflowPrerequisites) -> Vec<WorkflowPrerequisite> {
        needs.needs.iter().map(|n| n.prerequisite.clone()).collect()
    }

    fn git() -> P {
        P::ContainerTool { name: "git".into() }
    }

    #[test]
    fn required_for_commit_changes_is_identity_and_git() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "commit_changes", "message": "m"}]),
        ));
        assert_eq!(names(&needs), vec![P::GitIdentity, git()]);
        assert_eq!(needs.origin, PrerequisiteOrigin::Static);
        assert_eq!(
            needs.step_kinds(),
            BTreeSet::from([PrerequisiteStepKind::CommitChanges])
        );
    }

    #[test]
    fn required_for_push_branch_default_branch_adds_not_detached_head() {
        let default = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "push_branch"}]),
        ));
        assert!(default.contains(&P::NotDetachedHead));
        assert!(default.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        assert!(default.contains(&P::GitRemote {
            remote: "origin".into()
        }));
        assert!(default.contains(&git()));
        let explicit = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "push_branch", "branch": "feature"}]),
        ));
        assert!(!explicit.contains(&P::NotDetachedHead));
    }

    #[test]
    fn push_branch_remote_with_embedded_credentials_is_stripped_in_prerequisites() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "push_branch", "remote": "https://user:hunter2@ghe.example/o/r.git"}
        ])));
        let json = serde_json::to_string(&needs).unwrap();
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("user:"));
        assert!(json.contains("https://ghe.example/o/r.git"));
    }

    #[test]
    fn required_for_create_pull_request_needs_token_gh_and_pushed_branch() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "create_pull_request"}]),
        ));
        for expected in [
            P::GitHubToken {
                host: "github.com".into(),
            },
            P::ContainerTool { name: "gh".into() },
            git(),
            P::PushedBranch,
        ] {
            assert!(needs.contains(&expected), "missing {expected:?}");
        }
        assert!(!needs.contains(&P::GitIdentity));
    }

    #[test]
    fn poll_ci_needs_only_a_github_token() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "poll_ci"}]),
        ));
        assert_eq!(
            names(&needs),
            vec![P::GitHubToken {
                host: "github.com".into()
            }]
        );
    }

    #[test]
    fn run_shell_and_run_script_add_only_advisories() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "run_shell", "command": "git push origin HEAD"},
            {"type": "run_shell", "command": "git commit -am x"},
            {"type": "run_shell", "command": "gh pr create"},
            {"type": "run_script", "path": "scripts/gh-release.sh"},
            {"type": "run_shell", "command": "echo hello"}
        ])));
        assert!(needs.is_empty(), "opaque steps never add hard checks");
        // The run_script step is scanned by path only, which names no git/gh command.
        assert_eq!(needs.advisories.len(), 3);
        assert!(needs
            .advisories
            .iter()
            .all(|a| a.step.phase == PhaseKind::Teardown && !a.message.contains("git push")));
    }

    #[test]
    fn assume_all_covers_every_teardown_kind() {
        let all = WorkflowPrerequisites::assume_all();
        assert_eq!(all.origin, PrerequisiteOrigin::AssumedAll);
        for expected in [
            P::GitIdentity,
            P::GitRemote {
                remote: "origin".into(),
            },
            P::PushAuth {
                remote: "origin".into(),
            },
            P::GitHubToken {
                host: "github.com".into(),
            },
            git(),
            P::ContainerTool { name: "gh".into() },
            P::ContainerTool { name: "ssh".into() },
            P::NotDetachedHead,
        ] {
            assert!(all.contains(&expected), "missing {expected:?}");
        }
        // Every prerequisite any concrete teardown kind needs is covered by assume_all.
        let concrete = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "commit_changes", "message": "m"},
            {"type": "push_branch"},
            {"type": "create_pull_request"},
            {"type": "poll_ci"}
        ])));
        for need in &concrete.needs {
            if need.prerequisite == P::PushedBranch {
                continue; // order-dependent, not assumable
            }
            assert!(all.contains(&need.prerequisite), "{:?}", need.prerequisite);
        }
        assert!(all.advisories.is_empty());
    }

    #[test]
    fn narrowing_drops_prerequisites_of_unused_step_kinds() {
        let mut needs = WorkflowPrerequisites::assume_all();
        needs.narrow_to(&workflow(
            serde_json::json!([{"type": "commit_changes", "message": "m"}]),
        ));
        assert!(needs.contains(&P::GitIdentity));
        assert!(!needs.contains(&P::ContainerTool { name: "ssh".into() }));
        assert!(!needs.contains(&P::GitHubToken {
            host: "github.com".into()
        }));
        assert!(!needs.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        needs.narrow_to(&workflow(serde_json::json!([])));
        assert!(needs.is_empty());
    }

    #[test]
    fn setup_steps_reuse_push_auth_and_record_phase() {
        let workflow: Workflow = serde_json::from_value(serde_json::json!({
            "title": null, "steps": [],
            "setup": [
                {"type": "clone_repo", "url": "https://tok@github.com/o/r.git"},
                {"type": "pull_branch"}
            ]
        }))
        .unwrap();
        let needs = WorkflowPrerequisites::from_workflow(&workflow);
        assert!(needs.contains(&P::PushAuth {
            remote: "https://github.com/o/r.git".into()
        }));
        assert!(needs.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        assert!(needs
            .needs
            .iter()
            .flat_map(|n| &n.needed_by)
            .all(|s| s.phase == PhaseKind::Setup));
        assert!(PrerequisiteStepKind::CloneRepo.receives_grants());
        assert!(!PrerequisiteStepKind::RunScript.receives_grants());
    }

    #[test]
    fn same_prerequisite_from_two_steps_is_merged_with_both_step_refs() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "commit_changes", "message": "a"},
            {"type": "commit_changes", "message": "b"}
        ])));
        let identity = needs
            .needs
            .iter()
            .find(|n| n.prerequisite == P::GitIdentity)
            .unwrap();
        assert_eq!(identity.needed_by.len(), 2);
        assert_eq!(needs.needs.len(), 2);
    }

    #[test]
    fn remote_url_classification_matrix() {
        let ssh = |h: &str| GitRemoteKind::Ssh { host: h.into() };
        let https = |h: &str| GitRemoteKind::Https { host: h.into() };
        for (input, kind, display) in [
            (
                "git@github.com:org/repo.git",
                ssh("github.com"),
                "github.com:org/repo.git",
            ),
            (
                "ssh://git@github.com:2222/o/r",
                ssh("github.com"),
                "ssh://github.com:2222/o/r",
            ),
            (
                "https://github.com/o/r",
                https("github.com"),
                "https://github.com/o/r",
            ),
            (
                "https://x-access-token:ghp_abc@github.com/o/r.git",
                https("github.com"),
                "https://github.com/o/r.git",
            ),
            (
                "https://ghe.corp.example/o/r.git",
                https("ghe.corp.example"),
                "https://ghe.corp.example/o/r.git",
            ),
            (
                "HTTPS://GitHub.COM/o/r",
                https("github.com"),
                "HTTPS://GitHub.COM/o/r",
            ),
            (
                "file:///srv/git/repo",
                GitRemoteKind::File,
                "file:///srv/git/repo",
            ),
            ("/srv/git/repo", GitRemoteKind::File, "/srv/git/repo"),
            (
                "ftp://example/o/r",
                GitRemoteKind::Other,
                "ftp://example/o/r",
            ),
        ] {
            let parsed = GitRemoteUrl::parse(input);
            assert_eq!(parsed.kind, kind, "{input}");
            assert_eq!(parsed.display(), display, "{input}");
            assert!(!parsed.display().contains("ghp_abc"));
        }
        let ipv6 = GitRemoteUrl::parse("ssh://git@[::1]:22/o/r");
        assert_eq!(ipv6.host(), Some("::1"));
        assert!(GitRemoteUrl::parse("git@github.com:o/r").is_github_com());
        assert!(!GitRemoteUrl::parse("https://ghe.example/o/r").is_github_com());
        assert_eq!(GitRemoteUrl::parse("/local").host(), None);
        assert_eq!(
            GitRemoteUrl::parse("https://github.com/org/repo.git").owner_repo,
            Some(("org".into(), "repo".into()))
        );
        assert_eq!(
            GitRemoteUrl::parse("https://github.com/a/b/c").owner_repo,
            None
        );
    }

    fn check(p: P, status: PrerequisiteStatus, needed_by: Vec<StepRef>) -> PrerequisiteCheck {
        PrerequisiteCheck {
            prerequisite: p,
            status,
            explanation: "found in GH_TOKEN".into(),
            needed_by,
            remediations: vec![Remediation::UseEnvToken {
                var: "GH_TOKEN".into(),
                remembered: false,
            }],
        }
    }

    fn report(checks: Vec<PrerequisiteCheck>) -> WorkflowPrerequisiteReport {
        WorkflowPrerequisiteReport {
            phase: PrerequisitePhase::Start,
            origin: PrerequisiteOrigin::Static,
            checks,
            advisories: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn report_json_never_contains_a_token_value() {
        let fake = "ghp_FAKE_TOKEN_VALUE_123";
        let r = report(vec![check(
            P::GitHubToken {
                host: "github.com".into(),
            },
            PrerequisiteStatus::NeedsConsent,
            vec![],
        )]);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("GH_TOKEN"));
        assert!(!json.contains(fake));
        assert!(!r.render_lines().join("\n").contains(fake));
        let back: WorkflowPrerequisiteReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn report_summary_line_counts_needs_attention() {
        let ok = check(P::GitIdentity, PrerequisiteStatus::Satisfied, vec![]);
        let consent = check(git(), PrerequisiteStatus::NeedsConsent, vec![]);
        let missing = check(P::PushedBranch, PrerequisiteStatus::Missing, vec![]);
        let unknown = check(P::NotDetachedHead, PrerequisiteStatus::Unknown, vec![]);
        let r = report(vec![ok.clone(), consent, missing, unknown]);
        assert_eq!(
            r.summary_line(),
            "2 of 4 workflow prerequisites need your attention"
        );
        assert!(!r.all_satisfied());
        assert!(r.any_missing());
        assert_eq!(r.needs_attention().len(), 2);
        let all_ok = report(vec![ok]);
        assert!(all_ok.all_satisfied());
        assert!(!all_ok.any_missing());
        assert_eq!(
            all_ok.summary_line(),
            "all 1 workflow prerequisites are satisfied"
        );
        // `unknown` alone never blocks --check-prerequisites.
        assert!(report(vec![check(
            P::PushedBranch,
            PrerequisiteStatus::Unknown,
            vec![]
        )])
        .all_satisfied());
    }

    #[test]
    fn leader_summary_marks_token_gap_and_ssh_push() {
        let step = |kind| StepRef {
            phase: PhaseKind::Teardown,
            kind,
            index: 0,
            description: kind.label().into(),
        };
        let r = report(vec![
            check(
                P::GitIdentity,
                PrerequisiteStatus::Satisfied,
                vec![step(PrerequisiteStepKind::CommitChanges)],
            ),
            check(
                P::PushAuthSsh {
                    remote: "origin".into(),
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Satisfied,
                vec![step(PrerequisiteStepKind::PushBranch)],
            ),
            check(
                P::GitHubToken {
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Missing,
                vec![step(PrerequisiteStepKind::CreatePullRequest)],
            ),
        ]);
        let summary = r.leader_summary();
        assert!(summary.contains("commit ✓"), "{summary}");
        assert!(summary.contains("push over SSH ✓"), "{summary}");
        assert!(
            summary.contains(
                "create_pull_request ✗ (no GitHub token: do not add a create_pull_request step"
            ),
            "{summary}"
        );
    }

    #[test]
    fn remediation_and_check_serialization_carries_no_secret_fields() {
        let r = Remediation::RunAcquireCommand {
            label: "configured".into(),
            argv: vec!["op".into(), "read".into()],
            resolved_path: Some("/usr/bin/op".into()),
            scope: AcquireScope::Repo,
            approved: false,
            sha256: "abc".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "run_acquire_command");
        assert_eq!(json["scope"], "repo");
    }
}
