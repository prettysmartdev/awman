//! Real-guest driver for the network and resource `builtin_hw_*` tests.
//!
//! A separate executable for the same reason as `builtin_hw_driver`: the
//! embedded worker re-runs the current executable, and Cargo test executables
//! are refused as worker hosts. Built only with `--features builtin-runtime`
//! as the `builtin_net_driver` example.
//!
//! Usage: `builtin_net_driver --state-dir S --archive A --tag T --work W
//! --plan PLAN.json`. The plan is `{"phases": [[vm, ...], ...]}`; phases run
//! one after another and the VMs of one phase run at the same time. Each VM is
//! `{"name", "vcpus", "memoryMib", "network": <builtin.network block>,
//! "script"}`; `network` is applied as the global block, so it passes through
//! the same validation and compilation as a user's config. The guest runs
//! `/bin/sh -c <script>`.
//!
//! Output: `<name>.stdout` / `<name>.stderr` in `--work`, and on stdout
//! `IMPORT\t<round>`, `EXIT\t<name>\t<code>`, `REFUSED\t<name>\t<error>` (the
//! runtime refused the VM before starting it), `LEFTOVER\t<count>` (VMs still
//! running after every phase finished) and `DONE`. Exit codes: 0 plan ran,
//! 77 BLOCKED, 2 usage, 1 failure.

#![deny(unsafe_code)]

#[path = "../../../src/engine/container/builtin/worker.rs"]
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
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use awman::data::config::builtin_network::BuiltinNetworkConfig;
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
        ContainerName, Entrypoint, ImageRef, ResolvedContainerOptions,
    };
    use awman::engine::container::runtime::BuiltinRuntimeSettings;
    use awman::engine::container::ContainerRuntime;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Vm {
        name: String,
        vcpus: u8,
        memory_mib: u32,
        #[serde(default)]
        network: BuiltinNetworkConfig,
        script: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        phases: Vec<Vec<Vm>>,
    }

    struct Args {
        state: PathBuf,
        archive: PathBuf,
        tag: String,
        work: PathBuf,
        plan: PathBuf,
    }

    fn parse() -> Result<Args, String> {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let get = |flag: &str| -> Result<String, String> {
            argv.iter()
                .position(|a| a == flag)
                .and_then(|i| argv.get(i + 1))
                .cloned()
                .ok_or_else(|| format!("missing {flag}"))
        };
        Ok(Args {
            state: get("--state-dir")?.into(),
            archive: get("--archive")?.into(),
            tag: get("--tag")?,
            work: get("--work")?.into(),
            plan: get("--plan")?.into(),
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
        // Held so the guest's stdin stays open until the agent exits.
        _stdin: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    }

    fn frontend() -> (Box<Frontend>, Peer) {
        let (stdout_tx, stdout) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr) = tokio::sync::mpsc::unbounded_channel();
        let (stdin, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let io = AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx: stdin.clone(),
            stdin_rx,
            resize: None,
            initial_size: None,
        };
        (
            Box::new(Frontend { io: Some(io) }),
            Peer {
                stdout,
                stderr,
                _stdin: stdin,
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

    fn runtime_for(args: &Args, vm: &Vm) -> Result<ContainerRuntime, String> {
        let network = BuiltinNetworkConfig::resolve(Some(&vm.network), None)
            .map_err(|e| format!("network: {e}"))?;
        let settings = BuiltinRuntimeSettings {
            state_dir: args.state.clone(),
            vcpus: vm.vcpus,
            memory_mib: vm.memory_mib,
            image_source: None,
            images: BTreeMap::new(),
            registries: BTreeMap::new(),
            network,
            ambient_overrides: Env::from_process().ambient_msb_overrides(),
            test_isolation: false,
        };
        ContainerRuntime::builtin(settings).map_err(|e| e.to_string())
    }

    struct Running {
        name: String,
        execution: AgentExecution,
        peer: Peer,
    }

    pub async fn run() -> ExitCode {
        let args = match parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("usage error: {e}");
                return ExitCode::from(2);
            }
        };
        let plan: Plan = match std::fs::read(&args.plan)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        {
            Ok(plan) => plan,
            Err(e) => {
                eprintln!("usage error: plan: {e}");
                return ExitCode::from(2);
            }
        };
        std::fs::create_dir_all(&args.work).unwrap();

        // One import with default settings; every VM uses the cached image.
        let importer = match runtime_for(
            &args,
            &Vm {
                name: "import".into(),
                vcpus: 1,
                memory_mib: 256,
                network: BuiltinNetworkConfig::default(),
                script: String::new(),
            },
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                println!("BLOCKED\t{error}");
                return ExitCode::from(77);
            }
        };
        if let Err(error) = importer.availability() {
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
            if let Err(error) = importer.import_image(&request, &mut Quiet) {
                println!("FAILED\timport {round}: {error}");
                return ExitCode::from(1);
            }
            println!("IMPORT\t{round}");
        }

        let mut runtimes = Vec::new();
        for phase in &plan.phases {
            let mut running = Vec::new();
            for vm in phase {
                let runtime = match runtime_for(&args, vm) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        println!("REFUSED\t{}\t{error}", vm.name);
                        continue;
                    }
                };
                let options = ResolvedContainerOptions {
                    image: Some(ImageRef::new(&args.tag)),
                    name: Some(ContainerName::new(format!("awman-net-{}", vm.name))),
                    remove_on_exit: true,
                    entrypoint: Some(Entrypoint::new(["/bin/sh", "-c", vm.script.as_str()])),
                    ..Default::default()
                };
                let started = runtime.build(options).and_then(|instance| {
                    let (fe, peer) = frontend();
                    instance
                        .run_with_frontend(fe)
                        .map(|execution| (execution, peer))
                });
                match started {
                    Ok((execution, peer)) => running.push(Running {
                        name: vm.name.clone(),
                        execution,
                        peer,
                    }),
                    Err(error) => println!("REFUSED\t{}\t{error}", vm.name),
                }
                runtimes.push(runtime);
            }
            for mut vm in running {
                let code = match vm.execution.wait().await {
                    Ok(info) => info.exit_code,
                    Err(error) => {
                        println!("FAILED\twait {}: {error}", vm.name);
                        -1
                    }
                };
                let _ = std::fs::write(
                    args.work.join(format!("{}.stdout", vm.name)),
                    drain(&mut vm.peer.stdout),
                );
                let _ = std::fs::write(
                    args.work.join(format!("{}.stderr", vm.name)),
                    drain(&mut vm.peer.stderr),
                );
                println!("EXIT\t{}\t{code}", vm.name);
            }
        }

        // Removal on exit is asynchronous in the worker; give it a bounded
        // moment before counting what is still running.
        let mut leftover = 0;
        for _ in 0..50 {
            leftover = runtimes
                .iter()
                .map(|r| r.list_running_all().map(|l| l.len()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            if leftover == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        println!("LEFTOVER\t{leftover}");
        println!("DONE");
        ExitCode::SUCCESS
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
