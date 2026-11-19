//! Whole-token CLI/TUI/API ordering and task-presence admission.
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{catalogue::CommandCatalogue, Dispatch, Engines};
use awman::data::session::{Session, SessionOpenOptions};
use awman::frontend::{api::command_frontend::ApiDispatchFrontend, cli::CliFrontend};
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use serde_json::json;
use std::sync::Arc;

fn strings(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|s| (*s).into()).collect()
}

#[test]
fn cli_tui_and_api_share_ordering_and_literal_boundaries() {
    let catalogue = CommandCatalogue::get();
    for tokens in [
        &["review", "--model=m"][..],
        &["review", "-nn"][..],
        &["review", "--set=agents.stuckTimeout=60s"][..],
        &["review", "--", "--agent=claude"][..],
        &["--agent-flag", "--headless", "review"][..],
    ] {
        let raw = catalogue
            .parse_raw_args(&["exec", "prompt"], &strings(tokens))
            .unwrap_err();
        assert_eq!(raw.exit_code(), 2);
        let tui = Dispatch::<ApiDispatchFrontend>::parse_command_box_input(&format!(
            "exec prompt {}",
            tokens.join(" ")
        ))
        .unwrap_err();
        assert_eq!(tui.code(), "invalid_request");
        let mut argv = strings(&["awman", "exec", "prompt"]);
        argv.extend(strings(tokens));
        let matches = catalogue
            .build_clap_command()
            .try_get_matches_from(&argv)
            .unwrap();
        let error = CliFrontend::new(matches).with_argv(&argv).err().unwrap();
        assert_eq!(error.code(), "invalid_request");
    }
    for tokens in [
        &["--", "--model"][..],
        &["Explain --model and --headless"][..],
        &[
            "--agent-flag",
            "--effort",
            "--agent-flag",
            "bogus",
            "review",
        ][..],
        &["review", "--", "unknown"][..],
    ] {
        let parsed = catalogue
            .parse_raw_args(&["exec", "prompt"], &strings(tokens))
            .unwrap();
        let mut argv = strings(&["awman", "exec", "prompt"]);
        argv.extend(strings(tokens));
        let cli = catalogue.parse_cli_tail(&argv).unwrap().unwrap();
        assert_eq!(cli, parsed);
        let command_box = format!(
            "exec prompt {}",
            tokens
                .iter()
                .map(|token| format!("'{token}'"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let tui = Dispatch::<ApiDispatchFrontend>::parse_command_box_input(&command_box).unwrap();
        assert_eq!(tui.args, parsed);
        let matches = catalogue
            .build_clap_command()
            .try_get_matches_from(&argv)
            .unwrap();
        let frontend = CliFrontend::new(matches).with_argv(&argv).unwrap();
        use awman::command::dispatch::CommandFrontend;
        assert_eq!(
            frontend.arguments(&["exec", "prompt"], "prompt").unwrap(),
            parsed.arguments("prompt")
        );
        assert_eq!(frontend.literal_tail(), parsed.literal_tail());
    }
}

#[test]
fn cli_prompt_reconstruction_fails_closed_on_unclassified_prefix_tokens() {
    let catalogue = CommandCatalogue::get();
    let valid = strings(&["awman", "exec", "prompt", "task"]);
    let matches = catalogue
        .build_clap_command()
        .try_get_matches_from(&valid)
        .unwrap();
    let frontend = CliFrontend::new(matches);
    let malformed = strings(&["awman", "-nx", "exec", "prompt", "task"]);
    let error = match frontend.with_argv(&malformed) {
        Ok(_) => panic!("unclassified CLI prefix must not fall back to clap-captured args"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "invalid_request");
    assert!(error.to_string().contains("could not safely reconstruct"));
}

#[tokio::test]
async fn blank_task_is_usage_error_in_every_interaction_mode_before_runtime_work() {
    let s = crate::support::sandbox("command::misplaced_flags::blank_task_is_usage_error_in_every_interaction_mode_before_runtime_work");
    ConfigTree::new()
        .global(json!({"runtime":{"kind":"docker"}}))
        .write(&s, None);
    let repo = RepoBuilder::new(&s, "blank-input")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    let env = crate::support::env_snapshot(&s);
    let session = Session::open_at_git_root(
        repo.path().into(),
        repo.path().into(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::for_env(env).unwrap();
    let mark = docker.calls().len();
    for mode in [None, Some("--headless"), Some("--interactive")] {
        for task in [None, Some(""), Some(" \n\t ")] {
            let mut args = Vec::new();
            if let Some(mode) = mode {
                args.push(mode.to_string());
            }
            if let Some(task) = task {
                args.push(task.to_string());
            }
            let bus = EventBus::new(32);
            let frontend = ApiDispatchFrontend::new("exec prompt", &args, bus.sender());
            let error = Dispatch::new(
                frontend,
                Arc::new(tokio::sync::RwLock::new(session.clone())),
                engines.clone(),
            )
            .run_command(&["exec", "prompt"])
            .await
            .unwrap_err();
            assert_eq!(
                (error.exit_code(), error.http_status(), error.code()),
                (2, 400, "invalid_request"),
                "{mode:?}/{task:?}: {error}"
            );
            assert!(error.to_string().contains("chat --automation"));
        }
    }
    assert_eq!(
        docker.calls().len(),
        mark,
        "blank tasks must not prepare or launch"
    );
    s.finish();
}

#[test]
fn shipped_cli_rejects_misplaced_flags_and_blank_tasks_with_usage_exit() {
    let s = crate::support::sandbox("command::misplaced_flags::shipped_cli_rejects_misplaced_flags_and_blank_tasks_with_usage_exit");
    ConfigTree::new()
        .global(json!({"runtime":{"kind":"docker"},"tui":{"backend":"builtin"}}))
        .write(&s, None);
    let repo = RepoBuilder::new(&s, "cli-input")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    for args in [
        &["exec", "prompt"][..],
        &["exec", "prompt", "--interactive", "  "][..],
        &["exec", "prompt", "review", "--model=m"][..],
        &[
            "exec",
            "prompt",
            "review",
            "--",
            "--set=agents.stuckTimeout=60s",
        ][..],
    ] {
        let result = crate::support::awman(&s)
            .args(args)
            .current_dir(repo.path())
            .run();
        assert_eq!(
            result.exit,
            awman_test_support::Exit::Code(2),
            "{args:?}: {}",
            result.stderr_text()
        );
        assert!(
            result.stderr_text().contains(if args.contains(&"review") {
                "before the prompt"
            } else {
                "requires a nonblank task"
            }),
            "{}",
            result.stderr_text()
        );
    }
    assert!(!docker.calls().iter().any(|call| call
        .argv
        .first()
        .is_some_and(|a| a == "run" || a == "build")));
    s.finish();
}
