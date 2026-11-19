# Work Item: Task

Title: Enable and validate the builtin runtime on Linux KVM
Issue: https://github.com/prettysmartdev/awman/issues/47

Status: In progress on `fix/linux-builtin-runtime`, based on `nextgen`.

## Current execution contract (2026-11-19)

This section supersedes the historical commands and optional-release framing
below. WI 0144's `ci/gates.toml` suites and `awman-test-support` are mandatory;
retired native scripts and test-count claims are not acceptance evidence.

- Enable Linux x86_64 GNU builds only after native KVM guest and optimized
  artifact acceptance. Validate `/dev/kvm` access as an ordinary user, the
  embedded kernel's private-memory boot path, concurrent guests, lifecycle,
  networking, mounts, source imports, and local Docker builder fallback.
- The supplied native machine is Linux x86_64. Linux ARM64 remains a separate
  hardware acceptance obligation; its compile/payload checks cannot establish
  that it boots. Do not enable an architecture on borrowed evidence.
- Support explicit archive, OCI registry, and Docker Engine sources. Apple
  Containers is a macOS source and must refuse explicitly on Linux. Preserve
  source identity and cached execution without a source service or builder.
- Provide `make linux-prerelease` through the existing native-validation
  runner, with private fixtures, registered suites, exact candidate digests,
  per-test results, portable logs and independent packet verification.
- There is no previous published Linux builtin executable. Keep genuine
  previous-release catalog acceptance for macOS; record Linux executable
  replacement and cached restart separately without claiming an old-release
  upgrade. Future Linux releases must pin their published predecessor.
- Repair ordinary Linux `make build` / `make install` and distribution asset
  selection; privilege escalation is restricted to installation. Release
  assets must include their documented runtime and declare their ABI honestly.
- Review WI 0120 / issue #46 and keep every changed upstream patch, provenance,
  removal condition and saved upstream proposal current. Submit no upstream PR.
- Update current user documentation, create a PR targeting `nextgen`, and
  require green CI plus the native evidence before declaring merge readiness.

The work packages below retain the historical coverage backlog. Real TLS/mTLS,
private-CA/proxy, failure and format coverage must be accounted for explicitly;
unavailable ARM64, Windows or Docker Sandboxes infrastructure is not a pass.
Parent: [WI 0121](completed/0121-complete-builtin-runtime-and-native-verification.md)
Split from: [WI 0123](completed/0123-builtin-native-verification-and-release-closure.md)
(the macOS Apple Silicon `builtin-experimental` release)

## Summary:

- WI 0123 ships `builtin-experimental` on Apple Silicon macOS only and closes
  the builtin-runtime work stream, resolving WI 0119 and WI 0121. The criteria
  it could not cover are recorded there as **Deferred (WI 0124, optional)**.
- This item collects that deferred work: Linux KVM support, Docker Engine
  sources, and registry edge cases beyond WI 0123's normal-case validation.
  Pick up any package independently, in any later release.
- Start from the [evidence register](../review-notes/0121-evidence-register.md),
  WI 0121 sections A–J, WI 0120's patch inventory, and the macOS evidence and
  fixes from [WI 0122](completed/0122-native-apple-image-store-bridge.md) and
  WI 0123. The WI 0122 fixes (lock mode, bind-source ancestors, `only_lo`
  semantics) are platform-neutral and must be re-validated on Linux.

## User Stories

### User Story 1:
As a: user on Linux

I want to:
Use the builtin runtime (`runtime.kind: "builtin"`, 0139) on Linux arm64 or x86_64 with KVM, and import
images from Docker Engine and authenticated registries.

So I can:
Run agents in the embedded microVM runtime without the Docker daemon at run
time.

### User Story 2:
As a: maintainer

I want to:
Extend the experimental runtime beyond Apple Silicon and harden image sources
beyond the normal cases, with native Linux and real-service evidence.

So I can:
Offer the runtime on more platforms and sources in later releases.

## Implementation Details:

### Required agent assignments and preflight

These are capability-based assignments awaiting dispatch, not claims that an
agent has run:

| Assignment | Required access before accepting the task |
|---|---|
| Git/build coordinator | Accessible awman object database, authoritative WI 0119 base and reviewed head, history for genuine old/new builds; clean checkout/export capability |
| Linux ARM64 agent | Native ARM64 with usable `/dev/kvm`; disposable Docker Unix/TLS/mTLS and registry/proxy/CA services; strace; enough disk/RAM for bounded feature/release builds |
| Linux x86_64 agent | Native x86_64 with usable `/dev/kvm` and the same service/trace/build capabilities |
| Docker service agent (any host) | Disposable Docker Engine (Unix, TCP+TLS, mTLS) and authenticated private-CA registry/proxy/keychain services |

Each agent first records native architecture, KVM access, service/fixture
availability, Git identity and artifact paths. Fail missing prerequisites
explicitly and reassign; never count skips as success. Use disposable
secrets, redact evidence, and never dump ambient credentials.

### Work packages

1. **Payloads:** verify the `x86_64-unknown-linux-gnu` payload natively (it is
   still `unverified`); re-verify `aarch64-unknown-linux-gnu` from a clean
   checkout. Build with the pinned Linux payload (0.1.41 uses pure Rust `caps`; no libcap-ng input).
2. **Enable Linux:** lift WI 0123's platform gate for Linux arm64/x86_64 only
   after packages 3–8 pass on that architecture. Add the `builtin-runtime`
   feature to the matching Linux release assets and update the docs WI 0123
   marked "Apple Silicon only".
3. **Clean build and boot (D-03/D-11 Linux):** `tools/native-builtin-ci.sh`
   for both Linux targets; actual awman boots through the current provider.
4. **Native scenarios on Linux (D-04/D-05/D-06/D-07/D-08/D-09/D-12):** repeat
   WI 0123 section C packages 2–5, 7 and 8 on both architectures, including
   the x86_64 writable kernel copy, Linux deleted-executable upgrade cases, and
   SQLite old/new catalog checks.
5. **Docker Engine source (D-01/D-02):** real Unix/TLS/mTLS Docker suites;
   credential failure/expiry/disconnect/retry/deadline/cancel; late-failure
   report suppression; source shutdown with cached guest boot.
6. **Registry edge cases (D-02):** WI 0123 validates the normal cases against
   real public registries. This package covers the rest:
   - private-CA registries (`store_registry_real_ca_proxy_auth_roundtrip`) and
     `insecure` registries;
   - HTTPS/HTTP proxies with observed `CONNECT`, proxy credentials and
     `NO_PROXY`;
   - Bearer token expiry mid-pull and re-authentication;
   - disconnects mid-blob, stalled responses, deadlines and cancellation;
   - 5xx storms, retry exhaustion, final (non-retried) auth/TLS/digest errors;
   - late-failure report suppression.
7. **Corpus (D-15):** the full required format × shipped-template corpus from
   Docker and registry sources (Apple exports were covered in WI 0122/0123).
8. **Docker backend regressions (D-11/D-15):** existing `docker` runtime
   regressions with the builtin runtime present.
9. **Linux release artifacts (D-13/D-14):** optimized/LTO/stripped artifacts
   boot; trace executable/firmware access and helper/provider retention;
   minimal PATH and enforced-offline cached execution; ABI, measurements,
   notices, corresponding source and relink/source-offer materials.
10. **Docker-dependent leftovers from WI 0123:** the live
    `docker-sbx-experimental` regression (needs a signed-in Docker account,
    `sbx login`); `docker save` and buildx OCI exports in the real template
    corpus (`tests/oci_import/corpus/build-corpus.sh` with Docker; WI 0123
    shipped an Apple-only corpus mode); a native Windows run of the
    existing-backend refusal checks (CI builds it).
11. **Record-keeping:** as each package lands, update the evidence register
    rows WI 0123 marked Deferred to PASS, with linked evidence.

## Edge Case Considerations:

- The same behaviour as WI 0121: no ambient runtime discovery, host agent
  execution, helper fallback, secret leakage or unlisted parity exception.
- Linux `/tmp` and similar paths are not symlinks, but bind-source ancestor
  canonicalization (WI 0122) must not change Linux behaviour; test it.
- The embedded kernel's inert `dummy0` also appears on Linux; the WI 0122
  `only_lo` probe semantics apply.

## Test Considerations:

- [ ] x86_64 payload verified; arm64 re-verified from a clean checkout.
- [ ] D-01/D-02: Docker Engine sources work through real Unix/TLS/mTLS
  services with auth/failure/retry; cached execution needs no source.
- [ ] D-03/D-11: clean-checkout builds boot on Linux arm64 and x86_64 KVM.
- [ ] D-04/D-05/D-06/D-07/D-08/D-09/D-10/D-12 on both Linux architectures.
- [ ] D-13/D-14: Linux release artifacts boot and pass native scans; materials
  and measurements recorded.
- [ ] D-15: full format × template corpus and Docker backend regressions.
- [ ] Linux enabled for `runtime.kind: "builtin"` (0139 drops the `-experimental` value suffix; maturity is stated in docs and `awman config show`); docs updated.
- [ ] Registry edge cases (package 6) pass against disposable services.
- [ ] Register rows updated from Deferred to PASS as packages land. Each
  package can be completed and released on its own.

## Codebase Integration:

- Follow the conventions and suites established in WI 0121–0123.
- Update WI 0120's inventory and the evidence register with every Linux fix.

## Documentation

When Linux is enabled, update the platform statements WI 0123 added
(`README.md`, `docs/00-getting-started.md`, `docs/07-configuration.md`,
`docs/11-runtimes.md` and the others listed there) to describe Linux support
and its KVM requirements. User-facing docs only; implementation detail stays
here and in code comments.
