//! The CLI-shaped backends share phase capture and explicit stop semantics.
use awman::{
    data::{
        message::{UserMessage, UserMessageSink},
        session::AgentHandle,
    },
    engine::{
        agent_runtime::{
            frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus},
            AgentRuntimeEngine,
        },
        container::{
            options::{ContainerName, ImageRef, ResolvedContainerOptions},
            ContainerRuntime,
        },
    },
};
use std::{io::Read, time::Duration};
struct Frontend(Option<AgentIo>);
impl UserMessageSink for Frontend {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl AgentFrontend for Frontend {
    fn take_io(&mut self) -> AgentIo {
        self.0.take().unwrap()
    }
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn grace_timeout(&self) -> Duration {
        Duration::ZERO
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_backends_keep_ordered_phase_transcripts_and_propagate_failed_stop() {
    let mut s = crate::support::sandbox("engine::conformance::cli_backends_keep_ordered_phase_transcripts_and_propagate_failed_stop");
    s.env_mut().set("AWMAN_TEST_DOCKER", "1");
    s.env_mut().set("AWMAN_TEST_APPLE_CONTAINER", "1");
    let expected = (0..250)
        .map(|n| format!("phase-{n:03}\n"))
        .collect::<String>();
    for program in ["docker", "container"] {
        let fake = s.install_fake(crate::support::fake_cli_exe(), program);
        fake.on(&["run"]).stdout(&expected).exit(37);
        fake.on(&["stop"]).exit(9);
        fake.on(&["rm"]);
        let runtime = if program == "docker" {
            ContainerRuntime::docker_in(crate::support::env_snapshot(&s))
        } else {
            ContainerRuntime::apple_in(crate::support::env_snapshot(&s))
        };
        let instance = runtime
            .build(ResolvedContainerOptions {
                image: Some(ImageRef::new("fixture")),
                name: Some(ContainerName::new("awman-phase")),
                startup_grace: Some(Duration::MAX),
                full_transcript: true,
                remove_on_exit: false,
                ..Default::default()
            })
            .unwrap();
        let (stdout, mut output) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, mut errors) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut execution = instance
            .run_with_frontend(Box::new(Frontend(Some(AgentIo {
                stdout,
                stderr,
                stdin_tx,
                stdin_rx,
                initial_size: None,
                resize: None,
            }))))
            .unwrap();
        let tail = execution.output_tail().unwrap();
        assert_eq!(execution.wait().await.unwrap().exit_code, 37);
        assert!(tail.wait_for_writers(Duration::from_secs(2)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while output.recv().await.is_some() {}
            while errors.recv().await.is_some() {}
        })
        .await
        .expect("retained transcript must not keep frontend output channels open");
        let mut transcript = String::new();
        tail.transcript_reader()
            .unwrap()
            .read_to_string(&mut transcript)
            .unwrap();
        assert_eq!(transcript, expected);
        assert_eq!(tail.snapshot().len(), 100);
        let handle = AgentHandle {
            id: "awman-phase".into(),
            name: "awman-phase".into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        };
        assert!(runtime.stop(&handle).is_err());
        runtime.remove_agent(&handle.id).unwrap();
        assert!(fake
            .calls()
            .iter()
            .any(|call| call.argv == ["rm", "awman-phase"]));
    }
}

#[test]
fn apple_stopped_discovery_preserves_terminal_owned_instances() {
    let mut s = crate::support::sandbox(
        "engine::conformance::apple_stopped_discovery_preserves_terminal_owned_instances",
    );
    s.env_mut().set("AWMAN_TEST_APPLE_CONTAINER", "1");
    let fake = s.install_fake(crate::support::fake_cli_exe(), "container");
    fake.on(&["list", "--all", "--format", "json"]).stdout(
        r#"[
        {"name":"awman-stopped","status":"stopped","CreatedAt":"2026-01-01T00:00:00Z"},
        {"name":"awman-running","status":"running"},
        {"name":"awman-paused","status":"paused"},
        {"name":"unowned","status":"stopped"}
    ]"#,
    );
    let runtime = ContainerRuntime::apple_in(crate::support::env_snapshot(&s));
    let handles = runtime.list_stopped().unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].name, "awman-stopped");
    assert_eq!(runtime.list_running_all().unwrap().len(), 2);
    fake.on(&["list", "--all", "--format", "json"])
        .stdout("incomplete JSON");
    assert!(runtime.list_stopped().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_create_rechecks_the_mount_identity_after_build() {
    let mut s = crate::support::sandbox(
        "engine::conformance::runtime_create_rechecks_the_mount_identity_after_build",
    );
    s.env_mut().set("AWMAN_TEST_DOCKER", "1");
    let fake = s.install_fake(crate::support::fake_cli_exe(), "docker");
    fake.on(&["run"]);
    let root = s.work();
    let authority = awman::data::mount_authority::MountAuthority::exact_workdir(&root).unwrap();
    let guard = awman::data::mount_authority::MountSourceGuard {
        authority,
        sources: vec![(
            root.clone(),
            awman::data::mount_authority::SourceIdentity::of(&root).unwrap(),
        )],
    };
    let runtime = ContainerRuntime::docker_in(crate::support::env_snapshot(&s));
    let instance = runtime
        .build(ResolvedContainerOptions {
            image: Some(ImageRef::new("fixture")),
            name: Some(ContainerName::new("awman-mount-boundary")),
            mount_guard: Some(guard),
            ..Default::default()
        })
        .unwrap();
    std::fs::rename(&root, s.tmp().join("original-workspace")).unwrap();
    std::fs::create_dir(&root).unwrap();
    let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let result = instance.run_with_frontend(Box::new(Frontend(Some(AgentIo {
        stdout,
        stderr,
        stdin_tx,
        stdin_rx,
        initial_size: None,
        resize: None,
    }))));
    assert!(matches!(
        result,
        Err(awman::engine::error::EngineError::MountOutsideAuthority { .. })
    ));
    assert!(!fake
        .calls()
        .iter()
        .any(|call| call.argv.first().is_some_and(|arg| arg == "run")));
    s.finish();
}
