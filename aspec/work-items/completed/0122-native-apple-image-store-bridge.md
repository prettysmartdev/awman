# Work Item: Feature

Title: Implement and validate the pinned in-process Apple image-store adapter
Issue: n/a

Current distribution policy (2026-09-28): signing and notarization are out of
scope by user instruction. No signing identity, Apple distribution account,
notarization credentials or explicit ad-hoc signing step is required. Native
boot and final-artifact checks still apply; failures must be reported honestly.
Status: Completed 2026-09-29. The in-process, version-gated adapter is
implemented and natively validated on Apple Silicon against Apple Containers
0.12.0, 1.4.1 and 1.5.0, with 1.3.1 refused. The evidence and its frozen code
hash are in [native evidence notes](../../review-notes/0122-evidence/notes.md).
The offline builtin run used a locally ad-hoc-signed artifact, because
Hypervisor.framework requires the `com.apple.security.hypervisor` entitlement;
reconciling that with the unsigned-distribution policy belongs to WI 0123.
Parent: [WI 0121](../0121-complete-builtin-runtime-and-native-verification.md), D-01/D-02/D-15; [WI 0119](../0119-strict-embedded-microsandbox-runtime.md), acceptance criterion 5

## Summary:

- The user explicitly authorized moving work blocked on unavailable resources
  to a follow-up assigned to an agent with those resources. This item carries
  the **missing Apple-store implementation**, not just a validation run.
- `src/engine/oci/apple_store.rs` currently refuses acquisition. Its typed
  contract and historical upstream investigation are inputs, not an adapter.
  Manual archive export does not satisfy this item.
- Assign execution to an agent with native Apple Silicon macOS, the supported
  Apple Containers service and version, Swift/Xcode tools, and a
  clean checkout with accessible Git objects. Do not dispatch to a Linux-only
  agent or claim that an agent has these resources without its preflight.

## User Stories

### User Story 1:
As a: user

I want to:
Import an image directly from the explicitly selected Apple store with the
single awman executable, then execute the verified cached image offline.

So I can:
Use the builtin runtime without a host CLI helper, private-store scraping,
manual source substitution or an installed Microsandbox runtime.

## Implementation Details:

1. Record native host/service/toolchain versions and exact awman base/HEAD plus
   dirty patch. Read the current evidence register and `apple_store.rs` contract.
   Inspect the actual installed, supported service and pinned upstream sources;
   the historical 1.4.1 contract must not be presumed current or stable.
2. Implement a version-gated in-process bridge. Evaluate a statically linked
   Swift/C ABI shim or a narrowly reviewed Rust/native boundary. Preserve the
   crate's unsafe policy: put any necessary exception in a separately scoped,
   reviewed boundary with provenance, tests and WI 0120 inventory; do not bypass
   the rule with an extracted executable or DSO.
3. Negotiate/verify the service release before image routes; refuse unknown
   versions, unavailable services, ambiguous images and wrong architecture.
   Select an image using the service API, export only into awman's private
   leased staging area, then use the existing limits, validation and atomic
   cache publication. Do not read the store's private on-disk layout.
4. Carry cancellation/deadline through service requests and file production.
   Validate output type/ownership and prevent symlink/path substitution. Clean
   interrupted exports, retain previously committed refs, and sanitize errors.
5. Integrate the production source dispatch and ready path. No new source kind,
   automatic archive fallback, host agent or CLI invocation is authorized.

## Edge Case Considerations:

- Version mismatch, helper crash/restart, delayed response, cancellation, ENOSPC,
  malformed/truncated output, wrong platform and mixed-image stores.
- Multiple consumers and concurrent import/prune; service shutdown after import;
  non-root guest defaults; long/non-ASCII private paths and malicious image names.
- Existing Apple backend behavior remains compatible; strict bridge failure
  cannot change backend selection or leak credentials into argv/logs.

## Test Considerations:

- [x] Actual awman imports all required shipped-template images through the live
  supported Apple store on Apple Silicon; record fixture/source digests.
  All nine agent templates and the project image:
  [template-exports.log](../../review-notes/0122-evidence/final/template-exports.log).
- [x] Version/platform/auth/service/error/cancellation/retry cases execute against
  disposable services; unknown protocol/version fails explicitly.
  The native runs cover: disposable 1.5.0/1.4.1 (pass), 1.3.1 (refused), a
  stopped service, mid-save cancellation, concurrent importers, and a helper
  killed mid-save and then retried. Wrong-platform, malformed and peer-error
  cases use scripted transports and the shared validator. The only
  authorization is the helper's euid check; there are no credentials.
- [x] No helper CLI, extracted executable/firmware/DSO or private-store access in
  an actual native execution trace. This is kernel-enforced with
  `sandbox-exec` (no fork, no exec other than itself, no private-store access,
  with positive controls):
  [sandboxed-export.log](../../review-notes/0122-evidence/final/sandboxed-export.log).
  Actual-awman runs also trap `docker`/`container`/`sbx` on PATH.
- [x] Stop the store, remove source access and run the imported image using the
  actual builtin runtime with denied egress and a minimal PATH.
  [guest-runs.log](../../review-notes/0122-evidence/final/guest-runs.log).
- [x] Local changes pass `make pre-push`, feature Clippy and builtin tests; native
  required-hardware tests execute, with no opt-out counted as a pass.
  [hermetic.log](../../review-notes/0122-evidence/final/hermetic.log): the only
  failures are two host `network_probe_*` tests that need GNU `timeout`,
  identical on the base commit.
- [x] Integrate with WI 0123's source/format/template and final-artifact matrix.
  Every mandatory Apple row has final linked evidence at the same revision.
  The Apple-store source, format and template rows are linked from the WI 0121
  register (D-01) to logs from one frozen tree. Optimized/distributed-artifact
  rows remain WI 0123 scope and depend on its entitlement/signing decision.

## Codebase Integration:

Use the existing source resolver, CachingAcquirer controls/leases and archive
validator. Coordinate any native library/ABI/build-input patch with WI 0120.
The evidence register must continue to mark D-01 FAIL until this code exists
and the required executions pass; task transfer is not runtime acceptance.

## Documentation

Update existing image-source/onboarding/runtime guides, security/build specs,
WI 0120 patch provenance and the shared evidence register after implementation.
Preserve the original WI 0119 gate; close this item only with native evidence.
