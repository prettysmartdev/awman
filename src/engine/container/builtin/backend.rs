//! Container contract implementation. No operation renders a host CLI argv.
use super::{
    driver::*,
    exec_bridge::AttachInstance,
    instance::{Instance, LaunchPlan},
    naming,
    paths::BuiltinPaths,
    resources,
};
use crate::{
    data::{
        config::env::Env,
        message::UserMessageSink,
        oci_identity::ImageIdentity,
        session::{AgentHandle, Session},
    },
    engine::{
        agent_runtime::{
            background::ExecOutput,
            execution::{AgentInstance, AgentStats},
            Capabilities, DindSupport, ImageAcquisition, ImageImportRequest, ImportedImage,
        },
        container::{
            backend::ContainerBackend,
            options::{ModelFlagForm, OverlayPermission, OverlaySpec, ResolvedContainerOptions},
            runtime::{BuiltinRuntimeSettings, ContainerImageInfo},
        },
        error::EngineError,
        oci::{default_acquirer, AcquireLimits, AcquirePolicy, AcquireRequest},
    },
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
static CAPABILITIES: Capabilities = Capabilities {
    arbitrary_env_vars: true,
    arbitrary_host_mounts: true,
    cpu_limits: true,
    per_resource_stats: true,
    persistent_lifecycle: false,
    kit_declarative: false,
    dind: DindSupport::Never,
    host_paths_visible: true,
    session_label_supported: true,
    has_image_store: true,
    image_acquisition: ImageAcquisition::Import,
    fractional_cpu: false,
};
pub struct BuiltinBackend {
    pub driver: Arc<dyn SandboxDriver>,
    pub paths: BuiltinPaths,
    pub settings: BuiltinRuntimeSettings,
    pub owner: String,
    // Background env is intentionally memory-only; it must never enter the SDK
    // catalog. Setup/teardown operations always belong to their launching client.
    pub background_env: Mutex<HashMap<String, BackgroundState>>,
}
#[derive(Clone, Default)]
pub struct BackgroundState {
    pub env: HashMap<String, String>,
    pub user: Option<String>,
}
fn unsupported(operation: &'static str) -> EngineError {
    EngineError::UnsupportedOnRuntime {
        operation,
        runtime: "builtin",
    }
}
fn guest_path(path: &Path) -> Result<String, EngineError> {
    path.to_str()
        .filter(|s| s.starts_with('/'))
        .map(str::to_owned)
        .ok_or_else(|| EngineError::Config("guest path must be absolute UTF-8".into()))
}
/// Resolve the image USER (`name|uid[:group|gid]`) to the numeric guest owner
/// presented on bind mounts, using the image's own account database. No USER
/// means the SDK default (root) and no override. A named account that the
/// image does not define is refused rather than guessed.
fn image_owner(
    user: Option<&str>,
    accounts: &ImageAccounts,
) -> Result<Option<(u32, u32)>, EngineError> {
    let Some(user) = user.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let (name, group) = match user.split_once(':') {
        Some((name, group)) => (name, Some(group)),
        None => (user, None),
    };
    let unresolved = |what: &str| {
        EngineError::Config(format!(
            "cannot resolve image USER '{user}': {what} is not defined in the image's /etc/passwd or /etc/group; use a numeric USER uid:gid in the Dockerfile"
        ))
    };
    let entry = |db: &Option<String>, key: &str| -> Option<Vec<String>> {
        db.as_deref()?.lines().find_map(|line| {
            let fields: Vec<String> = line.split(':').map(str::to_owned).collect();
            (fields.len() >= 4 && fields[0] == key).then_some(fields)
        })
    };
    let (uid, primary_gid) = match name.parse::<u32>() {
        Ok(uid) => (uid, entry_by_uid(&accounts.passwd, uid).unwrap_or(0)),
        Err(_) if name == "root" && accounts.passwd.is_none() => (0, 0),
        Err(_) => {
            let fields = entry(&accounts.passwd, name).ok_or_else(|| unresolved("the user"))?;
            (
                fields[2].parse().map_err(|_| unresolved("the uid"))?,
                fields[3].parse().map_err(|_| unresolved("the gid"))?,
            )
        }
    };
    let gid = match group {
        None => primary_gid,
        Some(group) => match group.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => entry(&accounts.group, group)
                .and_then(|fields| fields[2].parse().ok())
                .ok_or_else(|| unresolved("the group"))?,
        },
    };
    Ok(Some((uid, gid)))
}
fn entry_by_uid(passwd: &Option<String>, uid: u32) -> Option<u32> {
    passwd.as_deref()?.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 4 && fields[2].parse::<u32>().ok() == Some(uid))
            .then(|| fields[3].parse().ok())
            .flatten()
    })
}
fn agent_argv(
    mut argv: Vec<String>,
    options: &ResolvedContainerOptions,
) -> Result<Vec<String>, EngineError> {
    if options.acp {
        return Ok(argv);
    }
    if let Some(flag) = &options.non_interactive_flag {
        argv.push(flag.clone());
    }
    argv.extend(options.agent_mode_flags.iter().cloned());
    for (tools, flag, label) in [
        (
            &options.disallowed_tools,
            &options.disallowed_tools_flag,
            "disallowed",
        ),
        (
            &options.allowed_tools,
            &options.allowed_tools_flag,
            "allowed",
        ),
    ] {
        if !tools.is_empty() {
            let flag = flag
                .as_ref()
                .ok_or_else(|| EngineError::Config(format!("{label} tools need an agent flag")))?;
            argv.extend([flag.clone(), tools.join(",")]);
        }
    }
    if let Some(model) = &options.model {
        match model {
            ModelFlagForm::Argument(name) => argv.extend(["--model".into(), name.clone()]),
            ModelFlagForm::Shorthand(flag) => argv.push(flag.clone()),
        }
    }
    if let Some((_, path, flag)) = &options.system_prompt_file {
        argv.extend([flag.clone(), guest_path(path)?]);
    }
    if let Some((flag, text)) = &options.system_prompt_inline {
        argv.extend([flag.clone(), text.clone()]);
    }
    for (flag, path) in &options.agent_add_dirs {
        argv.extend([flag.clone(), guest_path(path)?]);
    }
    if options.interactive {
        if let Some(prompt) = &options.seeded_prompt {
            if let Some(flag) = &options.interactive_seed_flag {
                argv.push(flag.clone());
            }
            argv.push(prompt.clone());
        }
    }
    Ok(argv)
}
fn mounts(overlays: &[OverlaySpec]) -> Result<Vec<MountSpec>, EngineError> {
    let mut resolved: Vec<MountSpec> = overlays.iter().map(|m| {
        let metadata = std::fs::metadata(&m.host_path).map_err(|e| EngineError::io(&m.host_path, e))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(EngineError::Config("builtin mounts require regular files or directories; sockets require an explicit bridge".into()));
        }
        Ok(MountSpec {
            host: m.host_path.clone(),
            guest: guest_path(&m.container_path)?,
            read_only: m.permission == OverlayPermission::ReadOnly,
        })
    }).collect::<Result<_, _>>()?;
    resolved.sort_by_key(|mount| mount.guest.matches('/').count());
    Ok(resolved)
}
impl BuiltinBackend {
    fn owned(&self, id: &str) -> Result<SandboxSummary, EngineError> {
        let summary = self.driver.get(id)?;
        naming::check_protocol(&summary.labels)?;
        if !summary
            .labels
            .get(naming::LABEL_OWNER)
            .is_some_and(|token| token.starts_with(&format!("{}/", self.owner)))
        {
            return Err(EngineError::Other(
                "builtin sandbox belongs to another session".into(),
            ));
        }
        Ok(summary)
    }
    fn plan(&self, options: ResolvedContainerOptions) -> Result<LaunchPlan, EngineError> {
        if options.allow_docker {
            return Err(unsupported("Docker socket bridge"));
        }
        let image = options
            .image
            .as_ref()
            .ok_or_else(|| EngineError::Config("builtin launch needs an image".into()))?
            .0
            .clone();
        let config = self.driver.image_config(&image)?.ok_or_else(|| {
            EngineError::Config(format!(
                "image {image} is not imported; run awman ready with an explicit image source"
            ))
        })?;
        let name = options
            .name
            .as_ref()
            .map(|n| n.0.clone())
            .unwrap_or_else(|| format!("awman-{}", uuid::Uuid::new_v4().simple()));
        let leases = crate::engine::credential_refresh::register_container_leases(&options, &name);
        let (vcpus, memory_mib) = resources::resolve(
            options.cpu,
            options.memory,
            (self.settings.vcpus, self.settings.memory_mib),
        )?;
        let mut overlays = options.overlays.clone();
        if let Some(settings) = &options.agent_settings {
            overlays.extend(settings.overlays.iter().cloned());
        }
        if let Some((host_path, container_path, _)) = &options.system_prompt_file {
            overlays.push(OverlaySpec {
                host_path: host_path.clone(),
                container_path: container_path.clone(),
                permission: OverlayPermission::ReadOnly,
            });
        }
        if let Some((_, host_path, container_path)) = &options.system_prompt_env_file {
            overlays.push(OverlaySpec {
                host_path: host_path.clone(),
                container_path: container_path.clone(),
                permission: OverlayPermission::ReadOnly,
            });
        }
        // The imported image is what actually runs, so its USER wins (the same
        // precedence HOME uses in engine::agent). The local Dockerfile's USER
        // is only a fallback for images that declare none.
        let user = config.user.clone().or(options.dockerfile_user.clone());
        let spec = SandboxSpec {
            name: naming::sandbox_name_for(&name),
            image,
            labels: naming::labels(
                &name,
                &format!("{}/{}", self.owner, uuid::Uuid::new_v4()),
                &options.labels,
            )?,
            mounts: mounts(&overlays)?,
            mount_owner: image_owner(user.as_deref(), &config.accounts)?,
            vcpus,
            memory_mib,
        };
        let argv = agent_argv(
            options
                .entrypoint
                .as_ref()
                .map(|e| e.0.clone())
                .unwrap_or(config.argv),
            &options,
        )?;
        let mut env = Vec::new();
        for key in options.env_passthrough {
            if let Some(value) = crate::data::config::env::host_var(&key.0) {
                env.push((key.0, value));
            }
        }
        env.extend(options.env_literal.into_iter().map(|e| (e.key, e.value)));
        env.extend(options.agent_credentials);
        if let Some((variable, _, container_path)) = &options.system_prompt_env_file {
            env.push((variable.clone(), guest_path(container_path)?));
        }
        let cwd = options
            .working_dir
            .map(|p| {
                p.to_str().map(str::to_owned).ok_or_else(|| {
                    EngineError::Config("guest working directory must be UTF-8".into())
                })
            })
            .transpose()?
            .or(config.workdir);
        let command = ExecRequest {
            argv,
            cwd,
            user,
            env,
            ..Default::default()
        };
        Ok(LaunchPlan {
            spec,
            command,
            name,
            remove_on_exit: options.remove_on_exit,
            persistent_stdin: options.acp || options.interactive,
            seeded_prompt: if options.acp || options.interactive {
                None
            } else {
                options.seeded_prompt
            },
            leases,
        })
    }
    fn identities_path(&self) -> std::path::PathBuf {
        self.paths.home.join("awman-images.json")
    }
    fn identities(&self) -> Result<std::collections::BTreeMap<String, ImageIdentity>, EngineError> {
        let path = self.identities_path();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| EngineError::Config("invalid builtin image identity catalog".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(EngineError::io(path, e)),
        }
    }
    fn save_identities(
        &self,
        identities: &std::collections::BTreeMap<String, ImageIdentity>,
    ) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec(identities)
            .map_err(|_| EngineError::Other("cannot serialize image identities".into()))?;
        let mut file = tempfile::NamedTempFile::new_in(&self.paths.home)
            .map_err(|e| EngineError::io(&self.paths.home, e))?;
        std::io::Write::write_all(&mut file, &bytes)
            .map_err(|e| EngineError::io(file.path(), e))?;
        file.as_file()
            .sync_all()
            .map_err(|e| EngineError::io(file.path(), e))?;
        file.persist(self.identities_path())
            .map_err(|e| EngineError::io(self.identities_path(), e.error))?;
        Ok(())
    }
}
impl ContainerBackend for BuiltinBackend {
    fn name(&self) -> &'static str {
        "builtin"
    }
    fn display_name(&self) -> &'static str {
        "Builtin Microsandbox"
    }
    fn capabilities(&self) -> &'static Capabilities {
        &CAPABILITIES
    }
    fn host_cli(&self) -> Option<&'static str> {
        None
    }
    fn reattach_after_owner_exit(&self) -> bool {
        false
    }
    fn is_available(&self) -> Result<(), EngineError> {
        if self.settings.test_isolation {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "disabled under test isolation".into(),
            });
        }
        // On macOS the actual worker startup handshake reports entitlement/HVF
        // failures. SDK doctor only checks the architecture on macOS; it does
        // not verify the executable's signing entitlement or attempt HVF boot.
        self.driver.hypervisor_available()
    }
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.is_available()?;
        let plan = self.plan(options)?;
        let owner = plan
            .spec
            .labels
            .get(naming::LABEL_OWNER)
            .expect("launch plan owns its labels")
            .clone();
        Ok(Box::new(Instance {
            driver: self.driver.clone(),
            paths: self.paths.clone(),
            owner,
            plan,
        }))
    }
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        let summary = self.driver.get(&handle.id)?;
        naming::check_protocol(&summary.labels)?;
        if summary.stopped {
            return Err(EngineError::Other("builtin agent is stopped".into()));
        }
        let owner = summary
            .labels
            .get(naming::LABEL_OWNER)
            .ok_or_else(|| EngineError::Other("builtin sandbox has no owner label".into()))?;
        Ok(Box::new(AttachInstance {
            owner: owner.clone(),
            handle: summary.handle,
            paths: self.paths.clone(),
        }))
    }
    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.list_running_all()
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.list_running_with_name_prefix("")
    }
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self
            .driver
            .list()?
            .into_iter()
            .filter(|s| !s.stopped && s.handle.name.starts_with(prefix))
            .map(|s| s.handle)
            .collect())
    }
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self
            .driver
            .list()?
            .into_iter()
            .filter(|s| s.stopped)
            .map(|s| s.handle)
            .collect())
    }
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.driver.stats(&handle.id)
    }
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        // An explicit, user-initiated stop may target an agent launched by any
        // awman process (as `docker stop` can). The protocol label and the
        // handle generation (creation time + name) protect against stopping a
        // replacement that reused the name; the launch-scoped owner token is
        // only required for automatic cleanup paths (cancel, finish, stuck).
        let current = self.driver.get(&handle.id)?;
        naming::check_protocol(&current.labels)?;
        if current.handle.started_at != handle.started_at || current.handle.name != handle.name {
            return Err(EngineError::Other(
                "builtin handle refers to an older sandbox generation".into(),
            ));
        }
        let owner = current
            .labels
            .get(naming::LABEL_OWNER)
            .ok_or_else(|| EngineError::Other("builtin sandbox has no owner label".into()))?;
        self.driver
            .stop_owned(&handle.id, owner, Duration::from_secs(2))
    }
    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        let current = self.driver.get(id)?;
        self.driver.remove_stopped(id)?;
        if let Some(owner) = current.labels.get(naming::LABEL_OWNER) {
            let _ = std::fs::remove_file(self.paths.attach(id, owner));
        }
        Ok(())
    }
    fn exec_args(
        &self,
        _id: &str,
        _cwd: &str,
        _entry: &[&str],
        _env: &[(&str, &str)],
    ) -> Option<Vec<String>> {
        None
    }
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        Ok(self.driver.image_config(tag)?.is_some())
    }
    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError> {
        Ok(self.driver.image_config(tag)?.and_then(|c| c.home))
    }
    fn image_identity(&self, tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        Ok(self.identities()?.remove(tag))
    }
    fn build_image(
        &self,
        _tag: &str,
        _file: &Path,
        _context: &Path,
        _no_cache: bool,
        _line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Err(unsupported(
            "image building; build externally and import with ready",
        ))
    }
    fn import_image(
        &self,
        request: &ImageImportRequest,
        _sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        let acquirer = default_acquirer(
            &self.paths.home,
            AcquireLimits {
                max_archive_bytes: 32 * 1024 * 1024 * 1024,
                max_layer_bytes: 8 * 1024 * 1024 * 1024,
                max_layers: 256,
                min_free_bytes: 1024 * 1024 * 1024,
            },
            &Env::from_process(),
        );
        let acquired = acquirer.acquire(
            &AcquireRequest {
                tag: request.tag.clone(),
                source: request.source.clone(),
                platform: request.platform.clone(),
                policy: if request.refresh {
                    AcquirePolicy::Refresh
                } else {
                    AcquirePolicy::IfMissing
                },
                registries: self.settings.registries.clone(),
            },
            &mut |_| {},
        )?;
        // Imports serialise on their own lock; the catalog lock is only held
        // for the short identity read-modify-write so status/list never wait
        // on a long import.
        let _import = self.paths.import_lock()?;
        if self.image_identity(&request.tag)?.as_ref() != Some(&acquired.identity)
            || !self.image_exists(&request.tag)?
        {
            let (_staging, archive) = crate::engine::oci::archive::prepare_runtime_archive(
                &acquired,
                &request.tag,
                &AcquireLimits {
                    max_archive_bytes: 32 << 30,
                    max_layer_bytes: 8 << 30,
                    max_layers: 256,
                    min_free_bytes: 1 << 30,
                },
            )?;
            self.driver.import_archive(archive, request.tag.clone())?;
        }
        let config = self.driver.image_config(&request.tag)?.unwrap_or_default();
        {
            let _lock = self.paths.lock()?;
            let mut identities = self.identities()?;
            identities.insert(request.tag.clone(), acquired.identity.clone());
            self.save_identities(&identities)?;
        }
        Ok(ImportedImage {
            tag: request.tag.clone(),
            identity: acquired.identity,
            home_dir: config.home,
            user: config.user,
        })
    }
    fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError> {
        // Only explicitly tracked imports belong to awman. All have references;
        // never run the SDK's global prune against another client's images.
        Ok(Vec::new())
    }
    fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        let _lock = self.paths.lock()?;
        let mut identities = self.identities()?;
        if !identities.contains_key(id) {
            return Err(EngineError::Other("image is not owned by awman".into()));
        }
        self.driver.remove_image(id)?;
        identities.remove(id);
        self.save_identities(&identities)
    }
    fn start_background(
        &self,
        image: &str,
        workdir: &Path,
        env: &HashMap<String, String>,
        overlays: &[OverlaySpec],
    ) -> Result<String, EngineError> {
        self.is_available()?;
        let name = format!("awman-background-{}", uuid::Uuid::new_v4().simple());
        let id = naming::sandbox_name_for(&name);
        let mut mounted = mounts(overlays)?;
        let guest = workdir
            .to_str()
            .ok_or_else(|| EngineError::Config("guest working directory must be UTF-8".into()))?;
        if !mounted.iter().any(|m| m.guest == guest) {
            mounted.insert(
                0,
                MountSpec {
                    host: workdir.into(),
                    guest: guest.into(),
                    read_only: false,
                },
            );
        }
        let config = self.driver.image_config(image)?.ok_or_else(|| {
            EngineError::Config(format!(
                "image {image} is not imported; run awman ready with an explicit image source"
            ))
        })?;
        self.driver.create(SandboxSpec {
            name: id.clone(),
            image: image.into(),
            labels: naming::labels(
                &name,
                &format!("{}/{}", self.owner, uuid::Uuid::new_v4()),
                &[],
            )?,
            mounts: mounted,
            mount_owner: image_owner(config.user.as_deref(), &config.accounts)?,
            vcpus: self.settings.vcpus,
            memory_mib: self.settings.memory_mib,
        })?;
        self.background_env
            .lock()
            .map_err(|_| EngineError::Other("builtin background state poisoned".into()))?
            .insert(
                id.clone(),
                BackgroundState {
                    env: env.clone(),
                    user: config.user,
                },
            );
        Ok(id)
    }
    fn exec_in_background(
        &self,
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.exec_in_background_streaming(id, command, working_dir, env, &mut |_| {})
    }
    fn exec_in_background_streaming(
        &self,
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.owned(id)?;
        let state = self
            .background_env
            .lock()
            .map_err(|_| EngineError::Other("builtin background state poisoned".into()))?
            .get(id)
            .cloned()
            .unwrap_or_default();
        let mut vars = state.env;
        if let Some(env) = env {
            vars.extend(env.clone());
        }
        let session = self.driver.exec(
            id,
            ExecRequest {
                argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
                cwd: Some(working_dir.into()),
                // Like `docker exec`, setup/teardown run as the image USER.
                user: state.user,
                env: vars.into_iter().collect(),
                ..Default::default()
            },
        )?;
        // Blocking receiver is isolated from the caller's Tokio context.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut events = session.events;
            while let Ok(event) = events.blocking_recv() {
                let terminal = matches!(event, ExecEvent::Exited(_) | ExecEvent::Failed);
                if tx.send(event).is_err() || terminal {
                    break;
                }
            }
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_line = Vec::new();
        let mut stderr_line = Vec::new();
        let code = loop {
            match rx.recv() {
                Ok(ExecEvent::Stdout(bytes)) => {
                    stdout.extend_from_slice(&bytes);
                    lines(&mut stdout_line, bytes, on_line);
                }
                Ok(ExecEvent::Stderr(bytes)) => {
                    stderr.extend_from_slice(&bytes);
                    lines(&mut stderr_line, bytes, on_line);
                }
                Ok(ExecEvent::Exited(code)) => break code,
                _ => {
                    return Err(EngineError::Other(
                        "builtin background exec failed without an exit status".into(),
                    ))
                }
            }
        };
        for pending in [stdout_line, stderr_line] {
            if !pending.is_empty() {
                on_line(&String::from_utf8_lossy(&pending));
            }
        }
        Ok(ExecOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: code,
        })
    }
    fn stop_and_remove(&self, id: &str) -> Result<(), EngineError> {
        let current = self.owned(id)?;
        self.driver.stop_owned(
            id,
            &current.labels[naming::LABEL_OWNER],
            Duration::from_secs(2),
        )?;
        if let Ok(mut env) = self.background_env.lock() {
            env.remove(id);
        }
        let _ = std::fs::remove_file(self.paths.attach(id, &current.labels[naming::LABEL_OWNER]));
        Ok(())
    }
}

fn lines(pending: &mut Vec<u8>, bytes: Vec<u8>, callback: &mut dyn FnMut(&str)) {
    for byte in bytes {
        if byte == b'\n' {
            callback(&String::from_utf8_lossy(pending));
            pending.clear();
        } else {
            pending.push(byte);
        }
    }
}

#[cfg(test)]
mod compat_tests {
    use super::*;

    #[test]
    fn maps_agent_flags_without_splitting_values_and_preserves_acp_argv() {
        let options = ResolvedContainerOptions {
            interactive: true,
            agent_mode_flags: vec!["--permission-mode".into(), "plan".into()],
            allowed_tools: vec!["Bash(git status)".into(), "Read".into()],
            allowed_tools_flag: Some("--allowedTools".into()),
            model: Some(ModelFlagForm::Argument("model with spaces".into())),
            system_prompt_inline: Some(("--system".into(), "first\nsecond".into())),
            seeded_prompt: Some("task with spaces".into()),
            ..Default::default()
        };
        let base = vec!["claude".into()];
        assert_eq!(
            agent_argv(base.clone(), &options).unwrap(),
            vec![
                "claude",
                "--permission-mode",
                "plan",
                "--allowedTools",
                "Bash(git status),Read",
                "--model",
                "model with spaces",
                "--system",
                "first\nsecond",
                "task with spaces",
            ]
        );
        let mut acp = options;
        acp.acp = true;
        assert_eq!(agent_argv(base.clone(), &acp).unwrap(), base);
    }

    #[test]
    fn maps_single_file_without_mounting_its_parent_and_rejects_socket() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("prompt.txt");
        std::fs::write(&file, b"prompt").unwrap();
        let specs = mounts(&[OverlaySpec {
            host_path: file.clone(),
            container_path: "/home/agent/prompt.txt".into(),
            permission: OverlayPermission::ReadOnly,
        }])
        .unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host, file);
        assert_eq!(specs[0].guest, "/home/agent/prompt.txt");
        assert!(specs[0].read_only);
        let nested = mounts(&[
            OverlaySpec {
                host_path: file.clone(),
                container_path: "/workspace/nested/prompt.txt".into(),
                permission: OverlayPermission::ReadOnly,
            },
            OverlaySpec {
                host_path: dir.path().into(),
                container_path: "/workspace".into(),
                permission: OverlayPermission::ReadWrite,
            },
        ])
        .unwrap();
        assert_eq!(nested[0].guest, "/workspace");
        assert_eq!(nested[1].guest, "/workspace/nested/prompt.txt");
        #[cfg(unix)]
        {
            let socket = dir.path().join("docker.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            assert!(mounts(&[OverlaySpec {
                host_path: socket,
                container_path: "/var/run/docker.sock".into(),
                permission: OverlayPermission::ReadWrite
            }])
            .is_err());
        }
    }

    #[test]
    fn numeric_image_user_sets_guest_mount_owner() {
        let none = ImageAccounts::default();
        assert_eq!(
            image_owner(Some("1001:1002"), &none).unwrap(),
            Some((1001, 1002))
        );
        assert_eq!(image_owner(Some("1001"), &none).unwrap(), Some((1001, 0)));
        assert_eq!(image_owner(None, &none).unwrap(), None);
        assert_eq!(image_owner(Some("root"), &none).unwrap(), Some((0, 0)));
        let accounts = ImageAccounts {
            passwd: Some("agent:x:1001:1234::/home/agent:/bin/sh\n".into()),
            group: None,
        };
        assert_eq!(
            image_owner(Some("1001"), &accounts).unwrap(),
            Some((1001, 1234))
        );
    }

    #[test]
    fn named_image_user_resolves_from_the_image_account_database() {
        let accounts = ImageAccounts {
            passwd: Some(
                "root:x:0:0:root:/root:/bin/bash\nawman:x:1000:1000::/home/awman:/bin/bash\n"
                    .into(),
            ),
            group: Some("root:x:0:\nawman:x:1000:\nstaff:x:50:awman\n".into()),
        };
        assert_eq!(
            image_owner(Some("awman"), &accounts).unwrap(),
            Some((1000, 1000))
        );
        assert_eq!(
            image_owner(Some("awman:staff"), &accounts).unwrap(),
            Some((1000, 50))
        );
        assert_eq!(
            image_owner(Some("1000"), &accounts).unwrap(),
            Some((1000, 1000))
        );
        assert!(matches!(
            image_owner(Some("ghost"), &accounts),
            Err(EngineError::Config(message)) if message.contains("ghost")
        ));
        assert!(image_owner(Some("awman:nogroup"), &accounts).is_err());
        assert!(image_owner(Some("awman"), &ImageAccounts::default()).is_err());
    }
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod matrix_tests;
