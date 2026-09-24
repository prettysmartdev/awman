# Work Item: Task

Title: Create an upstreamable embedded-kernel API for Microsandbox's libkrun
Issue: n/a

## Summary:
- Turn the strict-embedding proof and the local dependency patch from
  [WI 0119](0119-strict-embedded-microsandbox-runtime.md) into a small, reusable,
  tested upstream change in `superradcompany/libkrun`, the repository publishing
  Microsandbox's `msb_krun` dependency.
- Support kernel bytes owned by the embedding application without requiring
  a firmware shared library, a magic filesystem path or process-global symbol
  discovery. Preserve existing external-firmware users and defaults.
- Separate any necessary Microsandbox SDK/runtime integration change into a
  follow-up PR to `superradcompany/microsandbox`. Do not describe the entire
  feature as requiring a fork of the msb application.
- WI 0119 ships against the controlled local patch while this work proceeds.
  Upstream acceptance is not required to finish WI 0119 or to finish preparing
  a reviewable patch here. Adoption/removal of the local patch is a separately
  gated final phase once a compatible upstream commit/release exists.
- A fork URL has not yet been supplied. Prepare a local reviewable change and
  PR material without creating branches, pushing, publishing or submitting a
  PR unless the user explicitly authorizes those actions.

## User Stories

### User Story 1:
As a: developer embedding Microsandbox's VM stack

I want to: pass a validated kernel payload directly from my application

So I can: ship a single executable without extracting a firmware library or
relying on application-specific loader hacks.

### User Story 2:
As a: upstream maintainer

I want to: review an additive API with focused tests and preserved defaults

So I can: support embedding without taking on awman-specific behavior or
breaking existing firmware-library consumers.

### User Story 3:
As a: awman maintainer

I want to: replace the local dependency patch with a pinned upstream version

So I can: reduce divergence while retaining the validated strict runtime.

## Implementation Details:

### Proven starting point

- Baseline: `msb_krun 0.1.39`, libkrun commit
  `2bd0f84ad0956f3032e0490d3b8512b6851eca12`, used by Microsandbox `v0.7.2`
  (`60d4dc8a436fb9365491567ec21d073e924e3c6d`). Verify the actual upstream
  layout and current APIs before rebasing; do not assume the pinned version
  is still current when implementing this item.
- Prototype patch:
  `tools/oci-runtime-spike/strict-embed/embedded-loader.patch`. It resolves
  `krunfw_get_kernel` from the current executable when given the executable
  path or a sentinel. The awman-side probe returns bytes and boot addresses
  from a 64-KiB-aligned static kernel array.
- The full strict Apple Silicon probe booted and passed 9 image assertions,
  32 mount/config assertions, exit status 37 and embedded-provider observation.
  Only macOS system libraries/frameworks were linked; no extracted-helper
  candidates were found. Raw evidence and archive hash are in the strict spike
  report and `tools/oci-runtime-spike/strict-embed/results/mac-arm64/`.
- Linux ARM64 linked and imported OCI, then reached KVM initialization using
  the embedded provider. Guest boot was blocked by missing `/dev/kvm`; Linux
  x86_64 execution is also outstanding. Do not describe those as boot passes.
- Existing guest-agent byte embedding and the public runner entrypoint already
  exist in msb. The firmware source is the narrow missing low-level API, not
  an absence of Rust-native VM execution.
- The prototype's sentinel, self-as-firmware path, exported getter and global
  symbol lookup are evidence only. They are not the proposed upstream interface.

### Phase 1: review and API design

1. Read the actual target repositories' `AGENTS.md` and contribution rules.
   Work in a separate upstream checkout/fork; keep awman's unrelated changes
   untouched. Record upstream base, local patch and correspondence to WI 0119.
2. Propose an application-neutral typed provider on the kernel builder, backed
   by owned or process-lifetime immutable bytes with explicit guest load and
   entry addresses. Prefer the smallest API that covers native Linux x86_64,
   Linux ARM64 and Apple Silicon without prescribing a particular frontend.
3. Specify alignment, payload length, host lifetime, guest-address bounds,
   arithmetic overflow and architecture constraints. Do not introduce an
   apparently safe constructor over arbitrary raw pointers. If unsafe input
   is necessary, confine and document its exact safety contract and provide
   a safe owned/static-data path for normal embedding.
4. Define how embedded data interacts with existing firmware paths, external
   kernels and initramfs options. Reject ambiguous/conflicting configuration
   explicitly; never silently ignore the embedded payload or fall back to a
   dynamic library if it is invalid. Preserve existing no-embedded-source
   behavior and error handling.
5. Establish ownership through VM teardown, multiple VM constructions and
   error paths. Retain the backing storage or owner for as long as VM memory
   needs it. Test the same-executable process model, not a new promise that
   multiple VMs can safely run in arbitrary host threads.

### Phase 2: focused implementation and review material

1. Implement an additive libkrun API plus tests, an embedding example and API
   documentation. Preserve the normal firmware DSO path and its validation.
   Do not expose awman's versioning, image stores, config types or CLI routing
   in this lower-level crate.
2. Avoid requiring exported C symbols, `--export-dynamic`, custom executable
   section names or a fake firmware pathname for the new kernel-provider API.
   Awman's independent worker-version metadata is a separate concern.
3. Support architecture-appropriate validation rather than hardcoding the
   prototype's ARM64 addresses/24,576,000-byte size as universal constants.
   Keep unrelated EFI/TEE/Windows paths unchanged or explicitly reject an
   unsupported combination without regressing existing consumers.
4. Coordinate the API shape with WI 0119's local patch. If implementation
   precedes upstream acceptance, carry the same tested interface locally;
   avoid maintaining two unrelated embedded-kernel implementations.
5. If Microsandbox needs an integration hook to pass the typed provider or
   select same-executable workers, prepare it as a separate, minimal dependent
   change. Preserve legacy external msb+firmware resolution, installed-runtime
   behavior and launch protocol compatibility. Never serialize host addresses
   or raw pointers into persisted/IPC launch configuration; resolve embedded
   identities to trusted compiled-in payloads in the worker.
6. Keep awman's SQLite alignment, Linux native-library packaging, source-store
   adapters and full backend implementation out of the firmware API PR.
   WI 0119's SQLite decision is a separate manifest-only backport of SQLx
   commit `94aafe3a68884d923b0798a767c8d7f6cfda89d2`, already merged upstream.
   Do not create a duplicate SQLx PR or couple its replacement by a published
   release to acceptance of the embedded-kernel API.
7. Prepare a PR description with motivation, public API example, compatibility
   analysis, explicit non-goals, test matrix, and measured evidence. Distinguish
   upstream-msb tests, old prototype tests and tests of the actual proposed API.
   Do not claim acceptance, release availability or unperformed platform tests.

### Phase 3: adoption when upstream is available

- Pin the reviewed upstream commit or released crate version rather than a
  floating branch. A fork-only commit is still a downstream dependency, not
  proof the change has landed upstream.
- Replace WI 0119's local patch in a focused awman change and regenerate the
  lockfile. Remove vendored code, linker flags, exported provider symbols and
  firmware-path shims only when their replacements are actually in use.
- Re-run strict artifact, image/mount, worker lifecycle, ordinary external
  firmware and required-platform regression tests. Retain kernel/guest-agent
  provenance and source/notice obligations; upstreaming does not remove them.
- Record the upstream reference and patch-removal result. If review is pending
  or declined, leave the tested local patch owned and documented, with an
  explicit maintenance decision; do not block or mislabel WI 0119's completion.

### Acceptance criteria

- [ ] A focused reviewable patch against a recorded libkrun upstream base
  exposes an application-neutral embedded-kernel API with clear ownership and
  validation, without the prototype's symbol/path workaround.
- [ ] Existing firmware loading/default behavior remains covered and unchanged;
  invalid/conflicting input fails explicitly, with no hidden fallback.
- [ ] Tests and examples cover the API and backing-storage lifetime, and the
  actual proposed patch boots on Apple Silicon and Linux ARM64/x86_64 with KVM.
  Blocked hardware gates are reported rather than claimed complete.
- [ ] Any Microsandbox integration patch is separately scoped, versioned and
  tested for the relevant launch/runtime compatibility boundaries.
- [ ] WI 0119's local patch and the proposed API have a documented migration
  path; necessary local maintenance remains possible while upstream reviews.
- [ ] PR-ready rationale, compatibility notes and reproducible test evidence
  are prepared. Submission occurs only with explicit authorization. Upstream
  acceptance is tracked separately and is not represented as already achieved.

## Edge Case Considerations:
- Empty, truncated, oversized, wrong-architecture or improperly aligned kernel
  data; invalid entry/load addresses; range overflow and incompatible initramfs.
- Payload owner dropped too early, accidental copying/moving of a borrowed
  backing buffer, teardown after startup failure, and shared immutable payloads
  across separate VM worker processes.
- Optimization/LTO/dead-stripping, macOS signing, Linux target ABI and differing
  host page sizes. Test the real payload layout, not only a toy byte slice.
- Existing users explicitly selecting a firmware DSO, defaults that discover
  one, consumers using other boot modes, and older launch/config producers.
  Preserve their contracts rather than forcing everyone onto embedded mode.
- Upstream API/version changes, reviewers preferring another safe provider
  shape, or a separate msb glue change being needed after libkrun acceptance.

## Test Considerations:
- Unit tests: input validation, configuration precedence/conflicts, ownership
  and lifetime, error cleanup, and ordinary dynamic-firmware behavior.
- Build/link tests: Linux ARM64/x86_64 and Apple Silicon, optimized/LTO builds,
  provider retained without executable-symbol export workarounds, and unchanged
  builds for unaffected upstream targets/features.
- Real-hardware tests: native guest boot, clean shutdown, repeated launches and
  error paths with a known kernel and fake workloads. Do not execute an agent
  on the host, use real credentials, or require paid provider access.
- Re-run awman's strict fixture: all image and mount/config assertions, exit
  status, same-executable worker identity, minimal-PATH/offline execution and
  dynamic-dependency/helper-extraction checks. The old prototype's passing
  results do not substitute for validating the new API implementation.
- Run upstream's relevant tests/contribution gates and cross-version protocol
  fixtures for any msb integration change, plus awman regressions when adopting.

## Codebase Integration:
- Initial upstream target: `superradcompany/libkrun`, especially its Rust
  kernel builder and VM firmware-loading boundary under `src/krun/src/api/`.
  Follow that repository's public API, ownership, formatting and test patterns.
- Potential separate target: `superradcompany/microsandbox` SDK runtime
  resolution/launch and runner kernel configuration. Do not expand the libkrun
  PR to include awman's unrelated runtime implementation.
- Awman references: WI 0119 and
  `tools/oci-runtime-spike/strict-embed/README.md`. Preserve the historical
  proof/evidence, but migrate production code to the reviewed typed API.

## Documentation

- Provide upstream API docs and a minimal executable-embedding example, with
  ownership, supported boot modes/platforms and OS prerequisites stated.
- Keep fork/patch maintenance, review status, reproducibility and migration
  details in this item and developer material, not a user-facing WI guide.
- Update awman user documentation only if adopting upstream changes alters a
  user-visible prerequisite or behavior; upstream patch mechanics alone do not
  warrant a new end-user guide.
