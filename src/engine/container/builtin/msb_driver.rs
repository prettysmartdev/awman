//! SDK operations run on a host control thread. VMM execution occurs exclusively
//! in the same-executable child dispatched by worker.rs.
use super::{cpu, driver::*, naming, paths::BuiltinPaths};
use crate::engine::{agent_runtime::execution::AgentStats, error::EngineError};
use microsandbox::{
    backend::{Backend, LocalBackend},
    Image, Sandbox,
};
use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
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
    pub fn open(
        paths: BuiltinPaths,
        ambient_overrides: &[&'static str],
    ) -> Result<Arc<Self>, EngineError> {
        let ambient_overrides = ambient_overrides.to_vec();
        super::blocking::BlockingOperation::run(move || {
            Self::open_blocking(paths, &ambient_overrides)
        })
    }
    fn open_blocking(
        paths: BuiltinPaths,
        ambient_overrides: &[&'static str],
    ) -> Result<Arc<Self>, EngineError> {
        let executable = super::embedded::EmbeddedRuntime::resolve(ambient_overrides)?;
        paths.check_catalog()?;
        // This pinned SDK entrypoint starts with GlobalConfig::default() and
        // never loads installed msb config or resolves paths from HOME/XDG.
        // All state roots below are owned by awman and remain explicit.
        let local = isolated_backend(&paths)?;
        let resolved = microsandbox::setup::resolve_runtime(local.config())
            .map_err(|_| failure("embedded resolution"))?;
        if resolved.msb_path != executable
            || resolved.libkrunfw_path != executable
            || resolved.origin != microsandbox::setup::RuntimeOrigin::SdkPackage
            || local.config().paths.agentd.is_some()
        {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "SDK resolved a non-embedded runtime component".into(),
            });
        }
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
        Ok(Arc::new(Self {
            runtime,
            shutdown: Mutex::new(Some(shutdown)),
            local,
            active: Arc::new(Mutex::new(Default::default())),
            cpu: Arc::new(Mutex::new(Default::default())),
        }))
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        let backend = self.backend();
        let runtime = self.runtime.clone();
        super::blocking::BlockingOperation::run(move || {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            runtime.spawn(async move {
                let _ = tx.send(microsandbox::backend::with_backend(backend, future).await);
            });
            rx.recv().map_err(|_| failure("control task"))?
        })
    }

    fn backend(&self) -> Arc<dyn Backend> {
        self.local.clone()
    }
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
struct ControlSender(tokio::sync::mpsc::UnboundedSender<Control>);
impl ExecControl for ControlSender {
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
            let control = Arc::new(ControlSender(tx));
            tokio::spawn(pump(exec, rx, events.clone(), sandbox));
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
) {
    use microsandbox::sandbox::exec::ExecEvent as M;
    let mut stdin = exec.take_stdin();
    let mut controls_open = true;
    loop {
        tokio::select! {
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn bind_sources_resolve_ancestor_symlinks_but_not_the_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(&real, real.join("alias")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("leaf")).unwrap();
        assert_eq!(
            super::symlink_free_ancestors(&real.join("alias").join("target")),
            real.join("target")
        );
        assert_eq!(
            super::symlink_free_ancestors(&real.join("leaf")),
            real.join("leaf")
        );
        let missing = std::path::Path::new("/nonexistent-awman/x");
        assert_eq!(super::symlink_free_ancestors(missing), missing);
    }

    use super::*;

    #[tokio::test]
    async fn sdk_network_config_spec_roundtrips_defaults_and_explicit_strict_sni() {
        use microsandbox::sandbox::NetworkSpec;
        use microsandbox_network::config::NetworkConfig;

        let defaults = NetworkConfig::default();
        let legacy: NetworkConfig = serde_json::from_str("{}").unwrap();
        assert!(defaults.strict && legacy.strict);
        assert!(!defaults.strict_sni && !legacy.strict_sni);
        let legacy_spec: NetworkSpec = serde_json::from_str("{}").unwrap();
        assert!(legacy_spec.strict);
        assert!(!legacy_spec.strict_sni);
        for strict in [false, true] {
            for strict_sni in [false, true] {
                let original = NetworkConfig {
                    strict,
                    strict_sni,
                    ..Default::default()
                };
                let spec: NetworkSpec =
                    serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
                assert_eq!((spec.strict, spec.strict_sni), (strict, strict_sni));
                let restored: NetworkConfig =
                    serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
                assert_eq!((restored.strict, restored.strict_sni), (strict, strict_sni));
                // Exercise the SDK builder's actual config -> spec conversion too.
                let config = Sandbox::builder("strict-sni-roundtrip")
                    .image("example.com/fixture:latest")
                    .network(|net| net.strict(strict).strict_sni(strict_sni))
                    .build()
                    .await
                    .unwrap();
                assert_eq!(
                    (config.spec.network.strict, config.spec.network.strict_sni),
                    (strict, strict_sni)
                );
            }
        }
    }

    #[test]
    fn isolated_sdk_constructor_ignores_explicit_config_and_keeps_builder_values() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("hostile.json");
        std::fs::write(&file, b"{ malformed user configuration").unwrap();
        assert!(LocalBackend::builder()
            .config_path(&file)
            .build_lazy()
            .is_err());
        let local = LocalBackend::builder()
            .config_path(&file)
            .home(directory.path().join("builder-home"))
            .default_cpus(3)
            .default_memory_mib(1024)
            .build_lazy_isolated()
            .unwrap();
        assert_eq!(local.config().home(), directory.path().join("builder-home"));
        assert_eq!(local.config().sandbox_defaults.cpus, 3);
        assert_eq!(local.config().sandbox_defaults.memory_mib, 1024);
    }

    #[test]
    fn sdk_hostname_dns_binding_cannot_authorize_udp_or_quic() {
        use crate::data::config::builtin_network::{
            BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode,
        };
        use microsandbox_network::{
            policy::{Action, DomainName, Protocol},
            shared::{ResolvedHostnameFamily, SharedState},
        };
        let settings = BuiltinNetworkSettings {
            mode: NetworkMode::Allowlist,
            allow: vec![NetworkAllowEntry::Domain("allowed.example".into())],
            ..Default::default()
        };
        let plan = crate::engine::container::builtin::network::compile(&settings);
        let policy: microsandbox::sandbox::NetworkPolicy =
            serde_json::from_value(plan.policy_json()).unwrap();
        let state = SharedState::new(16);
        let destination: std::net::SocketAddr = "93.184.216.34:443".parse().unwrap();
        state.cache_resolved_hostname(
            "allowed.example",
            ResolvedHostnameFamily::Ipv4,
            vec![destination.ip()],
            std::time::Duration::from_secs(60),
        );
        // Use the exact pinned SDK evaluator called by the UDP netstack.
        assert_eq!(
            policy.evaluate_egress(destination, Protocol::Udp, &state),
            Action::Deny
        );
        assert_eq!(
            policy.evaluate_egress(destination, Protocol::Tcp, &state),
            Action::Allow
        );
        assert_eq!(
            policy.evaluate_egress("93.184.216.35:443".parse().unwrap(), Protocol::Tcp, &state),
            Action::Deny
        );
        assert_eq!(
            policy.evaluate_dns_query(
                &"allowed.example".parse::<DomainName>().unwrap(),
                Protocol::Udp,
                53
            ),
            Action::Allow
        );
        assert_eq!(
            policy.evaluate_dns_query(
                &"unlisted.example".parse::<DomainName>().unwrap(),
                Protocol::Udp,
                53
            ),
            Action::Deny
        );
    }

    #[tokio::test]
    async fn sdk_accepts_every_compiled_network_policy() {
        use crate::data::config::builtin_network::{
            BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode,
        };
        use crate::engine::container::builtin::network;

        let none = BuiltinNetworkSettings {
            mode: NetworkMode::None,
            ..Default::default()
        };
        let public = BuiltinNetworkSettings::default();
        let allowlist = BuiltinNetworkSettings {
            mode: NetworkMode::Allowlist,
            allow: vec![
                NetworkAllowEntry::Domain("api.example.test".into()),
                NetworkAllowEntry::Suffix("example.org".into()),
            ],
            host_ports: vec![8765],
            ..Default::default()
        };
        for settings in [none, public, allowlist] {
            let plan = network::compile(&settings);
            let policy: microsandbox::sandbox::NetworkPolicy =
                serde_json::from_value(plan.policy_json()).unwrap();
            assert_eq!(serde_json::to_value(&policy).unwrap(), plan.policy_json());
            let builder =
                Sandbox::builder("network-policy-test").image("example.com/fixture:latest");
            let builder = if plan.enabled {
                builder.network(|net| {
                    net.policy(policy)
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
                builder.disable_network().network(|net| {
                    net.policy(policy)
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
            let config = builder.build().await.unwrap();
            let net = &config.spec.network;
            // These 0.7.8 additions would require an executing launch probe.
            assert!(!net.http.deny_response);
            assert!(net.tcp_accept_queue_size.is_none());
            assert!(config.spec.runtime.guest_clock.is_none());
            assert_eq!(net.enabled, plan.enabled);
            assert_eq!(net.strict, plan.strict);
            assert_eq!(net.strict_sni, plan.strict);
            let restored: microsandbox_network::config::NetworkConfig =
                serde_json::from_value(serde_json::to_value(net).unwrap()).unwrap();
            assert_eq!(restored.strict_sni, plan.strict);
            assert_eq!(
                serde_json::to_value(net.policy.as_ref().unwrap()).unwrap(),
                plan.policy_json()
            );
            assert!(!net.tls.as_ref().unwrap().enabled);
            assert!(net.ports.is_empty());
            assert!(net.outbound_proxy.is_none());
        }
    }

    #[test]
    fn sdk_isolated_config_ignores_installed_msb_config() {
        const CHILD: &str = "AWMAN_SDK_ISOLATION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let home = std::env::var_os("HOME").unwrap();
            let home = PathBuf::from(home);
            let ambient = microsandbox::config::config_path();
            let real_home = home.canonicalize().unwrap();
            assert!(
                ambient.starts_with(&home) || ambient.starts_with(&real_home),
                "test config must stay in temp HOME"
            );
            std::fs::create_dir_all(ambient.parent().unwrap()).unwrap();
            std::fs::write(
                &ambient,
                br#"{"home":"/tmp/hostile-msb-home","paths":{"msb":"/tmp/hostile-msb","libkrunfw":"/tmp/hostile-lib","agentd":"/tmp/hostile-agentd","cache":"/tmp/hostile-cache"}}"#,
            )
            .unwrap();
            let paths = BuiltinPaths {
                home: home.join("awman-state"),
            };
            let local = isolated_backend(&paths).unwrap();
            let config = local.config();
            assert_eq!(config.home(), paths.home);
            assert_eq!(config.cache_dir(), paths.home.join("cache"));
            assert_eq!(config.sandboxes_dir(), paths.home.join("sandboxes"));
            assert_eq!(config.volumes_dir(), paths.home.join("volumes"));
            assert_eq!(config.snapshots_dir(), paths.home.join("snapshots"));
            assert_eq!(config.logs_dir(), paths.home.join("logs"));
            assert_eq!(config.secrets_dir(), paths.home.join("secrets"));
            assert!(config.paths.agentd.is_none());
            assert!(config.paths.msb.is_none());
            assert!(config.paths.libkrunfw.is_none());
            // A malformed installed document must not stop this constructor
            // either; the ambient SDK constructor would fail here.
            std::fs::write(&ambient, b"{ hostile config").unwrap();
            let again = isolated_backend(&paths).unwrap();
            assert_eq!(again.config().home(), paths.home);
            assert!(again.config().paths.agentd.is_none());
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("engine::container::builtin::msb_driver::tests::sdk_isolated_config_ignores_installed_msb_config")
            .arg("--nocapture")
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    }

    #[test]
    fn image_accounts_are_bounded_and_an_invalid_upper_file_never_falls_back() {
        use microsandbox_image::{
            erofs::write_erofs,
            tree::{FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode, TreeNode},
        };
        let dir = tempfile::tempdir().unwrap();
        let write_layer = |name: &str, bytes: Vec<u8>| {
            let mut tree = FileTree::new();
            tree.insert(
                b"etc/passwd",
                TreeNode::RegularFile(RegularFileNode {
                    id: RegularFileId::new(),
                    metadata: InodeMetadata::default(),
                    xattrs: Vec::new(),
                    data: FileData::Memory(bytes),
                    nlink: 1,
                }),
            )
            .unwrap();
            let path = dir.path().join(name);
            write_erofs(&tree, &path).unwrap();
            path
        };
        let text = "agent:x:1000:1001::/home/agent:/bin/sh\n";
        let lower = write_layer("lower.erofs", text.as_bytes().to_vec());
        assert_eq!(
            read_top_down(std::slice::from_ref(&lower), "etc", "passwd").as_deref(),
            Some(text)
        );
        for bytes in [vec![b'x'; 4 * 1024 * 1024 + 1], vec![0xff]] {
            let upper = write_layer("upper.erofs", bytes);
            assert!(read_top_down(&[upper, lower.clone()], "etc", "passwd").is_none());
        }
    }

    #[test]
    fn numeric_user_without_gid_requires_the_image_account_database() {
        for user in ["1000", "agent", "agent:1001", "1000:staff"] {
            assert!(needs_image_accounts(Some(user)), "{user}");
        }
        for user in [None, Some(""), Some("1000:1001")] {
            assert!(!needs_image_accounts(user));
        }
    }
}
