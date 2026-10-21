# Optional upstream libkrun documentation fix (prepared locally, NOT submitted)

Status: **draft text only.** No fork, branch, push or pull request was created. The
Microsandbox and libkrun contribution rules (`AGENTS.md` at v0.7.8) forbid branches,
commits, pushes and PRs without an explicit human request, and nothing in WI 0120
authorizes one. A person must choose to submit this; awman does not depend on it.

Target: `superradcompany/libkrun`, crate docs of `msb_krun` 0.1.41
(`src/krun/src/lib.rs`, "Static kernel" example at lines 61–82, the multi-image
example at 97–101, and the "Alignment and writability" section at 162–183).
Base commit: `b35c7e118d7a1e48c8fad103b258e64ff5c49147`.

## Problem

The `static-krunfw` example declares the kernel as

```rust
#[repr(align(65536))]
struct Aligned([u8; 0x10_0000]);
static mut KERNEL: Aligned = Aligned([0; 0x10_0000]);
```

and returns its address from `krunfw_get_kernel`. The docs require a 64 KiB-aligned,
writable image. On Linux ELF a `static` with `align(65536)` is honored. On **Mach-O**
the linker caps section alignment well below 64 KiB, so the symbol can land on a
smaller boundary. awman hit this on Apple Silicon (WI 0122): a `static` hand-off is
not reliably 64 KiB-aligned there, and the VMM then refuses the image or maps the
wrong range. The example is therefore misleading on macOS.

## Proposed documentation change

In "Alignment and writability", after the sentence recommending `#[repr(align(65536))]`,
add:

> `repr(align)` on a `static` is a request to the linker, and some object formats do
> not honor it. In particular, Mach-O (macOS) caps section alignment below 64 KiB, so
> a `static mut` declared as above can be misaligned there. Portable embedders should
> instead copy the kernel into a leaked, 64 KiB-aligned heap allocation at startup and
> return that pointer. The allocation must be writable (x86_64 maps it as guest RAM)
> and must never be freed or moved while any `Vm` exists.

Add a second example next to the `static mut` one:

```rust
use std::sync::OnceLock;

const ALIGN: usize = 64 * 1024;
static KERNEL_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kernel.bin"));
// Only the address is stored, so the static stays Send + Sync.
static IMAGE: OnceLock<usize> = OnceLock::new();

fn image() -> *mut u8 {
    let addr = *IMAGE.get_or_init(|| {
        // Over-allocate by one alignment unit, then round the start up.
        let buf: &'static mut [u8] =
            Box::leak(vec![0u8; KERNEL_BYTES.len() + ALIGN].into_boxed_slice());
        let start = (buf.as_mut_ptr() as usize).next_multiple_of(ALIGN);
        let off = start - buf.as_mut_ptr() as usize;
        buf[off..off + KERNEL_BYTES.len()].copy_from_slice(KERNEL_BYTES);
        start
    });
    std::ptr::with_exposed_provenance_mut(addr)
}
```

State that `KERNEL_BYTES.len()` must itself be a multiple of 64 KiB (libkrunfw's
bundles are), and that the VMM re-checks alignment against `sysconf(_SC_PAGESIZE)`.
awman's production version is `crates/awman-host-shims/src/kernel.rs`
(`writable_aligned_copy`), which also validates size, guest/entry addresses and the
arm64 `Image` magic before handing the buffer over.

## macOS hypervisor entitlement

Add one note to the crate docs' macOS section: any macOS binary that actually boots a
VM through Hypervisor.framework, including test and example binaries, must be
code-signed with the `com.apple.security.hypervisor` entitlement. Ad-hoc signing
(`codesign --force --sign - --entitlements <plist>`) is enough for local use. Without
it the boot fails at VM creation. awman's plist is
`packaging/macos/awman.entitlements`; Microsandbox's own is `msb-entitlements.plist`.
Re-sign after `strip`, because stripping invalidates the signature.

## Evidence this is needed

- awman WI 0122/0123 notes (`aspec/review-notes/0122-evidence/notes.md`,
  `aspec/review-notes/0123-evidence/notes.md`) record the Mach-O alignment finding on
  Apple Silicon with the vendored typed provider.
- WI 0120 (`aspec/work-items/0120-upstream-embedded-kernel-support.md`, "Upstream
  state") records the upstream example verbatim.
- The Linux ELF behavior and the heap-copy alternative were exercised on aarch64 Linux
  in the WI 0120 kernel-callback step (`writable_copy_is_aligned_and_identical`). The
  Mach-O claim is based on the WI 0122 native observation; **re-confirm it on Apple
  Silicon with the 0.7.8 payload before submitting**, because it was not re-run in
  this environment.

## If a human chooses to submit

Follow the upstream conventions: Conventional Commit title (e.g.
`docs(krun): note Mach-O alignment limit for static-krunfw`), a PR body with
`## TL;DR`, `## Description` and `## Test Plan`, no emojis, `cargo fmt`, and
`cargo doc`/doctest runs for the changed example. Keep the change documentation-only.
