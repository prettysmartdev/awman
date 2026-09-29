# WI 0122 native evidence

Final sweep recorded 2026-09-29 on Apple Silicon. Every log under [`final/`](final/)
was produced from one frozen tree: base `105b5cbd7cb5506e16e22d9766e0905c0de72b65`
plus the WI 0122 code patch whose SHA-256 is recorded in each log's first line.
The patch covers `src/`, `tests/`, `tools/`, the payload manifest, build
inputs and the new files. When the patch is committed unchanged, that commit is
the evidence revision.

## Host, service and toolchain

| Item | Value |
|---|---|
| Host | macOS 26.6.2 (25G83), arm64, `kern.hv_support=1` |
| Installed Apple Containers | 0.12.0 release, commit `651811cc090937457956643dd2c454df77eb141b` |
| Disposable releases | 1.5.0 (`d265d669…`), 1.4.1 (`9a8917ca…`), 1.3.1 (`a9a62e28…`): Apple-signed, notarized `.pkg`s expanded into private install roots with private app roots. No system install, and the user's store was never touched. |
| Toolchain | Apple clang 21.0.0, Swift 6.3.1 (not used by the build); Rust per `rust-toolchain.toml` |

## Contract

The tagged sources of 0.12.0, 1.4.1 and 1.5.0 agree on everything awman uses:
`ImageServiceXPCKeys`, `ImageDescription`, the `list`/`save` harness handlers,
the route/error keys in `XPCMessage`, and `XPCServer`'s euid-only peer check.
Only the `apiServerVersion` spelling differs (0.x and 1.3.x use a long form;
1.4+ use a bare `X.Y.Z`). The bridge parses both, then pins version plus full
commit plus `release` build.

## Results (all at the frozen tree)

| Evidence | Log | Result |
|---|---|---|
| Hermetic gates: fmt, clippy (default and builtin-runtime), architecture lint, full suite `--no-fail-fast`, feature lib and bin tests | [hermetic.log](final/hermetic.log) | PASS, except two `network_probe_*` host tests that need GNU `timeout`. They fail identically on the base commit and are unrelated. |
| Native bridge on 0.12.0: ping and release pin, list, stop, export round trip, deterministic mid-save cancellation with no leftovers, concurrent importers, helper killed mid-save (`Interrupted`, then a retry that re-pings and succeeds with the same digest) | [native-0.12.0.log](final/native-0.12.0.log) | PASS |
| Every shipped agent template (antigravity, claude, cline, codex, copilot, crush, gemini, maki, opencode) plus the project image, exported through the live store and validated; store index digest recorded next to the validated manifest | [template-exports.log](final/template-exports.log) | PASS (cline, copilot and maki were built from `templates/` with Apple's builder for this run) |
| No helper, CLI or private-store access: the export runs under a `sandbox-exec` profile denying fork, any exec except the test binary itself, and all reads and writes under `~/Library/Application Support/com.apple.container`, with positive controls for both denials | [sandboxed-export.log](final/sandboxed-export.log) | PASS (kernel-enforced; used in place of a root-only dtrace trace) |
| Actual `awman`: `ready` imports the fixture from the Apple store through the bridge. The Apple service is stopped. Cached `ready` succeeds, and a real builtin guest runs the agent with network `none`, an env-cleared host and a private PATH whose `docker`/`container`/`sbx` stubs record any call (none). With nothing cached, the stopped service is reported with the start hint. | [guest-runs.log](final/guest-runs.log) | PASS |
| Apple-exported fixture boots a guest with no usable interface and no routes; DNS and IP egress refused | [guest-runs.log](final/guest-runs.log) | PASS |
| 1.5.0 and 1.4.1: full native bridge suite. 1.3.1: every acquisition refused (`UnsupportedImageSource`) before any image route; the installed 0.12.0 was then restored | [releases.log](final/releases.log) | PASS |

## Defects found and fixed while booting on macOS

The macOS builtin guest had never run natively before. Four latent defects
were found and fixed, each with a regression test:

1. `worker.rs`: the lifecycle-lock probe required mode `0600`, but the SDK
   creates the lock with the process umask (`0644`). It now requires only
   that group and other cannot write.
2. `embedded/kernel.rs`: Mach-O caps section alignment below 64 KiB, so the
   `repr(align(65536))` kernel static was misaligned. macOS now uses the same
   aligned, process-lifetime copy as x86_64.
3. `msb_driver.rs`: the SDK opens bind roots without following any symlink,
   so paths under macOS `/tmp` and `/var` were refused (ENOTDIR). Bind sources
   now have their ancestors canonicalized, and a symlinked final component is
   still refused.
4. `tests/builtin_runtime/network_resources.rs`: `only_lo` expected
   `/sys/class/net` to hold only `lo`. The embedded kernel (the same bytes on
   Linux) always has an inert, down `dummy0`. The probe now requires that no
   non-`lo` interface is up or addressed and that no route exists. A negative
   control with networking enabled reports `usable: eth0 routes:2`.

## Payload

`tools/msb-payloads/fetch.sh aarch64-apple-darwin` ran natively; a bash 3.2
empty-array bug was fixed. The archive and agent match the pinned hashes. The
kernel extracted from `libkrunfw.5.dylib` is SHA-256
`c3f1897b2ac706450a178700072b43ecb648041c9e88dd26ba8937c37649e24b`, 24576000
bytes, load/entry `0x80000000`, byte-identical to the verified Linux aarch64
kernel. The manifest now marks the target `verified` (maintainer-authorized),
and a guest booted with it (above).

## Remaining caveats

- **Entitlement vs. distribution policy.** Hypervisor.framework requires the
  `com.apple.security.hypervisor` entitlement, which only a code signature can
  carry. The guest runs above used locally ad-hoc-signed copies. The current
  policy of no explicit signing step means an unsigned distributed binary
  cannot boot a guest on macOS; resolving that is WI 0123's release decision.
- Wrong-architecture and malformed-output handling is exercised by
  scripted-transport tests and the shared archive validator. The live store
  here holds only arm64 images.
