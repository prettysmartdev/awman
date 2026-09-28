//! Real-guest driver for the `builtin_hw_*` tests (WI 0119).
//!
//! It is a separate executable because the embedded worker re-runs the current
//! executable: Cargo test executables (in `deps/`) are refused as worker
//! hosts, and this file carries the same worker dispatch as `src/main.rs`. It
//! is built only with `--features builtin-runtime` and is meant to run on a
//! machine with KVM (Linux) or the hypervisor entitlement (macOS).
//!
//! The driver is deliberately dumb: it performs one scenario through the
//! public runtime contract (`ContainerRuntime::{import_image, build,
//! start_background, attach}`) and records raw observations in `--work`:
//! `<scenario>.stdout`, `<scenario>.stderr` (the guest's streams), plus
//! `KEY\tVALUE` lines on its own stdout. The test decides pass/fail.
//! Exit codes: 0 scenario ran, 77 BLOCKED (prerequisite missing), 2 usage.
//!
//! Status: compiled and exercised up to the hypervisor gate on hosts without
//! KVM. Its guest-side behaviour has NOT been run in this repository's CI
//! container (no /dev/kvm); see the BLOCKED rows in test-coverage-runtime.md.

#![deny(unsafe_code)]

#[path = "../../src/engine/container/builtin/worker.rs"]
mod builtin_worker;

#[cfg(not(awman_builtin))]
fn main() -> std::process::ExitCode {
    if builtin_worker::dispatch(std::env::vars_os()) {
        return std::process::ExitCode::SUCCESS;
    }
    println!("BLOCKED\tthis target has no builtin runtime payload");
    std::process::ExitCode::from(77)
}

#[cfg(awman_builtin)]
mod driver {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use awman::data::config::env::Env;
    use awman::data::config::image_source::ImageSourceSpec;
    use awman::data::message::{UserMessage, UserMessageSink};
    use awman::data::oci_identity::OciPlatform;
    use awman::engine::agent_runtime::execution::AgentExecution;
    use awman::engine::agent_runtime::frontend::{
        AgentFrontend, AgentIo, AgentProgress, AgentStatus,
    };
    use awman::engine::agent_runtime::{AgentRuntimeEngine, ImageImportRequest};
    use awman::engine::container::options::{
        ContainerName, CpuLimit, Entrypoint, EnvLiteral, ImageRef, MemoryLimit, OverlayPermission,
        OverlaySpec, ResolvedContainerOptions,
    };
    use awman::engine::container::runtime::BuiltinRuntimeSettings;
    use awman::engine::container::ContainerRuntime;

    type Res<T> = Result<T, String>;

    struct Args {
        scenario: String,
        state: PathBuf,
        archive: PathBuf,
        tag: String,
        work: PathBuf,
        scripts: PathBuf,
    }

    fn parse() -> Res<Args> {
        let mut map = BTreeMap::new();
        let mut it = std::env::args().skip(1);
        while let Some(key) = it.next() {
            let value = it.next().ok_or_else(|| format!("{key} needs a value"))?;
            map.insert(key, value);
        }
        let get = |k: &str| map.get(k).cloned().ok_or_else(|| format!("missing {k}"));
        Ok(Args {
            scenario: get("--scenario")?,
            state: get("--state-dir")?.into(),
            archive: get("--archive")?.into(),
            tag: get("--tag")?,
            work: get("--work")?.into(),
            scripts: get("--scripts")?.into(),
        })
    }

    struct Quiet;
    impl UserMessageSink for Quiet {
        fn write_message(&mut self, message: UserMessage) {
            eprintln!("[runtime] {}", message.text);
        }
        fn replay_queued(&mut self) {}
    }

    struct Frontend {
        io: Option<AgentIo>,
    }
    impl UserMessageSink for Frontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl AgentFrontend for Frontend {
        fn report_status(&mut self, status: AgentStatus) {
            eprintln!("[status] {status:?}");
        }
        fn report_progress(&mut self, _: AgentProgress) {}
        fn take_io(&mut self) -> AgentIo {
            self.io.take().expect("io is taken once")
        }
        fn grace_timeout(&self) -> Duration {
            Duration::from_secs(3600)
        }
        fn stuck_timeout(&self) -> Duration {
            Duration::from_secs(3600)
        }
    }

    struct Peer {
        stdout: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        stderr: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        resize: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>,
    }

    fn frontend(pty: Option<(u16, u16)>) -> (Box<Frontend>, Peer) {
        let (stdout_tx, stdout) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr) = tokio::sync::mpsc::unbounded_channel();
        let (stdin, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx: stdin.clone(),
            stdin_rx,
            resize: pty.map(|_| resize_rx),
            initial_size: pty,
        };
        (
            Box::new(Frontend { io: Some(io) }),
            Peer {
                stdout,
                stderr,
                stdin,
                resize: pty.map(|_| resize_tx),
            },
        )
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            out.extend(chunk);
        }
        out
    }

    struct Outcome {
        exit: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    fn options(tag: &str, name: &str) -> ResolvedContainerOptions {
        ResolvedContainerOptions {
            image: Some(ImageRef::new(tag)),
            name: Some(ContainerName::new(name)),
            remove_on_exit: true,
            ..Default::default()
        }
    }

    fn sh(script: &str) -> Option<Entrypoint> {
        Some(Entrypoint::new(["/bin/sh", "-c", script]))
    }

    fn launch(
        runtime: &ContainerRuntime,
        options: ResolvedContainerOptions,
        pty: Option<(u16, u16)>,
    ) -> Res<(AgentExecution, Peer)> {
        let instance = runtime.build(options).map_err(|e| format!("build: {e}"))?;
        let (fe, peer) = frontend(pty);
        let execution = instance
            .run_with_frontend(fe)
            .map_err(|e| format!("run: {e}"))?;
        Ok((execution, peer))
    }

    async fn finish(mut execution: AgentExecution, mut peer: Peer) -> Res<Outcome> {
        let info = execution.wait().await.map_err(|e| format!("wait: {e}"))?;
        Ok(Outcome {
            exit: info.exit_code,
            stdout: drain(&mut peer.stdout),
            stderr: drain(&mut peer.stderr),
        })
    }

    fn record(work: &Path, scenario: &str, outcome: &Outcome) {
        let _ = std::fs::write(work.join(format!("{scenario}.stdout")), &outcome.stdout);
        let _ = std::fs::write(work.join(format!("{scenario}.stderr")), &outcome.stderr);
        println!("EXIT\t{}", outcome.exit);
    }

    async fn read_until(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        needle: &str,
        limit: Duration,
    ) -> Res<String> {
        let start = Instant::now();
        let mut seen = String::new();
        while !seen.contains(needle) {
            let left = limit.saturating_sub(start.elapsed());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(chunk)) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                _ => return Err(format!("timed out waiting for {needle:?}; saw {seen:?}")),
            }
        }
        Ok(seen)
    }

    fn overlay(host: &Path, guest: &str, rw: bool) -> OverlaySpec {
        OverlaySpec {
            host_path: host.to_path_buf(),
            container_path: guest.into(),
            permission: if rw {
                OverlayPermission::ReadWrite
            } else {
                OverlayPermission::ReadOnly
            },
        }
    }

    /// The mount tree of `tools/oci-runtime-spike/mount-probe.sh`, verbatim.
    struct Tree {
        overlays: Vec<OverlaySpec>,
        workspace: PathBuf,
        claude: PathBuf,
        single_ro: PathBuf,
        outside_secret: PathBuf,
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn tree(base: &Path, scripts: &Path) -> Tree {
        let d = |p: &str| base.join(p);
        for dir in [
            "workspace/nested",
            "readonly",
            "nested",
            "skills/named",
            "contexts/global",
            "contexts/repo",
            "contexts/workflow",
            "direct",
            "claude",
            "gemini/antigravity-cli",
            "extra",
            "outside",
        ] {
            std::fs::create_dir_all(d(dir)).unwrap();
        }
        write(&d("workspace/input"), "workspace");
        write(&d("workspace/AGENTS.md"), "agents-md");
        write(&d("outside/secret"), "secret");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(d("outside"), d("workspace/escape"));
        write(&d("nested/value"), "nested");
        write(&d("skills/named/SKILL.md"), "skill");
        for scope in ["global", "repo", "workflow"] {
            write(&d(&format!("contexts/{scope}/AGENTS.md")), scope);
        }
        write(&d("direct/settings.json"), "direct");
        write(&d("claude.json"), "claude-config");
        write(&d("claude/settings.json"), "sanitized");
        write(&d("claude/.credentials.json"), "access-token-v1");
        write(&d("gemini/antigravity-cli/token"), "fake-token");
        write(&d("extra/AGENTS.md"), "extra");
        write(&d("prompt.md"), "prompt");
        write(&d("single-ro"), "file-ro");
        write(&d("single-rw"), "file-rw");
        let overlays = vec![
            overlay(&d("workspace"), "/workspace", true),
            overlay(&d("readonly"), "/readonly", false),
            overlay(&d("nested"), "/workspace/nested", false),
            overlay(&d("skills"), "/skills", false),
            overlay(&d("contexts/global"), "/contexts/global", false),
            overlay(&d("contexts/repo"), "/contexts/repo", false),
            overlay(&d("contexts/workflow"), "/contexts/workflow", true),
            overlay(&d("direct"), "/agent-home/direct", true),
            overlay(&d("claude"), "/agent-home/.claude", true),
            overlay(&d("gemini"), "/agent-home/.gemini", true),
            overlay(&d("extra"), "/extra", false),
            overlay(scripts, "/spike", false),
            overlay(&d("single-ro"), "/single-ro", false),
            overlay(&d("single-rw"), "/single-rw", true),
            overlay(&d("claude.json"), "/agent-home/.claude.json", true),
            overlay(&d("prompt.md"), "/prompt.md", false),
        ];
        Tree {
            overlays,
            workspace: d("workspace"),
            claude: d("claude"),
            single_ro: d("single-ro"),
            outside_secret: d("outside/secret"),
        }
    }

    /// Host-side atomic refresh once the guest signals readiness: temp file in
    /// the same directory, then rename over the target (never in-place).
    fn refresher(tree: &Tree) -> std::thread::JoinHandle<()> {
        let (ready, claude, single) = (
            tree.workspace.join("refresh-ready"),
            tree.claude.clone(),
            tree.single_ro.clone(),
        );
        std::thread::spawn(move || {
            for _ in 0..1200 {
                if ready.exists() {
                    let next = claude.join(".credentials.next");
                    std::fs::write(&next, "access-token-v2").unwrap();
                    std::fs::rename(&next, claude.join(".credentials.json")).unwrap();
                    let next = single.with_extension("next");
                    std::fs::write(&next, "file-ro-v2").unwrap();
                    std::fs::rename(&next, &single).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    }

    pub async fn run() -> ExitCode {
        let args = match parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("usage error: {e}");
                return ExitCode::from(2);
            }
        };
        std::fs::create_dir_all(&args.work).unwrap();
        let settings = BuiltinRuntimeSettings {
            state_dir: args.state.clone(),
            vcpus: if args.scenario == "limits" { 1 } else { 2 },
            memory_mib: if args.scenario == "limits" { 512 } else { 1024 },
            image_source: None,
            images: BTreeMap::new(),
            registries: BTreeMap::new(),
            ambient_overrides: Env::from_process().ambient_msb_overrides(),
            test_isolation: false,
        };
        let runtime = match ContainerRuntime::builtin(settings) {
            Ok(runtime) => runtime,
            Err(error) => {
                println!("BLOCKED\t{error}");
                return ExitCode::from(77);
            }
        };
        if let Err(error) = runtime.availability() {
            println!("BLOCKED\t{error}");
            return ExitCode::from(77);
        }
        let request = ImageImportRequest {
            tag: args.tag.clone(),
            source: ImageSourceSpec::Archive {
                path: args.archive.clone(),
            },
            platform: OciPlatform::host_linux(),
            refresh: false,
        };
        for round in ["first", "cached"] {
            let started = Instant::now();
            match runtime.import_image(&request, &mut Quiet) {
                Ok(image) => println!(
                    "IMPORT\t{round}\t{:?}\t{}ms",
                    image.home_dir,
                    started.elapsed().as_millis()
                ),
                Err(error) => {
                    println!("FAILED\timport {round}: {error}");
                    return ExitCode::from(1);
                }
            }
        }
        match scenario(&runtime, &args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                println!("FAILED\t{error}");
                ExitCode::from(1)
            }
        }
    }

    async fn scenario(runtime: &ContainerRuntime, a: &Args) -> Res<()> {
        let tag = a.tag.as_str();
        let name = |n: &str| format!("awman-hw-{n}");
        match a.scenario.as_str() {
            // Image defaults: no entrypoint/cwd/env supplied, so USER, WORKDIR,
            // HOME, ENV, ENTRYPOINT and CMD all come from the imported image.
            "image-defaults" => {
                let (exec, peer) = launch(runtime, options(tag, &name("image")), None)?;
                record(&a.work, "image-defaults", &finish(exec, peer).await?);
            }
            // Explicit runtime overrides on top of the image defaults.
            "image-overrides" => {
                let mut o = options(tag, &name("override"));
                o.entrypoint =
                    sh("id -u; id -g; pwd; printf '%s\\n' \"$HOME\" \"$IMAGE_ENV\" \"$EXTRA\"");
                o.working_dir = Some("/tmp".into());
                o.env_literal = vec![
                    EnvLiteral {
                        key: "IMAGE_ENV".into(),
                        value: "runtime override".into(),
                    },
                    EnvLiteral {
                        key: "EXTRA".into(),
                        value: "added".into(),
                    },
                ];
                let (exec, peer) = launch(runtime, o, None)?;
                record(&a.work, "image-overrides", &finish(exec, peer).await?);
            }
            "exit-code" => {
                let mut o = options(tag, &name("exit"));
                o.entrypoint = sh("printf out-marker; printf err-marker >&2; exit 37");
                let (exec, peer) = launch(runtime, o, None)?;
                record(&a.work, "exit-code", &finish(exec, peer).await?);
            }
            "mounts" => {
                let tree = tree(&a.work.join("tree"), &a.scripts);
                let refresh = refresher(&tree);
                let mut o = options(tag, &name("mounts"));
                o.overlays = tree.overlays.clone();
                o.entrypoint = Some(Entrypoint::new(["/bin/sh", "/spike/guest-checks.sh"]));
                o.working_dir = Some("/workspace".into());
                o.env_literal = vec![
                    EnvLiteral {
                        key: "SPIKE_LITERAL".into(),
                        value: "literal with spaces".into(),
                    },
                    EnvLiteral {
                        key: "SPIKE_SECRET".into(),
                        value: "fake-secret".into(),
                    },
                    EnvLiteral {
                        key: "SPIKE_PROMPT_FILE".into(),
                        value: "/prompt.md".into(),
                    },
                    EnvLiteral {
                        key: "SPIKE_HOST_SECRET".into(),
                        value: tree.outside_secret.to_string_lossy().into(),
                    },
                    EnvLiteral {
                        key: "SPIKE_FILES".into(),
                        value: "1".into(),
                    },
                ];
                let (exec, peer) = launch(runtime, o, None)?;
                let outcome = finish(exec, peer).await?;
                let _ = refresh.join();
                let from_guest =
                    std::fs::read_to_string(tree.workspace.join("from-guest")).unwrap_or_default();
                println!("HOST_WRITEBACK\t{from_guest}");
                record(&a.work, "mounts", &outcome);
            }
            // Two VMs share one staged credentials directory; the host
            // replaces the file atomically and both must observe it.
            "refresh-multi" => {
                let tree = tree(&a.work.join("tree"), &a.scripts);
                let script = "for i in $(seq 1 100); do \
                    [ \"$(cat /agent-home/.claude/.credentials.json)\" = access-token-v2 ] && { echo seen-v2; exit 0; }; \
                    sleep 0.1; done; echo never-saw-v2; exit 1";
                let mut launched = Vec::new();
                for n in 0..2 {
                    let mut o = options(tag, &name(&format!("refresh-{n}")));
                    o.overlays = vec![overlay(&tree.claude, "/agent-home/.claude", true)];
                    o.entrypoint = sh(script);
                    launched.push(launch(runtime, o, None)?);
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                let next = tree.claude.join(".credentials.next");
                std::fs::write(&next, "access-token-v2").unwrap();
                std::fs::rename(&next, tree.claude.join(".credentials.json")).unwrap();
                for (n, (exec, peer)) in launched.into_iter().enumerate() {
                    let outcome = finish(exec, peer).await?;
                    record(&a.work, &format!("refresh-multi-{n}"), &outcome);
                }
            }
            "limits" => {
                let mut o = options(tag, &name("limits"));
                o.entrypoint = sh("nproc; awk '/MemTotal/ {print $2}' /proc/meminfo");
                let (exec, peer) = launch(runtime, o, None)?;
                record(&a.work, "limits-default", &finish(exec, peer).await?);
                let mut o = options(tag, &name("limits-explicit"));
                o.entrypoint = sh("nproc; awk '/MemTotal/ {print $2}' /proc/meminfo");
                o.cpu = Some(CpuLimit(2.0));
                o.memory = Some(MemoryLimit(768));
                let (exec, peer) = launch(runtime, o, None)?;
                record(&a.work, "limits", &finish(exec, peer).await?);
            }
            "multi-vm" => {
                let mut launched = Vec::new();
                for n in 0..3 {
                    let mut o = options(tag, &name(&format!("multi-{n}")));
                    o.entrypoint = sh(&format!("sleep 3; echo vm-{n}"));
                    launched.push(launch(runtime, o, None)?);
                }
                let running = runtime
                    .list_running_with_name_prefix("awman-hw-multi-")
                    .map_err(|e| e.to_string())?;
                println!("RUNNING_DURING\t{}", running.len());
                // Stopping one leaves the others alone.
                if let Some(victim) = running.iter().find(|h| h.name.ends_with("multi-0")) {
                    runtime.stop(victim).map_err(|e| e.to_string())?;
                }
                let mut outputs = Vec::new();
                for (n, (exec, peer)) in launched.into_iter().enumerate().skip(1) {
                    let outcome = finish(exec, peer).await?;
                    outputs.push(format!(
                        "{n}:{}:{}",
                        outcome.exit,
                        String::from_utf8_lossy(&outcome.stdout).trim()
                    ));
                }
                println!("SURVIVORS\t{}", outputs.join(","));
                let left = runtime
                    .list_running_with_name_prefix("awman-hw-multi-")
                    .map_err(|e| e.to_string())?;
                println!("RUNNING_AFTER\t{}", left.len());
            }
            "cancel" => {
                let mut o = options(tag, &name("cancel"));
                o.entrypoint = Some(Entrypoint::new(["/bin/sleep", "300"]));
                let (exec, peer) = launch(runtime, o, None)?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let started = Instant::now();
                exec.cancel_handle()
                    .ok_or("no cancel handle")?
                    .cancel()
                    .map_err(|e| e.to_string())?;
                let outcome = finish(exec, peer).await?;
                println!("CANCEL_SECONDS\t{}", started.elapsed().as_secs());
                let left = runtime.list_running_all().map_err(|e| e.to_string())?;
                println!("RUNNING_AFTER\t{}", left.len());
                record(&a.work, "cancel", &outcome);
            }
            "pty-resize" => {
                let mut o = options(tag, &name("pty"));
                o.interactive = true;
                o.entrypoint = sh("stty size; read line; stty size");
                let (exec, mut peer) = launch(runtime, o, Some((80, 24)))?;
                let first = read_until(&mut peer.stdout, "24 80", Duration::from_secs(60)).await?;
                peer.resize
                    .as_ref()
                    .ok_or("no resize channel")?
                    .send((100, 30))
                    .map_err(|e| e.to_string())?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                peer.stdin
                    .send(b"go\n".to_vec())
                    .map_err(|e| e.to_string())?;
                let rest = read_until(&mut peer.stdout, "30 100", Duration::from_secs(60)).await?;
                println!("PTY_SIZES\t{}", (first + &rest).replace(['\r', '\n'], " "));
                let outcome = finish(exec, peer).await?;
                println!("EXIT\t{}", outcome.exit);
            }
            "acp-framing" => {
                let mut o = options(tag, &name("acp"));
                o.acp = true;
                o.interactive = false;
                o.entrypoint = Some(Entrypoint::new(["/bin/cat"]));
                let (exec, mut peer) = launch(runtime, o, None)?;
                let frame: Vec<u8> = (0..=255u8).chain([b'\n', 0, b'\r', 255]).collect();
                peer.stdin.send(frame.clone()).map_err(|e| e.to_string())?;
                let mut echoed = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(60);
                while echoed.len() < frame.len() && Instant::now() < deadline {
                    if let Ok(Some(chunk)) =
                        tokio::time::timeout(Duration::from_secs(1), peer.stdout.recv()).await
                    {
                        echoed.extend(chunk);
                    }
                }
                println!("ACP_ECHO_EQUAL\t{}", echoed == frame);
                println!("ACP_LENGTHS\t{}/{}", echoed.len(), frame.len());
                drop(peer.stdin);
                let outcome = tokio::time::timeout(
                    Duration::from_secs(30),
                    finish(
                        exec,
                        Peer {
                            stdin: tokio::sync::mpsc::unbounded_channel().0,
                            ..peer
                        },
                    ),
                )
                .await
                .map_err(|_| "cat did not exit after stdin closed".to_string())??;
                println!("EXIT\t{}", outcome.exit);
            }
            "reattach" => {
                let mut o = options(tag, &name("reattach"));
                o.interactive = true;
                o.entrypoint =
                    sh("while read l; do echo got:$l; [ \"$l\" = quit ] && exit 5; done");
                let (exec, mut owner) = launch(runtime, o, Some((80, 24)))?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let handle = runtime
                    .list_running_with_name_prefix("awman-hw-reattach")
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .next()
                    .ok_or("the launched agent is not discoverable")?;
                let attach = runtime.attach(&handle).map_err(|e| e.to_string())?;
                let (fe, mut second) = frontend(Some((80, 24)));
                let attached = attach.run_with_frontend(fe).map_err(|e| e.to_string())?;
                second
                    .stdin
                    .send(b"from-attach\n".to_vec())
                    .map_err(|e| e.to_string())?;
                let seen_owner = read_until(
                    &mut owner.stdout,
                    "got:from-attach",
                    Duration::from_secs(30),
                )
                .await?;
                let seen_second = read_until(
                    &mut second.stdout,
                    "got:from-attach",
                    Duration::from_secs(30),
                )
                .await?;
                println!(
                    "REATTACH_BOTH_SAW\t{}",
                    seen_owner.contains("got:from-attach")
                        && seen_second.contains("got:from-attach")
                );
                // Detaching the second client must not stop the agent.
                drop(attached);
                owner
                    .stdin
                    .send(b"quit\n".to_vec())
                    .map_err(|e| e.to_string())?;
                let outcome = finish(exec, owner).await?;
                println!("EXIT\t{}", outcome.exit);
            }
            other => return Err(format!("unknown scenario {other}")),
        }
        Ok(())
    }
}

#[cfg(awman_builtin)]
fn main() -> std::process::ExitCode {
    if builtin_worker::dispatch(std::env::vars_os()) {
        return std::process::ExitCode::SUCCESS;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(driver::run())
}
