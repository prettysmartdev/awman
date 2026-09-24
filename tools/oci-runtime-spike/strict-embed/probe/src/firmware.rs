use std::ffi::c_char;

include!(concat!(env!("OUT_DIR"), "/kernel.rs"));

#[no_mangle]
pub unsafe extern "C" fn krunfw_get_kernel(
    guest_address: *mut u64,
    entry_address: *mut u64,
    size: *mut usize,
) -> *mut c_char {
    guest_address.write(GUEST_ADDRESS);
    entry_address.write(ENTRY_ADDRESS);
    size.write(KERNEL.0.len());
    eprintln!("embedded-kernel-provider-called bytes={}", KERNEL.0.len());
    KERNEL.0.as_ptr() as *mut c_char
}
