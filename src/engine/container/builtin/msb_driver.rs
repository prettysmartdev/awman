//! SDK operations run on a host control thread. VMM execution occurs exclusively
//! in the same-executable child dispatched by worker.rs.
use super::{driver::*, naming, paths::BuiltinPaths};
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
        E::SandboxAlreadyExists(_) | E::SandboxStillRunning(_) => EngineError::Other(format!(
            "a builtin sandbox for agent '{name}' already exists; stop or remove it (awman clean) or choose another name"
        )),
        E::ImageNotFound(_) => EngineError::Config(format!(
            "image {image} is not in the builtin image store; run awman ready to import it"
        )),
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
    pub fn open(paths: BuiltinPaths) -> Result<Arc<Self>, EngineError> {
        let executable = super::embedded::resolve()?;
        super::catalog::check(&paths)?;
        // SDK 0.7.2 loads this file BEFORE applying .home(). It has no setter
        // to disable that merge. Fail closed rather than inherit agentd paths.
        let ambient = microsandbox::config::config_path();
        if ambient.exists() {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: format!(
                    "a Microsandbox configuration exists at {}; awman's builtin runtime cannot run while it is present because it could change the embedded runtime. Move it aside (for example, rename it) and retry, or use runtime docker",
                    ambient.display()
                ),
            });
        }
        let local = LocalBackend::builder()
            .home(&paths.home)
            .cache_dir(paths.home.join("cache"))
            .sandboxes_dir(paths.home.join("sandboxes"))
            .logs_dir(paths.home.join("logs"))
            .volumes_dir(paths.home.join("volumes"))
            .snapshots_dir(paths.home.join("snapshots"))
            .secrets_dir(paths.home.join("secrets"))
            .registry_hosts(Default::default())
            .ca_certs(None)
            .try_build_lazy()
            .map_err(|_| failure("configuration"))?;
        let resolved = microsandbox::setup::resolve_runtime(local.config())
            .map_err(|_| failure("embedded resolution"))?;
        if resolved.msb_path != executable
            || resolved.libkrunfw_path != executable
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
        }))
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, EngineError>> + Send + 'static,
    ) -> Result<T, EngineError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let backend = self.backend();
        self.runtime.spawn(async move {
            let _ = tx.send(microsandbox::backend::with_backend(backend, future).await);
        });
        rx.recv().map_err(|_| failure("control task"))?
    }
    fn backend(&self) -> Arc<dyn Backend> {
        self.local.clone()
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
                .pull_policy(microsandbox::sandbox::PullPolicy::Never)
                .labels(spec.labels)
                .quiet_logs();
            for mount in spec.mounts {
                let owner = spec.mount_owner;
                builder = builder.volume(mount.guest, |m| {
                    let mut m = m.bind(mount.host);
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
            if !info.stopped && handle.stop_with_timeout(grace).await.is_err() {
                handle
                    .kill_with_timeout(Duration::from_secs(5))
                    .await
                    .map_err(|_| failure("stop"))?;
            }
            active
                .lock()
                .map_err(|_| failure("lifecycle registry"))?
                .remove(&id);
            if remove {
                handle.remove().await.map_err(|_| failure("remove"))?;
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
        self.call(async move {
            let handle = backend
                .sandboxes()
                .get(backend.clone(), &id)
                .await
                .map_err(|_| failure("lookup"))?;
            let info = summary(&handle)?;
            let stats = handle.metrics().await.map_err(|_| failure("metrics"))?;
            Ok(AgentStats {
                name: info.handle.name,
                cpu_percent: f64::from(stats.cpu_percent),
                memory_mb: stats.memory_bytes as f64 / 1048576.0,
            })
        })
    }
    fn image_config(&self, tag: &str) -> Result<Option<ImageConfigSummary>, EngineError> {
        let local = self.local.clone();
        let tag = tag.to_string();
        self.call(async move {
            let detail = match Image::inspect_local(&local, &tag).await {
                Ok(d) => d,
                Err(microsandbox::MicrosandboxError::ImageNotFound(_)) => return Ok(None),
                Err(_) => return Err(failure("image inspection")),
            };
            let Some(config) = detail.config else {
                return Ok(Some(ImageConfigSummary::default()));
            };
            let home = config
                .env
                .iter()
                .rev()
                .find_map(|e| e.strip_prefix("HOME=").map(str::to_owned));
            let mut argv = config.entrypoint.unwrap_or_default();
            argv.extend(config.cmd.unwrap_or_default());
            let accounts = if needs_image_accounts(config.user.as_deref()) {
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
            Ok(Some(ImageConfigSummary {
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

#[cfg(test)]
mod tests {
    use super::*;

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
