//! A hermetic builtin `ContainerRuntime` for tests above this module: the real
//! `BuiltinBackend` (planning, launch settings, background launches) over the
//! in-memory driver. No VM, hypervisor, SDK or real HOME.
use super::backend::BuiltinBackend;
use super::driver::SandboxSpec;
use super::network;
use super::paths::BuiltinPaths;
use super::testing::{fixture_image, frontend, state_dir, FakeDriver, Peer};
use crate::data::config::builtin_network::BuiltinNetworkSettings;
use crate::engine::agent_runtime::execution::{AgentExecution, AgentInstance};
use crate::engine::container::runtime::{BuiltinRuntimeSettings, ContainerRuntime};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What one VM was created with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LaunchedVm {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub network_enabled: bool,
    pub network_policy: serde_json::Value,
}

impl From<SandboxSpec> for LaunchedVm {
    fn from(spec: SandboxSpec) -> Self {
        Self {
            vcpus: spec.vcpus,
            memory_mib: spec.memory_mib,
            network_enabled: spec.network.enabled,
            network_policy: spec.network.policy_json(),
        }
    }
}

/// One process-wide builtin runtime shared by every session in a test.
pub(crate) struct FakeBuiltin {
    _temp: tempfile::TempDir,
    driver: Arc<FakeDriver>,
    peers: Mutex<Vec<Peer>>,
    pub runtime: Arc<ContainerRuntime>,
    /// The process-wide state directory, as configured.
    pub state_dir: PathBuf,
}

impl FakeBuiltin {
    pub fn new() -> Self {
        let (temp, state) = state_dir();
        let paths = BuiltinPaths::resolve(&state).unwrap();
        let driver = FakeDriver::new();
        let backend = BuiltinBackend {
            driver: driver.clone(),
            paths,
            settings: BuiltinRuntimeSettings {
                state_dir: state.clone(),
                ambient_overrides: Vec::new(),
                test_isolation: false,
            },
            owner: "fake-process".into(),
            background_env: Default::default(),
        };
        Self {
            _temp: temp,
            driver,
            peers: Mutex::new(Vec::new()),
            runtime: Arc::new(ContainerRuntime::with_backend(Arc::new(backend))),
            state_dir: state,
        }
    }

    /// Make `tag` an imported image.
    pub fn import(&self, tag: &str) {
        self.driver
            .state
            .lock()
            .unwrap()
            .images
            .insert(tag.into(), fixture_image());
    }

    /// Run a built instance until its agent process starts; returns its id.
    pub async fn run(&self, instance: Box<dyn AgentInstance>) -> (String, AgentExecution) {
        let id = instance.handle_preview().id;
        let (fe, peer) = frontend(None);
        let execution = instance.run_with_frontend(fe).unwrap();
        self.driver.wait_for_exec(&id).await;
        self.peers.lock().unwrap().push(peer);
        (id, execution)
    }

    /// The VM `id` was created with.
    pub fn launched(&self, id: &str) -> LaunchedVm {
        self.driver.spec(id).into()
    }

    /// How many VMs were created.
    pub fn creates(&self) -> usize {
        self.driver.creates()
    }

    /// Finish the next phase shell execution and retain its launch policy for
    /// command-layer factory tests, even when normal cleanup removes the VM.
    pub async fn finish_phase_execution(&self) -> LaunchedVm {
        for _ in 0..400 {
            let launched = {
                let state = self.driver.state.lock().unwrap();
                state.sandboxes.iter().find_map(|sandbox| {
                    sandbox
                        .events
                        .as_ref()
                        .map(|events| (sandbox.spec.clone(), events.clone()))
                })
            };
            if let Some((spec, events)) = launched {
                let _ = events.send(super::driver::ExecEvent::Exited(0));
                return spec.into();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("phase execution never started");
    }

    /// The policy the backend compiles `settings` into.
    pub fn policy_for(settings: &BuiltinNetworkSettings) -> serde_json::Value {
        network::compile(settings).policy_json()
    }
}
