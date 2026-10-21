# Work Item: Task

Title: Replace the vendored embedded-kernel patch with upstream libkrun support
Issue: n/a

Distribution policy (2026-09-29, from
[WI 0123](completed/0123-builtin-native-verification-and-release-closure.md);
supersedes the 2026-09-28 "no signing" policy): the Apple Silicon artifact is
**ad-hoc signed** with exactly the `com.apple.security.hypervisor` entitlement
(`packaging/macos/awman.entitlements`, `tools/macos-sign.sh`,
`make sign-macos`). There is no Developer ID identity, notarization, Apple
distribution account or signing secret. Any binary that boots a VM on macOS,
including upstream test and example binaries, needs the same ad-hoc
entitlement signature. Native boot and final-artifact checks still apply,
and failures must be reported honestly.

Last reconciled: 2026-10-21, against completed WIs 0119 and 0121–0127,
open WI 0124, and upstream as listed in "Upstream state" below.

## Summary:
- WI 0119/0121 delivered the builtin runtime on a vendored
  `msb_krun 0.1.39` patch (a typed, process-lifetime `EmbeddedKernel`
  registry). WI 0122/0123 then booted that patch natively on Apple Silicon and
  shipped it as `builtin-experimental` (macOS/aarch64 only). Linux KVM boots
  are still unproven and belong to optional
  [WI 0124](0124-builtin-linux-and-docker-gates.md).
- **Upstream changed after this item was written.** libkrun PR #144 (commit
  `84ca483a9511a9dddb1c411d7cdc8cc76dec8bc6`, merged 2026-09-24, published in
  `msb_krun 0.1.40` and `0.1.41`) adds an opt-in `static-krunfw` Cargo
  feature. With it, the embedding program supplies the kernel through a
  link-time C-ABI `krunfw_get_kernel(path, guest_addr, entry_addr, size)`
  callback instead of `dlopen`-ing libkrunfw. That covers this item's core
  need (kernel bytes owned by the application, no firmware shared library),
  with a different shape from the one proposed here (see "Upstream state").
- This item's goal is now: **remove the vendored `msb_krun` patch by adopting
  a published upstream kernel-provider API**, keeping the validated strict
  runtime. The primary route (Track A) is to adopt `static-krunfw`, which
  needs no libkrun fork or PR. An additive, safe, typed Rust provider API
  upstream (Track B) is an optional follow-up, pursued only if Track A shows a
  concrete gap or the maintainers want it.
- Microsandbox SDK/runtime integration stays a separate concern. Microsandbox
  does not forward `static-krunfw`, but Cargo feature unification lets
  awman's direct `msb_krun` dependency enable it for the whole graph. A
  Microsandbox PR is needed only if that proves insufficient. Do not describe
  the feature as requiring a fork of the msb application.
- Adopting `msb_krun >= 0.1.40` requires Microsandbox `>= 0.7.7`
  (0.7.3–0.7.6 pin `=0.1.39`). The SDK, network, types and filesystem patches
  carried by WI 0119/0121 must therefore be rebased as part of this item. That
  dependency bump is the largest piece of work here.
- No fork URL has been supplied, and no awman-owned fork of
  `superradcompany/libkrun` or `superradcompany/microsandbox` exists publicly
  (checked 2026-10-21). Prepare local, reviewable changes and PR material
  without creating forks or branches, pushing, publishing or submitting a PR
  unless the user explicitly authorizes it.

## User Stories

### User Story 1:
As a: developer embedding Microsandbox's VM stack

I want to: pass a validated kernel payload directly from my application

So I can: ship a single executable without extracting a firmware library or
relying on application-specific loader hacks.

### User Story 2:
As a: upstream maintainer

I want to: receive only changes that are additive, focused and tested, and
that preserve existing defaults

So I can: support embedding without taking on awman-specific behavior or
breaking existing firmware-library consumers.

### User Story 3:
As a: awman maintainer

I want to: replace the local dependency patch with a pinned upstream version

So I can: reduce divergence while retaining the validated strict runtime.

## Upstream state (verified 2026-10-21)

| Repository | State | Relevance |
|---|---|---|
| `superradcompany/libkrun` | HEAD `b35c7e118d7a1e48c8fad103b258e64ff5c49147`; workspace version `0.1.41` (bump `e39792c`, 2026-10-05; crates.io 2026-10-08). `0.1.40` published 2026-10-02. No `AGENTS.md`/`CONTRIBUTING.md` at root. | `static-krunfw` (#144) is in both 0.1.40 and 0.1.41. |
| `superradcompany/microsandbox` | Latest tag `v0.7.8` pins `msb_krun =0.1.41`; `v0.7.7` pins `=0.1.40`; `v0.7.3`–`v0.7.6` pin `=0.1.39`. main HEAD `59d14ba`. Has `AGENTS.md` (no branches/commits/pushes/PRs without explicit human request) and `CONTRIBUTING.md` (conventional commits, scoped PRs with test plan). | No crate forwards `static-krunfw`. There is no isolated builder, `strict_sni` or `MSB_EMBED_ARTIFACTS_ROOT` upstream yet, so those patches remain. `vendor/libkrunfw` is a submodule; its kernel may differ from the payload pinned in `third_party/msb-payloads/manifest.toml`. |
| `sqlx-sqlite` | Latest release is still `0.9.0`. | The SQLx bound backport remains required; it is independent of this item. |

What `static-krunfw` provides (`src/krun/src/lib.rs` crate docs,
`src/krun/src/api/vm.rs` `load_krunfw`/`load_krunfw_library`):
- A compile-time feature that replaces dynamic loading for the entire build
  graph. There is no per-VM or runtime choice between embedded and DSO.
- The embedder defines `krunfw_get_kernel(const char *path, uint64_t
  *guest_addr, uint64_t *entry_addr, size_t *size) -> char *`. `path` is the
  NUL-terminated `KernelBuilder::krunfw_path` value, or NULL if unset. A NULL
  return is a hard build error ("no krunfw image for the requested path"),
  with no fallback.
- The callback is resolved by the static linker, not by `dlsym`. It needs no
  `--export-dynamic`, custom section or fake library. It is a `#[no_mangle]`
  global symbol in the executable, though, and on Mach-O executables globals
  appear in the export trie.
- Documented requirements: the image must be 64 KiB-aligned and **writable**
  (it is mapped into guest memory), and must stay valid for the life of the
  `Vm`, in practice the process.
- The upstream doc example uses `#[repr(align(65536))] static mut`. WI 0122
  found that Mach-O caps section alignment below 64 KiB, so that example is
  misaligned on macOS. A small upstream documentation fix is worthwhile; awman
  must not rely on it.

This differs from the original design goals below. Upstream chose an exported
C symbol plus a raw-pointer return, both of which this item had told
implementers to avoid. Maintainers have accepted that shape, so a competing
typed API needs a concrete justification (Track B).

## Implementation Details:

### Proven starting point

- Current carried baseline: `msb_krun 0.1.39`, libkrun commit
  `2bd0f84ad0956f3032e0490d3b8512b6851eca12`
  (`awman::data::builtin_runtime::LIBKRUN_COMMIT`), used by Microsandbox
  `v0.7.2` (`60d4dc8a436fb9365491567ec21d073e924e3c6d`; version constant
  `define_builtin_runtime_version!("0.7.2")` in `src/data/builtin_runtime.rs`).
- Vendored patch: `third_party/msb_krun-0.1.39/upstream.diff` (SHA-256
  `cf6d58d0574d8bfefad6137b3f6435ec5524dbf11e8f6b24ff2133c8610dcfba`, 3
  files). `load_krunfw_library` selects the registered `EmbeddedKernel` when
  the firmware path canonicalizes to the current executable or equals
  `@embedded-kernel`; other paths keep the DSO behavior.
- Awman's provider: `crates/awman-host-shims/src/kernel.rs` (moved from
  `src/engine/container/builtin/embedded/kernel.rs` by WI 0126 D1). It
  registers the build-verified kernel (`include!` of `OUT_DIR/msb_kernel.rs`
  from `crates/awman-host-shims/build.rs`). Linux aarch64 hands over the
  aligned static. x86_64 Linux and macOS hand over a leaked, writable,
  64 KiB-aligned heap copy, because x86_64 maps the buffer as guest RAM
  (`MmapRegion::build_raw`) and Mach-O does not honour the static's alignment.
  **The registry's `bytes: &'static [u8]` is therefore unsound as an API
  contract on x86_64,** where the guest writes through it. Any replacement
  must express a writable, owned buffer.
- Native evidence for the **vendored typed patch** (not only the spike):
  Apple Silicon booted actual awman guests in WI 0122 and passed WI 0123's
  hardware, clean-checkout and signed-artifact tiers
  ([WI 0122 notes](../review-notes/0122-evidence/notes.md),
  [WI 0123 notes](../review-notes/0123-evidence/notes.md)). The macOS kernel
  is SHA-256 `c3f1897b2ac706450a178700072b43ecb648041c9e88dd26ba8937c37649e24b`,
  24,576,000 bytes, load/entry `0x80000000`, byte-identical to the Linux
  aarch64 kernel. Linux ARM64 reached KVM initialization only, and Linux
  x86_64 has not been executed. Neither is a boot pass, and both are deferred
  to WI 0124.
- The strict-embed spike
  (`tools/oci-runtime-spike/strict-embed/embedded-loader.patch`, sentinel,
  self-as-firmware path, exported getter, `Library::this()` lookup) remains
  historical evidence only.

### Phase 1: decide the upstream route

1. Read the target repositories' contribution rules (microsandbox `AGENTS.md`,
   `CONTRIBUTING.md`, `DEVELOPMENT.md`; libkrun's README/CODEOWNERS and recent
   PR conventions). Work in separate upstream checkouts and leave awman's
   unrelated changes untouched. Record the upstream bases used.
2. **Track A (default): adopt `static-krunfw`.** Confirm, in a spike against
   Microsandbox `v0.7.8`/`msb_krun 0.1.41`, that:
   - enabling `msb_krun/static-krunfw` from awman's direct dependency (only
     under the `builtin-runtime` feature) unifies across
     `microsandbox-runtime`, `-image`, `-network` and `-cli`, and that nothing
     in the in-process SDK/CLI path still requires `dlopen`;
   - an awman-owned callback in `crates/awman-host-shims/src/kernel.rs` can
     replace the registry. The callback must: accept only the firmware path
     awman configures (`microsandbox::set_libkrunfw_path(&executable)` in
     `src/engine/container/builtin/embedded/mod.rs`, enforced in
     `msb_driver.rs`); return NULL for any other or NULL path, which upstream
     turns into an explicit error; and return a leaked, writable, 64 KiB-aligned
     copy on every target, which removes the per-target split;
   - the symbol survives optimized/LTO/stripped and signed builds, and no
     `--export-dynamic`, custom section or extra helper is introduced.
     Record honestly that the binary now has one global C symbol.
   Confine the `#[unsafe(no_mangle)] unsafe extern "C"` function under
   `#![deny(unsafe_code)]` with an exact `// SAFETY:` contract (pointer
   validity of out-parameters, NUL-terminated `path`, process-lifetime
   buffer, single image), as `version.rs` does for its metadata static. The
   `awman` library keeps `forbid(unsafe_code)`.
3. **Track B (optional): typed provider upstream.** Pursue it only if Track A
   reveals a concrete gap: for example a need for runtime coexistence of
   embedded and DSO kernels in one build, an unacceptable symbol, or
   reviewer interest. Then propose an additive, safe API over an owned,
   writable, aligned buffer with explicit guest load/entry addresses (for
   example a `KernelBuilder` provider). Specify alignment, length, lifetime,
   guest-address bounds, overflow and per-architecture constraints. It must
   coexist with, not replace, `static-krunfw` and the DSO path. Open an
   upstream discussion or issue before writing the PR, since #144 already
   settled the maintainers' preferred shape.
4. Either track must reject invalid or conflicting configuration explicitly
   and never fall back to a dynamic library. Preserve installed-runtime and
   no-embedded-source behavior for non-awman consumers.

### Phase 2: dependency rebase (Track A prerequisite)

1. Bump the pinned Microsandbox crates from `0.7.2` to the chosen release
   (`>= 0.7.7`; prefer the latest, currently `0.7.8`) and `msb_krun` to the
   matching `=0.1.4x`. Update the single version constant and
   `LIBKRUN_COMMIT` in `src/data/builtin_runtime.rs` (WI 0126 C3), and the
   worker protocol string derived from it.
2. Rebase each carried Microsandbox patch onto the new published crate, with
   new archive and patch SHA-256s and `PATCH.md` provenance: SDK
   `build_lazy_isolated`, network `strict_sni`/exact DNS binding/
   `host.builtin.internal` alias, types `strict_sni` transport, and the
   filesystem `MSB_EMBED_ARTIFACTS_ROOT` lookup. Drop any patch whose behavior
   has landed upstream; as of 2026-10-21, none has.
3. Re-check the SQLx bound backport against the new graph. It stays separate
   from this item and must not be coupled to it.
4. Compare the new release's `vendor/libkrunfw` kernel with
   `third_party/msb-payloads/manifest.toml`. Re-pin and re-verify payloads
   (`tools/msb-payloads/`) only if the kernel or guest agent changes.
5. Re-validate SDK behavior that awman relies on and that may have moved
   between 0.7.2 and 0.7.8: the launch contract and installed-config refusal,
   `AMBIENT_MSB_OVERRIDES` (`src/data/config/env.rs`, all 13 variables), the
   lifecycle-lock mode, no-follow bind roots, and attach/exec protocol.

### Phase 3: adoption and patch removal

- Pin the published crate versions rather than a floating branch. A
  fork-only commit is still a downstream dependency, not evidence that a
  change has landed upstream.
- In one focused awman change, replace the `EmbeddedKernel` registration with
  the Track A callback (or Track B API), remove `msb_krun` from
  `[patch.crates-io]`, delete `third_party/msb_krun-0.1.39/`, update
  `third_party/README.md` and regenerate the lockfile. Keep
  `set_libkrunfw_path(executable)` while the SDK still requires a real
  firmware path. It becomes the callback's image selector.
- Re-run on Apple Silicon: the WI 0123 hermetic, feature and hardware tiers
  (`make test-builtin`), clean-checkout CI, and the ad-hoc-signed optimized
  artifact boot and scans (`tools/release-artifact-check.sh`), plus the strict
  fixture (image/mount/config assertions, exit status, same-executable worker
  identity, minimal-PATH/offline execution, no extracted helpers). Linux
  ARM64/x86_64 KVM boots stay WI 0124 gates. Report them as blocked or
  deferred if no hardware is available, never as passes.
- Retain kernel/guest-agent provenance and source/notice obligations;
  upstreaming does not remove them.
- Record the upstream references and patch-removal result here and in the
  [evidence register](../review-notes/0121-evidence-register.md). If adoption
  is blocked, leave the tested local patch documented with an explicit
  maintenance decision.

### Downstream patch inventory and migration

Six published crates are patched (`[patch.crates-io]` in `Cargo.toml`), plus
awman's own integration glue. Each item has a separate replacement gate, and
replacing one does not mean the others can be removed. Hashes were checked
against the tree on 2026-10-21.

| Carried change | Exact current scope | Replacement | Removal gate |
|---|---|---|---|
| SQLx SQLite bound backport | `third_party/sqlx-sqlite-0.9.0/` (patch SHA-256 `22a7e9a2e4f9dbf588a514e5305dfd6f21377107bb7600c348660606d664031f`, 2 files). Raises the `libsqlite3-sys` upper bound from `<0.38.0` to `<0.39.0`, matching upstream SQLx commit `94aafe3a68884d923b0798a767c8d7f6cfda89d2`, which resolves the `rusqlite 0.40.2` / `libsqlite3-sys 0.38.2` native-links conflict. Also adds one `#[allow(deprecated)]` for `time >= 0.3.55` format components. Independent of embedded-kernel work. | A published compatible `sqlx-sqlite` release (latest is still 0.9.0). | Update the dependency and lockfile, remove the patch path and vendor tree, then pass the combined awman/msb link, exactly-one-bundled-SQLite check, awman store tests, msb migrations, cross-driver checks, and old/new catalog round trips on supported targets. Do not couple this to libkrun work. |
| libkrun kernel-provider loader | `third_party/msb_krun-0.1.39/` (patch SHA-256 `cf6d58d0574d8bfefad6137b3f6435ec5524dbf11e8f6b24ff2133c8610dcfba`): the typed process-lifetime `EmbeddedKernel` registry and loader branch in `load_krunfw_library`. External firmware paths keep the DSO behavior. No exported C getter and no process-symbol search. Booted natively on Apple Silicon (WI 0122/0123); Linux is unbooted. | **Track A:** published `msb_krun >= 0.1.40` with `static-krunfw` and an awman-owned `krunfw_get_kernel` callback. **Track B:** an accepted typed provider API. | Requires the Microsandbox `>= 0.7.7` bump (Phase 2). Adapt `crates/awman-host-shims/src/kernel.rs`, pin the crate, then pass the Apple Silicon gates in Phase 3 and artifact scans. Linux KVM boots are WI 0124 gates. Remove the vendored tree only after those pass. |
| Microsandbox guest-agent artifact-root glue | `third_party/microsandbox-filesystem-0.7.2/` (patch SHA-256 `b87c1e5f67fe0b9b3044bc43ff332601233280012bd35ea4145b6b75b9feab61`). Only `build.rs` changes: it adds target-aware lookup at `MSB_EMBED_ARTIFACTS_ROOT/<target-arch>/agentd`, and existing inputs remain. Upstream main still has only `MSB_EMBED_ARTIFACTS_DIR`. | An equivalent upstream target-aware build input, or awman setting `MSB_EMBED_ARTIFACTS_DIR` per target in its own build. | Pin the release/commit, remove the vendor patch and path override, and verify clean builds for all required targets plus guest-agent hash/architecture checks. Must be rebased if Phase 2 runs first. |
| Awman-side worker and provider integration (not a vendored dependency patch) | `crates/awman-host-shims/src/{worker.rs,kernel.rs,version.rs}` (moved by WI 0126 D1, built behind `cfg(awman_builtin)`), `src/engine/container/builtin/embedded/mod.rs` (`set_libkrunfw_path`), `src/engine/container/builtin/msb_driver.rs` (firmware-path check), constants in `src/data/builtin_runtime.rs`, and the single early call `awman_host_shims::dispatch_worker(...)` in `src/main.rs`. Together these compile and register the selected kernel, embed worker protocol metadata, recognize the private same-executable `machine` invocation, and configure the SDK paths used to reach the provider. | Track A replaces registration with the callback. Remove the firmware-path transport only when the SDK can represent an embedded origin directly. Keep the worker protocol and early dispatch while the SDK still launches the same executable. Scope any needed Microsandbox runtime patch as its own upstream change and record it here before carrying it. | Test worker launch/version compatibility first, then remove only the glue the upstream API makes redundant. Verify same-executable boot, installed-runtime refusal, protocol compatibility, and no symbol/path workaround beyond the one documented Track A callback. |
| Awman-side macOS builtin fixes (WI 0122; awman code, not dependency patches) | `crates/awman-host-shims/src/kernel.rs` uses the aligned process-lifetime copy on macOS (Mach-O caps section alignment below 64 KiB). `crates/awman-host-shims/src/worker.rs` accepts the SDK's umask-mode lifecycle lock (no group/other write). `src/engine/container/builtin/msb_driver.rs` canonicalizes bind-source ancestors, because the SDK's no-follow root open refuses macOS `/tmp` and `/var` symlinks; a symlinked leaf is still refused. The fourth WI 0122 fix (`only_lo` probe tolerates the kernel's inert `dummy0`) is test-only and has no upstream counterpart. | Upstream: a kernel provider that owns alignment (Track A keeps the copy in awman), an SDK lock created `0600`, and SDK root resolution that tolerates system symlink ancestors while refusing a symlinked leaf. | Remove each only after the matching upstream change is pinned and the macOS actual-awman guest tests pass again. All three are platform-neutral and must be revalidated on Linux in WI 0124. |
| Awman-side Apple Containers XPC transport (WI 0122; not a vendored dependency patch, no upstream code carried) | `crates/awman-host-shims/src/apple_xpc.rs` (macOS only; moved by WI 0126 D1), adapted by `crates/awman-cli/src/apple_transport.rs` (`NativeXpc`), and registered at startup in `src/main.rs` through `engine::oci::apple_store::install_transport`. Unsafe scope: public libxpc/libSystem symbols (`xpc_connection_*`, `xpc_dictionary_*`, `xpc_get_type`, `xpc_retain`/`xpc_release`, type/error singletons) and one capture-free global block with `_NSConcreteGlobalBlock` isa. No Swift toolchain, framework, DSO or executable is added. The protocol (keys, routes, release pins by version and full commit) lives in the safe library (`src/engine/oci/apple_store.rs`, contract in `crates/awman-host-shims/src/xpc.rs`). Provenance is `apple/container` tags 0.12.0/1.4.1/1.5.0, each natively validated, recorded in [WI 0122 notes](../review-notes/0122-evidence/notes.md). Unrelated to libkrun. | A supported, versioned public client API from Apple (e.g. a C-ABI or documented XPC protocol version). | Replace the transport, keep the library's release gate and output checks, and rerun the native `apple_native_*` tests and template exports on Apple Silicon. Each new Apple release needs a source comparison and pin entry before it is accepted. |
| Microsandbox isolated configuration builder (WI 0121 downstream patch) | `third_party/microsandbox-0.7.2/` vendors published `microsandbox 0.7.2` (crate SHA-256 `0fda3a76b3754d8eb10a5f9b6267a57517b9231eb2fe8254ab175df8a80fa8c8`, Apache-2.0, upstream revision `60d4dc8a436fb9365491567ec21d073e924e3c6d`). Its one-method [`upstream.diff`](../../third_party/microsandbox-0.7.2/upstream.diff) (SHA-256 `7cd71253c1d0c7d2b13e90e957019eec1c3c4aea0929f0a5a0c254bbd86d6cb4`) adds `LocalBackendBuilder::build_lazy_isolated(self) -> LocalBackend` using `build_lazy_from(GlobalConfig::default())`. Not upstream as of main `59d14ba`. See [`PATCH.md`](../../third_party/microsandbox-0.7.2/PATCH.md). | A pinned upstream SDK release with equivalent isolated-builder semantics. A small, self-contained candidate for a separate Microsandbox PR. | Update the dependency and lockfile, remove the path patch and vendor tree, then rerun hostile installed-config tests, SDK runtime origin/path checks, native installed-runtime coexistence and guest boots on Apple Silicon (Linux KVM in WI 0124). Must be rebased if Phase 2 runs first. Do not claim native coexistence from a feature compile alone. |
| Microsandbox visible-SNI network enforcement (WI 0121) | `third_party/microsandbox-network-0.7.2/`, published crate SHA-256 `be4f1c9d36c1957b35d674a993a59c5db1741f47c96c952b491b09d9674b92d7`, Apache-2.0, same upstream revision. Seven files: six policy files add default-off `strict_sni` to config/builder/poll/TCP proxy and tighten suffix policies to require the exact claimed name's DNS/IP binding, and the guest host alias is renamed to `host.builtin.internal`. [`upstream.diff`](../../third_party/microsandbox-network-0.7.2/upstream.diff) SHA-256 `63cceba410585743c1b5afcb71a5e6d0d96a1a7de0ed2f31b3080e044ee5d4bd`; [provenance and boundary](../../third_party/microsandbox-network-0.7.2/PATCH.md). Ordinary strict opaque-TLS refusal is unchanged unless explicitly opted in, and suffix-binding tightening applies in every mode. | A pinned upstream release with explicit visible-SNI enforcement and exact DNS binding without TLS interception. The alias rename stays awman-specific unless upstream accepts a configurable alias. | Remove alongside the types patch after actual SDK TLS positive/negative, missing/wrong/shared-IP/sibling-binding, config roundtrip and native non-root guest network tests pass on Apple Silicon (Linux KVM in WI 0124). Do not infer encrypted HTTP authority or ECH inner-name enforcement, or native packet routing, from host-side TLS tests. Must be rebased if Phase 2 runs first. |
| Microsandbox network option transport (WI 0121) | `third_party/microsandbox-types-0.7.2/`, published crate SHA-256 `8f705cfd6b163fc5b00987c6cf6143b5ceed311b097e68926822b2d6daa4cfee`, Apache-2.0, same upstream revision. Three Rust files add default-false `strict_sni` to local/cloud NetworkSpec and preserve both conversions, so the SDK builder cannot silently drop the network option. [`upstream.diff`](../../third_party/microsandbox-types-0.7.2/upstream.diff) SHA-256 `30bf86e5a7e58acbe889d3d05eb898795b3078dedc608d0435b252196c6eed4f`; [provenance](../../third_party/microsandbox-types-0.7.2/PATCH.md). | The paired upstream networking release must carry the explicit option through all configuration conversions. | Remove with the network patch only after actual SandboxBuilder NetworkConfig→NetworkSpec→NetworkConfig roundtrip, backward-compatible defaults and SDK/native enforcement tests pass. No enforcement by an unpatched cloud server is claimed. Must be rebased if Phase 2 runs first. |

WI 0121's completion follow-up also added direct manifest edges (normal Rustls
for the shared API/squad TLS server's ring provider, a feature-gated
`microsandbox-network =0.7.2` edge for SDK policy-evaluator tests, and
feature-gated `sqlx =0.9.0` with SQLite/Tokio for driver transaction tests).
These are awman manifest changes, not patches. The pinned `=0.7.2` edges move
with the Phase 2 bump.

The carried sources and their patch notes are listed in
[`third_party/README.md`](../../third_party/README.md) and each tree's
`PATCH.md`. No patch has been accepted or released upstream. The historical
spike under `tools/oci-runtime-spike/` remains evidence only and is not a
production runtime dependency.

### Acceptance criteria

- [ ] The upstream route is decided and recorded (Track A by default; Track B
  only with a recorded, concrete gap), with the upstream bases used.
- [ ] The Microsandbox/msb_krun dependency is bumped to a release containing
  the chosen API, and every carried Microsandbox patch is rebased (or dropped
  because it landed upstream) with new provenance and hashes.
- [ ] The vendored `msb_krun` patch is removed. Kernel provision uses the
  published API with explicit validation, a writable aligned buffer kept for
  the process lifetime, and no hidden fallback to a dynamic library.
- [ ] Any remaining unsafe code is confined to `awman-host-shims` with exact
  `// SAFETY:` contracts. The `awman` library keeps `forbid(unsafe_code)`.
- [ ] Apple Silicon gates (WI 0123 tiers, signed optimized artifact,
  strict fixture) pass on the adopted API. Linux ARM64/x86_64 KVM boots are
  recorded as WI 0124 gates, never as passes without hardware evidence.
- [ ] Any upstream PR (libkrun docs fix, Track B API, Microsandbox isolated
  builder/filesystem/network changes) is separately scoped and tested, and is
  submitted only with explicit authorization. Acceptance is tracked here, not
  assumed.
- [ ] `third_party/README.md`, the evidence register and this inventory agree
  with the tree after adoption.

## Edge Case Considerations:
- Empty, truncated, oversized, wrong-architecture or improperly aligned kernel
  data; invalid entry/load addresses; range overflow and incompatible initramfs.
- `krunfw_get_kernel` called with NULL, an unexpected path, non-UTF-8 bytes,
  or more than once per process, and concurrent VM construction in one
  process. The callback must be idempotent and return the same buffer.
- A read-only or misaligned buffer given to a VMM that maps it as guest RAM
  (x86_64), and Mach-O alignment caps on `static` data.
- Feature unification: `static-krunfw` silently changes every `msb_krun`
  consumer in the graph. Confirm that no in-process SDK/CLI path still
  expects a libkrunfw DSO, and that non-builtin builds do not pull the feature.
- Optimization/LTO/dead-stripping, `strip`, ad-hoc re-signing, Linux target
  ABI and differing host page sizes. Test the real payload layout, not only a
  toy byte slice.
- Microsandbox `0.7.2 → 0.7.8` drift: launch-contract, worker protocol,
  database migrations, network/types schema and default changes that
  invalidate rebased patches or old/new catalog compatibility.
- Upstream API/version changes, reviewers preferring another provider shape,
  or a separate msb glue change being needed after adoption.

## Test Considerations:
- Unit tests: callback path validation, NULL/unknown-path refusal,
  idempotence, buffer alignment and writability, and configuration
  precedence/conflicts.
- Build/link tests: Apple Silicon and Linux ARM64/x86_64 builds with
  `builtin-runtime` (symbol present, no `--export-dynamic`), optimized/LTO and
  stripped builds, and unchanged builds without the feature (no
  `static-krunfw` in the graph).
- Real-hardware tests: native guest boot, clean shutdown, repeated launches
  and error paths with the pinned kernel and fake workloads. Do not execute an
  agent on the host, use real credentials, or require paid provider access.
- Re-run awman's strict fixture and WI 0123 tiers. The vendored patch's
  passing results do not substitute for validating the adopted API.
- For any upstream PR, run that repository's relevant tests and
  contribution gates, plus cross-version protocol fixtures for any msb
  integration change.

## Codebase Integration:
- Awman: `crates/awman-host-shims/` (kernel provider, worker, version
  metadata), `src/engine/container/builtin/{embedded/mod.rs,msb_driver.rs}`,
  `src/data/builtin_runtime.rs`, `src/data/config/env.rs`, root `Cargo.toml`
  (`[workspace.dependencies]`, `[patch.crates-io]`, `builtin-runtime`
  feature), `third_party/`, `tools/msb-payloads/`.
- Upstream libkrun: `src/krun/src/lib.rs` (`static-krunfw` docs),
  `src/krun/src/api/{vm.rs,builders.rs}`. Follow that repository's public API,
  ownership, formatting and test patterns.
- Upstream Microsandbox (separate, optional PRs): `sdk/rust/lib/backend/local/`
  (isolated builder), `crates/filesystem/build.rs` (artifact root),
  `crates/network`, `packages/microsandbox-types`. Do not expand any libkrun
  PR to include awman's runtime implementation.
- References: WI 0119, 0121, 0122, 0123, 0126 D1/C2/C3, WI 0124, and
  `tools/oci-runtime-spike/strict-embed/README.md` (historical).

## Documentation

- For any upstream PR, provide API docs and a minimal embedding example that
  state ownership, supported boot modes/platforms and OS prerequisites
  (including the macOS hypervisor entitlement and the Mach-O alignment caveat).
- Keep fork/patch maintenance, review status, reproducibility and migration
  details in this item, `third_party/` and developer material, not in a
  user-facing WI guide.
- Update awman user documentation only if adoption changes a user-visible
  prerequisite or behavior. Upstream patch mechanics alone do not warrant a
  new end-user guide.
