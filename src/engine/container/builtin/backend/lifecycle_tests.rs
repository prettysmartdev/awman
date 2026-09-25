//! Hermetic lifecycle tests: an in-memory driver stands in for the SDK, so no
//! VM, hypervisor, real HOME, credential store or paid agent is involved.
use super::*;
use crate::data::config::image_source::ImageSourceKind;
use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::agent_runtime::frontend::AgentStatus;
use crate::engine::container::builtin::testing::*;
use crate::engine::container::options::{
    ContainerName, CpuLimit, Entrypoint, EnvLiteral, ImageRef, MemoryLimit,
};
use std::collections::BTreeMap;

pub(super) struct Rig {
    _temp: tempfile::TempDir,
    pub driver: Arc<FakeDriver>,
    pub backend: BuiltinBackend,
}

pub(super) fn settings(state_dir: &std::path::Path) -> BuiltinRuntimeSettings {
    BuiltinRuntimeSettings {
        state_dir: state_dir.into(),
        vcpus: 2,
        memory_mib: 4096,
        image_source: None,
        images: BTreeMap::new(),
        registries: BTreeMap::new(),
        ambient_overrides: Vec::new(),
        test_isolation: false,
    }
}

impl Rig {
    pub fn new(owner: &str) -> Self {
        let (temp, state) = state_dir();
        let paths = BuiltinPaths::resolve(&state).unwrap();
        let driver = FakeDriver::new().with_image("agent:latest", fixture_image());
        let backend = Self::backend_on(&driver, &paths, &state, owner);
        Rig {
            _temp: temp,
            driver,
            backend,
        }
    }

    fn backend_on(
        driver: &Arc<FakeDriver>,
        paths: &BuiltinPaths,
        state: &std::path::Path,
        owner: &str,
    ) -> BuiltinBackend {
        BuiltinBackend {
            driver: driver.clone(),
            paths: paths.clone(),
            settings: settings(state),
            owner: owner.into(),
            background_env: Default::default(),
        }
    }

    /// Another awman process (session) sharing this state directory and store.
    pub fn second_session(&self, owner: &str) -> BuiltinBackend {
        Self::backend_on(
            &self.driver,
            &self.backend.paths,
            &self.backend.settings.state_dir,
            owner,
        )
    }
}

pub(super) fn options(name: &str) -> ResolvedContainerOptions {
    ResolvedContainerOptions {
        image: Some(ImageRef::new("agent:latest")),
        entrypoint: Some(Entrypoint::new(["claude"])),
        name: Some(ContainerName::new(name)),
        working_dir: Some("/workspace".into()),
        remove_on_exit: true,
        ..Default::default()
    }
}

fn token_of(driver: &FakeDriver, id: &str) -> String {
    driver.spec(id).labels[naming::LABEL_OWNER].clone()
}

fn new_spec(name: &str, owner: &str) -> SandboxSpec {
    SandboxSpec {
        name: naming::sandbox_name_for(name),
        image: "agent:latest".into(),
        labels: naming::labels(name, owner, &[]).unwrap(),
        mounts: Vec::new(),
        mount_owner: None,
        vcpus: 1,
        memory_mib: 256,
    }
}

/// Launch `name` through the public backend contract and return its id.
async fn launch(
    rig: &Rig,
    backend: &BuiltinBackend,
    name: &str,
) -> (
    String,
    crate::engine::agent_runtime::execution::AgentExecution,
    Peer,
) {
    let instance = backend.build(options(name)).unwrap();
    let id = instance.handle_preview().id;
    let (fe, peer) = frontend(None);
    let execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    (id, execution, peer)
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_create_exec_exit_and_remove() {
    let rig = Rig::new("session-a");
    let instance = rig.backend.build(options("awman-t1")).unwrap();
    let preview = instance.handle_preview();
    assert_eq!(preview.name, "awman-t1");
    assert_eq!(preview.id, naming::sandbox_name_for("awman-t1"));
    assert_eq!(rig.driver.creates(), 0, "build() must not create anything");

    let (fe, mut peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&preview.id).await;

    let spec = rig.driver.spec(&preview.id);
    assert_eq!(spec.image, "agent:latest");
    assert_eq!((spec.vcpus, spec.memory_mib), (2, 4096));
    assert_eq!(spec.labels[naming::LABEL_NAME], "awman-t1");
    assert_eq!(spec.labels[naming::LABEL_PROTOCOL], naming::PROTOCOL);
    let request = rig.driver.execs(&preview.id).remove(0);
    assert_eq!(request.argv, ["claude"]);
    assert_eq!(request.tty, None, "piped frontends never get a PTY");
    assert_eq!(request.user.as_deref(), Some("probe"));
    assert_eq!(request.cwd.as_deref(), Some("/workspace"));

    rig.driver
        .emit(&preview.id, ExecEvent::Stdout(vec![b'o', 0, 255]));
    rig.driver
        .emit(&preview.id, ExecEvent::Stderr(b"err".to_vec()));
    rig.driver.emit(&preview.id, ExecEvent::Exited(37));
    let info = execution.wait().await.unwrap();
    assert_eq!(
        info.exit_code, 37,
        "the guest's real exit code is propagated"
    );
    assert_eq!(peer.stdout.recv().await.unwrap(), vec![b'o', 0, 255]);
    assert_eq!(peer.stderr.recv().await.unwrap(), b"err".to_vec());

    assert!(
        rig.driver.names().is_empty(),
        "the sandbox is removed on exit"
    );
    let finished = rig.driver.finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].0, preview.id);
    assert!(finished[0].2, "remove_on_exit removes the sandbox");
    let statuses = peer.statuses.lock().unwrap().clone();
    assert!(statuses.contains(&AgentStatus::Running {
        container_name: "awman-t1".into()
    }));
    assert!(statuses.contains(&AgentStatus::Exited(37)));
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_preserves_the_stopped_sandbox_for_explicit_removal() {
    let rig = Rig::new("session-a");
    let mut kept = options("awman-keep");
    kept.remove_on_exit = false;
    let instance = rig.backend.build(kept).unwrap();
    let id = instance.handle_preview().id;
    let (fe, _peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    rig.driver.emit(&id, ExecEvent::Exited(0));
    execution.wait().await.unwrap();

    assert_eq!(rig.driver.is_stopped(&id), Some(true));
    assert!(!rig.driver.finished()[0].2, "--keep must not remove");
    let stopped = rig.backend.list_stopped().unwrap();
    assert_eq!(stopped.len(), 1);
    assert!(rig.backend.list_running_all().unwrap().is_empty());
    rig.backend.remove_agent(&id).unwrap();
    assert!(rig.driver.names().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_size_resize_and_ctrl_c_bytes_reach_the_guest_verbatim() {
    let rig = Rig::new("session-a");
    let instance = rig.backend.build(options("awman-pty")).unwrap();
    let id = instance.handle_preview().id;
    let (fe, peer) = frontend(Some((80, 24)));
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    assert_eq!(rig.driver.execs(&id)[0].tty, Some((80, 24)));

    peer.resize.as_ref().unwrap().send((132, 43)).unwrap();
    peer.stdin.send(vec![0x03]).unwrap();
    eventually("resize and raw Ctrl-C", || {
        let sent = rig.driver.controls(&id);
        sent.contains(&Sent::Resize(132, 43)) && sent.contains(&Sent::Stdin(vec![0x03]))
    })
    .await;
    rig.driver.emit(&id, ExecEvent::Exited(130));
    assert_eq!(execution.wait().await.unwrap().exit_code, 130);
}

#[tokio::test(flavor = "multi_thread")]
async fn noninteractive_seeded_prompt_is_queued_then_stdin_closes() {
    let rig = Rig::new("session-a");
    let mut seeded = options("awman-seed");
    seeded.seeded_prompt = Some("do the thing".into());
    seeded.interactive = false;
    let instance = rig.backend.build(seeded).unwrap();
    let id = instance.handle_preview().id;
    let (fe, peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    assert!(
        !execution.try_inject_stdin(b"late").unwrap(),
        "a one-shot piped agent accepts no later injection"
    );
    drop(peer.stdin);
    eventually("prompt bytes then EOF", || {
        rig.driver.controls(&id)
            == [
                Sent::Stdin(b"do the thing".to_vec()),
                Sent::Stdin(b"\n".to_vec()),
                Sent::Eof,
            ]
    })
    .await;
    rig.driver.emit(&id, ExecEvent::Exited(0));
    execution.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_keeps_persistent_binary_clean_framing() {
    let rig = Rig::new("session-a");
    let mut acp = options("awman-acp");
    acp.acp = true;
    acp.interactive = false;
    acp.non_interactive_flag = Some("--print".into());
    acp.seeded_prompt = Some("must not be injected".into());
    acp.entrypoint = Some(Entrypoint::new(["cline", "--acp"]));
    let instance = rig.backend.build(acp).unwrap();
    let id = instance.handle_preview().id;
    let (fe, mut peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    assert_eq!(rig.driver.execs(&id)[0].argv, ["cline", "--acp"]);
    assert!(execution.try_inject_stdin(b"{}").unwrap());

    let frame = vec![0, 255, b'\n', b'\r', 0];
    peer.stdin.send(frame.clone()).unwrap();
    eventually("binary frame forwarded verbatim", || {
        rig.driver
            .controls(&id)
            .contains(&Sent::Stdin(frame.clone()))
    })
    .await;
    assert!(
        !rig.driver
            .controls(&id)
            .contains(&Sent::Stdin(b"must not be injected".to_vec())),
        "ACP never receives a seeded prompt"
    );
    // Chunk boundaries and NUL/newline bytes survive unchanged in both directions.
    rig.driver
        .emit(&id, ExecEvent::Stdout(b"{\"jsonrpc\"".to_vec()));
    rig.driver
        .emit(&id, ExecEvent::Stdout(b":\"2.0\"}\n\0".to_vec()));
    assert_eq!(peer.stdout.recv().await.unwrap(), b"{\"jsonrpc\"".to_vec());
    assert_eq!(peer.stdout.recv().await.unwrap(), b":\"2.0\"}\n\0".to_vec());
    rig.driver.emit(&id, ExecEvent::Exited(0));
    execution.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_stream_is_an_error_never_exit_zero() {
    let rig = Rig::new("session-a");
    let (id, mut execution, peer) = launch(&rig, &rig.backend, "awman-fail").await;
    rig.driver.emit(&id, ExecEvent::Failed);
    assert!(execution.wait().await.is_err());
    assert!(peer
        .statuses
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, AgentStatus::Failed(_))));
    assert!(
        !peer
            .statuses
            .lock()
            .unwrap()
            .contains(&AgentStatus::Exited(0)),
        "no fabricated exit 0"
    );
    // Cleanup still ran for the owned launch.
    assert_eq!(rig.driver.finished().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_start_failure_removes_the_created_sandbox() {
    let rig = Rig::new("session-a");
    rig.driver.state.lock().unwrap().fail_exec = true;
    let instance = rig.backend.build(options("awman-noexec")).unwrap();
    let (fe, _peer) = frontend(None);
    assert!(instance.run_with_frontend(fe).is_err());
    assert!(
        rig.driver.names().is_empty(),
        "a sandbox whose exec never started must not linger"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_and_does_not_kill_an_agent_that_exits_within_grace() {
    let rig = Rig::new("session-a");
    let (id, mut execution, _peer) = launch(&rig, &rig.backend, "awman-cancel").await;
    let cancel = execution.cancel_handle().expect("cancel handle");
    cancel.cancel().unwrap();
    assert!(rig.driver.controls(&id).contains(&Sent::Interrupt));
    rig.driver.emit(&id, ExecEvent::Exited(130));
    assert_eq!(execution.wait().await.unwrap().exit_code, 130);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !rig.driver.controls(&id).contains(&Sent::Kill),
        "an agent that exits inside the grace window is never killed"
    );
    assert_eq!(rig.driver.finished().len(), 1, "cleanup runs exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_never_touches_another_sessions_sandboxes() {
    let rig = Rig::new("session-a");
    let other = rig.second_session("session-b");
    let (a_id, _a_exec, _a_peer) = launch(&rig, &rig.backend, "awman-a").await;
    let (b_id, _b_exec, _b_peer) = launch(&rig, &other, "awman-b").await;

    // Automatic cleanup paths require the launch-scoped owner token.
    assert!(rig.backend.stop_and_remove(&b_id).is_err());
    assert!(rig
        .backend
        .exec_in_background(&b_id, "true", "/", None)
        .is_err());
    assert!(rig
        .driver
        .finish_owned(&b_id, &token_of(&rig.driver, &a_id), Duration::ZERO, true)
        .is_err());
    assert!(
        rig.backend.remove_agent(&b_id).is_err(),
        "a running sandbox is never a removal candidate"
    );
    assert_eq!(rig.driver.is_stopped(&b_id), Some(false));
    assert_eq!(rig.driver.finished().len(), 0);

    // Each process still discovers both (squads rely on this).
    assert_eq!(rig.backend.list_running_all().unwrap().len(), 2);

    // B finishing its own sandbox leaves A's untouched.
    other.stop_and_remove(&b_id).unwrap();
    assert_eq!(rig.driver.is_stopped(&b_id), None);
    assert_eq!(rig.driver.is_stopped(&a_id), Some(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_stop_works_across_sessions_but_not_for_a_reused_name() {
    let rig = Rig::new("session-a");
    let other = rig.second_session("session-b");
    let (b_id, _b_exec, _b_peer) = launch(&rig, &other, "awman-b").await;
    let handle = other.list_running_all().unwrap().remove(0);

    let mut stale = handle.clone();
    stale.started_at -= chrono::Duration::seconds(5);
    assert!(
        rig.backend.stop(&stale).is_err(),
        "a handle from an older generation must not stop a replacement"
    );
    assert_eq!(rig.driver.is_stopped(&b_id), Some(false));

    rig.backend.stop(&handle).unwrap();
    assert_eq!(rig.driver.is_stopped(&b_id), None, "explicit stop removes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_protocol_mismatch_is_refused_everywhere() {
    let rig = Rig::new("session-a");
    let mut spec = new_spec("awman-old", "someone/else");
    spec.labels
        .insert(naming::LABEL_PROTOCOL.into(), "0.6.0/1/old".into());
    let id = spec.name.clone();
    rig.driver.create(spec).unwrap();
    let handle = rig.backend.list_running_all().unwrap().remove(0);

    let mismatch = |error: EngineError| matches!(error, EngineError::WorkerProtocolMismatch { .. });
    assert!(mismatch(rig.backend.attach(&handle).err().unwrap()));
    assert!(mismatch(rig.backend.stop(&handle).unwrap_err()));
    assert!(mismatch(rig.backend.stop_and_remove(&id).unwrap_err()));
    assert!(mismatch(
        rig.backend
            .exec_in_background(&id, "true", "/", None)
            .unwrap_err()
    ));
    assert_eq!(rig.driver.is_stopped(&id), Some(false), "left untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_refuses_stopped_sandboxes() {
    let rig = Rig::new("session-a");
    let mut kept = options("awman-stopped");
    kept.remove_on_exit = false;
    let instance = rig.backend.build(kept).unwrap();
    let id = instance.handle_preview().id;
    let (fe, _peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    let running = rig.backend.list_running_all().unwrap().remove(0);
    assert!(rig.backend.attach(&running).is_ok());
    rig.driver.emit(&id, ExecEvent::Exited(0));
    execution.wait().await.unwrap();
    assert!(rig.backend.attach(&running).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_filters_by_full_name_prefix_and_state() {
    let rig = Rig::new("session-a");
    let mut keep = Vec::new();
    for name in [
        "awman-squad-team-a1b2c3d4",
        "awman-squad-team-e5f6a7b8",
        "awman-4242-99",
    ] {
        keep.push(launch(&rig, &rig.backend, name).await);
    }
    let squad = rig
        .backend
        .list_running_with_name_prefix("awman-squad-team-")
        .unwrap();
    let mut names: Vec<_> = squad.iter().map(|h| h.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["awman-squad-team-a1b2c3d4", "awman-squad-team-e5f6a7b8"],
        "the full awman name (not the hashed sandbox name) is the identity"
    );
    assert!(squad
        .iter()
        .all(|h| h.id.len() == 16 && h.id.starts_with("aw")));
    assert!(rig
        .backend
        .list_running_with_name_prefix("awman-squad-other-")
        .unwrap()
        .is_empty());
    assert_eq!(rig.backend.list_running_all().unwrap().len(), 3);
    assert!(rig.backend.list_stopped().unwrap().is_empty());
}

#[test]
fn resource_limits_are_rejected_before_anything_is_created() {
    let rig = Rig::new("session-a");
    let fractional = ResolvedContainerOptions {
        cpu: Some(CpuLimit(0.5)),
        ..options("awman-limit")
    };
    assert!(matches!(
        rig.backend.build(fractional).err().unwrap(),
        EngineError::UnsupportedResourceRequest { .. }
    ));
    for memory in [0, 64, u64::from(u32::MAX) + 1] {
        let bad = ResolvedContainerOptions {
            memory: Some(MemoryLimit(memory)),
            ..options("awman-limit")
        };
        assert!(
            matches!(
                rig.backend.build(bad).err().unwrap(),
                EngineError::UnsupportedResourceRequest { .. }
            ),
            "memory {memory} MiB"
        );
    }
    assert_eq!(rig.driver.creates(), 0);

    let explicit = ResolvedContainerOptions {
        cpu: Some(CpuLimit(3.0)),
        memory: Some(MemoryLimit(2048)),
        ..options("awman-limit")
    };
    let plan = rig.backend.plan(explicit).unwrap();
    assert_eq!((plan.spec.vcpus, plan.spec.memory_mib), (3, 2048));
    let defaults = rig.backend.plan(options("awman-defaults")).unwrap();
    assert_eq!((defaults.spec.vcpus, defaults.spec.memory_mib), (2, 4096));
}

#[test]
fn unsupported_requests_fail_closed_without_side_effects() {
    let rig = Rig::new("session-a");
    let docker = ResolvedContainerOptions {
        allow_docker: true,
        ..options("awman-docker")
    };
    assert!(matches!(
        rig.backend.build(docker).err().unwrap(),
        EngineError::UnsupportedOnRuntime {
            runtime: "builtin",
            ..
        }
    ));
    let missing = ResolvedContainerOptions {
        image: Some(ImageRef::new("never-imported:latest")),
        ..options("awman-missing")
    };
    match rig.backend.build(missing).err().unwrap() {
        EngineError::Config(message) => assert!(message.contains("not imported"), "{message}"),
        other => panic!("expected a Config error, got {other:?}"),
    }
    let tools_without_flag = ResolvedContainerOptions {
        allowed_tools: vec!["Read".into()],
        ..options("awman-tools")
    };
    assert!(rig.backend.build(tools_without_flag).is_err());
    assert_eq!(rig.driver.creates(), 0, "nothing was pulled or created");
}

#[test]
fn hypervisor_and_test_isolation_gate_every_launch_path() {
    let mut rig = Rig::new("session-a");
    rig.driver.state.lock().unwrap().hypervisor_error = Some("no /dev/kvm here".into());
    let error = rig.backend.build(options("awman-nokvm")).err().unwrap();
    assert!(
        matches!(&error, EngineError::BuiltinRuntimeUnavailable { reason } if reason.contains("kvm")),
        "{error:?}"
    );
    assert!(rig
        .backend
        .start_background(
            "agent:latest",
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            &[]
        )
        .is_err());
    assert_eq!(rig.driver.creates(), 0);

    rig.driver.state.lock().unwrap().hypervisor_error = None;
    assert!(rig.backend.is_available().is_ok());
    rig.backend.settings.test_isolation = true;
    assert!(matches!(
        rig.backend.is_available(),
        Err(EngineError::BuiltinRuntimeUnavailable { reason }) if reason.contains("test isolation")
    ));
    assert!(rig.backend.build(options("awman-isolated")).is_err());
}

#[test]
fn capabilities_describe_an_importing_hostless_runtime() {
    let rig = Rig::new("session-a");
    let caps = rig.backend.capabilities();
    assert_eq!(
        caps.image_acquisition,
        crate::engine::agent_runtime::ImageAcquisition::Import
    );
    assert!(!caps.fractional_cpu);
    assert_eq!(rig.backend.host_cli(), None);
    assert!(rig.backend.exec_args("id", "/", &["sh"], &[]).is_none());
    assert!(!rig.backend.reattach_after_owner_exit());
    assert_eq!(rig.backend.name(), "builtin");
    assert!(rig.backend.list_dangling_images().unwrap().is_empty());
    assert!(rig
        .backend
        .build_image(
            "t",
            std::path::Path::new("Dockerfile"),
            std::path::Path::new("."),
            false,
            &mut |_| {}
        )
        .is_err());
}

#[test]
fn secrets_never_reach_argv_labels_names_or_mounts() {
    let rig = Rig::new("session-a");
    let mut secret = options("awman-secret");
    secret.agent_credentials = vec![("ANTHROPIC_API_KEY".into(), "sk-ant-SECRET-1".into())];
    secret.env_literal = vec![EnvLiteral {
        key: "PRIVATE_TOKEN".into(),
        value: "literal-SECRET-2".into(),
    }];
    secret.allowed_tools = vec!["Read".into()];
    secret.allowed_tools_flag = Some("--allowedTools".into());
    let plan = rig.backend.plan(secret).unwrap();

    let visible: Vec<String> = plan
        .command
        .argv
        .iter()
        .cloned()
        .chain(plan.command.cwd.clone())
        .chain(plan.command.user.clone())
        .chain([
            plan.spec.name.clone(),
            plan.spec.image.clone(),
            plan.name.clone(),
        ])
        .chain(plan.spec.labels.iter().map(|(k, v)| format!("{k}={v}")))
        .chain(
            plan.spec
                .mounts
                .iter()
                .map(|m| format!("{}:{}", m.host.display(), m.guest)),
        )
        .collect();
    assert!(
        visible.iter().all(|value| !value.contains("SECRET")),
        "a secret leaked into argv, a label, a name or a mount: {visible:?}"
    );
    assert!(plan
        .command
        .env
        .contains(&("ANTHROPIC_API_KEY".into(), "sk-ant-SECRET-1".into())));
    assert!(plan
        .command
        .env
        .contains(&("PRIVATE_TOKEN".into(), "literal-SECRET-2".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_starts_get_distinct_sandboxes_and_owners() {
    let rig = Rig::new("session-a");
    let mut launches = Vec::new();
    for index in 0..8 {
        let instance = rig
            .backend
            .build(options(&format!("awman-par-{index}")))
            .unwrap();
        launches.push(tokio::spawn(async move {
            let (fe, peer) = frontend(None);
            let started = instance.run_with_frontend(fe);
            (started.is_ok(), peer)
        }));
    }
    for launch in launches {
        assert!(launch.await.unwrap().0);
    }
    assert_eq!(rig.driver.creates(), 8);
    let names = rig.driver.names();
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), 8);
    let owners: std::collections::BTreeSet<_> =
        names.iter().map(|id| token_of(&rig.driver, id)).collect();
    assert_eq!(owners.len(), 8, "every launch has its own owner token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_starts_of_one_name_leave_the_winner_untouched() {
    let rig = Rig::new("session-a");
    let mut launches = Vec::new();
    for _ in 0..4 {
        let instance = rig.backend.build(options("awman-dup")).unwrap();
        launches.push(tokio::spawn(async move {
            let (fe, peer) = frontend(None);
            let started = instance.run_with_frontend(fe);
            (started.is_ok(), peer)
        }));
    }
    let mut winners = 0;
    for launch in launches {
        winners += usize::from(launch.await.unwrap().0);
    }
    assert_eq!(winners, 1, "exactly one launch owns the name");
    assert_eq!(rig.driver.names().len(), 1);
    assert!(
        rig.driver.finished().is_empty(),
        "losing launches must not stop or remove the winner"
    );
}

#[test]
fn background_sandbox_env_lives_in_memory_and_exec_runs_as_the_image_user() {
    let rig = Rig::new("session-a");
    let work = tempfile::tempdir_in("/tmp").unwrap();
    let extra = tempfile::tempdir_in("/tmp").unwrap();
    let mut env = HashMap::new();
    env.insert("SETUP_TOKEN".to_string(), "bg-SECRET-3".to_string());
    let id = rig
        .backend
        .start_background(
            "agent:latest",
            work.path(),
            &env,
            &[OverlaySpec {
                host_path: extra.path().into(),
                container_path: "/extra".into(),
                permission: OverlayPermission::ReadOnly,
            }],
        )
        .unwrap();

    let spec = rig.driver.spec(&id);
    let guest_work = work.path().to_str().unwrap();
    assert_eq!(spec.mounts[0].guest, guest_work, "workdir mounts first, rw");
    assert!(!spec.mounts[0].read_only);
    assert!(spec
        .mounts
        .iter()
        .any(|m| m.guest == "/extra" && m.read_only));
    assert!(spec.labels.values().all(|v| !v.contains("SECRET")));
    assert_eq!(spec.mount_owner, Some((1234, 1234)));
    assert_eq!(
        rig.backend.background_env.lock().unwrap()[&id].env["SETUP_TOKEN"],
        "bg-SECRET-3"
    );

    let mut lines = Vec::new();
    let driver = rig.driver.clone();
    let emitter = std::thread::spawn({
        let id = id.clone();
        move || {
            for _ in 0..600 {
                if driver
                    .state
                    .lock()
                    .unwrap()
                    .sandboxes
                    .iter()
                    .any(|s| s.events.is_some())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            driver.emit(&id, ExecEvent::Stdout(b"line1\nlin".to_vec()));
            driver.emit(&id, ExecEvent::Stdout(b"e2\n".to_vec()));
            driver.emit(&id, ExecEvent::Stderr(b"warn".to_vec()));
            driver.emit(&id, ExecEvent::Exited(3));
        }
    });
    let mut extra_env = HashMap::new();
    extra_env.insert("STEP_VAR".to_string(), "1".to_string());
    let output = rig
        .backend
        .exec_in_background_streaming(&id, "echo hi", "/work", Some(&extra_env), &mut |line| {
            lines.push(line.to_string())
        })
        .unwrap();
    emitter.join().unwrap();
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, "line1\nline2\n");
    assert_eq!(output.stderr, "warn");
    assert_eq!(lines, ["line1", "line2", "warn"]);

    let request = rig.driver.execs(&id).remove(0);
    assert_eq!(request.argv, ["/bin/sh", "-c", "echo hi"]);
    assert_eq!(request.cwd.as_deref(), Some("/work"));
    assert_eq!(request.user.as_deref(), Some("probe"));
    assert!(request
        .env
        .contains(&("SETUP_TOKEN".into(), "bg-SECRET-3".into())));
    assert!(request.env.contains(&("STEP_VAR".into(), "1".into())));

    rig.backend.stop_and_remove(&id).unwrap();
    assert!(rig.driver.names().is_empty());
    assert!(rig.backend.background_env.lock().unwrap().is_empty());
    assert!(rig
        .backend
        .exec_in_background(&id, "true", "/", None)
        .is_err());
}

#[test]
fn a_background_exec_that_fails_is_an_error_not_a_zero_exit() {
    let rig = Rig::new("session-a");
    let work = tempfile::tempdir_in("/tmp").unwrap();
    let id = rig
        .backend
        .start_background("agent:latest", work.path(), &HashMap::new(), &[])
        .unwrap();
    let driver = rig.driver.clone();
    let emitter = std::thread::spawn({
        let id = id.clone();
        move || {
            for _ in 0..600 {
                if driver
                    .state
                    .lock()
                    .unwrap()
                    .sandboxes
                    .iter()
                    .any(|s| s.events.is_some())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            driver.emit(&id, ExecEvent::Failed);
        }
    });
    assert!(rig
        .backend
        .exec_in_background(&id, "true", "/", None)
        .is_err());
    emitter.join().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_agent_deletes_only_its_own_stale_attach_socket() {
    let rig = Rig::new("session-a");
    let mut kept = options("awman-stale");
    kept.remove_on_exit = false;
    let instance = rig.backend.build(kept).unwrap();
    let id = instance.handle_preview().id;
    let token = {
        let (fe, _peer) = frontend(None);
        let mut execution = instance.run_with_frontend(fe).unwrap();
        rig.driver.wait_for_exec(&id).await;
        let token = token_of(&rig.driver, &id);
        rig.driver.emit(&id, ExecEvent::Exited(0));
        execution.wait().await.unwrap();
        token
    };
    // A crashed earlier launch of the same sandbox left a stale endpoint behind.
    let stale = rig.backend.paths.attach(&id, "crashed-launch");
    std::fs::write(&stale, b"stale").unwrap();
    let own = rig.backend.paths.attach(&id, &token);
    std::fs::write(&own, b"own").unwrap();

    rig.backend.remove_agent(&id).unwrap();
    assert!(!own.exists(), "the removed launch's endpoint is cleaned");
    assert!(
        stale.exists(),
        "another launch's endpoint is never unlinked"
    );
}

#[test]
fn image_identity_catalog_round_trips_and_only_tracked_images_are_removed() {
    let rig = Rig::new("session-a");
    let digest = |c: char| Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap();
    let identity = crate::data::oci_identity::ImageIdentity {
        reference: "registry.example/agent:1".into(),
        manifest_digest: digest('a'),
        config_digest: digest('b'),
        platform: OciPlatform::host_linux(),
        source: ImageSourceKind::Archive,
    };
    assert_eq!(rig.backend.image_identity("agent:latest").unwrap(), None);
    let mut catalog = BTreeMap::new();
    catalog.insert("agent:latest".to_string(), identity.clone());
    rig.backend.save_identities(&catalog).unwrap();
    assert_eq!(
        rig.backend.image_identity("agent:latest").unwrap(),
        Some(identity)
    );
    assert!(rig.backend.image_exists("agent:latest").unwrap());
    assert!(!rig.backend.image_exists("other:latest").unwrap());
    assert_eq!(
        rig.backend
            .image_home_dir("agent:latest")
            .unwrap()
            .as_deref(),
        Some("/home/probe")
    );

    assert!(rig.backend.remove_image("untracked:latest").is_err());
    assert!(rig.driver.state.lock().unwrap().removed_images.is_empty());
    rig.backend.remove_image("agent:latest").unwrap();
    assert_eq!(rig.backend.image_identity("agent:latest").unwrap(), None);
    assert_eq!(
        rig.driver.state.lock().unwrap().removed_images,
        ["agent:latest"]
    );
}

#[test]
fn a_corrupt_identity_catalog_is_an_error_not_an_empty_catalog() {
    let rig = Rig::new("session-a");
    std::fs::write(rig.backend.identities_path(), b"{not json").unwrap();
    assert!(rig.backend.image_identity("agent:latest").is_err());
    assert!(rig.backend.remove_image("agent:latest").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn reattach_streams_to_a_second_client_and_detach_never_kills() {
    let rig = Rig::new("session-a");
    let (id, mut launcher, mut first) = launch(&rig, &rig.backend, "awman-reattach").await;
    let handle = rig.backend.list_running_all().unwrap().remove(0);

    // Two attach clients: one plain, one with a PTY that resizes.
    let attach_a = rig.backend.attach(&handle).unwrap();
    let (fe_a, mut peer_a) = frontend(None);
    let mut exec_a = attach_a.run_with_frontend(fe_a).unwrap();
    let attach_b = rig.backend.attach(&handle).unwrap();
    let (fe_b, mut peer_b) = frontend(Some((100, 30)));
    let mut exec_b = attach_b.run_with_frontend(fe_b).unwrap();
    eventually("both clients subscribed", || rig.driver.receivers(&id) >= 3).await;

    peer_a.stdin.send(b"from-a".to_vec()).unwrap();
    peer_b.resize.as_ref().unwrap().send((120, 40)).unwrap();
    eventually("attach input and resize reach the guest", || {
        let sent = rig.driver.controls(&id);
        sent.contains(&Sent::Stdin(b"from-a".to_vec())) && sent.contains(&Sent::Resize(120, 40))
    })
    .await;

    rig.driver.emit(&id, ExecEvent::Stdout(vec![1, 0, 2]));
    rig.driver.emit(&id, ExecEvent::Stderr(b"e".to_vec()));
    rig.driver.emit(&id, ExecEvent::Exited(37));
    for peer in [&mut first, &mut peer_a, &mut peer_b] {
        assert_eq!(peer.stdout.recv().await.unwrap(), vec![1, 0, 2]);
        assert_eq!(peer.stderr.recv().await.unwrap(), b"e".to_vec());
    }
    assert_eq!(launcher.wait().await.unwrap().exit_code, 37);
    assert_eq!(
        exec_a.wait().await.unwrap().exit_code,
        37,
        "real exit, not 0"
    );
    assert_eq!(exec_b.wait().await.unwrap().exit_code, 37);
    let sent = rig.driver.controls(&id);
    assert!(
        !sent.contains(&Sent::Kill) && !sent.contains(&Sent::Interrupt),
        "attach and detach never signal the target: {sent:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_after_the_owner_finished_is_an_error() {
    let rig = Rig::new("session-a");
    let mut kept = options("awman-gone");
    kept.remove_on_exit = false;
    let instance = rig.backend.build(kept).unwrap();
    let id = instance.handle_preview().id;
    let (fe, _peer) = frontend(None);
    let mut execution = instance.run_with_frontend(fe).unwrap();
    rig.driver.wait_for_exec(&id).await;
    let handle = rig.backend.list_running_all().unwrap().remove(0);
    // Simulate a still-registered sandbox whose launcher (and socket host) died.
    let attach = rig.backend.attach(&handle).unwrap();
    rig.driver.emit(&id, ExecEvent::Exited(0));
    execution.wait().await.unwrap();
    let (fe, _peer) = frontend(None);
    assert!(
        attach.run_with_frontend(fe).is_err(),
        "no launcher, no endpoint: reattach after owner exit is unsupported and says so"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_awman_with_the_same_protocol_attaches_to_an_older_owners_sandbox() {
    // Upgrade recovery: the launcher (another awman process, e.g. the previous
    // binary) is still alive and hosts the attach endpoint; this session did not
    // start the sandbox but may attach because the protocol label matches.
    let rig = Rig::new("new-awman-session");
    let old_owner = rig.second_session("old-awman-session");
    let (id, mut launcher, mut first) = launch(&rig, &old_owner, "awman-survivor").await;
    let handle = rig.backend.list_running_all().unwrap().remove(0);
    let attach = rig.backend.attach(&handle).unwrap();
    let (fe, mut peer) = frontend(None);
    let mut attached = attach.run_with_frontend(fe).unwrap();
    eventually("attach subscribed", || rig.driver.receivers(&id) >= 2).await;
    rig.driver
        .emit(&id, ExecEvent::Stdout(b"still running".to_vec()));
    assert_eq!(peer.stdout.recv().await.unwrap(), b"still running".to_vec());
    assert_eq!(
        first.stdout.recv().await.unwrap(),
        b"still running".to_vec()
    );
    rig.driver.emit(&id, ExecEvent::Exited(0));
    assert_eq!(attached.wait().await.unwrap().exit_code, 0);
    assert_eq!(launcher.wait().await.unwrap().exit_code, 0);
    // Cleanup stayed with the launching owner, never the attaching session.
    assert_eq!(rig.driver.finished().len(), 1);
    assert!(
        rig.driver.finished()[0].1.starts_with("old-awman-session/"),
        "the launching owner's token, not the attaching session's"
    );
}
