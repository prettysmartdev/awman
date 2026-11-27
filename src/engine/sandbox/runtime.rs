//! `SandboxRuntime` — the sandbox-class `AgentRuntimeEngine` impl.
//!
//! Holds an `Arc<dyn SandboxBackend>`. The concrete driver is invisible
//! outside this module. Platform guards live in the constructors: a user on
//! an unsupported platform gets `BackendUnsupportedOnPlatform` from the
//! constructor and never reaches the backend.

use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::agent_runtime::execution::{AgentExitInfo, ExecutionBackend, StuckEvent};
use crate::engine::agent_runtime::{
    AgentExecution, AgentFrontend, AgentHandle, AgentHandlePreview, AgentInstance,
    AgentRuntimeEngine, AgentStats, Capabilities, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::error::EngineError;
use crate::engine::sandbox::backend::SandboxBackend;
use crate::engine::sandbox::dsbx::DSbxBackend;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Capabilities shared by sandbox-class runtimes: kit-declarative,
/// persistent, workspace-only mounts, private DinD per VM.
static SANDBOX_CAPABILITIES: Capabilities = Capabilities {
    arbitrary_host_mounts: false,
    kit_declarative: true,
    has_image_store: false,
    image_acquisition: crate::engine::agent_runtime::capabilities::ImageAcquisition::Kit,
    per_launch_vm_settings: false,
    reports_resource_stats: false,
};

pub struct SandboxRuntime {
    env: crate::data::config::env::EnvSnapshot,
    backend: Arc<dyn SandboxBackend>,
}

/// Experimental release switch, enabled after the accepted native smoke on
/// Apple silicon macOS and Linux x86_64. Platform/option refusals still apply.
const SBX_RUNTIME_ENABLED: bool = true;

/// Whether construction may proceed past the release switch.
fn sbx_enabled(env: &crate::data::config::env::EnvSnapshot) -> bool {
    SBX_RUNTIME_ENABLED || qualification_seam(env)
}

/// Test and debug builds only: a test-isolated process may reach the
/// backend so component and boundary tests drive the production `ready`,
/// `chat` and `exec prompt` paths. The seam does not exist in a release
/// build, so the shipped binary cannot be talked into enabling the runtime.
#[cfg(any(test, debug_assertions))]
fn qualification_seam(env: &crate::data::config::env::EnvSnapshot) -> bool {
    env.test_isolation()
}
#[cfg(not(any(test, debug_assertions)))]
fn qualification_seam(_: &crate::data::config::env::EnvSnapshot) -> bool {
    false
}

/// Platform outcome of the Docker Sandboxes runtime. Apple silicon macOS
/// and Linux x86_64 have accepted native evidence; other hosts are refused with an
/// explicit reason. There is no fallback to another runtime.
fn platform_outcome(os: &str, arch: &str) -> Result<(), EngineError> {
    let refusal = |platform: String| EngineError::BackendUnsupportedOnPlatform {
        backend: "docker-sbx-experimental".into(),
        platform,
    };
    match (os, arch) {
        ("macos", "aarch64") | ("linux", "x86_64") => Ok(()),
        ("macos", arch) => Err(refusal(format!(
            "macos/{arch}: Docker Sandboxes supports macOS on Apple silicon only"
        ))),
        ("linux", arch) => Err(refusal(format!(
            "linux/{arch}: not qualified; awman currently supports Docker Sandboxes on Linux x86_64 only"
        ))),
        ("windows", _) => Err(refusal(
            "windows: not qualified. Docker ships Sandboxes for Windows 11, but awman has no \
             native evidence on this platform"
                .into(),
        )),
        (os, arch) => Err(refusal(format!(
            "{os}/{arch}: Docker Sandboxes does not support this platform"
        ))),
    }
}

impl SandboxRuntime {
    /// Prepare this runtime for a single agent at `awman ready` time: refuse
    /// an agent without a route, admit the `sbx` client, check sign-in, emit
    /// and validate the agent's kit, and report sandboxes a launch would
    /// refuse as stale. Registers no credential and removes no sandbox;
    /// `no_cache` only lists the agent's sandboxes with their removal
    /// command. Successful probes stay quiet.
    pub fn ready_agent(
        &self,
        agent: &str,
        no_cache: bool,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        super::dsbx::ready_agent_in(&self.env, agent, no_cache, sink)
    }

    /// Construct with the Docker Sandbox (`sbx`) backend.
    pub fn dsbx() -> Result<Self, EngineError> {
        Self::dsbx_in(crate::engine::host_cli::process_env())
    }

    /// The only construction path. Fails before any platform or CLI access
    /// while the release switch is off, then admits only Apple silicon macOS
    /// and Linux x86_64.
    pub fn dsbx_in(env: crate::data::config::env::EnvSnapshot) -> Result<Self, EngineError> {
        if !sbx_enabled(&env) {
            return Err(EngineError::SbxRuntimeDisabled);
        }
        platform_outcome(std::env::consts::OS, std::env::consts::ARCH)?;
        Ok(Self {
            backend: Arc::new(DSbxBackend::with_env(env.clone())),
            env,
        })
    }
}

impl AgentRuntimeEngine for SandboxRuntime {
    fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    fn display_name(&self) -> &'static str {
        match self.backend.name() {
            "docker-sbx-experimental" => "Docker Sandboxes (experimental)",
            _ => "Sandbox",
        }
    }

    fn capabilities(&self) -> &Capabilities {
        &SANDBOX_CAPABILITIES
    }

    fn is_available(&self) -> bool {
        crate::engine::host_cli::checked_output(
            crate::engine::host_cli::command(&self.env, self.backend.cli_binary()).args([
                "--no-input",
                "ls",
                "--json",
            ]),
            self.backend.cli_binary(),
        )
        .is_ok()
    }

    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        match options {
            ResolvedAgentOptions::Sandbox(mut opts) => {
                if opts.host_env.is_none() {
                    opts.host_env = Some(self.env.clone());
                }
                opts.validate_tool_restrictions()?;
                Ok(Box::new(SandboxAgentInstance { options: *opts }))
            }
            other => Err(EngineError::OptionVariantMismatch {
                runtime: self.runtime_name().to_string(),
                got: other.paradigm(),
            }),
        }
    }

    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no session label; attribution is by name (WI 0090).
        self.backend.list_running()
    }

    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.backend.list_running()
    }

    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.backend.stats(handle)
    }

    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }

    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        Ok(Box::new(SandboxAttachInstance {
            env: self.env.clone(),
            handle: handle.clone(),
            cli_binary: self.backend.cli_binary(),
        }))
    }

    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        // Sandboxes have no server-side name filter; list and filter by name.
        let mut running = self.backend.list_running()?;
        running.retain(|h| h.name.starts_with(prefix));
        Ok(running)
    }

    fn ready_agent(
        &self,
        agent: &str,
        opts: ReadyAgentOptions,
        sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        // `opts.build` is inert here: a kit is emitted and validated every
        // time, so there is no cached image to force a rebuild of.
        SandboxRuntime::ready_agent(self, agent, opts.no_cache, sink)
    }

    fn image_exists(&self, _tag: &str) -> Result<bool, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "local image probe",
        })
    }

    fn image_home_dir(&self, _tag: &str) -> Result<Option<String>, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "image HOME lookup",
        })
    }

    fn build_image(
        &self,
        _request: &crate::engine::container::BuildRequest,
        _on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: self.runtime_name(),
            operation: "image build",
        })
    }
}

/// Configured-but-not-running sandbox agent — the sandbox tier's half of the
/// two-step build/run pattern.
struct SandboxAgentInstance {
    options: ResolvedSandboxOptions,
}

impl AgentInstance for SandboxAgentInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        let name = self
            .options
            .sandbox_name
            .clone()
            .unwrap_or_else(|| self.options.agent_id.clone());
        AgentHandlePreview {
            id: name.clone(),
            name,
            // Sandboxes boot a kit/template rather than a local image; the
            // kit selector is the closest analogue.
            image: self.options.agent_id.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        // The interactive launch (session config, credential injection, kit
        // selection, PTY-bridged `sbx run`) lives in the dsbx driver.
        super::dsbx::run_interactive(self.options, frontend)
    }
}

// ─── Attach (re-attach into a foreign, already-running sandbox) ─────────────
//
// Sandbox attach exists for cross-tier `AgentRuntimeEngine` completeness and
// the tier-parity tests; squad itself refuses to run under the sandbox tier
// (see Part 5). The sandbox tier carries its own minimal PTY bridge here
// rather than reaching into the dsbx driver's private one, keeping the
// WI-0090 layering rule (no `src/engine/container/` import) intact.

/// Configured-but-not-running sandbox attach handle. `run_with_frontend`
/// opens an `sbx exec -it <name> bash` session and bridges it to the frontend.
struct SandboxAttachInstance {
    env: crate::data::config::env::EnvSnapshot,
    handle: AgentHandle,
    cli_binary: &'static str,
}

/// Build the `sbx exec` argv:
/// `exec -it [--env K=V…] <name> <entrypoint…>`.
fn sbx_attach_argv(name: &str, env_vars: &[(&str, &str)], entrypoint: &[&str]) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-it".to_string()];
    for (k, v) in env_vars {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push(name.to_string());
    args.extend(entrypoint.iter().map(|s| s.to_string()));
    args
}

/// Kill only the local `sbx exec` client — never `sbx stop`, because attach
/// does not own the sandbox.
fn kill_local_sbx_exec(pid: Option<u32>) {
    if let Some(pid) = pid {
        #[cfg(unix)]
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
        }
    }
}

impl AgentInstance for SandboxAttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        use portable_pty::{native_pty_system, PtySize};

        let started_at = chrono::Utc::now();
        let handle = self.handle.clone();

        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: handle.name.clone(),
            },
        );

        let io = frontend.take_io();
        let (stuck_tx, _) = tokio::sync::broadcast::channel::<StuckEvent>(8);
        let stuck_tx = std::sync::Arc::new(stuck_tx);

        // PTY path: pass the frontend's terminal size through as COLUMNS/LINES.
        if let Some((cols, rows)) = io.initial_size {
            let cols_s = cols.to_string();
            let rows_s = rows.to_string();
            let argv = sbx_attach_argv(
                &handle.id,
                &[("COLUMNS", &cols_s), ("LINES", &rows_s)],
                &["bash"],
            );
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| EngineError::Sandbox(format!("openpty: {e}")))?;
            let mut cmd = crate::engine::host_cli::pty_command(&self.env, self.cli_binary);
            for arg in &argv {
                cmd.arg(arg);
            }
            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| EngineError::Sandbox(format!("spawn sbx exec via pty: {e}")))?;
            let child_pid = child.process_id();
            let master = bridge_attach_pty(io, pair)?;
            let backend = SandboxAttachExecution {
                child: None,
                pty_child: Some(child),
                pty_master: Some(master),
                child_pid,
                started_at,
            };
            return Ok(AgentExecution::new(
                handle,
                Box::new(backend),
                stuck_tx,
                None,
            ));
        }

        // Piped path.
        let argv = sbx_attach_argv(&handle.id, &[], &["bash"]);
        let mut cmd = crate::engine::host_cli::command(&self.env, self.cli_binary);
        cmd.args(&argv);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| EngineError::Sandbox(format!("spawn sbx exec: {e}")))?;
        let child_pid = Some(child.id());
        bridge_attach_piped(io, &mut child);
        let backend = SandboxAttachExecution {
            child: Some(child),
            pty_child: None,
            pty_master: None,
            child_pid,
            started_at,
        };
        Ok(AgentExecution::new(
            handle,
            Box::new(backend),
            stuck_tx,
            None,
        ))
    }
}

type SbxPtyMaster = std::sync::Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

/// Wire a PTY master to the frontend's channels: reader thread (PTY→stdout),
/// writer task (stdin→PTY), resize task.
fn bridge_attach_pty(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    pair: portable_pty::PtyPair,
) -> Result<SbxPtyMaster, EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Sandbox(format!("clone sbx pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Sandbox(format!("take sbx pty writer: {e}")))?;
    let mut writer =
        crate::engine::container::io_bridge::QuietPtyWriter::new(writer, pair.master.as_ref());

    let stdout_tx = io.stdout;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        while let Some(bytes) = stdin_rx.recv().await {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    let master_arc: SbxPtyMaster = std::sync::Arc::new(std::sync::Mutex::new(pair.master));
    if let Some(mut resize_rx) = io.resize {
        let master_for_resize = std::sync::Arc::clone(&master_arc);
        tokio::spawn(async move {
            use portable_pty::PtySize;
            while let Some((cols, rows)) = resize_rx.recv().await {
                if let Ok(master) = master_for_resize.lock() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }
    Ok(master_arc)
}

/// Wire a piped child's stdio to the frontend's channels.
fn bridge_attach_piped(
    io: crate::engine::agent_runtime::frontend::AgentIo,
    child: &mut std::process::Child,
) {
    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stdout;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stderr;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stderr_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }
}

/// Execution backend for a sandbox attach session. Cancellation kills only the
/// local `sbx exec` client — never `sbx stop`, because the target sandbox
/// belongs to another process.
struct SandboxAttachExecution {
    child: Option<std::process::Child>,
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pty_master: Option<SbxPtyMaster>,
    child_pid: Option<u32>,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl ExecutionBackend for SandboxAttachExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Sandbox(format!("wait sbx exec (pty): {e}")))?;
            self.pty_master = None;
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }
        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Sandbox("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Sandbox(format!("wait sbx exec: {e}")))?;
        let exit_code = status.code().unwrap_or(-1);
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Ok(AgentExitInfo {
            exit_code,
            signal,
            started_at: self.started_at,
            ended_at: chrono::Utc::now(),
        })
    }

    fn cancel(&self) -> Result<(), EngineError> {
        kill_local_sbx_exec(self.child_pid);
        Ok(())
    }

    fn cancel_handle(&self) -> crate::engine::agent_runtime::execution::CancelHandle {
        let pid = self.child_pid;
        crate::engine::agent_runtime::execution::CancelHandle::new(move || {
            kill_local_sbx_exec(pid);
            Ok(())
        })
    }
}

#[cfg(test)]
mod workspace_access_tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    #[test]
    fn sbx_runtime_is_enabled_without_test_isolation_on_supported_release_hosts() {
        let plain = crate::data::config::env::EnvSnapshot::empty();
        assert!(
            sbx_enabled(&plain),
            "a normal release must enable SBX without a test seam"
        );
        let result = SandboxRuntime::dsbx_in(plain);
        let expected = platform_outcome(std::env::consts::OS, std::env::consts::ARCH);
        assert_eq!(result.is_ok(), expected.is_ok());
        assert!(!matches!(result, Err(EngineError::SbxRuntimeDisabled)));
        // The test seam passes only an isolated test process, and only in
        // test/debug builds; past it, the platform decides.
        let isolated =
            crate::data::config::env::EnvSnapshot::with_overrides([("AWMAN_TEST_ISOLATION", "1")]);
        assert!(sbx_enabled(&isolated));
        let result = SandboxRuntime::dsbx_in(isolated);
        let expected = platform_outcome(std::env::consts::OS, std::env::consts::ARCH);
        assert_eq!(result.is_ok(), expected.is_ok());
        if let (Err(got), Err(want)) = (result, expected) {
            assert_eq!(got.to_string(), want.to_string());
        }
    }

    #[test]
    fn accepted_mac_arm64_and_linux_x64_pass_and_untested_hosts_are_refused() {
        assert!(platform_outcome("macos", "aarch64").is_ok());
        assert!(platform_outcome("linux", "x86_64").is_ok());
        let linux = platform_outcome("linux", "aarch64")
            .unwrap_err()
            .to_string();
        assert!(linux.contains("linux/aarch64: not qualified"), "{linux}");
        let windows = platform_outcome("windows", "x86_64")
            .unwrap_err()
            .to_string();
        assert!(windows.contains("windows: not qualified"), "{windows}");
        assert!(platform_outcome("macos", "x86_64").is_err());
        assert!(platform_outcome("freebsd", "x86_64").is_err());
        for (os, arch) in [
            ("linux", "aarch64"),
            ("windows", "aarch64"),
            ("macos", "x86_64"),
        ] {
            assert!(matches!(
                platform_outcome(os, arch),
                Err(EngineError::BackendUnsupportedOnPlatform { .. })
            ));
        }
    }

    #[test]
    fn sandbox_cannot_advertise_or_prepare_read_only_workspaces() {
        let sandbox = TestSandbox::for_test(TestCase::component("engine::sandbox::runtime::workspace_access_tests::sandbox_cannot_advertise_or_prepare_read_only_workspaces"));
        let env = crate::data::config::env::EnvSnapshot::from_full_env(
            sandbox
                .env()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        );
        let runtime = SandboxRuntime {
            backend: Arc::new(DSbxBackend::with_env(env.clone())),
            env: env.clone(),
        };
        assert!(!runtime.capabilities().read_only_workspace());
        let session = Session::open_at_directory_root(
            sandbox.work(),
            crate::data::session::SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap();
        let engine = crate::engine::agent::AgentEngine::new(
            Arc::new(crate::engine::overlay::OverlayEngine::new(&session).unwrap()),
            Arc::new(runtime),
        )
        .with_read_only_workspace(true);
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let run = crate::engine::agent::AgentRunOptions::default();
        let error = engine
            .build_sandbox_options(&session, &agent, &run)
            .unwrap_err();
        assert!(error.to_string().contains(
            "cannot mount the workspace read-only; this run requires a read-only workspace"
        ));
        assert!(engine.build_options(&session, &agent, &run).is_err());
    }
}
