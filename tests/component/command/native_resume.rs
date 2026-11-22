//! Every provider resumes through the same production option and argv builder.
use awman::{
    data::{fs::AuthPathResolver, session::AgentName, settings_profile::NativeSessionId},
    engine::{
        agent::{first_party::FIRST_PARTY_AGENTS, AgentEngine, AgentRunOptions},
        container::options::{PlanMode, ResolvedContainerOptions},
        overlay::OverlayEngine,
    },
};
#[cfg(unix)]
use awman_test_support::TestSandbox;
use std::sync::Arc;
#[test]
fn per_agent_exact_resume_argv_parity_and_parent_option_order() {
    let s = crate::support::sandbox(
        "command::native_resume::per_agent_exact_resume_argv_parity_and_parent_option_order",
    );
    let session = super::settings_profiles::session(&s);
    let runtime = super::settings_profiles::Runtime::new();
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        AuthPathResolver::at_home(s.home()),
    ));
    let engine = AgentEngine::new(overlay, runtime);
    for agent in FIRST_PARTY_AGENTS {
        for headless in [false, true] {
            let run = AgentRunOptions {
                native_session: Some(NativeSessionId::parse("exact-conversation").unwrap()),
                non_interactive: headless,
                structured_output: true,
                initial_prompt: headless.then(|| "follow-up".into()),
                ..Default::default()
            };
            let options = ResolvedContainerOptions::resolve(
                engine
                    .build_options(&session, &AgentName::new(*agent).unwrap(), &run)
                    .unwrap(),
            )
            .unwrap();
            let argv = options.agent_argv().unwrap();
            let selector = match *agent {
                "codex" => "resume",
                "opencode" | "opencode-v2" => "--session",
                "cline" => "--id",
                "antigravity" => "--conversation",
                _ => "--resume",
            };
            if *agent == "copilot" {
                assert_eq!(
                    argv.iter()
                        .filter(|s| s.as_str() == "--resume=exact-conversation")
                        .count(),
                    1
                );
            } else {
                let at = argv.iter().position(|s| s == selector).unwrap();
                assert_eq!(argv[at + 1], "exact-conversation", "{agent} {argv:?}");
            }
            assert!(!argv.iter().any(|s| s == session.id().to_string().as_str()));
            if *agent == "codex" && headless {
                let at = argv.iter().position(|s| s == "resume").unwrap();
                assert_eq!(argv[at + 2], "-");
                assert!(argv.iter().position(|s| s == "--json").unwrap() < at);
            }
            if *agent == "copilot" && headless {
                assert_eq!(&argv[argv.len() - 2..], ["-p", "follow-up"]);
            }
        }
    }
}

#[test]
fn breaking_opencode_layouts_have_distinct_profile_identity() {
    let v1 = awman::engine::agent::first_party::first_party("opencode")
        .unwrap()
        .profile_state
        .unwrap()
        .layout_key();
    let v2 = awman::engine::agent::first_party::first_party("opencode-v2")
        .unwrap()
        .profile_state
        .unwrap()
        .layout_key();
    assert_ne!(v1, v2);
}
#[test]
fn plan_reapplied_on_resume_and_conflicting_provider_selector_refused() {
    let s = crate::support::sandbox(
        "command::native_resume::plan_reapplied_on_resume_and_conflicting_provider_selector_refused",
    );
    let session = super::settings_profiles::session(&s);
    let runtime = super::settings_profiles::Runtime::new();
    let engine = AgentEngine::new(
        Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(s.home()),
        )),
        runtime,
    );
    let run = AgentRunOptions {
        native_session: Some(NativeSessionId::parse("exact").unwrap()),
        non_interactive: true,
        plan: Some(PlanMode::Enabled),
        ..Default::default()
    };
    let resolved = ResolvedContainerOptions::resolve(
        engine
            .build_options(&session, &AgentName::new("codex").unwrap(), &run)
            .unwrap(),
    )
    .unwrap();
    let argv = resolved.agent_argv().unwrap();
    assert!(argv.windows(2).any(|w| w == ["--sandbox", "read-only"]));
    assert!(argv.windows(2).any(|w| w == ["resume", "exact"]));
    let conflict = AgentRunOptions {
        agent_flags: Some(vec!["--resume=other".into()]),
        ..run
    };
    assert!(engine
        .build_options(&session, &AgentName::new("codex").unwrap(), &conflict)
        .is_err());

    for (agent, selector) in [
        ("claude", "-r"),
        ("claude", "-c"),
        ("opencode", "-s"),
        ("opencode-v2", "-c"),
        ("copilot", "--continue=latest"),
        ("claude", "--fork-session"),
    ] {
        let conflict = AgentRunOptions {
            native_session: Some(NativeSessionId::parse("exact").unwrap()),
            agent_flags: Some(vec![selector.into()]),
            ..Default::default()
        };
        assert!(
            engine
                .build_options(&session, &AgentName::new(agent).unwrap(), &conflict)
                .is_err(),
            "{agent} accepted conflicting selector {selector}"
        );
    }
}
#[test]
fn translators_preserve_native_identity_and_never_promote_partial_to_native_completion() {
    use awman::{
        data::agent_output_format::AgentOutputFormat as F,
        engine::agent_output::{translator_for, OutputStream},
    };
    for (format, init, done) in [
        (
            F::ClaudeStreamJson,
            r#"{"type":"system","subtype":"init","session_id":"exact"}"#,
            r#"{"type":"result","result":"done"}"#,
        ),
        (
            F::CodexJson,
            r#"{"type":"thread.started","thread_id":"exact"}"#,
            r#"{"type":"turn.completed"}"#,
        ),
        (
            F::CursorStreamJson,
            r#"{"type":"system","subtype":"init","session_id":"exact"}"#,
            r#"{"type":"result","result":"done"}"#,
        ),
        (
            F::OpenCodeJson,
            r#"{"type":"step_start","sessionID":"exact"}"#,
            r#"{"type":"step_finish","part":{"reason":"stop"}}"#,
        ),
        (
            F::OpenCodeV2Json,
            r#"{"type":"step_start","sessionID":"exact"}"#,
            r#"{"type":"step_finish","part":{"reason":"stop"}}"#,
        ),
        (
            F::CopilotJson,
            r#"{"type":"session.start","data":{"sessionId":"exact"}}"#,
            r#"{"type":"session.idle"}"#,
        ),
        (
            F::ClineJson,
            r#"{"type":"task_started","taskId":"exact"}"#,
            r#"{"type":"task_completed"}"#,
        ),
        (
            F::AntigravityStreamJson,
            r#"{"type":"init","conversation_id":"exact"}"#,
            r#"{"type":"result","result":"done"}"#,
        ),
    ] {
        let mut translator = translator_for(format);
        translator.feed_line(OutputStream::Stdout, init);
        assert_eq!(
            translator.native_observation().identity.unwrap().as_str(),
            "exact"
        );
        translator.finish();
        assert!(!translator.native_observation().completed);
        translator.feed_line(OutputStream::Stdout, done);
        assert!(translator.native_observation().completed, "{format:?}");
    }
}

#[tokio::test]
async fn native_stream_requires_exact_first_identity_across_split_frames() {
    use awman::engine::{
        agent_output::{
            native_observation::{with_native_observation, NativeStream},
            AgentOutputFormat,
        },
        agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus},
    };
    struct Frontend(Option<AgentIo>);
    impl awman::data::message::UserMessageSink for Frontend {
        fn write_message(&mut self, _: awman::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl AgentFrontend for Frontend {
        fn report_status(&mut self, _: AgentStatus) {}
        fn report_progress(&mut self, _: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            self.0.take().unwrap()
        }
    }
    for returned in [Some("exact"), Some("prefix-exact"), None] {
        let (stdout, mut output) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let io = AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        };
        let stream = NativeStream::new(Some(NativeSessionId::parse("exact").unwrap()));
        let mut frontend = with_native_observation(
            Box::new(Frontend(Some(io))),
            AgentOutputFormat::CodexJson,
            stream.clone(),
        );
        let io = frontend.take_io();
        if let Some(id) = returned {
            let frame = format!("{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}\n");
            io.stdout.send(frame.as_bytes()[..10].to_vec()).unwrap();
            io.stdout.send(frame.as_bytes()[10..].to_vec()).unwrap();
        }
        io.stdout.send(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"answer\"}}\n{\"type\":\"turn.completed\"}\n".to_vec()).unwrap();
        drop(io);
        drop(frontend);
        let mut received = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(bytes) = output.recv().await {
                received.extend(bytes)
            }
        })
        .await
        .unwrap();
        assert_eq!(stream.exact_resume_confirmed(), returned == Some("exact"));
        if returned == Some("exact") {
            assert!(stream.complete());
            assert!(String::from_utf8(received).unwrap().contains("answer"));
        } else {
            assert!(received.is_empty());
        }
    }
}

#[tokio::test]
#[cfg(unix)] // settings profiles are unix-only
async fn profile_plan_patches_both_private_opencode_settings_files() {
    use awman::{
        command::commands::launch_policy::settings_profile::{enter, ProfileRequest},
        data::fs::{agent_profiles::TurnStateAtExit, private_files::create_private_dir},
        engine::agent::first_party::first_party,
    };
    let s = TestSandbox::new();
    let session = super::settings_profiles::session(&s);
    let runtime = super::settings_profiles::Runtime::new();
    create_private_dir(&s.home().join(".config/opencode")).unwrap();
    for file in ["opencode.json", "opencode.jsonc"] {
        std::fs::write(
            s.home().join(".config/opencode").join(file),
            r#"{"model":"selected","default_agent":"build","permission":{"bash":"deny"}}"#,
        )
        .unwrap();
    }
    let agent = AgentName::new("opencode-v2").unwrap();
    let run = AgentRunOptions {
        plan: Some(PlanMode::Enabled),
        ..Default::default()
    };
    let p = enter(
        &session,
        &agent,
        &ProfileRequest::parse(Some("review"), None).unwrap(),
        &run,
        &first_party("opencode-v2").unwrap(),
        runtime.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let run = AgentRunOptions {
        profile_state: Some(p.run_state()),
        ..run
    };
    let engine = AgentEngine::new(
        Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(s.home()),
        )),
        runtime.clone(),
    );
    engine.build_options(&session, &agent, &run).unwrap();
    for file in ["opencode.json", "opencode.jsonc"] {
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(p.run_state().run_dir.join(".config/opencode").join(file)).unwrap(),
        )
        .unwrap();
        assert_eq!(value["default_agent"], "plan");
        assert_eq!(value["permission"]["bash"], "deny");
    }
    p.leave(runtime, TurnStateAtExit::Unknown).await.unwrap();
}
