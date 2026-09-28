//! Process-lifetime kernel provider for a statically embedded application kernel.

use std::sync::OnceLock;

/// The caller owns these bytes for the entire process lifetime. Addresses are
/// guest physical addresses; callers must validate them against pinned inputs.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedKernel {
    pub bytes: &'static [u8],
    pub guest_address: u64,
    pub entry_address: u64,
}

static KERNEL: OnceLock<EmbeddedKernel> = OnceLock::new();

/// Register exactly one kernel in this process.
pub fn register_embedded_kernel(kernel: EmbeddedKernel) -> Result<(), EmbeddedKernel> {
    KERNEL.set(kernel)
}

/// Return the registered process-lifetime kernel, if any.
pub fn embedded_kernel() -> Option<EmbeddedKernel> {
    KERNEL.get().copied()
}
