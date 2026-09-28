//! VM handle for entering microVMs.

#[cfg(not(feature = "tee"))]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::Infallible;
#[cfg(any(not(feature = "tee"), test))]
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::Arc;
#[cfg(not(feature = "tee"))]
use std::time::Duration;
use std::time::{Instant, SystemTime};

#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::ffi::CString;

use crossbeam_channel::unbounded;
#[cfg(not(target_os = "windows"))]
use devices::virtio::vsock::VsockDatagramPortBackend;
use devices::virtio::vsock::VsockPortBackend;
use log::error;
use polly::event_manager::EventManager;
use utils::eventfd::EventFd;
#[cfg(not(feature = "tee"))]
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};
#[cfg(not(target_os = "windows"))]
use vmm::resources::TsiFlags;
use vmm::resources::VmResources;
use vmm::vmm_config::kernel_bundle::InitrdBundle;
use vmm::vmm_config::kernel_bundle::KernelBundle;
use vmm::vmm_config::kernel_cmdline::KernelCmdlineConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

use super::builders::PlacementObserver;
use super::embedded_kernel::{embedded_kernel, EmbeddedKernel};
use super::error::{BuildError, Error, Result, RuntimeError};
use super::exit_handle::ExitHandle;
use super::metrics::MetricsHandle;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const INIT_PATH: &str = "/init.krun";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Handle to a configured VM ready to enter.
///
/// Created via [`VmBuilder::build()`](super::builder::VmBuilder::build).
pub struct Vm {
    vmr: VmResources,
    kernel_cmdline: Option<String>,
    exec_path: Option<String>,
    args: Option<String>,
    env: Option<String>,
    workdir: Option<String>,
    rlimits: Option<String>,
    krunfw_path: Option<PathBuf>,
    initramfs_path: Option<PathBuf>,
    init_path: Option<String>,
    exit_observers: Vec<Box<dyn Fn(i32) + Send + 'static>>,
    placement_observer: Option<PlacementObserver>,
    /// Pre-created exit event fd for triggering VM shutdown.
    exit_evt: EventFd,
    /// Shared exit code — written by the VMM, readable by exit observers.
    exit_code: Arc<AtomicI32>,
    /// Opt in to the automatic `TsiFlags::HIJACK_INET` fallback that
    /// bridges guest INET sockets to the host via vsock when no
    /// virtio-net device is configured. Set via
    /// [`MachineBuilder::enable_inet_hijack`](super::builders::MachineBuilder::enable_inet_hijack).
    #[cfg(not(target_os = "windows"))]
    enable_inet_hijack: bool,
    #[cfg(not(target_os = "windows"))]
    vsock_unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    vsock_custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
    #[cfg(not(target_os = "windows"))]
    vsock_custom_dgram_port_map: Option<HashMap<u32, Arc<dyn VsockDatagramPortBackend>>>,
    #[cfg(not(target_os = "windows"))]
    vsock_host_port_map: Option<HashMap<u16, u16>>,
    /// Keeps the libkrunfw library loaded so kernel memory pointers remain valid.
    _krunfw_library: Option<libloading::Library>,
    /// Keeps an explicit initramfs allocation alive until it is copied to guest memory.
    _initramfs_data: Option<Vec<u8>>,
    /// Receives the VMM reference and execution-state notifications after startup begins.
    #[cfg(not(feature = "tee"))]
    vmm_control: Arc<VmControlRegistry>,
    #[cfg(not(feature = "tee"))]
    execution_restore: Option<vmm::execution_state::ExecutionState>,
    #[cfg(not(feature = "tee"))]
    memory_restore: Option<Box<dyn VmMemoryRestoreSource>>,
    /// Device state installed after memory/execution reconstruction and before first activation.
    #[cfg(all(not(feature = "tee"), feature = "blk"))]
    device_restores: Vec<VmDeviceRestore>,
    /// Leave the constructed VMM at its initial paused boundary for an external activation gate.
    #[cfg(not(feature = "tee"))]
    start_paused: bool,
}

/// One construction-only device restore requested before [`Vm::enter`].
#[cfg(all(not(feature = "tee"), feature = "blk"))]
enum VmDeviceRestore {
    Virtio(vmm::device_state::VirtioDeviceState),
    Block {
        device_id: String,
        state: vmm::device_state::BlockDeviceState,
    },
}

/// Streams a complete memory image into a newly constructed, never-activated VM.
///
/// Implementations may read directly from an archive or object store. They do not need to stage a
/// second complete copy of guest memory before calling [`Vm::enter`].
#[cfg(not(feature = "tee"))]
pub trait VmMemoryRestoreSource: Send {
    /// Materializes every range required by the complete memory generation.
    fn restore(&mut self, target: &mut dyn VmMemoryRestoreTarget) -> io::Result<()>;
}

/// Construction-only target used by [`VmMemoryRestoreSource`].
#[cfg(not(feature = "tee"))]
pub trait VmMemoryRestoreTarget {
    /// Writes exact bytes to one guest-physical range.
    fn write_bytes(
        &mut self,
        range: vmm::memory_state::GuestMemoryRange,
        bytes: &[u8],
    ) -> io::Result<()>;

    /// Writes zeros to one guest-physical range without requiring a source buffer.
    fn write_zero(&mut self, range: vmm::memory_state::GuestMemoryRange) -> io::Result<()>;
}

#[cfg(not(feature = "tee"))]
struct VmmMemoryRestoreTarget<'a> {
    vmm: &'a mut vmm::Vmm,
    expected: Vec<vmm::memory_state::GuestMemoryRange>,
    restored: Vec<vmm::memory_state::GuestMemoryRange>,
    /// Coalesced ranges previously written by this restore source. Zero ranges outside these
    /// remain guaranteed-zero lazy backing, even when the source emits records out of order.
    written: RestoreWrittenRanges,
}

#[cfg(not(feature = "tee"))]
#[derive(Default)]
struct RestoreWrittenRanges(BTreeMap<u64, u64>);

/// Shared VMM registry and notification point for execution-state observers.
#[cfg(not(feature = "tee"))]
struct VmControlRegistry {
    state: std::sync::Mutex<VmControlRegistryState>,
    state_changed: std::sync::Condvar,
}

/// State protected by [`VmControlRegistry::state`].
#[cfg(not(feature = "tee"))]
struct VmControlRegistryState {
    vmm: Option<std::sync::Weak<std::sync::Mutex<vmm::Vmm>>>,
    execution: Option<VmExecutionState>,
    transitioning: bool,
}

/// Cloneable handle for live VM resource control.
///
/// Obtained through [`Vm::control_handle`] before `enter()`; background
/// threads use it to establish generation-scoped pause barriers and drive live resizes while the
/// VM runs. Memory resize is backed by virtio-mem and only available when the machine reserved
/// capacity with [`max_memory_mib`](super::builders::MachineBuilder::max_memory_mib).
#[cfg(not(feature = "tee"))]
#[derive(Clone)]
pub struct VmControl {
    boot_mib: u64,
    mem: Option<Arc<std::sync::Mutex<devices::virtio::Mem>>>,
    cpu: Option<Arc<std::sync::Mutex<devices::virtio::Cpu>>>,
    vmm: Arc<VmControlRegistry>,
    generation: Option<Arc<std::sync::Mutex<devices::virtio::Generation>>>,
}

/// Runtime-local generation of one completed VM-wide pause barrier.
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmPauseGeneration(vmm::PauseGeneration);

#[cfg(not(feature = "tee"))]
impl VmPauseGeneration {
    /// Returns the request id that established this pause boundary.
    pub fn get(self) -> u64 {
        self.0.request_id().get()
    }
}

/// Point-in-time execution state observed through [`VmControl`].
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmExecutionState {
    /// Every vCPU has acknowledged this pause generation.
    Paused(VmPauseGeneration),
    /// Every vCPU resumed from this pause generation.
    Running {
        /// Pause generation from which execution most recently resumed.
        resumed_from: VmPauseGeneration,
    },
    /// A partial or timed-out control barrier makes the execution boundary uncertain.
    Indeterminate,
}

/// Opaque 16-byte value mixed into the guest kernel CRNG after clone or rollback.
#[cfg(not(feature = "tee"))]
pub type VmGenerationId = devices::virtio::GenerationId;

/// One exact VM-generation request installed by the host.
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmGenerationRequest {
    /// Device-local sequence used to reject delayed acknowledgements.
    pub sequence: u64,

    /// Identifier the guest kernel must process for this request.
    pub id: VmGenerationId,
}

/// Point-in-time state of the VM-generation device.
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmGenerationState {
    /// Whether the guest driver completed probing.
    pub driver_ready: bool,

    /// Whether the guest driver rejected the device protocol.
    pub driver_error: bool,

    /// Whether the bound guest kernel can correct wall time before activation acknowledgement.
    pub clock_sync_supported: bool,

    /// Request currently published by the host, if one has been installed.
    pub requested: Option<VmGenerationRequest>,

    /// Request last reported as processed by the guest kernel, if any.
    pub processed: Option<VmGenerationRequest>,
}

/// Result of waiting for one exact VM-generation request.
#[cfg(not(feature = "tee"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmGenerationWaitOutcome {
    /// The guest kernel processed the exact sequence and identifier.
    Processed,

    /// A newer request replaced this request before it was acknowledged.
    Superseded,

    /// The deadline elapsed without an exact acknowledgement.
    TimedOut,

    /// The guest kernel rejected activation without acknowledging it.
    Failed,
}

/// Point-in-time CPU sizing of a running VM as seen through [`VmControl`].
/// `actual` is what the guest driver last reported; `enforced` is what the
/// VMM allows to execute regardless of guest cooperation.
#[cfg(not(feature = "tee"))]
#[derive(Debug, Clone, Copy)]
pub struct VmCpuState {
    /// CPUs possible in this boot.
    pub possible: u32,

    /// Online count the host asked the guest to converge on.
    pub requested_online: u32,

    /// Online count the guest driver last reported.
    pub actual_online: u32,

    /// Online count the VMM currently enforces.
    pub enforced: u32,
}

/// Point-in-time memory sizing of a running VM, in MiB, as seen through
/// [`VmControl`]. `current` trails `target` while the guest converges.
#[cfg(not(feature = "tee"))]
#[derive(Debug, Clone, Copy)]
pub struct VmMemoryState {
    /// Memory the VM booted with.
    pub boot_mib: u64,

    /// Total memory the host asked the guest to converge on.
    pub target_mib: u64,

    /// Total memory currently usable by the guest (boot + plugged).
    pub current_mib: u64,

    /// Boot-time ceiling for live growth (boot + hotplug capacity).
    pub max_mib: u64,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Vm {
    /// Create a new Vm instance.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        vmr: VmResources,
        kernel_cmdline: Option<String>,
        exec_path: Option<String>,
        args: Option<String>,
        env: Option<String>,
        workdir: Option<String>,
        rlimits: Option<String>,
        krunfw_path: Option<PathBuf>,
        initramfs_path: Option<PathBuf>,
        init_path: Option<String>,
        exit_observers: Vec<Box<dyn Fn(i32) + Send + 'static>>,
        placement_observer: Option<PlacementObserver>,
        exit_evt: EventFd,
        exit_code: Arc<AtomicI32>,
        #[cfg(not(target_os = "windows"))] enable_inet_hijack: bool,
        #[cfg(not(target_os = "windows"))] vsock_unix_ipc_port_map: Option<
            HashMap<u32, (PathBuf, bool)>,
        >,
        vsock_custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
        #[cfg(not(target_os = "windows"))] vsock_custom_dgram_port_map: Option<
            HashMap<u32, Arc<dyn VsockDatagramPortBackend>>,
        >,
        #[cfg(not(target_os = "windows"))] vsock_host_port_map: Option<HashMap<u16, u16>>,
    ) -> Self {
        Self {
            vmr,
            kernel_cmdline,
            exec_path,
            args,
            env,
            workdir,
            rlimits,
            krunfw_path,
            initramfs_path,
            init_path,
            exit_observers,
            placement_observer,
            exit_evt,
            exit_code,
            #[cfg(not(target_os = "windows"))]
            enable_inet_hijack,
            #[cfg(not(target_os = "windows"))]
            vsock_unix_ipc_port_map,
            vsock_custom_port_map,
            #[cfg(not(target_os = "windows"))]
            vsock_custom_dgram_port_map,
            #[cfg(not(target_os = "windows"))]
            vsock_host_port_map,
            _krunfw_library: None,
            _initramfs_data: None,
            #[cfg(not(feature = "tee"))]
            vmm_control: Arc::new(VmControlRegistry::new()),
            #[cfg(not(feature = "tee"))]
            execution_restore: None,
            #[cfg(not(feature = "tee"))]
            memory_restore: None,
            #[cfg(all(not(feature = "tee"), feature = "blk"))]
            device_restores: Vec::new(),
            #[cfg(not(feature = "tee"))]
            start_paused: false,
        }
    }

    /// Supplies backend-qualified execution state to restore before the first guest instruction.
    #[cfg(not(feature = "tee"))]
    pub fn set_execution_restore(&mut self, state: vmm::execution_state::ExecutionState) {
        self.execution_restore = Some(state);
    }

    /// Supplies a streaming complete-memory source to run before the first guest instruction.
    #[cfg(not(feature = "tee"))]
    pub fn set_memory_restore<S>(&mut self, source: S)
    where
        S: VmMemoryRestoreSource + 'static,
    {
        self.memory_restore = Some(Box::new(source));
    }

    /// Supply an immutable private file mapping instead of streaming memory into anonymous RAM.
    /// Execution restore must also be supplied. The caller owns file immutability, and unsupported
    /// placement/discard combinations fail before activation rather than selecting eager memory.
    #[cfg(not(feature = "tee"))]
    pub fn set_private_memory_backing(
        &mut self,
        backing: vmm::private_memory::PrivateMemoryBacking,
    ) {
        self.vmr.private_memory_backing = Some(backing);
    }

    /// Use private sparse zero backing for a fresh boot; this does not create a checkpoint.
    #[cfg(not(feature = "tee"))]
    pub fn set_private_memory_boot(&mut self, enabled: bool) {
        self.vmr.private_memory_boot = enabled;
    }

    /// Supplies one destination-recreated virtio device state for construction-only restore.
    ///
    /// Device restores run after memory and execution reconstruction but before the first guest
    /// instruction. The restored device remains quiesced until the VM is activated.
    #[cfg(all(not(feature = "tee"), feature = "blk"))]
    pub fn add_virtio_device_restore(&mut self, state: vmm::device_state::VirtioDeviceState) {
        self.device_restores.push(VmDeviceRestore::Virtio(state));
    }

    /// Supplies one virtio-block state for construction-only restore.
    ///
    /// `device_id` names the already-configured destination block device. Its backend must be
    /// resolved by the caller before [`enter()`](Self::enter); captured host handles are never
    /// reopened from device-state bytes.
    #[cfg(all(not(feature = "tee"), feature = "blk"))]
    pub fn add_block_device_restore(
        &mut self,
        device_id: impl Into<String>,
        state: vmm::device_state::BlockDeviceState,
    ) {
        self.device_restores.push(VmDeviceRestore::Block {
            device_id: device_id.into(),
            state,
        });
    }

    /// Select whether initial activation remains paused after construction and restore.
    ///
    /// The default is `false`, preserving ordinary boot behavior. Restore coordinators set this
    /// to `true`, wait for [`VmControl::wait_until_paused`], complete freshness/ownership gates,
    /// and resume with the returned generation token.
    #[cfg(not(feature = "tee"))]
    pub fn set_start_paused(&mut self, start_paused: bool) {
        self.start_paused = start_paused;
    }

    /// Get a cloneable handle that triggers VM exit from any thread.
    ///
    /// Must be called **before** [`enter()`](Self::enter). Background tasks
    /// use this to shut down the VMM (e.g. idle timeout, max duration).
    pub fn exit_handle(&self) -> ExitHandle {
        ExitHandle::from_event_fd(&self.exit_evt)
            .expect("Failed to create ExitHandle from exit EventFd")
    }

    /// Get a shared reference to the VM exit code.
    ///
    /// The VMM writes the guest exit code here before invoking exit
    /// observers. Read it inside an [`on_exit`](super::builder::VmBuilder::on_exit)
    /// closure to record the exit status.
    ///
    /// Sentinel value `i32::MAX` means "not yet set".
    pub fn exit_code(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.exit_code)
    }

    /// Get a cloneable handle for VM metrics.
    ///
    /// Must be called before [`enter()`](Self::enter) if the caller needs to
    /// sample metrics while the VM is running, because `enter()` never returns
    /// on a successful boot.
    pub fn metrics_handle(&self) -> MetricsHandle {
        self.vmr.metrics.handle()
    }

    /// Get a cloneable handle for execution and resource control.
    ///
    /// Must be called **before** [`enter()`](Self::enter), because `enter()`
    /// never returns on a successful boot. Live memory resize is only
    /// available when the machine reserved capacity with
    /// [`max_memory_mib`](super::builders::MachineBuilder::max_memory_mib)
    /// above the boot memory size.
    #[cfg(not(feature = "tee"))]
    pub fn control_handle(&self) -> VmControl {
        VmControl {
            boot_mib: self.vmr.vm_config().mem_size_mib.unwrap_or(128) as u64,
            mem: self.vmr.mem_device.clone(),
            cpu: self.vmr.cpu_device.clone(),
            vmm: Arc::clone(&self.vmm_control),
            generation: self.vmr.generation_device.clone(),
        }
    }

    /// Start the VM. This call never returns on success — the VMM calls
    /// `_exit()` when the guest shuts down, killing the entire process.
    ///
    /// Only returns `Err` if something fails before the VMM takes over.
    pub fn enter(mut self) -> Result<Infallible> {
        #[cfg(not(feature = "tee"))]
        if self.vmr.private_memory_boot
            && (self.vmr.private_memory_backing.is_some()
                || self.memory_restore.is_some()
                || self.execution_restore.is_some())
        {
            return Err(Error::Build(BuildError::Start(
                "private zero boot excludes restored memory and execution state".into(),
            )));
        }
        #[cfg(not(feature = "tee"))]
        if self.vmr.private_memory_backing.is_some()
            && (self.memory_restore.is_some() || self.execution_restore.is_none())
        {
            return Err(Error::Build(BuildError::Start(
                "private memory backing requires execution restore and excludes an eager memory source".into(),
            )));
        }
        let mut trace = BootTrace::new("api");
        trace.mark("enter.start");

        // Set process name on Linux
        #[cfg(target_os = "linux")]
        {
            let prname = match env::var("HOSTNAME") {
                Ok(val) => CString::new(format!("VM:{val}")).unwrap_or_default(),
                Err(_) => CString::new("libkrun VM").unwrap_or_default(),
            };
            unsafe { libc::prctl(libc::PR_SET_NAME, prname.as_ptr()) };
        }

        // Create event manager
        let mut event_manager = EventManager::new()
            .map_err(|e| Error::Build(BuildError::Start(format!("EventManager: {e:?}"))))?;
        trace.mark("event_manager.ready");

        // Load kernel from libkrunfw if not already configured
        if self.vmr.external_kernel.is_none()
            && self.vmr.kernel_bundle.is_none()
            && self.vmr.firmware_config.is_none()
            && cfg!(not(feature = "efi"))
        {
            self.load_krunfw()?;
        }
        trace.mark("kernel.ready");

        // Capture boot start timestamp (epoch nanoseconds) for guest-side timing.
        let boot_start_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // Build kernel command line
        let kernel_cmdline = self.build_kernel_cmdline(boot_start_ns);

        self.vmr
            .set_kernel_cmdline(kernel_cmdline)
            .map_err(|e| Error::Build(BuildError::Start(format!("kernel cmdline: {e:?}"))))?;
        trace.mark("kernel_cmdline.ready");

        // Configure vsock
        self.configure_vsock()?;
        trace.mark("vsock.configured");

        // Create shutdown EventFd on macOS aarch64 (needed for GPIO shutdown device)
        let shutdown_efd = if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
            Some(
                EventFd::new(utils::eventfd::EFD_NONBLOCK)
                    .map_err(|e| Error::Build(BuildError::Start(format!("shutdown_efd: {e:?}"))))?,
            )
        } else {
            None
        };

        // Build the microVM
        let (sender, _receiver) = unbounded();

        #[cfg(not(feature = "tee"))]
        {
            self.vmr.zeroed_restore_memory = self.memory_restore.is_some();
        }

        let (_vmm, placement_report) = vmm::builder::build_microvm_paused(
            &mut self.vmr,
            &mut event_manager,
            shutdown_efd,
            sender,
            self.exit_evt,
            self.exit_code,
        )
        .map_err(|e| Error::Build(BuildError::Start(format!("build_microvm: {e:?}"))))?;
        trace.mark("build_microvm.ready");

        #[cfg(not(feature = "tee"))]
        {
            let mut vmm = _vmm.lock().expect("Poisoned VMM mutex");
            if let Some(mut source) = self.memory_restore.take() {
                let mut target = VmmMemoryRestoreTarget::new(&mut vmm);
                source.restore(&mut target).map_err(|error| {
                    Error::Build(BuildError::Start(format!("restore memory: {error}")))
                })?;
                target.finish().map_err(|error| {
                    Error::Build(BuildError::Start(format!("restore memory: {error}")))
                })?;
            }
            if let Some(state) = self.execution_restore.take() {
                vmm.restore_execution_state(&state).map_err(|error| {
                    Error::Build(BuildError::Start(format!(
                        "restore execution state: {error}"
                    )))
                })?;
            }
            #[cfg(feature = "blk")]
            for restore in self.device_restores.drain(..) {
                match restore {
                    VmDeviceRestore::Virtio(state) => {
                        vmm.restore_virtio_device_state(&state).map_err(|error| {
                            Error::Build(BuildError::Start(format!(
                                "restore virtio device {}: {error}",
                                state.device_id
                            )))
                        })?
                    }
                    VmDeviceRestore::Block { device_id, state } => vmm
                        .restore_block_device_state(&device_id, &state)
                        .map_err(|error| {
                            Error::Build(BuildError::Start(format!(
                                "restore block device {device_id}: {error}"
                            )))
                        })?,
                }
            }
        }
        trace.mark("restore.ready");

        if let Some(observer) = self.placement_observer.take() {
            observer(&placement_report);
        }
        trace.mark("placement.reconciled");

        #[cfg(not(feature = "tee"))]
        let initial_execution_state = {
            let mut vmm = _vmm.lock().expect("Poisoned VMM mutex");
            if !self.start_paused {
                vmm.resume_vcpus()
                    .map_err(|e| Error::Build(BuildError::Start(format!("resume_vcpus: {e:?}"))))?;
            }
            public_execution_state(vmm.execution_state())
        };
        #[cfg(feature = "tee")]
        _vmm.lock()
            .expect("Poisoned VMM mutex")
            .resume_vcpus()
            .map_err(|e| Error::Build(BuildError::Start(format!("resume_vcpus: {e:?}"))))?;
        #[cfg(not(feature = "tee"))]
        trace.mark(if self.start_paused {
            "vcpus.activation_gated"
        } else {
            "vcpus.resumed"
        });
        #[cfg(feature = "tee")]
        trace.mark("vcpus.resumed");

        #[cfg(not(feature = "tee"))]
        {
            self.vmm_control.publish_vmm(&_vmm, initial_execution_state);
        }

        // Register user exit observers
        {
            let mut vmm = _vmm.lock().expect("Poisoned VMM mutex");
            for observer in self.exit_observers {
                vmm.add_exit_observer(observer);
            }
        }
        trace.mark("observers.ready");

        // Start worker threads if needed
        #[cfg(target_os = "macos")]
        if self.vmr.gpu_virgl_flags.is_some() {
            vmm::worker::start_worker_thread(_vmm.clone(), _receiver)
                .map_err(|e| Error::Runtime(RuntimeError::EventLoop(format!("{e:?}"))))?;
        }

        #[cfg(target_arch = "x86_64")]
        if self.vmr.split_irqchip {
            vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone())
                .map_err(|e| Error::Runtime(RuntimeError::EventLoop(format!("{e:?}"))))?;
        }

        #[cfg(all(not(feature = "tee"), target_os = "windows"))]
        if self.vmr.fs.iter().any(|fs| fs.shm_size.is_some()) {
            vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone())
                .map_err(|e| Error::Runtime(RuntimeError::EventLoop(format!("{e:?}"))))?;
        }

        #[cfg(any(feature = "amd-sev", feature = "tdx"))]
        vmm::worker::start_worker_thread(_vmm.clone(), _receiver.clone())
            .map_err(|e| Error::Runtime(RuntimeError::EventLoop(format!("{e:?}"))))?;
        trace.mark("event_loop.start");

        // Run the event loop. On normal guest exit, the VMM calls _exit() directly.
        loop {
            match event_manager.run() {
                Ok(_) => {}
                Err(e) => {
                    error!("Error in EventManager loop: {e:?}");
                    // Run exit observers before returning so cleanup (terminal
                    // restore, console reset, user callbacks) still fires.
                    _vmm.lock()
                        .expect("Poisoned VMM mutex")
                        .notify_exit_observers(1);
                    return Err(Error::Runtime(RuntimeError::EventLoop(format!("{e:?}"))));
                }
            }
        }
    }

    /// Load kernel from libkrunfw.
    fn load_krunfw(&mut self) -> Result<()> {
        let krunfw = load_krunfw_library(self.krunfw_path.as_deref())?;

        // Get kernel from libkrunfw
        let mut kernel_guest_addr: u64 = 0;
        let mut kernel_entry_addr: u64 = 0;
        let mut kernel_size: usize = 0;

        let kernel_host_addr = match &krunfw {
            KrunfwBindings::Embedded(kernel) => {
                kernel_guest_addr = kernel.guest_address;
                kernel_entry_addr = kernel.entry_address;
                kernel_size = kernel.bytes.len();
                kernel.bytes.as_ptr() as *mut std::ffi::c_char
            }
            KrunfwBindings::Dynamic { get_kernel, .. } => unsafe {
                get_kernel(
                    &mut kernel_guest_addr as *mut u64,
                    &mut kernel_entry_addr as *mut u64,
                    &mut kernel_size as *mut usize,
                )
            },
        };

        let kernel_bundle = KernelBundle {
            host_addr: kernel_host_addr as u64,
            guest_addr: kernel_guest_addr,
            entry_addr: kernel_entry_addr,
            size: kernel_size,
        };

        self.vmr
            .set_kernel_bundle(kernel_bundle)
            .map_err(|e| Error::Build(BuildError::Krunfw(format!("{e:?}"))))?;

        if let Some(initramfs_path) = &self.initramfs_path {
            let initramfs_data = std::fs::read(initramfs_path)?;
            let initrd_bundle = InitrdBundle {
                host_addr: initramfs_data.as_ptr() as u64,
                size: initramfs_data.len(),
            };

            self.vmr
                .set_initrd_bundle(initrd_bundle)
                .map_err(|e| Error::Build(BuildError::Krunfw(format!("{e:?}"))))?;
            self._initramfs_data = Some(initramfs_data);
        }

        // Keep the library alive so the kernel memory pointers remain valid.
        if let KrunfwBindings::Dynamic { library, .. } = krunfw {
            self._krunfw_library = Some(library);
        }

        Ok(())
    }

    /// Configure the vsock device.
    ///
    /// The device is only attached when a typed host route exists or the
    /// caller enabled TSI as a transport. This keeps the per-VM IRQ/MMIO
    /// budget free when nothing uses vsock.
    #[cfg(not(target_os = "windows"))]
    fn configure_vsock(&mut self) -> Result<()> {
        let tsi_flags = self.compute_tsi_flags();

        if self.vsock_unix_ipc_port_map.is_none()
            && self.vsock_custom_port_map.is_none()
            && self.vsock_custom_dgram_port_map.is_none()
            && tsi_flags.is_empty()
        {
            return Ok(());
        }

        let vsock_config = VsockDeviceConfig {
            vsock_id: "vsock0".to_string(),
            guest_cid: 3,
            host_port_map: self.vsock_host_port_map.take(),
            unix_ipc_port_map: self.vsock_unix_ipc_port_map.take(),
            custom_port_map: self.vsock_custom_port_map.take(),
            custom_dgram_port_map: self.vsock_custom_dgram_port_map.take(),
            tsi_flags,
        };

        self.vmr
            .set_vsock_device(vsock_config)
            .map_err(|e| Error::Build(BuildError::DeviceRegistration(format!("vsock: {e:?}"))))?;

        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn configure_vsock(&mut self) -> Result<()> {
        if self.vsock_custom_port_map.is_none() {
            return Ok(());
        }

        self.vmr
            .set_vsock_device(VsockDeviceConfig {
                vsock_id: "vsock0".to_string(),
                guest_cid: 3,
                host_port_map: None,
                unix_ipc_port_map: None,
                custom_port_map: self.vsock_custom_port_map.take(),
                tsi_flags: vmm::resources::TsiFlags::empty(),
            })
            .map_err(|err| {
                Error::Build(BuildError::DeviceRegistration(format!("vsock: {err:?}")))
            })?;
        Ok(())
    }

    /// Decide which `TsiFlags` should be enabled for this VM.
    ///
    /// Extracted from [`configure_vsock`](Self::configure_vsock) so the
    /// flag-selection logic can be exercised by unit tests without
    /// touching `VmResources::set_vsock_device`.
    #[cfg(not(target_os = "windows"))]
    fn compute_tsi_flags(&self) -> TsiFlags {
        let mut tsi_flags = TsiFlags::empty();

        // Enable TSI INET hijack as a fallback when no virtio-net is
        // configured and the caller opted in via
        // `MachineBuilder::enable_inet_hijack(true)`. Default is air-gap.
        #[cfg(feature = "net")]
        if self.enable_inet_hijack && self.vmr.net.list.is_empty() {
            tsi_flags |= TsiFlags::HIJACK_INET;
        }

        #[cfg(not(feature = "net"))]
        if self.enable_inet_hijack {
            tsi_flags |= TsiFlags::HIJACK_INET;
        }

        // Enable TSI for AF_UNIX if single root virtio-fs
        #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
        {
            tsi_flags = self.maybe_enable_hijack_unix(tsi_flags);
        }

        tsi_flags
    }

    fn get_exec_path(&self) -> String {
        self.exec_path
            .as_ref()
            .map(|p| format!("KRUN_INIT={p}"))
            .unwrap_or_default()
    }

    fn get_workdir(&self) -> String {
        self.workdir
            .as_ref()
            .map(|p| format!("KRUN_WORKDIR={p}"))
            .unwrap_or_default()
    }

    fn get_rlimits(&self) -> String {
        self.rlimits
            .as_ref()
            .map(|r| format!("KRUN_RLIMITS={r}"))
            .unwrap_or_default()
    }

    fn get_env(&self) -> String {
        self.env
            .as_ref()
            .map(|e| format!("KRUN_ENV={e}"))
            .unwrap_or_default()
    }

    fn get_args(&self) -> String {
        self.args.clone().unwrap_or_default()
    }

    fn build_kernel_cmdline(&self, boot_start_ns: u64) -> KernelCmdlineConfig {
        let init = self.init_path.as_deref().unwrap_or(INIT_PATH);
        let user_cmdline = self
            .kernel_cmdline
            .as_deref()
            .map(|cmdline| format!(" {cmdline}"))
            .unwrap_or_default();
        // Escape hatch for boot debugging: appended last so it can override earlier parameters (e.g. `ignore_loglevel`, `maxcpus=1`) without any API plumbing.
        let debug_cmdline = std::env::var("MSB_KRUN_KERNEL_CMDLINE")
            .map(|extra| format!(" {extra}"))
            .unwrap_or_default();

        KernelCmdlineConfig {
            prolog: Some(format!(
                "{}{}{debug_cmdline} root=/dev/root init={init}",
                vmm::vmm_config::kernel_cmdline::DEFAULT_KERNEL_CMDLINE,
                user_cmdline,
            )),
            krun_env: Some(format!(
                " {} {} {} {} KRUN_BOOT_START_NS={boot_start_ns}",
                self.get_exec_path(),
                self.get_workdir(),
                self.get_rlimits(),
                self.get_env(),
            )),
            epilog: Some(format!(" -- {}", self.get_args())),
        }
    }

    #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
    fn maybe_enable_hijack_unix(&self, mut tsi_flags: TsiFlags) -> TsiFlags {
        if cfg!(target_os = "macos") {
            return tsi_flags;
        }

        if tsi_flags.contains(TsiFlags::HIJACK_INET)
            && self.vmr.fs.len() == 1
            && self.vmr.fs[0].fs_id == "/dev/root"
        {
            tsi_flags |= TsiFlags::HIJACK_UNIX;
        }

        tsi_flags
    }
}

#[cfg(not(feature = "tee"))]
impl VmControlRegistry {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(VmControlRegistryState {
                vmm: None,
                execution: None,
                transitioning: false,
            }),
            state_changed: std::sync::Condvar::new(),
        }
    }

    /// Publishes the control handle and its already-established initial execution boundary atomically.
    fn publish_vmm(&self, vmm: &Arc<std::sync::Mutex<vmm::Vmm>>, execution: VmExecutionState) {
        let mut state = self.state.lock().expect("Poisoned VMM control registry");
        state.vmm = Some(Arc::downgrade(vmm));
        state.execution = Some(execution);
        state.transitioning = false;
        self.state_changed.notify_all();
    }

    /// Makes an in-flight barrier externally conservative until its final state is known.
    fn publish_transition_started(&self) {
        let mut state = self.state.lock().expect("Poisoned VMM control registry");
        state.execution = Some(VmExecutionState::Indeterminate);
        state.transitioning = true;
        self.state_changed.notify_all();
    }

    /// Mirrors a completed or indeterminate execution transition before notifying observers.
    fn publish_execution_state(&self, execution: VmExecutionState) {
        let mut state = self.state.lock().expect("Poisoned VMM control registry");
        state.execution = Some(execution);
        state.transitioning = false;
        self.state_changed.notify_all();
    }

    fn execution_state(&self) -> Option<VmExecutionState> {
        let state = self.state.lock().ok()?;
        state.vmm.as_ref()?.upgrade()?;
        state.execution
    }

    fn running_vmm(&self) -> Result<Arc<std::sync::Mutex<vmm::Vmm>>> {
        self.state
            .lock()
            .map_err(|_| {
                Error::Runtime(RuntimeError::Control(
                    "VMM control registry is poisoned".to_string(),
                ))
            })?
            .vmm
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(Error::Runtime(RuntimeError::NotStarted))
    }

    fn wait_until_running(&self, timeout: Duration) -> Result<VmExecutionState> {
        let started = Instant::now();
        let mut state = self.state.lock().map_err(|_| {
            Error::Runtime(RuntimeError::Control(
                "VMM control registry is poisoned".to_string(),
            ))
        })?;

        loop {
            match state.execution {
                Some(running @ VmExecutionState::Running { .. }) => return Ok(running),
                Some(VmExecutionState::Indeterminate) if !state.transitioning => {
                    return Err(Error::Runtime(RuntimeError::Control(
                        "VM execution state became indeterminate while waiting for Running"
                            .to_string(),
                    )));
                }
                Some(VmExecutionState::Paused(_))
                | Some(VmExecutionState::Indeterminate)
                | None => {}
            }

            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(wait_until_running_timeout(timeout, state.execution));
            };
            let (next_state, wait) =
                self.state_changed
                    .wait_timeout(state, remaining)
                    .map_err(|_| {
                        Error::Runtime(RuntimeError::Control(
                            "VMM control registry is poisoned".to_string(),
                        ))
                    })?;
            state = next_state;

            // Check the predicate once more at the deadline so a simultaneous notification wins.
            if wait.timed_out()
                && !matches!(state.execution, Some(VmExecutionState::Running { .. }))
            {
                return Err(wait_until_running_timeout(timeout, state.execution));
            }
        }
    }

    fn wait_until_paused(&self, timeout: Duration) -> Result<VmExecutionState> {
        self.wait_until_paused_inner(Some(timeout))
    }

    fn wait_until_paused_inner(&self, timeout: Option<Duration>) -> Result<VmExecutionState> {
        let started = Instant::now();
        let mut state = self.state.lock().map_err(|_| {
            Error::Runtime(RuntimeError::Control(
                "VMM control registry is poisoned".to_string(),
            ))
        })?;

        loop {
            match state.execution {
                Some(paused @ VmExecutionState::Paused(_)) => return Ok(paused),
                Some(VmExecutionState::Indeterminate) if !state.transitioning => {
                    return Err(Error::Runtime(RuntimeError::Control(
                        "VM execution state became indeterminate while waiting for Paused"
                            .to_string(),
                    )));
                }
                Some(VmExecutionState::Running { .. })
                | Some(VmExecutionState::Indeterminate)
                | None => {}
            }

            let Some(timeout) = timeout else {
                // Construction may be dominated by slow storage. A separate owner cancels
                // construction; it must not spend the later execution-barrier deadline here.
                state = self.state_changed.wait(state).map_err(|_| {
                    Error::Runtime(RuntimeError::Control(
                        "VMM control registry is poisoned".to_string(),
                    ))
                })?;
                continue;
            };
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(wait_until_paused_timeout(timeout, state.execution));
            };
            let (next_state, wait) =
                self.state_changed
                    .wait_timeout(state, remaining)
                    .map_err(|_| {
                        Error::Runtime(RuntimeError::Control(
                            "VMM control registry is poisoned".to_string(),
                        ))
                    })?;
            state = next_state;

            if wait.timed_out() && !matches!(state.execution, Some(VmExecutionState::Paused(_))) {
                return Err(wait_until_paused_timeout(timeout, state.execution));
            }
        }
    }
}

struct BootTrace {
    enabled: bool,
    scope: &'static str,
    start: Instant,
    last: Instant,
}

impl BootTrace {
    fn new(scope: &'static str) -> Self {
        let now = Instant::now();
        Self {
            enabled: std::env::var_os("MSB_KRUN_BOOT_TRACE").is_some(),
            scope,
            start: now,
            last: now,
        }
    }

    fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }

        let now = Instant::now();
        eprintln!(
            "krun.boot scope={} label={} elapsed_us={} delta_us={}",
            self.scope,
            label,
            now.duration_since(self.start).as_micros(),
            now.duration_since(self.last).as_micros(),
        );
        self.last = now;
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

#[cfg(not(feature = "tee"))]
fn public_execution_state(state: vmm::VmmExecutionState) -> VmExecutionState {
    match state {
        vmm::VmmExecutionState::Paused(generation) => {
            VmExecutionState::Paused(VmPauseGeneration(generation))
        }
        vmm::VmmExecutionState::Running { resumed_from } => VmExecutionState::Running {
            resumed_from: VmPauseGeneration(resumed_from),
        },
        vmm::VmmExecutionState::Indeterminate => VmExecutionState::Indeterminate,
    }
}

#[cfg(not(feature = "tee"))]
fn wait_until_running_timeout(timeout: Duration, state: Option<VmExecutionState>) -> Error {
    Error::Runtime(RuntimeError::Control(format!(
        "timed out after {timeout:?} waiting for VM to reach Running; last state: {state:?}"
    )))
}

#[cfg(not(feature = "tee"))]
fn wait_until_paused_timeout(timeout: Duration, state: Option<VmExecutionState>) -> Error {
    Error::Runtime(RuntimeError::Control(format!(
        "timed out after {timeout:?} waiting for VM to reach Paused; last state: {state:?}"
    )))
}

/// Bindings to libkrunfw functions.
enum KrunfwBindings {
    Embedded(EmbeddedKernel),
    Dynamic {
        get_kernel: unsafe extern "C" fn(*mut u64, *mut u64, *mut usize) -> *mut std::ffi::c_char,
        library: libloading::Library,
    },
}

/// Library name for libkrunfw.
#[cfg(target_os = "linux")]
const KRUNFW_NAME: &str = "libkrunfw.so.5";
#[cfg(target_os = "macos")]
const KRUNFW_NAME: &str = "libkrunfw.5.dylib";
#[cfg(target_os = "windows")]
const KRUNFW_NAME: &str = "libkrunfw.dll";

/// Load the libkrunfw library.
///
/// If `path` is provided, loads from that exact path. Otherwise falls back to the
/// default library name, which lets the OS dynamic linker search standard paths.
fn load_krunfw_library(path: Option<&std::path::Path>) -> Result<KrunfwBindings> {
    // The SDK currently requires a real firmware path in its launch contract.
    // The embedding executable is that path until WI 0120 provides a typed SDK origin.
    let is_self = path.and_then(|p| p.canonicalize().ok())
        .zip(std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()))
        .is_some_and(|(requested, executable)| requested == executable);
    if is_self || path == Some(std::path::Path::new("@embedded-kernel")) {
        return embedded_kernel().map(KrunfwBindings::Embedded).ok_or_else(|| {
            Error::Build(BuildError::Krunfw("embedded kernel not registered".into()))
        });
    }
    let name = path
        .map(|p| p.as_os_str().to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from(KRUNFW_NAME));
    let library = unsafe { libloading::Library::new(&name) }.map_err(|e| {
        Error::Build(BuildError::Krunfw(format!(
            "load {}: {e}",
            name.to_string_lossy()
        )))
    })?;

    let get_kernel = unsafe {
        *library
            .get::<unsafe extern "C" fn(*mut u64, *mut u64, *mut usize) -> *mut std::ffi::c_char>(
                b"krunfw_get_kernel\0",
            )
            .map_err(|e| Error::Build(BuildError::Krunfw(format!("krunfw_get_kernel: {e}"))))?
    };

    Ok(KrunfwBindings::Dynamic {
        get_kernel,
        library,
    })
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_os = "windows"))]
    use crate::VmBuilder;
    use utils::eventfd::EFD_NONBLOCK;
    #[cfg(not(target_os = "windows"))]
    use vmm::resources::TsiFlags;
    #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
    use vmm::vmm_config::fs::FsDeviceConfig;

    fn make_vm() -> Vm {
        Vm::new(
            VmResources::default(),
            Some("debug loglevel=7".to_string()),
            None,
            Some("\"--flag\"".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            EventFd::new(EFD_NONBLOCK).unwrap(),
            Arc::new(AtomicI32::new(i32::MAX)),
            #[cfg(not(target_os = "windows"))]
            false,
            #[cfg(not(target_os = "windows"))]
            None,
            None,
            #[cfg(not(target_os = "windows"))]
            None,
            #[cfg(not(target_os = "windows"))]
            None,
        )
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn execution_control_is_unavailable_before_vmm_startup() {
        let control = make_vm().control_handle();

        assert_eq!(control.execution_state(), None);
        assert!(matches!(
            control.pause(),
            Err(Error::Runtime(RuntimeError::NotStarted))
        ));
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn waiting_for_running_times_out_before_vmm_startup() {
        let control = make_vm().control_handle();

        let error = control
            .wait_until_running(Duration::from_millis(1))
            .unwrap_err();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::Control(message))
                if message.contains("timed out") && message.contains("last state: None")
        ));
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn waiting_for_paused_times_out_before_vmm_startup() {
        let control = make_vm().control_handle();

        let error = control
            .wait_until_paused(Duration::from_millis(1))
            .unwrap_err();

        assert!(matches!(
            error,
            Error::Runtime(RuntimeError::Control(message))
                if message.contains("timed out") && message.contains("last state: None")
        ));
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn construction_wait_wakes_on_an_indeterminate_boundary() {
        let control = make_vm().control_handle();
        let waiting = control.clone();
        let (finished, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished
                .send(waiting.wait_until_paused_without_timeout())
                .unwrap();
        });
        assert!(matches!(
            result.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        control
            .vmm
            .publish_execution_state(VmExecutionState::Indeterminate);
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(Error::Runtime(RuntimeError::Control(message))) if message.contains("indeterminate")
        ));
        worker.join().unwrap();
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn construction_wait_refuses_an_already_indeterminate_boundary() {
        let control = make_vm().control_handle();
        control
            .vmm
            .publish_execution_state(VmExecutionState::Indeterminate);
        assert!(control.wait_until_paused_without_timeout().is_err());
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn initial_pause_gate_is_explicit_and_disabled_by_default() {
        let mut vm = make_vm();
        assert!(!vm.start_paused);

        vm.set_start_paused(true);

        assert!(vm.start_paused);
    }

    #[cfg(not(target_os = "windows"))]
    fn make_vm_with(enable_inet_hijack: bool) -> Vm {
        Vm::new(
            VmResources::default(),
            Some("debug loglevel=7".to_string()),
            None,
            Some("\"--flag\"".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            EventFd::new(EFD_NONBLOCK).unwrap(),
            Arc::new(AtomicI32::new(i32::MAX)),
            enable_inet_hijack,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn build_kernel_cmdline_keeps_user_cmdline() {
        let vm = make_vm();
        let cmdline = vm.build_kernel_cmdline(42);

        let prolog = cmdline.prolog.expect("missing prolog");
        assert!(prolog.contains("debug loglevel=7"));
        assert!(prolog.contains("init=/init.krun"));
    }

    #[cfg(all(not(feature = "tee"), not(target_os = "windows")))]
    #[test]
    fn maybe_enable_hijack_unix_respects_platform_support() {
        let mut vm = make_vm();
        vm.vmr.fs.push(FsDeviceConfig {
            fs_id: "/dev/root".to_string(),
            shared_dir: "/tmp/rootfs".to_string(),
            shm_size: None,
            allow_root_dir_delete: false,
        });

        let flags = vm.maybe_enable_hijack_unix(TsiFlags::HIJACK_INET);

        #[cfg(target_os = "macos")]
        assert!(!flags.contains(TsiFlags::HIJACK_UNIX));

        #[cfg(not(target_os = "macos"))]
        assert!(flags.contains(TsiFlags::HIJACK_UNIX));
    }

    #[cfg(all(
        not(feature = "tee"),
        not(target_os = "macos"),
        not(target_os = "windows")
    ))]
    #[test]
    fn maybe_enable_hijack_unix_requires_root_fs_id() {
        let mut vm = make_vm();
        vm.vmr.fs.push(FsDeviceConfig {
            fs_id: "data".to_string(),
            shared_dir: "/".to_string(),
            shm_size: None,
            allow_root_dir_delete: false,
        });

        let flags = vm.maybe_enable_hijack_unix(TsiFlags::HIJACK_INET);

        assert!(!flags.contains(TsiFlags::HIJACK_UNIX));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn compute_tsi_flags_air_gaps_by_default_with_no_net() {
        let vm = make_vm();
        let flags = vm.compute_tsi_flags();
        assert!(!flags.contains(TsiFlags::HIJACK_INET));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn compute_tsi_flags_enables_inet_hijack_when_opted_in() {
        let vm = make_vm_with(true);
        let flags = vm.compute_tsi_flags();
        assert!(flags.contains(TsiFlags::HIJACK_INET));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn typed_vsock_routes_imply_attachment_and_reach_vm_config() {
        use std::io;

        use devices::virtio::vsock::{
            VsockConnectRequest, VsockNotifier, VsockPortBackend, VsockStreamBackend,
        };

        struct RejectService;

        impl VsockPortBackend for RejectService {
            fn connect(
                &self,
                _request: VsockConnectRequest,
                _notifier: VsockNotifier,
            ) -> io::Result<Box<dyn VsockStreamBackend>> {
                Err(io::Error::from(io::ErrorKind::ConnectionRefused))
            }
        }

        let vm = VmBuilder::new()
            .vsock(|vsock| {
                vsock
                    .unix_connect(5000, "/tmp/supervisor.sock")
                    .unix_listen(5001, "/tmp/events.sock")
                    .custom(6000, Arc::new(RejectService))
                    .inet_hijack(true)
                    .tcp_listen_remap(8080, 18080)
            })
            .build()
            .expect("typed vsock configuration should build");

        assert_eq!(
            vm.vsock_unix_ipc_port_map
                .as_ref()
                .and_then(|routes| routes.get(&5000)),
            Some(&(PathBuf::from("/tmp/supervisor.sock"), false))
        );
        assert_eq!(
            vm.vsock_unix_ipc_port_map
                .as_ref()
                .and_then(|routes| routes.get(&5001)),
            Some(&(PathBuf::from("/tmp/events.sock"), true))
        );
        assert!(vm
            .vsock_custom_port_map
            .as_ref()
            .is_some_and(|routes| routes.contains_key(&6000)));
        assert_eq!(
            vm.vsock_host_port_map
                .as_ref()
                .and_then(|routes| routes.get(&8080)),
            Some(&18080)
        );
        assert!(vm.compute_tsi_flags().contains(TsiFlags::HIJACK_INET));
    }

    #[cfg(all(
        not(feature = "tee"),
        not(target_os = "macos"),
        not(target_os = "windows")
    ))]
    #[test]
    fn compute_tsi_flags_unix_hijack_follows_inet_hijack() {
        let mut vm = make_vm();
        vm.vmr.fs.push(FsDeviceConfig {
            fs_id: "/dev/root".to_string(),
            shared_dir: "/tmp/rootfs".to_string(),
            shm_size: None,
            allow_root_dir_delete: false,
        });

        let flags = vm.compute_tsi_flags();

        // `maybe_enable_hijack_unix` gates UNIX hijack on INET hijack
        // already being set, so the default (no opt-in) drops both.
        assert!(!flags.contains(TsiFlags::HIJACK_INET));
        assert!(!flags.contains(TsiFlags::HIJACK_UNIX));
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn restore_coverage_coalesces_chunks_and_overlays() {
        let range =
            |start, length| vmm::memory_state::GuestMemoryRange::new(start, length).unwrap();
        assert_eq!(
            merge_restore_ranges(vec![
                range(0x2000, 0x1000),
                range(0x1000, 0x1000),
                range(0x1800, 0x1000),
                range(0x5000, 0x1000),
            ]),
            vec![range(0x1000, 0x2000), range(0x5000, 0x1000)]
        );
    }

    #[cfg(not(feature = "tee"))]
    #[test]
    fn fresh_restore_zeroes_only_intersect_previously_written_bytes() {
        let range =
            |start, length| vmm::memory_state::GuestMemoryRange::new(start, length).unwrap();
        let mut writes = RestoreWrittenRanges::default();
        assert!(writes.intersections(range(0, 32 << 30)).is_empty());
        for (start, length) in [(100, 20), (20, 30), (45, 10), (80, 20), (120, 10)] {
            writes.insert(range(start, length));
        }
        assert_eq!(
            writes.intersections(range(0, 256)),
            vec![range(20, 35), range(80, 50)]
        );
        assert_eq!(
            writes.intersections(range(40, 60)),
            vec![range(40, 15), range(80, 20)]
        );
        assert!(writes.intersections(range(55, 25)).is_empty());
        writes.insert(range(0, 256));
        assert_eq!(writes.0.len(), 1);
        assert_eq!(writes.intersections(range(40, 60)), vec![range(40, 60)]);
    }
}

#[cfg(not(feature = "tee"))]
impl VmMemoryRestoreTarget for VmmMemoryRestoreTarget<'_> {
    fn write_bytes(
        &mut self,
        range: vmm::memory_state::GuestMemoryRange,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.vmm
            .materialize_memory(range, bytes)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.written.insert(range);
        self.restored.push(range);
        Ok(())
    }

    fn write_zero(&mut self, range: vmm::memory_state::GuestMemoryRange) -> io::Result<()> {
        let length = usize::try_from(range.length()).map_err(io::Error::other)?;
        if !self
            .vmm
            .guest_memory()
            .check_range(vm_memory::GuestAddress(range.start()), length)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero restore range is outside guest RAM",
            ));
        }
        // Fresh mappings are known-zero, not merely nonresident. Overlays that replace bytes
        // previously written by this source still require physical zeroing of those bytes.
        for overlap in self.written.intersections(range) {
            self.vmm
                .materialize_zero_memory(overlap)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        self.restored.push(range);
        Ok(())
    }
}

#[cfg(not(feature = "tee"))]
impl<'a> VmmMemoryRestoreTarget<'a> {
    fn new(vmm: &'a mut vmm::Vmm) -> Self {
        let expected = vmm
            .guest_memory()
            .iter()
            .map(|region| {
                vmm::memory_state::GuestMemoryRange::new(
                    region.start_addr().raw_value(),
                    region.len(),
                )
                .expect("constructed guest-memory regions are non-empty and bounded")
            })
            .collect::<Vec<_>>();
        let expected = merge_restore_ranges(expected);
        Self {
            vmm,
            expected,
            restored: Vec::new(),
            written: RestoreWrittenRanges::default(),
        }
    }

    fn finish(self) -> io::Result<()> {
        if merge_restore_ranges(self.restored) != self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "restore source did not materialize the complete guest-memory topology",
            ));
        }
        Ok(())
    }
}

#[cfg(not(feature = "tee"))]
impl RestoreWrittenRanges {
    fn insert(&mut self, range: vmm::memory_state::GuestMemoryRange) {
        let mut start = range.start();
        let mut end = start + range.length();
        if let Some((&previous, &previous_end)) = self.0.range(..=start).next_back() {
            if previous_end >= start {
                start = previous;
                end = end.max(previous_end);
                self.0.remove(&previous);
            }
        }
        while let Some((&next, &next_end)) = self.0.range(start..=end).next() {
            end = end.max(next_end);
            self.0.remove(&next);
        }
        self.0.insert(start, end);
    }

    fn intersections(
        &self,
        range: vmm::memory_state::GuestMemoryRange,
    ) -> Vec<vmm::memory_state::GuestMemoryRange> {
        let start = range.start();
        let end = start + range.length();
        let first = self
            .0
            .range(..=start)
            .next_back()
            .map_or(start, |(&key, _)| key);
        self.0
            .range(first..end)
            .filter_map(|(&written_start, &written_end)| {
                let overlap_start = start.max(written_start);
                let overlap_end = end.min(written_end);
                (overlap_start < overlap_end).then(|| {
                    vmm::memory_state::GuestMemoryRange::new(
                        overlap_start,
                        overlap_end - overlap_start,
                    )
                    .unwrap()
                })
            })
            .collect()
    }
}

#[cfg(not(feature = "tee"))]
fn merge_restore_ranges(
    mut ranges: Vec<vmm::memory_state::GuestMemoryRange>,
) -> Vec<vmm::memory_state::GuestMemoryRange> {
    ranges.sort_unstable_by_key(|range| range.start());
    let mut merged: Vec<vmm::memory_state::GuestMemoryRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start() <= previous.end() {
                *previous = vmm::memory_state::GuestMemoryRange::new(
                    previous.start(),
                    previous.end().max(range.end()) - previous.start(),
                )
                .expect("merged restore ranges remain non-empty and bounded");
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

#[cfg(not(feature = "tee"))]
impl VmControl {
    /// Pauses every vCPU at one generation-correlated execution boundary.
    pub fn pause(&self) -> Result<VmPauseGeneration> {
        let vmm = self.running_vmm()?;
        let result = {
            let mut vmm = vmm.lock().map_err(|_| {
                Error::Runtime(RuntimeError::Control("VMM mutex is poisoned".to_string()))
            })?;
            if matches!(
                vmm.execution_state(),
                vmm::VmmExecutionState::Running { .. }
            ) {
                self.vmm.publish_transition_started();
            }
            let result = vmm.pause_vcpus();
            let execution = public_execution_state(vmm.execution_state());
            // Publish before releasing the VMM mutex so concurrent barriers cannot reorder states.
            self.vmm.publish_execution_state(execution);
            result
        };
        result
            .map(VmPauseGeneration)
            .map_err(|error| Error::Runtime(RuntimeError::Control(error.to_string())))
    }

    /// Resumes every vCPU only from the named pause generation.
    pub fn resume(&self, generation: VmPauseGeneration) -> Result<()> {
        let vmm = self.running_vmm()?;
        let result = {
            let mut vmm = vmm.lock().map_err(|_| {
                Error::Runtime(RuntimeError::Control("VMM mutex is poisoned".to_string()))
            })?;
            if matches!(
                vmm.execution_state(),
                vmm::VmmExecutionState::Paused(current) if current == generation.0
            ) {
                self.vmm.publish_transition_started();
            }
            let result = vmm.resume_vcpus_from(generation.0);
            let execution = public_execution_state(vmm.execution_state());
            // Publish before releasing the VMM mutex so concurrent barriers cannot reorder states.
            self.vmm.publish_execution_state(execution);
            result
        };
        result.map_err(|error| Error::Runtime(RuntimeError::Control(error.to_string())))
    }

    /// Returns the current execution boundary, or `None` before VMM startup completes.
    pub fn execution_state(&self) -> Option<VmExecutionState> {
        self.vmm.execution_state()
    }

    /// Blocks until every vCPU has acknowledged a running boundary.
    ///
    /// Unlike repeatedly calling [`execution_state`](Self::execution_state), this wait sleeps on a
    /// runtime notification and therefore introduces no fixed polling interval. It works both for
    /// initial VM startup and for a later transition out of [`VmExecutionState::Paused`].
    pub fn wait_until_running(&self, timeout: Duration) -> Result<VmExecutionState> {
        self.vmm.wait_until_running(timeout)
    }

    /// Blocks until the constructed VMM publishes a confirmed paused boundary.
    ///
    /// Restore coordinators use this with [`Vm::set_start_paused`] to finish activation work
    /// without polling or allowing a guest instruction to run first.
    pub fn wait_until_paused(&self, timeout: Duration) -> Result<VmExecutionState> {
        self.vmm.wait_until_paused(timeout)
    }

    /// Wait for the construction-paused boundary without charging storage preparation
    /// against an execution-barrier deadline. The embedding runtime owns cancellation.
    /// This does not resume the VM or relax any vCPU/device quiescence barriers.
    pub fn wait_until_paused_without_timeout(&self) -> Result<VmExecutionState> {
        self.vmm.wait_until_paused_inner(None)
    }

    /// Captures the exact backend execution state at the current paused boundary.
    pub fn capture_execution_state(&self) -> Result<vmm::execution_state::ExecutionState> {
        self.with_running_vmm(vmm::Vmm::capture_execution_state)
    }

    /// Captures one virtio-block device and parks it until this VM is resumed.
    #[cfg(feature = "blk")]
    pub fn capture_block_device_state(
        &self,
        device_id: &str,
    ) -> Result<vmm::device_state::BlockDeviceState> {
        self.with_running_vmm(|vmm| vmm.capture_block_device_state(device_id))
    }

    /// Returns the deterministic logical inventory of registered virtio-mmio devices.
    #[cfg(feature = "blk")]
    pub fn virtio_device_inventory(&self) -> Result<Vec<(u32, String)>> {
        self.with_running_vmm(|vmm| Ok(vmm.virtio_device_inventory()))
    }

    /// Reports whether one configured virtio device supports reversible queue quiescence.
    #[cfg(feature = "blk")]
    pub fn virtio_device_supports_quiesce(
        &self,
        device_type: u32,
        device_id: &str,
    ) -> Result<bool> {
        self.with_running_vmm(|vmm| vmm.virtio_device_supports_quiesce(device_type, device_id))
    }

    /// Captures one quiesce-capable destination-recreated virtio device.
    #[cfg(feature = "blk")]
    pub fn capture_virtio_device_state(
        &self,
        device_type: u32,
        device_id: &str,
    ) -> Result<vmm::device_state::VirtioDeviceState> {
        self.with_running_vmm(|vmm| vmm.capture_virtio_device_state(device_type, device_id))
    }

    /// Restores one destination-recreated virtio device before resuming the paused VM.
    #[cfg(feature = "blk")]
    pub fn restore_virtio_device_state(
        &self,
        state: &vmm::device_state::VirtioDeviceState,
    ) -> Result<()> {
        self.with_running_vmm(|vmm| vmm.restore_virtio_device_state(state))
    }

    /// Restores one parked virtio-block device before resuming the paused VM.
    #[cfg(feature = "blk")]
    pub fn restore_block_device_state(
        &self,
        device_id: &str,
        state: &vmm::device_state::BlockDeviceState,
    ) -> Result<()> {
        self.with_running_vmm(|vmm| vmm.restore_block_device_state(device_id, state))
    }

    /// Replaces one parked block backend with an already opened, compatible chain.
    #[cfg(feature = "blk")]
    pub fn replace_block_backend(
        &self,
        device_id: &str,
        backend: devices::virtio::PreparedBlockBackend,
    ) -> Result<()> {
        self.with_running_vmm(|vmm| vmm.replace_block_backend(device_id, backend))
    }

    /// Grows an exclusively owned writable block device at a paused queue boundary.
    /// The image and virtio capacity change; guest filesystem expansion remains the caller's job.
    /// Persist recovery intent first and recover forward if this returns an I/O error.
    #[cfg(feature = "blk")]
    pub fn grow_block_capacity(&self, device_id: &str, size_bytes: u64) -> Result<()> {
        self.with_running_vmm(|vmm| vmm.grow_block_capacity(device_id, size_bytes))
    }

    /// Plans a complete memory generation while the VM is paused.
    pub fn plan_full_memory_capture(&self) -> Result<vmm::memory_state::MemoryCapturePlan> {
        self.with_running_vmm(vmm::Vmm::plan_full_memory_capture)
    }

    /// Plans a memory delta relative to the latest retained baseline.
    pub fn plan_incremental_memory_capture(
        &self,
        baseline: vmm::memory_state::MemoryBaselineToken,
    ) -> Result<vmm::memory_state::IncrementalCaptureDecision> {
        self.with_running_vmm(|vmm| vmm.plan_incremental_memory_capture(baseline))
    }

    /// Plans a delta with a caller-selected complete-capture crossover percentage.
    pub fn plan_incremental_memory_capture_with_threshold(
        &self,
        baseline: vmm::memory_state::MemoryBaselineToken,
        max_dirty_percent: u64,
    ) -> Result<vmm::memory_state::IncrementalCaptureDecision> {
        self.with_running_vmm(|vmm| {
            vmm.plan_incremental_memory_capture_with_threshold(baseline, max_dirty_percent)
        })
    }

    /// Streams one pending complete or incremental memory generation.
    pub fn capture_memory(
        &self,
        capture: &vmm::memory_state::MemoryCapturePlan,
        options: vmm::memory_state::MemoryCaptureOptions,
        sink: &mut dyn vmm::memory_state::MemoryCaptureSink,
    ) -> Result<vmm::memory_state::MemoryCaptureStats> {
        self.with_running_vmm(|vmm| vmm.capture_memory(capture, options, sink))
    }

    /// Accepts a memory generation after its durable objects and manifest are published.
    pub fn publish_memory_capture(
        &self,
        capture: &vmm::memory_state::MemoryCapturePlan,
    ) -> Result<vmm::memory_state::MemoryBaselineToken> {
        self.with_running_vmm(|vmm| vmm.publish_memory_capture(capture))
    }

    /// Abandons a failed candidate while preserving its dirty coverage for retry.
    pub fn abandon_memory_capture(
        &self,
        capture: &vmm::memory_state::MemoryCapturePlan,
    ) -> Result<()> {
        self.with_running_vmm(|vmm| vmm.abandon_memory_capture(capture))
    }

    /// Releases the retained baseline and removes backend dirty-tracking overhead.
    pub fn release_memory_baseline(&self) -> Result<()> {
        self.with_running_vmm(vmm::Vmm::release_memory_baseline)
    }

    /// Returns the currently retained runtime-local memory baseline.
    pub fn retained_memory_baseline(&self) -> Option<vmm::memory_state::MemoryBaselineToken> {
        let vmm = self.vmm.running_vmm().ok()?;
        let baseline = vmm.lock().ok()?.retained_memory_baseline();
        baseline
    }

    /// Whether this VM was constructed with the VM-generation transport.
    ///
    /// Use [`vm_generation_state`](Self::vm_generation_state) after boot to distinguish transport
    /// presence from a bundled or custom kernel that actually bound the guest driver.
    pub fn vm_generation_transport_present(&self) -> bool {
        self.generation.is_some()
    }

    /// Publish a fresh VM generation and signal the guest driver.
    ///
    /// The caller must generate `id` with its operating-system CSPRNG and persist it with the
    /// restore attempt. Returning `None` means the transport is absent or its sequence space was
    /// exhausted. Clone/rollback activation must fail closed in either case.
    pub fn install_vm_generation_id(&self, id: VmGenerationId) -> Option<VmGenerationRequest> {
        let generation = self.generation.as_ref()?;
        let sequence = generation.lock().unwrap().install(id)?;
        Some(VmGenerationRequest { sequence, id })
    }

    /// Publish one identity-and-clock activation; unsupported guest kernels return `None`.
    ///
    /// The guest reads fresh host time during processing, not the time when this request was
    /// installed. A processed acknowledgement covers both the identity and the clock update.
    pub fn install_vm_generation_and_clock(
        &self,
        id: VmGenerationId,
    ) -> Option<VmGenerationRequest> {
        let generation = self.generation.as_ref()?;
        let sequence = generation.lock().unwrap().install_with_clock(id)?;
        Some(VmGenerationRequest { sequence, id })
    }

    /// Whether the bound guest kernel supports wall-clock correction without clone semantics.
    pub fn clock_sync_supported(&self) -> bool {
        self.generation
            .as_ref()
            .is_some_and(|generation| generation.lock().unwrap().clock_only_supported())
    }

    /// Request wall-clock correction for ordinary pause/resume without rotating VM identity.
    ///
    /// Resume vCPUs to let the kernel process this request, then wait through
    /// [`Self::wait_vm_generation_processed`] before thawing user workloads. `None` means the
    /// guest lacks the capability, an identity activation is pending, or requests are exhausted.
    pub fn request_clock_sync(&self) -> Option<VmGenerationRequest> {
        let (sequence, id) = self
            .generation
            .as_ref()?
            .lock()
            .unwrap()
            .request_clock_sync()?;
        Some(VmGenerationRequest { sequence, id })
    }

    /// Wait without polling for the guest kernel to process one exact generation request.
    pub fn wait_vm_generation_processed(
        &self,
        request: VmGenerationRequest,
        timeout: Duration,
    ) -> Option<VmGenerationWaitOutcome> {
        let generation = self.generation.as_ref()?;
        let processing = generation.lock().unwrap().processing_handle();
        let outcome = processing.wait_processed(request.sequence, request.id, timeout);
        Some(match outcome {
            devices::virtio::GenerationWaitOutcome::Processed => VmGenerationWaitOutcome::Processed,
            devices::virtio::GenerationWaitOutcome::Superseded => {
                VmGenerationWaitOutcome::Superseded
            }
            devices::virtio::GenerationWaitOutcome::TimedOut => VmGenerationWaitOutcome::TimedOut,
            devices::virtio::GenerationWaitOutcome::Failed => VmGenerationWaitOutcome::Failed,
        })
    }

    /// Return the latest request and guest-kernel acknowledgement.
    pub fn vm_generation_state(&self) -> Option<VmGenerationState> {
        let generation = self.generation.as_ref()?;
        let snapshot = generation.lock().unwrap().state_snapshot();
        Some(VmGenerationState {
            driver_ready: snapshot.driver_ready,
            driver_error: snapshot.driver_error,
            clock_sync_supported: snapshot.clock_sync_supported,
            requested: (snapshot.request_sequence != 0).then_some(VmGenerationRequest {
                sequence: snapshot.request_sequence,
                id: snapshot.requested_id,
            }),
            processed: (snapshot.processed_sequence != 0).then_some(VmGenerationRequest {
                sequence: snapshot.processed_sequence,
                id: snapshot.processed_id,
            }),
        })
    }

    /// Whether the running VM can resize memory live.
    pub fn memory_resize_supported(&self) -> bool {
        self.mem.is_some()
    }

    /// Whether the running VM can resize its online CPU count live.
    pub fn cpu_resize_supported(&self) -> bool {
        self.cpu.is_some()
    }

    /// Ask the guest to converge on `online` CPUs and enforce that ceiling
    /// host-side. Returns the accepted target (clamped to 1..=possible), or
    /// `None` when the VM booted without CPU capacity. The guest driver
    /// onlines/offlines asynchronously; poll [`cpu_state`](Self::cpu_state)
    /// for convergence — enforcement applies immediately either way.
    pub fn set_cpu_target(&self, online: u32) -> Option<u32> {
        let cpu = self.cpu.as_ref()?;
        Some(cpu.lock().unwrap().set_requested_online(online))
    }

    /// Current CPU sizing, or `None` when the VM booted without capacity.
    pub fn cpu_state(&self) -> Option<VmCpuState> {
        let cpu = self.cpu.as_ref()?;
        let snap = cpu.lock().unwrap().state_snapshot();
        Some(VmCpuState {
            possible: snap.possible,
            requested_online: snap.requested_online,
            actual_online: snap.actual_online,
            enforced: snap.enforced,
        })
    }

    /// Ask the guest to converge on `total_mib` of usable memory.
    ///
    /// Returns the accepted target in MiB (clamped to the boot..max range and
    /// rounded down to hotplug block granularity), or `None` when the VM
    /// booted without hotplug capacity. The guest plugs/unplugs blocks
    /// asynchronously; poll [`memory_state`](Self::memory_state) for
    /// convergence.
    pub fn set_memory_target_mib(&self, total_mib: u64) -> Option<u64> {
        let mem = self.mem.as_ref()?;
        let hotplug_target = total_mib.saturating_sub(self.boot_mib) << 20;
        let accepted = mem.lock().unwrap().set_requested_size(hotplug_target);
        Some(self.boot_mib + (accepted >> 20))
    }

    /// Current memory sizing, or `None` when the VM booted without capacity.
    pub fn memory_state(&self) -> Option<VmMemoryState> {
        let mem = self.mem.as_ref()?;
        let snap = mem.lock().unwrap().state_snapshot();
        Some(VmMemoryState {
            boot_mib: self.boot_mib,
            target_mib: self.boot_mib + (snap.requested_size >> 20),
            current_mib: self.boot_mib + (snap.plugged_size >> 20),
            max_mib: self.boot_mib + (snap.region_size >> 20),
        })
    }

    fn running_vmm(&self) -> Result<Arc<std::sync::Mutex<vmm::Vmm>>> {
        self.vmm.running_vmm()
    }

    fn with_running_vmm<T>(
        &self,
        operation: impl FnOnce(&mut vmm::Vmm) -> vmm::Result<T>,
    ) -> Result<T> {
        let vmm = self.running_vmm()?;
        let mut vmm = vmm.lock().map_err(|_| {
            Error::Runtime(RuntimeError::Control("VMM mutex is poisoned".to_string()))
        })?;
        operation(&mut vmm)
            .map_err(|error| Error::Runtime(RuntimeError::Control(error.to_string())))
    }
}
