#![allow(clippy::field_reassign_with_default)]
//! WI 0149 command-level policy, remediation and security contracts.
use super::*;
use crate::command::dispatch::{catalogue::FrontendKind, Engines};
use crate::command::error::CommandError;
use crate::data::config::flags::*;
use crate::data::config::workflows::*;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::data::session::Session;
use crate::data::workflow_definition::Workflow;
use crate::data::workflow_prerequisites::*;
use crate::engine::auth::redaction::RedactionSet;
use crate::engine::workflow::prerequisite_grants::*;

#[derive(Default)]
struct Frontend {
    interactive: bool,
    messages: Vec<String>,
    identity_calls: usize,
    abort_tool: bool,
}
impl UserMessageSink for Frontend {
    fn replay_queued(&mut self) {}
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message.text);
    }
}
impl WorkflowPrerequisiteFrontend for Frontend {
    fn can_ask_prerequisites(&self) -> bool {
        self.interactive
    }
    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.identity_calls += 1;
        Ok(prompt.answer_for_key('h').unwrap())
    }
    fn ask_prerequisite_tool(
        &mut self,
        _: &Prompt<ToolChoice>,
    ) -> Result<ToolChoice, CommandError> {
        Ok(if self.abort_tool {
            ToolChoice::Abort
        } else {
            ToolChoice::Continue
        })
    }
}
fn check(prerequisite: WorkflowPrerequisite, status: PrerequisiteStatus) -> PrerequisiteCheck {
    PrerequisiteCheck {
        prerequisite,
        status,
        explanation: "fixture gap".into(),
        needed_by: vec![],
        remediations: vec![],
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
fn needs(report: &WorkflowPrerequisiteReport) -> WorkflowPrerequisites {
    WorkflowPrerequisites {
        origin: report.origin,
        needs: report
            .checks
            .iter()
            .map(|c| PrerequisiteNeed {
                prerequisite: c.prerequisite.clone(),
                needed_by: c.needed_by.clone(),
            })
            .collect(),
        advisories: vec![],
    }
}
fn flow(
    root: &std::path::Path,
    flags: PrerequisiteFlags,
    policy: PrerequisitePolicy,
) -> WorkflowPrerequisiteFlow {
    let session = Session::for_tests_isolated(root, root);
    let mut options = WorkflowPrerequisiteFlowOptions::for_session(
        &session,
        FrontendKind::Cli,
        &flags,
        None,
        PrerequisiteOrigin::Static,
        None,
    );
    options.policy = policy;
    WorkflowPrerequisiteFlow::new(&Engines::for_tests(root), &session, options)
}
#[test]
fn wi0149_flag_parsing_and_identity_validation_matrix() {
    assert_eq!(parse_identity("host").unwrap(), IdentityFlag::Host);
    assert_eq!(parse_identity("deny").unwrap(), IdentityFlag::Deny);
    assert_eq!(
        parse_identity(" Ada <ada@example.com> ").unwrap(),
        IdentityFlag::Fixed {
            name: "Ada".into(),
            email: "ada@example.com".into()
        }
    );
    for value in [
        "",
        "Ada",
        "<a@b>",
        "Ada <invalid>",
        "A\nB <a@b>",
        "Ada <a@b> extra",
    ] {
        assert!(parse_identity(value).is_err(), "{value:?}");
    }
    assert_eq!(parse_ssh("all").unwrap(), SshFlag::All);
    assert_eq!(parse_ssh("deny").unwrap(), SshFlag::Deny);
    assert_eq!(
        parse_ssh("~/.ssh/id_ed25519").unwrap(),
        SshFlag::Key("~/.ssh/id_ed25519".into())
    );
    assert!(parse_ssh("").is_err());
    for (value, expected) in [
        ("deny", TokenFlag::Deny),
        ("acquire", TokenFlag::Acquire),
        ("env:GH_TOKEN", TokenFlag::Env("GH_TOKEN".into())),
        ("secret:pat", TokenFlag::Secret("pat".into())),
    ] {
        assert_eq!(parse_token(value).unwrap(), expected);
    }
    for value in [
        "",
        "ghp_literal",
        "env:",
        "env:BAD-NAME",
        "env:1TOKEN",
        "secret:",
    ] {
        assert!(parse_token(value).is_err(), "{value}");
    }
}
#[test]
fn wi0149_remote_frontends_refuse_widening_but_accept_narrowing() {
    for kind in [FrontendKind::Api, FrontendKind::SquadDaemon] {
        for flags in [
            PrerequisiteFlags {
                ssh: Some(SshFlag::All),
                ..Default::default()
            },
            PrerequisiteFlags {
                identity: Some(IdentityFlag::Host),
                ..Default::default()
            },
            PrerequisiteFlags {
                token: Some(TokenFlag::Env("GH_TOKEN".into())),
                ..Default::default()
            },
            PrerequisiteFlags {
                token: Some(TokenFlag::Acquire),
                ..Default::default()
            },
        ] {
            let mut policy = PrerequisitePolicy::default();
            assert!(matches!(
                check_flag_widening(kind, &flags, &policy),
                Err(CommandError::PrerequisiteWideningRefused { .. })
            ));
            policy.allow_api_client_grants = true;
            check_flag_widening(kind, &flags, &policy).unwrap();
            for local in [FrontendKind::Cli, FrontendKind::Tui] {
                check_flag_widening(local, &flags, &PrerequisitePolicy::default()).unwrap();
            }
        }
        let flags = PrerequisiteFlags {
            identity: Some(IdentityFlag::Deny),
            ssh: Some(SshFlag::Deny),
            token: Some(TokenFlag::Deny),
            unresolved: Some(UnresolvedMode::Fail),
            ..Default::default()
        };
        check_flag_widening(kind, &flags, &PrerequisitePolicy::default()).unwrap();
    }
}
#[test]
fn wi0149_remote_grants_are_limited_to_the_operators_exact_key_and_env_list() {
    let mut policy = PrerequisitePolicy::default();
    policy.ssh.use_ = CredentialUse::Allow;
    policy.ssh.key = Some("~/.ssh/deploy".into());
    policy.github_token.use_ = CredentialUse::Allow;
    policy.github_token.env_vars = vec!["GH_TOKEN".into()];
    for (flags, allowed) in [
        (
            PrerequisiteFlags {
                ssh: Some(SshFlag::Key("~/.ssh/deploy".into())),
                ..Default::default()
            },
            true,
        ),
        (
            PrerequisiteFlags {
                ssh: Some(SshFlag::Key("~/.ssh/personal".into())),
                ..Default::default()
            },
            false,
        ),
        (
            PrerequisiteFlags {
                ssh: Some(SshFlag::All),
                ..Default::default()
            },
            false,
        ),
        (
            PrerequisiteFlags {
                token: Some(TokenFlag::Env("GH_TOKEN".into())),
                ..Default::default()
            },
            true,
        ),
        (
            PrerequisiteFlags {
                token: Some(TokenFlag::Env("OTHER_TOKEN".into())),
                ..Default::default()
            },
            false,
        ),
    ] {
        assert_eq!(
            check_flag_widening(FrontendKind::Api, &flags, &policy).is_ok(),
            allowed
        );
    }
}
#[test]
fn wi0149_explicit_identity_skips_prompt_even_on_tty() {
    let dir = tempfile::tempdir().unwrap();
    let flags = PrerequisiteFlags {
        identity: Some(IdentityFlag::Fixed {
            name: "Ada".into(),
            email: "ada@example.com".into(),
        }),
        ..Default::default()
    };
    let mut flow = flow(dir.path(), flags, PrerequisitePolicy::default());
    let report = report(vec![check(
        WorkflowPrerequisite::GitIdentity,
        PrerequisiteStatus::Missing,
    )]);
    let mut frontend = Frontend {
        interactive: true,
        ..Default::default()
    };
    let outcome = flow
        .resolve(&needs(&report), report, &RedactionSet::new(), &mut frontend)
        .unwrap();
    assert_eq!(frontend.identity_calls, 0);
    assert_eq!(
        outcome.grants.identity.unwrap().source,
        IdentitySource::Flag
    );
    assert!(outcome.unresolved.is_empty());
}
#[test]
fn wi0149_host_identity_choice_satisfies_consent_without_persisting_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut flow = flow(
        dir.path(),
        PrerequisiteFlags::default(),
        PrerequisitePolicy::default(),
    );
    let mut c = check(
        WorkflowPrerequisite::GitIdentity,
        PrerequisiteStatus::NeedsConsent,
    );
    c.remediations.push(Remediation::UseHostIdentity {
        name: "Ada".into(),
        email: "ada@example.com".into(),
    });
    let report = report(vec![c]);
    let mut frontend = Frontend {
        interactive: true,
        ..Default::default()
    };
    let outcome = flow
        .resolve(&needs(&report), report, &RedactionSet::new(), &mut frontend)
        .unwrap();
    assert_eq!(frontend.identity_calls, 1);
    assert_eq!(
        outcome.grants.identity.unwrap().source,
        IdentitySource::Host
    );
    assert_eq!(
        outcome.report.checks[0].status,
        PrerequisiteStatus::Satisfied
    );
    assert!(!dir.path().join("prerequisite-grants.json").exists());
    assert!(!dir.path().join(".git/config").exists());
}
#[test]
fn wi0149_headless_ask_fails_static_but_warns_for_assumed_all() {
    let dir = tempfile::tempdir().unwrap();
    for origin in [PrerequisiteOrigin::Static, PrerequisiteOrigin::AssumedAll] {
        let mut flow = flow(
            dir.path(),
            PrerequisiteFlags::default(),
            PrerequisitePolicy::default(),
        );
        let mut report = report(vec![check(
            WorkflowPrerequisite::GitHubToken {
                host: "github.com".into(),
            },
            PrerequisiteStatus::Missing,
        )]);
        report.origin = origin;
        let result = flow.resolve(
            &needs(&report),
            report,
            &RedactionSet::new(),
            &mut Frontend::default(),
        );
        if origin == PrerequisiteOrigin::Static {
            let err = result.err().expect("static missing credential must fail");
            assert_eq!(err.exit_code(), 2);
            assert_eq!(err.stage(), Some("workflow_prerequisites"));
        } else {
            assert!(result.unwrap().grants.is_empty());
        }
    }
}

#[test]
fn unusable_ssh_policy_and_remembered_key_warn_without_granting() {
    use crate::data::fs::prerequisite_grants::{
        GrantKey, PrerequisiteGrantStore, RememberedGrant, RememberedKind,
    };
    for missing_remembered_key in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = PrerequisitePolicy::default();
        policy.unresolved = Some(UnresolvedMode::Warn);
        let expected = if missing_remembered_key {
            PrerequisiteGrantStore::load(dir.path())
                .remember(
                    GrantKey {
                        git_root: dir.path().canonicalize().unwrap(),
                        remote_url: String::new(),
                        prerequisite: RememberedKind::Ssh,
                    },
                    RememberedGrant::SshKey {
                        key: dir.path().join(".ssh/missing"),
                    },
                )
                .unwrap();
            "remembered SSH key is no longer usable"
        } else {
            policy.ssh.use_ = CredentialUse::Allow;
            "ssh.use: allow needs ssh.key"
        };
        let mut flow = flow(dir.path(), PrerequisiteFlags::default(), policy);
        let report = report(vec![check(
            WorkflowPrerequisite::PushAuthSsh {
                host: "github.com".into(),
                remote: "origin".into(),
            },
            PrerequisiteStatus::NeedsConsent,
        )]);
        let mut frontend = Frontend::default();
        let outcome = flow
            .resolve(&needs(&report), report, &RedactionSet::new(), &mut frontend)
            .unwrap();
        assert!(outcome.grants.is_empty());
        assert!(
            frontend
                .messages
                .iter()
                .any(|message| message.contains(expected)),
            "{:?}",
            frontend.messages
        );
    }
}
#[test]
fn wi0149_missing_tool_abort_returns_exit_130() {
    let dir = tempfile::tempdir().unwrap();
    let mut flow = flow(
        dir.path(),
        PrerequisiteFlags::default(),
        PrerequisitePolicy::default(),
    );
    let report = report(vec![check(
        WorkflowPrerequisite::ContainerTool { name: "gh".into() },
        PrerequisiteStatus::Missing,
    )]);
    let err = flow
        .resolve(
            &needs(&report),
            report,
            &RedactionSet::new(),
            &mut Frontend {
                interactive: true,
                abort_tool: true,
                ..Default::default()
            },
        )
        .err()
        .unwrap();
    assert!(matches!(err, CommandError::Aborted));
    assert_eq!(err.exit_code(), 130);
}
#[test]
fn wi0149_generated_workflow_drops_unused_ssh_and_token_grants() {
    let dir = tempfile::tempdir().unwrap();
    let mut flow = flow(
        dir.path(),
        PrerequisiteFlags::default(),
        PrerequisitePolicy::default(),
    );
    let workflow: Workflow =
        toml::from_str("[[steps]]\nname='work'\nagent='claude'\nprompt_template='work'\n").unwrap();
    let mut grants = PrerequisiteGrants {
        ssh: Some(SshGrant::WholeDir {
            ssh_dir: dir.path().join(".ssh"),
        }),
        token: Some(TokenGrant {
            secret: crate::engine::auth::credential::SecretString::new("fake_token_0149"),
            source: TokenGrantSource::Pasted,
            host: "github.com".into(),
            acquire: None,
        }),
        ..Default::default()
    };
    let (_, report) = flow
        .narrow_after_generation(
            &workflow,
            &mut grants,
            &RedactionSet::new(),
            &mut Frontend::default(),
        )
        .unwrap();
    assert!(grants.is_empty());
    assert!(report
        .notes
        .iter()
        .any(|n| n == "granted but unused: SSH key (not mounted)"));
    assert!(report
        .notes
        .iter()
        .any(|n| n == "granted but unused: GitHub token (not passed)"));
}

#[cfg(unix)]
#[test]
fn wi0149_repo_acquire_requires_exact_hash_approval_in_all_headless_frontends() {
    use crate::data::fs::agent_trust::{TrustKey, TrustStore};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("acquire");
    let marker = dir.path().join("spawned");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ntouch '{}'\necho fake_token_from_acquire_0149\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut policy = PrerequisitePolicy::default();
    policy.unresolved = Some(UnresolvedMode::Warn);
    policy.github_token.acquire = Some(AcquirePolicy {
        argv: vec![program.to_str().unwrap().into()],
        timeout_secs: 2,
        refresh: AcquireRefresh::Start,
        scope: AcquireScope::Repo,
        sha256: "approved-hash".into(),
    });
    let flags = PrerequisiteFlags {
        token: Some(TokenFlag::Acquire),
        ..Default::default()
    };
    let r = report(vec![check(
        WorkflowPrerequisite::GitHubToken {
            host: "github.com".into(),
        },
        PrerequisiteStatus::Missing,
    )]);
    for kind in [
        FrontendKind::Api,
        FrontendKind::SquadDaemon,
        FrontendKind::Cli,
    ] {
        let session = Session::for_tests_isolated(dir.path(), dir.path());
        let mut opts = WorkflowPrerequisiteFlowOptions::for_session(
            &session, kind, &flags, None, r.origin, None,
        );
        opts.policy = policy.clone();
        let mut flow =
            WorkflowPrerequisiteFlow::new(&Engines::for_tests(dir.path()), &session, opts);
        let outcome = flow
            .resolve(
                &needs(&r),
                r.clone(),
                &RedactionSet::new(),
                &mut Frontend::default(),
            )
            .unwrap();
        assert!(outcome.grants.is_empty());
        assert!(
            !marker.exists(),
            "unapproved repo command executed for {kind:?}"
        );
    }
    let key = TrustKey::Repo {
        git_root: dir.path().to_path_buf(),
        name: ACQUIRE_TRUST_NAME.into(),
    };
    let mut trust = TrustStore::load(dir.path());
    trust
        .approve(key, ACQUIRE_TRUST_NAME, "different-hash")
        .unwrap();
    let mut f = flow(dir.path(), flags, policy);
    let outcome = f
        .resolve(
            &needs(&r),
            r,
            &RedactionSet::new(),
            &mut Frontend::default(),
        )
        .unwrap();
    assert!(
        outcome.grants.is_empty(),
        "approval for different argv hash must not grant"
    );
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn wi0149_before_teardown_refreshes_acquire_and_redacts_both_token_generations() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    let script = installed.path().join("acquire");
    let marker = dir.path().join("called");
    std::fs::write(&script, format!("#!/bin/sh\nif [ -f '{}' ]; then echo refreshed_token_wi0149; else touch '{}'; echo initial_token_wi0149; fi\n", marker.display(), marker.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut policy = PrerequisitePolicy::default();
    policy.github_token.use_ = CredentialUse::Allow;
    policy.github_token.env_vars = vec!["WI0149_UNSET_FOR_REFRESH".into()];
    policy.github_token.acquire = Some(AcquirePolicy {
        argv: vec![script.to_str().unwrap().into()],
        timeout_secs: 3,
        refresh: AcquireRefresh::BeforeTeardown,
        scope: AcquireScope::Global,
        sha256: "fixture".into(),
    });
    let mut flow = flow(dir.path(), PrerequisiteFlags::default(), policy);
    let report = report(vec![check(
        WorkflowPrerequisite::GitHubToken {
            host: "github.com".into(),
        },
        PrerequisiteStatus::Missing,
    )]);
    let needs = needs(&report);
    let redaction = RedactionSet::new();
    let mut frontend = Frontend::default();
    let mut outcome = flow
        .resolve(&needs, report, &redaction, &mut frontend)
        .unwrap();
    assert_eq!(
        outcome.grants.token.as_ref().unwrap().secret.expose(),
        "initial_token_wi0149"
    );
    let report =
        flow.recheck_before_teardown(&needs, &mut outcome.grants, &redaction, &mut frontend);
    assert_eq!(
        outcome.grants.token.unwrap().secret.expose(),
        "refreshed_token_wi0149"
    );
    assert_eq!(report.phase, PrerequisitePhase::BeforeTeardown);
    assert_eq!(
        redaction.redact("initial_token_wi0149 refreshed_token_wi0149"),
        "*** ***"
    );
}

#[test]
fn wi0149_before_teardown_unset_env_emits_one_summary_without_prompting() {
    let dir = tempfile::tempdir().unwrap();
    let f = flow(
        dir.path(),
        PrerequisiteFlags::default(),
        PrerequisitePolicy::default(),
    );
    let r = report(vec![check(
        WorkflowPrerequisite::GitHubToken {
            host: "github.com".into(),
        },
        PrerequisiteStatus::Satisfied,
    )]);
    let mut grants = PrerequisiteGrants {
        token: Some(TokenGrant {
            secret: crate::engine::auth::credential::SecretString::new("old_env_token_0149"),
            source: TokenGrantSource::Env("WI0149_INTENTIONALLY_UNSET".into()),
            host: "github.com".into(),
            acquire: None,
        }),
        ..Default::default()
    };
    let mut frontend = Frontend::default();
    let r = f.recheck_before_teardown(&needs(&r), &mut grants, &RedactionSet::new(), &mut frontend);
    assert!(r
        .checks
        .iter()
        .any(|c| c.status == PrerequisiteStatus::Missing));
    assert_eq!(
        frontend
            .messages
            .iter()
            .filter(|m| m.starts_with("workflow prerequisites before teardown:"))
            .count(),
        1
    );
    assert_eq!(frontend.identity_calls, 0);
}

#[cfg(unix)]
#[test]
fn wi0149_repo_allow_and_acquire_cannot_grant_to_api_squad_or_headless_cli() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let home = dir.path().join("home");
    std::fs::create_dir_all(repo.join(".awman")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let program = dir.path().join("acquire");
    let marker = dir.path().join("spawned");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ntouch '{}'\necho forbidden_repo_token_0149\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let key = home.join(".ssh/id_ed25519");
    std::fs::write(&key, "-----BEGIN PRIVATE KEY-----\nfixture\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(repo.join(".awman/config.json"), serde_json::json!({"workflows":{"prerequisites":{"ssh":{"use":"allow","key":key},"githubToken":{"use":"allow","acquire":{"argv":[program]}}}}}).to_string()).unwrap();
    for kind in [
        FrontendKind::Api,
        FrontendKind::SquadDaemon,
        FrontendKind::Cli,
    ] {
        let session = Session::for_tests_with_env(
            &repo,
            crate::data::config::env::EnvSnapshot::with_overrides([
                ("HOME", home.to_str().unwrap()),
                ("AWMAN_CONFIG_HOME", home.to_str().unwrap()),
            ]),
        );
        let mut opts = WorkflowPrerequisiteFlowOptions::for_session(
            &session,
            kind,
            &PrerequisiteFlags::default(),
            None,
            PrerequisiteOrigin::AssumedAll,
            None,
        );
        assert_ne!(opts.policy.ssh.use_, CredentialUse::Allow);
        opts.policy.unresolved = Some(UnresolvedMode::Warn);
        let mut f = WorkflowPrerequisiteFlow::new(&Engines::for_tests(&repo), &session, opts);
        let r = report(vec![
            check(
                WorkflowPrerequisite::PushAuthSsh {
                    host: "github.com".into(),
                    remote: "origin".into(),
                },
                PrerequisiteStatus::NeedsConsent,
            ),
            check(
                WorkflowPrerequisite::GitHubToken {
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Missing,
            ),
        ]);
        let outcome = f
            .resolve(
                &needs(&r),
                r,
                &RedactionSet::new(),
                &mut Frontend::default(),
            )
            .unwrap();
        assert!(outcome.grants.is_empty(), "{kind:?}");
        assert!(!marker.exists(), "repo command ran for {kind:?}");
    }
}

#[test]
fn changed_backup_destination_drops_an_existing_grant_despite_unchanged_origin() {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap()
            .status
            .success())
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "git@github.com:org/repo.git"]);
    git(&["remote", "add", "backup", "git@trusted.test:org/repo.git"]);
    let flags = PrerequisiteFlags {
        ssh: Some(SshFlag::All),
        ..Default::default()
    };
    let policy = PrerequisitePolicy {
        unresolved: Some(UnresolvedMode::Warn),
        ..Default::default()
    };
    let mut flow = flow(root.path(), flags, policy);
    let report = report(vec![check(
        WorkflowPrerequisite::PushAuthSsh {
            remote: "backup".into(),
            host: "trusted.test".into(),
        },
        PrerequisiteStatus::NeedsConsent,
    )]);
    let mut grants = flow
        .resolve(
            &needs(&report),
            report,
            &RedactionSet::new(),
            &mut Frontend::default(),
        )
        .unwrap()
        .grants;
    assert!(grants.ssh.is_some());
    assert!(grants.allows_ssh(&GitRemoteUrl::parse("git@trusted.test:org/repo.git").redacted));
    git(&["remote", "set-url", "backup", "git@evil.test:org/repo.git"]);
    let workflow: Workflow = toml::from_str(
        "steps=[]\n[[teardown]]\ntype='push_branch'\nremote='backup'\nbranch='main'\n",
    )
    .unwrap();
    let _ = flow
        .narrow_after_generation(
            &workflow,
            &mut grants,
            &RedactionSet::new(),
            &mut Frontend::default(),
        )
        .unwrap();
    assert!(grants.ssh.is_none());
    assert!(!grants.allows_ssh(&GitRemoteUrl::parse("git@evil.test:org/repo.git").redacted));
}

#[test]
fn remotely_stored_task_policy_is_rechecked_after_operator_revocation() {
    let mut task = PrerequisitePolicy::from_flags(&PrerequisiteFlags {
        ssh: Some(SshFlag::All),
        ..Default::default()
    });
    task.ssh.source = PolicySource::RemoteTask;
    let mut operator = PrerequisitePolicy {
        allow_api_client_grants: true,
        ..Default::default()
    };
    check_task_widening(&task, &operator).unwrap();
    operator.allow_api_client_grants = false;
    assert!(check_task_widening(&task, &operator).is_err());
}

#[test]
fn check_only_exit_code_requires_satisfaction_including_consent_and_unknown_boundaries() {
    for status in [
        PrerequisiteStatus::Satisfied,
        PrerequisiteStatus::NeedsConsent,
        PrerequisiteStatus::Missing,
        PrerequisiteStatus::Unknown,
    ] {
        let report = report(vec![check(WorkflowPrerequisite::GitIdentity, status)]);
        assert_eq!(
            WorkflowPrerequisiteFlow::check_only_exit_code(&report),
            if status == PrerequisiteStatus::Satisfied {
                0
            } else {
                1
            }
        );
    }
}
