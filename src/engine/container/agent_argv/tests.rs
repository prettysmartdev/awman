use super::*;
use crate::engine::container::options::{ContainerName, Entrypoint, ImageRef};

fn tail(options: &ResolvedContainerOptions) -> Vec<String> {
    AgentArgv::from_options(options)
        .unwrap()
        .append_to(Vec::new())
}

#[test]
fn maps_agent_flags_without_splitting_values() {
    let options = ResolvedContainerOptions {
        interactive: true,
        agent_mode_flags: vec!["--permission-mode".into(), "plan".into()],
        allowed_tools: vec!["Bash(git status)".into(), "Read".into()],
        allowed_tools_flag: Some("--allowedTools".into()),
        model: Some(ModelFlagForm::Argument("model with spaces".into())),
        system_prompt_inline: Some(("--system".into(), "first\nsecond".into())),
        seeded_prompt: Some("task with spaces".into()),
        ..Default::default()
    };
    assert_eq!(
        AgentArgv::from_options(&options)
            .unwrap()
            .append_to(vec!["claude".into()]),
        vec![
            "claude",
            "--permission-mode",
            "plan",
            "--allowedTools",
            "Bash(git status),Read",
            "--model",
            "model with spaces",
            "--system",
            "first\nsecond",
            "task with spaces",
        ]
    );
}

#[test]
fn emits_every_field_in_the_fixed_order() {
    let options = ResolvedContainerOptions {
        interactive: true,
        non_interactive_flag: Some("exec".into()),
        agent_mode_flags: vec!["--yolo".into()],
        disallowed_tools: vec!["Write".into(), "Bash".into()],
        disallowed_tools_flag: Some("--deny".into()),
        allowed_tools: vec!["Read".into()],
        allowed_tools_flag: Some("--allow".into()),
        model: Some(ModelFlagForm::Shorthand("--model=m".into())),
        system_prompt_file: Some(("/host/p".into(), "/tmp/p.md".into(), "--sp".into())),
        system_prompt_inline: Some(("--si".into(), "text".into())),
        agent_add_dirs: vec![("--add-dir".into(), "/ctx".into())],
        seeded_prompt: Some("go".into()),
        interactive_seed_flag: Some("--prompt".into()),
        ..Default::default()
    };
    assert_eq!(
        tail(&options),
        vec![
            "exec",
            "--yolo",
            "--deny",
            "Write,Bash",
            "--allow",
            "Read",
            "--model=m",
            "--sp",
            "/tmp/p.md",
            "--si",
            "text",
            "--add-dir",
            "/ctx",
            "--prompt",
            "go",
        ]
    );
}

#[test]
fn acp_tail_is_empty() {
    let options = ResolvedContainerOptions {
        acp: true,
        interactive: true,
        non_interactive_flag: Some("exec".into()),
        agent_mode_flags: vec!["--yolo".into()],
        seeded_prompt: Some("go".into()),
        ..Default::default()
    };
    assert!(tail(&options).is_empty());
}

#[test]
fn acp_with_a_tool_restriction_is_refused_even_with_a_flag() {
    for (allowed, disallowed) in [(vec!["Read"], vec![]), (vec![], vec!["Write"])] {
        let options = ResolvedContainerOptions {
            acp: true,
            allowed_tools: allowed.into_iter().map(String::from).collect(),
            disallowed_tools: disallowed.into_iter().map(String::from).collect(),
            allowed_tools_flag: Some("--allowed".into()),
            disallowed_tools_flag: Some("--disallowed".into()),
            ..Default::default()
        };
        assert!(matches!(
            AgentArgv::from_options(&options),
            Err(EngineError::Config(_))
        ));
    }
}

#[test]
fn seeded_prompt_is_argv_only_when_interactive() {
    let mut options = ResolvedContainerOptions {
        seeded_prompt: Some("go".into()),
        ..Default::default()
    };
    assert!(tail(&options).is_empty(), "piped on stdin instead");
    options.interactive = true;
    assert_eq!(tail(&options), vec!["go"]);
}

#[test]
fn missing_tool_flag_fails_closed_with_a_typed_error() {
    for (restriction, options) in [
        (
            ToolRestriction::Disallowed,
            ResolvedContainerOptions {
                disallowed_tools: vec!["Write".into()],
                ..Default::default()
            },
        ),
        (
            ToolRestriction::Allowed,
            ResolvedContainerOptions {
                allowed_tools: vec!["Read".into()],
                ..Default::default()
            },
        ),
    ] {
        match AgentArgv::from_options(&options) {
            Err(EngineError::ToolRestrictionFlagMissing { restriction: got }) => {
                assert_eq!(got, restriction)
            }
            other => panic!("expected ToolRestrictionFlagMissing, got {other:?}"),
        }
    }
    let message = EngineError::ToolRestrictionFlagMissing {
        restriction: ToolRestriction::Disallowed,
    }
    .to_string();
    assert!(message.starts_with("invalid configuration:"), "{message}");
    assert!(message.contains("disallowed-tools"), "{message}");
}

#[test]
fn empty_tool_lists_need_no_flag() {
    assert!(tail(&ResolvedContainerOptions::default()).is_empty());
}

#[test]
fn builtin_strict_rendering_refuses_relative_container_paths() {
    let options = ResolvedContainerOptions {
        agent_add_dirs: vec![("--add-dir".into(), "relative/ctx".into())],
        ..Default::default()
    };
    assert!(matches!(
        AgentArgv::from_options_strict(&options),
        Err(EngineError::Config(_))
    ));
    let options = ResolvedContainerOptions {
        system_prompt_file: Some(("/h".into(), "p.md".into(), "--sp".into())),
        ..Default::default()
    };
    assert!(matches!(
        AgentArgv::from_options_strict(&options),
        Err(EngineError::Config(_))
    ));
}

#[cfg(unix)]
#[test]
fn docker_and_apple_render_container_paths_as_before() {
    use std::os::unix::ffi::OsStringExt;
    let bad = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/c\xffx".to_vec()));
    let options = ResolvedContainerOptions {
        agent_add_dirs: vec![
            ("--add-dir".into(), "relative/ctx".into()),
            ("--add-dir".into(), bad),
        ],
        ..Default::default()
    };
    assert_eq!(
        tail(&options),
        vec!["--add-dir", "relative/ctx", "--add-dir", "/c\u{FFFD}x"]
    );
}

/// Docker (and Apple, which shares `build_run_argv`) put exactly the
/// entrypoint and this tail after the image.
#[test]
fn docker_run_argv_ends_with_entrypoint_and_tail() {
    let options = ResolvedContainerOptions {
        interactive: true,
        entrypoint: Some(Entrypoint(vec!["claude".into()])),
        agent_mode_flags: vec!["--yolo".into()],
        disallowed_tools: vec!["Write".into()],
        disallowed_tools_flag: Some("--disallowedTools".into()),
        seeded_prompt: Some("go".into()),
        ..Default::default()
    };
    let image = ImageRef::new("img:latest");
    let argv = crate::engine::container::docker::build_run_argv(
        &ContainerName::new("n"),
        &image,
        &options,
    )
    .unwrap();
    let after_image = &argv[argv.iter().position(|a| a == &image.0).unwrap() + 1..];
    let mut expected = vec!["claude".to_string()];
    expected.extend(tail(&options));
    assert_eq!(after_image, expected);
}

/// Decision 8: Docker and Apple used to drop a tool list silently when the
/// agent had no flag for it; now they refuse the launch like builtin does.
#[test]
fn docker_run_argv_refuses_a_tool_list_without_a_flag() {
    let options = ResolvedContainerOptions {
        entrypoint: Some(Entrypoint(vec!["codex".into()])),
        disallowed_tools: vec!["Write".into()],
        ..Default::default()
    };
    assert!(matches!(
        crate::engine::container::docker::build_run_argv(
            &ContainerName::new("n"),
            &ImageRef::new("img"),
            &options
        ),
        Err(EngineError::ToolRestrictionFlagMissing {
            restriction: ToolRestriction::Disallowed
        })
    ));
}

/// The refusal happens at `build()`, before a lease is registered or a host
/// CLI child exists, on both CLI backends.
#[test]
fn docker_and_apple_refuse_at_build_time() {
    use crate::engine::container::ContainerRuntime;
    for runtime in [ContainerRuntime::docker(), ContainerRuntime::apple()] {
        let options = ResolvedContainerOptions {
            image: Some(ImageRef::new("img:latest")),
            entrypoint: Some(Entrypoint(vec!["codex".into()])),
            allowed_tools: vec!["Read".into()],
            ..Default::default()
        };
        assert!(matches!(
            runtime.build(options),
            Err(EngineError::ToolRestrictionFlagMissing {
                restriction: ToolRestriction::Allowed
            })
        ));
    }
}

#[test]
fn appends_agent_flags_before_the_prompt_and_after_acp_entrypoint() {
    let options = ResolvedContainerOptions {
        interactive: true,
        model: Some(ModelFlagForm::Argument("gpt-5".into())),
        agent_flags: vec!["--no-daemon".into()],
        seeded_prompt: Some("task".into()),
        ..Default::default()
    };
    assert_eq!(
        AgentArgv::from_options(&options)
            .unwrap()
            .append_to(vec!["codex".into()]),
        vec!["codex", "--model", "gpt-5", "--no-daemon", "task"]
    );
    let mut acp = options;
    acp.acp = true;
    assert_eq!(
        AgentArgv::from_options(&acp)
            .unwrap()
            .append_to(vec!["cline".into(), "--acp".into()]),
        vec!["cline", "--acp", "--no-daemon"]
    );
}
