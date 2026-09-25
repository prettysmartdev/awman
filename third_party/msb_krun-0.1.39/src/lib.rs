//! msb_krun - Native Rust API for libkrun microVMs.
//!
//! This crate provides a builder-pattern API for creating and entering microVMs
//! using libkrun's VMM infrastructure.
//!
//! # Lifecycle
//!
//! [`Vm::enter()`] never returns on success. When the guest shuts down, the
//! VMM calls `_exit()`, killing the entire process. `enter()` only returns
//! `Err` if something fails before the VMM takes over.
//!
//! # Example
//!
//! ```rust,no_run
//! use msb_krun::{VmBuilder, Result};
//!
//! fn main() -> Result<()> {
//!     VmBuilder::new()
//!         .machine(|m| m.vcpus(4).memory_mib(2048))
//!         .fs(|fs| fs.root("/path/to/rootfs"))
//!         .exec(|e| e.path("/bin/myapp").args(["--flag"]).env("HOME", "/root"))
//!         .build()?
//!         .enter()?;
//!
//!     unreachable!()
//! }
//! ```

//--------------------------------------------------------------------------------------------------
// Modules
//--------------------------------------------------------------------------------------------------

pub mod api;
pub mod backends;

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

pub use api::builder::VmBuilder;
#[cfg(feature = "blk")]
pub use api::builders::CacheMode;
#[cfg(feature = "blk")]
pub use api::builders::DiskBuilder;
#[cfg(feature = "blk")]
pub use api::builders::DiskImageFormat;
#[cfg(feature = "blk")]
pub use api::builders::DiskLayer;
pub use api::builders::FsBuilder;
#[cfg(feature = "net")]
pub use api::builders::NetBuilder;
#[cfg(feature = "blk")]
pub use api::builders::SyncMode;
pub use api::builders::VsockBuilder;
#[cfg(feature = "blk")]
pub use api::builders::WritebackLimit;
pub use api::builders::{
    ConsoleBuilder, ConsolePortOptions, ExecBuilder, HostCpuId, HostMemoryPolicy, KernelBuilder,
    MachineBuilder, MemoryPlacementResult, NumaBuilder, NumaDistance, NumaNodeBuilder,
    NumaNodeConfig, NumaTopology, PlacementReport, VcpuPlacementResult,
};
pub use api::error::{BuildError, ConfigError, Error, Result, RuntimeError};
pub use api::exit_handle::ExitHandle;
pub use api::metrics::{
    BlockDeviceMetrics, BlockMetrics, CpuMetrics, FilesystemMetrics, MemoryMetrics, MetricsHandle,
    VmMetrics,
};
pub use api::vm::Vm;
#[cfg(not(feature = "tee"))]
pub use api::vm::{
    VmControl, VmCpuState, VmExecutionState, VmGenerationId, VmGenerationRequest,
    VmGenerationState, VmGenerationWaitOutcome, VmMemoryRestoreSource, VmMemoryRestoreTarget,
    VmMemoryState, VmPauseGeneration,
};
#[cfg(feature = "blk")]
pub use api::{
    BlockBackendSpec, BlockImageFormat, BlockLayerSpec, BlockSyncMode, PreparedBlockBackend,
};
#[cfg(all(feature = "blk", not(feature = "tee")))]
pub use api::{BlockDeviceState, VirtioDeviceState};
#[cfg(not(feature = "tee"))]
pub use api::{
    ExecutionArchitecture, ExecutionBackend, ExecutionState, FullCaptureReason, GuestMemoryRange,
    IncrementalCaptureDecision, MemoryBaselineToken, MemoryCaptureKind, MemoryCaptureOptions,
    MemoryCapturePlan, MemoryCaptureSink, MemoryCaptureStats, MemoryGeneration,
    MemoryTopologyGeneration, VcpuExecutionState,
};
#[cfg(not(feature = "tee"))]
pub use api::{PrivateMemoryBacking, PrivateMemoryRegion};
#[cfg(feature = "net")]
pub use devices::virtio::net::rate_limit::{
    RateLimiterConfig, RateLimiterConfigError, TokenBucketConfig,
};

#[cfg(not(target_os = "windows"))]
pub use backends::console::ConsolePortBackend;

#[cfg(not(any(feature = "tee", feature = "aws-nitro")))]
pub use backends::fs::DynFileSystem;

#[cfg(feature = "net")]
pub use backends::net::NetBackend;

pub use backends::vsock::{
    VsockConnectRequest, VsockNotifier, VsockPortBackend, VsockShutdown, VsockStreamBackend,
};
