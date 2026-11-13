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
fn session_git_socket_is_mounted_only_for_the_exact_session_repository() {
    use awman::data::{
        git_transport::{GitAuthKind, GitAuthSpec},
        workflow_prerequisites::{GitRemoteUrl, PrerequisiteStepKind},
        workflow_state::PhaseKind,
    };
    use awman::engine::{
        git::{credential_socket::CacheSocketListener, lease_transport::CredentialScope},
        overlay::prerequisite_grants::GrantTarget,
        workflow::{prerequisite_grants::PrerequisiteGrants, step_commands::PushAuth},
    };
    let s = crate::support::sandbox(
        "engine::overlay::session_git_socket_is_mounted_only_for_the_exact_session_repository",
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
    for (url, allowed) in [
        ("https://git.example/team/repo.git", true),
        ("https://other.example/team/repo.git", false),
        ("https://git.example/team/other.git", false),
    ] {
        let remote = GitRemoteUrl::parse(url);
        let grant = engines
            .overlay_engine
            .prerequisite_grant_overlays(
                &GrantTarget {
                    repo: None,
                    git_source: None,
                    step_kind: PrerequisiteStepKind::PushBranch,
                    phase: PhaseKind::Teardown,
                    container_home: Some("/root"),
                    remote: Some(&remote),
                    session_git_socket: Some(&listener),
                },
                &PrerequisiteGrants::default(),
            )
            .unwrap();
        assert_eq!(
            matches!(grant.push_auth, PushAuth::SessionSocket { .. }),
            allowed,
            "{url}"
        );
        assert_eq!(
            grant
                .overlays
                .iter()
                .any(|overlay| overlay.host_path == listener.dir()),
            allowed,
            "{url}"
        );
    }
}
