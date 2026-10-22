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

Before WI 0120 (historical, from WI 0119/0121–0123):

- Baseline: `msb_krun 0.1.39`, libkrun `2bd0f84ad0956f3032e0490d3b8512b6851eca12`,
  Microsandbox `v0.7.2` (`60d4dc8a436fb9365491567ec21d073e924e3c6d`). The vendored
  `third_party/msb_krun-0.1.39/upstream.diff` (SHA-256
  `cf6d58d0574d8bfefad6137b3f6435ec5524dbf11e8f6b24ff2133c8610dcfba`, 3 files) added a
  typed process-lifetime `EmbeddedKernel` registry to `load_krunfw_library`. Its
  `bytes: &'static [u8]` contract was unsound on x86_64, where the guest writes
  through the buffer.
- Native evidence for that vendored patch: Apple Silicon booted actual awman guests in
  WI 0122 and passed WI 0123's hardware, clean-checkout and signed-artifact tiers
  ([WI 0122 notes](../review-notes/0122-evidence/notes.md),
  [WI 0123 notes](../review-notes/0123-evidence/notes.md)). That evidence is for the
  **0.7.2 / 0.1.39 build**. It does not carry over to the adopted build. The 0.7.2
  macOS kernel was SHA-256 `c3f1897b2ac706450a178700072b43ecb648041c9e88dd26ba8937c37649e24b`.
  Linux ARM64 reached KVM initialization only and Linux x86_64 was never executed;
  both are deferred to WI 0124.
- The strict-embed spike (`tools/oci-runtime-spike/strict-embed/embedded-loader.patch`)
  remains historical evidence only.

Result of this work item (2026-10-21, aarch64 Linux build host without `/dev/kvm`):

- **Route: Track A.** Published `msb_krun =0.1.41` (libkrun
  `b35c7e118d7a1e48c8fad103b258e64ff5c49147`, which contains PR #144 commit
  `84ca483a9511a9dddb1c411d7cdc8cc76dec8bc6`) with its `static-krunfw` feature,
  enabled only by awman's `builtin-runtime` features (`Cargo.toml`,
  `crates/awman-host-shims/Cargo.toml`). Awman defines `krunfw_get_kernel` in
  `crates/awman-host-shims/src/kernel.rs`. Track B (typed provider API) was not
  needed and was not pursued.
- **Dependency bump:** Microsandbox `=0.7.8` (tag `v0.7.8`,
  `7b7b9dc89e9a1c77801918f7833c3381d1754579`); `version` and `LIBKRUN_COMMIT` in
  `src/data/builtin_runtime.rs` updated, worker protocol `0.7.8/18/awman-1`.
- **Patch removal:** `third_party/msb_krun-0.1.39/` is deleted and `msb_krun` is no
  longer in `[patch.crates-io]`. The old diff does not apply to 0.1.41 (two `vm.rs`
  hunks fail) and is no longer needed. No `libloading` symbols remain in the linked
  builtin binary, and there is no dynamic-library fallback.
- **Kernel callback:** validates size (non-empty, <= 64 MiB, 64 KiB multiple), 64 KiB
  guest alignment, address overflow, entry in range and the arm64 `Image` header; hands
  over a leaked, writable, 64 KiB-aligned heap copy on every target; matches the
  firmware path byte-exactly against the executable path (`set_libkrunfw_path` is now
  the selector). `unsafe` is limited to `awman-host-shims`; the `awman` library keeps
  `forbid(unsafe_code)`. Linking evidence: `krunfw_get_kernel` is a `T` symbol in debug
  and plain `--release` builds of the builtin binary and absent from `.dynsym`. It
  disappears after `strip` or under fat LTO without affecting behavior, so artifact
  checks must not grep for the name there.
- **Payload re-pin:** the v0.7.8 release's `vendor/libkrunfw` is
  `21f169f7e94798c8916315f8ac6f5999d5b88566` (Linux 6.12.111; 0.7.2 was `cf4c22b…`),
  so the kernel and guest agent changed. Linux aarch64 is re-pinned and verified:
  kernel SHA-256 `cf6cb036c7b40923588950cc8f80750617114243730668d6859216de4d52ee39`,
  24,576,000 bytes, load/entry `0x80000000`. Darwin aarch64 and Linux x86_64 are
  **unverified** until extracted on native hosts, so an Apple Silicon `make build`
  selects the non-builtin build until the Darwin payload is verified.
- Evidence and commands are in the
  [evidence register](../review-notes/0121-evidence-register.md#wi-0120-adoption-record-2026-10-21).
  Not run here, and therefore **not passes**: Apple Silicon hardware boots, the
  ad-hoc-signed optimized artifact boot and scans, and the strict fixture (BLOCKED:
  no Apple Silicon host); Linux ARM64/x86_64 KVM boots (WI 0124 gates; `/dev/kvm`
  absent).

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

Five published crates are patched (`[patch.crates-io]` in `Cargo.toml`: SQLx plus
four Microsandbox 0.7.8 trees), plus awman's own integration glue. The vendored
`msb_krun` patch was removed by this work item. Each item has a separate
replacement gate, and replacing one does not mean the others can be removed. Hashes
were checked against the tree with `sha256sum` on 2026-10-21 after the 0.7.8 rebase.

| Carried change | Exact current scope | Replacement | Removal gate |
|---|---|---|---|
| SQLx SQLite bound backport | `third_party/sqlx-sqlite-0.9.0/` (patch SHA-256 `22a7e9a2e4f9dbf588a514e5305dfd6f21377107bb7600c348660606d664031f`, 2 files). Raises the `libsqlite3-sys` upper bound from `<0.38.0` to `<0.39.0`, matching upstream SQLx commit `94aafe3a68884d923b0798a767c8d7f6cfda89d2`, which resolves the `rusqlite 0.40.2` / `libsqlite3-sys 0.38.2` native-links conflict. Also adds one `#[allow(deprecated)]` for `time >= 0.3.55` format components. Independent of embedded-kernel work. | A published compatible `sqlx-sqlite` release (latest is still 0.9.0). | Update the dependency and lockfile, remove the patch path and vendor tree, then pass the combined awman/msb link, exactly-one-bundled-SQLite check, awman store tests, msb migrations, cross-driver checks, and old/new catalog round trips on supported targets. Do not couple this to libkrun work. |
| ~~libkrun kernel-provider loader~~ (**removed by WI 0120**) | Was `third_party/msb_krun-0.1.39/` (patch SHA-256 `cf6d58d0574d8bfefad6137b3f6435ec5524dbf11e8f6b24ff2133c8610dcfba`). The tree is deleted and no `[patch.crates-io]` entry remains. | Published `msb_krun =0.1.41` with `static-krunfw` and the awman-owned `krunfw_get_kernel` callback (Track A). | Done for the build/link gates (see the register). Still open: Apple Silicon boot, signed-artifact and strict-fixture gates (BLOCKED, not run here) and Linux KVM boots (WI 0124). A future `msb_krun` bump must re-read `load_krunfw_library`, because Rust cannot type-check the `no_mangle` signature against the C declaration. |
| Microsandbox guest-agent artifact-root glue | `third_party/microsandbox-filesystem-0.7.8/` (published crate SHA-256 `e890749fd6c87b7966865a4f7a0add888926adce222fdd4525eb64e047fee30b`; patch SHA-256 `b87c1e5f67fe0b9b3044bc43ff332601233280012bd35ea4145b6b75b9feab61`). Only `build.rs` changes: it adds target-aware lookup at `MSB_EMBED_ARTIFACTS_ROOT/<target-arch>/agentd`, and existing inputs remain. Published 0.7.8 still has only `MSB_EMBED_ARTIFACTS_DIR`. | An equivalent upstream target-aware build input, or awman setting `MSB_EMBED_ARTIFACTS_DIR` per target in its own build. | Pin the release/commit, remove the vendor patch and path override, and verify clean builds for all required targets plus guest-agent hash/architecture checks. |
| Awman-side worker and provider integration (not a vendored dependency patch) | `crates/awman-host-shims/src/{worker.rs,kernel.rs,version.rs}` (built behind `cfg(awman_builtin)`), `src/engine/container/builtin/embedded/mod.rs` (`set_libkrunfw_path`), `src/engine/container/builtin/msb_driver.rs` (firmware-path check), constants in `src/data/builtin_runtime.rs`, and the single early call `awman_host_shims::dispatch_worker(...)` in `src/main.rs`. `kernel::prepare()` arms the process (validation plus the aligned copy) before the worker runs; `krunfw_get_kernel` is the one documented exported symbol. | Remove the firmware-path transport only when the SDK can represent an embedded origin directly. Keep the worker protocol and early dispatch while the SDK still launches the same executable. Scope any needed Microsandbox runtime patch as its own upstream change and record it here before carrying it. | Test worker launch/version compatibility first, then remove only the glue the upstream API makes redundant. Verify same-executable boot, installed-runtime refusal, protocol compatibility, and no symbol/path workaround beyond the one documented callback. |
| Awman-side macOS builtin fixes (WI 0122; awman code, not dependency patches) | `crates/awman-host-shims/src/kernel.rs` uses the aligned process-lifetime copy on every serving target (Mach-O caps section alignment below 64 KiB). x86_64 serving is refused until its runner uses private-memory boot. `crates/awman-host-shims/src/worker.rs` accepts the SDK's umask-mode lifecycle lock (no group/other write). `src/engine/container/builtin/msb_driver.rs` canonicalizes bind-source ancestors, because the SDK's no-follow root open refuses macOS `/tmp` and `/var` symlinks; a symlinked leaf is still refused. The fourth WI 0122 fix (`only_lo` probe tolerates the kernel's inert `dummy0`) is test-only and has no upstream counterpart. | Upstream: a kernel provider that owns alignment (Track A keeps the copy in awman), an SDK lock created `0600`, and SDK root resolution that tolerates system symlink ancestors while refusing a symlinked leaf. | Remove each only after the matching upstream change is pinned and the macOS actual-awman guest tests pass again. All three are platform-neutral and must be revalidated on Linux in WI 0124. |
| Awman-side Apple Containers XPC transport (WI 0122; not a vendored dependency patch, no upstream code carried) | `crates/awman-host-shims/src/apple_xpc.rs` (macOS only; moved by WI 0126 D1), adapted by `crates/awman-cli/src/apple_transport.rs` (`NativeXpc`), and registered at startup in `src/main.rs` through `engine::oci::apple_store::install_transport`. Unsafe scope: public libxpc/libSystem symbols (`xpc_connection_*`, `xpc_dictionary_*`, `xpc_get_type`, `xpc_retain`/`xpc_release`, type/error singletons) and one capture-free global block with `_NSConcreteGlobalBlock` isa. No Swift toolchain, framework, DSO or executable is added. The protocol (keys, routes, release pins by version and full commit) lives in the safe library (`src/engine/oci/apple_store.rs`, contract in `crates/awman-host-shims/src/xpc.rs`). Provenance is `apple/container` tags 0.12.0/1.4.1/1.5.0, each natively validated, recorded in [WI 0122 notes](../review-notes/0122-evidence/notes.md). Unrelated to libkrun. | A supported, versioned public client API from Apple (e.g. a C-ABI or documented XPC protocol version). | Replace the transport, keep the library's release gate and output checks, and rerun the native `apple_native_*` tests and template exports on Apple Silicon. Each new Apple release needs a source comparison and pin entry before it is accepted. |
| Microsandbox isolated configuration builder (WI 0121 downstream patch) | `third_party/microsandbox-0.7.8/` vendors published `microsandbox 0.7.8` (crate SHA-256 `d4e2876d9ac789a99f9c7641be568ee8996d78eeebd19aa524aa2662273b3e44`, Apache-2.0, upstream revision `7b7b9dc89e9a1c77801918f7833c3381d1754579`). Its method-and-regression [`upstream.diff`](../../third_party/microsandbox-0.7.8/upstream.diff) (SHA-256 `196167178dc241f18d15aa55ed0abb76e67565a181dbf6e3a319afa46c441e42`) adds `LocalBackendBuilder::build_lazy_isolated(self) -> MicrosandboxResult<LocalBackend>` using `BackendConfig::new(...).prepare_for_local_backend(...)` with built-in defaults and an empty managed layer (0.7.8's validation is fallible, so the return type changed from 0.7.2). Not upstream as of `v0.7.8`. See [`PATCH.md`](../../third_party/microsandbox-0.7.8/PATCH.md). | A pinned upstream SDK release with equivalent isolated-builder semantics. A small, self-contained candidate for a separate Microsandbox PR. | Update the dependency and lockfile, remove the path patch and vendor tree, then rerun hostile installed-config tests, SDK runtime origin/path checks, native installed-runtime coexistence and guest boots on Apple Silicon (Linux KVM in WI 0124). Do not claim native coexistence from a feature compile alone. |
| Microsandbox visible-SNI network enforcement (WI 0121) | `third_party/microsandbox-network-0.7.8/`, published crate SHA-256 `9bfc80d59e60d46b44d19c60e3e9abe5c1b3b41c92cad739f661b752376902c2`, Apache-2.0, same upstream revision. Seven files: six policy files add default-off `strict_sni` to config/builder/poll/TCP proxy and tighten suffix policies to require the exact claimed name's DNS/IP binding, and the guest host alias is renamed to `host.builtin.internal`. [`upstream.diff`](../../third_party/microsandbox-network-0.7.8/upstream.diff) SHA-256 `5dbe1bbd4ee65adb2a518ded77e6737c87225dab1d4ad76a39b8c4538c17a665`; [provenance and boundary](../../third_party/microsandbox-network-0.7.8/PATCH.md). Upstream `strict` now defaults to true; `strict_sni` stays off. Ordinary strict opaque-TLS refusal is unchanged unless explicitly opted in, and suffix-binding tightening applies in every mode. | A pinned upstream release with explicit visible-SNI enforcement and exact DNS binding without TLS interception. The alias rename stays awman-specific unless upstream accepts a configurable alias. | Remove alongside the types patch after actual SDK TLS positive/negative, missing/wrong/shared-IP/sibling-binding, config roundtrip and native non-root guest network tests pass on Apple Silicon (Linux KVM in WI 0124). Do not infer encrypted HTTP authority or ECH inner-name enforcement, or native packet routing, from host-side TLS tests. |
| Microsandbox network option transport (WI 0121) | `third_party/microsandbox-types-0.7.8/`, published crate SHA-256 `74af7e859dcfb714bf612e7e27ad682aaedc946be419ace40e661d7e6ae74ba6`, Apache-2.0, same upstream revision. Three Rust files add default-false `strict_sni` to local/cloud NetworkSpec and preserve both conversions, so the SDK builder cannot silently drop the network option. [`upstream.diff`](../../third_party/microsandbox-types-0.7.8/upstream.diff) SHA-256 `f0db5e8555259e07288a97a45a4d3dcb66a561a98cd4c1229bbd26b0f4b0fa41`; [provenance](../../third_party/microsandbox-types-0.7.8/PATCH.md). | The paired upstream networking release must carry the explicit option through all configuration conversions. | Remove with the network patch only after actual SandboxBuilder NetworkConfig→NetworkSpec→NetworkConfig roundtrip, backward-compatible defaults and SDK/native enforcement tests pass. No enforcement by an unpatched cloud server is claimed. |

WI 0121's completion follow-up also added direct manifest edges (normal Rustls
for the shared API/squad TLS server's ring provider, a feature-gated
`microsandbox-network` edge for SDK policy-evaluator tests, and
feature-gated `sqlx =0.9.0` with SQLite/Tokio for driver transaction tests).
These are awman manifest changes, not patches. The pinned edges moved to `=0.7.8`
with the Phase 2 bump.

The carried sources and their patch notes are listed in
[`third_party/README.md`](../../third_party/README.md) and each tree's
`PATCH.md`. None of the five remaining patches has been accepted or released upstream. The
`msb_krun` patch was unnecessary once upstream shipped `static-krunfw`. The historical
spike under `tools/oci-runtime-spike/` remains evidence only and is not a
production runtime dependency.

### Acceptance criteria

- [x] The upstream route is decided and recorded (Track A by default; Track B
  only with a recorded, concrete gap), with the upstream bases used. Recorded in the register: Track A, `msb_krun 0.1.41` / libkrun `b35c7e1…`, Microsandbox `v0.7.8`.
- [x] The Microsandbox/msb_krun dependency is bumped to a release containing
  the chosen API, and every carried Microsandbox patch is rebased (or dropped
  because it landed upstream) with new provenance and hashes. All four Microsandbox patches were rebased onto 0.7.8 (none landed upstream); hashes are in the inventory and `third_party/README.md`. Payload re-pinned for Linux aarch64 only (see "Proven starting point").
- [x] The vendored `msb_krun` patch is removed. Kernel provision uses the
  published API with explicit validation, a writable aligned buffer kept for
  the process lifetime, and no hidden fallback to a dynamic library.
- [x] Any remaining unsafe code is confined to `awman-host-shims` with exact
  `// SAFETY:` contracts. The `awman` library keeps `forbid(unsafe_code)`.
- [ ] Apple Silicon gates (WI 0123 tiers, signed optimized artifact,
  strict fixture) pass on the adopted API. Linux ARM64/x86_64 KVM boots are
  recorded as WI 0124 gates, never as passes without hardware evidence.
  **Not done:** no Apple Silicon host was available. These gates are BLOCKED/pending, and the Darwin payload is unverified. Linux ARM64/x86_64 KVM boots remain WI 0124 gates.
- [ ] Any upstream PR (libkrun docs fix, Track B API, Microsandbox isolated
  builder/filesystem/network changes) is separately scoped and tested, and is
  submitted only with explicit authorization. Acceptance is tracked here, not
  assumed.
  **Not done:** nothing was submitted. A libkrun documentation fix is drafted locally only, in [`upstream-libkrun-docs.md`](../review-notes/0120-evidence/upstream-libkrun-docs.md); no fork, branch or PR exists.
- [x] `third_party/README.md`, the evidence register and this inventory agree
  with the tree after adoption. Hashes and versions were compared with `sha256sum` and `Cargo.toml` on 2026-10-21. Final verification re-checked every `upstream.diff` and published-crate SHA-256 against the README, register, inventory, `PATCH.md` files, `Cargo.lock` and the fetched crates. A Git-tracked-checkout audit remains blocked because this workspace's `.git` points to an unavailable host worktree; repeat it on a real checkout.

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

### Adversarial review fixes and release blocker (2026-10-21)

WI 0120 remains **unreleased / incomplete**: Darwin 0.7.8 extraction, hash/layout
verification, 16 KiB-host execution, signing/strip/re-sign, final-artifact boot and
strict-fixture gates require Apple Silicon and have not passed. The release workflow
intentionally depends on its failing native payload gate; do not bypass it to publish
other assets. `make payloads` currently exits nonzero on Darwin; `make build` omits
builtin there. User docs now disclose this temporary source-build limitation.

The pinned x86_64 runner maps the shared callback allocation directly into kernel
RAM. `kernel::prepare` now refuses that consumer, even if its payload is later
verified. WI 0124 must enable `Vm::set_private_memory_boot(true)` (or equivalent
per-VM copying), add a two-mapping isolation test and pass native concurrent boots
before removing that refusal. Supported ARM64 consumers copy the image into each
VM; pointer equality tests prove accessor idempotence, not VM isolation. Each normal
worker currently enters only one VM. Callback consumption occurs at `Vm::enter`.

Upstream 0.1.41 changes virtio-fs capability management from libcap-ng to Rust
`caps`. Current builds no longer require or link libcap-ng. Its source pipeline is
retained only for the historical 0.7.2 standalone catalog reader. No host agent is run.

The live SQLite tool uses 0.7.8 and the production static shim; the old reader is
self-contained. Compatibility means forward 27-to-29 migration, row preservation
(including strict_sni and labels) and explicit non-destructive 0.7.2 refusal, not a
successful downgrade. The actual-awman old/new build upgrade test was dropped: the builtin runtime has not been released, so no user holds a 0.7.2 catalog. Executable replacement remains an independent hardware test. In 0.7.8 the process-level `set_sdk_msb_path`/`set_libkrunfw_path` registrations feed the isolated config's `paths.msb`/`paths.libkrunfw`, so `resolve_runtime` reports `RuntimeOrigin::Configuration`, although its docs describe process-level setters as `SdkPackage`; `msb_driver` now requires that origin plus both config paths equal to the executable.
The no-VM artifact diagnostic `__awman-builtin-kernel-check` invokes the actual C
accessor and checks its pointer, layout and full contents. It does not earn a boot PASS.
