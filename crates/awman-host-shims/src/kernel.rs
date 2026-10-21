//! Build-verified kernel image served to msb_krun's `static-krunfw` accessor.
//!
//! `krunfw_get_kernel` is the only global C symbol awman defines. The static
//! linker resolves msb_krun's `extern "C"` reference to it: no
//! `--export-dynamic`, custom link section, `dlsym` or firmware library is
//! involved, and a missing definition is a link error, not a silent fallback.
//! Everything except the callback itself is safe code.
use std::ffi::{c_char, CStr};
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/msb_kernel.rs"));

/// Alignment of the served image and granularity of its size and guest
/// address. 64 KiB covers 4, 16 and 64 KiB host pages, and msb_krun refuses a
/// host or guest address that is not host-page aligned.
const KERNEL_ALIGN: usize = 65536;
/// Upper bound shared with `build.rs`; anything larger is not a guest kernel.
const KERNEL_MAX_LEN: usize = 64 * 1024 * 1024;
/// arm64 `Image` header: the magic `ARM\x64` at byte offset 56.
const ARM64_IMAGE_MAGIC: (usize, &[u8; 4]) = (56, b"ARM\x64");

/// The image this process serves, materialised once by [`prepare`].
struct Image {
    /// Byte strings of the firmware path the parent configured with
    /// `microsandbox::set_libkrunfw_path(&executable)`: this executable as
    /// reported by the OS, plus its canonical form when that differs.
    accepted_paths: Vec<Vec<u8>>,
    /// Exposed address of the leaked, writable, `KERNEL_ALIGN`-aligned copy.
    /// Kept as an address so no Rust reference aliases the VMM's writes.
    address: usize,
    layout: Layout,
}

/// Validated guest placement reported to the VMM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    guest_address: u64,
    entry_address: u64,
    size: usize,
}

static IMAGE: OnceLock<Result<Image, String>> = OnceLock::new();

/// Arm the accessor for this worker process.
///
/// Called by the private worker route before the SDK builds the VM. Validates
/// the build-verified payload and creates its process-lifetime copy exactly
/// once; repeated calls return the first outcome. An `Err` fails the worker
/// closed (exit 70). A process that never calls this serves no kernel.
pub fn prepare() -> Result<(), String> {
    IMAGE
        .get_or_init(|| {
            require_copying_consumer(std::env::consts::ARCH)?;
            let layout = validate_image(
                &KERNEL.0,
                GUEST_ADDRESS,
                ENTRY_ADDRESS,
                std::env::consts::ARCH,
            )?;
            let accepted_paths = executable_paths()?;
            let address = writable_aligned_copy(&KERNEL.0).expose_provenance();
            Ok(Image {
                accepted_paths,
                address,
                layout,
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

/// The x86_64 pinned runner maps the callback allocation directly as guest RAM.
/// Repeated callbacks are idempotent, so that runner would share writable kernel
/// data between VMs. Until it enables Vm::set_private_memory_boot(true), fail
/// closed even if someone later verifies an x86_64 payload (WI 0124).
fn require_copying_consumer(arch: &str) -> Result<(), String> {
    if arch == "x86_64" {
        return Err(
            "embedded x86_64 kernel requires a private-memory boot consumer (WI 0124)".into(),
        );
    }
    Ok(())
}

/// Exercise the actual accessor before any VM exists in this private process.
/// This is for artifact verification, including after optimization and stripping.
pub(crate) fn smoke_check() -> Result<(), String> {
    prepare()?;
    let path = std::ffi::CString::new(executable_paths()?.remove(0))
        .map_err(|_| "executable path contains NUL")?;
    let (mut guest, mut entry, mut size) = (0, 0, 0);
    // SAFETY: path is a live CString and the three outputs are distinct,
    // aligned writable locals. No VM is running in this diagnostic process.
    #[allow(unsafe_code)]
    let pointer = unsafe { krunfw_get_kernel(path.as_ptr(), &mut guest, &mut entry, &mut size) };
    check_smoke_result(pointer.cast(), guest, entry, size)
}

fn check_smoke_result(pointer: *mut u8, guest: u64, entry: u64, size: usize) -> Result<(), String> {
    let image = IMAGE
        .get()
        .and_then(|result| result.as_ref().ok())
        .ok_or("embedded kernel is not prepared")?;
    if pointer.is_null()
        || pointer.addr() != image.address
        || (guest, entry, size)
            != (
                image.layout.guest_address,
                image.layout.entry_address,
                image.layout.size,
            )
        || !pointer.addr().is_multiple_of(KERNEL_ALIGN)
    {
        return Err("embedded kernel callback refused or returned an invalid layout".into());
    }
    // SAFETY: address and size equal the prepared, leaked allocation. This
    // private diagnostic is called before any VM can write it; the slice is
    // dropped here and is never retained across a consumer's writes.
    #[allow(unsafe_code)]
    let bytes = unsafe {
        std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(image.address), size)
    };
    if bytes != KERNEL.0 {
        return Err("embedded kernel callback contents differ from the payload".into());
    }
    Ok(())
}

/// The prepared image, if `requested` names it. No I/O: the parent passes the
/// executable path it resolved itself, so an exact byte match is required.
fn served_image(requested: &[u8]) -> Option<&'static Image> {
    let image = IMAGE.get()?.as_ref().ok()?;
    selects_embedded_kernel(requested, &image.accepted_paths).then_some(image)
}

/// Raw OS bytes of this executable's path and canonical path, deduplicated.
fn executable_paths() -> Result<Vec<Vec<u8>>, String> {
    use std::os::unix::ffi::OsStrExt;
    let executable = std::env::current_exe()
        .map_err(|_| "builtin worker cannot resolve its executable".to_string())?;
    let mut paths = vec![executable.as_os_str().as_bytes().to_vec()];
    if let Ok(canonical) = executable.canonicalize() {
        let canonical = canonical.as_os_str().as_bytes().to_vec();
        if canonical != paths[0] {
            paths.push(canonical);
        }
    }
    Ok(paths)
}

/// Exact, non-empty byte equality with one accepted path. Paths are compared
/// as raw bytes, so non-UTF-8 names neither panic nor match loosely.
fn selects_embedded_kernel(requested: &[u8], accepted: &[Vec<u8>]) -> bool {
    !requested.is_empty() && accepted.iter().any(|path| path.as_slice() == requested)
}

/// Check the payload and its metadata against everything msb_krun and the
/// VMMs assume before any pointer is handed out.
fn validate_image(bytes: &[u8], guest: u64, entry: u64, arch: &str) -> Result<Layout, String> {
    let size = bytes.len();
    if size == 0 || size > KERNEL_MAX_LEN {
        return Err("embedded kernel is empty or oversized".into());
    }
    if !size.is_multiple_of(KERNEL_ALIGN) {
        return Err("embedded kernel size is not a 64 KiB multiple".into());
    }
    if !guest.is_multiple_of(KERNEL_ALIGN as u64) {
        return Err("embedded kernel guest address is not 64 KiB aligned".into());
    }
    let end = guest
        .checked_add(size as u64)
        .ok_or_else(|| "embedded kernel guest range overflows".to_string())?;
    if !(guest..end).contains(&entry) {
        return Err("embedded kernel entry address is outside the image".into());
    }
    // libkrunfw's x86_64 image is a raw vmlinux body without a magic number;
    // its hash pin in `build.rs` is the architecture check there.
    if arch == "aarch64" {
        let (offset, magic) = ARM64_IMAGE_MAGIC;
        if bytes.get(offset..offset + magic.len()) != Some(magic.as_slice()) {
            return Err("embedded kernel is not an arm64 Image".into());
        }
    }
    Ok(Layout {
        guest_address: guest,
        entry_address: entry,
        size,
    })
}

/// Copy `source` into leaked, zero-initialised heap memory starting at a
/// `KERNEL_ALIGN` boundary and return a raw pointer to it. The allocation is
/// never freed and no reference to it outlives this function, so the VMM may
/// map it as guest RAM (x86_64 does) for the rest of the process.
fn writable_aligned_copy(source: &[u8]) -> *mut u8 {
    let buffer: &'static mut [u8] =
        Box::leak(vec![0u8; source.len() + KERNEL_ALIGN].into_boxed_slice());
    let offset =
        (buffer.as_ptr() as usize).next_multiple_of(KERNEL_ALIGN) - buffer.as_ptr() as usize;
    let aligned = &mut buffer[offset..offset + source.len()];
    aligned.copy_from_slice(source);
    aligned.as_mut_ptr()
}

/// msb_krun `static-krunfw` accessor, called from `Vm::enter` (`load_krunfw`).
///
/// Returns the embedded kernel for the executable path armed by [`prepare`],
/// and NULL — which msb_krun reports as "no krunfw image for the requested
/// path", with no dynamic-library fallback — for anything else.
///
/// # Safety
///
/// - `path` is NULL or points to a NUL-terminated byte string that stays valid
///   for this call (msb_krun passes a `CString` built from
///   `KernelBuilder::krunfw_path`). It is read, never retained, and need not
///   be UTF-8.
/// - `guest_addr`, `entry_addr` and `size` are NULL or valid, writable,
///   aligned pointers to caller-owned `u64`/`u64`/`usize`. Each is written
///   once, and only when a non-NULL pointer is returned.
/// - A non-NULL return is the start of the single image of this process: a
///   leaked, writable, 64 KiB-aligned allocation of `*size` bytes, never freed
///   and never aliased by a Rust reference after [`prepare`]. Repeated and
///   concurrent calls return the same address and metadata. Consumers must copy
///   it into independent guest memory before a guest writes. Direct-mapping
///   x86_64 consumers are refused by prepare().
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn krunfw_get_kernel(
    path: *const c_char,
    guest_addr: *mut u64,
    entry_addr: *mut u64,
    size: *mut usize,
) -> *mut c_char {
    if path.is_null() || guest_addr.is_null() || entry_addr.is_null() || size.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `path` is non-NULL and NUL-terminated per the contract; the
    // borrow ends before this function returns.
    let requested = unsafe { CStr::from_ptr(path) }.to_bytes();
    let Some(image) = served_image(requested) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the out-pointers are non-NULL, valid, aligned and writable per
    // the contract; each receives one plain integer.
    unsafe {
        guest_addr.write(image.layout.guest_address);
        entry_addr.write(image.layout.entry_address);
        size.write(image.layout.size);
    }
    std::ptr::with_exposed_provenance_mut(image.address)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_mapping_consumer_is_refused() {
        assert!(require_copying_consumer("x86_64")
            .unwrap_err()
            .contains("private-memory"));
        assert!(require_copying_consumer("aarch64").is_ok());
    }

    #[test]
    fn artifact_smoke_rejects_callback_refusal_and_bad_metadata() {
        prepare().unwrap();
        smoke_check().unwrap();
        assert!(check_smoke_result(
            std::ptr::null_mut(),
            GUEST_ADDRESS,
            ENTRY_ADDRESS,
            KERNEL.0.len()
        )
        .is_err());
        let image = IMAGE.get().unwrap().as_ref().unwrap();
        assert!(check_smoke_result(
            std::ptr::with_exposed_provenance_mut(image.address),
            GUEST_ADDRESS,
            ENTRY_ADDRESS,
            0
        )
        .is_err());
    }
    #[test]
    fn writable_copy_is_aligned_and_identical() {
        let source = vec![7u8; 4096 + 3];
        let copy = writable_aligned_copy(&source);
        assert!((copy as usize).is_multiple_of(KERNEL_ALIGN));
        // SAFETY: the copy is a leaked allocation of `source.len()` bytes.
        #[allow(unsafe_code)]
        let copy = unsafe { std::slice::from_raw_parts(copy, source.len()) };
        assert_eq!(copy, source.as_slice());
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;
    use std::ffi::CString;

    #[allow(unsafe_code)]
    fn call(path: &CStr) -> (usize, u64, u64, usize) {
        let (mut guest, mut entry, mut size) = (11, 22, 33);
        // SAFETY: CStr is terminated; outputs are distinct writable locals.
        let pointer =
            unsafe { krunfw_get_kernel(path.as_ptr(), &mut guest, &mut entry, &mut size) };
        (pointer as usize, guest, entry, size)
    }

    fn in_fresh_process(test: &str, child: &str, body: impl FnOnce()) {
        if std::env::var_os(child).is_some() {
            body();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env_clear()
            .env(child, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn selector_requires_exact_nonempty_os_bytes() {
        let accepted = vec![
            b"/worker".to_vec(),
            b"/canonical/worker".to_vec(),
            b"/worker-\xff".to_vec(),
        ];
        for path in &accepted {
            assert!(selects_embedded_kernel(path, &accepted));
        }
        for path in [
            b"".as_slice(),
            b"/worker/",
            b"/./worker",
            b"@embedded-kernel",
            b"/worker-\xfe",
        ] {
            assert!(!selects_embedded_kernel(path, &accepted), "{path:?}");
        }
        assert!(!selects_embedded_kernel(b"/worker", &[]));
        assert!(!selects_embedded_kernel(b"", &[vec![]]));
    }

    #[test]
    fn unarmed_process_refuses_the_executable_path() {
        in_fresh_process(
            "kernel::callback_tests::unarmed_process_refuses_the_executable_path",
            "AWMAN_KERNEL_UNARMED_CHILD",
            || {
                assert!(IMAGE.get().is_none());
                for path in executable_paths().unwrap() {
                    assert_eq!(call(&CString::new(path).unwrap()), (0, 11, 22, 33));
                }
            },
        );
    }

    #[test]
    #[allow(unsafe_code)]
    fn null_path_and_each_null_output_are_refused_without_writes() {
        prepare().unwrap();
        let path = CString::new(executable_paths().unwrap().remove(0)).unwrap();
        for missing in 0..4 {
            let (mut guest, mut entry, mut size) = (11, 22, 33);
            // SAFETY: each pointer is NULL or valid for its documented type.
            let pointer = unsafe {
                krunfw_get_kernel(
                    if missing == 0 {
                        std::ptr::null()
                    } else {
                        path.as_ptr()
                    },
                    if missing == 1 {
                        std::ptr::null_mut()
                    } else {
                        &mut guest
                    },
                    if missing == 2 {
                        std::ptr::null_mut()
                    } else {
                        &mut entry
                    },
                    if missing == 3 {
                        std::ptr::null_mut()
                    } else {
                        &mut size
                    },
                )
            };
            assert!(pointer.is_null());
            assert_eq!((guest, entry, size), (11, 22, 33));
        }
    }

    #[test]
    fn unknown_empty_and_non_utf8_paths_are_refused_without_writes() {
        prepare().unwrap();
        for path in [
            b"/nonexistent".as_slice(),
            b"@embedded-kernel",
            b"",
            b"/unknown-\xff",
        ] {
            assert_eq!(call(&CString::new(path).unwrap()), (0, 11, 22, 33));
        }
        let path = executable_paths().unwrap().remove(0);
        let mut alias = b"/./".to_vec();
        alias.extend_from_slice(&path[1..]);
        assert_eq!(call(&CString::new(alias).unwrap()), (0, 11, 22, 33));
    }

    #[test]
    #[allow(unsafe_code)]
    fn matching_callback_is_idempotent_concurrent_aligned_and_writable() {
        // Isolate writes and race the first prepare(), not just OnceLock reads.
        in_fresh_process("kernel::callback_tests::matching_callback_is_idempotent_concurrent_aligned_and_writable", "AWMAN_KERNEL_ARMED_CHILD", || {
            assert!(IMAGE.get().is_none());
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let threads: Vec<_> = (0..8).map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    prepare().unwrap();
                    let path = CString::new(executable_paths().unwrap().remove(0)).unwrap();
                    let first = call(&path);
                    for _ in 0..16 {
                        prepare().unwrap();
                        assert_eq!(call(&path), first);
                    }
                    first
                })
            }).collect();
            let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
            let first = results[0];
            assert_ne!(first.0, 0);
            assert_eq!(first.0 % KERNEL_ALIGN, 0);
            assert_eq!((first.1, first.2, first.3), (GUEST_ADDRESS, ENTRY_ADDRESS, KERNEL.0.len()));
            assert!(results.iter().all(|result| *result == first));
            for path in executable_paths().unwrap() {
                assert_eq!(call(&CString::new(path).unwrap()), first);
            }
            let pointer = std::ptr::with_exposed_provenance_mut::<u8>(first.0);
            // SAFETY: callback returns KERNEL.0.len() writable bytes. All threads
            // have joined; no references or concurrent accesses alias this copy.
            unsafe {
                assert_eq!(std::slice::from_raw_parts(pointer, first.3), &KERNEL.0);
                for index in [0, first.3 / 2, first.3 - 1] {
                    let byte = pointer.add(index);
                    let original = byte.read();
                    byte.write(original ^ 0xff);
                    assert_eq!(byte.read(), original ^ 0xff);
                    byte.write(original);
                }
            }
        });
    }

    #[test]
    fn image_validation_rejects_empty_oversized_and_non_page_sizes() {
        for bytes in [vec![], vec![0; KERNEL_MAX_LEN + 1]] {
            assert_eq!(
                validate_image(&bytes, 0, 0, "x86_64").unwrap_err(),
                "embedded kernel is empty or oversized"
            );
        }
        for size in [1, 59, KERNEL_ALIGN - 1, KERNEL_ALIGN + 1] {
            assert_eq!(
                validate_image(&vec![0; size], 0, 0, "aarch64").unwrap_err(),
                "embedded kernel size is not a 64 KiB multiple"
            );
        }
    }

    #[test]
    fn image_validation_checks_alignment_overflow_and_entry_boundaries() {
        let bytes = vec![0; KERNEL_ALIGN];
        assert!(validate_image(&bytes, 1, 1, "x86_64")
            .unwrap_err()
            .contains("not 64 KiB aligned"));
        let guest = u64::MAX - (KERNEL_ALIGN as u64 - 1);
        assert!(validate_image(&bytes, guest, guest, "x86_64")
            .unwrap_err()
            .contains("overflows"));
        let guest = KERNEL_ALIGN as u64;
        for entry in [guest - 1, guest * 2, u64::MAX] {
            assert!(validate_image(&bytes, guest, entry, "x86_64")
                .unwrap_err()
                .contains("outside the image"));
        }
        for entry in [guest, guest * 2 - 1] {
            assert_eq!(
                validate_image(&bytes, guest, entry, "x86_64").unwrap(),
                Layout {
                    guest_address: guest,
                    entry_address: entry,
                    size: bytes.len()
                }
            );
        }
    }

    #[test]
    fn image_validation_checks_arm64_magic_and_accepts_the_pinned_payload() {
        let mut bytes = vec![0; KERNEL_ALIGN];
        assert!(validate_image(&bytes, 0, 0, "aarch64")
            .unwrap_err()
            .contains("not an arm64 Image"));
        bytes[56..60].copy_from_slice(b"ARM\x64");
        assert!(validate_image(&bytes, 0, 0, "aarch64").is_ok());
        assert_eq!(
            validate_image(
                &KERNEL.0,
                GUEST_ADDRESS,
                ENTRY_ADDRESS,
                std::env::consts::ARCH
            )
            .unwrap()
            .size,
            KERNEL.0.len()
        );
    }
}
