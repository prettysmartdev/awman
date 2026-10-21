//! Binary-side native integrations. The awman library keeps its safe-code boundary.
//! Only Layer 0 constants are imported from awman; engine contracts are adapted by
//! the binary package. No helper executable is produced or extracted.
#![deny(unsafe_code)]

mod worker;
pub use worker::dispatch_worker;

/// Prepare the static provider and retain SDK version metadata for an embedding
/// worker. Must run before entering the VM. Unsupported consumers fail closed.
#[cfg(awman_builtin)]
pub fn prepare_builtin_worker() -> Result<(), String> {
    version::retain();
    kernel::prepare()
}

#[cfg(awman_builtin)]
mod kernel;
#[cfg(awman_builtin)]
mod version;

#[cfg(target_os = "macos")]
mod apple_xpc;
/// Safe dictionary transport contract used by the native shim.
pub mod xpc;
#[cfg(target_os = "macos")]
pub use apple_xpc::NativeXpc;
