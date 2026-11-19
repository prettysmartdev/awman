//! Prompt delivery through the production agent option builder.

use awman::command::dispatch::Engines;
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::agent::AgentRunOptions;
use awman::engine::container::options::{OverlayPermission, ResolvedContainerOptions};
use awman_test_support::fixtures::RepoBuilder;

#[test]
fn oversized_argv_prompt_is_preserved_in_a_read_only_owned_file() {
    let s = crate::support::sandbox(
        "engine::agent::oversized_argv_prompt_is_preserved_in_a_read_only_owned_file",
    );
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    let repo = RepoBuilder::new(&s, "large-prompt").init();
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
    // Larger than Linux's single-argument limit, with multibyte text and
    // shell syntax that must survive unchanged as data.
    let prompt = format!(
        "{}\n'\" $(do-not-execute) `literal`\nEND",
        "Task 🦀\n".repeat(20_000)
    );
    for (agent, non_interactive) in [("claude", false), ("copilot", true)] {
        let options = engines
            .agent_engine
            .build_options(
                &session,
                &AgentName::new(agent).unwrap(),
                &AgentRunOptions {
                    initial_prompt: Some(prompt.clone()),
                    non_interactive,
                    ..Default::default()
                },
            )
            .unwrap();
        let resolved = ResolvedContainerOptions::resolve_in(options, session.env()).unwrap();
        let seed = resolved.seeded_prompt.as_deref().unwrap();
        assert!(seed.len() < 1024, "large prompt still travels in argv");
        let mount = resolved
            .overlays
            .iter()
            .find(|o| o.container_path.starts_with("/awman/prompts"))
            .expect("mounted prompt");
        assert_eq!(mount.permission, OverlayPermission::ReadOnly);
        assert!(seed.contains(mount.container_path.to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(&mount.host_path).unwrap(), prompt);
        assert!(mount.host_path.starts_with(s.root()));
        let path = mount.host_path.clone();
        let retained = resolved.clone();
        drop(resolved);
        assert!(path.is_file(), "another launch owner retains the prompt");
        drop(retained);
        assert!(!path.exists(), "last owner cleans up the prompt");
    }
    // Claude's headless stdin delivery has no argv-size problem; small
    // interactive prompts retain their direct delivery too.
    for (non_interactive, text) in [(true, prompt.as_str()), (false, "small task")] {
        let options = engines
            .agent_engine
            .build_options(
                &session,
                &AgentName::new("claude").unwrap(),
                &AgentRunOptions {
                    initial_prompt: Some(text.into()),
                    non_interactive,
                    ..Default::default()
                },
            )
            .unwrap();
        let resolved = ResolvedContainerOptions::resolve_in(options, session.env()).unwrap();
        assert_eq!(resolved.seeded_prompt.as_deref(), Some(text));
        assert!(!resolved
            .overlays
            .iter()
            .any(|o| o.container_path.starts_with("/awman/prompts")));
    }
}
