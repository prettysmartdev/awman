**Title:** `docs(static-krunfw): repr(align(65536)) static example is misaligned on macOS (Mach-O)`

---

## Summary

The `static-krunfw` crate docs (`src/krun/src/lib.rs`, "Static kernel" example, as of
`b35c7e1` / `msb_krun 0.1.41`) declare the kernel as:

```rust
#[repr(align(65536))]
struct Aligned([u8; 0x10_0000]);
static mut KERNEL: Aligned = Aligned([0; 0x10_0000]);
```

The image is then returned from `krunfw_get_kernel`. The same docs require a
64 KiB-aligned, writable image. On Linux (ELF) the `align(65536)` request is
honoured. On macOS, the Mach-O linker caps section alignment below 64 KiB, so this
`static` can land on a smaller boundary. The VMM then refuses the image or maps the
wrong range.

## How we found it

We embed libkrun via Microsandbox in [awman](https://github.com/prettysmartdev/awman) and use
`static-krunfw` to link the kernel into the binary. On Apple Silicon a
`static`-backed hand-off wasn't reliably 64 KiB-aligned. We switched to a leaked,
64 KiB-aligned heap copy made at startup, and that works on every target we've tried.

## Suggested fix

This is a documentation change only, with no API change:

- Note in "Alignment and writability" that `repr(align)` on a `static` is a request to
  the linker, and that Mach-O caps it below 64 KiB.
- Add a portable example: copy the kernel into a leaked, 64 KiB-aligned heap buffer
  once, behind a `OnceLock`, and return that pointer. The buffer stays writable,
  which x86_64 needs because it maps the image as guest RAM, and it is never freed.
- Mention that any binary that boots a VM on macOS needs the
  `com.apple.security.hypervisor` entitlement.

We have the wording and example ready and are happy to open a PR.
