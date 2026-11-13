//! SDK operations run on a host control thread. VMM execution occurs exclusively
//! in the same-executable child dispatched by worker.rs.
use super::catalog::{CatalogState, CatalogUpgrade, CatalogUpgraded, LabelledSandbox};
use super::{cpu, driver::*, naming, paths::BuiltinPaths};
use crate::engine::{agent_runtime::execution::AgentStats, error::EngineError};
use microsandbox::{
    backend::{Backend, LocalBackend},
    Image, Sandbox,
};
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub struct MsbDriver {
    runtime: tokio::runtime::Handle,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    local: Arc<LocalBackend>,
    active: Arc<Mutex<std::collections::HashMap<String, Sandbox>>>,
    /// The last vCPU-time reading per sandbox, so each stats call measures
    /// usage since the previous one (see `cpu`).
    cpu: Arc<Mutex<std::collections::HashMap<String, cpu::CpuState>>>,
}
fn failure(operation: &str) -> EngineError {
    // SDK errors can include serialized configuration or user command/env.
    // Do not forward them to logs, frontends, or support bundles.
    EngineError::Other(format!("builtin runtime {operation} failed"))
}
/// Map a sandbox creation failure to a stable, config-free error. Only a
/// failure of the worker/VM startup itself carries the KVM/HVF hint; the SDK
/// message is never forwarded because it may contain the serialized config.
fn create_error(error: &microsandbox::MicrosandboxError, name: &str, image: &str) -> EngineError {
    use microsandbox::MicrosandboxError as E;
    match error {
        E::SandboxAlreadyExists(_) | E::SandboxStillRunning(_) => {
            EngineError::BuiltinSandboxExists {
                name: name.to_string(),
            }
        }
        E::ImageNotFound(_) => EngineError::BuiltinImageNotImported {
            image: image.to_string(),
        },
        E::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => EngineError::Other(
            "builtin sandbox creation failed: no space left on device".into(),
        ),
        E::Io(io) => EngineError::Other(format!(
            "builtin sandbox creation failed: I/O error ({})",
            io.kind()
        )),
        E::Database(_) => failure("catalog update during create"),
        E::InvalidConfig(_) => failure("create configuration"),
        E::BootStart { .. }
        | E::Runtime(_)
        | E::LibkrunfwNotFound(_)
        | E::RuntimeNotInstalled(_)
        | E::RuntimeIncomplete(_) => EngineError::BuiltinRuntimeUnavailable {
            reason: "embedded worker startup failed; verify KVM access or the macOS com.apple.security.hypervisor entitlement and matching payloads".into(),
        },
        _ => failure("sandbox creation"),
    }
}
impl Drop for MsbDriver {
    fn drop(&mut self) {
        if let Ok(sender) = self.shutdown.get_mut() {
            sender.take();
        }
    }
}
impl MsbDriver {
    /// Open the driver, first upgrading an older catalog in place when the
    /// marker allows it. The second value describes that upgrade, if one ran.
    pub fn open(
        paths: BuiltinPaths,
        ambient_overrides: &[&'static str],
    ) -> Result<(Arc<Self>, Option<CatalogUpgraded>), EngineError> {
        let ambient_overrides = ambient_overrides.to_vec();
        super::blocking::BlockingOperation::run(move || {
            Self::open_blocking(paths, &ambient_overrides)
        })
    }
    fn open_blocking(
        paths: BuiltinPaths,
        ambient_overrides: &[&'static str],
    ) -> Result<(Arc<Self>, Option<CatalogUpgraded>), EngineError> {
        let executable = super::embedded::EmbeddedRuntime::resolve(ambient_overrides)?;
        let catalog = paths.prepare_catalog()?;
        // This pinned SDK entrypoint starts with GlobalConfig::default() and
        // never loads installed msb config or resolves paths from HOME/XDG.
        // All state roots below are owned by awman and remain explicit.
        let local = isolated_backend(&paths)?;
        check_embedded_resolution(&local, &executable)?;
        let local = Arc::new(local);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("awman-builtin-control".into())
            .spawn(move || {
                // The SDK emits command argv at debug level. Keep third-party
                // control-plane traces out of user logs even under RUST_LOG=trace.
                let _quiet =
                    tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        let _ = tx.send(Ok(runtime.handle().clone()));
                        runtime.block_on(async {
                            let _ = done.await;
                        });
                    }
                    Err(_) => {
                        let _ = tx.send(Err(failure("control runtime startup")));
                    }
                }
            })
            .map_err(|_| failure("control thread startup"))?;
        let runtime = rx.recv().map_err(|_| failure("control thread startup"))??;
        let driver = Arc::new(Self {
            runtime,
            shutdown: Mutex::new(Some(shutdown)),
            local,
            active: Arc::new(Mutex::new(Default::default())),
            cpu: Arc::new(Mutex::new(Default::default())),
        });
        // The driver's own backend has not opened the database yet. The
        // upgrade migrates through a separate backend whose connections are
        // closed before the marker is committed (or `db/` restored), so the
        // driver only ever opens the committed catalog.
        let upgraded = match catalog {
            CatalogState::Ready => None,
            CatalogState::NeedsUpgrade { .. } => {
                let upgrade = MsbCatalogUpgrade {
                    runtime: driver.runtime.clone(),
                    local: Arc::new(isolated_backend(&paths)?),
                    opened: AtomicBool::new(false),
                };
                paths.upgrade_catalog(&upgrade)?
            }
        };
        Ok((driver, upgraded))
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        run_on(&self.runtime, self.backend(), future)
    }

    fn backend(&self) -> Arc<dyn Backend> {
        self.local.clone()
    }
}

/// Run `future` on the control runtime with `backend` as the SDK's backend.
fn run_on<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    backend: Arc<dyn Backend>,
    future: impl Future<Output = Result<T, EngineError>> + Send + 'static,
) -> Result<T, EngineError> {
    let runtime = runtime.clone();
    super::blocking::BlockingOperation::run(move || {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        runtime.spawn(async move {
            let _ = tx.send(microsandbox::backend::with_backend(backend, future).await);
        });
        rx.recv().map_err(|_| failure("control task"))?
    })
}

/// The SDK side of the in-place catalog upgrade (`catalog::upgrade_catalog`),
/// on an isolated backend of its own so every connection it opens can be
/// closed before the marker is committed or `db/` is restored.
struct MsbCatalogUpgrade {
    runtime: tokio::runtime::Handle,
    local: Arc<LocalBackend>,
    /// Whether `migrate` opened the database pools that `close` must close.
    opened: AtomicBool,
}
impl MsbCatalogUpgrade {
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        run_on(&self.runtime, self.local.clone(), future)
    }
}
impl CatalogUpgrade for MsbCatalogUpgrade {
    fn migrate(&self) -> Result<(), EngineError> {
        let local = self.local.clone();
        // Connecting applies the SDK's forward migrations and reconciles
        // snapshot descriptors; it never starts, stops or connects to a VM.
        self.call(async move {
            local
                .db()
                .await
                .map(|_| ())
                .map_err(|_| failure("catalog migration"))
        })?;
        self.opened.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn sandbox_labels(&self) -> Result<Vec<LabelledSandbox>, EngineError> {
        let backend: Arc<dyn Backend> = self.local.clone();
        self.call(async move {
            let mut all = Vec::new();
            let mut cursor = None;
            loop {
                let mut query = microsandbox::sandbox::SandboxListBuilder::default()
                    .label(naming::LABEL_AWMAN, "true");
                if let Some(c) = cursor {
                    query = query.cursor(c);
                }
                let page = backend
                    .sandboxes()
                    .list(backend.clone(), query)
                    .await
                    .map_err(|_| failure("upgrade discovery"))?;
                for handle in page.sandboxes {
                    let config = handle
                        .config()
                        .map_err(|_| failure("upgrade catalog read"))?;
                    all.push((handle.name().to_string(), config.spec.labels.clone()));
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            Ok(all)
        })
    }
    fn retire_stale_sandbox(&self, id: &str) -> Result<(), EngineError> {
        let backend: Arc<dyn Backend> = self.local.clone();
        let id = id.to_string();
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("upgrade lookup"))?;
            // Deliberately no `naming::check_protocol`: the sandbox is stale
            // by definition. The SDK refuses to remove one it still sees
            // running, which fails (and rolls back) the upgrade.
            handle
                .remove()
                .await
                .map_err(|_| failure("stale sandbox removal"))
        })
    }
    fn close(&self) -> Result<(), EngineError> {
        if !self.opened.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let local = self.local.clone();
        self.call(async move {
            let pools = local.db().await.map_err(|_| failure("catalog close"))?;
            let read = pools.read().inner().close_by_ref().await;
            let write = pools.write().inner().close_by_ref().await;
            read.and(write).map_err(|_| failure("catalog close"))
        })
    }
}

/// The SDK must resolve both runtime components to this executable, from the
/// process-level setters `EmbeddedRuntime::resolve` registered. In 0.7.8 those
/// setters feed the isolated config's `paths.msb`/`paths.libkrunfw`, so the
/// pair reports `RuntimeOrigin::Configuration`. That config has no installed or
/// managed layer and ambient `MSB_*` overrides are refused earlier, so these
/// values can only be awman's own registration; check them as well.
fn check_embedded_resolution(
    local: &LocalBackend,
    executable: &std::path::Path,
) -> Result<(), EngineError> {
    let resolved = microsandbox::setup::resolve_runtime(local.config())
        .map_err(|_| failure("embedded resolution"))?;
    let paths = &local.config().paths;
    if resolved.msb_path != executable
        || resolved.libkrunfw_path != executable
        || resolved.origin != microsandbox::setup::RuntimeOrigin::Configuration
        || paths.msb.as_deref() != Some(executable)
        || paths.libkrunfw.as_deref() != Some(executable)
        || paths.agentd.is_some()
    {
        return Err(EngineError::BuiltinRuntimeUnavailable {
            reason: "SDK resolved a non-embedded runtime component".into(),
        });
    }
    Ok(())
}
fn isolated_backend(paths: &BuiltinPaths) -> Result<LocalBackend, EngineError> {
    LocalBackend::builder()
        .home(&paths.home)
        .cache_dir(paths.home.join("cache"))
        .sandboxes_dir(paths.home.join("sandboxes"))
        .logs_dir(paths.home.join("logs"))
        .volumes_dir(paths.home.join("volumes"))
        .snapshots_dir(paths.home.join("snapshots"))
        .secrets_dir(paths.home.join("secrets"))
        .registry_hosts(Default::default())
        .ca_certs(None)
        .build_lazy_isolated()
        .map_err(|_| EngineError::BuiltinRuntimeUnavailable {
            reason: "isolated SDK configuration rejected".into(),
        })
}
/// How long a stats call with no earlier reading waits for the runtime's next
/// metrics sample (published once a second), and how often it looks.
const CPU_SAMPLE_WAIT: Duration = Duration::from_millis(1500);
const CPU_SAMPLE_POLL: Duration = Duration::from_millis(250);

fn cpu_reading(metrics: &microsandbox::SandboxMetrics) -> cpu::CpuReading {
    cpu::CpuReading {
        vcpu_time_ns: metrics.vcpu_time_ns,
        sampled_at: metrics.timestamp,
        uptime: metrics.uptime,
    }
}
fn summary(handle: &microsandbox::sandbox::SandboxHandle) -> Result<SandboxSummary, EngineError> {
    let config = handle.config().map_err(|_| failure("catalog read"))?;
    let labels = config.spec.labels.clone();
    if labels.get(naming::LABEL_AWMAN).map(String::as_str) != Some("true") {
        return Err(failure("ownership validation"));
    }
    let name = labels
        .get(naming::LABEL_NAME)
        .cloned()
        .ok_or_else(|| failure("name validation"))?;
    if naming::sandbox_name_for(&name) != handle.name() {
        return Err(failure("name collision validation"));
    }
    let image_tag = config
        .spec
        .image
        .oci_reference()
        .unwrap_or_default()
        .to_string();
    // The SDK's get/list already reconcile a Running/Draining row whose run PID
    // is dead (to Crashed/Stopped). When another process holds the transition
    // guard it returns the row unreconciled, but the handle then carries no
    // live PID; treat that as stopped rather than as a phantom running agent.
    use microsandbox::sandbox::SandboxStatus;
    let status = handle.status_snapshot();
    let live_pid = handle.local().and_then(|local| local.pid).is_some();
    let stopped = matches!(status, SandboxStatus::Stopped | SandboxStatus::Crashed)
        || (matches!(status, SandboxStatus::Running | SandboxStatus::Draining) && !live_pid);
    if !labels.contains_key(naming::LABEL_OWNER) {
        return Err(failure("owner validation"));
    }
    Ok(SandboxSummary {
        handle: crate::data::session::AgentHandle {
            id: handle.name().into(),
            name,
            image_tag,
            started_at: handle.created_at().unwrap_or_else(chrono::Utc::now),
        },
        labels,
        stopped,
    })
}
struct ControlSender(
    tokio::sync::mpsc::UnboundedSender<Control>,
    tokio::sync::mpsc::Sender<Vec<u8>>,
);
impl ExecControl for ControlSender {
    fn terminal_stdin(&self) -> Option<tokio::sync::mpsc::Sender<Vec<u8>>> {
        Some(self.1.clone())
    }
    fn send(&self, command: Control) -> Result<(), EngineError> {
        self.0
            .send(command)
            .map_err(|_| failure("exec control: session closed"))
    }
}
impl SandboxDriver for MsbDriver {
    fn create(&self, spec: SandboxSpec) -> Result<(), EngineError> {
        spec.check()?;
        let plan = spec.network.clone();
        let policy: microsandbox::sandbox::NetworkPolicy =
            serde_json::from_value(plan.policy_json()).map_err(|_| failure("network policy"))?;
        let backend = self.backend();
        let active = self.active.clone();
        self.call(async move {
            let id = spec.name.clone();
            let image = spec.image.clone();
            let display_name = spec
                .labels
                .get(naming::LABEL_NAME)
                .cloned()
                .unwrap_or_else(|| id.clone());
            let mut builder = Sandbox::builder(spec.name)
                .image(spec.image)
                .cpus(spec.vcpus)
                .memory(spec.memory_mib)
                .root_disk(spec.root_disk_mib)
                .pull_policy(microsandbox::sandbox::PullPolicy::Never)
                .labels(spec.labels)
                .quiet_logs();
            builder = if plan.enabled {
                builder.network(|network| {
                    network
                        .policy(policy)
                        .strict(plan.strict)
                        .strict_sni(plan.strict)
                        .trust_host_cas(plan.trust_host_cas)
                        .dns(|dns| {
                            dns.rebind_protection(true)
                                .nameservers(plan.nameservers.clone())
                        })
                        .tls(|tls| tls.enabled(false))
                })
            } else {
                builder.disable_network().network(|network| {
                    network
                        .policy(policy)
                        .strict(plan.strict)
                        .strict_sni(plan.strict)
                        .trust_host_cas(plan.trust_host_cas)
                        .dns(|dns| {
                            dns.rebind_protection(true)
                                .nameservers(plan.nameservers.clone())
                        })
                        .tls(|tls| tls.enabled(false))
                })
            };
            for mount in spec.mounts {
                let owner = spec.mount_owner;
                let quota = spec.mount_quota_mib;
                builder = builder.volume(mount.guest, |m| {
                    let mut m = m.bind(symlink_free_ancestors(&mount.host)).quota(quota);
                    if mount.read_only {
                        m = m.readonly();
                    }
                    if let Some((uid, gid)) = owner {
                        m = m.owner(uid, gid);
                    }
                    m
                });
            }
            let config = builder
                .build()
                .await
                .map_err(|_| failure("create configuration"))?;
            // No env secrets or launch argv in durable config; exec carries those
            // over the SDK agent IPC after the guest starts.
            let sandbox = backend
                .sandboxes()
                .create(backend.clone(), config, true)
                .await
                .map_err(|e| create_error(&e, &display_name, &image))?;
            // Hold the SDK's attached lifecycle owner. Its parent-watch pipe
            // stops the VM after owner death; exec/attach never adopt that fd.
            active
                .lock()
                .map_err(|_| failure("lifecycle registry"))?
                .insert(id, sandbox);
            Ok(())
        })
    }
    fn get(&self, id: &str) -> Result<SandboxSummary, EngineError> {
        let backend = self.backend();
        let id = id.to_string();
        self.call(async move {
            summary(
                &backend
                    .sandboxes()
                    .get(backend.clone(), &id)
                    .await
                    .map_err(|_| failure("lookup"))?,
            )
        })
    }
    fn list(&self) -> Result<Vec<SandboxSummary>, EngineError> {
        let backend = self.backend();
        self.call(async move {
            let mut all = Vec::new();
            let mut cursor = None;
            loop {
                let mut query = microsandbox::sandbox::SandboxListBuilder::default()
                    .label(naming::LABEL_AWMAN, "true");
                if let Some(c) = cursor {
                    query = query.cursor(c);
                }
                let page = backend
                    .sandboxes()
                    .list(backend.clone(), query)
                    .await
                    .map_err(|_| failure("discovery"))?;
                for handle in page.sandboxes {
                    all.push(summary(&handle)?);
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            Ok(all)
        })
    }
    fn exec(&self, id: &str, request: ExecRequest) -> Result<ExecSession, EngineError> {
        let backend = self.backend();
        let id = id.to_string();
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("lookup"))?;
            naming::check_protocol(&summary(&handle)?.labels)?;
            let sandbox = handle.connect().await.map_err(|_| failure("connect"))?;
            let (program, args) = request
                .argv
                .split_first()
                .ok_or_else(|| EngineError::Config("builtin execution needs a command".into()))?;
            let exec = sandbox
                .exec_stream_with(program, |e| {
                    let mut e = e
                        .args(args.iter().cloned())
                        .envs(request.env)
                        .tty(request.tty.is_some());
                    if let Some(cwd) = request.cwd {
                        e = e.cwd(cwd);
                    }
                    if let Some(user) = request.user {
                        e = e.user(user);
                    }
                    if request.stdin {
                        e.stdin_pipe()
                    } else {
                        e.stdin_null()
                    }
                })
                .await
                .map_err(|_| failure("exec"))?;
            if let Some((cols, rows)) = request.tty {
                exec.resize(rows, cols)
                    .await
                    .map_err(|_| failure("initial resize"))?;
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (events, receiver) = tokio::sync::broadcast::channel(4096);
            let (terminal_stdin, terminal_input) = crate::engine::terminal_hub::input_channel();
            let control = Arc::new(ControlSender(tx, terminal_stdin));
            tokio::spawn(pump(exec, rx, events.clone(), sandbox, terminal_input));
            Ok(ExecSession {
                events: receiver,
                broadcast: events,
                control,
            })
        })
    }
    fn finish_owned(
        &self,
        id: &str,
        owner: &str,
        grace: Duration,
        remove: bool,
    ) -> Result<(), EngineError> {
        let backend = self.backend();
        let id = id.to_string();
        let owner = owner.to_string();
        let active = self.active.clone();
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("lookup"))?;
            let info = summary(&handle)?;
            naming::check_owner(&info.labels, &owner)?;
            if !info.stopped
                && handle.stop_with_timeout(grace).await.is_err()
                && handle
                    .kill_with_timeout(Duration::from_secs(5))
                    .await
                    .is_err()
            {
                // The agent may have exited, or its launcher's own exit
                // cleanup stopped it, between the check above and the stop:
                // then stop and kill both fail although the goal is met.
                // Only a sandbox that is still running is a failure.
                let still_running = match backend.sandboxes().get(backend.clone(), &id).await {
                    Ok(current) => !summary(&current)?.stopped,
                    Err(_) => false, // already removed
                };
                if still_running {
                    return Err(failure("stop"));
                }
            }
            active
                .lock()
                .map_err(|_| failure("lifecycle registry"))?
                .remove(&id);
            if remove && handle.remove().await.is_err() {
                // Concurrent cleanup may already have removed it.
                if backend.sandboxes().get(backend.clone(), &id).await.is_ok() {
                    return Err(failure("remove"));
                }
            }
            Ok(())
        })
    }
    fn remove_stopped(&self, id: &str) -> Result<(), EngineError> {
        let backend = self.backend();
        let id = id.to_string();
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("lookup"))?;
            if !summary(&handle)?.stopped {
                return Err(failure("remove: sandbox is not stopped"));
            }
            handle.remove().await.map_err(|_| failure("remove"))
        })
    }
    fn stats(&self, id: &str) -> Result<AgentStats, EngineError> {
        let backend = self.backend();
        let id = id.to_string();
        let cpu = self.cpu.clone();
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("lookup"))?;
            let info = summary(&handle)?;
            let mut stats = handle.metrics().await.map_err(|_| failure("metrics"))?;
            let kept = cpu.lock().ok().and_then(|kept| kept.get(&id).copied());
            let percent = match cpu::step(kept.as_ref(), cpu_reading(&stats)) {
                cpu::CpuStep::Known(state) => Some(state),
                // Nothing to measure against yet: wait for the runtime's next
                // sample (it publishes one a second) and measure between them.
                cpu::CpuStep::AwaitNewerSample => {
                    let baseline = cpu_reading(&stats);
                    let deadline = tokio::time::Instant::now() + CPU_SAMPLE_WAIT;
                    loop {
                        tokio::time::sleep(CPU_SAMPLE_POLL).await;
                        let Ok(newer) = handle.metrics().await else {
                            break None;
                        };
                        let reading = cpu_reading(&newer);
                        if reading.sampled_at != baseline.sampled_at {
                            stats = newer;
                            break cpu::percent_between(&baseline, &reading)
                                .map(|percent| cpu::CpuState { reading, percent });
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break None;
                        }
                    }
                }
            };
            let cpu_percent = match percent {
                Some(state) => {
                    if let Ok(mut kept) = cpu.lock() {
                        kept.insert(id, state);
                    }
                    state.percent
                }
                // No second sample arrived in time: the runtime's own figure
                // is the best there is.
                None => f64::from(stats.cpu_percent),
            };
            Ok(AgentStats {
                name: info.handle.name,
                cpu_percent,
                memory_mb: stats.memory_bytes as f64 / 1048576.0,
            })
        })
    }
    fn image_config(&self, tag: &str) -> Result<Option<SandboxImageConfig>, EngineError> {
        let local = self.local.clone();
        let tag = tag.to_string();
        self.call(async move {
            let detail = match Image::inspect_local(&local, &tag).await {
                Ok(d) => d,
                Err(microsandbox::MicrosandboxError::ImageNotFound(_)) => return Ok(None),
                Err(_) => return Err(failure("image inspection")),
            };
            let Some(config) = detail.config else {
                return Ok(Some(SandboxImageConfig::default()));
            };
            let home = config
                .env
                .iter()
                .rev()
                .find_map(|e| e.strip_prefix("HOME=").map(str::to_owned));
            let mut argv = config.entrypoint.unwrap_or_default();
            argv.extend(config.cmd.unwrap_or_default());
            // `/etc/passwd` is needed to resolve a named USER, and to find
            // the effective user's home when the config sets no HOME (the
            // fallback Docker applies).
            let accounts = if needs_image_accounts(config.user.as_deref()) || home.is_none() {
                let mut layers: Vec<_> = detail.layers.iter().collect();
                layers.sort_by_key(|layer| std::cmp::Reverse(layer.position));
                let cache = microsandbox_image::GlobalCache::new(&local.cache_dir())
                    .map_err(|_| failure("image cache"))?;
                let paths: Vec<PathBuf> = layers
                    .iter()
                    .map(|layer| {
                        layer
                            .diff_id
                            .parse::<microsandbox_image::Digest>()
                            .map(|digest| cache.layer_erofs_path(&digest))
                            .map_err(|_| failure("image layer identity"))
                    })
                    .collect::<Result<_, _>>()?;
                ImageAccounts {
                    passwd: read_top_down(&paths, "etc", "passwd"),
                    group: read_top_down(&paths, "etc", "group"),
                }
            } else {
                ImageAccounts::default()
            };
            let home = home.or_else(|| {
                accounts
                    .passwd
                    .as_deref()
                    .and_then(|passwd| super::driver::passwd_home(passwd, config.user.as_deref()))
            });
            Ok(Some(SandboxImageConfig {
                home,
                user: config.user,
                workdir: config.working_dir,
                argv,
                accounts,
            }))
        })
    }
    fn import_archive(&self, path: PathBuf, tag: String) -> Result<(), EngineError> {
        let local = self.local.clone();
        self.call(async move {
            Image::load_local(&local, &path, vec![tag])
                .await
                .map_err(|_| failure("image import"))?;
            Ok(())
        })
    }
    fn remove_image(&self, tag: &str) -> Result<(), EngineError> {
        let local = self.local.clone();
        let tag = tag.to_string();
        self.call(async move {
            Image::remove_local(&local, &tag, false)
                .await
                .map_err(|_| failure("image removal"))
        })
    }
}
// A numeric UID without an explicit GID still inherits its passwd primary
// group. Skipping the lookup would present mounts as gid 0 even when exec
// resolves the same USER to a nonzero group.
fn needs_image_accounts(user: Option<&str>) -> bool {
    user.filter(|user| !user.is_empty())
        .is_some_and(|user| match user.split_once(':') {
            Some((uid, gid)) => uid.parse::<u32>().is_err() || gid.parse::<u32>().is_err(),
            None => true,
        })
}

/// Read `<dir>/<file>` from the effective rootfs of EROFS layers ordered top
/// first, honouring overlay whiteouts and opaque directories. Symlinks and
/// unreadable layers yield `None` (the caller then refuses a named USER).
fn read_top_down(layers: &[PathBuf], dir: &str, file: &str) -> Option<String> {
    use microsandbox_image::erofs::{ErofsEntryKind, ErofsReader};
    let path = format!("{dir}/{file}");
    for layer in layers {
        let mut reader = ErofsReader::new(std::fs::File::open(layer).ok()?).ok()?;
        match reader.entry_info(&path) {
            Ok(info) if info.whiteout => return None,
            Ok(info) if info.kind == ErofsEntryKind::RegularFile => {
                return read_bounded_account_file(&mut reader, &path);
            }
            Ok(_) => return None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        match reader.entry_info(dir) {
            Ok(info) if info.whiteout || info.opaque => return None,
            Ok(info) if info.kind != ErofsEntryKind::Directory => return None,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

fn read_bounded_account_file(
    reader: &mut microsandbox_image::erofs::ErofsReader,
    path: &str,
) -> Option<String> {
    use std::io::{Error, ErrorKind, Read};
    const LIMIT: u64 = 4 * 1024 * 1024;
    let mut contents = None;
    // The SDK's path-based read_file allocates the complete inode before
    // returning it. Walk only until this inode, then use its bounded stream.
    let _: Result<(), Error> = reader.walk_entries(|reader, entry| {
        if entry.path != std::path::Path::new(path) {
            return Ok(());
        }
        if entry.size <= LIMIT {
            let mut bytes = Vec::new();
            reader
                .file_data_reader(entry.nid)?
                .take(LIMIT + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 <= LIMIT {
                contents = String::from_utf8(bytes).ok();
            }
        }
        // Found or oversized: stop the walk without visiting unrelated files.
        Err(Error::from(ErrorKind::Interrupted))
    });
    contents
}
async fn pump(
    mut exec: microsandbox::sandbox::exec::ExecHandle,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Control>,
    events: tokio::sync::broadcast::Sender<ExecEvent>,
    _sandbox: Sandbox,
    mut terminal_input: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    use microsandbox::sandbox::exec::ExecEvent as M;
    let mut stdin = exec.take_stdin();
    let mut controls_open = true;
    loop {
        tokio::select! {
            Some(bytes) = terminal_input.recv() => {
                if let Some(input) = &stdin { let _ = input.write(bytes).await; }
            }
            event=exec.recv()=> {
                let (event,finished)=match event {
                    Some(M::Started{..})=>continue,
                    Some(M::Stdout(data))=>(ExecEvent::Stdout(data.to_vec()),false),
                    Some(M::Stderr(data))=>(ExecEvent::Stderr(data.to_vec()),false),
                    Some(M::Exited{code})=>(ExecEvent::Exited(code),true),
                    Some(M::StdinError(_))=>continue,
                    Some(M::Failed(_))|None=>(ExecEvent::Failed,true),
                };
                let _=events.send(event);
                if finished {break;}
            }
            command=commands.recv(),if controls_open=> {
                match command {
                    Some(Control::Stdin(bytes))=>if let Some(input)=&stdin {let _=input.write(bytes).await;},
                    Some(Control::Eof)=>if let Some(input)=stdin.take() {let _=input.close().await;},
                    Some(Control::Resize(cols,rows))=>{let _=exec.resize(rows,cols).await;},
                    Some(Control::Interrupt)=>{let _=exec.signal(2).await;},
                    Some(Control::Kill)=>{let _=exec.kill().await;},
                    None=>{ controls_open=false; if let Some(input)=stdin.take() {let _=input.close().await;} }
                }
            }
        }
    }
}

/// Resolve symlinks in `host`'s ancestors, keeping its final component as
/// given. The SDK opens every bind root without following any symlink, so a
/// path under a system symlink (macOS `/tmp`, `/var` → `/private/...`) would
/// otherwise be refused; a symlinked final component still is.
fn symlink_free_ancestors(host: &std::path::Path) -> std::path::PathBuf {
    match (host.parent(), host.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or_else(|_| host.to_path_buf()),
        _ => host.to_path_buf(),
    }
}
