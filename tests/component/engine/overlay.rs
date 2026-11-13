//! Overlay environment delivery through the agent option builder.

use awman::command::dispatch::Engines;
use awman::data::secret::{CommandSecrets, SecretString};
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::agent::AgentRunOptions;
use awman::engine::container::options::ResolvedContainerOptions;
use awman_test_support::fixtures::RepoBuilder;
use std::sync::Arc;

#[test]
fn agent_builder_injects_only_overlay_requested_secrets_as_private_overrides() {
    let mut s = crate::support::sandbox("engine::overlay::agent_builder_injects_only_overlay_requested_secrets_as_private_overrides");
    let value = "test-secret-builder";
    s.redact(value);
    s.env_mut().set("TOKEN", "host-token");
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    let repo = RepoBuilder::new(&s, "secret-overlay")
        .file("README.md", "fixture")
        .init();
    let session = Session::open_at_git_root(
        repo.path().into(),
        repo.path().into(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::build(&session).unwrap();
    let overlays = awman::data::config::overlays::parse_overlay_list("env(TOKEN)").unwrap();
    let collected = session
        .effective_config()
        .collected_overlays(overlays, None, None)
        .unwrap();
    let options = engines
        .agent_engine
        .build_options(
            &session,
            &AgentName::new("claude").unwrap(),
            &AgentRunOptions {
                command_secrets: Some(Arc::new(
                    CommandSecrets::new([
                        ("TOKEN".into(), SecretString::new(value)),
                        (
                            "UNREQUESTED".into(),
                            SecretString::new("test-secret-unrequested"),
                        ),
                    ])
                    .unwrap(),
                )),
                env_passthrough: Some(collected.env_passthrough),
                ..Default::default()
            },
        )
        .unwrap();
    let resolved = ResolvedContainerOptions::resolve_in(options, session.env()).unwrap();
    assert_eq!(resolved.env_overrides.len(), 1);
    assert_eq!(resolved.env_overrides[0].0, "TOKEN");
    assert_eq!(resolved.env_overrides[0].1.expose(), value);
    assert_eq!(session.env().get("TOKEN"), Some("host-token"));
    assert!(!format!("{resolved:?}").contains(value));
}

#[cfg(unix)]
#[test]
fn session_git_helper_is_seeded_only_for_the_exact_session_repository() {
    use awman::data::{
        git_transport::{GitAuthKind, GitAuthSpec},
        workflow_prerequisites::{GitRemoteUrl, PrerequisiteStepKind},
        workflow_state::PhaseKind,
    };
    use awman::engine::{
        git::{credential_socket::CacheSocketListener, lease_transport::CredentialScope},
        overlay::prerequisite_grants::GrantTarget,
        workflow::{
            prerequisite_grants::{PrerequisiteGrants, TokenGrant, TokenGrantSource},
            step_commands::PushAuth,
        },
    };
    let s = crate::support::sandbox(
        "engine::overlay::session_git_helper_is_seeded_only_for_the_exact_session_repository",
    );
    let engines = Engines::for_env(crate::support::env_snapshot(&s)).unwrap();
    let listener = CacheSocketListener::start(
        &s.root().join("r"),
        Arc::new(
            GitAuthSpec::new(
                GitAuthKind::Token,
                None,
                SecretString::new("test-secret-phase-scope"),
            )
            .unwrap(),
        ),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    )
    .unwrap();
    let api_grants = PrerequisiteGrants {
        token: Some(TokenGrant {
            secret: SecretString::new("test-secret-separate-api-token"),
            source: TokenGrantSource::Pasted,
            host: "git.example".into(),
            acquire: None,
        }),
        token_destinations: ["https://git.example/team/repo.git".into()].into(),
        ..Default::default()
    };
    for (url, allowed) in [
        ("https://git.example/team/repo.git", true),
        ("https://other.example/team/repo.git", false),
        ("https://git.example/team/other.git", false),
    ] {
        for kind in [
            PrerequisiteStepKind::PushBranch,
            PrerequisiteStepKind::PullBranch,
            PrerequisiteStepKind::CreatePullRequest,
            PrerequisiteStepKind::CloneRepo,
            PrerequisiteStepKind::RunShell,
        ] {
            let allowed = allowed
                && matches!(
                    kind,
                    PrerequisiteStepKind::PushBranch
                        | PrerequisiteStepKind::PullBranch
                        | PrerequisiteStepKind::CreatePullRequest
                );
            let remote = GitRemoteUrl::parse(url);
            let grant = engines
                .overlay_engine
                .prerequisite_grant_overlays(
                    &GrantTarget {
                        repo: None,
                        git_source: None,
                        step_kind: kind,
                        phase: PhaseKind::Teardown,
                        container_home: Some("/root"),
                        remote: Some(&remote),
                        session_git_socket: Some(&listener),
                    },
                    &api_grants,
                )
                .unwrap();
            assert_eq!(
                matches!(grant.push_auth, PushAuth::SessionSocket { .. }),
                allowed,
                "{url}"
            );
            assert_eq!(grant.git_cache_seed.is_some(), allowed, "{url}");
            if kind == PrerequisiteStepKind::CloneRepo && url == "https://git.example/team/repo.git"
            {
                assert!(
                    matches!(grant.push_auth, PushAuth::HttpsToken { .. }),
                    "clone retains its explicit grant"
                );
            }
            if allowed && kind != PrerequisiteStepKind::CreatePullRequest {
                assert!(
                    grant.secret_env.is_empty(),
                    "the API token must not replace session Git auth"
                );
            }
            assert!(!format!("{grant:?}").contains("test-secret-phase-scope"));
            assert!(!grant
                .overlays
                .iter()
                .any(|overlay| overlay.host_path == listener.dir()));
            if allowed {
                assert!(grant
                    .env_literals
                    .iter()
                    .any(|(key, value)| key == "GIT_CONFIG_VALUE_1"
                        && value.starts_with("cache --socket ")));
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn headless_preflight_and_teardown_recheck_accept_only_the_session_git_scope() {
    use awman::command::commands::workflow_prerequisites::{
        WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions, WorkflowPrerequisiteFrontend,
    };
    use awman::command::dispatch::catalogue::FrontendKind;
    use awman::data::{
        git_transport::{GitAuthKind, GitAuthSpec},
        workflow_prerequisites::*,
        workflow_state::PhaseKind,
    };
    use awman::engine::git::{
        credential_socket::CacheSocketListener, lease_transport::CredentialScope,
    };
    struct Frontend;
    impl awman::data::message::UserMessageSink for Frontend {
        fn write_message(&mut self, _: awman::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl WorkflowPrerequisiteFrontend for Frontend {}
    let s = crate::support::sandbox("engine::overlay::headless_preflight_and_teardown_recheck_accept_only_the_session_git_scope");
    let repo = RepoBuilder::new(&s, "app").file("README", "fixture").init();
    let session = Session::open_at_git_root(
        repo.path().into(),
        repo.path().into(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(&s)),
            ..Default::default()
        },
    )
    .unwrap();
    let mut engines = Engines::build(&session).unwrap();
    let listener = Arc::new(
        CacheSocketListener::start(
            &s.root().join("r"),
            Arc::new(
                GitAuthSpec::new(
                    GitAuthKind::Token,
                    None,
                    SecretString::new("test-secret-preflight"),
                )
                .unwrap(),
            ),
            CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
        )
        .unwrap(),
    );
    engines.ephemeral_secrets = Arc::new(
        engines
            .ephemeral_secrets
            .with_container_git_socket(Some(listener)),
    );
    for (url, kind, accepted) in [
        (
            "https://git.example/team/repo.git",
            PrerequisiteStepKind::PushBranch,
            true,
        ),
        (
            "https://git.example/team/repo.git",
            PrerequisiteStepKind::PullBranch,
            true,
        ),
        (
            "https://git.example/team/other.git",
            PrerequisiteStepKind::PushBranch,
            false,
        ),
        (
            "https://git.example/team/repo.git",
            PrerequisiteStepKind::CloneRepo,
            false,
        ),
    ] {
        let needs = WorkflowPrerequisites {
            origin: PrerequisiteOrigin::Static,
            advisories: vec![],
            needs: vec![PrerequisiteNeed {
                prerequisite: WorkflowPrerequisite::PushAuth { remote: url.into() },
                needed_by: vec![StepRef {
                    phase: PhaseKind::Teardown,
                    kind,
                    index: 0,
                    description: "test".into(),
                }],
            }],
        };
        let options = WorkflowPrerequisiteFlowOptions::for_session(
            &session,
            FrontendKind::WORKER_LOCAL,
            &Default::default(),
            None,
            PrerequisiteOrigin::Static,
            None,
        );
        let flow = WorkflowPrerequisiteFlow::new(&engines, &session, options);
        let report = flow.preflight(&needs, false);
        assert_eq!(
            report
                .checks
                .iter()
                .all(|c| c.status == PrerequisiteStatus::Satisfied),
            accepted,
            "{url} {kind:?}"
        );
        let prepared = flow.prepare(&needs, &mut Frontend);
        assert_eq!(prepared.is_ok(), accepted, "{url} {kind:?}");
        if let Ok(mut prepared) = prepared {
            let report = prepared.flow.recheck_before_teardown(
                &prepared.needs,
                &mut prepared.grants,
                &prepared.redaction,
                &mut Frontend,
            );
            assert!(report
                .checks
                .iter()
                .all(|c| c.status == PrerequisiteStatus::Satisfied));
            assert!(
                prepared.grants.token.is_none(),
                "session Git auth must not become a GitHub API grant"
            );
        }
    }
}
