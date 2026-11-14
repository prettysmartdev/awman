//! The one container process module — `pub(super)`.
//!
//! Docker and Apple Containers are the same CLI-shaped runtime: `<bin> run`
//! with the argv `docker.rs::build_run_argv` produces, a child spawned either
//! behind a PTY or on piped stdio, and the bytes bridged to the frontend's
//! `AgentIo` through `io_bridge`. Before WI 0113 that path existed twice, once
//! per backend, differing in ~51 of ~92 lines that were all the binary name
//! (`docker` ↔ `container`) or a type name (report F-11).
//!
//! This module owns it once:
//!
//! - [`ContainerCli`] is the whole of the per-backend difference: the binary
//!   name, the ownership label, the startup-chatter delay, and a post-wait
//!   hook.
//! - [`ContainerInstance`] and [`ContainerExecution`] replace the
//!   `Docker*`/`Apple*` pairs.
//! - [`SpawnRequest`] replaces the six 7-parameter spawn signatures.
//! - [`spawn_piped`], [`spawn_piped_interactive`] and [`spawn_pty_bridged`]
//!   are the three spawn paths.
//!
//! The genuinely backend-specific behaviour survives as two hooks and nothing
//! else:
//!
//! - [`PostWaitHook`] — Docker's `clear_stdio_nonblocking`, which repairs the
//!   O_NONBLOCK flags `docker -it` leaves on the inherited stdio fds. Apple's
//!   `container` does not do that, so its hook is [`no_post_wait`].
//! - [`AttachHook`] — Apple's attach rendezvous. Apple's CLI has no `attach`
//!   verb, so the launching process serves the live PTY over a per-container
//!   unix socket. Docker passes `None` and keeps `docker attach`.
//!
//! Everything else about a backend — `list`/`stats`/`stop` parsing, image
//! inspection, `attach` — stays in `docker.rs` / `apple.rs`, where the two
//! genuinely differ.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::execution::{
    AgentExecution, AgentExitInfo, AgentHandlePreview, AgentInstance, ExecutionBackend,
};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::container::attach_socket::AttachSocketGuard;
use crate::engine::container::instance::{handle_now, ContainerId};
use crate::engine::container::io_bridge::{BridgeConfig, CancelFn};
use crate::engine::container::options::{
    ContainerName, ImageRef, PromptFileGuard, ResolvedContainerOptions,
};
use crate::engine::credential_refresh::CredentialLease;
use crate::engine::error::EngineError;

/// The `Arc<Mutex<…>>` the I/O bridge hands back for a PTY-bridged run.
type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

// ─── Per-backend CLI description ────────────────────────────────────────────

/// Ran once after a piped child exits, before the exit status is read.
///
/// Docker's `-it` leaves O_NONBLOCK set on the inherited stdio fds; Apple's
/// `container` does not. This is the only difference between the two backends'
/// `wait_blocking` piped path.
pub(super) type PostWaitHook = fn();

/// The no-op [`PostWaitHook`] — used by every backend that leaves the process's
/// stdio the way it found it.
pub(super) fn no_post_wait() {}

/// Everything the shared spawn path needs to know about a container CLI.
///
/// A backend is a `ContainerCli` plus its own `list`/`stats`/`stop` parsing.
/// Adding a third CLI-shaped runtime should mean adding a constant here, not
/// a third copy of [`spawn_piped`].
#[derive(Clone)]
pub(super) struct ContainerCli {
    pub env: Option<Arc<crate::data::config::env::EnvSnapshot>>,
    /// The binary invoked for `run`, `stop` and `rm`.
    pub bin: &'static str,
    /// Ownership label stamped on every awman-spawned container so a backend's
    /// `list_running` can filter to ours.
    pub label: &'static str,
    /// Window after launch during which the stuck detector ignores output.
    /// Apple's `container` prints its own startup chatter ("creating
    /// container…") before the workload writes a byte; Docker does not.
    pub start_delay: Duration,
    /// Ran after a piped child exits. See [`PostWaitHook`].
    pub post_wait: PostWaitHook,
}

impl std::fmt::Debug for ContainerCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerCli")
            .field("bin", &self.bin)
            .field("label", &self.label)
            .field("start_delay", &self.start_delay)
            .finish_non_exhaustive()
    }
}

impl ContainerCli {
    pub(super) fn in_env(mut self, env: crate::data::config::env::EnvSnapshot) -> Self {
        self.env = Some(Arc::new(env));
        self
    }
    pub(super) fn snapshot(&self) -> std::borrow::Cow<'_, crate::data::config::env::EnvSnapshot> {
        match &self.env {
            Some(env) => std::borrow::Cow::Borrowed(env),
            None => std::borrow::Cow::Owned(crate::engine::host_cli::process_env()),
        }
    }
    pub(super) fn command(&self) -> Command {
        crate::engine::host_cli::command(&self.snapshot(), self.bin)
    }
    pub(super) const DOCKER: ContainerCli = ContainerCli {
        env: None,
        bin: "docker",
        label: "awman=true",
        start_delay: Duration::ZERO,
        post_wait: super::docker::clear_stdio_nonblocking,
    };

    pub(super) const APPLE: ContainerCli = ContainerCli {
        env: None,
        bin: "container",
        label: "awman=true",
        start_delay: crate::engine::container::timing::APPLE_CONTAINER_START_DELAY,
        post_wait: no_post_wait,
    };
}

// ─── The attach hook (Apple's attach rendezvous) ────────────────────────────

/// What a [`AttachHook`] is handed once the PTY bridge is wired up.
pub(super) struct AttachHookCtx<'a> {
    pub env: &'a crate::data::config::env::EnvSnapshot,
    pub terminal: Option<(
        Arc<crate::engine::terminal_hub::TerminalHub>,
        crate::data::ids::TerminalId,
    )>,
    /// The container's name — the key the socket path is derived from.
    pub container_name: &'a str,
    /// Live tap on the container's output. Already installed on the
    /// `BridgeConfig`, so every chunk the reader threads forward arrives here.
    pub output_broadcast: Arc<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// The bridge's stdin injector, so an attach client's keystrokes merge
    /// with whatever the launching frontend sends.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// A *weak* reference to the PTY master: the attach server must never keep
    /// the PTY alive past the execution backend that owns it.
    pub pty_master: Weak<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
}

/// Backend hook run after [`spawn_pty_bridged`] wires the I/O bridge.
///
/// Returning `Some` stores the guard on the [`ContainerExecution`], which drops
/// it (closing the socket) as soon as the container exits. Returning `None` —
/// which the hook does on any failure, after logging — means the container runs
/// but cannot be attached to.
pub(super) type AttachHook = fn(AttachHookCtx<'_>) -> Option<AttachSocketGuard>;

// ─── Bridge configuration ───────────────────────────────────────────────────

/// Build a `BridgeConfig` for this container, including a cancel callback that
/// runs `<bin> stop <name>` so the startup-grace detector can kill a container
/// that never produced output. We construct the same `stop` invocation the
/// backend's `cancel_handle` would issue.
pub(super) fn bridge_config_for(
    cli: ContainerCli,
    name: &ContainerName,
    grace_timeout: Duration,
    stuck_timeout: Duration,
) -> BridgeConfig {
    let container_name = name.0.clone();
    let cancel_cli = cli.clone();
    let cancel: CancelFn = Arc::new(move || {
        let _ = cancel_cli
            .command()
            .args(["stop", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    BridgeConfig {
        grace_timeout,
        stuck_timeout,
        container_start_delay: cli.start_delay,
        cancel_on_grace_expired: Some(cancel),
        output_tail: Arc::new(
            crate::engine::agent_runtime::output_tail::OutputTail::with_default_capacity(),
        ),
        output_broadcast: None,
        terminal: None,
        program_status: Default::default(),
    }
}

// ─── The instance ───────────────────────────────────────────────────────────

/// A built-but-not-running container. Both backends' `build` produce this.
pub(super) struct ContainerInstance {
    /// Which CLI runs this container. Also the value the spawn functions are
    /// handed; they take it as an explicit parameter so a test can substitute
    /// a stand-in binary without reaching into the instance.
    pub cli: ContainerCli,
    pub id: ContainerId,
    pub name: ContainerName,
    pub image: ImageRef,
    pub options: ResolvedContainerOptions,
    /// Credential-refresh leases, one per file-delivered credential. Moved into
    /// the [`ContainerExecution`] by each spawn fn so the lease's lifetime
    /// brackets the child process exactly (this instance box is dropped before
    /// the spawn fn returns).
    pub leases: Vec<CredentialLease>,
    /// Backend hook for the PTY path. `Some` only for Apple, whose CLI has no
    /// native attach. See [`AttachHook`].
    pub post_bridge: Option<AttachHook>,
}

impl ContainerInstance {
    pub(super) fn new(
        cli: ContainerCli,
        image: ImageRef,
        name: ContainerName,
        options: ResolvedContainerOptions,
        leases: Vec<CredentialLease>,
        post_bridge: Option<AttachHook>,
    ) -> Self {
        Self {
            cli,
            id: ContainerId::new(name.0.clone()),
            name,
            image,
            options,
            leases,
            post_bridge,
        }
    }
}

impl AgentInstance for ContainerInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.id.0.clone(),
            name: self.name.0.clone(),
            image: self.image.0.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        mut frontend: Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let cli = self.cli.clone();
        let post_bridge = self.post_bridge;
        let argv = super::docker::build_run_argv(&self.name, &self.image, &self.options)?;
        let started_at = chrono::Utc::now();
        let seeded = if self.options.headless_prompt_in_argv && !self.options.interactive {
            None
        } else {
            self.options.seeded_prompt.clone()
        };
        let handle = handle_now(&self.id, &self.name, &self.image);

        if self.options.agent_output_format.is_structured() {
            frontend.set_agent_output_format(self.options.agent_output_format);
        }
        frontend.report_status(
            crate::engine::agent_runtime::frontend::AgentStatus::Running {
                container_name: self.name.0.clone(),
            },
        );

        // Read per-frontend timeouts before draining `take_io`, which leaves
        // the frontend in a state where any further calls are
        // implementation-defined.
        let grace_timeout = self
            .options
            .startup_grace
            .unwrap_or_else(|| frontend.grace_timeout());
        let stuck_timeout = frontend.stuck_timeout();
        // WI 0141 §3: tell the user which requested host-terminal variables
        // were not passed (`build_run_argv` also traces it).
        if let Some(notice) = super::docker::host_terminal_env_notice(&self.options) {
            frontend.write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: notice,
            });
        }
        let io = frontend.take_io();

        let mut bridge_cfg =
            bridge_config_for(cli.clone(), &self.name, grace_timeout, stuck_timeout);
        bridge_cfg.program_status = self.options.program_status;
        bridge_cfg.terminal = frontend.terminal_launch();
        if self.options.full_transcript {
            bridge_cfg.output_tail =
                Arc::new(crate::engine::agent_runtime::output_tail::OutputTail::unbounded());
        }
        let req = SpawnRequest {
            io,
            argv,
            seeded,
            started_at,
            handle,
            bridge_cfg,
        };

        // PTY path: frontend requested interactive PTY bridging.
        if req.io.initial_size.is_some() {
            return spawn_pty_bridged(cli, self, req, post_bridge);
        }

        // ACP path: persistent piped stdio (no PTY). Unlike the one-shot piped
        // path below, the stdin channel is kept open for the whole session so
        // the ACP driver can carry a full bidirectional JSON-RPC exchange.
        // Every backend takes this path — shipping ACP under only one runtime
        // would be a platform-inconsistent regression (WI 0104 Edge Case
        // Considerations).
        if self.options.acp {
            return spawn_piped_interactive(cli, self, req);
        }

        // Piped path: headless or no PTY.
        spawn_piped(cli, self, req)
    }
}

// ─── The spawn request ──────────────────────────────────────────────────────

/// Everything a spawn function needs beyond the instance and its CLI.
///
/// Replaces the six 7-parameter signatures the two backends carried before
/// WI 0113 (report F-11).
pub(super) struct SpawnRequest {
    /// The frontend's I/O channels, drained from `take_io`.
    pub io: AgentIo,
    /// `<bin>`-less argv from `build_run_argv`.
    pub argv: Vec<String>,
    /// Prompt to write into stdin before the writer task starts. Honoured only
    /// by [`spawn_piped`]; see the other two functions for why.
    pub seeded: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub handle: AgentHandle,
    pub bridge_cfg: BridgeConfig,
}

/// Common `<bin> <argv>` setup: the child command with the agent credentials
/// and the resolved `env()` passthrough values on its environment.
///
/// Both are passed as name-only `-e KEY` in argv; their values are set on the
/// child CLI's environment so the CLI resolves them without the secret ever
/// touching the argument vector.
///
/// The passthrough values must be injected explicitly rather than relying on
/// ambient inheritance: inside the squad daemon they come from the Layer 0
/// daemon overlay, not from the daemon's own process environment.
fn piped_command(
    cli: ContainerCli,
    argv: &[String],
    options: &ResolvedContainerOptions,
) -> Command {
    let mut cmd = cli.command();
    cmd.args(argv);
    for (k, v) in super::docker::screened_agent_credentials(options) {
        cmd.env(k, v);
    }
    for (k, v) in super::docker::resolve_env_passthrough(options) {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// INV-6: a credentialed container must hold its lease before the child is
/// spawned. This closes the startup race where a token could expire between
/// staging and the container's first request.
fn assert_leases_before_spawn(options: &ResolvedContainerOptions, leases: &[CredentialLease]) {
    debug_assert!(
        options.refreshable_credentials.is_empty()
            || !leases.is_empty()
            || options.lease_factory.is_none(),
        "file-delivered credential spawned without a lease"
    );
}

fn spawn_child(cli: ContainerCli, cmd: &mut Command) -> Result<std::process::Child, EngineError> {
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::ContainerRuntimeUnavailable {
                binary: cli.bin.into(),
            }
        } else {
            EngineError::Container(format!("spawn {}: {e}", cli.bin))
        }
    })
}

// ─── The three spawn paths ──────────────────────────────────────────────────

/// Spawn `<bin> run -it` via `portable-pty` and bridge the PTY master to the
/// frontend's `AgentIo` channels via the shared I/O bridge.
///
/// `post_bridge` is the backend's attach hook; `None` means the backend has a
/// native attach and needs no rendezvous socket (Docker).
pub(super) fn spawn_pty_bridged(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
    post_bridge: Option<AttachHook>,
) -> Result<AgentExecution, EngineError> {
    use portable_pty::{native_pty_system, PtySize};

    let SpawnRequest {
        io,
        argv,
        seeded: _,
        started_at,
        handle,
        mut bridge_cfg,
    } = req;

    // Move the credential leases out of the instance (dropped before this fn
    // returns) and into the execution backend, so each lease brackets the child
    // process it credentials.
    let leases = std::mem::take(&mut instance.leases);
    let prompt_files = std::mem::take(&mut instance.options.prompt_files);

    let (cols, rows) = io.initial_size.expect("PTY path requires initial_size");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| EngineError::Container(format!("openpty: {e}")))?;

    let snapshot = cli.snapshot();
    let mut cmd = crate::engine::host_cli::pty_command(&snapshot, cli.bin);
    for arg in &argv {
        cmd.arg(arg);
    }
    // Agent credentials and the resolved `env()` passthrough values are passed
    // as name-only `-e KEY` in argv; set their values on the child's
    // environment so the CLI resolves them without the secret ever touching the
    // argument vector. The passthrough values cannot rely on ambient
    // inheritance — inside the squad daemon they live in the Layer 0 overlay,
    // not in the daemon's own process environment.
    for (k, v) in super::docker::screened_agent_credentials(&instance.options) {
        cmd.env(k, v);
    }
    for (k, v) in super::docker::resolve_env_passthrough(&instance.options) {
        cmd.env(k, v);
    }

    assert_leases_before_spawn(&instance.options, &leases);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| EngineError::Container(format!("spawn {} via pty: {e}", cli.bin)))?;

    // Interactive PTY runs pass the seeded prompt as a CLI positional arg
    // (appended by `build_run_argv`), so it must NOT also be written to stdin.
    // Writing it here would cause the PTY to echo the prompt text into the
    // terminal output, painting it over the TUI before the agent starts.

    // Only a backend with an attach hook needs a live tap on the output
    // stream; without one the bridge behaves exactly as it always has.
    let output_broadcast = post_bridge.map(|_| {
        let (tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(256);
        Arc::new(tx)
    });
    if let Some(tx) = &output_broadcast {
        if bridge_cfg.terminal.is_none() {
            bridge_cfg.output_broadcast = Some(Arc::clone(tx));
        }
    }
    let terminal_launch = bridge_cfg.terminal.clone();

    let (master_arc, bridge) =
        crate::engine::container::io_bridge::bridge_pty(io, pair, bridge_cfg)?;

    let attach_socket = match (post_bridge, output_broadcast) {
        (Some(hook), Some(output_broadcast)) => hook(AttachHookCtx {
            env: &snapshot,
            terminal: terminal_launch.and_then(|launch| {
                let id = launch
                    .handle
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()?
                    .id()
                    .clone();
                Some((launch.hub, id))
            }),
            container_name: &instance.name.0,
            output_broadcast,
            stdin_injector: bridge.stdin_injector.clone(),
            pty_master: Arc::downgrade(&master_arc),
        }),
        _ => None,
    };

    let backend = ContainerExecution {
        cli,
        child: None,
        pty_child: Some(child),
        pty_master: Some(master_arc),
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
        attach_socket,
        leases,
        prompt_files,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    )
    .with_program_status(bridge.program_status))
}

/// Spawn `<bin> run` with piped stdio and bridge through `AgentIo`.
pub(super) fn spawn_piped(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
) -> Result<AgentExecution, EngineError> {
    let SpawnRequest {
        io,
        argv,
        seeded,
        started_at,
        handle,
        bridge_cfg,
    } = req;

    let mut cmd = piped_command(cli.clone(), &argv, &instance.options);

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    let prompt_files = std::mem::take(&mut instance.options.prompt_files);
    assert_leases_before_spawn(&instance.options, &leases);

    let mut child = spawn_child(cli.clone(), &mut cmd)?;

    // Write seeded prompt into stdin channel before the writer task starts.
    if let Some(prompt) = seeded {
        let _ = io.stdin_tx.send(prompt.into_bytes());
        let _ = io.stdin_tx.send(b"\n".to_vec());
    }

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Headless (piped) path: drop the engine's stdin_injector so the
    // writer task sees EOF after draining the seeded prompt and closes the
    // child's stdin pipe. Without this, an agent that probes stdin for EOF
    // would hang waiting for input that will never come.
    // `try_inject_stdin` falls back to launching a fresh container — which
    // is the correct behaviour for a headless run that has already
    // consumed its single prompt.
    drop(bridge.stdin_injector);

    let backend = ContainerExecution {
        cli,
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: None,
        container_name: instance.name.0.clone(),
        started_at,
        attach_socket: None,
        leases,
        prompt_files,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    )
    .with_program_status(bridge.program_status))
}

/// Spawn `<bin> run -i` (no PTY) with piped stdio for an ACP session and bridge
/// through `AgentIo`.
///
/// The persistent-piped sibling of [`spawn_piped`]. The one difference — and
/// the whole point — is that it does **not** `drop(bridge.stdin_injector)`: the
/// stdin channel is retained on the [`ContainerExecution`] for the session's
/// entire lifetime, exactly as [`spawn_pty_bridged`] keeps its PTY master
/// alive. That keeps the container's stdin pipe open so the ACP driver can
/// write JSON-RPC request lines (via `try_inject_stdin`) across a full
/// bidirectional exchange, instead of the one-write-then-EOF an ordinary
/// headless run performs.
///
/// No seeded prompt is written to stdin here: an ACP session delivers its
/// prompt over the JSON-RPC channel (`session/prompt`), so writing raw text to
/// stdin would corrupt the newline-delimited JSON-RPC framing.
/// `SpawnRequest::seeded` is ignored, and accepted only so the three spawn
/// paths share one request type.
///
/// Security: identical wiring to [`spawn_piped`] — the bytes ride the stdio
/// pipes `-i` already wires up. No ports, no `--network`, no new mounts (see
/// `aspec/architecture/security.md`).
pub(super) fn spawn_piped_interactive(
    cli: ContainerCli,
    mut instance: Box<ContainerInstance>,
    req: SpawnRequest,
) -> Result<AgentExecution, EngineError> {
    let SpawnRequest {
        io,
        argv,
        seeded: _,
        started_at,
        handle,
        bridge_cfg,
    } = req;

    let mut cmd = piped_command(cli.clone(), &argv, &instance.options);

    // Move leases into the backend; assert one exists before spawn (INV-6).
    let leases = std::mem::take(&mut instance.leases);
    let prompt_files = std::mem::take(&mut instance.options.prompt_files);
    assert_leases_before_spawn(&instance.options, &leases);

    let mut child = spawn_child(cli.clone(), &mut cmd)?;

    let bridge = crate::engine::container::io_bridge::bridge_piped(io, &mut child, bridge_cfg);

    // Persistent-piped (ACP) path: KEEP the stdin_injector alive (do NOT drop
    // it, unlike `spawn_piped`). Retaining the sender both enables
    // `try_inject_stdin` and prevents the writer task from ever seeing EOF, so
    // the container's stdin pipe stays open for the whole JSON-RPC session.
    let backend = ContainerExecution {
        cli,
        child: Some(child),
        pty_child: None,
        pty_master: None,
        stdin_injector: Some(bridge.stdin_injector),
        container_name: instance.name.0.clone(),
        started_at,
        attach_socket: None,
        leases,
        prompt_files,
    };
    Ok(AgentExecution::new(
        handle,
        Box::new(backend),
        bridge.stuck_tx,
        Some(bridge.output_tail),
    )
    .with_program_status(bridge.program_status))
}

/// `<bin> stop <name>` then `<bin> rm <name>`, both best-effort. `stop` SIGTERMs
/// and then SIGKILLs after the runtime's own grace period; a nonzero exit (the
/// container is already gone) is fine.
///
/// The one place the stop-then-remove pair is written: `ContainerExecution`'s
/// cancel paths and `ContainerBackend::stop` all route through it.
pub(super) fn stop_and_remove(cli: &ContainerCli, name: &str) {
    let _ = cli
        .command()
        .args(["stop", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = cli
        .command()
        .args(["rm", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ─── The execution backend ──────────────────────────────────────────────────

/// The running container. One type for every CLI-shaped backend; the only
/// per-backend state is `cli`.
pub(super) struct ContainerExecution {
    /// Which CLI owns this container — the binary `cancel` shells out to and
    /// the name that appears in this backend's error messages.
    cli: ContainerCli,
    /// Set when running with piped stdio.
    child: Option<std::process::Child>,
    /// Set when running PTY-bridged. `portable_pty::Child` has its own wait
    /// API and cannot be unified with `std::process::Child`.
    pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Master PTY end. Held alive so the resize task can call into it and so
    /// the PTY isn't torn down before the child has finished writing.
    pty_master: Option<PtyMaster>,
    /// Stdin sender — same channel the writer task drains. Used by
    /// `try_inject_stdin` so workflow `ContinueInCurrentContainer` can push a
    /// fresh prompt into the running container.
    stdin_injector: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    container_name: String,
    started_at: chrono::DateTime<chrono::Utc>,
    /// The attach rendezvous socket, for backends whose CLI has no native
    /// attach and whose run took the PTY path (`None` otherwise). Explicitly
    /// closed in `wait_blocking` once the container has exited, which aborts
    /// the socket server task and removes the socket file.
    attach_socket: Option<AttachSocketGuard>,
    /// Credential-refresh leases held for the container's whole life. Dropped
    /// (deregistering each lease) when this backend is consumed by
    /// `wait_blocking` on child exit, or when the execution is dropped unwaited.
    /// The monitor stops writing this container's staged file the moment these
    /// drop. Not read directly — held purely for its `Drop`.
    #[allow(dead_code)]
    leases: Vec<CredentialLease>,
    /// This launch's temporary mounts. Released when the child
    /// exits: `wait_blocking` consumes this backend then, and an unwaited drop
    /// hands them to a thread that outlasts the child.
    prompt_files: Vec<PromptFileGuard>,
}

impl Drop for ContainerExecution {
    /// An execution dropped unwaited leaves its container running for a later
    /// reattach, and the guest can still read its temporary mounts. Keep
    /// them until the CLI child exits. When awman exits first the thread dies
    /// with it and the files stay on disk for that guest.
    fn drop(&mut self) {
        let prompt_files = std::mem::take(&mut self.prompt_files);
        let child = self.child.take();
        let pty_child = self.pty_child.take();
        if prompt_files.is_empty() || (child.is_none() && pty_child.is_none()) {
            return;
        }
        std::thread::spawn(move || {
            if let Some(mut child) = child {
                let _ = child.wait();
            }
            if let Some(mut child) = pty_child {
                let _ = child.wait();
            }
            drop(prompt_files);
        });
    }
}

impl ExecutionBackend for ContainerExecution {
    fn wait_blocking(mut self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        // PTY-bridged path: wait on the portable-pty child.
        if let Some(mut child) = self.pty_child.take() {
            let status = child
                .wait()
                .map_err(|e| EngineError::Container(format!("wait {} (pty): {e}", self.cli.bin)))?;
            // Drop the master AFTER the child exits so the reader thread sees
            // EOF cleanly.
            self.pty_master = None;
            // Close the attach rendezvous socket now that the container has
            // exited, rather than leaving it to the eventual drop of `self`.
            drop(self.attach_socket.take());
            let exit_code = status.exit_code().try_into().unwrap_or(-1);
            return Ok(AgentExitInfo {
                exit_code,
                signal: None,
                started_at: self.started_at,
                ended_at: chrono::Utc::now(),
            });
        }

        // Piped path: wait on std::process::Child.
        let mut child = self
            .child
            .take()
            .ok_or_else(|| EngineError::Container("execution already waited".into()))?;
        let status = child
            .wait()
            .map_err(|e| EngineError::Container(format!("wait {}: {e}", self.cli.bin)))?;

        // Backend hook: after an interactive run, docker leaves stdio in
        // O_NONBLOCK mode on Unix. Restore it.
        (self.cli.post_wait)();

        // Piped runs never open an attach socket, but drop it explicitly for
        // symmetry with the PTY-bridged path above.
        drop(self.attach_socket.take());

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

    fn try_inject_stdin(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        if let Some(tx) = &self.stdin_injector {
            tx.send(bytes.to_vec())
                .map_err(|e| EngineError::Container(format!("inject stdin: {e}")))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cancel(&self) -> Result<(), EngineError> {
        stop_and_remove(&self.cli, &self.container_name);
        Ok(())
    }

    fn cancel_handle(&self) -> crate::engine::agent_runtime::execution::CancelHandle {
        let cli = self.cli.clone();
        let name = self.container_name.clone();
        crate::engine::agent_runtime::execution::CancelHandle::new(move || {
            stop_and_remove(&cli, &name);
            Ok(())
        })
    }

    fn cancel_on_execution_drop(&self) -> bool {
        // Docker containers are explicitly reattachable after their owner
        // exits. A dropped, never-awaited execution must not remove one.
        false
    }
}
