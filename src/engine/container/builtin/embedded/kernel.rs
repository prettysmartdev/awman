//! Build-verified, process-lifetime kernel registration. No pointers cross IPC.
include!(concat!(env!("OUT_DIR"), "/msb_kernel.rs"));

const KERNEL_ALIGN: usize = 65536;

pub fn register() -> Result<(), String> {
    let bytes = guest_kernel_bytes();
    if bytes.is_empty() || !(bytes.as_ptr() as usize).is_multiple_of(KERNEL_ALIGN) {
        return Err("embedded kernel is empty or incorrectly aligned".into());
    }
    let kernel = msb_krun::api::embedded_kernel::EmbeddedKernel {
        bytes,
        guest_address: GUEST_ADDRESS,
        entry_address: ENTRY_ADDRESS,
    };
    if let Some(existing) = msb_krun::api::embedded_kernel::embedded_kernel() {
        if std::ptr::eq(existing.bytes, bytes)
            && existing.guest_address == GUEST_ADDRESS
            && existing.entry_address == ENTRY_ADDRESS
        {
            return Ok(());
        }
        return Err("a different embedded kernel is already registered".into());
    }
    msb_krun::api::embedded_kernel::register_embedded_kernel(kernel)
        .map_err(|_| "embedded kernel registration raced with another provider".into())
}

/// aarch64 (and Windows x86_64) VMMs copy the kernel into guest RAM, so the
/// immutable static can be handed over directly.
#[cfg(not(target_arch = "x86_64"))]
fn guest_kernel_bytes() -> &'static [u8] {
    &KERNEL.0
}

/// On x86_64 Linux, msb_krun_vmm maps the host kernel buffer *as* guest RAM
/// (`MmapRegion::build_raw`, no copy). The static lives in read-only
/// `.rodata`, where every guest write into its own kernel image would fault.
/// Hand the VMM a writable, aligned, process-lifetime copy instead.
#[cfg(target_arch = "x86_64")]
fn guest_kernel_bytes() -> &'static [u8] {
    static COPY: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
    COPY.get_or_init(|| writable_aligned_copy(&KERNEL.0))
}

/// Copy `source` into leaked heap memory starting at a `KERNEL_ALIGN` boundary.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn writable_aligned_copy(source: &[u8]) -> &'static [u8] {
    let buffer: &'static mut [u8] =
        Box::leak(vec![0u8; source.len() + KERNEL_ALIGN].into_boxed_slice());
    let offset =
        (buffer.as_ptr() as usize).next_multiple_of(KERNEL_ALIGN) - buffer.as_ptr() as usize;
    let aligned = &mut buffer[offset..offset + source.len()];
    aligned.copy_from_slice(source);
    aligned
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writable_copy_is_aligned_and_identical() {
        let source = vec![7u8; 4096 + 3];
        let copy = writable_aligned_copy(&source);
        assert_eq!(copy, source.as_slice());
        assert!((copy.as_ptr() as usize).is_multiple_of(KERNEL_ALIGN));
    }
}
